use clap::Parser;
use tokio::sync::mpsc;
use tetherd::config::Args;
use tetherd::server::{Server, ServerState};
use tracing::{debug, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let pipeline = start_capture()?;
    let (input_tx, input_rx) = mpsc::channel(256);
    start_injector(input_rx);

    let server = Server::bind(
        args.bind,
        args.port,
        args.allow.clone(),
        ServerState {
            resolution: pipeline.resolution,
            frames: pipeline.frames,
            input_tx,
        },
    )
    .await?;

    tokio::select! {
        result = server.run() => result,
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn start_capture() -> anyhow::Result<tetherd::pipeline::Pipeline> {
    tetherd::pipeline::start(
        || tetherd::capture::macos::SckCapturer::main_display(30),
        || tetherd::encode::JpegEncoder::new(75),
    )
}

#[cfg(not(target_os = "macos"))]
fn start_capture() -> anyhow::Result<tetherd::pipeline::Pipeline> {
    anyhow::bail!("no screen capture implementation for this platform yet (Phase 1 hosts macOS only)")
}

/// Drain input events onto a dedicated thread; the injector is built there
/// because CGEventSource is thread-affine. Requires Accessibility permission
/// (System Settings → Privacy & Security → Accessibility) — without it macOS
/// silently discards posted events.
#[cfg(target_os = "macos")]
fn start_injector(mut input_rx: mpsc::Receiver<tether_protocol::InputEvent>) {
    use tetherd::input::InputInjector;
    std::thread::spawn(move || {
        let mut injector = match tetherd::input::macos::MacInjector::new() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "input injection unavailable");
                return;
            }
        };
        while let Some(ev) = input_rx.blocking_recv() {
            debug!(?ev, "injecting");
            if let Err(e) = injector.inject(&ev) {
                tracing::warn!(error = %e, "inject failed");
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn start_injector(mut input_rx: mpsc::Receiver<tether_protocol::InputEvent>) {
    tokio::spawn(async move { while input_rx.recv().await.is_some() {} });
}
