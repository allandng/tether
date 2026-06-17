// Soft-keyboard text capture for touch devices.
//
// iOS Safari gives no usable KeyboardEvent.code from the on-screen keyboard,
// so we summon it by focusing a hidden (but rendered — not display:none)
// input and harvest committed text from `beforeinput`. Letters/emoji/
// autocorrect become TextInput; backspace and return map to their key codes
// (the host has real virtual keys for those). The field is kept empty so it
// never scrolls or accumulates, and focus() must run inside a user gesture.

export interface KeyboardSink {
  /** Committed Unicode text (letters, emoji, autocorrect replacements). */
  sendText(text: string): void;
  /** A control key that has a real host keycode (Backspace, Enter). */
  sendKeyTap(code: string): void;
}

export class SoftKeyboard {
  private readonly input: HTMLInputElement;
  private open = false;

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
    input.setAttribute("autocapitalize", "off");
    input.setAttribute("autocorrect", "off");
    input.setAttribute("autocomplete", "off");
    input.setAttribute("spellcheck", "false");
    input.setAttribute("inputmode", "text");
    input.setAttribute("aria-hidden", "true");
    input.tabIndex = -1;

    input.addEventListener("beforeinput", (e) => this.onBeforeInput(e as InputEvent));
    // Some IMEs only surface committed text via compositionend.
    input.addEventListener("compositionend", (e) => {
      const data = (e as CompositionEvent).data;
      if (data) this.sink.sendText(data);
      input.value = "";
    });

    document.body.appendChild(input);
    this.input = input;
  }

  get isOpen(): boolean {
    return this.open;
  }

  /** Toggle the keyboard. MUST be called from a user-gesture handler. */
  toggle(): boolean {
    if (this.open) {
      this.input.blur();
      this.open = false;
    } else {
      this.input.focus();
      this.open = true;
    }
    return this.open;
  }

  blur(): void {
    this.input.blur();
    this.open = false;
  }

  private onBeforeInput(e: InputEvent): void {
    switch (e.inputType) {
      case "insertText":
      case "insertReplacementText":
      case "insertFromComposition":
      case "insertFromPaste": {
        if (e.data) this.sink.sendText(e.data);
        break;
      }
      case "deleteContentBackward":
      case "deleteWordBackward":
      case "deleteContent": {
        this.sink.sendKeyTap("Backspace");
        break;
      }
      case "insertLineBreak":
      case "insertParagraph": {
        this.sink.sendKeyTap("Enter");
        break;
      }
      default:
        break;
    }
    // Never let text accumulate in the field — keep it empty without blurring.
    queueMicrotask(() => {
      this.input.value = "";
    });
  }
}
