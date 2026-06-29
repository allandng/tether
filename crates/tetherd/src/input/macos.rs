//! macOS input injection via CGEvent.
//!
//! Synthetic events do not inherit held synthetic modifiers, so the injector
//! tracks modifier key state itself and stamps the accumulated flags onto
//! every key and mouse event (that's what makes shift/ctrl/cmd combos and
//! cmd-click work). Requires the Accessibility permission; without it macOS
//! silently discards posted events.

pub mod keymap;

use std::time::{Duration, Instant};

use anyhow::anyhow;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};
use tether_protocol::{InputEvent, MouseButton, modifiers};
use tracing::debug;

use super::InputInjector;

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP_POINTS: f64 = 5.0;

/// Synthetic CGEvents are not "user activity" to the power system, so they
/// don't wake a sleeping display — but waking the screen is exactly what a
/// remote controller's first input should do. IOPMAssertionDeclareUserActivity
/// is the supported way to say "a user did something."
mod user_activity {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    // SAFETY contract: IOPMAssertionDeclareUserActivity (IOKit/pwr_mgt) takes
    // a CFStringRef name, an IOPMUserActiveType (0 = local user), and an out
    // assertion id; it is safe to call from any thread.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: core_foundation::string::CFStringRef,
            user_type: u32,
            assertion_id: *mut u32,
        ) -> i32;
    }

    pub fn declare() {
        let name = CFString::new("tether remote input");
        let mut id: u32 = 0;
        // SAFETY: see the extern block contract; `name` outlives the call and
        // `id` is a valid out-pointer.
        unsafe {
            IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), 0, &mut id);
        }
    }
}

pub struct MacInjector {
    source: CGEventSource,
    bounds: CGRect,  // active display, in points
    display_id: u32, // active display (normalized coords map onto this)
    pos: CGPoint,
    held: [bool; 3], // left, middle, right
    flags: CGEventFlags,
    last_click: Option<(Instant, CGPoint, MouseButton)>,
    click_state: i64,
    last_activity: Option<Instant>,
}

