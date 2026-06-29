// LAN WebSocket transport (Phase 1 path, kept as a fallback transport).
// Protocol logic lives in ProtocolSession; this file only owns the socket.

import type { DisplayInfo, FrameData, InputEvent, Resolution } from "./protocol";
import { tokenStore, wsChannelBinding } from "./pairing";
import { type AuthContext, ProtocolSession } from "./session";

export type ConnectionStatus = "connecting" | "connected" | "closed";

/** Stable per-browser identity used for pairing + signaling. */
export interface Identity {
  deviceId: string;
  deviceName: string;
}

export interface ConnectionEvents {
  /** `fatal` marks a close that retrying won't fix (wrong secret, protocol
   * mismatch, bad target) — the UI should stop reconnecting and show `detail`. */
  onStatus(status: ConnectionStatus, detail?: string, fatal?: boolean): void;
  onResolution(resolution: Resolution): void;
  onFrame(frame: FrameData): void;
  onClipboard(text: string): void;
  onDisplays(displays: DisplayInfo[]): void;
  /** Host wants a pairing code entered. */
  onPairingRequired(): void;
  onPairingFailed(): void;
}

/** Common shape of all transports (WS today, WebRTC in webrtc.ts). */
export interface Transport {
  close(): void;
  sendInput(ev: InputEvent): void;
  sendClipboard(text: string): void;
  sendText(text: string): void;
  selectDisplay(id: number): void;
  /** Submit a host pairing code (after onPairingRequired). */
  submitPairingCode(code: string): void;
  readonly connected: boolean;
}

export class TetherConnection implements Transport {
  private ws: WebSocket | null = null;
  private session: ProtocolSession | null = null;

  constructor(
    private readonly events: ConnectionEvents,
    private readonly identity: Identity,
  ) {}

  connect(hostPort: string): void {
    this.close();
    this.events.onStatus("connecting");

    let ws: WebSocket;
    try {
      ws = new WebSocket(`ws://${hostPort}`);
    } catch (e) {
      this.events.onStatus("closed", `invalid address: ${e}`, true);
      return;
    }
    this.ws = ws;
    ws.binaryType = "arraybuffer";

    const authCtx: AuthContext = {
      deviceId: this.identity.deviceId,
      deviceName: this.identity.deviceName,
      getToken: () => tokenStore.get(hostPort),
      setToken: (t) => tokenStore.set(hostPort, t),
      channelBinding: wsChannelBinding, // direct LAN link → constant binding
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
        onProtocolError: (detail) => this.fail(detail, true),
      },
      (bytes) => {
        if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(bytes);
      },
      authCtx,
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

  sendText(text: string): void {
    this.session?.sendText(text);
  }

  selectDisplay(id: number): void {
    this.session?.sendSelectDisplay(id);
  }

  submitPairingCode(code: string): void {
    void this.session?.submitPairingCode(code);
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

  private fail(detail: string, fatal = false): void {
    this.close();
    this.events.onStatus("closed", detail, fatal);
  }
}
