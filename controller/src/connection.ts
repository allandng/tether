// WebSocket connection to tetherd with the Hello handshake.

import {
  CAP_CAN_CONTROL,
  type FrameData,
  type InputEvent,
  type Resolution,
  PROTOCOL_VERSION,
  Role,
  decodeMessage,
  encodeHello,
  encodeInputEvent,
} from "./protocol";

export type ConnectionStatus = "connecting" | "connected" | "closed";

export interface ConnectionEvents {
  onStatus(status: ConnectionStatus, detail?: string): void;
  onResolution(resolution: Resolution): void;
  onFrame(frame: FrameData): void;
}

export class TetherConnection {
  private ws: WebSocket | null = null;
  private handshaken = false;

  constructor(private readonly events: ConnectionEvents) {}

  connect(hostPort: string): void {
    this.close();
    this.handshaken = false;
    this.events.onStatus("connecting");

    let ws: WebSocket;
    try {
      ws = new WebSocket(`ws://${hostPort}`);
    } catch (e) {
      this.events.onStatus("closed", `invalid address: ${e}`);
      return;
    }
    this.ws = ws;
    ws.binaryType = "arraybuffer";

    ws.onopen = () => {
      ws.send(
        encodeHello({
          version: PROTOCOL_VERSION,
          role: Role.Controller,
          capabilities: CAP_CAN_CONTROL,
        }),
      );
    };

    ws.onmessage = (event: MessageEvent) => {
      if (!(event.data instanceof ArrayBuffer)) return;
      const result = decodeMessage(event.data);
      if (!result.ok) {
        if (result.reason === "unknown-type") return; // forward compat: skip
        console.warn("dropping corrupt message:", result.detail);
        return;
      }
      const message = result.message;
      if (!this.handshaken) {
        if (message.type !== "hello") {
          this.fail("protocol error: expected Hello first");
          return;
        }
        if (message.version !== PROTOCOL_VERSION || message.role !== Role.Host) {
          this.fail(`incompatible host (version ${message.version})`);
          return;
        }
        this.handshaken = true;
        this.events.onStatus("connected");
        return;
      }
      switch (message.type) {
        case "resolution":
          this.events.onResolution(message);
          break;
        case "frame":
          this.events.onFrame(message);
          break;
        default:
          console.warn("unexpected message from host:", message.type);
      }
    };

    ws.onclose = () => {
      if (this.ws === ws) {
        this.ws = null;
        this.events.onStatus("closed");
      }
    };
    ws.onerror = () => {
      // onclose always follows; nothing useful in the error event itself
    };
  }

  get connected(): boolean {
    return this.handshaken && this.ws?.readyState === WebSocket.OPEN;
  }

  sendInput(ev: InputEvent): void {
    if (this.connected) {
      this.ws!.send(encodeInputEvent(ev));
    }
  }

  close(): void {
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onclose = null;
      ws.close();
    }
  }

  private fail(detail: string): void {
    this.close();
    this.events.onStatus("closed", detail);
  }
}
