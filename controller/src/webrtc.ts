// WebRTC transport: signal → offer → two data channels → ProtocolSession.
//
// Channel layout (mirrored by tetherd's webrtc module):
//   "tether-ctl"   reliable + ordered    Hello, Resolution, InputEvent
//   "tether-media" unordered, no retx    FrameData, chunked (see chunks.ts)
//
// The controller always initiates: it creates both channels and the offer;
// the host answers. Media stays peer-to-peer (DTLS); the signal server only
// introduces us.

import { FrameReassembler } from "./chunks";
import type { ConnectionEvents, Transport } from "./connection";
import type { InputEvent } from "./protocol";
import { ProtocolSession } from "./session";
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
  private signaling: SignalingClient | null = null;
  private session: ProtocolSession | null = null;
  private readonly reassembler = new FrameReassembler();

  constructor(private readonly events: ConnectionEvents) {}

  connect(config: RtcConfig): void {
    this.close();
    this.events.onStatus("connecting");

    const pc = new RTCPeerConnection({
      iceServers: [{ urls: config.stunServers ?? DEFAULT_STUN }],
    });
    this.pc = pc;

    const session = new ProtocolSession(
      {
        onConnected: () => this.events.onStatus("connected"),
        onResolution: (r) => this.events.onResolution(r),
        onFrame: (f) => this.events.onFrame(f),
        onProtocolError: (detail) => this.fail(detail),
      },
      (bytes) => {
        // our encoders always allocate plain ArrayBuffers; the cast just
        // narrows ArrayBufferLike for RTCDataChannel.send's signature
        if (this.ctl?.readyState === "open") this.ctl.send(bytes as Uint8Array<ArrayBuffer>);
      },
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

    const media = pc.createDataChannel("tether-media", {
      ordered: false,
      maxRetransmits: 0,
    });
    media.binaryType = "arraybuffer";
    media.onmessage = (e) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      const wire = this.reassembler.onChunk(e.data);
      if (wire) session.onMessage(wire);
    };

    const signaling = new SignalingClient({
      onRegistered: () => {
        void (async () => {
          try {
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

  close(): void {
    this.signaling?.close();
    this.signaling = null;
    this.session = null;
    this.ctl = null;
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
