// The session control bar on phones and tablets ("compact": a coarse pointer
// or a narrow window — COMPACT_QUERY, mirrored by index.html's compact rules).
//
// Compact: one row — back, status/host name, keyboard, pointer mode, ⋯. The
// other controls live in #more, which ⋯ opens as a small sheet. They are the
// same elements and handlers as on desktop (where #more is display: contents,
// so the full bar looks as it always did), which keeps main.ts's per-status
// `hidden` flags working inside the sheet: a hidden control stays hidden.
//
// Immersive: with auto-hide on, the bar becomes an overlay on the stage and
// slides away a few seconds after the last bar interaction (autohide.ts); a
// pull-tab at the top edge brings it back. Taps on the remote screen are host
// input and never reveal it. As an overlay the bar never resizes the stage
// while it slides, so the remote screen doesn't rescale or jump each time it
// comes and goes; the stage changes size only when overlay mode itself
// switches (the toggle, or rotating across the compact breakpoint), and that
// relayouts once.

import { BarAutoHide } from "./autohide";
import { relayoutSession } from "./relayout";

export const COMPACT_QUERY = "(pointer: coarse), (max-width: 600px)";
export const AUTO_HIDE_KEY = "tether-bar-autohide";

export interface SessionBarOptions {
  /** #session-view: holds #bar (with #more, #more-btn, #more-scrim,
   * #autohide) and #bar-handle. */
  view: HTMLElement;
  /** The session is connected; the bar never hides otherwise. */
  connected(): boolean;
  /** Overlays that keep the bar up while showing — a dialog while open,
   * anything else while not hidden. Watched, so changes needn't be reported. */
  holds: HTMLElement[];
  /** Hand keystroke focus back to the session (the soft keyboard's field if
   * it's open, else the canvas). */
  refocus(): void;
  store: { get(k: string): string | null; set(k: string, v: string): void };
}

export interface SessionBar {
  /** Re-decide visibility; call when the connection status changes. */
  sync(): void;
  closeSheet(): void;
}

