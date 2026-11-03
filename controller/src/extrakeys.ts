// Special-keys toolbar for touch controllers: the keys an on-screen keyboard
// lacks (Esc, Tab, arrows, Home/End/PgUp/PgDn, Del, F1–F12) plus sticky
// Ctrl/Alt/Cmd/Shift.
//
//   modifiers  One-shot and sticky, like iOS Shift: tap latches for the next
//              key, a quick double-tap locks, a tap on a lit modifier releases.
//              While one is latched, characters typed on the soft keyboard go
//              out as key chords (Ctrl+C) instead of TextInput, which carries
//              no modifiers — so they're mapped back to US-layout key codes.
//   chords     Modifier keys are pressed and released explicitly around the
//              key. The mask alone isn't enough: the Windows injector ignores
//              it (it tracks state from injected keys), and the macOS one ORs
//              it into its tracked flags — explicit presses work on both.
//
// The toolbar's modifiers name host keys (⌘ on a Mac, Win on Windows), so its
// chords are host-native; the caller sends them past the Cmd→Ctrl remap.

import { HostOs, type InputEvent, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT } from "./protocol";

export type Modifier = "ctrl" | "alt" | "meta" | "shift";
export type Latch = "off" | "latched" | "locked";
export type KeyStroke = Extract<InputEvent, { kind: "keydown" | "keyup" }>;

/** Toolbar order. */
export const MODIFIERS: readonly Modifier[] = ["ctrl", "alt", "meta", "shift"];

export const MODIFIER_BIT: Readonly<Record<Modifier, number>> = {
  ctrl: MOD_CTRL,
  alt: MOD_ALT,
  meta: MOD_META,
  shift: MOD_SHIFT,
};

/** Second tap within this long locks the modifier instead of releasing it. */
export const DOUBLE_TAP_MS = 400;

/** Latch/lock state for the four modifiers. Time is passed in (testable). */
export class StickyModifiers {
  private readonly state = new Map<Modifier, Latch>();
  private readonly tappedAt = new Map<Modifier, number>();

  constructor(private readonly doubleTapMs = DOUBLE_TAP_MS) {}

  get(mod: Modifier): Latch {
    return this.state.get(mod) ?? "off";
  }

  /** off → latched; latched → locked if tapped again quickly, else off;
   * locked → off. Returns the new state. */
  tap(mod: Modifier, now: number): Latch {
    const cur = this.get(mod);
    const quick = now - (this.tappedAt.get(mod) ?? Number.NEGATIVE_INFINITY) <= this.doubleTapMs;
    const next: Latch = cur === "off" ? "latched" : cur === "latched" && quick ? "locked" : "off";
    this.state.set(mod, next);
    this.tappedAt.set(mod, now);
    return next;
  }

  /** MOD_* mask of every latched or locked modifier. */
  get mask(): number {
    let m = 0;
    for (const [mod, s] of this.state) if (s !== "off") m |= MODIFIER_BIT[mod];
    return m;
  }

  /** A key went out with the modifiers: one-shot latches are spent, locks
   * stay. Returns true if anything changed. */
  consume(): boolean {
    let changed = false;
    for (const [mod, s] of this.state) {
      if (s === "latched") {
        this.state.set(mod, "off");
        changed = true;
      }
    }
    return changed;
  }

  /** Release everything (keyboard closed, session ended). */
  clear(): boolean {
    const changed = this.mask !== 0;
    this.state.clear();
    return changed;
  }
}

export interface CharKey {
  code: string;
  shift: boolean;
}

const LETTER = /^[a-zA-Z]$/;
const DIGIT = /^[0-9]$/;

/** US-layout punctuation: char → [code, shift]. */
const US_PUNCT: Record<string, readonly [string, boolean]> = {
  " ": ["Space", false],
  "\t": ["Tab", false],
  "\n": ["Enter", false],
  "\r": ["Enter", false],
  "`": ["Backquote", false],
  "~": ["Backquote", true],
  "!": ["Digit1", true],
  "@": ["Digit2", true],
  "#": ["Digit3", true],
  $: ["Digit4", true],
  "%": ["Digit5", true],
  "^": ["Digit6", true],
  "&": ["Digit7", true],
  "*": ["Digit8", true],
  "(": ["Digit9", true],
  ")": ["Digit0", true],
  "-": ["Minus", false],
  _: ["Minus", true],
  "=": ["Equal", false],
  "+": ["Equal", true],
  "[": ["BracketLeft", false],
  "{": ["BracketLeft", true],
  "]": ["BracketRight", false],
  "}": ["BracketRight", true],
  "\\": ["Backslash", false],
  "|": ["Backslash", true],
  ";": ["Semicolon", false],
  ":": ["Semicolon", true],
  "'": ["Quote", false],
  '"': ["Quote", true],
  ",": ["Comma", false],
  "<": ["Comma", true],
  ".": ["Period", false],
  ">": ["Period", true],
  "/": ["Slash", false],
  "?": ["Slash", true],
  // iOS Smart Punctuation curls quotes as they're typed; the key is the same.
  "‘": ["Quote", false],
  "’": ["Quote", false],
  "“": ["Quote", true],
  "”": ["Quote", true],
};

