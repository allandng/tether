// Soft-keyboard text capture for touch devices.
//
// iOS Safari gives no usable KeyboardEvent.code from the on-screen keyboard,
// so we summon it by focusing a hidden (but rendered — not display:none)
// input and mirror what happens in it onto the host:
//
//   text     The field keeps a short rolling context (about the last word) so
//            autocorrect, QuickType, dictation and IMEs have something to work
//            on. After every `input` (or compositionend) the old and new
//            values are diffed into Backspaces + text — "teh" → "the" is two
//            Backspaces + "he" — rather than trusting beforeinput's ranges.
//   keys     iOS fires only `keydown` for Return, and for Backspace on an
//            empty field (no beforeinput, no input), so those — plus Tab,
//            Escape, arrows, Delete and hardware-keyboard shortcuts — are
//            taken at keydown and tapped on the host as real keys.
//
// The diff assumes the host caret sits right after the context, so anything
// that may move it (a tap on the screen, a control key, text sent from
// elsewhere) drops the context. focus() must run inside a user gesture or iOS
// won't raise the keyboard.

import { modifierMask } from "./input";
import { MAX_TEXT_INPUT_LEN } from "./protocol";
import { isAppleClient } from "./remap";

export interface KeyboardSink {
  /** Committed Unicode text, at most MAX_TEXT_INPUT_LEN UTF-8 bytes. */
  sendText(text: string): void;
  /** Tap a key that has a real host keycode (KeyboardEvent.code + MOD_* mask). */
  sendKeyTap(code: string, modifiers: number): void;
  /** Optional: true while modifiers latched elsewhere (the special-keys
   * toolbar) wait for a key. The sink then sends typed text as key chords
   * (Ctrl+C, not "c"), so the host no longer holds what the field does and
   * the context is dropped after the edit. */
  modifiersLatched?(): boolean;
}

/** Control keys taken at keydown and tapped on the host. Matched on
 * KeyboardEvent.key (layout-independent); each name is also its host code. */
const CONTROL_KEYS = new Set([
  "Enter",
  "Backspace",
  "Tab",
  "Escape",
  "Delete",
  "ArrowUp",
  "ArrowDown",
  "ArrowLeft",
  "ArrowRight",
  "Home",
  "End",
  "PageUp",
  "PageDown",
]);

/** Legacy keyCodes for the same keys, for engines that report key "Unidentified". */
const CONTROL_KEYCODES = new Map([
  [8, "Backspace"],
  [9, "Tab"],
  [13, "Enter"],
  [27, "Escape"],
  [46, "Delete"],
  [37, "ArrowLeft"],
  [38, "ArrowUp"],
  [39, "ArrowRight"],
  [40, "ArrowDown"],
]);

const MODIFIER_CODES = new Set([
  "ShiftLeft",
  "ShiftRight",
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
]);

