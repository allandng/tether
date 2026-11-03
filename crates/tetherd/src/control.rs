//! Local control channel (Phase 9): drive the running daemon without a
//! terminal attached to it — `tetherd pair` arms a pairing code and prints
//! it, `tetherd status` reports what the daemon is doing. Once tetherd runs as
//! a LaunchAgent or a logon task its stdout goes nowhere, so this is the only
//! way to pair a new phone without stopping the service.
//!
//! The channel arms pairing codes, so it is a privilege boundary:
//!
//! - **Windows:** a named pipe, `\\.\pipe\tether-control-<user>`, whose DACL
//!   grants access to the current user and SYSTEM only (the default pipe DACL
//!   would also let Everyone read), refusing remote clients, and created
//!   with `FILE_FLAG_FIRST_PIPE_INSTANCE` so nothing can squat the name first.
//! - **Unix:** a socket at `<state dir>/control.sock`, mode 0600 inside the
//!   0700 state directory, and every connection's peer uid is checked against
//!   our own.
//!
//! Wire format: one JSON request line, one JSON response line, close.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Semaphore, watch};
use tracing::info;

use crate::auth::PairingAuth;
use crate::files::DirAccess;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Pair,
    Status,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub version: String,
    pub device_id: String,
    pub transports: Vec<String>,
    pub controllers: u32,
    pub max_controllers: u32,
    pub displays: Vec<String>,
    pub paired_devices: usize,
    pub audio: bool,
    // Added after the first release, so optional both ways: an older daemon
    // omits them (`default`) and an older CLI ignores them.
    /// macOS Screen Recording grant *of the daemon process* (the CLI runs in
    /// another TCC context, so only the daemon can answer). `None` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_recording: Option<bool>,
    /// macOS Accessibility grant (`AXIsProcessTrusted`) of the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<bool>,
    /// Where uploads land.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_dir: Option<String>,
    /// Whether the daemon can use [`Status::upload_dir`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_dir_access: Option<DirAccess>,
}

