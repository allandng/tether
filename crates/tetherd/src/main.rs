use clap::Parser;
use tokio::sync::mpsc;
use tetherd::config::Args;
use tetherd::input::InjectCommand;
use tetherd::server::{Server, ServerState};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    args.validate().map_err(anyhow::Error::msg)?;

    let pipeline = start_capture(args.codec, args.bitrate_kbps)?;
    let (input_tx, input_rx) = mpsc::channel(256);
    start_injector(input_rx);
    let clipboard = start_clipboard()?;

    // Device pairing state (host key + allowlist), shared across sessions.
    let auth_dir = tetherd::auth::PairingAuth::default_dir()?;
    let mut pairing = tetherd::auth::PairingAuth::load_or_create(&auth_dir)?;
    if args.pair {
        let code = pairing.arm(tetherd::auth::now_unix());
        info!("pairing armed (valid 5 min) — enter this code on the controller:");
        println!("\n    Pairing code:  {}\n", tetherd::auth::group_code(&code));
    }
    if args.allow_unpaired {
        tracing::warn!("--allow-unpaired: device pairing is NOT enforced (dev/LAN only)");
    }
    if !pairing.is_empty() {
        info!(devices = pairing.paired_devices().len(), "paired devices loaded; auth required");
    }
    let auth = std::sync::Arc::new(tokio::sync::Mutex::new(pairing));

    let state = ServerState {
        resolution: pipeline.resolution,
        frames: pipeline.frames,
        input_tx,
        clipboard_out: clipboard.outbound,
        clipboard_in: clipboard.inbound_tx,
        auth,
        auth_policy: tetherd::server::AuthPolicy {
            require_pairing: args.require_pairing,
            allow_unpaired: args.allow_unpaired,
        },
        bitrate: pipeline.bitrate,
        bitrate_ceiling_kbps: args.bitrate_kbps,
    };

    let lan = match args.bind {
        Some(bind) => {
            let server = Server::bind(bind, args.port, args.allow.clone(), state.clone()).await?;
            Some(tokio::spawn(server.run()))
        }
        None => None,
    };

    let rtc = match args.signal_url() {
        Some(signal_url) => {
            let device_id = args.device_id.clone().unwrap_or_else(|| {
                gethostname::gethostname().to_string_lossy().into_owned()
            });
            info!(%device_id, %signal_url, "webrtc transport enabled");
            let config = tetherd::webrtc::RtcConfig {
                signal_url,
                secret: args.secret.clone().expect("validated"),
                device_name: device_id.clone(),
                device_id,
                stun: args.stun.clone(),
            };
            Some(tokio::spawn(tetherd::webrtc::run_host(config, state)))
        }
        None => None,
    };

    let transports = async {
        match (lan, rtc) {
            (Some(l), Some(r)) => tokio::select! { res = l => res?, res = r => res? },
            (Some(l), None) => l.await?,
            (None, Some(r)) => r.await?,
            (None, None) => unreachable!("validated"),
        }
    };

    tokio::select! {
        result = transports => result,
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn start_capture(
    codec: tetherd::config::CodecArg,
    bitrate_kbps: u32,
) -> anyhow::Result<tetherd::pipeline::Pipeline> {
    use tetherd::capture::FrameEncoder;
    use tetherd::config::CodecArg;
    tetherd::pipeline::start(
        || tetherd::capture::macos::SckCapturer::main_display(30),
        move || -> anyhow::Result<Box<dyn FrameEncoder>> {
            Ok(match codec {
                CodecArg::Jpeg => Box::new(tetherd::encode::JpegEncoder::new(75)?),
                CodecArg::H264 => Box::new(tetherd::encode::h264::VtH264Encoder::new(bitrate_kbps)?),
            })
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn start_capture(
    _codec: tetherd::config::CodecArg,
    _bitrate_kbps: u32,
) -> anyhow::Result<tetherd::pipeline::Pipeline> {
    anyhow::bail!("no screen capture implementation for this platform yet (Phase 1 hosts macOS only)")
}

/// Drain input events onto a dedicated thread; the injector is built there
/// because CGEventSource is thread-affine. Requires Accessibility permission
/// (System Settings → Privacy & Security → Accessibility) — without it macOS
/// silently discards posted events.
#[cfg(target_os = "macos")]
fn start_injector(mut input_rx: mpsc::Receiver<InjectCommand>) {
    use tetherd::input::InputInjector;
    std::thread::spawn(move || {
        let mut injector = match tetherd::input::macos::MacInjector::new() {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "input injection unavailable");
                return;
            }
        };
        while let Some(cmd) = input_rx.blocking_recv() {
            let result = match &cmd {
                InjectCommand::Event(ev) => injector.inject(ev),
                InjectCommand::Text(text) => injector.inject_text(text),
            };
            if let Err(e) = result {
                tracing::warn!(error = %e, "inject failed");
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn start_injector(mut input_rx: mpsc::Receiver<InjectCommand>) {
    tokio::spawn(async move { while input_rx.recv().await.is_some() {} });
}

#[cfg(target_os = "macos")]
fn start_clipboard() -> anyhow::Result<tetherd::clipboard::ClipboardSync> {
    Ok(tetherd::clipboard::start(tetherd::clipboard::macos::MacClipboard::new)?)
}

#[cfg(not(target_os = "macos"))]
fn start_clipboard() -> anyhow::Result<tetherd::clipboard::ClipboardSync> {
    struct NoClipboard;
    impl tetherd::clipboard::Clipboard for NoClipboard {
        fn change_count(&mut self) -> i64 {
            0
        }
        fn read_text(&mut self) -> Option<String> {
            None
        }
        fn write_text(&mut self, _text: &str) -> i64 {
            0
        }
    }
    Ok(tetherd::clipboard::start(|| NoClipboard)?)
}
