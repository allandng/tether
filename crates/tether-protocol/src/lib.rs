//! Transport-agnostic wire protocol for Tether.
//!
//! Every message is encoded as:
//!
//! ```text
//! [ u32 LE total_len ][ u8 msg_type ][ payload ... ]
//! ```
//!
//! where `total_len` covers everything after the length field (msg_type +
//! payload). The length prefix makes messages self-delimiting on byte-stream
//! transports (TCP); on message-oriented transports (WebSocket binary frames,
//! WebRTC data channels) exactly one protocol message is sent per transport
//! message and the prefix is redundant but harmless. See docs/protocol.md for
//! the byte-level spec.

use bytes::{BufMut, Bytes, BytesMut};

/// Protocol version sent in the `Hello` exchange.
pub const PROTOCOL_VERSION: u16 = 1;

/// Sanity cap on a single message (length-prefix value). Larger lengths are
/// rejected as corrupt rather than buffered.
pub const MAX_MESSAGE_LEN: u32 = 64 * 1024 * 1024;

/// Longest accepted `KeyCode` string (W3C `KeyboardEvent.code` values are
/// short; anything longer is corrupt).
pub const MAX_KEY_CODE_LEN: usize = 32;

/// Clipboard payloads above this are refused (never silently truncated):
/// clipboard rides the ordered control channel and must not stall input.
pub const MAX_CLIPBOARD_LEN: usize = 256 * 1024;

/// Committed text from a soft keyboard is normally 1–3 chars; cap generously
/// but bound it so a runaway paste can't masquerade as typed text.
pub const MAX_TEXT_INPUT_LEN: usize = 1024;

mod msg_type {
    pub const HELLO: u8 = 0x01;
    pub const RESOLUTION: u8 = 0x02;
    pub const FRAME_DATA: u8 = 0x03;
    pub const INPUT_EVENT: u8 = 0x04;
    pub const CLIPBOARD_DATA: u8 = 0x05;
    pub const TEXT_INPUT: u8 = 0x06;
}

/// Capability bit: this peer can be controlled (host path exists).
pub const CAP_CAN_HOST: u8 = 0b01;
/// Capability bit: this peer can control others.
pub const CAP_CAN_CONTROL: u8 = 0b10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host = 0,
    Controller = 1,
}

impl Role {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Role::Host),
            1 => Ok(Role::Controller),
            _ => Err(DecodeError::InvalidValue("role")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Jpeg = 0,
    H264 = 1,
}

impl Codec {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(Codec::Jpeg),
            1 => Ok(Codec::H264),
            _ => Err(DecodeError::InvalidValue("codec")),
        }
    }
}

/// DOM `MouseEvent.button` numbering, kept as-is on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
}

impl MouseButton {
    fn from_u8(v: u8) -> Result<Self, DecodeError> {
        match v {
            0 => Ok(MouseButton::Left),
            1 => Ok(MouseButton::Middle),
            2 => Ok(MouseButton::Right),
            _ => Err(DecodeError::InvalidValue("mouse button")),
        }
    }
}

/// Modifier bitmask carried on key events. Modifier keys also arrive as their
/// own KeyDown/KeyUp events; the mask is informational/diagnostic.
pub mod modifiers {
    pub const SHIFT: u8 = 0b0001;
    pub const CTRL: u8 = 0b0010;
    pub const ALT: u8 = 0b0100;
    pub const META: u8 = 0b1000;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    pub role: Role,
    /// Bitmask of `CAP_CAN_HOST` / `CAP_CAN_CONTROL`.
    pub capabilities: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameData {
    pub codec: Codec,
    pub seq: u32,
    /// Capture timestamp in microseconds, host clock (monotonic-ish; used for
    /// pacing/latency diagnostics, never for cross-machine comparison).
    pub timestamp_micros: u64,
    pub payload: Bytes,
}

/// Mouse coordinates are normalized fixed-point: `0..=65535` spans the host's
/// capture area. The host maps them onto its own coordinate space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove { x: u16, y: u16 },
    MouseDown { button: MouseButton, x: u16, y: u16 },
    MouseUp { button: MouseButton, x: u16, y: u16 },
    /// Scroll deltas in pixels (DOM `WheelEvent` deltaMode 0), positive = down/right.
    Scroll { dx: i16, dy: i16 },
    /// `code` is the W3C `KeyboardEvent.code` string (physical key, layout-independent).
    KeyDown { code: String, modifiers: u8 },
    KeyUp { code: String, modifiers: u8 },
}

