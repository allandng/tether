use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde::Deserialize;

/// Tether host daemon: stream this machine's screen to an allowed controller
/// and inject the input events it sends back. Two transports, either or both:
/// a LAN WebSocket (--bind/--allow) and signaled WebRTC (--signal/--secret).
///
/// Every flag can also be set in `config.toml` in the state directory
/// (`~/.config/tether` on macOS, `%APPDATA%\tether` on Windows), which is how
/// a service runs it without the secret appearing in its command line. A flag
/// given on the command line overrides the file; the file overrides defaults.
#[derive(Parser, Debug, Clone)]
#[command(name = "tetherd", version)]
pub struct Args {
    /// Management subcommands. With none, `tetherd` runs the daemon — so every
    /// existing invocation keeps working unchanged.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Read settings from this file instead of `<state dir>/config.toml`.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// LAN transport: interface address to bind. Must be loopback or a
    /// private/link-local LAN address; 0.0.0.0 and public addresses are
    /// refused.
    #[arg(long)]
    pub bind: Option<IpAddr>,

    /// LAN transport: TCP port to listen on.
    #[arg(long, default_value_t = 7878)]
    pub port: u16,

    /// LAN transport: controller IP allowed to connect (repeat for multiple).
    /// Connections from any other address are dropped before the handshake.
    #[arg(long = "allow")]
    pub allow: Vec<IpAddr>,

    /// WebRTC transport: signal server, as host:port or a full ws:// URL.
    #[arg(long)]
    pub signal: Option<String>,

    /// WebRTC transport: pre-shared secret for the signal server.
    #[arg(long)]
    pub secret: Option<String>,

    /// Device id announced to the signal server (default: hostname).
    #[arg(long)]
    pub device_id: Option<String>,

    /// STUN server (repeatable).
    #[arg(long = "stun", default_value = "stun:stun.l.google.com:19302")]
    pub stun: Vec<String>,

    /// Frame codec. JPEG is robust everywhere; H.264 (hardware, VideoToolbox)
    /// cuts bandwidth ~10x and is what you want over WAN.
    #[arg(long, value_enum, default_value_t = CodecArg::Jpeg)]
    pub codec: CodecArg,

    /// Target bitrate for --codec h264 (also the adaptive-bitrate ceiling).
    #[arg(long, default_value_t = 4000)]
    pub bitrate_kbps: u32,

    /// Maximum simultaneous controllers (shared across LAN + WebRTC). >1 lets
    /// several devices view and control at once; input interleaves. Default 1.
    #[arg(long, default_value_t = 1)]
    pub max_controllers: u32,

    /// Arm a one-time pairing code at startup (printed to the terminal) so a
    /// new controller can pair. Valid 5 minutes.
    #[arg(long)]
    pub pair: bool,

    /// Refuse any controller that isn't paired, even before the first device is
    /// paired. (The gate auto-activates once any device is paired regardless.)
    #[arg(long)]
    pub require_pairing: bool,

    /// Escape hatch: keep the unauthenticated path open even after devices are
    /// paired. Dev/LAN only — defeats pairing; logged loudly.
    #[arg(long)]
    pub allow_unpaired: bool,

    /// Folder that files uploaded from a controller land in (default: this
    /// user's Downloads folder). The controller never chooses the location.
    #[arg(long)]
    pub upload_dir: Option<std::path::PathBuf>,

    /// Largest file a controller may upload, in MiB.
    #[arg(long, default_value_t = 4096)]
    pub max_upload_mb: u64,

    /// Stream this machine's system audio to controllers (WebRTC only). Off
    /// by default: it would otherwise broadcast whatever is playing in the
    /// room to whoever connects.
    #[arg(long)]
    pub audio: bool,
}

/// `config.toml`: the same settings as the flags, by the same names.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub bind: Option<IpAddr>,
    pub port: Option<u16>,
    pub allow: Option<Vec<IpAddr>>,
    pub signal: Option<String>,
    pub secret: Option<String>,
    pub device_id: Option<String>,
    pub stun: Option<Vec<String>>,
    pub codec: Option<CodecArg>,
    pub bitrate_kbps: Option<u32>,
    pub max_controllers: Option<u32>,
    pub require_pairing: Option<bool>,
    pub allow_unpaired: Option<bool>,
    pub upload_dir: Option<PathBuf>,
    pub max_upload_mb: Option<u64>,
    pub audio: Option<bool>,
}

/// The default config file location.
pub fn default_config_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("config.toml"))
}

