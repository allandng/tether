// WebRTC transport: signal → offer → two data channels → ProtocolSession.
//
// Channel layout (mirrored by tetherd's webrtc module):
//   "tether-ctl"   reliable + ordered  Hello, Resolution, InputEvent
//   "tether-media" reliable + ordered  FrameData, chunked (see chunks.ts)
//   "tether-bulk"  reliable + ordered  oversized messages (clipboard),
//                                      chunked — SCTP caps single messages
//                                      at ~64 KiB
//
// The controller always initiates: it creates both channels and the offer;
// the host answers. Media stays peer-to-peer (DTLS); the signal server only
// introduces us.

import { FrameReassembler, chunkFrame } from "./chunks";
import type { ConnectionEvents, Transport } from "./connection";
import { channelBinding, sdpFingerprint, tokenStore } from "./pairing";
import { encodeClipboardData, type InputEvent } from "./protocol";
import { type AuthContext, ProtocolSession } from "./session";
import { SignalingClient } from "./signaling";

export interface RtcConfig {
  signalUrl: string; // e.g. ws://server:7879/ws
  secret: string;
  deviceId: string;
  deviceName: string;
  targetHostId: string;
  stunServers?: string[];
}

const DEFAULT_STUN = ["stun:stun.l.google.com:19302"];

export class WebRtcTransport implements Transport {
  private pc: RTCPeerConnection | null = null;
  private ctl: RTCDataChannel | null = null;
  private bulk: RTCDataChannel | null = null;
  private bulkSeq = 0;
  private signaling: SignalingClient | null = null;
  private session: ProtocolSession | null = null;
  private readonly reassembler = new FrameReassembler();
  private readonly bulkReassembler = new FrameReassembler();

  constructor(private readonly events: ConnectionEvents) {}