mod input_kind {
    pub const MOUSE_MOVE: u8 = 0;
    pub const MOUSE_DOWN: u8 = 1;
    pub const MOUSE_UP: u8 = 2;
    pub const SCROLL: u8 = 3;
    pub const KEY_DOWN: u8 = 4;
    pub const KEY_UP: u8 = 5;
}

/// Clipboard content, either direction. Text-only for now; the kind byte on
/// the wire reserves room for images/files without a protocol version bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardData {
    pub text: String,
}

mod clipboard_kind {
    pub const TEXT: u8 = 0;
}

/// Committed text from a controller's soft keyboard (controller → host). The
/// host injects it as Unicode directly, bypassing the DOM-code → virtual-key
/// path — which soft keyboards can't drive (no usable `KeyboardEvent.code`)
/// and which can't express emoji or non-US layouts anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Resolution(Resolution),
    FrameData(FrameData),
    InputEvent(InputEvent),
    ClipboardData(ClipboardData),
    TextInput(TextInput),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("message truncated")]
    Truncated,
    #[error("declared length {0} exceeds maximum {MAX_MESSAGE_LEN}")]
    TooLong(u32),
    #[error("declared length does not match payload for message type {0:#04x}")]
    BadLength(u8),
    #[error("invalid value for {0}")]
    InvalidValue(&'static str),
    #[error("key code is not valid UTF-8")]
    BadKeyCode,
}

/// Result of decoding one message from the front of a buffer.
#[derive(Debug, PartialEq)]
pub enum Decoded {
    /// A complete, understood message; `consumed` bytes were used.
    Message { message: Message, consumed: usize },
    /// The buffer does not yet hold one complete message (stream transports:
    /// read more bytes and retry).
    NeedMoreData,
    /// A complete message of a type this version does not understand. The
    /// length prefix lets the caller skip it: drop `consumed` bytes and go on.
    Unknown { msg_type: u8, consumed: usize },
}

