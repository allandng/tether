use tetherd::config::Args;
use tetherd::input::InjectCommand;
use tetherd::server::{Server, ServerState};
use tokio::sync::{mpsc, watch};
use tracing::info;

/// Resolve when the process is asked to stop — Ctrl-C (SIGINT) or, so a service
/// manager (launchd `launchctl stop`, systemd) gets a graceful stop, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // colour only for a person at a terminal, never in a service log file
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    #[cfg(windows)]
    tetherd::win::init_dpi_awareness();

    let args = Args::load()?;
    if let Some(command) = &args.command {
        return run_command(command, &args).await;
    }
    args.validate().map_err(anyhow::Error::msg)?;
    #[cfg(target_os = "macos")]
    tetherd::macos_perms::check();

    let pipeline = start_capture(args.codec, args.bitrate_kbps)?;
    let (input_tx, input_rx) = mpsc::channel(256);
    start_injector(input_rx, pipeline.displays.clone());
    let clipboard = start_clipboard()?;

    // Device pairing state (host key + allowlist), shared across sessions.
    let auth_dir = tetherd::auth::PairingAuth::default_dir()?;
    let mut pairing = tetherd::auth::PairingAuth::load_or_create(&auth_dir)?;
    if args.pair {
        let code = pairing.arm(tetherd::auth::now_unix());
        info!("pairing armed (valid 5 min) — enter this code on the controller:");
        println!(
            "\n    Pairing code:  {}\n",
            tetherd::auth::group_code(&code)
        );
    }
    if args.allow_unpaired {
        tracing::warn!("--allow-unpaired: device pairing is NOT enforced (dev/LAN only)");
    }
    if !pairing.is_empty() {
        info!(
            devices = pairing.paired_devices().len(),
            "paired devices loaded; auth required"
        );
    }
    // Captured before the mutex swallows it: the WebRTC task signs every
    // registration with this, and taking the lock on each reconnect would be
    // needless contention for a value that never changes.
    let identity_seed = pairing.signal_identity_seed();
    let auth = std::sync::Arc::new(tokio::sync::Mutex::new(pairing));

    // System audio, opt-in. Unavailable (no device, unsupported platform) is
    // a warning, not a startup failure: video still works.
    let audio = if args.audio {
        match tetherd::audio::start_platform() {
            Ok(feed) => {
                info!("system audio enabled (WebRTC controllers only)");
                Some(feed)
            }
            Err(e) => {
                tracing::warn!(error = %e, "--audio requested but unavailable; continuing without");
                None
            }
        }
    } else {
        None
    };
    let host_name = args
        .device_id
        .clone()
        .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
    let upload_dir = args
        .upload_dir
        .clone()
        .unwrap_or_else(tetherd::files::FileConfig::default_dir);
    tetherd::files::sweep_stale_parts(&upload_dir);
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
        controller_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(
            args.max_controllers.max(1) as usize,
        )),
        displays: pipeline.displays,
        select_display: pipeline.select_display,
        host_name: host_name.clone().into(),
        files: std::sync::Arc::new(tetherd::files::FileConfig {
            dir: upload_dir,
            max_upload_bytes: args.max_upload_mb.saturating_mul(1024 * 1024),
        }),
        audio,
    };

    // Local control channel: `tetherd pair` / `tetherd status` against this
    // daemon. Failing to open it (another daemon owns it) is not fatal.
    let mut transports = Vec::new();
    if let Some(bind) = args.bind {
        transports.push(format!("lan {bind}:{}", args.port));
    }
    if let Some(url) = args.signal_url() {
        transports.push(format!("webrtc via {url}"));
    }
    let control = tetherd::control::ControlContext {
        auth: state.auth.clone(),
        slots: state.controller_slots.clone(),
        max_controllers: args.max_controllers.max(1),
        displays: state.displays.clone(),
        device_id: host_name.clone(),
        transports,
        audio: state.audio.is_some(),
    };
    tokio::spawn(async move {
        if let Err(e) = tetherd::control::serve(control).await {
            tracing::warn!(error = %e, "control channel unavailable; `tetherd pair` won't reach this daemon");
        }
    });

    let lan = match args.bind {
        Some(bind) => {
            let server = Server::bind(bind, args.port, args.allow.clone(), state.clone()).await?;
            Some(tokio::spawn(server.run()))
        }
        None => None,
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let rtc = match args.signal_url() {
        Some(signal_url) => {
            let device_id = host_name.clone();
            info!(%device_id, %signal_url, "webrtc transport enabled");
            let config = tetherd::webrtc::RtcConfig {
                signal_url,
                secret: args.secret.clone().expect("validated"),
                device_name: device_id.clone(),
                device_id,
                stun: args.stun.clone(),
                identity_seed,
            };
            Some(tokio::spawn(tetherd::webrtc::run_host(
                config,
                state,
                shutdown_rx,
            )))
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
        _ = shutdown_signal() => {
            info!("shutting down");
            // Ask the WebRTC host to close active peer connections so controllers
            // see a prompt disconnect instead of waiting out an ICE timeout, then
            // give it a brief moment before the process exits.
            let _ = shutdown_tx.send(true);
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(())
        }
    }
}

/// Management subcommands. `pair` and `status` talk to the running daemon over
/// the control channel; `devices` and `setup` operate on the on-disk state and
/// exit. None of them start a capture pipeline, so they work on a machine
/// where screen recording isn't granted at all.
async fn run_command(command: &tetherd::config::Command, args: &Args) -> anyhow::Result<()> {
    use tetherd::config::{Command, DevicesAction};
    use tetherd::control::{Request, request};
    match command {
        Command::Pair => {
            let resp = request(&Request::Pair).await?;
            match resp.code {
                Some(code) if resp.ok => {
                    println!("\n    Pairing code:  {code}\n");
                    println!("Valid for 5 minutes. Enter it on the controller when it asks.");
                    return Ok(());
                }
                _ => anyhow::bail!(resp.error.unwrap_or_else(|| "pairing failed".into())),
            }
        }
        Command::Status => {
            let resp = request(&Request::Status).await?;
            let Some(st) = resp.status else {
                anyhow::bail!(resp.error.unwrap_or_else(|| "no status".into()));
            };
            println!("tetherd {} — device id {}", st.version, st.device_id);
            for t in &st.transports {
                println!("  transport   {t}");
            }
            println!(
                "  controllers {} of {} connected",
                st.controllers, st.max_controllers
            );
            println!("  paired      {} device(s)", st.paired_devices);
            println!("  audio       {}", if st.audio { "on" } else { "off" });
            for d in &st.displays {
                println!("  display   {d}");
            }
            return Ok(());
        }
        Command::Setup {
            signal,
            secret,
            device_id,
            codec,
            audio,
        } => {
            let secret = match secret {
                Some(s) => s.clone(),
                None => {
                    eprint!("Signal secret: ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim().to_owned()
                }
            };
            if secret.is_empty() {
                anyhow::bail!("a secret is required");
            }
            let mut keys: Vec<(&str, toml::Value)> =
                vec![("signal", signal.clone().into()), ("secret", secret.into())];
            if let Some(id) = device_id {
                keys.push(("device_id", id.clone().into()));
            }
            if let Some(c) = codec {
                let name = match c {
                    tetherd::config::CodecArg::Jpeg => "jpeg",
                    tetherd::config::CodecArg::H264 => "h264",
                };
                keys.push(("codec", name.into()));
            }
            if *audio {
                keys.push(("audio", true.into()));
            }
            let path = match &args.config {
                Some(p) => p.clone(),
                None => tetherd::config::default_config_path()?,
            };
            tetherd::config::write_config_keys(&path, &keys)?;
            println!("Wrote {} (readable by you only).", path.display());
            return Ok(());
        }
        Command::Devices { .. } => {}
    }
    let dir = tetherd::auth::PairingAuth::default_dir()?;
    let mut auth = tetherd::auth::PairingAuth::load_or_create(&dir)?;
    match command {
        Command::Pair | Command::Status | Command::Setup { .. } => unreachable!("handled above"),
        Command::Devices {
            action: DevicesAction::List,
        } => {
            let devices = auth.paired_devices();
            if devices.is_empty() {
                println!("No paired devices. Start tetherd with --pair to add one.");
                return Ok(());
            }
            let now = tetherd::auth::now_unix();
            // Sorted so repeated runs are diffable rather than hash-ordered.
            // sort_by, not sort_by_key: a key borrowed from the element can't
            // outlive the closure call.
            let mut rows: Vec<_> = devices.iter().collect();
            rows.sort_by(|a, b| a.0.cmp(b.0));
            println!("{:<34}  {:<20}  PAIRED", "DEVICE ID", "NAME");
            for (id, dev) in rows {
                let age_days = now.saturating_sub(dev.paired_at) / 86_400;
                println!(
                    "{:<34}  {:<20}  {} days ago (unix {})",
                    id, dev.name, age_days, dev.paired_at
                );
            }
        }
        Command::Devices {
            action: DevicesAction::Revoke { device_id },
        } => {
            if auth.revoke(device_id)? {
                println!("Revoked {device_id}.");
                println!(
                    "A running tetherd picks this up at the device's next connect; \
                     an active session is not cut off."
                );
            } else {
                anyhow::bail!("no paired device with id {device_id}");
            }
        }
    }
    Ok(())
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
                CodecArg::H264 => {
                    Box::new(tetherd::encode::h264::VtH264Encoder::new(bitrate_kbps)?)
                }
            })
        },
    )
}