/**
 * The US-layout key (DOM code + Shift) that types `ch`, or null when there is
 * none (accented letters, other scripts, emoji, more than one character).
 * Used to send a typed character as a chord when modifiers are latched; a
 * host with a different layout gets the same physical key, as with a
 * hardware keyboard.
 */
export function charToKey(ch: string): CharKey | null {
  if (LETTER.test(ch)) return { code: `Key${ch.toUpperCase()}`, shift: ch !== ch.toLowerCase() };
  if (DIGIT.test(ch)) return { code: `Digit${ch}`, shift: false };
  const p = US_PUNCT[ch];
  return p ? { code: p[0], shift: p[1] } : null;
}

/** Modifier keys pressed for a chord, in press order. */
const CHORD_ORDER: ReadonlyArray<readonly [number, string]> = [
  [MOD_CTRL, "ControlLeft"],
  [MOD_ALT, "AltLeft"],
  [MOD_SHIFT, "ShiftLeft"],
  [MOD_META, "MetaLeft"],
];

/**
 * The key events for `code` with `mods` held: each modifier down, the key
 * down/up, the modifiers up in reverse. Every event carries the mask a
 * browser would report at that moment (a modifier's own bit set on its
 * keydown, cleared on its keyup). With no modifiers it's a plain tap.
 */
export function chord(mods: number, code: string): KeyStroke[] {
  const held = CHORD_ORDER.filter(([bit]) => mods & bit);
  const out: KeyStroke[] = [];
  let m = 0;
  for (const [bit, mod] of held) {
    m |= bit;
    out.push({ type: "input", kind: "keydown", code: mod, modifiers: m });
  }
  out.push({ type: "input", kind: "keydown", code, modifiers: m });
  out.push({ type: "input", kind: "keyup", code, modifiers: m });
  for (const [bit, mod] of [...held].reverse()) {
    m &= ~bit;
    out.push({ type: "input", kind: "keyup", code: mod, modifiers: m });
  }
  return out;
}

export interface ExtraKeysOut {
  /** One key event, modifier presses included. */
  key(stroke: KeyStroke): void;
  /** Committed text (TextInput). */
  text(text: string): void;
  /** A one-shot latch was spent or cleared: re-render the toolbar. */
  changed?(): void;
}

/** Routes keys and typed text through the sticky modifiers. */
export class ExtraKeys {
  constructor(
    private readonly out: ExtraKeysOut,
    readonly sticky = new StickyModifiers(),
  ) {}

  /** Any modifier latched or locked (the next key will carry it). */
  get latched(): boolean {
    return this.sticky.mask !== 0;
  }

  /** Tap `code` with the latched modifiers plus `mods`, then spend the latches. */
  tap(code: string, mods = 0): void {
    const latched = this.sticky.mask;
    for (const ev of chord(latched | mods, code)) this.out.key(ev);
    if (latched && this.sticky.consume()) this.out.changed?.();
  }

  /**
   * Typed text: TextInput while nothing is latched. With modifiers latched,
   * each character with a US key becomes a chord (Ctrl + "c" → Ctrl+C) until
   * a one-shot latch is spent; the rest goes as text. A character with no key
   * (é, emoji) can't carry modifiers, so it's typed and the latch waits.
   */
  type(text: string): void {
    let plain = "";
    for (const ch of text) {
      const key = this.sticky.mask ? charToKey(ch) : null;
      if (!key) {
        plain += ch;
        continue;
      }
      if (plain) this.out.text(plain);
      plain = "";
      this.tap(key.code, key.shift ? MOD_SHIFT : 0);
    }
    if (plain) this.out.text(plain);
  }

  clear(): void {
    if (this.sticky.clear()) this.out.changed?.();
  }
}

export interface ModifierLabel {
  label: string;
  title: string;
}

/** Toolbar labels for the host's modifier keys (macOS when unknown: hosts
 * that predate HostInfo are Macs). */