impl Status {
    /// `tetherd status` lines for the grants and the upload folder, with what
    /// to do about anything missing. Empty for a daemon that predates them.
    pub fn permission_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(granted) = self.screen_recording {
            out.push(format!(
                "  permission  Screen Recording: {}",
                if granted {
                    "granted"
                } else {
                    "NOT granted — nothing can be captured; approve tetherd in System Settings → \
                     Privacy & Security → Screen Recording, then restart it"
                }
            ));
        }
        if let Some(granted) = self.accessibility {
            out.push(format!(
                "  permission  Accessibility: {}",
                if granted {
                    "granted"
                } else {
                    "NOT granted — remote input is ignored; enable tetherd in System Settings → \
                     Privacy & Security → Accessibility"
                }
            ));
        }
        if let Some(dir) = &self.upload_dir {
            let access = self.upload_dir_access.unwrap_or(DirAccess::Unknown);
            let state = match access {
                DirAccess::Ok => "ok",
                DirAccess::Missing => "not created yet (the first upload creates it)",
                DirAccess::Denied => "NO ACCESS",
                DirAccess::Error => "UNUSABLE",
                DirAccess::TimedOut => "check timed out",
                DirAccess::Unknown => "unknown",
            };
            let advice = access.advice();
            let sep = if advice.is_empty() { "" } else { " — " };
            out.push(format!("  uploads     {dir}: {state}{sep}{advice}"));
        }
        out
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the control channel can see of the daemon.
#[derive(Clone)]
pub struct ControlContext {
    pub auth: Arc<Mutex<PairingAuth>>,
    pub slots: Arc<Semaphore>,
    pub max_controllers: u32,
    pub displays: watch::Receiver<Vec<tether_protocol::DisplayInfo>>,
    pub device_id: String,
    pub transports: Vec<String>,
    pub audio: bool,
    /// Upload folder, probed for status requests (cached briefly).
    pub upload_dir: std::path::PathBuf,
}

/// The daemon's own macOS grants; `(None, None)` on other platforms.
fn host_grants() -> (Option<bool>, Option<bool>) {
    #[cfg(target_os = "macos")]
    {
        (
            Some(crate::macos_perms::screen_recording_granted()),
            Some(crate::macos_perms::accessibility_granted()),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        (None, None)
    }
}

/// A probe result is reused for this long.
const PROBE_TTL: Duration = Duration::from_secs(30);
/// How long a status request waits for a probe (the CLI gives up after 5 s).
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// One shared upload-folder probe for all status requests.
static UPLOAD_PROBE: ProbeCache = ProbeCache::new(PROBE_TTL, PROBE_WAIT);

/// Probe the upload folder off the async threads, bounded: on macOS a first
/// access to a protected folder can wait on a privacy prompt. Cached and
/// shared ([`ProbeCache`]), so `tetherd status` in a loop neither parks a
/// blocking thread per call nor churns probe files in ~/Downloads.
async fn probe_upload_dir(dir: &Path) -> DirAccess {
    UPLOAD_PROBE
        .get(dir, |dir| {
            DirAccess::from_probe(&crate::files::probe_dir(&dir))
        })
        .await
}

/// A probe finishing at the instant it records.
type ProbeFuture = Shared<BoxFuture<'static, (Instant, DirAccess)>>;

/// A blocking directory probe, cached for `ttl` and never run twice at once:
/// while one is in flight (say, waiting on a macOS privacy prompt), later
/// requests wait on *that* one — each for at most `wait`, answering
/// [`DirAccess::TimedOut`] — instead of starting another.
struct ProbeCache {
    ttl: Duration,
    wait: Duration,
    /// The latest probe (running or finished) and the directory it probes.
    slot: std::sync::Mutex<Option<(PathBuf, ProbeFuture)>>,
}

impl ProbeCache {
    const fn new(ttl: Duration, wait: Duration) -> Self {
        ProbeCache {
            ttl,
            wait,
            slot: std::sync::Mutex::new(None),
        }
    }

    async fn get(
        &self,
        dir: &Path,
        probe: impl FnOnce(PathBuf) -> DirAccess + Send + 'static,
    ) -> DirAccess {
        let pending = {
            let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            let reuse = match &*slot {
                // polled here so a probe that finished unobserved counts
                Some((d, fut)) if d == dir => match fut.clone().now_or_never() {
                    Some((at, access)) if at.elapsed() < self.ttl => return access,
                    Some(_) => None,           // stale: probe again
                    None => Some(fut.clone()), // in flight: share it
                },
                _ => None,
            };
            reuse.unwrap_or_else(|| {
                let owned = dir.to_path_buf();
                let fut = tokio::task::spawn_blocking(move || {
                    let access = probe(owned);
                    (Instant::now(), access)
                })
                .map(|joined| joined.unwrap_or_else(|_| (Instant::now(), DirAccess::Error)))
                .boxed()
                .shared();
                *slot = Some((dir.to_path_buf(), fut.clone()));
                fut
            })
        };
        match tokio::time::timeout(self.wait, pending).await {
            Ok((_, access)) => access,
            Err(_) => DirAccess::TimedOut,
        }
    }
}

impl ControlContext {
    async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Pair => {
                let code = self.auth.lock().await.arm(crate::auth::now_unix());
                info!("pairing armed via the control channel (valid 5 min)");
                Response {
                    ok: true,
                    code: Some(crate::auth::group_code(&code)),
                    ..Default::default()
                }
            }
            Request::Status => {
                let in_use = (self.max_controllers as usize)
                    .saturating_sub(self.slots.available_permits())
                    as u32;
                let displays = self
                    .displays
                    .borrow()
                    .iter()
                    .map(|d| {
                        format!(
                            "{}{} ({}x{})",
                            if d.active { "* " } else { "  " },
                            d.name,
                            d.width,
                            d.height
                        )
                    })
                    .collect();
                let paired_devices = self.auth.lock().await.paired_devices().len();
                let (screen_recording, accessibility) = host_grants();
                let upload_dir_access = probe_upload_dir(&self.upload_dir).await;
                Response {
                    ok: true,
                    status: Some(Status {
                        version: env!("CARGO_PKG_VERSION").into(),
                        device_id: self.device_id.clone(),
                        transports: self.transports.clone(),
                        controllers: in_use,
                        max_controllers: self.max_controllers,
                        displays,
                        paired_devices,
                        audio: self.audio,
                        screen_recording,
                        accessibility,
                        upload_dir: Some(self.upload_dir.display().to_string()),
                        upload_dir_access: Some(upload_dir_access),
                    }),
                    ..Default::default()
                }
            }
        }
    }
}

const MAX_REQUEST: usize = 4096;

