// Per-host-OS modifier policy (Phase 7 M3).
//
// The controller forwards physical keys (DOM `code`). On an Apple keyboard the
// user's shortcut key is Cmd (Meta); against a Windows host Meta is the
// Windows key, and Cmd+C would open the Start menu instead of copying. So
// when an Apple client drives a Windows host, Meta becomes Control — and a
// real Control key still passes through, so both Cmd+C and Ctrl+C copy.
// Against a macOS host nothing changes. Applied at the single point every
// key event leaves the controller (physical keys, the paste flow, the soft
// keyboard), so all three paths agree.

import { HostOs, type InputEvent, MOD_CTRL, MOD_META } from "./protocol";

/** Is this browser on an Apple platform (Cmd is the shortcut key)? */
export function isAppleClient(nav: { platform?: string; userAgent?: string } = navigator): boolean {
  const p = `${nav.platform ?? ""} ${nav.userAgent ?? ""}`;
  return /Mac|iPhone|iPad|iPod/i.test(p);
}

const META_TO_CTRL: Record<string, string> = {
  MetaLeft: "ControlLeft",
  MetaRight: "ControlRight",
};

/** Should Cmd act as Ctrl for this host/client pair? */
export function cmdActsAsCtrl(hostOs: number | null, appleClient: boolean): boolean {
  return appleClient && hostOs === HostOs.Windows;
}

/** Apply the modifier policy to one outgoing input event. */
export function remapInput(ev: InputEvent, cmdAsCtrl: boolean): InputEvent {
  if (!cmdAsCtrl || (ev.kind !== "keydown" && ev.kind !== "keyup")) return ev;
  const code = META_TO_CTRL[ev.code] ?? ev.code;
  let modifiers = ev.modifiers;
  if (modifiers & MOD_META) modifiers = (modifiers & ~MOD_META) | MOD_CTRL;
  return { ...ev, code, modifiers };
}