export function modifierLabels(hostOs: number | null): Record<Modifier, ModifierLabel> {
  const mac = hostOs === null || hostOs === HostOs.MacOs || hostOs === HostOs.Unknown;
  return {
    ctrl: { label: "Ctrl", title: "Control" },
    alt: mac ? { label: "⌥", title: "Option" } : { label: "Alt", title: "Alt" },
    meta: mac
      ? { label: "⌘", title: "Command" }
      : hostOs === HostOs.Windows
        ? { label: "Win", title: "Windows key" }
        : { label: "Super", title: "Super" },
    shift: { label: "⇧", title: "Shift" },
  };
}

// ---- toolbar DOM ------------------------------------------------------------

const PASTE = "paste";

/** The scrolling strip: [code, label, title]. PASTE is an action, not a key. */
const STRIP: ReadonlyArray<readonly [string, string, string]> = [
  ["Escape", "Esc", "Escape"],
  ["Tab", "Tab", "Tab"],
  [PASTE, "Paste", "Paste this device's clipboard on the host"],
  ["ArrowLeft", "←", "Left arrow"],
  ["ArrowUp", "↑", "Up arrow"],
  ["ArrowDown", "↓", "Down arrow"],
  ["ArrowRight", "→", "Right arrow"],
  ["Delete", "Del", "Forward delete"],
  ["Home", "Home", "Home"],
  ["End", "End", "End"],
  ["PageUp", "PgUp", "Page up"],
  ["PageDown", "PgDn", "Page down"],
  ...Array.from({ length: 12 }, (_, i) => [`F${i + 1}`, `F${i + 1}`, `F${i + 1}`] as const),
];

export interface ToolbarActions {
  key(code: string): void;
  modifier(mod: Modifier): void;
  /** Runs inside the click, so it may read the clipboard. */
  paste(): void;
}

/**
 * The bar itself: modifiers pinned on the left, a horizontally scrolling
 * strip of keys (F-keys at its end). Buttons never take focus — a focus move
 * would blur the soft keyboard's field and dismiss the keyboard.
 */
export class KeyToolbar {
  private readonly mods = new Map<Modifier, HTMLButtonElement>();

  constructor(
    readonly el: HTMLElement,
    private readonly sticky: StickyModifiers,
    actions: ToolbarActions,
  ) {
    el.innerHTML = "";
    el.setAttribute("role", "toolbar");
    el.setAttribute("aria-label", "Special keys");
    const group = (cls: string) => {
      const div = document.createElement("div");
      div.className = cls;
      el.appendChild(div);
      return div;
    };
    const button = (parent: HTMLElement, label: string, title: string) => {
      const b = document.createElement("button");
      b.type = "button";
      b.tabIndex = -1;
      b.textContent = label;
      b.title = title;
      b.setAttribute("aria-label", title);
      parent.appendChild(b);
      return b;
    };
    const modGroup = group("xk-mods");
    for (const mod of MODIFIERS) {
      const b = button(modGroup, "", "");
      b.dataset.mod = mod;
      b.setAttribute("aria-pressed", "false");
      this.mods.set(mod, b);
    }
    const strip = group("xk-strip");
    for (const [code, label, title] of STRIP) {
      const b = button(strip, label, title);
      if (code === PASTE) b.dataset.act = PASTE;
      else b.dataset.key = code;
    }

    el.addEventListener("mousedown", (e) => e.preventDefault());
    el.addEventListener("click", (e) => {
      const b = (e.target as HTMLElement).closest("button");
      if (!b || !el.contains(b)) return;
      if (b.dataset.mod) actions.modifier(b.dataset.mod as Modifier);
      else if (b.dataset.act === PASTE) actions.paste();
      else if (b.dataset.key) actions.key(b.dataset.key);
    });
    this.setHostOs(null);
  }

  set visible(on: boolean) {
    this.el.hidden = !on;
  }

  setHostOs(os: number | null): void {
    const labels = modifierLabels(os);
    for (const [mod, b] of this.mods) {
      b.textContent = labels[mod].label;
      b.title = labels[mod].title;
      b.setAttribute("aria-label", labels[mod].title);
    }
  }

  /** Reflect latch/lock state on the modifier buttons. */
  render(): void {
    for (const [mod, b] of this.mods) {
      const s = this.sticky.get(mod);
      b.classList.toggle("latched", s === "latched");
      b.classList.toggle("locked", s === "locked");
      b.setAttribute("aria-pressed", String(s !== "off"));
    }
  }
}