impl Args {
    /// Parse the command line, then fill every setting *not* given there from
    /// the config file (if it exists).
    pub fn load() -> anyhow::Result<Args> {
        let matches = Args::command().get_matches();
        let mut args = Args::from_arg_matches(&matches)?;
        let path = match &args.config {
            Some(p) => p.clone(),
            None => default_config_path()?,
        };
        if path.exists() {
            let file = read_config(&path)?;
            args.apply_file(file, |id| {
                matches.value_source(id) == Some(ValueSource::CommandLine)
            });
        } else if args.config.is_some() && !matches!(args.command, Some(Command::Setup { .. })) {
            // `setup --config <new file>` creates it; anything else needs it.
            anyhow::bail!("config file {} not found", path.display());
        }
        Ok(args)
    }

    /// Merge `file` under the command line: a setting is taken from the file
    /// only when `from_cli(<flag id>)` says the user didn't pass it.
    pub fn apply_file(&mut self, f: FileConfig, from_cli: impl Fn(&str) -> bool) {
        macro_rules! fill {
            ($field:ident) => {
                if !from_cli(stringify!($field))
                    && let Some(v) = f.$field
                {
                    self.$field = v;
                }
            };
            (opt $field:ident) => {
                if !from_cli(stringify!($field)) && f.$field.is_some() {
                    self.$field = f.$field;
                }
            };
        }
        fill!(opt bind);
        fill!(port);
        fill!(allow);
        fill!(opt signal);
        fill!(opt secret);
        fill!(opt device_id);
        fill!(stun);
        fill!(codec);
        fill!(bitrate_kbps);
        fill!(max_controllers);
        fill!(require_pairing);
        fill!(allow_unpaired);
        fill!(opt upload_dir);
        fill!(max_upload_mb);
        fill!(audio);
    }
}

/// Read and parse a config file. On Unix it holds the signal secret, so —
/// like `host.key` — a group/world-readable file is refused.
pub fn read_config(path: &Path) -> anyhow::Result<FileConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is group/world-accessible (mode {mode:o}) and holds the signal secret; \
                 run: chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

