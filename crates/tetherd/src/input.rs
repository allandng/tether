use tether_protocol::InputEvent;

/// One unit of work for the platform injector. Raw input events and committed
/// soft-keyboard text share a single ordered channel so a "type then click"
/// sequence stays in order on the host.
#[derive(Debug, Clone)]
pub enum InjectCommand {
    Event(InputEvent),
    /// Committed Unicode text from a soft keyboard, injected directly
    /// (bypasses the DOM-code → virtual-key path; handles emoji / any layout).
    Text(String),
    /// A controller session ended — release anything it may have held (mouse
    /// buttons, modifiers) so a mid-drag disconnect can't strand a button down.
    ReleaseAll,
}

/// Platform input injection. Not `Send`: platform handles (CGEventSource)
/// are thread-affine, so injectors are constructed on their worker thread.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
    fn inject_text(&mut self, text: &str) -> anyhow::Result<()>;
    /// Release all currently-held buttons/modifiers (on session end).
    fn release_all(&mut self) {}
    /// Map subsequent input onto this platform display id (multi-monitor).
    fn set_active_display(&mut self, _display_id: u32) {}
}

#[cfg(target_os = "macos")]
pub mod macos;
