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

    /// Pre-shared secret all devices must present to register. Prefer the
    /// environment variable in a deployment: a flag is visible in `ps` and in
    /// `docker inspect`.
    #[arg(long, env = "TETHER_SECRET")]
    secret: String,

    /// STUN URL(s) advertised to peers.
    #[arg(long = "stun-url", default_value = "stun:stun.l.google.com:19302")]
    stun_urls: Vec<String>,

    /// TURN/TURNS URL(s) advertised to peers (e.g. turn:relay:3478?transport=udp).
    /// Repeatable. STUN-only if omitted.
    #[arg(long = "turn-url")]
    turn_urls: Vec<String>,

    /// coturn static-auth-secret for minting ephemeral TURN credentials.
    /// Env-only (TETHER_TURN_SECRET) — never a CLI flag, which would be visible
    /// in `ps`.
    #[arg(env = "TETHER_TURN_SECRET", hide = true)]
    turn_secret: Option<String>,

    /// TURN credential lifetime in seconds (absolute expiry = now + ttl).
    #[arg(long, default_value_t = 86_400)]
    turn_ttl: u64,

    /// Where to persist `device_id -> identity key` pins. Without it the pins
    /// live in memory only, so every restart reopens the trust-on-first-use
    /// window for every device — fine for a test run, not for a deployment.
    #[arg(long)]
    identity_store: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
    let identities = match &args.identity_store {
        Some(path) => {
            let store = tether_signal::identity::IdentityStore::load(path)?;
            info!(path = %path.display(), pinned = store.len(), "identity pins loaded");
            store
        }
        None => {
            tracing::warn!(
                "no --identity-store: device identity pins are in-memory only, so a \
                 restart lets anyone with the secret re-claim any device id"
            );
            tether_signal::identity::IdentityStore::ephemeral()
        }
    };
    let state = AppState::with_ice_and_identities(args.secret, ice, identities);
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
