import { describe, expect, it } from "vitest";
import {
  ACTIVATION_EVENTS,
  ScreenWakeLock,
  type WakeLockNavigator,
  type WakeLockSentinelLike,
  syncOnUserActivation,
} from "./wakelock";

class FakeSentinel implements WakeLockSentinelLike {
  released = false;
  private listeners: (() => void)[] = [];
  addEventListener(_type: "release", fn: () => void): void {
    this.listeners.push(fn);
  }
  release(): Promise<void> {
    if (!this.released) {
      this.released = true;
      for (const fn of this.listeners) fn();
    }
    return Promise.resolve();
  }
}

function harness(opts: { fail?: boolean | "sync"; visible?: boolean } = {}) {
  const sentinels: FakeSentinel[] = [];
  let resolveNext: ((s: FakeSentinel) => void) | null = null;
  let deferred = false;
  let calls = 0;
  const nav: WakeLockNavigator = {
    wakeLock: {
      request: (type) => {
        expect(type).toBe("screen");
        calls++;
        if (opts.fail === "sync") throw new TypeError("nope");
        if (opts.fail) return Promise.reject(new DOMException("denied", "NotAllowedError"));
        const s = new FakeSentinel();
        sentinels.push(s);
        if (deferred) return new Promise((r) => (resolveNext = () => r(s)));
        return Promise.resolve(s);
      },
    },
  };
  let visible = opts.visible ?? true;
  const lock = new ScreenWakeLock(nav, () => visible);
  return {
    lock,
    sentinels,
    calls: () => calls,
    setVisible: (v: boolean) => (visible = v),
    defer: () => (deferred = true),
    resolve: () => resolveNext?.(sentinels[sentinels.length - 1]!),
  };
}

describe("ScreenWakeLock", () => {
  it("acquires once and releases", async () => {
    const h = harness();
    await h.lock.acquire();
    await h.lock.acquire();
    expect(h.sentinels).toHaveLength(1);
    expect(h.lock.held).toBe(true);
    await h.lock.release();
    expect(h.sentinels[0]!.released).toBe(true);
    expect(h.lock.held).toBe(false);
  });

  it("re-requests after the system drops it (page hidden)", async () => {
    const h = harness();
    await h.lock.acquire();
    h.setVisible(false);
    await h.sentinels[0]!.release(); // what the system does on hide
    expect(h.lock.held).toBe(false);
    await h.lock.sync(); // still hidden: nothing to do
    expect(h.sentinels).toHaveLength(1);
    h.setVisible(true);
    await h.lock.sync();
    expect(h.sentinels).toHaveLength(2);
    expect(h.lock.held).toBe(true);
  });

  it("waits for visibility before the first request", async () => {
    const h = harness({ visible: false });
    await h.lock.acquire();
    expect(h.sentinels).toHaveLength(0);
    h.setVisible(true);
    await h.lock.sync();
    expect(h.lock.held).toBe(true);
  });

  it("sync does nothing unless a lock is wanted", async () => {
    const h = harness();
    await h.lock.sync();
    expect(h.sentinels).toHaveLength(0);
  });

  it("drops a lock granted after release() was called", async () => {
    const h = harness();
    h.defer();
    const p = h.lock.acquire();
    await h.lock.release();
    h.resolve();
    await p;
    expect(h.sentinels[0]!.released).toBe(true);
    expect(h.lock.held).toBe(false);
  });

  it("never throws: unsupported, refused, or throwing synchronously", async () => {
    const none = new ScreenWakeLock({}, () => true);
    await expect(none.acquire()).resolves.toBeUndefined();
    await expect(none.release()).resolves.toBeUndefined();

    const refused = harness({ fail: true });
    await expect(refused.lock.acquire()).resolves.toBeUndefined();
    expect(refused.lock.held).toBe(false);

    const broken = harness({ fail: "sync" });
    await expect(broken.lock.acquire()).resolves.toBeUndefined();
    // a later sync tries again rather than being stuck on the failed attempt
    await expect(broken.lock.sync()).resolves.toBeUndefined();
    expect(broken.calls()).toBe(2);
  });

  it("a request refused outside a user gesture is retried by the next sync", async () => {
    // WebKit: NotAllowedError unless inside a gesture. The connect-time
    // acquire is refused; the user's next tap (sync) gets the lock.
    let inGesture = false;
    const nav: WakeLockNavigator = {
      wakeLock: {
        request: () =>
          inGesture
            ? Promise.resolve(new FakeSentinel())
            : Promise.reject(new DOMException("Permission was denied", "NotAllowedError")),
      },
    };
    const lock = new ScreenWakeLock(nav, () => true);
    await lock.acquire();
    expect(lock.held).toBe(false);
    inGesture = true;
    await lock.sync();
    expect(lock.held).toBe(true);
  });
});

describe("syncOnUserActivation", () => {
  it("syncs on every activation event until unsubscribed", () => {
    const listeners = new Map<string, () => void>();
    const target = {
      addEventListener: (type: string, fn: () => void, capture: boolean) => {
        expect(capture).toBe(true);
        listeners.set(type, fn);
      },
      removeEventListener: (type: string, fn: () => void) => {
        if (listeners.get(type) === fn) listeners.delete(type);
      },
    };
    let syncs = 0;
    const stop = syncOnUserActivation({ sync: () => (syncs++, Promise.resolve()) }, target);
    expect([...listeners.keys()]).toEqual([...ACTIVATION_EVENTS]);
    expect(ACTIVATION_EVENTS).toContain("pointerup");
    expect(ACTIVATION_EVENTS).not.toContain("pointerdown"); // no activation on iOS
    for (const fn of listeners.values()) fn();
    expect(syncs).toBe(ACTIVATION_EVENTS.length);
    stop();
    expect(listeners.size).toBe(0);
  });
});
