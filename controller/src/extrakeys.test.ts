import { afterEach, describe, expect, it, vi } from "vitest";
import {
  DOUBLE_TAP_MS,
  ExtraKeys,
  type KeyStroke,
  StickyModifiers,
  charToKey,
  chord,
  modifierLabels,
} from "./extrakeys";
import { SoftKeyboard } from "./keyboard";
import { HostOs, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT } from "./protocol";
import { remapInput } from "./remap";

/** Compact form of a key event: "+Code:mods" down, "-Code:mods" up. */
const fmt = (e: KeyStroke) => `${e.kind === "keydown" ? "+" : "-"}${e.code}:${e.modifiers}`;

describe("StickyModifiers", () => {
  it("a tap latches; the next key spends it", () => {
    const s = new StickyModifiers();
    expect(s.tap("ctrl", 0)).toBe("latched");
    expect(s.mask).toBe(MOD_CTRL);
    expect(s.consume()).toBe(true);
    expect(s.get("ctrl")).toBe("off");
    expect(s.mask).toBe(0);
  });

  it("a quick double-tap locks, and a lock survives keys", () => {
    const s = new StickyModifiers();
    s.tap("shift", 1000);
    expect(s.tap("shift", 1000 + DOUBLE_TAP_MS)).toBe("locked");
    expect(s.consume()).toBe(false);
    expect(s.consume()).toBe(false);
    expect(s.mask).toBe(MOD_SHIFT);
  });

  it("tapping a lit modifier releases it", () => {
    const s = new StickyModifiers();
    s.tap("alt", 0);
    expect(s.tap("alt", DOUBLE_TAP_MS + 1)).toBe("off"); // too slow to lock
    s.tap("alt", 5000);
    s.tap("alt", 5100); // locked
    expect(s.tap("alt", 5200)).toBe("off"); // a third tap releases even if quick
    expect(s.mask).toBe(0);
  });

  it("re-latching after a spent latch is a fresh tap, not a double-tap", () => {
    const s = new StickyModifiers();
    s.tap("ctrl", 0);
    s.consume();
    expect(s.tap("ctrl", 50)).toBe("latched");
  });

  it("modifiers combine; consume spends latches but keeps locks", () => {
    const s = new StickyModifiers();
    s.tap("ctrl", 0);
    s.tap("meta", 0);
    s.tap("meta", 10); // locked
    s.tap("shift", 20);
    expect(s.mask).toBe(MOD_CTRL | MOD_META | MOD_SHIFT);
    expect(s.consume()).toBe(true);
    expect(s.mask).toBe(MOD_META);
    expect(s.get("meta")).toBe("locked");
  });

  it("clear releases everything and reports whether anything was lit", () => {
    const s = new StickyModifiers();
    expect(s.clear()).toBe(false);
    s.tap("alt", 0);
    s.tap("alt", 1); // locked
    expect(s.clear()).toBe(true);
    expect(s.get("alt")).toBe("off");
    expect(s.tap("alt", 2)).toBe("latched");
  });

  it("honors a custom double-tap window", () => {
    const s = new StickyModifiers(100);
    s.tap("ctrl", 0);
    expect(s.tap("ctrl", 150)).toBe("off");
  });
});

