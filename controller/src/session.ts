// Transport-independent protocol session: the Hello handshake and message
// dispatch shared by the LAN WebSocket transport and the WebRTC data-channel
// transport. Owns no socket — callers feed bytes in and provide a byte sink.

import {
  CAP_AUDIO,
  CAP_CAN_CONTROL,
  CAP_FILES,
  type DisplayInfo,
  type FileMessage,
  type FrameData,
  type HostInfo,
  type InputEvent,
  type Resolution,
  PROTOCOL_VERSION,
  Role,
  decodeMessage,
  encodeAuth,
  encodeClipboardData,
  encodeHello,
  encodeInputEvent,
  encodePairRequest,
  encodeSelectDisplay,
  encodeTextInput,
} from "./protocol";
import { normalizeCode, pairingProof } from "./pairing";

const HANDSHAKE_TIMEOUT_MS = 10_000;

/** Per-host identity + token storage + transport-specific channel binding the
 * session needs to authenticate or pair. */
export interface AuthContext {
  deviceId: string;
  deviceName: string;
  getToken(): string | null;
  setToken(token: string): void;
  /** Channel binding for the pairing proof (DTLS-fp hash / WS constant). */
  channelBinding(): Promise<Uint8Array>;
}

export interface SessionEvents {
  onConnected(): void;
  onResolution(resolution: Resolution): void;
  onFrame(frame: FrameData): void;
  onClipboard(text: string): void;
  onDisplays(displays: DisplayInfo[]): void;
  /** What the host is (older hosts never send it). */
  onHostInfo(info: HostInfo): void;
  /** A file-transfer message from the host. */
  onFileMessage(msg: FileMessage): void;
  /** Host rejected our token (or we have none) — the user must enter the
   * host's pairing code. Call `submitPairingCode` to proceed. */
  onPairingRequired(): void;
  /** A pairing attempt with a code was rejected. */
  onPairingFailed(): void;
  /** Protocol-level failure; the caller should tear down the transport. */
  onProtocolError(detail: string): void;
}

type Phase = "hello" | "authenticating" | "pairing" | "connected";

export class ProtocolSession {
  private phase: Phase = "hello";
  private handshakeTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly events: SessionEvents,
    private readonly sendBytes: (bytes: Uint8Array) => void,
    private readonly auth: AuthContext,
    /** True on transports that can receive an audio track (WebRTC). */
    private readonly canPlayAudio = false,
  ) {}

  /** Call when the transport is ready to carry bytes: sends our Hello. */
  start(): void {
    this.phase = "hello";
    this.armHandshakeTimeout();
    this.sendBytes(
      encodeHello({
        version: PROTOCOL_VERSION,
        role: Role.Controller,
        capabilities: CAP_CAN_CONTROL | CAP_FILES | (this.canPlayAudio ? CAP_AUDIO : 0),
      }),
    );
  }

  /** A stalled handshake (no host response, lost pairing reply) must not hang
   * the UI silently; abort it so the transport tears down. Waiting on a human
   * to enter a pairing code is exempt: the timer is cleared when the host asks
   * for one, and the pairing phase never trips it. */
  private armHandshakeTimeout(): void {
    if (this.handshakeTimer) clearTimeout(this.handshakeTimer);
    this.handshakeTimer = setTimeout(() => {
      if (this.phase !== "connected" && this.phase !== "pairing") {
        this.events.onProtocolError("handshake timed out");
      }
    }, HANDSHAKE_TIMEOUT_MS);
  }

  private clearHandshakeTimeout(): void {
    if (this.handshakeTimer) {
      clearTimeout(this.handshakeTimer);
      this.handshakeTimer = null;
    }
  }

  private connect(): void {
    this.clearHandshakeTimeout();
    this.phase = "connected";
    this.events.onConnected();
  }

  get connected(): boolean {
    return this.phase === "connected";
  }

  /** User entered the host's pairing code; compute the channel-bound proof and
   * send a PairRequest. */
  async submitPairingCode(code: string): Promise<void> {
    if (this.phase !== "authenticating") return; // ignore double-submit / stray calls
    try {
      const binding = await this.auth.channelBinding();
      const proof = await pairingProof(normalizeCode(code), binding);
      this.phase = "pairing";
      this.sendBytes(
        encodePairRequest({ deviceId: this.auth.deviceId, name: this.auth.deviceName, proof }),
      );
    } catch (e) {
      this.events.onProtocolError(`pairing failed: ${e}`);
    }
  }

  sendInput(ev: InputEvent): void {
    if (this.connected) {
      this.sendBytes(encodeInputEvent(ev));
    }
  }

  sendClipboard(text: string): void {
    if (this.connected) {
      this.sendBytes(encodeClipboardData({ text }));
    }
  }

  sendSelectDisplay(id: number): void {
    if (this.connected) {
      this.sendBytes(encodeSelectDisplay({ id }));
    }
  }

  sendText(text: string): void {
    if (this.connected && text.length > 0) {
      this.sendBytes(encodeTextInput({ text }));
    }
  }

  /** Feed one complete wire message from the transport. */
  onMessage(data: ArrayBuffer | Uint8Array): void {
    const result = decodeMessage(data);
    if (!result.ok) {
      if (result.reason === "unknown-type") return; // forward compat: skip
      console.warn("dropping corrupt message:", result.detail);
      return;
    }
    const message = result.message;

    switch (this.phase) {
      case "hello": {
        if (message.type !== "hello") {
          this.events.onProtocolError("protocol error: expected Hello first");
          return;
        }
        if (message.version !== PROTOCOL_VERSION || message.role !== Role.Host) {
          this.events.onProtocolError(`incompatible host (version ${message.version})`);
          return;
        }
        // host Hello accepted → authenticate with our stored token (may be empty)
        this.phase = "authenticating";
        this.sendBytes(
          encodeAuth({ deviceId: this.auth.deviceId, token: this.auth.getToken() ?? "" }),
        );
        return;
      }
      case "authenticating": {
        if (message.type === "auth_result") {
          if (message.ok) {
            this.connect();
          } else {
            this.clearHandshakeTimeout(); // the user may take a while (or leave the app)
            this.events.onPairingRequired(); // need a code from the host
          }
        } else {
          this.events.onProtocolError(`unexpected ${message.type} during auth`);
        }
        return;
      }
      case "pairing": {
        if (message.type === "pair_result") {
          if (message.ok) {
            this.auth.setToken(message.token);
            this.connect();
          } else {
            this.phase = "authenticating"; // let the user retry a code
            this.events.onPairingFailed();
          }
        } else {
          this.events.onProtocolError(`unexpected ${message.type} during pairing`);
        }
        return;
      }
      case "connected": {
        switch (message.type) {
          case "resolution":
            this.events.onResolution(message);
            break;
          case "frame":
            this.events.onFrame(message);
            break;
          case "clipboard":
            this.events.onClipboard(message.text);
            break;
          case "displays":
            this.events.onDisplays(message.displays);
            break;
          case "host_info":
            this.events.onHostInfo(message);
            break;
          case "file_offer":
          case "file_accept":
          case "file_chunk":
          case "file_end":
          case "file_request":
            this.events.onFileMessage(message);
            break;
          default:
            console.warn("unexpected message from host:", message.type);
        }
      }
    }
  }
}