impl Message {
    /// Encode to the full wire form, including the length prefix.
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.encoded_payload_hint() + 5);
        buf.put_u32_le(0); // length placeholder
        match self {
            Message::Hello(h) => {
                buf.put_u8(msg_type::HELLO);
                buf.put_u16_le(h.version);
                buf.put_u8(h.role as u8);
                buf.put_u8(h.capabilities);
            }
            Message::Resolution(r) => {
                buf.put_u8(msg_type::RESOLUTION);
                buf.put_u32_le(r.width);
                buf.put_u32_le(r.height);
            }
            Message::FrameData(f) => {
                buf.put_u8(msg_type::FRAME_DATA);
                buf.put_u8(f.codec as u8);
                buf.put_u32_le(f.seq);
                buf.put_u64_le(f.timestamp_micros);
                buf.put_slice(&f.payload);
            }
            Message::InputEvent(ev) => {
                buf.put_u8(msg_type::INPUT_EVENT);
                encode_input_event(&mut buf, ev);
            }
            Message::ClipboardData(c) => {
                debug_assert!(c.text.len() <= MAX_CLIPBOARD_LEN);
                buf.put_u8(msg_type::CLIPBOARD_DATA);
                buf.put_u8(clipboard_kind::TEXT);
                buf.put_slice(c.text.as_bytes());
            }
            Message::TextInput(t) => {
                debug_assert!(t.text.len() <= MAX_TEXT_INPUT_LEN);
                buf.put_u8(msg_type::TEXT_INPUT);
                buf.put_slice(t.text.as_bytes());
            }
        }
        let total_len = (buf.len() - 4) as u32;
        buf[0..4].copy_from_slice(&total_len.to_le_bytes());
        buf.freeze()
    }

    fn encoded_payload_hint(&self) -> usize {
        match self {
            Message::FrameData(f) => 14 + f.payload.len(),
            _ => 64,
        }
    }

    /// Decode one message from the front of `buf`. See [`Decoded`].
    pub fn decode(buf: &[u8]) -> Result<Decoded, DecodeError> {
        if buf.len() < 4 {
            return Ok(Decoded::NeedMoreData);
        }
        let total_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if total_len > MAX_MESSAGE_LEN {
            return Err(DecodeError::TooLong(total_len));
        }
        if total_len == 0 {
            return Err(DecodeError::InvalidValue("zero-length message"));
        }
        let consumed = 4 + total_len as usize;
        if buf.len() < consumed {
            return Ok(Decoded::NeedMoreData);
        }
        let msg_type = buf[4];
        let payload = &buf[5..consumed];
        let message = match msg_type {
            msg_type::HELLO => Message::Hello(decode_hello(payload, msg_type)?),
            msg_type::RESOLUTION => Message::Resolution(decode_resolution(payload, msg_type)?),
            msg_type::FRAME_DATA => Message::FrameData(decode_frame_data(payload, msg_type)?),
            msg_type::INPUT_EVENT => Message::InputEvent(decode_input_event(payload, msg_type)?),
            msg_type::CLIPBOARD_DATA => Message::ClipboardData(decode_clipboard(payload, msg_type)?),
            msg_type::TEXT_INPUT => Message::TextInput(decode_text_input(payload)?),
            other => return Ok(Decoded::Unknown { msg_type: other, consumed }),
        };
        Ok(Decoded::Message { message, consumed })
    }
}

fn encode_input_event(buf: &mut BytesMut, ev: &InputEvent) {
    match ev {
        InputEvent::MouseMove { x, y } => {
            buf.put_u8(input_kind::MOUSE_MOVE);
            buf.put_u16_le(*x);
            buf.put_u16_le(*y);
        }
        InputEvent::MouseDown { button, x, y } => {
            buf.put_u8(input_kind::MOUSE_DOWN);
            buf.put_u8(*button as u8);
            buf.put_u16_le(*x);
            buf.put_u16_le(*y);
        }
        InputEvent::MouseUp { button, x, y } => {
            buf.put_u8(input_kind::MOUSE_UP);
            buf.put_u8(*button as u8);
            buf.put_u16_le(*x);
            buf.put_u16_le(*y);
        }
        InputEvent::Scroll { dx, dy } => {
            buf.put_u8(input_kind::SCROLL);
            buf.put_i16_le(*dx);
            buf.put_i16_le(*dy);
        }
        InputEvent::KeyDown { code, modifiers } => {
            buf.put_u8(input_kind::KEY_DOWN);
            buf.put_u8(*modifiers);
            debug_assert!(code.len() <= MAX_KEY_CODE_LEN);
            buf.put_u8(code.len() as u8);
            buf.put_slice(code.as_bytes());
        }
        InputEvent::KeyUp { code, modifiers } => {
            buf.put_u8(input_kind::KEY_UP);
            buf.put_u8(*modifiers);
            debug_assert!(code.len() <= MAX_KEY_CODE_LEN);
            buf.put_u8(code.len() as u8);
            buf.put_slice(code.as_bytes());
        }
    }
}

fn decode_hello(p: &[u8], t: u8) -> Result<Hello, DecodeError> {
    if p.len() != 4 {
        return Err(DecodeError::BadLength(t));
    }
    Ok(Hello {
        version: u16::from_le_bytes([p[0], p[1]]),
        role: Role::from_u8(p[2])?,
        capabilities: p[3],
    })
}

