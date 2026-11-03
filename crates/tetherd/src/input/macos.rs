//! macOS input injection via CGEvent.
//!
//! Synthetic events do not inherit held synthetic modifiers, so the injector
//! tracks which keys it holds down itself and stamps the resulting modifier
//! flags onto every key and mouse event (that's what makes shift/ctrl/cmd
//! combos and cmd-click work). The same held-key set lets a disconnect release
//! everything. Requires the Accessibility permission; without it macOS
//! silently discards posted events.
//!
//! Two kinds of "key" aren't keyboard virtual keys at all: Caps Lock is a lock
//! state in the HID system (synced from the protocol's modifiers byte via
//! IOHIDSetModifierLockState — or, for controllers that predate the lock
//! bits, toggled by their `CapsLock` key events: [`LegacyCapsKey`]), and
//! media/volume keys are NSSystemDefined aux-control-button events.

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
use tracing::{debug, info, warn};

use super::InputInjector;

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP_POINTS: f64 = 5.0;
/// How long cached display bounds are trusted. CGDisplayBounds is a
/// WindowServer round-trip (~35 µs measured), too dear for every mouse move
/// but cheap at this rate; a resolution/scaling change on the *same* display
/// is picked up within this window, and mouse-downs always re-query.
const BOUNDS_TTL: Duration = Duration::from_millis(250);
/// How often to retry opening IOHIDSystem (for Caps Lock) after it failed.
const HID_RETRY: Duration = Duration::from_secs(5);
/// A legacy sender's `CapsLock` keyup this soon after its keydown is the
/// release of one press (Windows/Linux/ChromeOS browsers send both); later
/// than this it is a separate press (macOS browsers send keydown to turn the
/// lock on and only a keyup to turn it off). See [`LegacyCapsKey`].
const LEGACY_CAPS_PRESS: Duration = Duration::from_millis(500);

/// Synthetic CGEvents are not "user activity" to the power system, so they
/// don't wake a sleeping display — but waking the screen is exactly what a
/// remote controller's first input should do. IOPMAssertionDeclareUserActivity
/// is the supported way to say "a user did something."
mod user_activity {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use tracing::debug;

    // SAFETY contract: IOPMAssertionDeclareUserActivity (IOKit/pwr_mgt) takes
    // a CFStringRef name, an IOPMUserActiveType (0 = local user), and an
    // in/out assertion id; IOPMAssertionRelease takes an id. Both are safe to
    // call from any thread.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: CFStringRef,
            user_type: u32,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }

    /// kIOPMNullAssertionID.
    const NULL_ID: u32 = 0;

    /// One "user is active" assertion, re-declared in place. IOPMLib.h:
    /// "track the returned value for assertionID, and pass it in as an
    /// argument on each call" — a fresh id per call stacks up a new assertion
    /// every time.
    pub struct UserActivity {
        id: u32,
    }

    impl UserActivity {
        pub const fn new() -> Self {
            UserActivity { id: NULL_ID }
        }

        pub fn declare(&mut self) {
            let name = CFString::new("tether remote input");
            // SAFETY: see the extern block contract; `name` outlives the call
            // and `self.id` is a valid in/out pointer.
            let rc = unsafe {
                IOPMAssertionDeclareUserActivity(name.as_concrete_TypeRef(), 0, &mut self.id)
            };
            if rc != 0 {
                debug!(rc, "IOPMAssertionDeclareUserActivity failed");
                self.id = NULL_ID;
            }
        }

        /// Drop the assertion now rather than waiting out its timeout (the
        /// session that was generating activity has ended).
        pub fn release(&mut self) {
            if self.id != NULL_ID {
                // SAFETY: `self.id` came from IOPMAssertionDeclareUserActivity.
                unsafe { IOPMAssertionRelease(self.id) };
                self.id = NULL_ID;
            }
        }
    }

    impl Drop for UserActivity {
        fn drop(&mut self) {
            self.release();
        }
    }
}

/// Caps Lock is a lock *state* in the HID system, not a held key: a synthetic
/// kVK_CapsLock event doesn't reliably toggle it. IOHIDSetModifierLockState on
/// an IOHIDSystem parameter connection does (and lights the LED), and
/// IOHIDGetModifierLockState reads it back.
mod hid_system {
    use std::ffi::{c_char, c_int, c_void};

    /// mach_port_t / io_object_t / io_service_t / io_connect_t.
    type MachPort = u32;
    const KERN_SUCCESS: i32 = 0;
    /// kIOHIDParamConnectType (IOKit/hidsystem/IOHIDShared.h).
    const PARAM_CONNECT_TYPE: u32 = 1;
    /// kIOHIDCapsLockState (IOKit/hidsystem/IOHIDParameter.h).
    const CAPS_LOCK_STATE: c_int = 1;

    // SAFETY contract: `mach_task_self_` is libSystem's send right for this
    // task, initialized before main and never written afterwards.
    unsafe extern "C" {
        static mach_task_self_: MachPort;
    }

