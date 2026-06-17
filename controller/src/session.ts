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
  encodeClipboardData,
  encodeHello,
  encodeInputEvent,
  encodeTextInput,
} from "./protocol";

export interface SessionEvents {
  onConnected(): void;
  onResolution(resolution: Resolution): void;
  onFrame(frame: FrameData): void;
  onClipboard(text: string): void;
  /** Protocol-level failure; the caller should tear down the transport. */
  onProtocolError(detail: string): void;
}

export class ProtocolSession {
  private handshaken = false;

  constructor(
    private readonly events: SessionEvents,
    private readonly sendBytes: (bytes: Uint8Array) => void,
  ) {}

  /** Call when the transport is ready to carry bytes: sends our Hello. */
  start(): void {
    this.handshaken = false;
    this.sendBytes(
      encodeHello({
        version: PROTOCOL_VERSION,
        role: Role.Controller,
        capabilities: CAP_CAN_CONTROL,
      }),
    );
  }

  get connected(): boolean {
    return this.handshaken;
  }

  sendInput(ev: InputEvent): void {
    if (this.handshaken) {
      this.sendBytes(encodeInputEvent(ev));
    }
  }

  sendClipboard(text: string): void {
    if (this.handshaken) {
      this.sendBytes(encodeClipboardData({ text }));
    }
  }

  sendText(text: string): void {
    if (this.handshaken && text.length > 0) {
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
    if (!this.handshaken) {
      if (message.type !== "hello") {
        this.events.onProtocolError("protocol error: expected Hello first");
        return;
      }
      if (message.version !== PROTOCOL_VERSION || message.role !== Role.Host) {
        this.events.onProtocolError(`incompatible host (version ${message.version})`);
        return;
      }
      this.handshaken = true;
      this.events.onConnected();
      return;
    }
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