fn decode_resolution(p: &[u8], t: u8) -> Result<Resolution, DecodeError> {
    if p.len() != 8 {
        return Err(DecodeError::BadLength(t));
    }
    Ok(Resolution {
        width: u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
        height: u32::from_le_bytes([p[4], p[5], p[6], p[7]]),
    })
}

fn decode_frame_data(p: &[u8], t: u8) -> Result<FrameData, DecodeError> {
    if p.len() < 13 {
        return Err(DecodeError::BadLength(t));
    }
    Ok(FrameData {
        codec: Codec::from_u8(p[0])?,
        seq: u32::from_le_bytes([p[1], p[2], p[3], p[4]]),
        timestamp_micros: u64::from_le_bytes([
            p[5], p[6], p[7], p[8], p[9], p[10], p[11], p[12],
        ]),
        payload: Bytes::copy_from_slice(&p[13..]),
    })
}

fn decode_key(p: &[u8], t: u8) -> Result<(String, u8), DecodeError> {
    if p.len() < 2 {
        return Err(DecodeError::BadLength(t));
    }
    let modifiers = p[0];
    let code_len = p[1] as usize;
    if code_len > MAX_KEY_CODE_LEN {
        return Err(DecodeError::InvalidValue("key code length"));
    }
    if p.len() != 2 + code_len {
        return Err(DecodeError::BadLength(t));
    }
    let code = std::str::from_utf8(&p[2..2 + code_len])
        .map_err(|_| DecodeError::BadKeyCode)?
        .to_owned();
    Ok((code, modifiers))
}

fn decode_clipboard(p: &[u8], t: u8) -> Result<ClipboardData, DecodeError> {
    if p.is_empty() {
        return Err(DecodeError::BadLength(t));
    }
    if p[0] != clipboard_kind::TEXT {
        return Err(DecodeError::InvalidValue("clipboard kind"));
    }
    let body = &p[1..];
    if body.len() > MAX_CLIPBOARD_LEN {
        return Err(DecodeError::InvalidValue("clipboard too large"));
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| DecodeError::InvalidValue("clipboard not UTF-8"))?
        .to_owned();
    Ok(ClipboardData { text })
}

fn decode_text_input(p: &[u8]) -> Result<TextInput, DecodeError> {
    if p.len() > MAX_TEXT_INPUT_LEN {
        return Err(DecodeError::InvalidValue("text input too large"));
    }
    let text = std::str::from_utf8(p)
        .map_err(|_| DecodeError::InvalidValue("text input not UTF-8"))?
        .to_owned();
    Ok(TextInput { text })
}