    // SAFETY contract (IOKitLib.h / IOHIDLib.h): IOServiceMatching returns a
    // +1 CFMutableDictionaryRef (or NULL) that IOServiceGetMatchingService
    // consumes; the returned service is +1 and released with IOObjectRelease;
    // a connection from IOServiceOpen is closed with IOServiceClose. The lock
    // state calls take an open connection and a kIOHID*LockState selector.
    // main_port 0 = kIOMainPortDefault.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: MachPort, matching: *mut c_void) -> MachPort;
        fn IOServiceOpen(
            service: MachPort,
            owning_task: MachPort,
            connect_type: u32,
            connect: *mut MachPort,
        ) -> i32;
        fn IOServiceClose(connect: MachPort) -> i32;
        fn IOObjectRelease(object: MachPort) -> i32;
        fn IOHIDGetModifierLockState(handle: MachPort, selector: c_int, state: *mut bool) -> i32;
        fn IOHIDSetModifierLockState(handle: MachPort, selector: c_int, state: bool) -> i32;
    }

    pub struct HidSystem {
        connect: MachPort,
    }

    impl HidSystem {
        pub fn open() -> Option<Self> {
            // SAFETY: see the extern block contracts; every object obtained
            // here is released on every path.
            unsafe {
                let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
                if matching.is_null() {
                    return None;
                }
                let service = IOServiceGetMatchingService(0, matching);
                if service == 0 {
                    return None;
                }
                let mut connect: MachPort = 0;
                let rc = IOServiceOpen(service, mach_task_self_, PARAM_CONNECT_TYPE, &mut connect);
                IOObjectRelease(service);
                (rc == KERN_SUCCESS && connect != 0).then_some(HidSystem { connect })
            }
        }

        pub fn caps_lock(&self) -> Option<bool> {
            let mut on = false;
            // SAFETY: open connection, valid selector, valid out-pointer.
            let rc = unsafe { IOHIDGetModifierLockState(self.connect, CAPS_LOCK_STATE, &mut on) };
            (rc == KERN_SUCCESS).then_some(on)
        }

        pub fn set_caps_lock(&self, on: bool) -> bool {
            // SAFETY: open connection, valid selector.
            unsafe { IOHIDSetModifierLockState(self.connect, CAPS_LOCK_STATE, on) == KERN_SUCCESS }
        }
    }

    impl Drop for HidSystem {
        fn drop(&mut self) {
            // SAFETY: `connect` came from IOServiceOpen and is closed once.
            unsafe { IOServiceClose(self.connect) };
        }
    }
}

/// Media/volume keys aren't keyboard virtual keys on macOS: hardware posts
/// them as NSSystemDefined events (subtype NX_SUBTYPE_AUX_CONTROL_BUTTONS)
/// whose data1 packs the NX_KEYTYPE and the up/down state. Built through
/// NSEvent (the only public constructor) and posted as its CGEvent.
mod media_key {
    use std::ffi::c_void;

    use anyhow::anyhow;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;

    /// NX_SUBTYPE_AUX_CONTROL_BUTTONS (IOKit/hidsystem/IOLLEvent.h).
    const AUX_CONTROL_BUTTONS: i16 = 8;
    /// kCGHIDEventTap.
    const HID_EVENT_TAP: u32 = 0;

    // SAFETY contract: CGEventPost takes a tap location and a valid CGEventRef
    // that it does not consume.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventPost(tap: u32, event: *const c_void);
    }

    /// NX_KEYDOWN (0xA) / NX_KEYUP (0xB) in bits 8-15 — the same value goes in
    /// the event's modifier flags — and the key type in bits 16-23.
    pub fn state_bits(down: bool) -> usize {
        if down { 0xA00 } else { 0xB00 }
    }

    pub fn data1(key_type: u8, down: bool) -> isize {
        (isize::from(key_type) << 16) | state_bits(down) as isize
    }

    pub fn post(key_type: u8, down: bool) -> anyhow::Result<()> {
        // The injector thread has no ambient pool; NSEvent's convenience
        // constructor returns an autoreleased object.
        autoreleasepool(|_| {
            let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::SystemDefined,
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags(state_bits(down)),
                0.0,
                0,
                None,
                AUX_CONTROL_BUTTONS,
                data1(key_type, down),
                -1,
            )
            .ok_or_else(|| anyhow!("failed to create media key event"))?;
            let cg = event
                .CGEvent()
                .ok_or_else(|| anyhow!("media key event has no CGEvent"))?;
            // SAFETY: `cg` is a live CGEventRef for the duration of the call.
            unsafe { CGEventPost(HID_EVENT_TAP, Retained::as_ptr(&cg).cast()) };
            Ok(())
        })
    }
}

/// Keys the controller currently holds down: keyboard virtual keys (every
/// kVK_* is < 128) and media keys (NX_KEYTYPE_* < 32). The source of truth for
/// modifier flags and for what a disconnect must release.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HeldKeys {
    keys: u128,
    media: u32,
}

impl HeldKeys {
    fn set(&mut self, vk: u16, down: bool) {
        if let Some(bit) = 1u128.checked_shl(u32::from(vk)) {
            if down {
                self.keys |= bit;
            } else {
                self.keys &= !bit;
            }
        }
    }

    fn set_media(&mut self, key_type: u8, down: bool) {
        if let Some(bit) = 1u32.checked_shl(u32::from(key_type)) {
            if down {
                self.media |= bit;
            } else {
                self.media &= !bit;
            }
        }
    }

    fn held(&self) -> impl Iterator<Item = u16> + '_ {
        (0..128u16).filter(|&vk| self.keys & (1u128 << vk) != 0)
    }

    /// Flags for the modifier keys held (left and right Shift are the same
    /// flag, so releasing one keeps it while the other is down).
    fn modifier_flags(&self) -> CGEventFlags {
        self.held()
            .filter_map(modifier_flag_for_vk)
            .fold(CGEventFlags::CGEventFlagNull, |acc, f| acc | f)
    }

    /// Held virtual keys in release order: ordinary keys first, modifiers
    /// last, so each key-up still carries the modifiers held with it — the
    /// order a person lifting their fingers would produce.
    fn release_order(&self) -> Vec<u16> {
        let (mods, keys): (Vec<u16>, Vec<u16>) = self
            .held()
            .partition(|&vk| modifier_flag_for_vk(vk).is_some());
        keys.into_iter().chain(mods).collect()
    }

    fn take_media(&mut self) -> Vec<u8> {
        let held = (0..32u8)
            .filter(|&k| self.media & (1u32 << k) != 0)
            .collect();
        self.media = 0;
        held
    }
}