export interface KeyLike {
  key: string;
  code: string;
  keyCode: number;
  isComposing: boolean;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

export interface KeyTap {
  code: string;
  modifiers: number;
}

/**
 * What a keydown on the hidden field should do: tap a host key (the caller
 * preventDefaults), or null to let the field have it. Never intercepts IME
 * composition (keyCode 229) — that would break CJK and dictation input.
 * Backspace is only taken on an empty field; otherwise it edits the context
 * and the input diff sends it (taking it here too would double-delete).
 * Ctrl/Cmd shortcuts from an iPad hardware keyboard are forwarded, except the
 * platform's paste combo (Cmd+V on Apple clients, else Ctrl+V), which pastes
 * into the field and so is typed on the host. The other one pastes nothing
 * there (iPadOS ignores Ctrl+V), so it's forwarded like any shortcut.
 */
export function keydownTap(e: KeyLike, fieldEmpty: boolean, appleClient = true): KeyTap | null {
  if (e.isComposing || e.keyCode === 229) return null;
  const modifiers = modifierMask(e);
  const legacy = e.key === "Unidentified" || e.key === "";
  const control = CONTROL_KEYS.has(e.key) ? e.key : legacy ? CONTROL_KEYCODES.get(e.keyCode) : undefined;
  if (control) {
    if (control === "Backspace" && !fieldEmpty) return null;
    return { code: control, modifiers };
  }
  const fieldPaste = e.code === "KeyV" && (appleClient ? e.metaKey : e.ctrlKey);
  if ((e.ctrlKey || e.metaKey) && e.code && !MODIFIER_CODES.has(e.code) && !fieldPaste) {
    return { code: e.code, modifiers };
  }
  return null;
}

/**
 * Fallback for keyboards that report a line break, or a delete on an empty
 * field, only as beforeinput (no keydown, and no `input` since the value
 * can't change). Returns the host key to tap, or null — including when the
 * keydown path already tapped it for the key still held (`heldTap`).
 */
export function beforeInputTap(inputType: string, fieldEmpty: boolean, heldTap: string | null): string | null {
  let code: string | null = null;
  if (inputType === "insertLineBreak" || inputType === "insertParagraph") {
    code = "Enter";
  } else if (fieldEmpty && inputType.startsWith("delete") && !inputType.startsWith("deleteBy")) {
    code = inputType.endsWith("Forward") ? "Delete" : "Backspace";
  }
  return code === heldTap ? null : code;
}

export interface Edit {
  /** Host Backspaces to send first (one per grapheme removed). */
  backspaces: number;
  /** Text to type after them. */
  text: string;
}

const segmenter =
  typeof Intl !== "undefined" && "Segmenter" in Intl
    ? new Intl.Segmenter(undefined, { granularity: "grapheme" })
    : null;

/** User-perceived characters (one host Backspace each); code points without
 * Intl.Segmenter. Either way a surrogate pair is never split. */
export function graphemes(s: string): string[] {
  return segmenter ? Array.from(segmenter.segment(s), (g) => g.segment) : Array.from(s);
}

/**
 * Turn a field edit into host keystrokes, assuming the host caret sits right
 * after `prev`: Backspace to the longest common prefix, then type the rest of
 * `next`. Covers typing, deleting, autocorrect/QuickType replacements,
 * dictation revisions and word deletes alike. Works per grapheme, so it never
 * half-deletes an emoji or sends half a surrogate pair.
 */
export function diffEdit(prev: string, next: string): Edit {
  if (prev === next) return { backspaces: 0, text: "" };
  const a = graphemes(prev);
  const b = graphemes(next);
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return { backspaces: a.length - i, text: b.slice(i).join("") };
}

/** Longest context kept in the field (UTF-16 units). Older text is already
 * on the host; dropping it only narrows what autocorrect can see. */
export const CONTEXT_MAX = 64;

/** What to keep of the field once it outgrows `max`: the last word and the
 * whitespace after it (enough for autocorrect and iOS's double-space ". "),
 * or nothing if even that is too long. */
export function trimContext(value: string, max = CONTEXT_MAX): string {
  if (value.length <= max) return value;
  const isSpace = (i: number) => /\s/.test(value[i] ?? "");
  let start = value.length;
  while (start > 0 && isSpace(start - 1)) start--;
  while (start > 0 && !isSpace(start - 1)) start--;
  return value.length - start <= max ? value.slice(start) : "";
}

const utf8Len = (cp: number) => (cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4);

/** Split text into pieces of at most `maxBytes` UTF-8 bytes (the TextInput
 * wire limit) on code-point boundaries, so a long paste or dictation isn't
 * dropped and no multi-byte sequence or surrogate pair is ever cut. A lone
 * surrogate counts as the 3-byte U+FFFD TextEncoder turns it into. */
export function chunkText(text: string, maxBytes = MAX_TEXT_INPUT_LEN): string[] {
  const chunks: string[] = [];
  let start = 0;
  let at = 0;
  let bytes = 0;
  for (const ch of text) {
    const n = utf8Len(ch.codePointAt(0) ?? 0);
    if (bytes + n > maxBytes && at > start) {
      chunks.push(text.slice(start, at));
      start = at;
      bytes = 0;
    }
    bytes += n;
    at += ch.length;
  }
  if (at > start) chunks.push(text.slice(start));
  return chunks;
}

/**
 * The on-screen keyboard. Public API (stable — other UI builds on it):
 *
 *   isOpen: boolean               the field has focus (keyboard up, or an iPad
 *                                 hardware keyboard typing into it)
 *   onChange(fn): () => void      fn(open) on every change, including iOS's own
 *                                 "Done"/dismiss; returns an unsubscribe
 *   open() / toggle(): boolean    MUST run inside a user gesture (iOS)
 *   close()                       safe anywhere; blur() is an alias
 *   sendText(text)                type text on the host (chunked to the limit)
 *   sendKey(code, modifiers = 0)  tap a host key: KeyboardEvent.code + MOD_* mask
 *   resetContext()                forget the autocorrect context (call when the
 *                                 host caret may have moved)
 *
 * Buttons shown alongside the keyboard should preventDefault() their
 * `mousedown` so a tap doesn't blur the field (blurring dismisses it).
 */
export class SoftKeyboard {
  private readonly input: HTMLInputElement;
  private opened = false;
  /** The field's value as last mirrored to the host. */
  private context = "";
  private composing = false;
  /** Code the keydown path tapped for the key still held, so a trailing
   * beforeinput for the same keystroke can't send it twice. */
  private heldTap: string | null = null;
  private readonly listeners = new Set<(open: boolean) => void>();
  /** Cmd+V, not Ctrl+V, pastes into the field (keydownTap). */
  private readonly appleClient = typeof navigator !== "undefined" && isAppleClient(navigator);

