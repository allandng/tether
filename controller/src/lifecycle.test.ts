import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BEAT_MS,
  FreezeDetector,
  RESUME_GRACE_MS,
  ResumeProbe,
  type ResumeState,
  STALE_FREEZE_MS,
  resumeAction,
  watchLifecycle,
} from "./lifecycle";

const base: ResumeState = {
  wantConnection: true,
  status: "connected",
  transportAlive: true,
  statusAgeMs: 120_000,
  frozenMs: 1000,
  rxAgeMs: 200,
};

describe("resumeAction", () => {
  it("never reconnects a session the user left (or a fatal close)", () => {
    expect(resumeAction({ ...base, wantConnection: false, status: "closed" })).toBe("keep");
    expect(resumeAction({ ...base, wantConnection: false, frozenMs: 600_000, rxAgeMs: 600_000 })).toBe("keep");
  });

  it("cuts a backoff wait short: a closed session reconnects immediately", () => {
    expect(resumeAction({ ...base, status: "closed", transportAlive: false })).toBe("reconnect");
    expect(resumeAction({ ...base, status: "closed", transportAlive: false, frozenMs: 60_000 })).toBe("reconnect");
  });

  it("leaves a healthy session after a short (or no) gap", () => {
    expect(resumeAction(base)).toBe("keep");
    expect(resumeAction({ ...base, frozenMs: STALE_FREEZE_MS - 1, rxAgeMs: 600_000 })).toBe("keep");
  });

  it("replaces a session whose transport already died", () => {
    expect(resumeAction({ ...base, transportAlive: false })).toBe("reconnect");
    // a frozen iOS page whose socket closed meanwhile: no grace period
    expect(resumeAction({ ...base, transportAlive: false, frozenMs: 90_000, rxAgeMs: 95_000 })).toBe("reconnect");
  });

  it("keeps a throttled-but-alive desktop tab: traffic arrived during the gap", () => {
    // Chrome ran the hidden tab's timers once a minute; frames kept coming
    expect(resumeAction({ ...base, frozenMs: 60_000, rxAgeMs: 250 })).toBe("keep");
    // a quiet host that last spoke partway through the gap counts too
    expect(resumeAction({ ...base, frozenMs: 60_000, rxAgeMs: 59_000 })).toBe("keep");
  });

  it("verifies a session that went quiet across a long gap (frozen page, or a silent host)", () => {
    // a frozen iOS page whose transport still claims to be open
    expect(resumeAction({ ...base, frozenMs: 60_000, rxAgeMs: 61_000 })).toBe("verify");
    expect(resumeAction({ ...base, frozenMs: 60_000, rxAgeMs: Infinity })).toBe("verify");
  });

  it("restarts an attempt that was in flight across a long gap", () => {
    expect(
      resumeAction({
        ...base,
        status: "connecting",
        transportAlive: false,
        frozenMs: 60_000,
        statusAgeMs: 61_000,
        rxAgeMs: 61_000,
      }),
    ).toBe("reconnect");
  });

  it("does not restart an attempt (or a session) that began after the gap", () => {
    // the overdue retry timer fired first on resume: that attempt is fresh
    expect(
      resumeAction({
        ...base,
        status: "connecting",
        transportAlive: false,
        frozenMs: 60_000,
        statusAgeMs: 5,
        rxAgeMs: Infinity,
      }),
    ).toBe("keep");
    expect(resumeAction({ ...base, status: "connecting", transportAlive: false })).toBe("keep");
    expect(resumeAction({ ...base, frozenMs: 60_000, statusAgeMs: 10, rxAgeMs: 61_000 })).toBe("keep");
  });
});

describe("ResumeProbe", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("reports a transport that stays silent through the grace period (frozen page, dead peer)", () => {
    const probe = new ResumeProbe();
    let silent = 0;
    const lastRxAt = Date.now() - 90_000; // nothing since before the freeze
    probe.start(() => lastRxAt, () => silent++);
    vi.advanceTimersByTime(RESUME_GRACE_MS - 1);
    expect(silent).toBe(0);
    vi.advanceTimersByTime(1);
    expect(silent).toBe(1);
  });

  it("accepts traffic arriving within the grace period", () => {
    const probe = new ResumeProbe();
    let silent = 0;
    let lastRxAt = Date.now() - 90_000;
    probe.start(() => lastRxAt, () => silent++);
    vi.advanceTimersByTime(250);
    lastRxAt = Date.now(); // the host's repeated frame
    vi.advanceTimersByTime(RESUME_GRACE_MS);
    expect(silent).toBe(0);
  });

  it("cancel stops it, and a new start replaces the old one", () => {
    const probe = new ResumeProbe();
    let silent = 0;
    probe.start(() => 0, () => silent++);
    probe.cancel();
    vi.advanceTimersByTime(RESUME_GRACE_MS * 2);
    expect(silent).toBe(0);
    probe.start(() => 0, () => silent++);
    vi.advanceTimersByTime(RESUME_GRACE_MS / 2);
    probe.start(() => 0, () => silent++);
    vi.advanceTimersByTime(RESUME_GRACE_MS * 2);
    expect(silent).toBe(1);
  });
});