async fn serve_conn<S: AsyncRead + AsyncWrite + Unpin>(stream: S, ctx: &ControlContext) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        (&mut reader).take(MAX_REQUEST as u64).read_line(&mut line),
    )
    .await;
    let response = match read {
        Ok(Ok(_)) => match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => ctx.handle(req).await,
            Err(e) => Response {
                error: Some(format!("bad request: {e}")),
                ..Default::default()
            },
        },
        _ => return,
    };
    let mut out = serde_json::to_string(&response).unwrap_or_default();
    out.push('\n');
    let stream = reader.get_mut();
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.flush().await;
}

// ---- endpoints ---------------------------------------------------------------

#[cfg(windows)]
pub fn endpoint() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_else(|_| "user".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!(r"\\.\pipe\tether-control-{user}")
}

#[cfg(unix)]
pub fn endpoint() -> std::path::PathBuf {
    crate::config::state_dir()
        .map(|d| d.join("control.sock"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/tether-control.sock"))
}

/// Serve the control channel until the process exits.
#[cfg(windows)]
pub async fn serve(ctx: ControlContext) -> anyhow::Result<()> {
    let name = endpoint();
    let mut server = windows_pipe::create(&name, true).with_context(|| {
        format!("creating control pipe {name} (is another tetherd already running?)")
    })?;
    info!(pipe = %name, "control channel listening");
    loop {
        // A client that connects and vanishes before we accept fails this
        // instance only; the channel must outlive it (it's the only way to
        // pair once tetherd runs in the background).
        match server.connect().await {
            Ok(()) => {
                let client = server;
                let ctx = ctx.clone();
                tokio::spawn(async move { serve_conn(client, &ctx).await });
            }
            Err(e) => {
                tracing::debug!(error = %e, "control pipe connect failed; recycling instance")
            }
        }
        server = loop {
            match windows_pipe::create(&name, false) {
                Ok(s) => break s,
                Err(e) => {
                    tracing::warn!(error = %e, "control pipe instance failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        };
    }
}

#[cfg(unix)]
pub async fn serve(ctx: ControlContext) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = endpoint();
    if path.exists() {
        // A live daemon answers; a stale socket from a crash doesn't.
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("another tetherd is already serving {}", path.display());
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding control socket {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    info!(socket = %path.display(), "control channel listening");
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    loop {
        let (stream, _) = listener.accept().await?;
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == me => {}
            other => {
                tracing::warn!(?other, "control connection from another user refused");
                continue;
            }
        }
        let ctx = ctx.clone();
        tokio::spawn(async move { serve_conn(stream, &ctx).await });
    }
}

/// Client side, for the `pair` / `status` subcommands.
pub async fn request(req: &Request) -> anyhow::Result<Response> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    #[cfg(windows)]
    let mut stream = {
        let name = endpoint();
        let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .map_err(|e| not_running(e, &name))?;
        // The name is predictable, so another local user could create it
        // first; only talk to a pipe served by a process running as us.
        windows_pipe::verify_server_is_us(&pipe)?;
        pipe
    };
    #[cfg(unix)]
    let mut stream = {
        let path = endpoint();
        tokio::net::UnixStream::connect(&path)
            .await
            .map_err(|e| not_running(e, &path.display().to_string()))?
    };
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut out = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut out),
    )
    .await
    .context("daemon did not answer")??;
    serde_json::from_str(out.trim()).context("bad response from daemon")
}

fn not_running(e: std::io::Error, at: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no running tetherd answered on {at} ({e}). Start the daemon first \
         (or run it in a terminal with --pair)."
    )
}

#[cfg(windows)]
mod windows_pipe {
    use std::ffi::c_void;

    use anyhow::{Context, anyhow};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    /// The current user's SID as a string (S-1-5-21-…).
    fn current_user_sid() -> anyhow::Result<String> {
        // SAFETY: pseudo-handle for this process; never needs closing.
        process_user_sid(unsafe { GetCurrentProcess() })
    }

    /// Refuse a control pipe whose server process isn't running as us.
    pub fn verify_server_is_us(
        pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
    ) -> anyhow::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let mut pid = 0u32;
        // SAFETY: a live pipe handle and an out-pointer; the process handle
        // is closed on every path.
        let theirs = unsafe {
            GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid)?;
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
            let sid = process_user_sid(process);
            let _ = windows::Win32::Foundation::CloseHandle(process);
            sid?
        };
        if theirs != current_user_sid()? {
            anyhow::bail!(
                "the control pipe is served by another user's process (pid {pid}); refusing to use it"
            );
        }
        Ok(())
    }

    fn process_user_sid(process: HANDLE) -> anyhow::Result<String> {
        // SAFETY: standard token query; buffers sized by the first call; the
        // token is closed on every path and the SID string freed with LocalFree.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token)?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let got = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            );
            let _ = windows::Win32::Foundation::CloseHandle(token);
            got?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid_str)?;
            let s = sid_str.to_string().context("SID not UTF-16")?;
            let _ = LocalFree(Some(HLOCAL(sid_str.0.cast())));
            Ok(s)
        }
    }

    pub fn create(name: &str, first: bool) -> anyhow::Result<NamedPipeServer> {
        // Protected DACL: full access for this user and SYSTEM, nobody else.
        let sddl = format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", current_user_sid()?);
        let wide = crate::win::wide(&sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: NUL-terminated SDDL; the descriptor is freed below after
        // the pipe (which copies it) has been created.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: attrs points at a valid SECURITY_ATTRIBUTES for the call.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    &mut attrs as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptor…; LocalFree.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        server.map_err(|e| anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_have_a_stable_json_shape() {
        assert_eq!(
            serde_json::to_string(&Request::Pair).unwrap(),
            r#"{"cmd":"pair"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"status"}"#).unwrap(),
            Request::Status
        );
        let r = Response {
            ok: true,
            code: Some("ABCD-EFGH".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"ok":true,"code":"ABCD-EFGH"}"#
        );
    }

    #[tokio::test]
    async fn pair_and_status_over_an_in_memory_stream() {
        let dir = std::env::temp_dir().join(format!("tether-control-test-{}", std::process::id()));
        let auth = Arc::new(Mutex::new(PairingAuth::load_or_create(&dir).unwrap()));
        let (_tx, displays) = watch::channel(vec![tether_protocol::DisplayInfo {
            id: 1,
            width: 1920,
            height: 1080,
            active: true,
            name: "Display 1".into(),
        }]);
        let ctx = ControlContext {
            auth,
            slots: Arc::new(Semaphore::new(2)),
            max_controllers: 2,
            displays,
            device_id: "pc".into(),
            transports: vec!["webrtc".into()],
            audio: false,
            upload_dir: dir.clone(),
        };
        for (req, check) in [
            ("{\"cmd\":\"pair\"}\n", "code"),
            ("{\"cmd\":\"status\"}\n", "status"),
            ("garbage\n", "error"),
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            let ctx2 = ctx.clone();
            let task = tokio::spawn(async move { serve_conn(server, &ctx2).await });
            client.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            BufReader::new(&mut client)
                .read_line(&mut out)
                .await
                .unwrap();
            task.await.unwrap();
            let resp: Response = serde_json::from_str(out.trim()).unwrap();
            match check {
                "code" => assert_eq!(resp.code.unwrap().len(), 9), // XXXX-XXXX
                "status" => {
                    let st = resp.status.unwrap();
                    assert_eq!((st.controllers, st.max_controllers), (0, 2));
                    assert_eq!(st.displays, vec!["* Display 1 (1920x1080)"]);
                    assert_eq!(st.upload_dir_access, Some(DirAccess::Ok));
                    assert_eq!(st.upload_dir, Some(dir.display().to_string()));
                    assert_eq!(st.screen_recording.is_some(), cfg!(target_os = "macos"));
                    assert_eq!(st.accessibility.is_some(), cfg!(target_os = "macos"));
                }
                _ => assert!(!resp.ok && resp.error.is_some()),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe that counts its runs and blocks until `release` says go.
    fn gated_probe(
        runs: &Arc<std::sync::atomic::AtomicUsize>,
        release: &Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
        result: DirAccess,
    ) -> impl FnOnce(PathBuf) -> DirAccess + Send + 'static {
        let (runs, release) = (Arc::clone(runs), Arc::clone(release));
        move |_| {
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = release.lock().unwrap().recv();
            result
        }
    }

    #[tokio::test]
    async fn a_pending_probe_is_shared_not_repeated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = ProbeCache::new(Duration::from_secs(60), Duration::from_millis(50));
        let runs = Arc::new(AtomicUsize::new(0));
        let (go, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(std::sync::Mutex::new(gate));
        let dir = Path::new("/nonexistent/Downloads");

        // A probe stuck on a privacy prompt: each status request times out
        // after `wait`, but they all wait on the one probe — no new blocking
        // thread per request.
        for _ in 0..3 {
            let got = cache
                .get(dir, gated_probe(&runs, &gate, DirAccess::Ok))
                .await;
            assert_eq!(got, DirAccess::TimedOut);
        }
        let concurrent = futures_util::future::join_all(
            (0..3).map(|_| cache.get(dir, gated_probe(&runs, &gate, DirAccess::Ok))),
        )
        .await;
        assert_eq!(concurrent, vec![DirAccess::TimedOut; 3]);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // The prompt is answered: the next request gets the probe's result
        // (even though nobody was waiting when it finished)...
        go.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = cache
            .get(dir, gated_probe(&runs, &gate, DirAccess::Denied))
            .await;
        assert_eq!(got, DirAccess::Ok);
        // ...and within the TTL it's served from the cache, probe-free.
        for _ in 0..3 {
            let got = cache
                .get(dir, gated_probe(&runs, &gate, DirAccess::Denied))
                .await;
            assert_eq!(got, DirAccess::Ok);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn probe_results_expire_and_are_per_directory() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let (go, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(std::sync::Mutex::new(gate));
        for _ in 0..8 {
            go.send(()).unwrap(); // these probes don't block
        }
        let (a, b) = (Path::new("/tmp/a"), Path::new("/tmp/b"));

        // TTL zero: every request probes afresh
        let fresh = ProbeCache::new(Duration::ZERO, Duration::from_secs(5));
        assert_eq!(
            fresh.get(a, gated_probe(&runs, &gate, DirAccess::Ok)).await,
            DirAccess::Ok
        );
        assert_eq!(
            fresh
                .get(a, gated_probe(&runs, &gate, DirAccess::Denied))
                .await,
            DirAccess::Denied
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        // a different directory is never answered from another's result
        let cache = ProbeCache::new(Duration::from_secs(60), Duration::from_secs(5));
        assert_eq!(
            cache.get(a, gated_probe(&runs, &gate, DirAccess::Ok)).await,
            DirAccess::Ok
        );
        assert_eq!(
            cache
                .get(b, gated_probe(&runs, &gate, DirAccess::Missing))
                .await,
            DirAccess::Missing
        );
        assert_eq!(runs.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn status_grants_are_optional_on_the_wire() {
        // A daemon from before the grant fields: the new CLI still parses it.
        let old = r#"{"ok":true,"status":{"version":"0.1.0","device_id":"mac","transports":[],"controllers":0,"max_controllers":1,"displays":[],"paired_devices":0,"audio":false}}"#;
        let resp: Response = serde_json::from_str(old).unwrap();
        let st = resp.status.unwrap();
        assert_eq!(
            (st.screen_recording, st.accessibility, st.upload_dir_access),
            (None, None, None)
        );
        assert!(st.permission_lines().is_empty());
        // ...and a status without them serializes exactly as it used to.
        assert_eq!(
            serde_json::to_string(&Status::default()).unwrap(),
            r#"{"version":"","device_id":"","transports":[],"controllers":0,"max_controllers":0,"displays":[],"paired_devices":0,"audio":false}"#
        );

        let new = Status {
            screen_recording: Some(true),
            accessibility: Some(false),
            upload_dir: Some("/home/me/Downloads".into()),
            upload_dir_access: Some(DirAccess::Denied),
            ..Default::default()
        };
        let json = serde_json::to_string(&new).unwrap();
        assert!(
            json.ends_with(
                r#""screen_recording":true,"accessibility":false,"upload_dir":"/home/me/Downloads","upload_dir_access":"denied"}"#
            ),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), new);
        // An older CLI's view of a newer daemon: unknown fields are ignored.
        #[derive(Deserialize)]
        struct OldStatus {
            version: String,
            audio: bool,
        }
        let old_view: OldStatus = serde_json::from_str(&json).unwrap();
        assert_eq!((old_view.version.as_str(), old_view.audio), ("", false));

        let lines = new.permission_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "  permission  Screen Recording: granted");
        assert!(lines[1].starts_with("  permission  Accessibility: NOT granted"));
        assert!(
            lines[2].starts_with("  uploads     /home/me/Downloads: NO ACCESS — uploads will fail"),
            "{}",
            lines[2]
        );
    }
}
