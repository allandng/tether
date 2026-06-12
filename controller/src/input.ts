// Capture pointer/wheel/keyboard events over the viewport and forward them
// as protocol InputEvents with normalized coordinates.

import type { PasteFlow } from "./clipboard";
import { MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT, type InputEvent } from "./protocol";
import type { Viewer } from "./viewer";

/** Anything input events can be sent through (a transport, or a facade over
 * whichever transport is currently active). */
export interface InputSink {
  sendInput(ev: InputEvent): void;
}

export interface NormalizedPoint {
  x: number;
  y: number;
}

export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * Map a client-space point onto protocol coordinates (0..65535 across the
 * displayed video rect). Points outside the rect clamp to its edge so drags
 * that stray over the letterbox bars stay usable.
 */
export function normalizedFromClient(clientX: number, clientY: number, rect: Rect): NormalizedPoint {
  const fx = (clientX - rect.left) / Math.max(1, rect.width);
  const fy = (clientY - rect.top) / Math.max(1, rect.height);
  return {
    x: Math.round(Math.min(1, Math.max(0, fx)) * 65535),
    y: Math.round(Math.min(1, Math.max(0, fy)) * 65535),
  };
}

export function modifierMask(e: { shiftKey: boolean; ctrlKey: boolean; altKey: boolean; metaKey: boolean }): number {
  return (
    (e.shiftKey ? MOD_SHIFT : 0) |
    (e.ctrlKey ? MOD_CTRL : 0) |
    (e.altKey ? MOD_ALT : 0) |
    (e.metaKey ? MOD_META : 0)
  );
}

const clampI16 = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v)));

/** WheelEvent deltas normalized to pixels (deltaMode can be lines/pages). */
export function wheelToPixels(delta: number, deltaMode: number): number {
  const scale = deltaMode === 1 ? 16 : deltaMode === 2 ? 800 : 1;
  return clampI16(delta * scale);
}

export function attachInput(
  canvas: HTMLCanvasElement,
  viewer: Viewer,
  connection: InputSink,
  pasteFlow?: PasteFlow,
): void {
  const point = (e: { clientX: number; clientY: number }) =>
    normalizedFromClient(e.clientX, e.clientY, viewer.displayedRect());

  canvas.addEventListener("pointermove", (e) => {
    const { x, y } = point(e);
    connection.sendInput({ type: "input", kind: "mousemove", x, y });
  });

  canvas.addEventListener("pointerdown", (e) => {
    if (e.button > 2) return;
    canvas.focus();
    canvas.setPointerCapture(e.pointerId);
    const { x, y } = point(e);
    connection.sendInput({ type: "input", kind: "mousedown", button: e.button, x, y });
    e.preventDefault();
  });

  canvas.addEventListener("pointerup", (e) => {
    if (e.button > 2) return;
    const { x, y } = point(e);
    connection.sendInput({ type: "input", kind: "mouseup", button: e.button, x, y });
    e.preventDefault();
  });

  // The host gets the right-click; don't also open the local menu.
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  canvas.addEventListener(
    "wheel",
    (e) => {
      connection.sendInput({
        type: "input",
        kind: "scroll",
        dx: wheelToPixels(e.deltaX, e.deltaMode),
        dy: wheelToPixels(e.deltaY, e.deltaMode),
      });
      e.preventDefault(); // keep the page from zooming/scrolling
    },
    { passive: false },
  );

  canvas.addEventListener("keydown", (e) => {
    // Cmd/Ctrl+V: don't forward and don't preventDefault — the default paste
    // action produces the `paste` event the clipboard sync harvests; the V
    // tap is sent by the PasteFlow afterwards, in order.
    if (pasteFlow && e.code === "KeyV" && (e.metaKey || e.ctrlKey)) {
      pasteFlow.onPasteCombo(modifierMask(e));
      return;
    }
    // Auto-repeat keydowns are forwarded: synthetic held keys don't repeat
    // on the host, so the browser's repeat stream stands in for it.
    connection.sendInput({ type: "input", kind: "keydown", code: e.code, modifiers: modifierMask(e) });
    e.preventDefault(); // browser shortcuts must not fire locally
  });

  canvas.addEventListener("keyup", (e) => {
    if (pasteFlow && e.code === "KeyV" && pasteFlow.onPasteKeyUp()) {
      e.preventDefault();
      return; // the flow already sent the full tap
    }
    connection.sendInput({ type: "input", kind: "keyup", code: e.code, modifiers: modifierMask(e) });
    e.preventDefault();
  });

  if (pasteFlow) {
    window.addEventListener("paste", (e) => {
      const text = e.clipboardData?.getData("text/plain") || null;
      if (pasteFlow.onPasteEvent(text)) {
        e.preventDefault(); // consumed by the remote session, not the page
      }
    });
  }

  // If the canvas loses focus mid-combo (cmd+tab away), release whatever the
  // host might still think is held to avoid stuck modifiers.
  canvas.addEventListener("blur", () => {
    for (const code of ["ShiftLeft", "ControlLeft", "AltLeft", "MetaLeft"]) {
      connection.sendInput({ type: "input", kind: "keyup", code, modifiers: 0 });
    }
  });
}