describe("charToKey", () => {
  it("letters: code from the letter, Shift for capitals", () => {
    expect(charToKey("c")).toEqual({ code: "KeyC", shift: false });
    expect(charToKey("C")).toEqual({ code: "KeyC", shift: true });
    expect(charToKey("z")).toEqual({ code: "KeyZ", shift: false });
    expect(charToKey("A")).toEqual({ code: "KeyA", shift: true });
  });

  it("digits and their shifted symbols", () => {
    expect(charToKey("0")).toEqual({ code: "Digit0", shift: false });
    expect(charToKey("7")).toEqual({ code: "Digit7", shift: false });
    const shifted = "!@#$%^&*()";
    const digits = "1234567890";
    for (let i = 0; i < shifted.length; i++) {
      expect(charToKey(shifted[i]!)).toEqual({ code: `Digit${digits[i]}`, shift: true });
    }
  });

  it("US punctuation, unshifted and shifted", () => {
    const pairs: Array<[string, string, string]> = [
      ["`", "~", "Backquote"],
      ["-", "_", "Minus"],
      ["=", "+", "Equal"],
      ["[", "{", "BracketLeft"],
      ["]", "}", "BracketRight"],
      ["\\", "|", "Backslash"],
      [";", ":", "Semicolon"],
      ["'", '"', "Quote"],
      [",", "<", "Comma"],
      [".", ">", "Period"],
      ["/", "?", "Slash"],
    ];
    for (const [plain, shifted, code] of pairs) {
      expect(charToKey(plain)).toEqual({ code, shift: false });
      expect(charToKey(shifted)).toEqual({ code, shift: true });
    }
  });

  it("whitespace keys", () => {
    expect(charToKey(" ")).toEqual({ code: "Space", shift: false });
    expect(charToKey("\t")).toEqual({ code: "Tab", shift: false });
    expect(charToKey("\n")).toEqual({ code: "Enter", shift: false });
  });

  it("iOS smart quotes map back to the Quote key", () => {
    expect(charToKey("’")).toEqual({ code: "Quote", shift: false });
    expect(charToKey("“")).toEqual({ code: "Quote", shift: true });
  });

  it("null for anything without a US key", () => {
    for (const ch of ["é", "ß", "ñ", "€", "£", "—", "😀", "あ", "Я", "", "ab", " "]) {
      expect(charToKey(ch)).toBeNull();
    }
  });
});

describe("chord", () => {
  it("no modifiers: a plain tap", () => {
    expect(chord(0, "Escape").map(fmt)).toEqual(["+Escape:0", "-Escape:0"]);
  });

  it("Ctrl+C presses Control around the key", () => {
    expect(chord(MOD_CTRL, "KeyC").map(fmt)).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+KeyC:${MOD_CTRL}`,
      `-KeyC:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
  });

  it("several modifiers: fixed press order, reverse release, browser-style masks", () => {
    const all = MOD_CTRL | MOD_ALT | MOD_SHIFT | MOD_META;
    expect(chord(all, "KeyT").map(fmt)).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+AltLeft:${MOD_CTRL | MOD_ALT}`,
      `+ShiftLeft:${MOD_CTRL | MOD_ALT | MOD_SHIFT}`,
      `+MetaLeft:${all}`,
      `+KeyT:${all}`,
      `-KeyT:${all}`,
      `-MetaLeft:${MOD_CTRL | MOD_ALT | MOD_SHIFT}`,
      `-ShiftLeft:${MOD_CTRL | MOD_ALT}`,
      `-AltLeft:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
  });

  it("every event is a protocol key event", () => {
    for (const ev of chord(MOD_SHIFT, "ArrowLeft")) expect(ev.type).toBe("input");
  });

  it("Cmd+V through the Windows remap is Ctrl+V with Control pressed", () => {
    const events = chord(MOD_META, "KeyV").map((e) => remapInput(e, true) as KeyStroke);
    expect(events.map(fmt)).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+KeyV:${MOD_CTRL}`,
      `-KeyV:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
  });
});

function makeKeys() {
  const out: string[] = [];
  let changed = 0;
  const keys = new ExtraKeys({
    key: (e) => out.push(fmt(e)),
    text: (t) => out.push(`text:${t}`),
    changed: () => changed++,
  });
  return { keys, out, changes: () => changed };
}

