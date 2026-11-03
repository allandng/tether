// Layout-change fan-out for the session view. The zoom pan, the trackpad
// virtual cursor and its dot are all laid out in client px, so anything that
// moves or resizes the stage — rotation, window resize, the visual viewport
// shrinking under the soft keyboard, the control bar re-wrapping — must
// re-fit them. main.ts installs the handler; any code that changes the
// stage's geometry itself (e.g. viewport fitting) calls relayoutSession().

let handler: (() => void) | null = null;
let queued = false;
let watching = false;

/** Install the function that re-fits the session view (one at a time). */
export function setRelayoutHandler(fn: (() => void) | null): void {
  handler = fn;
}

/** Re-fit the session view after a layout change. Coalesced to the next
 * animation frame (so layout has settled); safe to call any time. */
export function relayoutSession(): void {
  if (queued) return;
  queued = true;
  const run = () => {
    queued = false;
    handler?.();
  };
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(run);
  else run();
}

/** Re-fit on window resize, rotation, visual-viewport changes and — if
 * given — any size change of the stage element itself. Idempotent. */
export function watchLayout(stage?: Element): void {
  if (watching || typeof window === "undefined") return;
  watching = true;
  window.addEventListener("resize", relayoutSession);
  window.addEventListener("orientationchange", relayoutSession);
  window.visualViewport?.addEventListener("resize", relayoutSession);
  window.visualViewport?.addEventListener("scroll", relayoutSession);
  if (stage && typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => relayoutSession()).observe(stage);
  }
}
