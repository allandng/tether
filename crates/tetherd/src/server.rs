use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tether_protocol::Resolution;
use tracing::{info, warn};

use crate::auth::PairingAuth;
use crate::capture::EncodedFrame;
use crate::config::{ip_allowed, validate_bind_addr};
use crate::input::InjectCommand;
use crate::session;

/// Device-pairing enforcement policy (Phase 5). The gate activates once any
/// device is paired or `require_pairing` is set, so a host that has admitted a
/// device can't also be reached over the old unauthenticated path.
/// `allow_unpaired` is an explicit dev/LAN escape that keeps the gate off.
#[derive(Clone, Copy, Debug)]
pub struct AuthPolicy {
    pub require_pairing: bool,
    pub allow_unpaired: bool,
}

/// Everything a session needs, decoupled from where it comes from so tests
/// can drive sessions with fakes and Module 3/5 can plug in real pipelines.
#[derive(Clone)]
pub struct ServerState {
    /// Current capture resolution; re-broadcast to the controller on change.
    pub resolution: watch::Receiver<Resolution>,
    /// Latest encoded frame, latest-wins. `None` until capture produces one.
    pub frames: watch::Receiver<Option<EncodedFrame>>,
    /// Input events and soft-keyboard text bound for the platform injector,
    /// on one ordered channel so they interleave correctly.
    pub input_tx: mpsc::Sender<InjectCommand>,
    /// Latest host clipboard text; `None` until the first copy.
    pub clipboard_out: watch::Receiver<Option<String>>,
    /// Clipboard content from the controller, bound for the host pasteboard.
    pub clipboard_in: std::sync::mpsc::Sender<String>,
    /// Shared device-pairing state (host key, allowlist, active code).
    pub auth: Arc<tokio::sync::Mutex<PairingAuth>>,
    pub auth_policy: AuthPolicy,
}

pub struct Server {
    listener: TcpListener,
    allow: Vec<IpAddr>,
    state: ServerState,
}

impl Server {
    pub async fn bind(
        bind: IpAddr,
        port: u16,
        allow: Vec<IpAddr>,
        state: ServerState,
    ) -> anyhow::Result<Self> {
        validate_bind_addr(bind).map_err(anyhow::Error::msg)?;
        let listener = TcpListener::bind(SocketAddr::new(bind, port))
            .await
            .with_context(|| format!("failed to bind {bind}:{port}"))?;
        info!(addr = %listener.local_addr()?, "listening");
        Ok(Server { listener, allow, state })
    }

    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept loop. One controller session at a time; extra connections are
    /// dropped immediately. Returns only on listener error.
    pub async fn run(self) -> anyhow::Result<()> {
        let session_active = Arc::new(AtomicBool::new(false));
        loop {
            let (stream, peer) = self.listener.accept().await.context("accept failed")?;
            if !ip_allowed(&self.allow, peer.ip()) {
                warn!(%peer, "rejected: not in --allow list");
                continue; // drop the socket before any protocol bytes
            }
            if session_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                warn!(%peer, "rejected: a controller session is already active");
                continue;
            }
            let state = self.state.clone();
            let active = Arc::clone(&session_active);
            tokio::spawn(async move {
                if let Err(e) = session::run(stream, peer, state).await {
                    warn!(%peer, error = %e, "session ended with error");
                }
                active.store(false, Ordering::Release);
            });
        }
    }
}