describe("FreezeDetector", () => {
  it("reports the longest gap between beats, then resets", () => {
    const f = new FreezeDetector(0);
    f.beat(1000);
    f.beat(41_000); // frozen 40 s
    f.beat(42_000); // an overdue beat that ran before the resume event
    expect(f.take(42_100)).toBe(40_000);
    expect(f.take(43_000)).toBe(900);
  });

  it("counts a freeze still in progress at take time", () => {
    const f = new FreezeDetector(0);
    expect(f.take(30_000)).toBe(30_000);
  });
});

describe("watchLifecycle", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function setup() {
    const doc = Object.assign(new EventTarget(), { visibilityState: "visible" });
    const win = new EventTarget();
    const resumes: [string, number][] = [];
    let hidden = 0;
    const stop = watchLifecycle(doc, win, {
      onResume: (reason, frozenMs) => resumes.push([reason, frozenMs]),
      onHidden: () => hidden++,
    });
    const setVisibility = (v: string) => {
      doc.visibilityState = v;
      doc.dispatchEvent(new Event("visibilitychange"));
    };
    return { doc, win, resumes, hidden: () => hidden, stop, setVisibility };
  }

  it("reports hide, then a resume with the freeze length", () => {
    const h = setup();
    vi.advanceTimersByTime(BEAT_MS * 3);
    h.setVisibility("hidden");
    expect(h.hidden()).toBe(1);
    // frozen: the clock moves, no timers run
    vi.setSystemTime(Date.now() + 90_000);
    h.setVisibility("visible");
    expect(h.resumes).toHaveLength(1);
    expect(h.resumes[0]![0]).toBe("visible");
    expect(h.resumes[0]![1]).toBeGreaterThanOrEqual(90_000);
  });

  it("reports a page whose hidden timers kept their pace as unfrozen", () => {
    const h = setup();
    h.setVisibility("hidden");
    vi.advanceTimersByTime(90_000); // hidden, but the heartbeat kept beating every second
    h.setVisibility("visible");
    expect(h.resumes[0]![1]).toBeLessThanOrEqual(BEAT_MS);
  });

  it("can't tell a throttled hidden tab from a frozen one, so the transport decides", () => {
    const h = setup();
    h.setVisibility("hidden");
    // Desktop Chrome/Edge after 5 min hidden: timers wake about once a minute.
    for (let i = 0; i < 5; i++) {
      vi.setSystemTime(Date.now() + 60_000 - BEAT_MS);
      vi.advanceTimersByTime(BEAT_MS);
    }
    vi.setSystemTime(Date.now() + 30_000);
    h.setVisibility("visible");
    const frozenMs = h.resumes[0]![1];
    expect(frozenMs).toBeGreaterThanOrEqual(STALE_FREEZE_MS);
    // ...but the LAN socket kept delivering frames: the healthy session stays
    expect(resumeAction({ ...base, frozenMs, rxAgeMs: 250 })).toBe("keep");
    // while one silent since before the gap is checked before it's replaced
    expect(resumeAction({ ...base, frozenMs, rxAgeMs: 400_000 })).toBe("verify");
  });

  it("resumes on online and on a bfcache restore only", () => {
    const h = setup();
    h.win.dispatchEvent(new Event("online"));
    h.win.dispatchEvent(new Event("pageshow")); // ordinary load: not persisted
    h.win.dispatchEvent(Object.assign(new Event("pageshow"), { persisted: true }));
    expect(h.resumes.map((r) => r[0])).toEqual(["online", "pageshow"]);
  });

  it("stops listening when unsubscribed", () => {
    const h = setup();
    h.stop();
    h.setVisibility("hidden");
    h.win.dispatchEvent(new Event("online"));
    expect(h.hidden()).toBe(0);
    expect(h.resumes).toHaveLength(0);
  });
});
