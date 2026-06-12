// LAN WebSocket transport (Phase 1 path, kept as a fallback transport).
// Protocol logic lives in ProtocolSession; this file only owns the socket.

import type { FrameData, InputEvent, Resolution } from "./protocol";
import { ProtocolSession } from "./session";

export type ConnectionStatus = "connecting" | "connected" | "closed";

export interface ConnectionEvents {
  onStatus(status: ConnectionStatus, detail?: string): void;
  onResolution(resolution: Resolution): void;
  onFrame(frame: FrameData): void;
  onClipboard(text: string): void;
}

/** Common shape of all transports (WS today, WebRTC in webrtc.ts). */
export interface Transport {
  close(): void;
  sendInput(ev: InputEvent): void;
  sendClipboard(text: string): void;
  readonly connected: boolean;
}

export class TetherConnection implements Transport {
  private ws: WebSocket | null = null;
  private session: ProtocolSession | null = null;

  constructor(private readonly events: ConnectionEvents) {}

  connect(hostPort: string): void {
    this.close();
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

    const session = new ProtocolSession(
      {
        onConnected: () => this.events.onStatus("connected"),
        onResolution: (r) => this.events.onResolution(r),
        onFrame: (f) => this.events.onFrame(f),
        onClipboard: (text) => this.events.onClipboard(text),
        onProtocolError: (detail) => this.fail(detail),
      },
      (bytes) => {
        if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(bytes);
      },
    );
    this.session = session;

    ws.onopen = () => session.start();
    ws.onmessage = (event: MessageEvent) => {
      if (event.data instanceof ArrayBuffer) session.onMessage(event.data);
    };
    ws.onclose = () => {
      if (this.ws === ws) {
        this.ws = null;
        this.events.onStatus("closed");
      }
    };
  }

  get connected(): boolean {
    return (this.session?.connected ?? false) && this.ws?.readyState === WebSocket.OPEN;
  }

  sendInput(ev: InputEvent): void {
    this.session?.sendInput(ev);
  }

  sendClipboard(text: string): void {
    this.session?.sendClipboard(text);
  }

  close(): void {
    const ws = this.ws;
    this.ws = null;
    this.session = null;
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
