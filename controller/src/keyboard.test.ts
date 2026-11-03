import { describe, expect, it } from "vitest";
import {
  CONTEXT_MAX,
  type KeyLike,
  beforeInputTap,
  chunkText,
  diffEdit,
  graphemes,
  keydownTap,
  trimContext,
} from "./keyboard";
import { MAX_TEXT_INPUT_LEN, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT, encodeTextInput } from "./protocol";

const key = (over: Partial<KeyLike>): KeyLike => ({
  key: "",
  code: "",
  keyCode: 0,
  isComposing: false,
  shiftKey: false,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  ...over,
});

const utf8 = (s: string) => new TextEncoder().encode(s).length;

describe("keydownTap", () => {
  it("iOS soft Return (keydown only) taps Enter", () => {
    expect(keydownTap(key({ key: "Enter", code: "Enter", keyCode: 13 }), true)).toEqual({
      code: "Enter",
      modifiers: 0,
    });
    // ...even with context in the field
    expect(keydownTap(key({ key: "Enter", code: "Enter", keyCode: 13 }), false)?.code).toBe("Enter");
  });

  it("Backspace is tapped only on an empty field", () => {
    const bs = key({ key: "Backspace", code: "Backspace", keyCode: 8 });
    expect(keydownTap(bs, true)).toEqual({ code: "Backspace", modifiers: 0 });
    // non-empty: the field edits its context and the input diff sends it
    expect(keydownTap(bs, false)).toBeNull();
  });

  it("falls back to keyCode when key is Unidentified", () => {
    expect(keydownTap(key({ key: "Unidentified", keyCode: 13 }), true)?.code).toBe("Enter");
    expect(keydownTap(key({ key: "Unidentified", keyCode: 8 }), true)?.code).toBe("Backspace");
    expect(keydownTap(key({ key: "Unidentified", keyCode: 65 }), true)).toBeNull();
  });

  it("never intercepts IME composition", () => {
    expect(keydownTap(key({ key: "Enter", code: "Enter", keyCode: 13, isComposing: true }), true)).toBeNull();
    expect(keydownTap(key({ key: "Backspace", code: "Backspace", keyCode: 229 }), true)).toBeNull();
    expect(keydownTap(key({ key: "Process", keyCode: 229 }), true)).toBeNull();
  });

  it("leaves text keys to the field", () => {
    expect(keydownTap(key({ key: "a", code: "KeyA", keyCode: 65 }), true)).toBeNull();
    expect(keydownTap(key({ key: "A", code: "KeyA", keyCode: 65, shiftKey: true }), true)).toBeNull();
    expect(keydownTap(key({ key: "å", code: "KeyA", altKey: true }), true)).toBeNull();
    expect(keydownTap(key({ key: " ", code: "Space", keyCode: 32 }), true)).toBeNull();
  });

  it("taps Tab, Escape, Delete, arrows and nav keys with their modifiers", () => {
    for (const k of ["Tab", "Escape", "Delete", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"]) {
      expect(keydownTap(key({ key: k, code: k }), false)).toEqual({ code: k, modifiers: 0 });
    }
    expect(keydownTap(key({ key: "ArrowLeft", code: "ArrowLeft", shiftKey: true }), false)).toEqual({
      code: "ArrowLeft",
      modifiers: MOD_SHIFT,
    });
    expect(keydownTap(key({ key: "Tab", code: "Tab", shiftKey: true }), true)?.modifiers).toBe(MOD_SHIFT);
  });

  it("maps the numpad Enter to Enter", () => {
    expect(keydownTap(key({ key: "Enter", code: "NumpadEnter", keyCode: 13 }), true)?.code).toBe("Enter");
  });

  it("forwards hardware Ctrl/Cmd shortcuts, but lets the platform's paste combo paste into the field", () => {
    expect(keydownTap(key({ key: "c", code: "KeyC", metaKey: true }), false)).toEqual({
      code: "KeyC",
      modifiers: MOD_META,
    });
    expect(keydownTap(key({ key: "c", code: "KeyC", ctrlKey: true, altKey: true }), true)).toEqual({
      code: "KeyC",
      modifiers: MOD_CTRL | MOD_ALT,
    });
    // Apple client (iPad): Cmd+V pastes into the field; Ctrl+V pastes nothing
    // there, so it goes to the host as a key
    expect(keydownTap(key({ key: "v", code: "KeyV", metaKey: true }), true, true)).toBeNull();
    expect(keydownTap(key({ key: "v", code: "KeyV", metaKey: true, shiftKey: true }), true, true)).toBeNull();
    expect(keydownTap(key({ key: "v", code: "KeyV", ctrlKey: true }), true, true)).toEqual({
      code: "KeyV",
      modifiers: MOD_CTRL,
    });
    expect(keydownTap(key({ key: "v", code: "KeyV", ctrlKey: true }), false, true)?.modifiers).toBe(MOD_CTRL);
    // elsewhere it's the other way round
    expect(keydownTap(key({ key: "v", code: "KeyV", ctrlKey: true }), true, false)).toBeNull();
    expect(keydownTap(key({ key: "v", code: "KeyV", metaKey: true }), true, false)).toEqual({
      code: "KeyV",
      modifiers: MOD_META,
    });
    // a bare modifier press is nothing to tap
    expect(keydownTap(key({ key: "Meta", code: "MetaLeft", metaKey: true }), true)).toBeNull();
    expect(keydownTap(key({ key: "Control", code: "ControlLeft", ctrlKey: true }), true)).toBeNull();
  });

  it("modified Backspace on a non-empty field is still the field's (Alt+Backspace deletes a word there)", () => {
    expect(keydownTap(key({ key: "Backspace", code: "Backspace", altKey: true }), false)).toBeNull();
    expect(keydownTap(key({ key: "Backspace", code: "Backspace", altKey: true }), true)).toEqual({
      code: "Backspace",
      modifiers: MOD_ALT,
    });
  });
});

describe("beforeInputTap", () => {
  it("a line break taps Enter unless keydown already did", () => {
    expect(beforeInputTap("insertLineBreak", false, null)).toBe("Enter");
    expect(beforeInputTap("insertParagraph", true, null)).toBe("Enter");
    expect(beforeInputTap("insertLineBreak", true, "Enter")).toBeNull();
  });

  it("deletes tap only on an empty field, and only once per keystroke", () => {
    expect(beforeInputTap("deleteContentBackward", true, null)).toBe("Backspace");
    expect(beforeInputTap("deleteWordBackward", true, null)).toBe("Backspace");
    expect(beforeInputTap("deleteContentForward", true, null)).toBe("Delete");
    expect(beforeInputTap("deleteContentBackward", true, "Backspace")).toBeNull();
    // non-empty: the value changes, and the input diff sends it
    expect(beforeInputTap("deleteContentBackward", false, null)).toBeNull();
    expect(beforeInputTap("deleteByCut", true, null)).toBeNull();
  });

  it("text input is left to the diff", () => {
    for (const t of ["insertText", "insertReplacementText", "insertFromPaste", "insertCompositionText"]) {
      expect(beforeInputTap(t, true, null)).toBeNull();
    }
  });
});

describe("diffEdit", () => {
  it("no change sends nothing", () => {
    expect(diffEdit("same", "same")).toEqual({ backspaces: 0, text: "" });
    expect(diffEdit("", "")).toEqual({ backspaces: 0, text: "" });
  });

  it("appending types only the new text", () => {
    expect(diffEdit("", "h")).toEqual({ backspaces: 0, text: "h" });
    expect(diffEdit("hel", "hell")).toEqual({ backspaces: 0, text: "l" });
    expect(diffEdit("hello", "hello ")).toEqual({ backspaces: 0, text: " " });
  });

  it("a backspace deletes one", () => {
    expect(diffEdit("hello", "hell")).toEqual({ backspaces: 1, text: "" });
    expect(diffEdit("a", "")).toEqual({ backspaces: 1, text: "" });
  });

  it("autocorrect replacing a word mid-way rewrites only from the first change", () => {
    expect(diffEdit("teh", "the")).toEqual({ backspaces: 2, text: "he" });
    expect(diffEdit("I saw teh", "I saw the")).toEqual({ backspaces: 2, text: "he" });
    // correction + the space that triggered it, in one input
    expect(diffEdit("go teh", "go the ")).toEqual({ backspaces: 2, text: "he " });
    // QuickType: tap a suggestion for the partial word
    expect(diffEdit("see you tom", "see you tomorrow ")).toEqual({ backspaces: 0, text: "orrow " });
    // iOS double-space shortcut
    expect(diffEdit("done ", "done. ")).toEqual({ backspaces: 1, text: ". " });
  });

  it("delete-word removes the whole word", () => {
    expect(diffEdit("hello world", "hello ")).toEqual({ backspaces: 5, text: "" });
  });

  it("replacing the whole buffer (dictation revision) retypes it", () => {
    expect(diffEdit("abc", "xyz")).toEqual({ backspaces: 3, text: "xyz" });
    expect(diffEdit("I scream", "ice cream")).toEqual({ backspaces: 8, text: "ice cream" });
    expect(diffEdit("abc", "")).toEqual({ backspaces: 3, text: "" });
  });

  it("Hangul syllable composition replaces the previous syllable", () => {
    expect(diffEdit("ㅎ", "하")).toEqual({ backspaces: 1, text: "하" });
  });

  it("emoji: one Backspace per emoji, never half a surrogate pair", () => {
    expect(diffEdit("hi", "hi😀")).toEqual({ backspaces: 0, text: "😀" });
    expect(diffEdit("hi 😀", "hi ")).toEqual({ backspaces: 1, text: "" });
    // 😀 and 😃 share a high surrogate: a code-unit diff would split them
    expect(diffEdit("a😀", "a😃")).toEqual({ backspaces: 1, text: "😃" });
    // a ZWJ family and a flag are one character each
    expect(diffEdit("ok 👨‍👩‍👧", "ok ")).toEqual({ backspaces: 1, text: "" });
    expect(diffEdit("🇺🇸", "")).toEqual({ backspaces: 1, text: "" });
  });

  it("combining marks travel with their base character", () => {
    expect(diffEdit("cafe", "café")).toEqual({ backspaces: 1, text: "é" });
    expect(diffEdit("café", "caf")).toEqual({ backspaces: 1, text: "" });
  });

  it("an edit away from the end still leaves the host matching the field", () => {
    // caret moved inside the field (spacebar trackpad), then a char typed
    const prev = "hello world";
    const next = "hello, world";
    const { backspaces, text } = diffEdit(prev, next);
    const host = graphemes(prev).slice(0, -backspaces || undefined).join("") + text;
    expect(host).toBe(next);
  });
});

describe("trimContext", () => {
  it("keeps short context unchanged", () => {
    const s = "a".repeat(CONTEXT_MAX);
    expect(trimContext(s)).toBe(s);
  });

  it("keeps the last word and its trailing space once too long", () => {
    const long = `${"lorem ".repeat(12)}ipsum`;
    expect(long.length).toBeGreaterThan(CONTEXT_MAX);
    expect(trimContext(long)).toBe("ipsum");
    expect(trimContext(`${long}  `)).toBe("ipsum  ");
    expect(trimContext("one two three", 8)).toBe("three");
  });

  it("drops everything when one word alone is too long", () => {
    expect(trimContext("x".repeat(CONTEXT_MAX + 1))).toBe("");
    expect(trimContext(`hi ${"x".repeat(10)}`, 8)).toBe("");
  });
});

describe("chunkText", () => {
  it("passes short text through and drops empty text", () => {
    expect(chunkText("hello")).toEqual(["hello"]);
    expect(chunkText("")).toEqual([]);
  });

  it("splits ASCII at the byte limit", () => {
    const s = "x".repeat(MAX_TEXT_INPUT_LEN * 2 + 452);
    expect(chunkText(s).map((c) => c.length)).toEqual([MAX_TEXT_INPUT_LEN, MAX_TEXT_INPUT_LEN, 452]);
  });

  it("never splits a surrogate pair or a UTF-8 sequence", () => {
    for (const s of ["😀".repeat(300), "漢".repeat(400), "é".repeat(700), `a${"😀".repeat(300)}`]) {
      const chunks = chunkText(s);
      expect(chunks.length).toBeGreaterThan(1);
      expect(chunks.join("")).toBe(s);
      for (const c of chunks) {
        expect(utf8(c)).toBeLessThanOrEqual(MAX_TEXT_INPUT_LEN);
        expect(c).not.toMatch(/^[\uDC00-\uDFFF]|[\uD800-\uDBFF]$/); // no torn pair
        expect(() => encodeTextInput({ text: c })).not.toThrow();
      }
    }
    // 4-byte emoji: 256 fit exactly in 1024 bytes
    expect(chunkText("😀".repeat(300)).map((c) => Array.from(c).length)).toEqual([256, 44]);
    // 3-byte CJK: 341 fit (1023 bytes)
    expect(chunkText("漢".repeat(400)).map((c) => c.length)).toEqual([341, 59]);
  });

  it("counts a lone surrogate as the 3-byte replacement it encodes to", () => {
    const s = "\uD800".repeat(400);
    const chunks = chunkText(s);
    expect(chunks.map((c) => c.length)).toEqual([341, 59]);
    for (const c of chunks) expect(utf8(c)).toBeLessThanOrEqual(MAX_TEXT_INPUT_LEN);
  });

  it("honors a custom limit", () => {
    expect(chunkText("abcdef", 4)).toEqual(["abcd", "ef"]);
    expect(chunkText("a😀b", 4)).toEqual(["a", "😀", "b"]);
  });
});
