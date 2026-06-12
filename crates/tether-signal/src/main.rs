use std::net::IpAddr;

use clap::Parser;
use tether_signal::server::{self, AppState};
use tracing::info;

/// Tether signaling server: introduces controllers to hosts and relays
/// SDP/ICE. Carries no media.
#[derive(Parser, Debug)]
#[command(name = "tether-signal", version)]
struct Args {
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// TCP port to listen on.
    #[arg(long, default_value_t = 7879)]
    port: u16,

    /// Pre-shared secret all devices must present to register.
    #[arg(long)]
    secret: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let state = AppState::new(args.secret);
    let listener = tokio::net::TcpListener::bind((args.bind, args.port)).await?;
    info!(addr = %listener.local_addr()?, "signal server listening");

    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}