describe("ExtraKeys", () => {
  it("with nothing latched, text is text and keys are plain taps", () => {
    const { keys, out } = makeKeys();
    keys.type("hello world");
    keys.tap("Enter");
    expect(out).toEqual(["text:hello world", "+Enter:0", "-Enter:0"]);
    expect(keys.latched).toBe(false);
  });

  it("latched Ctrl turns the next character into Ctrl+key, then text resumes", () => {
    const { keys, out, changes } = makeKeys();
    keys.sticky.tap("ctrl", 0);
    expect(keys.latched).toBe(true);
    keys.type("cat");
    expect(out).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+KeyC:${MOD_CTRL}`,
      `-KeyC:${MOD_CTRL}`,
      "-ControlLeft:0",
      "text:at",
    ]);
    expect(keys.latched).toBe(false);
    expect(changes()).toBe(1);
  });

  it("a capital adds Shift to the latched modifiers", () => {
    const { keys, out } = makeKeys();
    keys.sticky.tap("meta", 0);
    keys.type("Z");
    expect(out).toEqual([
      `+ShiftLeft:${MOD_SHIFT}`,
      `+MetaLeft:${MOD_SHIFT | MOD_META}`,
      `+KeyZ:${MOD_SHIFT | MOD_META}`,
      `-KeyZ:${MOD_SHIFT | MOD_META}`,
      `-MetaLeft:${MOD_SHIFT}`,
      "-ShiftLeft:0",
    ]);
  });

  it("a locked modifier applies to every character", () => {
    const { keys, out, changes } = makeKeys();
    keys.sticky.tap("alt", 0);
    keys.sticky.tap("alt", 1);
    keys.type("ab");
    expect(out.filter((s) => s.startsWith("+Key"))).toEqual([`+KeyA:${MOD_ALT}`, `+KeyB:${MOD_ALT}`]);
    expect(out.some((s) => s.startsWith("text:"))).toBe(false);
    expect(keys.latched).toBe(true);
    expect(changes()).toBe(0); // nothing spent
  });

  it("a character with no key is typed and the latch waits for one that has", () => {
    const { keys, out } = makeKeys();
    keys.sticky.tap("ctrl", 0);
    keys.type("é");
    expect(out).toEqual(["text:é"]);
    expect(keys.latched).toBe(true);
    keys.type("😀x");
    expect(out.slice(1)).toEqual([
      "text:😀",
      `+ControlLeft:${MOD_CTRL}`,
      `+KeyX:${MOD_CTRL}`,
      `-KeyX:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
  });

  it("tap merges the event's own modifiers with the latches and spends them", () => {
    const { keys, out } = makeKeys();
    keys.sticky.tap("shift", 0);
    keys.tap("ArrowRight", MOD_CTRL);
    expect(out).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+ShiftLeft:${MOD_CTRL | MOD_SHIFT}`,
      `+ArrowRight:${MOD_CTRL | MOD_SHIFT}`,
      `-ArrowRight:${MOD_CTRL | MOD_SHIFT}`,
      `-ShiftLeft:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
    expect(keys.latched).toBe(false);
  });

  it("Alt+F4 from the toolbar", () => {
    const { keys, out } = makeKeys();
    keys.sticky.tap("alt", 0);
    keys.tap("F4");
    expect(out).toEqual([`+AltLeft:${MOD_ALT}`, `+F4:${MOD_ALT}`, `-F4:${MOD_ALT}`, "-AltLeft:0"]);
  });

  it("clear drops latches and notifies only if something was lit", () => {
    const { keys, changes } = makeKeys();
    keys.clear();
    expect(changes()).toBe(0);
    keys.sticky.tap("ctrl", 0);
    keys.clear();
    expect(changes()).toBe(1);
    expect(keys.latched).toBe(false);
  });
});

/** Just enough of an <input> for SoftKeyboard: events are fired by hand. */
function fakeField() {
  const handlers = new Map<string, (e: unknown) => void>();
  const el = {
    value: "",
    type: "",
    tabIndex: 0,
    style: {},
    setAttribute() {},
    addEventListener(type: string, fn: (e: unknown) => void) {
      handlers.set(type, fn);
    },
    focus() {
      handlers.get("focus")?.({});
    },
    blur() {
      handlers.get("blur")?.({});
    },
  };
  /** The user's edit: the field now holds `value`. */
  const edit = (value: string) => {
    el.value = value;
    handlers.get("input")?.({ isComposing: false });
  };
  /** Any other event the field listens for. */
  const fire = (type: string, e: object = {}) => handlers.get(type)?.(e);
  return { el, edit, fire };
}

