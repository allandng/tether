// Live machine directory (Phase 8): a controller registration with the signal
// server held open for as long as the app is, so the machine list shows which
// hosts are online without connecting to any of them.
//
// Mobile browsers drop this socket whenever the tab is backgrounded, so it
// reconnects with backoff, immediately when the page becomes visible again,
// and when the network comes back. It registers under its own device id
// (`<id>-dir`): reusing the session's id would make the server evict one
// registration whenever the other connects.

import { loadOrCreateIdentity } from "./identity";
import { type PeerInfo, SignalingClient } from "./signaling";

export type DirectoryState = "off" | "connecting" | "online" | "error";

export interface DirectoryEvents {
  onHosts(peers: PeerInfo[]): void;
  onState(state: DirectoryState, detail?: string): void;
}

export interface DirectoryConfig {
  url: string;
  secret: string;
  deviceId: string;
  deviceName: string;
}

const BASE_MS = 1000;
const MAX_MS = 30_000;
/** Refusals retrying can't fix — leave them on screen instead. */
const FATAL = new Set(["bad_auth", "identity_mismatch", "bad_identity", "identity_required"]);

export class Directory {
  private client: SignalingClient | null = null;
  private config: DirectoryConfig | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private delay = BASE_MS;
  private state: DirectoryState = "off";
  private readonly wake = () => {
    if (document.visibilityState === "visible" && this.config && this.state !== "online") {
      this.delay = BASE_MS;
      this.connectNow();
    }
  };

  constructor(private readonly events: DirectoryEvents) {
    document.addEventListener("visibilitychange", this.wake);
    window.addEventListener("online", this.wake);
  }

  start(config: DirectoryConfig): void {
    this.config = config;
    this.delay = BASE_MS;
    this.connectNow();
  }

  stop(): void {
    this.config = null;
    this.clearTimer();
    this.client?.close();
    this.client = null;
    this.setState("off");
  }

  private setState(state: DirectoryState, detail?: string): void {
    this.state = state;
    this.events.onState(state, detail);
  }

  private clearTimer(): void {
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
  }

  private connectNow(): void {
    const config = this.config;
    if (!config) return;
    this.clearTimer();
    this.client?.close();
    this.setState("connecting");
    const id = `${config.deviceId}-dir`;
    const client = new SignalingClient({
      onRegistered: () => {
        this.delay = BASE_MS;
        this.setState("online");
      },
      onPeers: (peers) => this.events.onHosts(peers.filter((p) => p.caps.can_host)),
      onAnswer: () => {},
      onIce: () => {},
      onError: (code, message) => {
        if (FATAL.has(code)) {
          this.client?.close();
          this.client = null;
          this.clearTimer();
          this.setState("error", message || code);
        }
      },
      onClosed: () => {
        if (this.client !== client) return;
        this.client = null;
        this.retry();
      },
    });
    this.client = client;
    try {
      client.connect(config.url, async (nonce) => {
        const identity = await loadOrCreateIdentity();
        return {
          device_id: id,
          name: config.deviceName,
          caps: { can_host: false, can_control: true },
          auth: config.secret,
          ...(identity ? { pubkey: identity.pubkey, sig: await identity.sign(nonce, id) } : {}),
        };
      });
    } catch (e) {
      this.client = null;
      this.setState("error", `bad signal server address: ${e}`);
    }
  }

  private retry(): void {
    if (!this.config || this.state === "error") return;
    const wait = Math.round(this.delay * (0.7 + Math.random() * 0.6));
    this.delay = Math.min(this.delay * 2, MAX_MS);
    this.setState("connecting", `retrying in ${Math.round(wait / 1000)}s`);
    this.timer = setTimeout(() => {
      this.timer = null;
      // a backgrounded tab waits for `wake` instead of burning retries
      if (document.visibilityState === "visible") this.connectNow();
    }, wait);
  }
}
