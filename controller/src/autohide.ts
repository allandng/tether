// Immersive mode for the session bar: with auto-hide on, the bar slides away
// a few seconds after the user last touched it and a pull-tab brings it back.
// This is the decision only — pure, clock injected; sessionbar.ts applies it
// and re-asks at the deadline it returns.
//
// The bar never hides while it is the user's only feedback or is in use:
// not connected (its status line carries connecting/retry/error text — there
// is no error state while connected), auto-hide off, or something "holds" it
// (pairing prompt, settings, the overflow sheet, the files panel, keyboard
// focus inside the bar). When a hold ends the countdown restarts from then,
// so the bar doesn't vanish the instant a session connects or a menu closes.

/** How long the bar stays up after the last interaction. */
export const AUTO_HIDE_MS = 3000;

export interface BarConditions {
  /** The session is connected. */
  connected: boolean;
  /** Auto-hide is switched on and the compact bar it belongs to is in use. */
  autoHide: boolean;
  /** Something needs the bar up right now (see above). */
  held: boolean;
}

export interface BarDecision {
  visible: boolean;
  /** When to ask again (ms, same clock as `now`): the moment the bar is due to
   * hide. null when nothing is pending. */
  deadline: number | null;
}

/** Visible with nothing pending: what every hold yields. */
const PINNED: BarDecision = { visible: true, deadline: null };

/** Does `c` keep the bar up regardless of the countdown? */
export function isPinned(c: BarConditions): boolean {
  return !c.connected || !c.autoHide || c.held;
}

/** The stateless rule: visible until `delayMs` after the last interaction. */
export function decideBar(
  c: BarConditions,
  lastInteraction: number,
  now: number,
  delayMs = AUTO_HIDE_MS,
): BarDecision {
  if (isPinned(c)) return PINNED;
  const hideAt = lastInteraction + delayMs;
  return now < hideAt ? { visible: true, deadline: hideAt } : { visible: false, deadline: null };
}

/** The countdown's memory: the last interaction, and whether the bar was
 * pinned at the previous update (a pin ending restarts the countdown). */
export class BarAutoHide {
  private last: number;
  private pinned = true;

  constructor(
    now: number,
    private readonly delayMs = AUTO_HIDE_MS,
  ) {
    this.last = now;
  }

  /** The user touched the bar (or the pull-tab): show it, restart the countdown. */
  interact(now: number): void {
    this.last = now;
  }

  /** Re-decide. Call on every condition change and when the deadline passes. */
  update(c: BarConditions, now: number): BarDecision {
    if (isPinned(c)) {
      this.pinned = true;
      return PINNED;
    }
    if (this.pinned) {
      this.pinned = false;
      this.last = now;
    }
    return decideBar(c, this.last, now, this.delayMs);
  }
}