  connect(config: RtcConfig): void {
    this.close();
    this.events.onStatus("connecting");

    const pc = new RTCPeerConnection({
      iceServers: [{ urls: config.stunServers ?? DEFAULT_STUN }],
    });
    this.pc = pc;

    const authCtx: AuthContext = {
      deviceId: config.deviceId,
      deviceName: config.deviceName,
      getToken: () => tokenStore.get(config.targetHostId),
      setToken: (t) => tokenStore.set(config.targetHostId, t),
      // Bind the pairing proof to the negotiated DTLS fingerprints so a
      // malicious signal relay that MITMs the connection can't pair itself.
      channelBinding: async () => {
        const local = pc.localDescription?.sdp ? sdpFingerprint(pc.localDescription.sdp) : null;
        const remote = pc.remoteDescription?.sdp ? sdpFingerprint(pc.remoteDescription.sdp) : null;
        if (local && remote) return channelBinding(local, remote);
        return channelBinding("tether-no-fp", "tether-no-fp"); // matches host fallback
      },
    };

    const session = new ProtocolSession(
      {
        onConnected: () => this.events.onStatus("connected"),
        onResolution: (r) => this.events.onResolution(r),
        onFrame: (f) => this.events.onFrame(f),
        onClipboard: (text) => this.events.onClipboard(text),
        onDisplays: (d) => this.events.onDisplays(d),
        onPairingRequired: () => this.events.onPairingRequired(),
        onPairingFailed: () => this.events.onPairingFailed(),
        onProtocolError: (detail) => this.fail(detail),
      },
      (bytes) => {
        // our encoders always allocate plain ArrayBuffers; the cast just
        // narrows ArrayBufferLike for RTCDataChannel.send's signature
        if (this.ctl?.readyState === "open") this.ctl.send(bytes as Uint8Array<ArrayBuffer>);
      },
      authCtx,
    );
    this.session = session;

    const ctl = pc.createDataChannel("tether-ctl", { ordered: true });
    ctl.binaryType = "arraybuffer";
    this.ctl = ctl;
    ctl.onopen = () => session.start();
    ctl.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) session.onMessage(e.data);
    };
    ctl.onclose = () => this.fail("control channel closed");

    // Reliable + ordered: H.264 delta frames poison the stream if any
    // predecessor is lost, so loss-tolerance moved to the host (it drops raw
    // frames before the encoder, and keyframes every 2s self-heal). Chunking
    // remains for the 64KiB message-size cap.
    const media = pc.createDataChannel("tether-media", { ordered: true });
    media.binaryType = "arraybuffer";
    media.onmessage = (e) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      const wire = this.reassembler.onChunk(e.data);
      if (wire) session.onMessage(wire);
    };

    const bulk = pc.createDataChannel("tether-bulk", { ordered: true });
    bulk.binaryType = "arraybuffer";
    this.bulk = bulk;
    bulk.onmessage = (e) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      const wire = this.bulkReassembler.onChunk(e.data);
      if (wire) session.onMessage(wire);
    };

    const signaling = new SignalingClient({
      onRegistered: (iceServers) => {
        void (async () => {
          try {
            // Apply the server-supplied ICE servers (STUN + ephemeral TURN)
            // before negotiating, so relay candidates are gathered.
            if (iceServers.length > 0) {
              pc.setConfiguration({
                iceServers: iceServers.map((s) => ({
                  urls: s.urls,
                  username: s.username,
                  credential: s.credential,
                })),
              });
            }
            const offer = await pc.createOffer();
            await pc.setLocalDescription(offer);
            signaling.send({ type: "offer", target: config.targetHostId, sdp: offer.sdp ?? "" });
          } catch (e) {
            this.fail(`offer failed: ${e}`);
          }
        })();
      },
      onAnswer: (_from, sdp) => {
        void pc
          .setRemoteDescription({ type: "answer", sdp })
          .catch((e) => this.fail(`bad answer: ${e}`));
      },
      onIce: (_from, candidate) => {
        try {
          void pc.addIceCandidate(JSON.parse(candidate)).catch(() => {});
        } catch {
          // unparseable candidate: drop
        }
      },
      onPeers: () => {},
      onError: (code, message) => this.fail(`${code}: ${message}`),
      onClosed: () => {
        // Signaling is only needed for setup; once the ctl channel is open a
        // signaling drop is harmless. Before that, it's fatal.
        if (this.ctl?.readyState !== "open") this.fail("signaling closed");
      },
    });
    this.signaling = signaling;

    pc.onicecandidate = (e) => {
      if (e.candidate) {
        signaling.send({
          type: "ice",
          target: config.targetHostId,
          candidate: JSON.stringify(e.candidate.toJSON()),
        });
      }
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed" || pc.connectionState === "disconnected") {
        this.fail(`peer connection ${pc.connectionState}`);
      }
    };

    signaling.connect(config.signalUrl, {
      device_id: config.deviceId,
      name: config.deviceName,
      caps: { can_host: false, can_control: true },
      auth: config.secret,
    });
  }

  get connected(): boolean {
    return (this.session?.connected ?? false) && this.ctl?.readyState === "open";
  }

  sendInput(ev: InputEvent): void {
    this.session?.sendInput(ev);
  }

  sendClipboard(text: string): void {
    // Always via the bulk channel: a single data-channel message caps out
    // around 64 KiB, and clipboard may be up to 256 KiB.
    if (!this.connected || this.bulk?.readyState !== "open") return;
    this.bulkSeq = (this.bulkSeq + 1) >>> 0;
    const wire = encodeClipboardData({ text });
    for (const chunk of chunkFrame(this.bulkSeq, wire)) {
      this.bulk.send(chunk as Uint8Array<ArrayBuffer>);
    }
  }

  sendText(text: string): void {
    // Small committed text rides the reliable ctl channel via the session.
    this.session?.sendText(text);
  }

  selectDisplay(id: number): void {
    this.session?.sendSelectDisplay(id);
  }

  submitPairingCode(code: string): void {
    void this.session?.submitPairingCode(code);
  }

  close(): void {
    this.signaling?.close();
    this.signaling = null;
    this.session = null;
    this.ctl = null;
    this.bulk = null;
    const pc = this.pc;
    this.pc = null;
    if (pc) {
      pc.onconnectionstatechange = null;
      pc.close();
    }
  }

  private fail(detail: string): void {
    if (!this.pc) return; // already closed
    this.close();
    this.events.onStatus("closed", detail);
  }
}