/// Caps Lock for a sender that doesn't report its lock state: a `CapsLock`
/// key event *without* `CAPS_LOCK_KNOWN` (controller builds from before the
/// lock bits — e.g. a cached PWA). Its raw key can't be posted (see
/// [`keymap::dom_code_to_vk`]), so the event toggles the HID lock instead.
///
/// The catch is that browsers disagree on what one press sends: Windows,
/// Linux and ChromeOS send keydown + keyup (plus auto-repeat keydowns while
/// held); macOS browsers send *only* a keydown when the lock turns on and
/// *only* a keyup when it turns off. The sender's OS isn't on the wire, so:
///
/// - a keydown toggles, unless one is already outstanding (auto-repeat);
/// - a keyup toggles, unless it closes an outstanding keydown that was
///   auto-repeated or is under [`LEGACY_CAPS_PRESS`] old — that's the
///   release half of a press whose keydown already toggled.
///
/// So a press toggles exactly once from either kind of sender, except a Mac
/// user turning the lock on *and* off again within 500 ms, or a non-Mac user
/// holding Caps Lock past 500 ms without auto-repeat starting; either leaves
/// host and controller inverted until the next press. Senders that set
/// `CAPS_LOCK_KNOWN` never get here (their lock state is synced instead).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LegacyCapsKey {
    /// When the outstanding (not yet released) keydown arrived.
    down_at: Option<Instant>,
    /// It has auto-repeated: a held key, so its keyup is a release.
    repeated: bool,
}

impl LegacyCapsKey {
    /// Whether a `CapsLock` key event with this modifiers byte toggles the
    /// lock: never when it carries the lock state (`CAPS_LOCK_KNOWN` — it was
    /// synced instead, and the legacy state is reset); otherwise
    /// [`Self::toggles`].
    fn on_caps_key(&mut self, mask: u8, down: bool, now: Instant) -> bool {
        if wanted_caps_lock(mask).is_some() {
            *self = LegacyCapsKey::default();
            return false;
        }
        self.toggles(down, now)
    }

    /// Whether this legacy `CapsLock` key event toggles the lock.
    fn toggles(&mut self, down: bool, now: Instant) -> bool {
        if down {
            if self.down_at.is_some() {
                self.repeated = true;
                return false;
            }
            self.down_at = Some(now);
            self.repeated = false;
            return true;
        }
        // A keyup closing a quick or auto-repeated keydown is that press's
        // release; any other keyup (none outstanding, or a long-past one) is
        // a press of its own.
        let repeated = std::mem::take(&mut self.repeated);
        let release = self
            .down_at
            .take()
            .is_some_and(|at| repeated || now.saturating_duration_since(at) < LEGACY_CAPS_PRESS);
        !release
    }
}

/// A connection opened lazily and, while it can't be, retried at most every
/// `retry` — so a failure at startup isn't permanent for the daemon's life.
struct Reopen<T> {
    conn: Option<T>,
    last_try: Option<Instant>,
    retry: Duration,
}

impl<T> Reopen<T> {
    fn new(retry: Duration) -> Self {
        Reopen {
            conn: None,
            last_try: None,
            retry,
        }
    }

    /// The connection, trying `open` first if there is none and the last try
    /// was at least `retry` ago. Returns whether this call tried, too.
    fn get(&mut self, now: Instant, open: impl FnOnce() -> Option<T>) -> (Option<&T>, bool) {
        let due = self.conn.is_none()
            && self
                .last_try
                .is_none_or(|t| now.saturating_duration_since(t) >= self.retry);
        if due {
            self.last_try = Some(now);
            self.conn = open();
        }
        (self.conn.as_ref(), due)
    }
}

/// The active display's bounds (in points), re-queried on display switches,
/// on demand, and otherwise at most every [`BOUNDS_TTL`] — so a resolution or
/// scaling change on the same display doesn't leave clicks misplaced.
struct ActiveDisplay {
    id: u32,
    bounds: CGRect,
    queried: Instant,
}

impl ActiveDisplay {
    fn new(id: u32) -> Self {
        ActiveDisplay {
            id,
            bounds: CGDisplay::new(id).bounds(),
            queried: Instant::now(),
        }
    }

    fn bounds(&mut self, force: bool) -> CGRect {
        self.bounds_with(force, Instant::now(), |id| CGDisplay::new(id).bounds())
    }

    fn bounds_with(
        &mut self,
        force: bool,
        now: Instant,
        query: impl FnOnce(u32) -> CGRect,
    ) -> CGRect {
        if force || now.saturating_duration_since(self.queried) >= BOUNDS_TTL {
            self.bounds = query(self.id);
            self.queried = now;
        }
        self.bounds
    }

    /// Switch displays; returns whether the id changed (bounds then need a
    /// forced re-query).
    fn switch_to(&mut self, id: u32) -> bool {
        let changed = id != self.id;
        self.id = id;
        changed
    }
}

pub struct MacInjector {
    source: CGEventSource,
    display: ActiveDisplay, // normalized coords map onto this display
    pos: CGPoint,
    held: [bool; 3], // mouse buttons: left, middle, right
    keys: HeldKeys,
    /// Last known (or, if setting it failed, last wanted) Caps Lock state,
    /// stamped on key events as AlphaShift.
    caps_lock: bool,
    /// IOHIDSystem, retried every [`HID_RETRY`] while it can't be opened;
    /// until then Caps Lock only affects the AlphaShift flag on injected keys.
    hid: Reopen<hid_system::HidSystem>,
    /// Warned that IOHIDSystem can't be opened / the lock can't be set (once
    /// each, until it works again).
    hid_open_warned: bool,
    caps_set_warned: bool,
    legacy_caps: LegacyCapsKey,
    last_click: Option<(Instant, CGPoint, MouseButton)>,
    click_state: i64,
    activity: user_activity::UserActivity,
    last_activity: Option<Instant>,
}