impl MacInjector {
    pub fn new() -> anyhow::Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow!("failed to create CGEventSource"))?;
        let main = CGDisplay::main();
        let bounds = main.bounds();
        Ok(MacInjector {
            source,
            bounds,
            display_id: main.id,
            pos: CGPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            held: [false; 3],
            flags: CGEventFlags::CGEventFlagNull,
            last_click: None,
            click_state: 1,
            last_activity: None,
        })
    }

    /// Declare user activity (wakes a sleeping display), at most every few
    /// seconds while input flows.
    fn declare_activity(&mut self) {
        let due = self
            .last_activity
            .is_none_or(|at| at.elapsed() > Duration::from_secs(5));
        if due {
            user_activity::declare();
            self.last_activity = Some(Instant::now());
        }
    }

    fn post_mouse(&self, event_type: CGEventType, button: CGMouseButton) -> anyhow::Result<()> {
        let event = CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, button)
            .map_err(|()| anyhow!("failed to create mouse event"))?;
        event.set_flags(self.flags);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn move_event_type(&self) -> (CGEventType, CGMouseButton) {
        if self.held[0] {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.held[2] {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.held[1] {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        }
    }

    fn update_click_state(&mut self, button: MouseButton) {
        let now = Instant::now();
        self.click_state = match self.last_click {
            Some((at, pos, prev_button))
                if prev_button == button
                    && now.duration_since(at) < DOUBLE_CLICK_WINDOW
                    && (pos.x - self.pos.x).abs() < DOUBLE_CLICK_SLOP_POINTS
                    && (pos.y - self.pos.y).abs() < DOUBLE_CLICK_SLOP_POINTS =>
            {
                self.click_state + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, self.pos, button));
    }
}

impl InputInjector for MacInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        self.declare_activity();
        match *event {
            InputEvent::MouseMove { x, y } => {
                self.pos = normalized_to_point(&self.bounds, x, y);
                let (event_type, button) = self.move_event_type();
                self.post_mouse(event_type, button)?;
            }
            InputEvent::MouseDown { button, x, y } => {
                self.pos = normalized_to_point(&self.bounds, x, y);
                self.update_click_state(button);
                self.held[button_index(button)] = true;
                let (event_type, cg_button) = button_down_type(button);
                let event =
                    CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, cg_button)
                        .map_err(|()| anyhow!("failed to create mouse event"))?;
                event
                    .set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_state);
                event.set_flags(self.flags);
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::MouseUp { button, x, y } => {
                self.pos = normalized_to_point(&self.bounds, x, y);
                self.held[button_index(button)] = false;
                let (event_type, cg_button) = button_up_type(button);
                let event =
                    CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, cg_button)
                        .map_err(|()| anyhow!("failed to create mouse event"))?;
                event
                    .set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_state);
                event.set_flags(self.flags);
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::Scroll { dx, dy } => {
                // DOM: positive deltaY = content scrolls down. CG axis 1:
                // positive = scroll up. Negate both axes.
                let event = CGEvent::new_scroll_event(
                    self.source.clone(),
                    ScrollEventUnit::PIXEL,
                    2,
                    -i32::from(dy),
                    -i32::from(dx),
                    0,
                )
                .map_err(|()| anyhow!("failed to create scroll event"))?;
                event.set_flags(self.flags);
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::KeyDown {
                ref code,
                modifiers: mask,
            } => {
                let Some(vk) = keymap::dom_code_to_vk(code) else {
                    debug!(code, "no macOS virtual key for DOM code, dropping");
                    return Ok(());
                };
                if let Some(flag) = modifier_flag_for_vk(vk) {
                    self.flags.insert(flag);
                }
                let event = CGEvent::new_keyboard_event(self.source.clone(), vk, true)
                    .map_err(|()| anyhow!("failed to create key event"))?;
                // Belt and braces: trust our tracked state, but OR in the
                // browser-reported mask in case a modifier down was lost.
                event.set_flags(self.flags | mask_to_flags(mask));
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::KeyUp {
                ref code,
                modifiers: mask,
            } => {
                let Some(vk) = keymap::dom_code_to_vk(code) else {
                    return Ok(());
                };
                if let Some(flag) = modifier_flag_for_vk(vk) {
                    self.flags.remove(flag);
                }
                let event = CGEvent::new_keyboard_event(self.source.clone(), vk, false)
                    .map_err(|()| anyhow!("failed to create key event"))?;
                event.set_flags(self.flags | mask_to_flags(mask));
                event.post(CGEventTapLocation::HID);
            }
        }
        Ok(())
    }

    /// Inject committed Unicode text via CGEventKeyboardSetUnicodeString:
    /// keycode 0, the string riding the keydown, keyup completing the tap.
    /// Layout-independent and emoji-capable — what soft keyboards need.
    fn inject_text(&mut self, text: &str) -> anyhow::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.declare_activity();
        let down = CGEvent::new_keyboard_event(self.source.clone(), 0, true)
            .map_err(|()| anyhow!("failed to create text keydown event"))?;
        down.set_string(text);
        // Held modifiers (e.g. a stuck cmd) would corrupt typed text; inject
        // the unicode string with no flags so it lands verbatim.
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(self.source.clone(), 0, false)
            .map_err(|()| anyhow!("failed to create text keyup event"))?;
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    /// Release every held mouse button and clear modifier flags. Called when a
    /// controller session ends so a mid-drag disconnect can't strand a button.
    fn release_all(&mut self) {
        for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            if self.held[button_index(button)] {
                self.held[button_index(button)] = false;
                let (event_type, cg_button) = button_up_type(button);
                if let Ok(event) =
                    CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, cg_button)
                {
                    event.post(CGEventTapLocation::HID);
                }
            }
        }
        self.flags = CGEventFlags::CGEventFlagNull;
    }

    /// Point subsequent normalized coordinates at `display_id` (multi-monitor
    /// switch). Normalized 0..65535 then spans that display's bounds.
    fn set_active_display(&mut self, display_id: u32) {
        if display_id == self.display_id {
            return;
        }
        self.bounds = CGDisplay::new(display_id).bounds();
        self.display_id = display_id;
    }
}

