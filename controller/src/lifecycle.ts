// App lifecycle: iOS freezes a backgrounded tab or home-screen app
// (and one whose phone just locked) within seconds. Timers stop, so the
// session's reconnect backoff sits wherever it was, and a session that was
// connected keeps showing "connected" over a peer the host has long dropped
// (webrtc-rs marks it disconnected after 5 s without ICE consent, failed 25 s
// later). Nothing in the protocol pings. A heartbeat that stops beating says
// the page's timers didn't run — but not why: a frozen page can't beat, and
// neither, often, can a desktop tab that's merely hidden (Chrome and Edge
// wake a hidden WebSocket-only page's timers about once a minute after five
// minutes; Safari's App Nap is similar), while its sockets keep delivering.
// So the gap only raises the question and the transport answers it: anything
// received during the gap means the page ran and the link is alive. A link
// that went quiet across the gap gets a short grace period to show traffic
// before it's replaced. Reconnecting is cheap — the host lets a new offer
// take over the old session — but it aborts file transfers, so it must not
// happen to a healthy session.

/** A timer gap at least this long puts the connection in question. */
export const STALE_FREEZE_MS = 15_000;
/** Heartbeat period; a gap much longer than this means the timers didn't run. */
export const BEAT_MS = 1000;

export interface ResumeState {
  /** The user wants a session (not one they backed out of, nor a fatal close). */
  wantConnection: boolean;
  /** "connecting" | "connected" | "closed". */
  status: string;
  /** The transport still reports its channel open. */
  transportAlive: boolean;
  /** How long the current status has stood (wall clock, freeze included). */
  statusAgeMs: number;
  /** Longest stretch the page's timers didn't run since the last check
   * (frozen, or throttled while hidden). */
  frozenMs: number;
  /** How long since the transport last received anything (wall clock);
   * Infinity when there is no transport. */
  rxAgeMs: number;
}

/** What coming back to the app should do: leave the session alone, replace
 * it now, or `verify` — wait RESUME_GRACE_MS for traffic (ResumeProbe) and
 * replace it only if none comes. */
export type ResumeAction = "keep" | "reconnect" | "verify";

/** After a long gap with nothing received, how long a resumed page waits for
 * the host to show signs of life. macOS hosts repeat the last frame every
 * 250 ms, so a live one shows up well within it; an older host on a static
 * screen sends nothing at all and gets replaced, as before. */
export const RESUME_GRACE_MS = 2500;

/** Decide what coming back to the app (visible / online / restored) does. */
export function resumeAction(s: ResumeState): ResumeAction {
  if (!s.wantConnection) return "keep";
  // Sitting out a backoff delay that the freeze stretched: retry now.
  if (s.status === "closed") return "reconnect";
  if (s.status === "connected" && !s.transportAlive) return "reconnect";
  // Only a connection (or attempt) already standing before a long gap is in
  // question; one that started after it (an overdue retry timer that fired
  // first on resume) is fresh and must not be restarted.
  if (s.frozenMs < STALE_FREEZE_MS || s.statusAgeMs < s.frozenMs) return "keep";
  // Something arrived since the gap began: the page kept running (merely
  // throttled) and the link carried it, so it's alive.
  if (s.rxAgeMs < s.frozenMs) return "keep";
  // Nothing since before the gap: a frozen page whose peer may be gone, or a
  // host with nothing to say. An attempt still mid-handshake is stale.
  return s.status === "connected" ? "verify" : "reconnect";
}

/** The grace period behind a `verify`: reports silence if the transport
 * receives nothing within `graceMs` of start. One probe at a time. */
export class ResumeProbe {
  private timer: ReturnType<typeof setTimeout> | null = null;

  /** Call `onSilent` unless `lastRxAt()` (a Date.now() timestamp) moves to
   * now or later within `graceMs`. Replaces any probe already waiting. */
  start(lastRxAt: () => number, onSilent: () => void, graceMs = RESUME_GRACE_MS): void {
    this.cancel();
    const since = Date.now();
    this.timer = setTimeout(() => {
      this.timer = null;
      if (lastRxAt() < since) onSilent();
    }, graceMs);
  }

  cancel(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }
}

/** Remembers the longest gap between heartbeats: a frozen page can't beat
 * (and a throttled one beats late). */
export class FreezeDetector {
  private last: number;
  private longest = 0;

  constructor(now: number) {
    this.last = now;
  }

  beat(now: number): void {
    this.longest = Math.max(this.longest, now - this.last);
    this.last = now;
  }

  /** The longest freeze since the previous take, including one ending now.
   * Kept across beats so an overdue heartbeat that runs before the resume
   * event can't hide the freeze. */
  take(now: number): number {
    this.beat(now);
    const gap = this.longest;
    this.longest = 0;
    return gap;
  }
}

export type ResumeReason = "visible" | "online" | "pageshow";

export interface LifecycleHandlers {
  /** The page is usable again; `frozenMs` is the longest stretch its timers
   * didn't run (frozen, or throttled while hidden). */
  onResume(reason: ResumeReason, frozenMs: number): void;
  /** The page was hidden (app switched, phone locked); it may freeze next. */
  onHidden(): void;
}

interface Listenable {
  addEventListener(type: string, fn: (e: Event) => void): void;
  removeEventListener(type: string, fn: (e: Event) => void): void;
}

/** Wire the lifecycle events (document/window injected for tests). Returns
 * an unsubscribe. */
export function watchLifecycle(
  doc: Listenable & { readonly visibilityState: string },
  win: Listenable,
  handlers: LifecycleHandlers,
): () => void {
  const freeze = new FreezeDetector(Date.now());
  const beat = setInterval(() => freeze.beat(Date.now()), BEAT_MS);
  const resume = (reason: ResumeReason) => handlers.onResume(reason, freeze.take(Date.now()));

  const onVisibility = () => {
    if (doc.visibilityState === "visible") resume("visible");
    else handlers.onHidden();
  };
  const onOnline = () => resume("online");
  // bfcache restore: every timer and socket was suspended meanwhile.
  const onPageShow = (e: Event) => {
    if ((e as Event & { persisted?: boolean }).persisted) resume("pageshow");
  };

  doc.addEventListener("visibilitychange", onVisibility);
  win.addEventListener("online", onOnline);
  win.addEventListener("pageshow", onPageShow);
  return () => {
    clearInterval(beat);
    doc.removeEventListener("visibilitychange", onVisibility);
    win.removeEventListener("online", onOnline);
    win.removeEventListener("pageshow", onPageShow);
  };
}
