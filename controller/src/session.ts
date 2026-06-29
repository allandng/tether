// Transport-independent protocol session: the Hello handshake and message
// dispatch shared by the LAN WebSocket transport and the WebRTC data-channel
// transport. Owns no socket — callers feed bytes in and provide a byte sink.

import {
  CAP_CAN_CONTROL,
  type FrameData,
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
  encodeTextInput,
} from "./protocol";
import { pairingProof } from "./pairing";

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

  constructor(
    private readonly events: SessionEvents,
    private readonly sendBytes: (bytes: Uint8Array) => void,
    private readonly auth: AuthContext,
  ) {}

  /** Call when the transport is ready to carry bytes: sends our Hello. */
  start(): void {
    this.phase = "hello";
    this.sendBytes(
      encodeHello({
        version: PROTOCOL_VERSION,
        role: Role.Controller,
        capabilities: CAP_CAN_CONTROL,
      }),
    );
  }

  get connected(): boolean {
    return this.phase === "connected";
  }

  /** User entered the host's pairing code; compute the channel-bound proof and
   * send a PairRequest. */
  async submitPairingCode(code: string): Promise<void> {
    try {
      const binding = await this.auth.channelBinding();
      const proof = await pairingProof(code.trim().toUpperCase().replace(/-/g, ""), binding);
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
            this.phase = "connected";
            this.events.onConnected();
          } else {
            this.events.onPairingRequired(); // need a code from the host
          }
        }
        return;
      }
      case "pairing": {
        if (message.type === "pair_result") {
          if (message.ok) {
            this.auth.setToken(message.token);
            this.phase = "connected";
            this.events.onConnected();
          } else {
            this.phase = "authenticating"; // let the user retry a code
            this.events.onPairingFailed();
          }
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
          default:
            console.warn("unexpected message from host:", message.type);
        }
      }
    }
  }
}
