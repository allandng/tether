//! Safe fake host for hands-on controller testing (iOS Simulator, desktop
//! browsers) on a machine whose real input must not be touched.
//!
//! It serves the same LAN WebSocket transport as `tetherd --bind 127.0.0.1`,
//! built from the library's real `Server` + capture/encode `pipeline`, but:
//!
//! * the screen is synthetic — a 1280×800 grid with coloured quadrants, a
//!   sweeping bar (frame flow is visible) and a marker where the host thinks
//!   the pointer is (white = up, red = a button held), so zoom/pan/cursor
//!   mapping can be judged from a screenshot;
//! * the "injector" NEVER posts anything: every input command is written as a
//!   JSON line to `--log <path>` (if given) and to stderr. Nothing here calls
//!   the platform injectors (`CGEventPost`/`SendInput`) or clipboards
//!   (NSPasteboard/Win32) — only `server`, `pipeline`, `encode::JpegEncoder`,
//!   `auth` and `files` are used;
//! * clipboard text from the controller is logged, not written anywhere;
//! * uploads land in a throwaway temp dir; pairing state lives in a temp dir
//!   and pairing is not enforced (`allow_unpaired`).
//!
//! ```sh
//! cargo run -p tetherd --example fake_host -- --port 7979 --log /tmp/fake_host.jsonl
//! # then open http://localhost:5173/?host=127.0.0.1:7979
//! ```

use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::{Value, json};
use tether_protocol::{InputEvent, MouseButton, Resolution, modifiers};
use tetherd::capture::{RawFrame, ScreenCapturer};
use tetherd::encode::JpegEncoder;
use tetherd::input::InjectCommand;
use tetherd::server::{AuthPolicy, Server, ServerState};
use tokio::sync::{mpsc, watch};

#[derive(Parser)]
#[command(about = "Fake Tether host: synthetic screen, logged (never injected) input")]
struct Args {
    /// Interface to bind (loopback or a private LAN address).
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// TCP port (deliberately not tetherd's 7878, so the two can't be confused).
    #[arg(long, default_value_t = 7979)]
    port: u16,
    /// Controller IPs allowed to connect (repeatable).
    #[arg(long, default_values_t = [IpAddr::from([127, 0, 0, 1])])]
    allow: Vec<IpAddr>,
    /// Append received input / clipboard as JSON lines to this file.
    #[arg(long)]
    log: Option<PathBuf>,
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 800)]
    height: u32,
}

/// Last pointer position (normalized 0..=65535) and whether a button is held,
/// shared from the logger to the capturer so the marker tracks input.
#[derive(Clone, Copy, Default)]
struct Pointer {
    x: u16,
    y: u16,
    down: bool,
    seen: bool,
}

/// ~30 fps synthetic screen. The static background is rendered once; each
/// frame copies it (one memcpy) and paints the bar and the pointer marker, so
/// even a debug build keeps up.
struct FakeCapturer {
    width: u32,
    height: u32,
    background: Vec<u8>,
    tick: u32,
    pointer: Arc<Mutex<Pointer>>,
}

impl FakeCapturer {
    fn new(width: u32, height: u32, pointer: Arc<Mutex<Pointer>>) -> Self {
        let (w, h) = (width as usize, height as usize);
        let mut bg = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // quadrant tint (BGR): TL blue, TR green, BL red, BR grey
                let (b, g, r) = match (x * 2 < w, y * 2 < h) {
                    (true, true) => (150, 60, 40),
                    (false, true) => (50, 130, 40),
                    (true, false) => (40, 50, 150),
                    (false, false) => (90, 90, 90),
                };
                // grid: thin lines every 40 px, bold every 200 px
                let line = if x % 200 < 3 || y % 200 < 3 {
                    255
                } else if x % 40 == 0 || y % 40 == 0 {
                    200
                } else {
                    0
                };
                let px = &mut bg[(y * w + x) * 4..][..4];
                if line > 0 {
                    px.copy_from_slice(&[line, line, line, 255]);
                } else {
                    px.copy_from_slice(&[b, g, r, 255]);
                }
            }
        }
        FakeCapturer {
            width,
            height,
            background: bg,
            tick: 0,
            pointer,
        }
    }

    fn fill(buf: &mut [u8], w: usize, h: usize, rect: (usize, usize, usize, usize), bgra: [u8; 4]) {
        let (x0, y0, x1, y1) = (rect.0.min(w), rect.1.min(h), rect.2.min(w), rect.3.min(h));
        for y in y0..y1 {
            for x in x0..x1 {
                buf[(y * w + x) * 4..][..4].copy_from_slice(&bgra);
            }
        }
    }
}

