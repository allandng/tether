//! Host clipboard sync: poll the platform clipboard for changes (macOS has
//! no change notification — `changeCount` polling is the only mechanism) and
//! apply clipboard content arriving from controllers, suppressing the echo
//! of our own writes.

use std::time::Duration;

use tether_protocol::MAX_CLIPBOARD_LEN;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Platform clipboard. Implementations are constructed on the sync thread
/// (platform handles are thread-affine, like the injector's).
pub trait Clipboard {
    fn change_count(&mut self) -> i64;
    fn read_text(&mut self) -> Option<String>;
    /// Write and return the change count that write produced.
    fn write_text(&mut self, text: &str) -> i64;
}

/// Handles the session layer uses.
pub struct ClipboardSync {
    /// Latest host clipboard text bound for controllers.
    pub outbound: watch::Receiver<Option<String>>,
    /// Clipboard content arriving from controllers.
    pub inbound_tx: std::sync::mpsc::Sender<String>,
}

/// How often the host clipboard is polled; also the host→controller latency
/// floor.
const POLL_INTERVAL: Duration = Duration::from_millis(600);

pub fn start<C, F>(make_clipboard: F) -> std::io::Result<ClipboardSync>
where
    C: Clipboard,
    F: FnOnce() -> C + Send + 'static,
{
    let (out_tx, out_rx) = watch::channel(None::<String>);
    let (in_tx, in_rx) = std::sync::mpsc::channel::<String>();
    // Callers may touch the clipboard immediately after start(); make sure
    // the baseline changeCount is snapshotted before we return.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);

    std::thread::Builder::new()
        .name("tether-clipboard".into())
        .spawn(move || {
            let mut clipboard = make_clipboard();
            let mut last_count = clipboard.change_count();
            let _ = ready_tx.send(());
            // Suppress re-broadcasting content we just applied or published.
            let mut last_applied: Option<String> = None;
            let mut last_published: Option<String> = None;

            loop {
                // recv_timeout doubles as the poll tick.
                match in_rx.recv_timeout(POLL_INTERVAL) {
                    Ok(text) => {
                        if Some(&text) == last_applied.as_ref() {
                            continue; // duplicate from the controller; nothing to do
                        }
                        info!(bytes = text.len(), "applying controller clipboard");
                        last_count = clipboard.write_text(&text);
                        last_applied = Some(text);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let count = clipboard.change_count();
                        if count == last_count {
                            continue;
                        }
                        last_count = count;
                        let Some(text) = clipboard.read_text() else {
                            debug!("clipboard changed to non-text content, ignoring");
                            continue;
                        };
                        if text.len() > MAX_CLIPBOARD_LEN {
                            warn!(
                                bytes = text.len(),
                                "clipboard exceeds {MAX_CLIPBOARD_LEN} bytes, not syncing"
                            );
                            continue;
                        }
                        if Some(&text) == last_applied.as_ref()
                            || Some(&text) == last_published.as_ref()
                        {
                            continue; // echo of our own write, or unchanged content
                        }
                        info!(bytes = text.len(), "host clipboard changed, publishing");
                        last_published = Some(text.clone());
                        if out_tx.send(Some(text)).is_err() {
                            return; // daemon shutting down
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        })?;

    let _ = ready_rx.recv_timeout(Duration::from_secs(2));
    Ok(ClipboardSync {
        outbound: out_rx,
        inbound_tx: in_tx,
    })
}

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeBoard {
        state: Arc<Mutex<(i64, Option<String>)>>, // (change_count, content)
    }

    impl FakeBoard {
        fn external_copy(&self, text: &str) {
            let mut s = self.state.lock().unwrap();
            s.0 += 1;
            s.1 = Some(text.into());
        }
    }

    impl Clipboard for FakeBoard {
        fn change_count(&mut self) -> i64 {
            self.state.lock().unwrap().0
        }
        fn read_text(&mut self) -> Option<String> {
            self.state.lock().unwrap().1.clone()
        }
        fn write_text(&mut self, text: &str) -> i64 {
            let mut s = self.state.lock().unwrap();
            s.0 += 1;
            s.1 = Some(text.into());
            s.0
        }
    }

    async fn next_outbound(rx: &mut watch::Receiver<Option<String>>) -> Option<String> {
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("no clipboard publish within deadline")
            .unwrap();
        rx.borrow_and_update().clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn external_change_is_published_once() {
        let board = FakeBoard::default();
        let sync = start({
            let board = board.clone();
            move || board
        })
        .unwrap();
        let mut rx = sync.outbound.clone();

        board.external_copy("hello from host");
        assert_eq!(
            next_outbound(&mut rx).await.as_deref(),
            Some("hello from host")
        );

        // no churn: nothing further arrives while the clipboard is idle
        let quiet = tokio::time::timeout(Duration::from_millis(1500), rx.changed()).await;
        assert!(quiet.is_err(), "idle clipboard must not republish");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_write_does_not_echo() {
        let board = FakeBoard::default();
        let sync = start({
            let board = board.clone();
            move || board
        })
        .unwrap();
        let mut rx = sync.outbound.clone();

        sync.inbound_tx.send("from controller".into()).unwrap();
        // give the writer + at least two poll ticks a chance to misbehave
        let echo = tokio::time::timeout(Duration::from_millis(2000), rx.changed()).await;
        assert!(echo.is_err(), "own write must not be published back");
        assert_eq!(
            board.state.lock().unwrap().1.as_deref(),
            Some("from controller")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_clipboard_is_not_synced() {
        let board = FakeBoard::default();
        let sync = start({
            let board = board.clone();
            move || board
        })
        .unwrap();
        let mut rx = sync.outbound.clone();

        board.external_copy(&"x".repeat(MAX_CLIPBOARD_LEN + 1));
        let published = tokio::time::timeout(Duration::from_millis(1500), rx.changed()).await;
        assert!(published.is_err(), "oversized content must be refused");

        // and the path still works for normal content afterwards
        board.external_copy("small again");
        assert_eq!(next_outbound(&mut rx).await.as_deref(), Some("small again"));
    }
}