fn decode_input_event(p: &[u8], t: u8) -> Result<InputEvent, DecodeError> {
    if p.is_empty() {
        return Err(DecodeError::BadLength(t));
    }
    let kind = p[0];
    let body = &p[1..];
    match kind {
        input_kind::MOUSE_MOVE => {
            if body.len() != 4 {
                return Err(DecodeError::BadLength(t));
            }
            Ok(InputEvent::MouseMove {
                x: u16::from_le_bytes([body[0], body[1]]),
                y: u16::from_le_bytes([body[2], body[3]]),
            })
        }
        input_kind::MOUSE_DOWN | input_kind::MOUSE_UP => {
            if body.len() != 5 {
                return Err(DecodeError::BadLength(t));
            }
            let button = MouseButton::from_u8(body[0])?;
            let x = u16::from_le_bytes([body[1], body[2]]);
            let y = u16::from_le_bytes([body[3], body[4]]);
            Ok(if kind == input_kind::MOUSE_DOWN {
                InputEvent::MouseDown { button, x, y }
            } else {
                InputEvent::MouseUp { button, x, y }
            })
        }
        input_kind::SCROLL => {
            if body.len() != 4 {
                return Err(DecodeError::BadLength(t));
            }
            Ok(InputEvent::Scroll {
                dx: i16::from_le_bytes([body[0], body[1]]),
                dy: i16::from_le_bytes([body[2], body[3]]),
            })
        }
        input_kind::KEY_DOWN => {
            let (code, modifiers) = decode_key(body, t)?;
            Ok(InputEvent::KeyDown { code, modifiers })
        }
        input_kind::KEY_UP => {
            let (code, modifiers) = decode_key(body, t)?;
            Ok(InputEvent::KeyUp { code, modifiers })
        }
        _ => Err(DecodeError::InvalidValue("input event kind")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Message) {
        let wire = msg.encode();
        match Message::decode(&wire).unwrap() {
            Decoded::Message { message, consumed } => {
                assert_eq!(message, msg);
                assert_eq!(consumed, wire.len(), "must consume the whole encoding");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    fn all_messages() -> Vec<Message> {
        vec![
            Message::Hello(Hello {
                version: PROTOCOL_VERSION,
                role: Role::Controller,
                capabilities: CAP_CAN_CONTROL,
            }),
            Message::Hello(Hello {
                version: 7,
                role: Role::Host,
                capabilities: CAP_CAN_HOST | CAP_CAN_CONTROL,
            }),
            Message::Resolution(Resolution { width: 3456, height: 2234 }),
            Message::FrameData(FrameData {
                codec: Codec::Jpeg,
                seq: 42,
                timestamp_micros: 1_718_000_000_123_456,
                payload: Bytes::from_static(&[0xFF, 0xD8, 0xFF, 0xD9]),
            }),
            Message::FrameData(FrameData {
                codec: Codec::H264,
                seq: u32::MAX,
                timestamp_micros: u64::MAX,
                payload: Bytes::new(), // empty payload is legal
            }),
            Message::InputEvent(InputEvent::MouseMove { x: 0, y: 65535 }),
            Message::InputEvent(InputEvent::MouseDown {
                button: MouseButton::Right,
                x: 32768,
                y: 1,
            }),
            Message::InputEvent(InputEvent::MouseUp {
                button: MouseButton::Left,
                x: 0,
                y: 0,
            }),
            Message::InputEvent(InputEvent::Scroll { dx: -120, dy: 240 }),
            Message::InputEvent(InputEvent::KeyDown {
                code: "KeyA".into(),
                modifiers: modifiers::SHIFT | modifiers::META,
            }),
            Message::InputEvent(InputEvent::KeyUp { code: "MetaLeft".into(), modifiers: 0 }),
            Message::ClipboardData(ClipboardData { text: "héllo 📋".into() }),
            Message::ClipboardData(ClipboardData { text: String::new() }),
            Message::TextInput(TextInput { text: "a".into() }),
            Message::TextInput(TextInput { text: "señor 🎯".into() }),
            Message::TextInput(TextInput { text: String::new() }),
        ]
    }

    #[test]
    fn round_trips_every_message_type() {
        for msg in all_messages() {
            round_trip(msg);
        }
    }

    #[test]
    fn truncation_at_every_boundary_is_need_more_data_never_garbage() {
        for msg in all_messages() {
            let wire = msg.encode();
            for cut in 0..wire.len() {
                match Message::decode(&wire[..cut]) {
                    Ok(Decoded::NeedMoreData) => {}
                    other => panic!("cut at {cut}/{} gave {other:?}", wire.len()),
                }
            }
        }
    }

    #[test]
    fn unknown_message_type_is_skippable() {
        let mut wire = BytesMut::new();
        wire.put_u32_le(3);
        wire.put_u8(0x7F); // unknown type
        wire.put_slice(&[1, 2]);
        // a real message follows in the same buffer
        let follow = Message::Resolution(Resolution { width: 1, height: 2 }).encode();
        wire.put_slice(&follow);

        let Decoded::Unknown { msg_type, consumed } = Message::decode(&wire).unwrap() else {
            panic!("expected Unknown");
        };
        assert_eq!(msg_type, 0x7F);
        assert_eq!(consumed, 7);
        let Decoded::Message { message, .. } = Message::decode(&wire[consumed..]).unwrap() else {
            panic!("expected Message after skipping unknown");
        };
        assert_eq!(message, Message::Resolution(Resolution { width: 1, height: 2 }));
    }

    #[test]
    fn rejects_corrupt_input() {
        // declared length exceeding cap
        let mut huge = BytesMut::new();
        huge.put_u32_le(MAX_MESSAGE_LEN + 1);
        huge.put_u8(msg_type::HELLO);
        assert_eq!(
            Message::decode(&huge),
            Err(DecodeError::TooLong(MAX_MESSAGE_LEN + 1))
        );

        // wrong payload size for a fixed-size message
        let mut bad = BytesMut::new();
        bad.put_u32_le(2);
        bad.put_u8(msg_type::RESOLUTION);
        bad.put_u8(0);
        assert_eq!(
            Message::decode(&bad),
            Err(DecodeError::BadLength(msg_type::RESOLUTION))
        );

        // invalid enum value
        let mut bad_role = BytesMut::new();
        bad_role.put_u32_le(5);
        bad_role.put_u8(msg_type::HELLO);
        bad_role.put_u16_le(1);
        bad_role.put_u8(9); // bogus role
        bad_role.put_u8(0);
        assert_eq!(
            Message::decode(&bad_role),
            Err(DecodeError::InvalidValue("role"))
        );

        // key code that is not UTF-8
        let mut bad_key = BytesMut::new();
        bad_key.put_u32_le(6);
        bad_key.put_u8(msg_type::INPUT_EVENT);
        bad_key.put_u8(input_kind::KEY_DOWN);
        bad_key.put_u8(0); // modifiers
        bad_key.put_u8(2); // code_len
        bad_key.put_slice(&[0xFF, 0xFE]);
        assert_eq!(Message::decode(&bad_key), Err(DecodeError::BadKeyCode));
    }

    #[test]
    fn version_mismatch_is_decodable_policy_lives_above() {
        // A Hello from a future version must still decode so the session layer
        // can reject it gracefully rather than choke on bytes.
        let hello = Message::Hello(Hello {
            version: PROTOCOL_VERSION + 1,
            role: Role::Controller,
            capabilities: CAP_CAN_CONTROL,
        });
        round_trip(hello);
    }

    /// Byte-for-byte vectors shared with controller/src/protocol.test.ts.
    /// If this test changes, the TS one must change identically.
    #[test]
    fn cross_implementation_byte_vectors() {
        let hello = Message::Hello(Hello {
            version: 1,
            role: Role::Controller,
            capabilities: CAP_CAN_CONTROL,
        });
        assert_eq!(&hello.encode()[..], &[5, 0, 0, 0, 0x01, 1, 0, 1, 2]);

        let resolution = Message::Resolution(Resolution { width: 1920, height: 1080 });
        assert_eq!(
            &resolution.encode()[..],
            &[9, 0, 0, 0, 0x02, 0x80, 0x07, 0, 0, 0x38, 0x04, 0, 0]
        );

        let frame = Message::FrameData(FrameData {
            codec: Codec::Jpeg,
            seq: 7,
            timestamp_micros: 0x01_0000_0002,
            payload: Bytes::from_static(&[0xAB, 0xCD]),
        });
        assert_eq!(
            &frame.encode()[..],
            &[16, 0, 0, 0, 0x03, 0, 7, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0xAB, 0xCD]
        );

        let key = Message::InputEvent(InputEvent::KeyDown {
            code: "KeyA".into(),
            modifiers: 0b1001,
        });
        assert_eq!(
            &key.encode()[..],
            &[8, 0, 0, 0, 0x04, 4, 0b1001, 4, 0x4B, 0x65, 0x79, 0x41]
        );

        let mouse = Message::InputEvent(InputEvent::MouseMove { x: 0, y: 65535 });
        assert_eq!(&mouse.encode()[..], &[6, 0, 0, 0, 0x04, 0, 0, 0, 0xFF, 0xFF]);

        let clipboard = Message::ClipboardData(ClipboardData { text: "hi".into() });
        assert_eq!(&clipboard.encode()[..], &[4, 0, 0, 0, 0x05, 0x00, 0x68, 0x69]);

        let text = Message::TextInput(TextInput { text: "hi".into() });
        assert_eq!(&text.encode()[..], &[3, 0, 0, 0, 0x06, 0x68, 0x69]);
    }

    #[test]
    fn text_input_rejects_oversize_and_non_utf8() {
        let mut huge = BytesMut::new();
        huge.put_u32_le(1 + MAX_TEXT_INPUT_LEN as u32 + 1);
        huge.put_u8(msg_type::TEXT_INPUT);
        huge.put_slice(&vec![b'a'; MAX_TEXT_INPUT_LEN + 1]);
        assert_eq!(
            Message::decode(&huge),
            Err(DecodeError::InvalidValue("text input too large"))
        );

        let mut bad_utf8 = BytesMut::new();
        bad_utf8.put_u32_le(3);
        bad_utf8.put_u8(msg_type::TEXT_INPUT);
        bad_utf8.put_slice(&[0xFF, 0xFE]);
        assert_eq!(
            Message::decode(&bad_utf8),
            Err(DecodeError::InvalidValue("text input not UTF-8"))
        );
    }

    #[test]
    fn clipboard_rejects_bad_kind_oversize_and_non_utf8() {
        let mut bad_kind = BytesMut::new();
        bad_kind.put_u32_le(2);
        bad_kind.put_u8(msg_type::CLIPBOARD_DATA);
        bad_kind.put_u8(7); // unknown kind
        assert_eq!(
            Message::decode(&bad_kind),
            Err(DecodeError::InvalidValue("clipboard kind"))
        );

        let mut huge = BytesMut::new();
        huge.put_u32_le(2 + MAX_CLIPBOARD_LEN as u32 + 1);
        huge.put_u8(msg_type::CLIPBOARD_DATA);
        huge.put_u8(0);
        huge.put_slice(&vec![b'a'; MAX_CLIPBOARD_LEN + 1]);
        assert_eq!(
            Message::decode(&huge),
            Err(DecodeError::InvalidValue("clipboard too large"))
        );

        let mut bad_utf8 = BytesMut::new();
        bad_utf8.put_u32_le(4);
        bad_utf8.put_u8(msg_type::CLIPBOARD_DATA);
        bad_utf8.put_u8(0);
        bad_utf8.put_slice(&[0xFF, 0xFE]);
        assert_eq!(
            Message::decode(&bad_utf8),
            Err(DecodeError::InvalidValue("clipboard not UTF-8"))
        );
    }

    #[test]
    fn stream_decoding_two_messages_back_to_back() {
        let a = Message::InputEvent(InputEvent::MouseMove { x: 1, y: 2 }).encode();
        let b = Message::Resolution(Resolution { width: 800, height: 600 }).encode();
        let mut stream = BytesMut::new();
        stream.put_slice(&a);
        stream.put_slice(&b);

        let Decoded::Message { message: m1, consumed } = Message::decode(&stream).unwrap() else {
            panic!()
        };
        assert_eq!(m1, Message::InputEvent(InputEvent::MouseMove { x: 1, y: 2 }));
        let Decoded::Message { message: m2, consumed: c2 } =
            Message::decode(&stream[consumed..]).unwrap()
        else {
            panic!()
        };
        assert_eq!(m2, Message::Resolution(Resolution { width: 800, height: 600 }));
        assert_eq!(consumed + c2, stream.len());
    }
}