impl ScreenCapturer for FakeCapturer {
    fn resolution(&self) -> Resolution {
        Resolution {
            width: self.width,
            height: self.height,
        }
    }

    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        std::thread::sleep(Duration::from_millis(33));
        self.tick = self.tick.wrapping_add(1);
        let (w, h) = (self.width as usize, self.height as usize);
        let mut buf = self.background.clone();
        // a yellow bar sweeping left→right, one screen width every ~4 s
        let bar_x = (self.tick as usize * 10) % w;
        Self::fill(
            &mut buf,
            w,
            h,
            (bar_x, 0, bar_x + 12, h),
            [0, 220, 255, 255],
        );
        let p = *self.pointer.lock().unwrap();
        if p.seen {
            let px = p.x as usize * (w - 1) / 65535;
            let py = p.y as usize * (h - 1) / 65535;
            let colour = if p.down {
                [0, 0, 255, 255]
            } else {
                [255, 255, 255, 255]
            };
            let rect = (px.saturating_sub(6), py.saturating_sub(6), px + 7, py + 7);
            Self::fill(&mut buf, w, h, rect, colour);
        }
        Ok(RawFrame::from_bgra(
            self.width,
            self.height,
            w * 4,
            buf,
            self.tick as u64 * 33_333,
        ))
    }
}

fn mod_names(m: u8) -> Vec<&'static str> {
    [
        (modifiers::SHIFT, "shift"),
        (modifiers::CTRL, "ctrl"),
        (modifiers::ALT, "alt"),
        (modifiers::META, "meta"),
        (modifiers::CAPS_LOCK, "caps_on"),
        (modifiers::CAPS_LOCK_KNOWN, "caps_known"),
    ]
    .into_iter()
    .filter(|(bit, _)| m & bit != 0)
    .map(|(_, name)| name)
    .collect()
}

fn button(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    }
}

/// JSON for one injector command; also updates the shared pointer marker.
fn describe(cmd: &InjectCommand, pointer: &Mutex<Pointer>) -> Value {
    let mut p = pointer.lock().unwrap();
    match cmd {
        InjectCommand::Event(InputEvent::MouseMove { x, y }) => {
            *p = Pointer {
                x: *x,
                y: *y,
                seen: true,
                ..*p
            };
            json!({"kind": "mouse_move", "x": x, "y": y})
        }
        InjectCommand::Event(InputEvent::MouseDown { button: b, x, y }) => {
            *p = Pointer {
                x: *x,
                y: *y,
                down: true,
                seen: true,
            };
            json!({"kind": "mouse_down", "button": button(*b), "x": x, "y": y})
        }
        InjectCommand::Event(InputEvent::MouseUp { button: b, x, y }) => {
            *p = Pointer {
                x: *x,
                y: *y,
                down: false,
                seen: true,
            };
            json!({"kind": "mouse_up", "button": button(*b), "x": x, "y": y})
        }
        InjectCommand::Event(InputEvent::Scroll { dx, dy }) => {
            json!({"kind": "scroll", "dx": dx, "dy": dy})
        }
        InjectCommand::Event(InputEvent::KeyDown { code, modifiers: m }) => {
            json!({"kind": "key_down", "code": code, "mods": m, "mod_names": mod_names(*m)})
        }
        InjectCommand::Event(InputEvent::KeyUp { code, modifiers: m }) => {
            json!({"kind": "key_up", "code": code, "mods": m, "mod_names": mod_names(*m)})
        }
        InjectCommand::Text(t) => {
            json!({"kind": "text", "text": t, "chars": t.chars().count()})
        }
        InjectCommand::ReleaseAll => {
            p.down = false;
            json!({"kind": "release_all"})
        }
    }
}

