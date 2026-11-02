//! W3C `KeyboardEvent.code` → PC/AT scan code set 1, for `SendInput` with
//! `KEYEVENTF_SCANCODE`. The DOM code names the *physical* key and so does a
//! scan code, so the mapping is keyboard-layout independent — the host's
//! active layout turns the scan code into a character, exactly as if the key
//! had been pressed on a local keyboard. Values follow the UI Events
//! KeyboardEvent code spec's "Windows" column.

/// A scan code, and whether it needs the 0xE0 prefix (`KEYEVENTF_EXTENDEDKEY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCode {
    pub code: u16,
    pub extended: bool,
}

const fn sc(code: u16) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: false,
    })
}

const fn ext(code: u16) -> Option<ScanCode> {
    Some(ScanCode {
        code,
        extended: true,
    })
}

/// Returns `None` for keys with no scan code (most browser/media keys);
/// callers should log and drop those.
pub fn dom_code_to_scancode(code: &str) -> Option<ScanCode> {
    match code {
        "Escape" => sc(0x01),
        "Digit1" => sc(0x02),
        "Digit2" => sc(0x03),
        "Digit3" => sc(0x04),
        "Digit4" => sc(0x05),
        "Digit5" => sc(0x06),
        "Digit6" => sc(0x07),
        "Digit7" => sc(0x08),
        "Digit8" => sc(0x09),
        "Digit9" => sc(0x0A),
        "Digit0" => sc(0x0B),
        "Minus" => sc(0x0C),
        "Equal" => sc(0x0D),
        "Backspace" => sc(0x0E),
        "Tab" => sc(0x0F),
        "KeyQ" => sc(0x10),
        "KeyW" => sc(0x11),
        "KeyE" => sc(0x12),
        "KeyR" => sc(0x13),
        "KeyT" => sc(0x14),
        "KeyY" => sc(0x15),
        "KeyU" => sc(0x16),
        "KeyI" => sc(0x17),
        "KeyO" => sc(0x18),
        "KeyP" => sc(0x19),
        "BracketLeft" => sc(0x1A),
        "BracketRight" => sc(0x1B),
        "Enter" => sc(0x1C),
        "ControlLeft" => sc(0x1D),
        "KeyA" => sc(0x1E),
        "KeyS" => sc(0x1F),
        "KeyD" => sc(0x20),
        "KeyF" => sc(0x21),
        "KeyG" => sc(0x22),
        "KeyH" => sc(0x23),
        "KeyJ" => sc(0x24),
        "KeyK" => sc(0x25),
        "KeyL" => sc(0x26),
        "Semicolon" => sc(0x27),
        "Quote" => sc(0x28),
        "Backquote" => sc(0x29),
        "ShiftLeft" => sc(0x2A),
        "Backslash" => sc(0x2B),
        "KeyZ" => sc(0x2C),
        "KeyX" => sc(0x2D),
        "KeyC" => sc(0x2E),
        "KeyV" => sc(0x2F),
        "KeyB" => sc(0x30),
        "KeyN" => sc(0x31),
        "KeyM" => sc(0x32),
        "Comma" => sc(0x33),
        "Period" => sc(0x34),
        "Slash" => sc(0x35),
        "ShiftRight" => sc(0x36),
        "NumpadMultiply" => sc(0x37),
        "AltLeft" => sc(0x38),
        "Space" => sc(0x39),
        "CapsLock" => sc(0x3A),
        "F1" => sc(0x3B),
        "F2" => sc(0x3C),
        "F3" => sc(0x3D),
        "F4" => sc(0x3E),
        "F5" => sc(0x3F),
        "F6" => sc(0x40),
        "F7" => sc(0x41),
        "F8" => sc(0x42),
        "F9" => sc(0x43),
        "F10" => sc(0x44),
        "NumLock" => sc(0x45),
        "ScrollLock" => sc(0x46),
        "Numpad7" => sc(0x47),
        "Numpad8" => sc(0x48),
        "Numpad9" => sc(0x49),
        "NumpadSubtract" => sc(0x4A),
        "Numpad4" => sc(0x4B),
        "Numpad5" => sc(0x4C),
        "Numpad6" => sc(0x4D),
        "NumpadAdd" => sc(0x4E),
        "Numpad1" => sc(0x4F),
        "Numpad2" => sc(0x50),
        "Numpad3" => sc(0x51),
        "Numpad0" => sc(0x52),
        "NumpadDecimal" => sc(0x53),
        "IntlBackslash" => sc(0x56),
        "F11" => sc(0x57),
        "F12" => sc(0x58),
        "NumpadEqual" => sc(0x59),
        "F13" => sc(0x64),
        "F14" => sc(0x65),
        "F15" => sc(0x66),
        "F16" => sc(0x67),
        "F17" => sc(0x68),
        "F18" => sc(0x69),
        "F19" => sc(0x6A),
        "F20" => sc(0x6B),
        "F21" => sc(0x6C),
        "F22" => sc(0x6D),
        "F23" => sc(0x6E),
        "KanaMode" => sc(0x70),
        "Lang2" => sc(0x71),
        "Lang1" => sc(0x72),
        "IntlRo" => sc(0x73),
        "F24" => sc(0x76),
        "Convert" => sc(0x79),
        "NonConvert" => sc(0x7B),
        "IntlYen" => sc(0x7D),
        "NumpadComma" => sc(0x7E),
        "MediaTrackPrevious" => ext(0x10),
        "MediaTrackNext" => ext(0x19),
        "NumpadEnter" => ext(0x1C),
        "ControlRight" => ext(0x1D),
        "AudioVolumeMute" => ext(0x20),
        "MediaPlayPause" => ext(0x22),
        "MediaStop" => ext(0x24),
        "AudioVolumeDown" => ext(0x2E),
        "AudioVolumeUp" => ext(0x30),
        "NumpadDivide" => ext(0x35),
        "PrintScreen" => ext(0x37),
        "AltRight" => ext(0x38),
        "Home" => ext(0x47),
        "ArrowUp" => ext(0x48),
        "PageUp" => ext(0x49),
        "ArrowLeft" => ext(0x4B),
        "ArrowRight" => ext(0x4D),
        "End" => ext(0x4F),
        "ArrowDown" => ext(0x50),
        "PageDown" => ext(0x51),
        "Insert" => ext(0x52),
        "Delete" => ext(0x53),
        "MetaLeft" => ext(0x5B),
        "MetaRight" => ext(0x5C),
        "ContextMenu" => ext(0x5D),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_keys() {
        assert_eq!(dom_code_to_scancode("KeyA"), sc(0x1E));
        assert_eq!(dom_code_to_scancode("Space"), sc(0x39));
        assert_eq!(dom_code_to_scancode("Enter"), sc(0x1C));
        assert_eq!(dom_code_to_scancode("Backspace"), sc(0x0E));
        assert_eq!(dom_code_to_scancode("Digit0"), sc(0x0B));
        assert_eq!(dom_code_to_scancode("ShiftRight"), sc(0x36));
    }

    #[test]
    fn navigation_and_right_modifiers_are_extended() {
        // Same make code as their numpad / left-hand twins; only the E0
        // prefix tells them apart, so getting `extended` wrong types a digit
        // instead of moving the caret.
        assert_eq!(dom_code_to_scancode("ArrowUp"), ext(0x48));
        assert_eq!(dom_code_to_scancode("Numpad8"), sc(0x48));
        assert_eq!(dom_code_to_scancode("Delete"), ext(0x53));
        assert_eq!(dom_code_to_scancode("NumpadDecimal"), sc(0x53));
        assert_eq!(dom_code_to_scancode("ControlRight"), ext(0x1D));
        assert_eq!(dom_code_to_scancode("ControlLeft"), sc(0x1D));
        assert_eq!(dom_code_to_scancode("NumpadEnter"), ext(0x1C));
        assert_eq!(dom_code_to_scancode("MetaLeft"), ext(0x5B));
    }

    #[test]
    fn unknown_keys_are_none_not_garbage() {
        assert_eq!(dom_code_to_scancode("BrowserBack"), None);
        assert_eq!(dom_code_to_scancode(""), None);
        assert_eq!(dom_code_to_scancode("NotAKey"), None);
    }
}