/// `tetherd setup`: write (or update) keys in the config file, owner-only.
/// Keys not mentioned are kept as they were.
pub fn write_config_keys(path: &Path, keys: &[(&str, toml::Value)]) -> anyhow::Result<()> {
    let mut table: toml::Table = if path.exists() {
        std::fs::read_to_string(path)?
            .parse()
            .with_context(|| format!("parsing config {}", path.display()))?
    } else {
        toml::Table::new()
    };
    for (k, v) in keys {
        table.insert((*k).to_owned(), v.clone());
    }
    // validate before writing: never leave a config the daemon can't load
    let text = toml::to_string(&table)?;
    let _: FileConfig = toml::from_str(&text).context("resulting config is invalid")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::auth::atomic_write(path, text.as_bytes(), 0o600)
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum Command {
    /// Arm a one-time pairing code on the running daemon and print it.
    Pair,
    /// Show the running daemon's state: transports, controllers, displays.
    Status,
    /// Write the signal server and secret (and optionally other settings) to
    /// the config file, owner-readable only — so a service can start without
    /// any secret on its command line. Omit --secret to be prompted for it.
    Setup {
        /// Signal server: host:port, or a full ws:// / wss:// URL.
        #[arg(long)]
        signal: String,
        /// Pre-shared signal secret (prompted for on stdin when omitted).
        #[arg(long)]
        secret: Option<String>,
        /// Device id to register as (default: hostname).
        #[arg(long)]
        device_id: Option<String>,
        /// Frame codec to configure.
        #[arg(long, value_enum)]
        codec: Option<CodecArg>,
        /// Also stream system audio.
        #[arg(long)]
        audio: bool,
    },
    /// Inspect and revoke paired devices.
    ///
    /// Operates directly on `~/.config/tether/paired.json`. A running daemon
    /// notices the change (it re-reads the file when its mtime moves), so a
    /// revocation takes effect at the device's next connect without a restart.
    Devices {
        #[command(subcommand)]
        action: DevicesAction,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum DevicesAction {
    /// List every paired device.
    List,
    /// Revoke a device: it must pair again with a fresh code to reconnect.
    Revoke {
        /// The device id from `tetherd devices list`.
        device_id: String,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecArg {
    Jpeg,
    H264,
}

impl Args {
    /// At least one transport, and each transport's flags complete.
    pub fn validate(&self) -> Result<(), String> {
        if self.bind.is_none() && self.signal.is_none() {
            return Err(
                "nothing to do: pass --bind/--allow (LAN) and/or --signal/--secret (WebRTC)".into(),
            );
        }
        if self.bind.is_some() && self.allow.is_empty() {
            return Err("--bind requires at least one --allow".into());
        }
        if self.bind.is_none() && !self.allow.is_empty() {
            return Err("--allow requires --bind".into());
        }
        if self.signal.is_some() != self.secret.is_some() {
            return Err("--signal and --secret go together".into());
        }
        Ok(())
    }

    /// Accept "host:port" shorthand or a full ws(s):// URL.
    pub fn signal_url(&self) -> Option<String> {
        self.signal.as_ref().map(|s| {
            if s.starts_with("ws://") || s.starts_with("wss://") {
                s.clone()
            } else {
                format!("ws://{s}/ws")
            }
        })
    }
}

/// Where per-user state lives (`host.key`, `paired.json`, `config.toml`):
/// `~/.config/tether` on macOS/Unix, `%APPDATA%\tether` on Windows.
pub fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .context("APPDATA not set")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config"))
        .context("HOME not set")?;
    Ok(base.join("tether"))
}

/// `tetherd` is a remote-control backdoor by design; refuse to listen on
/// anything that could be a public interface. Loopback is allowed for
/// same-machine development and tests.
pub fn validate_bind_addr(ip: IpAddr) -> Result<(), String> {
    let ok = match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    };
    if ip.is_unspecified() {
        return Err(format!(
            "refusing to bind {ip}: binding all interfaces would expose the daemon beyond the LAN"
        ));
    }
    if ok {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {ip}: not a loopback or private LAN address"
        ))
    }
}

pub fn ip_allowed(allow: &[IpAddr], peer: IpAddr) -> bool {
    allow.contains(&peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_fills_only_what_the_command_line_left_unset() {
        let mut args = Args::parse_from(["tetherd", "--port", "9000", "--codec", "jpeg"]);
        let file: FileConfig = toml::from_str(
            r#"
            signal = "wss://tether.example.com/ws"
            secret = "s3cret"
            port = 7000
            codec = "h264"
            bitrate_kbps = 2500
            allow = ["192.168.1.20"]
            audio = true
            "#,
        )
        .unwrap();
        let cli = ["port", "codec"];
        args.apply_file(file, |id| cli.contains(&id));
        assert_eq!(args.port, 9000, "CLI beats file");
        assert_eq!(args.codec, CodecArg::Jpeg, "CLI beats file");
        assert_eq!(args.signal.as_deref(), Some("wss://tether.example.com/ws"));
        assert_eq!(args.secret.as_deref(), Some("s3cret"));
        assert_eq!(args.bitrate_kbps, 2500, "file beats default");
        assert_eq!(args.allow, vec![ip("192.168.1.20")]);
        assert!(args.audio);
        assert_eq!(args.max_controllers, 1, "default when neither sets it");
    }

    #[test]
    fn config_file_rejects_typos() {
        assert!(toml::from_str::<FileConfig>("secrte = \"x\"").is_err());
        assert!(toml::from_str::<FileConfig>("codec = \"vp9\"").is_err());
    }

    #[test]
    fn setup_writes_a_loadable_owner_only_config() {
        let dir = std::env::temp_dir().join(format!("tether-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        write_config_keys(&path, &[("signal", "a:1".into()), ("secret", "x".into())]).unwrap();
        // a second setup keeps keys it doesn't mention
        write_config_keys(&path, &[("codec", "h264".into())]).unwrap();
        let cfg = read_config(&path).unwrap();
        assert_eq!(cfg.signal.as_deref(), Some("a:1"));
        assert_eq!(cfg.secret.as_deref(), Some("x"));
        assert_eq!(cfg.codec, Some(CodecArg::H264));
        assert!(write_config_keys(&path, &[("codec", "vp9".into())]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bind_validation_accepts_loopback_and_private() {
        for good in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.31.2",
            "192.168.1.50",
            "169.254.7.7",
            "::1",
        ] {
            assert!(
                validate_bind_addr(ip(good)).is_ok(),
                "{good} should be bindable"
            );
        }
    }

    #[test]
    fn bind_validation_rejects_unspecified_and_public() {
        for bad in [
            "0.0.0.0",
            "::",
            "8.8.8.8",
            "203.0.113.7",
            "2001:db8::1",
            "172.32.0.1",
        ] {
            assert!(
                validate_bind_addr(ip(bad)).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn allowlist_is_exact_match() {
        let allow = vec![ip("192.168.1.20"), ip("127.0.0.1")];
        assert!(ip_allowed(&allow, ip("192.168.1.20")));
        assert!(ip_allowed(&allow, ip("127.0.0.1")));
        assert!(!ip_allowed(&allow, ip("192.168.1.21")));
        assert!(!ip_allowed(&allow, ip("10.0.0.1")));
    }
}