/// Appends timestamped JSON lines to stderr and (optionally) the log file.
struct Logger {
    file: Option<std::fs::File>,
    start: Instant,
}

impl Logger {
    fn line(&mut self, mut v: Value) {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        v["t_ms"] = json!(self.start.elapsed().as_millis() as u64);
        v["unix_ms"] = json!(unix_ms);
        let s = v.to_string();
        eprintln!("{s}");
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let file = match &args.log {
        Some(path) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        ),
        None => None,
    };
    let logger = Arc::new(Mutex::new(Logger {
        file,
        start: Instant::now(),
    }));
    let log = |v: Value| logger.lock().unwrap().line(v);

    let pointer = Arc::new(Mutex::new(Pointer::default()));
    let (w, h) = (args.width, args.height);
    let cap_pointer = Arc::clone(&pointer);
    let pipeline = tetherd::pipeline::start(
        move || Ok(FakeCapturer::new(w, h, Arc::clone(&cap_pointer))),
        || JpegEncoder::new(75),
    )?;

    // The "injector": log, never inject.
    let (input_tx, mut input_rx) = mpsc::channel::<InjectCommand>(256);
    {
        let logger = Arc::clone(&logger);
        let pointer = Arc::clone(&pointer);
        tokio::spawn(async move {
            while let Some(cmd) = input_rx.recv().await {
                let v = describe(&cmd, &pointer);
                logger.lock().unwrap().line(v);
            }
        });
    }

    // Clipboard from the controller: log only (no pasteboard is touched).
    let (clipboard_in_tx, clipboard_in_rx) = std::sync::mpsc::channel::<String>();
    {
        let logger = Arc::clone(&logger);
        std::thread::spawn(move || {
            while let Ok(text) = clipboard_in_rx.recv() {
                let v = json!({"kind": "clipboard", "text": text, "chars": text.chars().count()});
                logger.lock().unwrap().line(v);
            }
        });
    }
    // Host → controller clipboard: never set. The sender is kept alive so
    // sessions don't treat the source as closed.
    let (_clipboard_out_tx, clipboard_out_rx) = watch::channel(None);

    let scratch = std::env::temp_dir().join(format!("tether-fake-host-{}", std::process::id()));
    let auth = tetherd::auth::PairingAuth::load_or_create(&scratch.join("auth"))?;
    let state = ServerState {
        resolution: pipeline.resolution,
        frames: pipeline.frames,
        input_tx,
        clipboard_out: clipboard_out_rx,
        clipboard_in: clipboard_in_tx,
        auth: Arc::new(tokio::sync::Mutex::new(auth)),
        auth_policy: AuthPolicy {
            require_pairing: false,
            allow_unpaired: true,
        },
        bitrate: pipeline.bitrate,
        bitrate_ceiling_kbps: 4000,
        controller_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        displays: pipeline.displays,
        select_display: pipeline.select_display,
        host_name: "fake-host".into(),
        files: Arc::new(tetherd::files::FileConfig {
            dir: scratch.join("uploads"),
            max_upload_bytes: 64 << 20,
        }),
        audio: None,
    };
    std::fs::create_dir_all(scratch.join("uploads"))?;

    let server = Server::bind(args.bind, args.port, args.allow.clone(), state).await?;
    log(json!({
        "kind": "listening",
        "addr": server.local_addr()?.to_string(),
        "uploads": scratch.join("uploads"),
    }));
    tokio::select! {
        res = server.run() => res,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
