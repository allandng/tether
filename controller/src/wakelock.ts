// Screen Wake Lock while a session is connected: a phone that's only watching
// the remote screen would otherwise auto-lock, iOS would freeze the page, and
// the session would die with it. The system drops the lock whenever the page
// is hidden, so it's re-requested on the way back. Strictly best effort:
// unsupported browsers, refusals (low-power mode, policy) and races with a
// release all end quietly — nothing here throws.
//
// WebKit grants the lock only inside a user gesture (NotAllowedError
// otherwise), but the connect that wants it completes asynchronously, outside
// any gesture — so on iOS the first request is always refused, as is the
// re-request on resume. syncOnUserActivation retries on the user's next
// interaction while the lock is wanted and not held.

export interface WakeLockSentinelLike {
  readonly released: boolean;
  release(): Promise<void>;
  addEventListener(type: "release", fn: () => void): void;
}

export interface WakeLockNavigator {
  wakeLock?: { request(type: "screen"): Promise<WakeLockSentinelLike> };
}

export class ScreenWakeLock {
  private sentinel: WakeLockSentinelLike | null = null;
  private pending: Promise<void> | null = null;
  private wanted = false;

  constructor(
    private readonly nav: WakeLockNavigator,
    private readonly isVisible: () => boolean,
  ) {}

  get held(): boolean {
    return this.sentinel !== null && !this.sentinel.released;
  }

  /** Keep the screen on (whenever the page is visible) until release(). */
  acquire(): Promise<void> {
    this.wanted = true;
    return this.sync();
  }

  release(): Promise<void> {
    this.wanted = false;
    const s = this.sentinel;
    this.sentinel = null;
    return s ? s.release().catch(() => {}) : Promise.resolve();
  }

  /** Re-request if wanted but not held — call when the page becomes visible. */
  sync(): Promise<void> {
    if (this.pending) return this.pending;
    if (!this.wanted || this.held || !this.isVisible()) return Promise.resolve();
    const wl = this.nav.wakeLock;
    if (typeof wl?.request !== "function") return Promise.resolve();
    const p = this.request(wl);
    this.pending = p;
    // (cleared here, not inside request(): a synchronous throw would finish
    // it before `pending` was even assigned)
    void p.then(() => {
      if (this.pending === p) this.pending = null;
    });
    return p;
  }

  private async request(wl: NonNullable<WakeLockNavigator["wakeLock"]>): Promise<void> {
    try {
      const s = await wl.request("screen");
      if (!this.wanted) {
        await s.release().catch(() => {}); // released while the request was out
        return;
      }
      this.sentinel = s;
      s.addEventListener("release", () => {
        if (this.sentinel === s) this.sentinel = null;
      });
    } catch {
      // NotAllowedError (hidden, low battery, policy): try again next sync
    }
  }
}

/** Events that carry user activation, measured on iOS 26 Safari: pointerup
 * (and touchend, click) grant the lock; pointerdown does not. keydown covers
 * hardware keyboards. */
export const ACTIVATION_EVENTS = ["pointerup", "keydown"] as const;

interface ActivationTarget {
  addEventListener(type: string, fn: () => void, capture: boolean): void;
  removeEventListener(type: string, fn: () => void, capture: boolean): void;
}

/** Call `lock.sync()` on every user interaction with `target` (capture phase,
 * so no handler can swallow it). Cheap when there's nothing to do: sync()
 * returns at once unless a lock is wanted and not held. Returns an
 * unsubscribe. */
export function syncOnUserActivation(lock: Pick<ScreenWakeLock, "sync">, target: ActivationTarget): () => void {
  const onActivation = () => void lock.sync();
  for (const type of ACTIVATION_EVENTS) target.addEventListener(type, onActivation, true);
  return () => {
    for (const type of ACTIVATION_EVENTS) target.removeEventListener(type, onActivation, true);
  };
}
