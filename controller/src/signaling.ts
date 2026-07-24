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
  | {
      type: "register";
      device_id: string;
      name: string;
      caps: Caps;
      auth: string;
      // Omitted entirely (not null) when this browser has no Ed25519, so the
      // wire shape matches what the Rust `Option` fields expect.
      pubkey?: string;
      sig?: string;
    }
  | { type: "offer"; target: string; sdp: string }
  | { type: "answer"; target: string; sdp: string }
  | { type: "ice"; target: string; candidate: string };

export type ServerMessage =
  | { type: "challenge"; nonce: string }
  | { type: "registered"; ice_servers: IceServer[] }
  | { type: "peers"; peers: PeerInfo[] }
  | { type: "offer"; from: string; sdp: string }
  | { type: "answer"; from: string; sdp: string }
  | { type: "ice"; from: string; candidate: string }
  | { type: "error"; code: string; message: string };

/**
 * Normalize whatever the user typed in the signal field into a URL.
 *
 * A bare `host:port` has to pick a scheme, and picking wrong is fatal rather
 * than merely insecure: an `https://` page is forbidden by mixed-content rules
 * from opening a `ws://` socket, and the controller must be served over HTTPS
 * anyway because pairing needs WebCrypto. So mirror the page's own scheme.
 */
export function signalUrl(input: string, pageProtocol = location.protocol): string {
  const trimmed = input.trim();
  if (/^wss?:\/\//i.test(trimmed)) return trimmed;
  // Paste-a-URL convenience: https://host/ws and http://host/ws map onto the
  // WebSocket schemes rather than being rejected.
  if (/^https:\/\//i.test(trimmed)) return `wss://${trimmed.slice(8)}`;
  if (/^http:\/\//i.test(trimmed)) return `ws://${trimmed.slice(7)}`;
  const scheme = pageProtocol === "https:" ? "wss" : "ws";
  const path = trimmed.includes("/") ? "" : "/ws";
  return `${scheme}://${trimmed}${path}`;
}

export type RegisterFields = Omit<ClientMessage & { type: "register" }, "type">;

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

  /**
   * `buildRegistration` is called with the server's per-connection nonce rather
   * than on open: the registration carries a signature over that nonce, so it
   * cannot be assembled until the `challenge` arrives.
   */
  connect(
    url: string,
    buildRegistration: (nonce: string) => Promise<RegisterFields>,
  ): void {
    this.close();
    const ws = new WebSocket(url);
    this.ws = ws;
    ws.onmessage = (event: MessageEvent) => {
      if (typeof event.data !== "string") return;
      const msg = parseServerMessage(event.data);
      if (!msg) return;
      switch (msg.type) {
        case "challenge":
          void buildRegistration(msg.nonce).then(
            (fields) => {
              // The socket may have been replaced while we were signing.
              if (this.ws === ws) this.send({ type: "register", ...fields });
            },
            () => this.events.onError("identity_failed", "could not sign the registration"),
          );
          break;
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
