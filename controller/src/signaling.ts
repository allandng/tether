// Client for tether-signal: JSON over WebSocket, mirroring
// crates/tether-signal/src/protocol.rs (serde `tag = "type"`, snake_case).
// signaling.test.ts pins the JSON shapes against the Rust side.

export interface Caps {
  can_host: boolean;
  can_control: boolean;
}

export interface PeerInfo {
  device_id: string;
  name: string;
  caps: Caps;
}

export interface IceServer {
  urls: string[];
  username?: string;
  credential?: string;
}

export type ClientMessage =
  | { type: "register"; device_id: string; name: string; caps: Caps; auth: string }
  | { type: "offer"; target: string; sdp: string }
  | { type: "answer"; target: string; sdp: string }
  | { type: "ice"; target: string; candidate: string };

export type ServerMessage =
  | { type: "registered"; ice_servers: IceServer[] }
  | { type: "peers"; peers: PeerInfo[] }
  | { type: "offer"; from: string; sdp: string }
  | { type: "answer"; from: string; sdp: string }
  | { type: "ice"; from: string; candidate: string }
  | { type: "error"; code: string; message: string };

export interface SignalingEvents {
  onRegistered(iceServers: IceServer[]): void;
  onPeers(peers: PeerInfo[]): void;
  onAnswer(from: string, sdp: string): void;
  onIce(from: string, candidate: string): void;
  onError(code: string, message: string): void;
  onClosed(): void;
}

export function parseServerMessage(text: string): ServerMessage | null {
  try {
    const msg = JSON.parse(text);
    return typeof msg?.type === "string" ? (msg as ServerMessage) : null;
  } catch {
    return null;
  }
}

export class SignalingClient {
  private ws: WebSocket | null = null;

  constructor(private readonly events: SignalingEvents) {}

  connect(url: string, registration: Omit<ClientMessage & { type: "register" }, "type">): void {
    this.close();
    const ws = new WebSocket(url);
    this.ws = ws;
    ws.onopen = () => this.send({ type: "register", ...registration });
    ws.onmessage = (event: MessageEvent) => {
      if (typeof event.data !== "string") return;
      const msg = parseServerMessage(event.data);
      if (!msg) return;
      switch (msg.type) {
        case "registered":
          this.events.onRegistered(msg.ice_servers ?? []);
          break;
        case "peers":
          this.events.onPeers(msg.peers);
          break;
        case "answer":
          this.events.onAnswer(msg.from, msg.sdp);
          break;
        case "ice":
          this.events.onIce(msg.from, msg.candidate);
          break;
        case "error":
          this.events.onError(msg.code, msg.message);
          break;
        case "offer":
          break; // controllers never receive offers
      }
    };
    ws.onclose = () => {
      if (this.ws === ws) {
        this.ws = null;
        this.events.onClosed();
      }
    };
  }

  send(msg: ClientMessage): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
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
}