describe("SoftKeyboard with latched toolbar modifiers", () => {
  afterEach(() => vi.unstubAllGlobals());

  function setup() {
    const field = fakeField();
    vi.stubGlobal("document", { createElement: () => field.el, body: { appendChild() {} } });
    const { keys, out } = makeKeys();
    // wired as main.ts wires it
    const kb = new SoftKeyboard({
      sendText: (t) => keys.type(t),
      sendKeyTap: (code, mods) => keys.tap(code, mods),
      modifiersLatched: () => keys.latched,
    });
    kb.open();
    return { field, keys, out, kb };
  }

  it("typed characters become chords, then the field starts clean", () => {
    const { field, keys, out, kb } = setup();
    field.edit("l");
    field.edit("ls");
    expect(out).toEqual(["text:l", "text:s"]);
    out.length = 0;
    keys.sticky.tap("ctrl", 0);
    kb.resetContext(); // what the toolbar does on a modifier tap
    field.edit("c");
    expect(out).toEqual([`+ControlLeft:${MOD_CTRL}`, `+KeyC:${MOD_CTRL}`, `-KeyC:${MOD_CTRL}`, "-ControlLeft:0"]);
    expect(field.el.value).toBe(""); // the host holds no "c", so neither does the field
    out.length = 0;
    field.edit("x"); // latch spent: plain text again
    field.edit("");
    expect(out).toEqual(["text:x", "+Backspace:0", "-Backspace:0"]);
  });

  it("a Backspace while latched carries the modifier (Ctrl+Backspace)", () => {
    const { field, keys, out, kb } = setup();
    field.edit("word");
    keys.sticky.tap("ctrl", 0);
    kb.resetContext();
    out.length = 0;
    kb.sendKey("Backspace"); // the empty-field keydown path
    expect(out).toEqual([
      `+ControlLeft:${MOD_CTRL}`,
      `+Backspace:${MOD_CTRL}`,
      `-Backspace:${MOD_CTRL}`,
      "-ControlLeft:0",
    ]);
    expect(keys.latched).toBe(false);
  });
});

describe("SoftKeyboard context resets", () => {
  afterEach(() => vi.unstubAllGlobals());

  function setup() {
    const field = fakeField();
    vi.stubGlobal("document", { createElement: () => field.el, body: { appendChild() {} } });
    const { keys, out } = makeKeys();
    const kb = new SoftKeyboard({
      sendText: (t) => keys.type(t),
      sendKeyTap: (code, mods) => keys.tap(code, mods),
      modifiersLatched: () => keys.latched,
    });
    kb.open();
    return { field, out, kb };
  }

  it("text pasted from the toolbar can't be reached by a later autocorrect", () => {
    const { field, out, kb } = setup();
    field.edit("t");
    field.edit("te");
    field.edit("teh");
    out.length = 0;
    // Paste: the host now holds "teh" + the pasted text, so main.ts's
    // pasteText drops the context before sending it...
    kb.resetContext();
    // ...and the space after it has no "teh" in the field to autocorrect
    // into "the " (2 Backspaces + "he " would eat the paste's tail).
    field.edit(" ");
    expect(out).toEqual(["text: "]);
  });

  it("a reset mid-composition (no compositionend) leaves the field working", () => {
    const { field, out, kb } = setup();
    field.fire("compositionstart");
    // a canvas tap or toolbar key clears the field; WebKit may cancel the
    // composition without ever sending compositionend
    kb.resetContext();
    field.edit("a");
    expect(out).toEqual(["text:a"]);
    out.length = 0;
    let prevented = false;
    field.fire("keydown", {
      key: "Enter",
      code: "Enter",
      keyCode: 13,
      isComposing: false,
      shiftKey: false,
      ctrlKey: false,
      altKey: false,
      metaKey: false,
      preventDefault: () => (prevented = true),
    });
    expect(out).toEqual(["+Enter:0", "-Enter:0"]);
    expect(prevented).toBe(true);
  });
});

describe("modifierLabels", () => {
  it("Mac symbols for macOS and unknown (pre-HostInfo) hosts", () => {
    for (const os of [HostOs.MacOs, HostOs.Unknown, null]) {
      const l = modifierLabels(os);
      expect(l.meta.label).toBe("⌘");
      expect(l.alt.label).toBe("⌥");
    }
  });

  it("Win and Alt on Windows, Super on Linux", () => {
    expect(modifierLabels(HostOs.Windows).meta.label).toBe("Win");
    expect(modifierLabels(HostOs.Windows).alt.label).toBe("Alt");
    expect(modifierLabels(HostOs.Linux).meta.label).toBe("Super");
    expect(modifierLabels(HostOs.Linux).ctrl.label).toBe("Ctrl");
  });
});
