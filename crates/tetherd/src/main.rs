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
    let (input_tx, mut input_rx) = mpsc::channel(256);

    // Injection lands in Module 5; drain and log so the channel never fills.
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            debug!(?ev, "input event (injection not yet wired)");
        }
    });

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