export function setupSessionBar(o: SessionBarOptions): SessionBar {
  const doc = o.view.ownerDocument;
  const q = <T extends HTMLElement>(sel: string) => o.view.querySelector<T>(sel)!;
  const bar = q<HTMLElement>("#bar");
  const sheet = q<HTMLElement>("#more");
  const moreBtn = q<HTMLButtonElement>("#more-btn");
  const scrim = q<HTMLElement>("#more-scrim");
  const autoHideBtn = q<HTMLButtonElement>("#autohide");
  const handle = q<HTMLButtonElement>("#bar-handle");
  const mq = typeof matchMedia === "function" ? matchMedia(COMPACT_QUERY) : null;
  const compact = () => mq?.matches ?? false;

  let autoHide = o.store.get(AUTO_HIDE_KEY) === "1";
  const countdown = new BarAutoHide(Date.now());
  let timer: ReturnType<typeof setTimeout> | null = null;

  // A dialog by its `open` attribute, not `instanceof HTMLDialogElement`:
  // Safari before 15.4 has no such class (naming it throws, aborting the rest
  // of setup), and there a <dialog> has no `open` property either.
  const showing = (el: HTMLElement) => (el.localName === "dialog" ? el.hasAttribute("open") : !el.hidden);
  const sheetOpen = () => bar.classList.contains("more-open");

  function sync(): void {
    const held = sheetOpen() || bar.matches(":focus-within") || o.holds.some(showing);
    const d = countdown.update(
      { connected: o.connected(), autoHide: autoHide && compact(), held },
      Date.now(),
    );
    o.view.classList.toggle("bar-hidden", !d.visible);
    handle.hidden = d.visible;
    if (timer) clearTimeout(timer);
    timer = d.deadline === null ? null : setTimeout(sync, Math.max(0, d.deadline - Date.now()));
  }

  // ---- overflow sheet -------------------------------------------------------
  const setSheet = (open: boolean) => {
    bar.classList.toggle("more-open", open);
    moreBtn.setAttribute("aria-expanded", String(open));
    sync();
  };
  /** Close; focus left inside the now-hidden sheet (a keyboard user's, or a
   * cancelled display picker's) goes back to ⋯ or the session, rather than
   * stranding keystrokes and pinning the bar via :focus-within. */
  const closeSheet = (focusTrigger = false) => {
    if (!sheetOpen()) return;
    const hadFocus = sheet.contains(doc.activeElement);
    setSheet(false);
    if (hadFocus && focusTrigger) moreBtn.focus();
    else if (hadFocus) o.refocus();
  };
  moreBtn.addEventListener("click", (e) => {
    setSheet(!sheetOpen());
    // Opened from a keyboard (no pointer, detail 0): move focus into the
    // sheet. A tap leaves focus alone so an open soft keyboard stays up.
    // (The selector avoids :is(), which Safari lacks before 14.)
    if (sheetOpen() && e.detail === 0) {
      sheet.querySelector<HTMLElement>("button:not([hidden]), select:not([hidden])")?.focus();
    }
  });
  // Outside tap: the scrim takes it (so it never reaches the host) and closes.
  scrim.addEventListener("mousedown", (e) => e.preventDefault());
  scrim.addEventListener("click", () => closeSheet());
  // Picking an item closes the sheet; the item's own handler ran first.
  sheet.addEventListener("click", (e) => {
    if ((e.target as Element).closest("button")) closeSheet();
  });
  sheet.addEventListener("change", () => closeSheet());
  // Esc closes it — and is kept from the host while it does.
  let swallowEscapeUp = false;
  doc.addEventListener(
    "keydown",
    (e) => {
      if (e.key !== "Escape" || !sheetOpen()) return;
      e.preventDefault();
      e.stopPropagation();
      swallowEscapeUp = true;
      closeSheet(true);
    },
    true,
  );
  doc.addEventListener(
    "keyup",
    (e) => {
      if (e.key !== "Escape" || !swallowEscapeUp) return;
      swallowEscapeUp = false;
      e.preventDefault();
      e.stopPropagation();
    },
    true,
  );
  // A tap on a bar button must not blur the soft keyboard's field (that
  // dismisses the iOS keyboard); actions that mean to close it do so
  // themselves. Desktop keeps native focus behavior. Selects and the pairing
  // field still take focus.
  bar.addEventListener("mousedown", (e) => {
    if (compact() && (e.target as Element).closest("button")) e.preventDefault();
  });
  handle.addEventListener("mousedown", (e) => e.preventDefault());

  // ---- auto-hide --------------------------------------------------------------
  // Any bar interaction restarts the countdown; the pull-tab is one too.
  const interact = () => {
    countdown.interact(Date.now());
    sync();
  };
  for (const type of ["pointerdown", "keydown", "focusin", "input", "change"]) {
    bar.addEventListener(type, interact, true);
  }
  bar.addEventListener("focusout", () => setTimeout(sync)); // :focus-within settles after it
  handle.addEventListener("click", interact);
  const holdWatcher = new MutationObserver(() => sync());
  for (const el of o.holds) {
    holdWatcher.observe(el, { attributes: true, attributeFilter: ["hidden", "open"] });
  }

  const reflectAutoHide = () => {
    autoHideBtn.setAttribute("aria-pressed", String(autoHide));
    autoHideBtn.classList.toggle("on", autoHide);
  };
  const applyMode = () => {
    const overlay = compact() && autoHide;
    if (o.view.classList.contains("bar-overlay") !== overlay) {
      o.view.classList.toggle("bar-overlay", overlay);
      relayoutSession(); // the stage just took or gave back the bar's height
    }
    sync();
  };
  autoHideBtn.addEventListener("click", () => {
    autoHide = !autoHide;
    o.store.set(AUTO_HIDE_KEY, autoHide ? "1" : "0");
    reflectAutoHide();
    applyMode();
  });
  const onCompactChange = () => {
    closeSheet();
    applyMode();
  };
  // MediaQueryList is an EventTarget only from Safari 14; before, addListener.
  if (typeof mq?.addEventListener === "function") mq.addEventListener("change", onCompactChange);
  else mq?.addListener(onCompactChange);

  // The overlay bar's height, for things that must clear it (the zoom chip).
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => o.view.style.setProperty("--bar-h", `${bar.offsetHeight}px`)).observe(bar);
  }

  // A tucked-away control asking for a tap (the host clipboard chip) marks
  // the ⋯ button and the pull-tab, since it's no longer in plain sight.
  const attention = Array.from(sheet.querySelectorAll<HTMLElement>("[data-attn]"));
  const reflectAttention = () => {
    const on = attention.some((el) => !el.hidden);
    moreBtn.classList.toggle("attn", on);
    handle.classList.toggle("attn", on);
  };
  new MutationObserver(reflectAttention).observe(sheet, {
    subtree: true,
    attributes: true,
    attributeFilter: ["hidden"],
  });

  reflectAutoHide();
  reflectAttention();
  applyMode();
  return { sync, closeSheet: () => closeSheet() };
}
