use clap::Parser;
use tokio::sync::{mpsc, watch};
use tether_protocol::Resolution;
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

    // Capture pipeline lands in Module 3; until then the server runs with an
    // empty frame source so the connection layer is testable end to end.
    let (_resolution_tx, resolution_rx) = watch::channel(Resolution { width: 0, height: 0 });
    let (_frames_tx, frames_rx) = watch::channel(None);
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
        ServerState { resolution: resolution_rx, frames: frames_rx, input_tx },
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