/// Map protocol-normalized coordinates onto display points.
pub fn normalized_to_point(bounds: &CGRect, x: u16, y: u16) -> CGPoint {
    CGPoint::new(
        bounds.origin.x + f64::from(x) / 65535.0 * bounds.size.width,
        bounds.origin.y + f64::from(y) / 65535.0 * bounds.size.height,
    )
}

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn button_down_type(button: MouseButton) -> (CGEventType, CGMouseButton) {
    match button {
        MouseButton::Left => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        MouseButton::Middle => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        MouseButton::Right => (CGEventType::RightMouseDown, CGMouseButton::Right),
    }
}

fn button_up_type(button: MouseButton) -> (CGEventType, CGMouseButton) {
    match button {
        MouseButton::Left => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        MouseButton::Middle => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        MouseButton::Right => (CGEventType::RightMouseUp, CGMouseButton::Right),
    }
}

/// Virtual key → the modifier flag it controls, for tracked-state updates.
fn modifier_flag_for_vk(vk: u16) -> Option<CGEventFlags> {
    match vk {
        0x38 | 0x3C => Some(CGEventFlags::CGEventFlagShift),
        0x3B | 0x3E => Some(CGEventFlags::CGEventFlagControl),
        0x3A | 0x3D => Some(CGEventFlags::CGEventFlagAlternate),
        0x37 | 0x36 => Some(CGEventFlags::CGEventFlagCommand),
        _ => None,
    }
}

fn mask_to_flags(mask: u8) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if mask & modifiers::SHIFT != 0 {
        flags.insert(CGEventFlags::CGEventFlagShift);
    }
    if mask & modifiers::CTRL != 0 {
        flags.insert(CGEventFlags::CGEventFlagControl);
    }
    if mask & modifiers::ALT != 0 {
        flags.insert(CGEventFlags::CGEventFlagAlternate);
    }
    if mask & modifiers::META != 0 {
        flags.insert(CGEventFlags::CGEventFlagCommand);
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(w: f64, h: f64) -> CGRect {
        CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &core_graphics::geometry::CGSize::new(w, h),
        )
    }

    #[test]
    fn normalized_coordinates_span_the_display() {
        // 1728x1117 points (Retina MBP logical size) — capture is 2x pixels,
        // but normalized coords are resolution-independent by design.
        let b = bounds(1728.0, 1117.0);
        let top_left = normalized_to_point(&b, 0, 0);
        assert_eq!((top_left.x, top_left.y), (0.0, 0.0));
        let bottom_right = normalized_to_point(&b, 65535, 65535);
        assert!((bottom_right.x - 1728.0).abs() < 0.001);
        assert!((bottom_right.y - 1117.0).abs() < 0.001);
        let center = normalized_to_point(&b, 32768, 32768);
        assert!((center.x - 864.0).abs() < 0.5);
        assert!((center.y - 558.5).abs() < 0.5);
    }

    #[test]
    fn modifier_masks_translate_to_cg_flags() {
        let f = mask_to_flags(modifiers::SHIFT | modifiers::META);
        assert!(f.contains(CGEventFlags::CGEventFlagShift));
        assert!(f.contains(CGEventFlags::CGEventFlagCommand));
        assert!(!f.contains(CGEventFlags::CGEventFlagControl));
        assert_eq!(mask_to_flags(0), CGEventFlags::CGEventFlagNull);
    }

    #[test]
    fn modifier_keys_map_to_their_flags() {
        assert_eq!(
            modifier_flag_for_vk(keymap::dom_code_to_vk("MetaLeft").unwrap()),
            Some(CGEventFlags::CGEventFlagCommand)
        );
        assert_eq!(
            modifier_flag_for_vk(keymap::dom_code_to_vk("ShiftRight").unwrap()),
            Some(CGEventFlags::CGEventFlagShift)
        );
        assert_eq!(
            modifier_flag_for_vk(keymap::dom_code_to_vk("KeyA").unwrap()),
            None
        );
    }
}
