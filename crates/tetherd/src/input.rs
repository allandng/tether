use tether_protocol::InputEvent;

/// Platform input injection. Not `Send`: platform handles (CGEventSource)
/// are thread-affine, so injectors are constructed on their worker thread.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
}

#[cfg(target_os = "macos")]
pub mod macos;