  constructor(private readonly sink: KeyboardSink) {
    const input = document.createElement("input");
    input.type = "text";
    // Rendered but invisible — display:none/visibility:hidden won't summon iOS.
    Object.assign(input.style, {
      position: "fixed",
      top: "0",
      left: "0",
      width: "1px",
      height: "1px",
      opacity: "0",
      border: "0",
      padding: "0",
      // keep it out of the way of layout / scroll-into-view
      transform: "translateY(-100px)",
    } satisfies Partial<CSSStyleDeclaration>);
    // Autocorrect/QuickType stay on: the diff mirrors their replacements, and
    // swipe typing and predictions come with them. Autocapitalize is off: the
    // field only holds a fragment of the host's text, so iOS would treat every
    // reset (each Return, each tap on the screen) as a sentence start and
    // capitalize mid-sentence — and shell commands. Spellcheck is left on for
    // the correction pipeline; the field is invisible, so no underlines show.
    input.setAttribute("autocorrect", "on");
    input.setAttribute("autocapitalize", "off");
    input.setAttribute("spellcheck", "true");
    input.setAttribute("autocomplete", "off");
    input.setAttribute("inputmode", "text");
    input.setAttribute("aria-hidden", "true");
    input.tabIndex = -1;

    input.addEventListener("keydown", (e) => this.onKeyDown(e));
    input.addEventListener("keyup", () => {
      this.heldTap = null;
    });
    input.addEventListener("beforeinput", (e) => this.onBeforeInput(e as InputEvent));
    // Mid-composition values are provisional; the commit is synced at
    // compositionend (or by the input that follows it — sync is idempotent).
    input.addEventListener("input", (e) => {
      if (!this.composing && !(e as InputEvent).isComposing) this.sync();
    });
    input.addEventListener("compositionstart", () => {
      this.composing = true;
    });
    input.addEventListener("compositionend", () => {
      this.composing = false;
      this.sync();
    });
    input.addEventListener("focus", () => {
      this.resetContext();
      this.setOpen(true);
    });
    // Also fires for iOS's "Done" and for focus moving elsewhere.
    input.addEventListener("blur", () => {
      this.composing = false;
      this.heldTap = null;
      this.resetContext();
      this.setOpen(false);
    });

    document.body.appendChild(input);
    this.input = input;
  }

  get isOpen(): boolean {
    return this.opened;
  }

  onChange(fn: (open: boolean) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Raise the keyboard (or keep it focused). MUST run inside a user gesture. */
  open(): void {
    this.input.focus();
  }

  close(): void {
    this.input.blur();
  }

  /** Alias of close(). */
  blur(): void {
    this.close();
  }

  /** Toggle the keyboard; returns the new state. MUST run inside a user gesture. */
  toggle(): boolean {
    if (this.opened) this.close();
    else this.open();
    return this.opened;
  }

  sendText(text: string): void {
    this.resetContext();
    this.emitText(text);
  }

  sendKey(code: string, modifiers = 0): void {
    this.resetContext();
    this.sink.sendKeyTap(code, modifiers);
  }

  resetContext(): void {
    this.context = "";
    this.input.value = "";
    // Clearing the field mid-composition can end it without a
    // compositionend; a stale flag would then drop every later edit and key.
    this.composing = false;
  }

  private setOpen(open: boolean): void {
    if (open === this.opened) return;
    this.opened = open;
    for (const fn of this.listeners) fn(open);
  }

  private emitText(text: string): void {
    for (const chunk of chunkText(text)) this.sink.sendText(chunk);
  }

  /** Mirror the field onto the host as a diff from the last mirrored value,
   * then trim the context if it has grown past CONTEXT_MAX. */
  private sync(): void {
    const value = this.input.value;
    const latched = this.sink.modifiersLatched?.() ?? false;
    const { backspaces, text } = diffEdit(this.context, value);
    this.context = value;
    for (let i = 0; i < backspaces; i++) this.sink.sendKeyTap("Backspace", 0);
    this.emitText(text);
    if (latched) {
      this.resetContext(); // the host got chords, not this text
      return;
    }
    const kept = trimContext(value);
    if (kept !== value) {
      this.input.value = kept;
      this.context = kept;
    }
  }

  private onKeyDown(e: KeyboardEvent): void {
    const tap = keydownTap(
      {
        key: e.key,
        code: e.code,
        keyCode: e.keyCode,
        isComposing: e.isComposing || this.composing,
        shiftKey: e.shiftKey,
        ctrlKey: e.ctrlKey,
        altKey: e.altKey,
        metaKey: e.metaKey,
      },
      this.input.value === "",
      this.appleClient,
    );
    if (!tap) return;
    e.preventDefault(); // no local edit, no Tab focus move, no form submit
    this.heldTap = tap.code;
    this.sendKey(tap.code, tap.modifiers); // the host caret moves: drop the context
  }

  private onBeforeInput(e: InputEvent): void {
    const lineBreak = e.inputType === "insertLineBreak" || e.inputType === "insertParagraph";
    const code = beforeInputTap(e.inputType, this.input.value === "", this.heldTap);
    if (lineBreak || code) e.preventDefault();
    if (code) this.sendKey(code);
  }
}
