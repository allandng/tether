// LAN WebSocket transport (Phase 1 path, kept as a fallback transport).
// Protocol logic lives in ProtocolSession; this file only owns the socket.

import {
  type DisplayInfo,
  type FileMessage,
  type FrameData,
  type HostInfo,
  type InputEvent,
  type Resolution,
  encodeMessage,
} from "./protocol";
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
  onHostInfo(info: HostInfo): void;
  onFileMessage(msg: FileMessage): void;
  /** The host's system-audio track arrived (WebRTC with `--audio` only). */
  onAudioTrack?(stream: MediaStream): void;
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
  /** Send a file-transfer message (on the bulk path where there is one). */
  sendFile(msg: FileMessage): void;
  /** Resolves when the transport can take more file data. */
  drainFiles(): Promise<void>;
  /** Submit a host pairing code (after onPairingRequired). */
  submitPairingCode(code: string): void;
  readonly connected: boolean;
}

/** Pause file data while this much is queued unsent on the transport. */
export const FILE_HIGH_WATER = 1024 * 1024;

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
        onHostInfo: (i) => this.events.onHostInfo(i),
        onFileMessage: (m) => this.events.onFileMessage(m),
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

  sendFile(msg: FileMessage): void {
    // One socket carries everything on LAN: file data interleaves with frames.
    if (this.connected) this.ws?.send(encodeMessage(msg));
  }

  async drainFiles(): Promise<void> {
    // WebSocket has no low-water event; poll its send buffer.
    while (this.ws && this.ws.bufferedAmount > FILE_HIGH_WATER) {
      await new Promise((r) => setTimeout(r, 10));
    }
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
