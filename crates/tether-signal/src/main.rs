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

    /// STUN URL(s) advertised to peers.
    #[arg(long = "stun-url", default_value = "stun:stun.l.google.com:19302")]
    stun_urls: Vec<String>,

    /// TURN/TURNS URL(s) advertised to peers (e.g. turn:relay:3478?transport=udp).
    /// Repeatable. STUN-only if omitted.
    #[arg(long = "turn-url")]
    turn_urls: Vec<String>,

    /// coturn static-auth-secret for minting ephemeral TURN credentials. Prefer
    /// TETHER_TURN_SECRET (env) over the flag, which is visible in `ps`.
    #[arg(long, env = "TETHER_TURN_SECRET")]
    turn_secret: Option<String>,

    /// TURN credential lifetime in seconds (absolute expiry = now + ttl).
    #[arg(long, default_value_t = 86_400)]
    turn_ttl: u64,
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
    let ice = tether_signal::turn::IceConfig {
        stun_urls: args.stun_urls,
        turn_urls: args.turn_urls,
        turn_secret: args.turn_secret,
        turn_ttl: args.turn_ttl,
    };
    if ice.turn_secret.is_some() && !ice.turn_urls.is_empty() {
        info!(turn_urls = ?ice.turn_urls, "minting ephemeral TURN credentials");
    }
    let state = AppState::with_ice(args.secret, ice);
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