impl MacInjector {
    pub fn new() -> anyhow::Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow!("failed to create CGEventSource"))?;
        let display = ActiveDisplay::new(CGDisplay::main().id);
        let bounds = display.bounds;
        let mut injector = MacInjector {
            source,
            display,
            pos: CGPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            ),
            held: [false; 3],
            keys: HeldKeys::default(),
            caps_lock: false,
            hid: Reopen::new(HID_RETRY),
            hid_open_warned: false,
            caps_set_warned: false,
            legacy_caps: LegacyCapsKey::default(),
            last_click: None,
            click_state: 1,
            activity: user_activity::UserActivity::new(),
            last_activity: None,
        };
        injector.caps_lock = injector.hid_caps_lock().unwrap_or(false);
        Ok(injector)
    }

    /// Declare user activity (wakes a sleeping display), at most every few
    /// seconds while input flows.
    fn declare_activity(&mut self) {
        let due = self
            .last_activity
            .is_none_or(|at| at.elapsed() > Duration::from_secs(5));
        if due {
            self.activity.declare();
            self.last_activity = Some(Instant::now());
        }
    }

    fn move_to(&mut self, x: u16, y: u16, force_bounds: bool) {
        let bounds = self.display.bounds(force_bounds);
        self.pos = normalized_to_point(&bounds, x, y);
    }

    fn post_mouse(&self, event_type: CGEventType, button: CGMouseButton) -> anyhow::Result<()> {
        let event = CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, button)
            .map_err(|()| anyhow!("failed to create mouse event"))?;
        event.set_flags(self.keys.modifier_flags());
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn post_key(&self, vk: u16, down: bool, mask: u8) -> anyhow::Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), vk, down)
            .map_err(|()| anyhow!("failed to create key event"))?;
        event.set_flags(key_event_flags(
            vk,
            self.keys.modifier_flags(),
            mask,
            self.caps_lock,
        ));
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn key(&mut self, code: &str, mask: u8, down: bool) -> anyhow::Result<()> {
        // Every key event may carry the controller's lock state — including
        // the CapsLock key itself, which is never posted (that would
        // double-toggle): with the lock bits it has just been synced; without
        // them it is a legacy sender's press, toggled here.
        self.sync_caps_lock(mask);
        if code == "CapsLock" {
            if self.legacy_caps.on_caps_key(mask, down, Instant::now()) {
                self.toggle_caps_lock();
            }
            return Ok(());
        }
        if let Some(key_type) = keymap::dom_code_to_media_key(code) {
            self.keys.set_media(key_type, down);
            return media_key::post(key_type, down);
        }
        let Some(vk) = keymap::dom_code_to_vk(code) else {
            debug!(code, "no macOS virtual key for DOM code, dropping");
            return Ok(());
        };
        self.keys.set(vk, down);
        self.post_key(vk, down, mask)
    }

    /// The IOHIDSystem connection, opening it first if it isn't open and a
    /// retry is due ([`HID_RETRY`]). Warns once while it can't be opened.
    fn hid(&mut self) -> Option<&hid_system::HidSystem> {
        let (hid, tried) = self.hid.get(Instant::now(), hid_system::HidSystem::open);
        if tried {
            match hid {
                None if !self.hid_open_warned => {
                    warn!(
                        "could not open IOHIDSystem; Caps Lock state will not sync \
                         (retrying every {} s)",
                        HID_RETRY.as_secs()
                    );
                    self.hid_open_warned = true;
                }
                Some(_) if self.hid_open_warned => {
                    info!("opened IOHIDSystem; Caps Lock syncs again");
                    self.hid_open_warned = false;
                }
                _ => {}
            }
        }
        hid
    }

    /// The HID Caps Lock state, if it can be read.
    fn hid_caps_lock(&mut self) -> Option<bool> {
        self.hid().and_then(|h| h.caps_lock())
    }

    /// Set the HID lock to `want` and stamp `want` on injected keys — even if
    /// setting it failed: AlphaShift then still gives the controller the
    /// letter case it asked for. Warns once per run of failures.
    fn set_caps_lock(&mut self, want: bool) {
        self.caps_lock = want;
        let Some(ok) = self.hid().map(|h| h.set_caps_lock(want)) else {
            return; // no connection (already warned): AlphaShift only
        };
        if ok {
            self.caps_set_warned = false;
        } else if !self.caps_set_warned {
            warn!(
                want,
                "IOHIDSetModifierLockState failed; Caps Lock only affects injected keys"
            );
            self.caps_set_warned = true;
        }
    }

    /// Bring the host's Caps Lock state in line with the controller's, when
    /// the event says what that is. Reads the HID state each time so a local
    /// toggle on the host is respected for events that don't carry it.
    fn sync_caps_lock(&mut self, mask: u8) {
        let actual = self.hid_caps_lock();
        match wanted_caps_lock(mask) {
            Some(want) if actual != Some(want) => self.set_caps_lock(want),
            _ => {
                if let Some(on) = actual {
                    self.caps_lock = on;
                }
            }
        }
    }

    /// Flip Caps Lock (a legacy sender's `CapsLock` press).
    fn toggle_caps_lock(&mut self) {
        let now = self.hid_caps_lock().unwrap_or(self.caps_lock);
        self.set_caps_lock(!now);
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
                self.move_to(x, y, false);
                let (event_type, button) = self.move_event_type();
                self.post_mouse(event_type, button)?;
            }
            InputEvent::MouseDown { button, x, y } => {
                // Clicks must land exactly: always use fresh bounds.
                self.move_to(x, y, true);
                self.update_click_state(button);
                self.held[button_index(button)] = true;
                let (event_type, cg_button) = button_down_type(button);
                let event =
                    CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, cg_button)
                        .map_err(|()| anyhow!("failed to create mouse event"))?;
                event
                    .set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_state);
                event.set_flags(self.keys.modifier_flags());
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::MouseUp { button, x, y } => {
                self.move_to(x, y, false);
                self.held[button_index(button)] = false;
                let (event_type, cg_button) = button_up_type(button);
                let event =
                    CGEvent::new_mouse_event(self.source.clone(), event_type, self.pos, cg_button)
                        .map_err(|()| anyhow!("failed to create mouse event"))?;
                event
                    .set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_state);
                event.set_flags(self.keys.modifier_flags());
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
                event.set_flags(self.keys.modifier_flags());
                event.post(CGEventTapLocation::HID);
            }
            InputEvent::KeyDown {
                ref code,
                modifiers: mask,
            } => self.key(code, mask, true)?,
            InputEvent::KeyUp {
                ref code,
                modifiers: mask,
            } => self.key(code, mask, false)?,
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

    /// Release every held mouse button, key and media key, and drop the
    /// user-activity assertion. Called when a controller session ends so a
    /// mid-drag or mid-combo disconnect can't strand a button or key down.
    /// Caps Lock is a lock, not a held key, so it is left as it is.
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
        for vk in self.keys.release_order() {
            self.keys.set(vk, false);
            if let Err(e) = self.post_key(vk, false, 0) {
                debug!(vk, error = %e, "key release failed");
            }
        }
        for key_type in self.keys.take_media() {
            if let Err(e) = media_key::post(key_type, false) {
                debug!(key_type, error = %e, "media key release failed");
            }
        }
        self.activity.release();
        self.last_activity = None;
        self.legacy_caps = LegacyCapsKey::default();
    }

    /// Point subsequent normalized coordinates at `display_id` (multi-monitor
    /// switch). Normalized 0..65535 then spans that display's bounds. Called
    /// before every command; bounds for an unchanged display are refreshed on
    /// the [`BOUNDS_TTL`] schedule by the mouse events themselves.
    fn set_active_display(&mut self, display_id: u32) {
        if self.display.switch_to(display_id) {
            self.display.bounds(true);
        }
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

/// Flags real keyboards stamp on a key because of *which key it is*, whatever
/// modifiers are held: arrows and the keypad are numeric-pad keys, and arrows,
/// F-keys and the Help/Home/End/PageUp/PageDown/forward-delete cluster are
/// "function" keys (NSEventModifierFlagFunction). Apps and the text system
/// look at these, so synthetic keys must carry them too.
fn key_class_flags(vk: u16) -> CGEventFlags {
    let function = CGEventFlags::CGEventFlagSecondaryFn;
    let keypad = CGEventFlags::CGEventFlagNumericPad;
    match vk {
        0x7B..=0x7E => function | keypad, // arrows
        // Help/Insert, Home, PageUp, forward delete, F4, End, F2, PageDown, F1
        0x72..=0x7A
        // F3, F5-F20
        | 0x63 | 0x60 | 0x61 | 0x62 | 0x64 | 0x65 | 0x6D | 0x67 | 0x6F
        | 0x69 | 0x6B | 0x71 | 0x6A | 0x40 | 0x4F | 0x50 | 0x5A => function,
        // keypad: . * + clear / enter - = 0-7 8 9 ,(JIS)
        0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x59 | 0x5B | 0x5C | 0x5F => keypad,
        _ => CGEventFlags::CGEventFlagNull,
    }
}

/// Full flag set for an injected key event: held modifiers, OR'd with the
/// browser-reported mask (belt and braces, in case a modifier down was lost),
/// the key's own class flags, and AlphaShift while Caps Lock is on.
fn key_event_flags(vk: u16, held: CGEventFlags, mask: u8, caps_lock: bool) -> CGEventFlags {
    let mut flags = held | mask_to_flags(mask) | key_class_flags(vk);
    if caps_lock {
        flags.insert(CGEventFlags::CGEventFlagAlphaShift);
    }
    flags
}

/// The controller's Caps Lock state, if this event carries it.
fn wanted_caps_lock(mask: u8) -> Option<bool> {
    (mask & modifiers::CAPS_LOCK_KNOWN != 0).then_some(mask & modifiers::CAPS_LOCK != 0)
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

    fn vk(code: &str) -> u16 {
        keymap::dom_code_to_vk(code).unwrap()
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
        // Lock-state and reserved bits never leak into held-modifier flags.
        assert_eq!(
            mask_to_flags(modifiers::CAPS_LOCK | modifiers::CAPS_LOCK_KNOWN | 0xC0),
            CGEventFlags::CGEventFlagNull
        );
    }

    #[test]
    fn modifier_keys_map_to_their_flags() {
        assert_eq!(
            modifier_flag_for_vk(vk("MetaLeft")),
            Some(CGEventFlags::CGEventFlagCommand)
        );
        assert_eq!(
            modifier_flag_for_vk(vk("ShiftRight")),
            Some(CGEventFlags::CGEventFlagShift)
        );
        assert_eq!(modifier_flag_for_vk(vk("KeyA")), None);
    }

    #[test]
    fn held_keys_track_presses_and_releases() {
        let mut held = HeldKeys::default();
        held.set(vk("KeyA"), true);
        held.set(vk("KeyA"), true); // auto-repeat keydown: idempotent
        held.set(vk("F19"), true);
        assert_eq!(held.held().collect::<Vec<_>>(), vec![0x00, 0x50]);
        held.set(vk("KeyA"), false);
        assert_eq!(held.held().collect::<Vec<_>>(), vec![0x50]);
        held.set(vk("KeyZ"), false); // release of a key never pressed
        assert_eq!(held.held().collect::<Vec<_>>(), vec![0x50]);
        held.set(0x7F, true); // highest representable kVK
        held.set(0x80, true); // out of range: ignored, not a panic
        held.set(u16::MAX, true);
        assert_eq!(held.held().collect::<Vec<_>>(), vec![0x50, 0x7F]);
    }

    #[test]
    fn held_modifier_flags_follow_both_sides() {
        let mut held = HeldKeys::default();
        assert_eq!(held.modifier_flags(), CGEventFlags::CGEventFlagNull);
        held.set(vk("ShiftLeft"), true);
        held.set(vk("ShiftRight"), true);
        held.set(vk("MetaLeft"), true);
        held.set(vk("KeyC"), true);
        assert_eq!(
            held.modifier_flags(),
            CGEventFlags::CGEventFlagShift | CGEventFlags::CGEventFlagCommand
        );
        // Releasing one Shift keeps the flag while the other is down.
        held.set(vk("ShiftLeft"), false);
        assert!(
            held.modifier_flags()
                .contains(CGEventFlags::CGEventFlagShift)
        );
        held.set(vk("ShiftRight"), false);
        assert_eq!(held.modifier_flags(), CGEventFlags::CGEventFlagCommand);
    }

    #[test]
    fn release_order_lifts_ordinary_keys_before_modifiers() {
        let mut held = HeldKeys::default();
        for code in ["MetaLeft", "ShiftLeft", "KeyQ", "F19", "ControlRight"] {
            held.set(vk(code), true);
        }
        let order = held.release_order();
        assert_eq!(order.len(), 5);
        let (keys, mods) = order.split_at(2);
        assert!(keys.iter().all(|&k| modifier_flag_for_vk(k).is_none()));
        assert!(mods.iter().all(|&k| modifier_flag_for_vk(k).is_some()));
        assert!(keys.contains(&vk("KeyQ")) && keys.contains(&vk("F19")));
    }

    #[test]
    fn held_media_keys_are_taken_once() {
        let mut held = HeldKeys::default();
        held.set_media(0, true);
        held.set_media(16, true);
        held.set_media(16, false);
        held.set_media(7, true);
        held.set_media(40, true); // out of range: ignored
        assert_eq!(held.take_media(), vec![0, 7]);
        assert_eq!(held.take_media(), Vec::<u8>::new());
    }

    #[test]
    fn navigation_keys_carry_hardware_class_flags() {
        let fn_pad = CGEventFlags::CGEventFlagSecondaryFn | CGEventFlags::CGEventFlagNumericPad;
        for code in ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"] {
            assert_eq!(key_class_flags(vk(code)), fn_pad, "{code}");
        }
        let fn_only = CGEventFlags::CGEventFlagSecondaryFn;
        for n in 1..=20 {
            let code = format!("F{n}");
            assert_eq!(key_class_flags(vk(&code)), fn_only, "{code}");
        }
        for code in [
            "Home", "End", "PageUp", "PageDown", "Delete", "Help", "Insert",
        ] {
            assert_eq!(key_class_flags(vk(code)), fn_only, "{code}");
        }
        let pad = CGEventFlags::CGEventFlagNumericPad;
        for code in [
            "Numpad0",
            "Numpad5",
            "Numpad9",
            "NumpadEnter",
            "NumpadAdd",
            "NumpadDecimal",
        ] {
            assert_eq!(key_class_flags(vk(code)), pad, "{code}");
        }
        for code in [
            "KeyA",
            "Digit1",
            "Enter",
            "Backspace",
            "Space",
            "ShiftLeft",
            "Escape",
        ] {
            assert_eq!(
                key_class_flags(vk(code)),
                CGEventFlags::CGEventFlagNull,
                "{code}"
            );
        }
    }

    #[test]
    fn key_event_flags_combine_held_mask_class_and_caps() {
        // Shift+ArrowRight (extend selection): shift survives alongside the
        // class flags instead of being overwritten by them (or vice versa).
        let f = key_event_flags(vk("ArrowRight"), CGEventFlags::CGEventFlagShift, 0, false);
        assert_eq!(
            f,
            CGEventFlags::CGEventFlagShift
                | CGEventFlags::CGEventFlagSecondaryFn
                | CGEventFlags::CGEventFlagNumericPad
        );
        // Browser mask ORs in a modifier whose keydown was lost.
        let f = key_event_flags(
            vk("KeyC"),
            CGEventFlags::CGEventFlagNull,
            modifiers::META,
            false,
        );
        assert_eq!(f, CGEventFlags::CGEventFlagCommand);
        // Caps Lock on: AlphaShift, and nothing else from the lock bits.
        let f = key_event_flags(
            vk("KeyA"),
            CGEventFlags::CGEventFlagNull,
            modifiers::CAPS_LOCK | modifiers::CAPS_LOCK_KNOWN,
            true,
        );
        assert_eq!(f, CGEventFlags::CGEventFlagAlphaShift);
    }

    #[test]
    fn caps_lock_bit_is_only_authoritative_when_known() {
        use modifiers::{CAPS_LOCK, CAPS_LOCK_KNOWN, SHIFT};
        assert_eq!(wanted_caps_lock(0), None); // old controller / synthetic
        assert_eq!(wanted_caps_lock(CAPS_LOCK), None); // never trust it bare
        assert_eq!(wanted_caps_lock(CAPS_LOCK_KNOWN), Some(false));
        assert_eq!(wanted_caps_lock(CAPS_LOCK_KNOWN | CAPS_LOCK), Some(true));
        assert_eq!(wanted_caps_lock(CAPS_LOCK_KNOWN | SHIFT), Some(false));
    }

    /// Feed `(down, at_ms)` CapsLock events from a legacy sender; returns
    /// which of them toggled the lock.
    fn legacy_toggles(events: &[(bool, u64)]) -> Vec<bool> {
        let t0 = Instant::now();
        let mut caps = LegacyCapsKey::default();
        events
            .iter()
            .map(|&(down, ms)| caps.on_caps_key(0, down, t0 + Duration::from_millis(ms)))
            .collect()
    }

    #[test]
    fn legacy_caps_key_toggles_once_per_press_from_mac_and_pc_senders() {
        // Windows/Linux browser: keydown + keyup per press — one toggle each
        assert_eq!(
            legacy_toggles(&[(true, 0), (false, 120), (true, 3000), (false, 3090)]),
            [true, false, true, false]
        );
        // ...held long enough to auto-repeat: the repeats and the release
        // don't toggle, however long it was held
        assert_eq!(
            legacy_toggles(&[(true, 0), (true, 500), (true, 533), (false, 2000)]),
            [true, false, false, false]
        );
        // macOS browser: keydown turns it on, a lone keyup (much later)
        // turns it off — each one a press
        assert_eq!(
            legacy_toggles(&[(true, 0), (false, 4000), (true, 9000), (false, 9800)]),
            [true, true, true, true]
        );
        // a keyup with no keydown seen (lock was on before the session, or
        // the keydown went to another window): a Mac turning it off
        assert_eq!(legacy_toggles(&[(false, 0)]), [true]);
        // the documented ambiguity: a Mac on→off within 500 ms reads as one
        // PC press (one toggle, not two)
        assert_eq!(legacy_toggles(&[(true, 0), (false, 300)]), [true, false]);
    }

    #[test]
    fn caps_key_with_lock_state_is_synced_not_toggled() {
        use modifiers::{CAPS_LOCK, CAPS_LOCK_KNOWN};
        let t0 = Instant::now();
        let mut caps = LegacyCapsKey::default();
        // current controllers: the bits rule, the key event never toggles
        for mask in [CAPS_LOCK_KNOWN, CAPS_LOCK_KNOWN | CAPS_LOCK] {
            assert!(!caps.on_caps_key(mask, true, t0));
            assert!(!caps.on_caps_key(mask, false, t0));
        }
        // bit 4 without bit 5 is not lock state: still a legacy press
        assert!(caps.on_caps_key(CAPS_LOCK, true, t0));
        // a lock-state event resets the legacy half-press, so a later lone
        // keyup counts as its own press
        assert!(!caps.on_caps_key(CAPS_LOCK_KNOWN, true, t0));
        assert_eq!(caps, LegacyCapsKey::default());
        assert!(caps.on_caps_key(0, false, t0 + Duration::from_millis(100)));
    }

    #[test]
    fn hid_connection_is_retried_on_a_schedule_until_it_opens() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut opens = 0;
        let mut hid = Reopen::<u32>::new(HID_RETRY);
        let mut try_at = |hid: &mut Reopen<u32>, at: Duration, works: bool| {
            let (conn, tried) = hid.get(t0 + at, || {
                opens += 1;
                works.then_some(7)
            });
            (conn.copied(), tried)
        };
        // fails at startup...
        assert_eq!(try_at(&mut hid, s(0), false), (None, true));
        // ...not retried on every key event...
        assert_eq!(try_at(&mut hid, s(1), true), (None, false));
        assert_eq!(try_at(&mut hid, s(4), true), (None, false));
        // ...but every HID_RETRY, until it opens
        assert_eq!(try_at(&mut hid, s(5), false), (None, true));
        assert_eq!(try_at(&mut hid, s(10), true), (Some(7), true));
        // and then it's kept, never reopened
        assert_eq!(try_at(&mut hid, s(60), false), (Some(7), false));
        assert_eq!(opens, 3);
    }

    #[test]
    fn media_key_data1_packs_key_type_and_state() {
        assert_eq!(media_key::data1(0, true), 0x00_0A00); // SOUND_UP down
        assert_eq!(media_key::data1(0, false), 0x00_0B00);
        assert_eq!(media_key::data1(16, true), 0x10_0A00); // PLAY down
        assert_eq!(media_key::data1(18, false), 0x12_0B00); // PREVIOUS up
    }

    #[test]
    fn display_bounds_refresh_on_ttl_force_and_switch() {
        let start = Instant::now();
        let mut d = ActiveDisplay {
            id: 1,
            bounds: bounds(1728.0, 1117.0),
            queried: start,
        };
        let mut queries = Vec::new();
        let mut query = |now: Instant, force: bool, d: &mut ActiveDisplay, w: f64| {
            d.bounds_with(force, now, |id| {
                queries.push(id);
                bounds(w, 1000.0)
            })
        };
        // Within the TTL: cached (same display, e.g. a burst of mouse moves).
        let b = query(start + BOUNDS_TTL / 2, false, &mut d, 1440.0);
        assert_eq!(b.size.width, 1728.0);
        // Past the TTL: a same-display resolution change is picked up.
        let b = query(start + BOUNDS_TTL, false, &mut d, 1440.0);
        assert_eq!(b.size.width, 1440.0);
        // Forced (mouse-down): re-queried even though just refreshed.
        let b = query(start + BOUNDS_TTL, true, &mut d, 1280.0);
        assert_eq!(b.size.width, 1280.0);
        // Switching displays reports a change (caller forces a re-query).
        assert!(!d.switch_to(1));
        assert!(d.switch_to(2));
        query(start + BOUNDS_TTL, true, &mut d, 2560.0);
        assert_eq!(queries, vec![1, 1, 2]);
    }

    // ---- Real-system tests. They act on the live HID system, so they are
    // #[ignore]d; run with `cargo test -p tetherd --lib -- --ignored --nocapture
    // --test-threads=1` on a Mac with Accessibility granted. They only ever
    // press F19 (released on every path), touch Caps Lock (restored), or
    // inspect the process's own power assertions.

    /// kVK_F19: a key nothing types with.
    const F19: u16 = 0x50;

    unsafe extern "C" {
        // SAFETY contract: CGEventSourceStateID (int32) and a CGKeyCode.
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    }

    fn f19_is_down() -> bool {
        // SAFETY: plain query; 1 = kCGEventSourceStateHIDSystemState.
        unsafe { CGEventSourceKeyState(1, F19) }
    }

    /// Posts an F19 key-up on drop, so a failed assertion can't leave it held.
    struct ReleaseF19;
    impl Drop for ReleaseF19 {
        fn drop(&mut self) {
            if let Ok(src) = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                && let Ok(up) = CGEvent::new_keyboard_event(src, F19, false)
            {
                up.post(CGEventTapLocation::HID);
            }
        }
    }

    #[test]
    #[ignore = "presses and releases the real F19 key; needs Accessibility"]
    fn release_all_lifts_held_keys() {
        let _guard = ReleaseF19;
        let mut injector = MacInjector::new().unwrap();
        assert!(!f19_is_down(), "F19 already down before the test");
        injector
            .inject(&InputEvent::KeyDown {
                code: "F19".into(),
                modifiers: 0,
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let down = f19_is_down();
        injector.release_all();
        std::thread::sleep(Duration::from_millis(100));
        let after = f19_is_down();
        println!("F19 down after keydown: {down}; after release_all: {after}");
        assert!(down, "keydown didn't register (Accessibility granted?)");
        assert!(!after, "release_all left F19 held");
        assert_eq!(injector.keys, HeldKeys::default());
    }

    /// Restores the original Caps Lock state on drop.
    struct RestoreCaps(bool);
    impl Drop for RestoreCaps {
        fn drop(&mut self) {
            if let Some(hid) = hid_system::HidSystem::open() {
                hid.set_caps_lock(self.0);
            }
        }
    }

    #[test]
    #[ignore = "toggles the real Caps Lock state (restored afterwards)"]
    fn caps_lock_bit_syncs_hid_lock_state() {
        use modifiers::{CAPS_LOCK, CAPS_LOCK_KNOWN};
        let hid = hid_system::HidSystem::open().expect("open IOHIDSystem");
        let original = hid.caps_lock().expect("read caps lock state");
        let _restore = RestoreCaps(original);
        let mut injector = MacInjector::new().unwrap();
        // The lock state reads back asynchronously (a few ms after a set on
        // macOS 26): wait for it to settle, recording the slowest.
        let slowest = std::cell::Cell::new(Duration::ZERO);
        let settled = |want: bool| {
            let t = Instant::now();
            while hid.caps_lock() != Some(want) && t.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(1));
            }
            slowest.set(slowest.get().max(t.elapsed()));
            hid.caps_lock()
        };
        let caps = |on: bool| {
            if on {
                CAPS_LOCK_KNOWN | CAPS_LOCK
            } else {
                CAPS_LOCK_KNOWN
            }
        };
        // The CapsLock key itself is never posted, only synced, so these
        // events produce no keystrokes.
        for (down, want) in [(true, !original), (false, original), (true, !original)] {
            let code = "CapsLock".to_owned();
            let modifiers = caps(want);
            let ev = if down {
                InputEvent::KeyDown { code, modifiers }
            } else {
                InputEvent::KeyUp { code, modifiers }
            };
            injector.inject(&ev).unwrap();
            assert_eq!(settled(want), Some(want));
            assert_eq!(injector.caps_lock, want);
        }
        // A legacy sender (no CAPS_LOCK_KNOWN; bit 4 alone means nothing):
        // its CapsLock key events toggle — once per press, PC-style
        // (keydown + quick keyup) or Mac-style (a lone keyup to turn off).
        let mut now = !original;
        let legacy = |code_down: bool, stray_bit: bool| {
            let code = "CapsLock".to_owned();
            let modifiers = if stray_bit { CAPS_LOCK } else { 0 };
            if code_down {
                InputEvent::KeyDown { code, modifiers }
            } else {
                InputEvent::KeyUp { code, modifiers }
            }
        };
        injector.inject(&legacy(true, now)).unwrap(); // PC press: down...
        now = !now;
        assert_eq!(settled(now), Some(now));
        assert_eq!(injector.caps_lock, now);
        injector.inject(&legacy(false, !now)).unwrap(); // ...and up: no-op
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hid.caps_lock(), Some(now));
        injector.inject(&legacy(false, now)).unwrap(); // Mac off-press
        now = !now;
        assert_eq!(settled(now), Some(now));
        assert_eq!(injector.caps_lock, now);
        // Timing: the per-key-event lock-state read.
        let t = Instant::now();
        for _ in 0..1000 {
            hid.caps_lock();
        }
        println!(
            "caps lock synced both ways from original={original}; IOHIDGetModifierLockState ≈ {:?}; \
             slowest settle {:?}",
            t.elapsed() / 1000,
            slowest.get()
        );
    }

    fn osascript(script: &str) -> Option<String> {
        let out = std::process::Command::new("osascript")
            .args(["-e", script])
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn output_volume() -> Option<u32> {
        osascript("output volume of (get volume settings)")?
            .parse()
            .ok()
    }

    /// Restores the original output volume and mute state on drop (a volume
    /// key also unmutes).
    struct RestoreVolume {
        volume: u32,
        muted: String,
    }
    impl Drop for RestoreVolume {
        fn drop(&mut self) {
            osascript(&format!("set volume output volume {}", self.volume));
            if self.muted == "true" || self.muted == "false" {
                osascript(&format!("set volume output muted {}", self.muted));
            }
        }
    }

    #[test]
    #[ignore = "taps a real volume key (original volume restored afterwards)"]
    fn volume_key_changes_output_volume() {
        let before = output_volume().expect("read output volume");
        let _restore = RestoreVolume {
            volume: before,
            muted: osascript("output muted of (get volume settings)").unwrap_or_default(),
        };
        // Step away from whichever end we're at so the change is visible.
        let code = if before >= 90 {
            "AudioVolumeDown"
        } else {
            "AudioVolumeUp"
        };
        let mut injector = MacInjector::new().unwrap();
        for down in [true, false] {
            let code = code.to_owned();
            let ev = if down {
                InputEvent::KeyDown { code, modifiers: 0 }
            } else {
                InputEvent::KeyUp { code, modifiers: 0 }
            };
            injector.inject(&ev).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        let after = output_volume().expect("read output volume");
        println!("{code}: output volume {before} -> {after} (restoring {before})");
        assert_ne!(before, after, "volume key had no effect");
        assert_eq!(injector.keys, HeldKeys::default());
    }

    fn tether_assertions() -> usize {
        let out = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("run pmset");
        let pid = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&pid) && l.contains("tether remote input"))
            .count()
    }

    #[test]
    #[ignore = "declares user activity (wakes the display); reads pmset"]
    fn user_activity_assertion_is_reused_not_stacked() {
        let mut activity = user_activity::UserActivity::new();
        for _ in 0..4 {
            activity.declare();
        }
        let held = tether_assertions();
        activity.release();
        let released = tether_assertions();
        println!("tether assertions after 4 declares: {held}; after release: {released}");
        assert_eq!(held, 1);
        assert_eq!(released, 0);
    }
}
