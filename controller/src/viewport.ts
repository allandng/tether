// Keyboard-aware layout. WebKit on iOS doesn't shrink the layout viewport for
// the on-screen keyboard (no `interactive-widget`), so a `height: 100%` session
// keeps its bottom ~40% under the keyboard; only `visualViewport` says how
// much is still visible. We mirror it into CSS variables on :root:
//
//   --vvh        visible height (px)
//   --vv-top     visible area's offset from the layout viewport's top (iOS can
//                pan the visual viewport while the keyboard is up)
//   --vv-bottom  layout height hidden below the visible area (the keyboard)
//
// #session-view pads itself by --vv-top/--vv-bottom, so the canvas letterboxes
// into what's visible and the special-keys toolbar sits right above the
// keyboard. The variables are only set while the visible area is smaller than
// the layout viewport; otherwise (and where `visualViewport` doesn't exist)
// they're removed and the CSS fallbacks give the plain full-height layout.
//
// Input mapping needs nothing here: it measures the canvas rect on every
// pointerdown.

export interface VisualViewportLike {
  readonly height: number;
  readonly offsetTop: number;
  readonly scale?: number;
}

export interface ViewportVars {
  vvh: number;
  vvTop: number;
  vvBottom: number;
}

/**
 * The CSS variable values for a visual viewport inside a layout viewport
 * `layoutHeight` px tall, or null when no adjustment is needed: no
 * visualViewport, nothing hidden, or a pinch zoom (scale > 1 also shrinks the
 * visual viewport, and the layout shouldn't chase it). Whole pixels; a
 * rubber-band overscroll's negative offset is clamped.
 */
export function viewportVars(
  vv: VisualViewportLike | null | undefined,
  layoutHeight: number,
): ViewportVars | null {
  if (!vv || !(vv.height > 0) || !(layoutHeight > 0)) return null;
  if ((vv.scale ?? 1) > 1.01) return null;
  const vvTop = Math.min(layoutHeight, Math.max(0, Math.round(vv.offsetTop)));
  const vvh = Math.min(layoutHeight - vvTop, Math.round(vv.height));
  const vvBottom = layoutHeight - vvTop - vvh;
  if (vvTop === 0 && vvBottom <= 1) return null; // the whole layout is visible
  return { vvh, vvTop, vvBottom };
}

export const VIEWPORT_VARS = ["--vvh", "--vv-top", "--vv-bottom"] as const;

/** viewportVars as CSS custom-property values. */
export function cssVars(v: ViewportVars): Record<(typeof VIEWPORT_VARS)[number], string> {
  return { "--vvh": `${v.vvh}px`, "--vv-top": `${v.vvTop}px`, "--vv-bottom": `${v.vvBottom}px` };
}

/**
 * Keep the variables in step with `window.visualViewport` (resize + scroll),
 * at most once per animation frame. No-op where visualViewport is missing.
 * Returns a stop function.
 */
export function trackVisualViewport(root: HTMLElement = document.documentElement): () => void {
  const vv = window.visualViewport;
  if (!vv) return () => {};
  let frame = 0;
  const apply = () => {
    frame = 0;
    const vars = viewportVars(vv, window.innerHeight);
    if (vars) {
      for (const [k, v] of Object.entries(cssVars(vars))) root.style.setProperty(k, v);
    } else {
      for (const k of VIEWPORT_VARS) root.style.removeProperty(k);
    }
  };
  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(apply);
  };
  vv.addEventListener("resize", schedule);
  vv.addEventListener("scroll", schedule);
  window.addEventListener("resize", schedule); // innerHeight (rotation, URL bar)
  apply();
  return () => {
    vv.removeEventListener("resize", schedule);
    vv.removeEventListener("scroll", schedule);
    window.removeEventListener("resize", schedule);
    if (frame) cancelAnimationFrame(frame);
    for (const k of VIEWPORT_VARS) root.style.removeProperty(k);
  };
}
