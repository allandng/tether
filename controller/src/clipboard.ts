// Controller-side clipboard plumbing.
//
// Controller → host rides the paste keystroke: the Cmd/Ctrl+V keydown is NOT
// forwarded; instead the browser's default paste action fires, the `paste`
// event hands us the local clipboard text, and we send ClipboardData followed
// by a synthetic V tap — same ordered channel, so the host pasteboard is set
// before the keystroke lands. A short fallback timer keeps the key alive when
// the clipboard is empty or the paste event never fires.
//
// Host → controller: auto-write via the async Clipboard API where the
// context allows (secure context + permission); otherwise the UI shows a
// chip and the user's tap supplies the gesture for the execCommand fallback.

export interface ClipboardOut {
  sendClipboard(text: string): void;
  /** Send a full key tap (down+up) for a DOM code with a modifier mask. */
  sendKeyTap(code: string, modifiers: number): void;
}

export const PASTE_FALLBACK_MS = 75;

/** Pastes at or above this size delay the V keystroke briefly: clipboard and
 * input ride different WebRTC channels, so cross-channel ordering isn't
 * guaranteed and a large transfer needs a head start. */
export const LARGE_PASTE_BYTES = 32 * 1024;
export const LARGE_PASTE_DELAY_MS = 150;

/** State machine for the Cmd/Ctrl+V intercept. */
export class PasteFlow {
  private armedModifiers: number | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private swallowKeyUps = 0;

  constructor(
    private readonly out: ClipboardOut,
    private readonly fallbackMs: number = PASTE_FALLBACK_MS,
  ) {}

  /**
   * Paste combo keydown observed. Always consumes the keydown (the caller
   * must neither forward KeyV nor preventDefault — the default action is
   * what produces the paste event).
   */
  onPasteCombo(modifiers: number): void {
    if (this.armedModifiers !== null) return; // auto-repeat while armed
    this.armedModifiers = modifiers;
    this.timer = setTimeout(() => this.finish(null), this.fallbackMs);
  }

  /** Browser paste event. Returns true if an armed combo consumed it. */
  onPasteEvent(text: string | null): boolean {
    if (this.armedModifiers === null) return false;
    this.finish(text);
    return true;
  }

  /** KeyV keyup observed. Returns true if it belongs to a consumed combo. */
  onPasteKeyUp(): boolean {
    if (this.swallowKeyUps > 0) {
      this.swallowKeyUps--;
      return true;
    }
    return false;
  }

  private finish(text: string | null): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    const modifiers = this.armedModifiers ?? 0;
    this.armedModifiers = null;
    this.swallowKeyUps++;
    if (text) {
      this.out.sendClipboard(text);
      if (text.length >= LARGE_PASTE_BYTES) {
        setTimeout(() => this.out.sendKeyTap("KeyV", modifiers), LARGE_PASTE_DELAY_MS);
        return;
      }
    }
    this.out.sendKeyTap("KeyV", modifiers);
  }
}

/** Host clipboard arriving at the controller. */
export class HostClipboard {
  private latest: string | null = null;

  constructor(private readonly onChip: (visible: boolean) => void) {}

  async receive(text: string): Promise<void> {
    this.latest = text;
    if (await this.tryAutoWrite(text)) {
      this.onChip(false);
    } else {
      this.onChip(true);
    }
  }

  /** Call from a user gesture (chip tap): copy `latest` locally. */
  async copyNow(): Promise<boolean> {
    if (this.latest === null) return false;
    if (await this.tryAutoWrite(this.latest)) {
      this.onChip(false);
      return true;
    }
    const ok = execCommandCopy(this.latest);
    if (ok) this.onChip(false);
    return ok;
  }

  private async tryAutoWrite(text: string): Promise<boolean> {
    const clipboard = globalThis.navigator?.clipboard;
    if (!clipboard?.writeText) return false; // insecure context
    try {
      await clipboard.writeText(text);
      return true;
    } catch {
      return false; // permission denied / no transient activation
    }
  }
}

/** Legacy copy path: works in insecure contexts, needs a user gesture. */
function execCommandCopy(text: string): boolean {
  if (typeof document === "undefined") return false;
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.focus();
  textarea.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  textarea.remove();
  return ok;
}