#[cfg(windows)]
fn start_capture(
    codec: tetherd::config::CodecArg,
    bitrate_kbps: u32,
) -> anyhow::Result<tetherd::pipeline::Pipeline> {
    use tetherd::capture::FrameEncoder;
    use tetherd::config::CodecArg;
    tetherd::pipeline::start(
        || tetherd::capture::windows::DxgiCapturer::primary(30),
        move || -> anyhow::Result<Box<dyn FrameEncoder>> {
            Ok(match codec {
                CodecArg::Jpeg => Box::new(tetherd::encode::JpegEncoder::new(75)?),
                CodecArg::H264 => Box::new(tetherd::encode::h264_soft::OpenH264Encoder::new(
                    bitrate_kbps,
                )?),
            })
        },
    )
}

#[cfg(not(any(target_os = "macos", windows)))]
fn start_capture(
    _codec: tetherd::config::CodecArg,
    _bitrate_kbps: u32,
) -> anyhow::Result<tetherd::pipeline::Pipeline> {
    anyhow::bail!("no screen capture implementation for this platform (hosts: macOS, Windows)")
}

/// Drain input events onto a dedicated thread; the injector is built there
/// because platform handles (CGEventSource) are thread-affine. macOS requires
/// Accessibility permission (System Settings → Privacy & Security →
/// Accessibility) — without it macOS silently discards posted events.
#[cfg(any(target_os = "macos", windows))]
fn start_injector(
    mut input_rx: mpsc::Receiver<InjectCommand>,
    displays: tokio::sync::watch::Receiver<Vec<tether_protocol::DisplayInfo>>,
) {
    use tetherd::input::InputInjector;
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let made = tetherd::input::macos::MacInjector::new();
        #[cfg(windows)]
        let made = tetherd::input::windows::WinInjector::new();
        let mut injector = match made {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(error = %e, "input injection unavailable");
                return;
            }
        };
        while let Some(cmd) = input_rx.blocking_recv() {
            // Point input at whatever display is currently captured (cheap
            // sync borrow; only acts on an actual change).
            if let Some(active) = displays.borrow().iter().find(|d| d.active) {
                injector.set_active_display(active.id);
            }
            let result = match &cmd {
                InjectCommand::Event(ev) => injector.inject(ev),
                InjectCommand::Text(text) => injector.inject_text(text),
                InjectCommand::ReleaseAll => {
                    injector.release_all();
                    Ok(())
                }
            };
            if let Err(e) = result {
                tracing::warn!(error = %e, "inject failed");
            }
        }
    });
}

#[cfg(not(any(target_os = "macos", windows)))]
fn start_injector(
    mut input_rx: mpsc::Receiver<InjectCommand>,
    _displays: tokio::sync::watch::Receiver<Vec<tether_protocol::DisplayInfo>>,
) {
    tokio::spawn(async move { while input_rx.recv().await.is_some() {} });
}

#[cfg(target_os = "macos")]
fn start_clipboard() -> anyhow::Result<tetherd::clipboard::ClipboardSync> {
    Ok(tetherd::clipboard::start(
        tetherd::clipboard::macos::MacClipboard::new,
    )?)
}

#[cfg(windows)]
fn start_clipboard() -> anyhow::Result<tetherd::clipboard::ClipboardSync> {
    // Built eagerly so a failure is reported at startup, then moved onto the
    // sync thread (WinClipboard only holds a handle to its own owner thread).
    let clipboard = tetherd::clipboard::windows::WinClipboard::new()?;
    Ok(tetherd::clipboard::start(move || clipboard)?)
}

#[cfg(not(any(target_os = "macos", windows)))]
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
