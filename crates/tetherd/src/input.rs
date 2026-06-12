use tether_protocol::InputEvent;

/// Platform input injection. macOS implementation arrives in Module 5.
pub trait InputInjector: Send {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
}
