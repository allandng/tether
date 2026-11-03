//! File transfer (Phase 11): controller ⇄ host, over the same transport as
//! everything else (`tether-bulk` on WebRTC, the one socket on LAN).
//!
//! Uploads (controller → host) land in a fixed host directory — by default
//! the user's Downloads folder — chosen here, never by the controller: an
//! offer carries a bare name, which [`sanitize_file_name`] reduces to a safe
//! basename or refuses. Bytes go to a hidden temp file in that directory and
//! only get their real name once complete, via a hard link that fails rather
//! than overwrite (collisions get ` (1)`, ` (2)`, …). A cancelled, failed or
//! disconnected transfer deletes its temp file, so a truncated file never
//! wears the final name.
//!
//! Downloads (host → controller) take any host path the controller names. A
//! paired controller already has full keyboard and mouse control of this
//! machine, so a path allowlist would be friction without security; every
//! transfer is logged at `info` with its path, size and peer instead.
//!
//! One [`FileTransfers`] lives per controller session. Replies go out on an
//! unbounded queue (a handful of small messages per transfer) and download
//! chunks on a bounded one, so a slow link backpressures the file reader
//! rather than buffering a whole file in memory — and the session loop that
//! drains both can never deadlock against the engine it feeds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tether_protocol::{
    FILE_CHUNK_LEN, FileAccept, FileChunk, FileEnd, FileOffer, FileRequest, FileStatus,
    MAX_FILE_NAME_LEN, Message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Concurrent transfers per session, per direction.
const MAX_ACTIVE: usize = 4;
/// Download chunks buffered between the file reader and the transport.
pub const DATA_QUEUE: usize = 16;
/// Keep this much disk free beyond the offered size.
const FREE_SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Where uploads land and how big they may be.
#[derive(Debug, Clone)]
pub struct FileConfig {
    pub dir: PathBuf,
    pub max_upload_bytes: u64,
}

impl FileConfig {
    /// The user's Downloads folder (`%USERPROFILE%\Downloads` on Windows,
    /// `~/Downloads` elsewhere).
    pub fn default_dir() -> PathBuf {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE");
        #[cfg(not(windows))]
        let home = std::env::var_os("HOME");
        home.map(|h| PathBuf::from(h).join("Downloads"))
            .unwrap_or_else(std::env::temp_dir)
    }
}

// ---- upload-folder access --------------------------------------------------

/// Can tetherd use the upload folder? Reported by `tetherd status`.
///
/// On macOS, `~/Downloads` is privacy-protected: a LaunchAgent's first access
/// raises a separate "access files in your Downloads folder" prompt, and if
/// that is denied (or nobody was there to answer) every upload fails with
/// EPERM. The startup check logs how to fix it; `tetherd status` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirAccess {
    /// Listable and writable.
    Ok,
    /// Does not exist yet; the first upload creates it.
    Missing,
    /// The OS refused (EPERM/EACCES): on macOS, the folder's privacy prompt
    /// was denied or never answered.
    Denied,
    /// Not a directory, a read-only volume, an I/O error, …
    Error,
    /// The check did not finish in time (it may be waiting on a prompt).
    TimedOut,
    /// A state this build does not know (sent by a newer daemon).
    #[serde(other)]
    Unknown,
}

impl DirAccess {
    /// Classify the outcome of [`probe_dir`].
    pub fn from_probe(probe: &std::io::Result<bool>) -> Self {
        match probe {
            Ok(true) => DirAccess::Ok,
            Ok(false) => DirAccess::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DirAccess::Denied,
            Err(_) => DirAccess::Error,
        }
    }

    /// What to tell the operator (empty when all is well).
    pub fn advice(self) -> &'static str {
        match self {
            DirAccess::Ok | DirAccess::Missing => "",
            DirAccess::Denied if cfg!(target_os = "macos") => {
                "uploads will fail: allow tetherd in System Settings → Privacy & Security → \
                 Files & Folders (Downloads Folder), or set upload_dir in config.toml to a \
                 folder outside Desktop/Documents/Downloads"
            }
            DirAccess::Denied => "uploads will fail: no permission to write to the upload folder",
            DirAccess::Error => "uploads will fail: the upload folder is not usable",
            DirAccess::TimedOut => {
                "the access check did not finish (a macOS privacy prompt may be waiting)"
            }
            DirAccess::Unknown => "",
        }
    }
}

/// Probe `dir` the way an upload uses it: list it, then create and remove a
/// hidden file. `Ok(false)` if it does not exist yet. On macOS the first call
/// for a protected folder can raise (and wait on) a privacy prompt, so keep
/// it off latency-sensitive threads.
pub fn probe_dir(dir: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(std::io::Error::other("not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    std::fs::read_dir(dir)?;
    static PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let probe = dir.join(format!(
        ".tether-{}-{}.probe",
        std::process::id(),
        PROBES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let _ = std::fs::remove_file(&probe);
    Ok(true)
}

/// Startup check of the upload folder: log anything that will make uploads
/// fail, with what to do about it.
pub fn check_upload_dir(dir: &Path) -> DirAccess {
    let probe = probe_dir(dir);
    let access = DirAccess::from_probe(&probe);
    match &probe {
        Ok(_) => info!(dir = %dir.display(), ?access, "upload folder"),
        Err(e) => warn!(dir = %dir.display(), error = %e, "{}", access.advice()),
    }
    access
}

/// Outbound message queues a session drains (see the module docs).
pub struct Outbound {
    pub control: mpsc::UnboundedReceiver<Message>,
    pub data: mpsc::Receiver<Message>,
}

/// Is this one of the file-transfer messages the engine handles?
pub fn is_file_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::FileOffer(_)
            | Message::FileAccept(_)
            | Message::FileChunk(_)
            | Message::FileEnd(_)
            | Message::FileRequest(_)
    )
}

// ---- name safety (pure, table-tested) ------------------------------------

/// Reduce a controller-supplied name to a safe basename, or refuse it.
///
/// Refused outright: empty names, path separators (`/` and `\` everywhere),
/// `.` / `..`, NUL and other control characters, and anything over 255 bytes.
/// With `windows_rules`, additionally: the characters `< > : " | ? *`
/// (`:` would otherwise open an NTFS alternate data stream), and the
/// reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
/// `LPT1`–`LPT9`, with or without an extension). Trailing dots and spaces —
/// which Windows silently strips, defeating naive suffix checks — are
/// removed first, on every platform, so a name means the same thing on both.
pub fn sanitize_file_name(name: &str, windows_rules: bool) -> Result<String, &'static str> {
    if name.chars().any(|c| c == '/' || c == '\\') {
        return Err("file names may not contain path separators");
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("file names may not contain control characters");
    }
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed.trim().is_empty() {
        return Err("empty file name");
    }
    if name == "." || name == ".." {
        return Err("not a file name");
    }
    if trimmed.len() > MAX_FILE_NAME_LEN {
        return Err("file name too long");
    }
    if windows_rules {
        if trimmed
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Err("file name contains a character Windows does not allow");
        }
        let stem = trimmed.split('.').next().unwrap_or("").trim_end();
        let upper = stem.to_ascii_uppercase();
        let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((upper.starts_with("COM") || upper.starts_with("LPT"))
                && upper.len() == 4
                && upper.as_bytes()[3].is_ascii_digit()
                && upper.as_bytes()[3] != b'0');
        if reserved {
            return Err("file name is a reserved Windows device name");
        }
    }
    Ok(trimmed.to_owned())
}

/// `name` → `name (n)`, keeping the extension: `a.txt` → `a (2).txt`.
pub fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// The candidate final paths for `name` in `dir`, in the order to try them.
pub fn candidate_paths(dir: &Path, name: &str) -> impl Iterator<Item = PathBuf> {
    let dir = dir.to_path_buf();
    let name = name.to_owned();
    std::iter::once(dir.join(&name))
        .chain((1..1000).map(move |n| dir.join(numbered_name(&name, n))))
}

/// Give a completed temp file its final name without ever overwriting: a
/// hard link fails if the target exists, so the existence check and the
/// claim are one atomic step. Filesystems without hard links (FAT/exFAT
/// drives) fall back to check-then-rename.
pub fn publish(tmp: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    for candidate in candidate_paths(dir, name) {
        match std::fs::hard_link(tmp, &candidate) {
            Ok(()) => {
                let _ = std::fs::remove_file(tmp);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                if candidate.exists() {
                    continue;
                }
                std::fs::rename(tmp, &candidate)?;
                return Ok(candidate);
            }
        }
    }
    Err(std::io::Error::other("too many files with that name"))
}

/// Free bytes on the volume holding `dir`, if the platform can tell us.
pub fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::core::PCWSTR;
        let wide = crate::win::wide(&dir.to_string_lossy());
        let mut free = 0u64;
        // SAFETY: NUL-terminated path; out-pointer to a live local.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }.ok()?;
        Some(free)
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: NUL-terminated path; out-pointer to a zeroed local.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        Some(st.f_bavail as u64 * st.f_frsize as u64)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = dir;
        None
    }
}

/// Delete upload temp files a crash or kill left behind (graceful paths
/// clean up their own). Only `.tether-*.part` older than a few minutes, so a
/// transfer in progress in another session is never touched.
pub fn sweep_stale_parts(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(".tether-") && name.ends_with(".part")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(600));
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            info!(file = %entry.path().display(), "removed a stale partial upload");
        }
    }
}

/// Hide a temp file in Explorer (dot-files are already hidden elsewhere).
#[cfg(windows)]
fn hide(path: &Path) {
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
    use windows::core::PCWSTR;
    let wide = crate::win::wide(&path.to_string_lossy());
    // SAFETY: NUL-terminated path; failure only leaves the file visible.
    unsafe {
        let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN);
    }
}

#[cfg(not(windows))]
fn hide(_path: &Path) {}

// ---- the per-session engine ----------------------------------------------

struct Upload {
    name: String,
    size: u64,
    received: u64,
    next_seq: u32,
    tmp: PathBuf,
    file: Option<tokio::fs::File>,
}

struct Download {
    cancel: Arc<AtomicBool>,
    /// Set by the sender task when it is done (any outcome), so the entry
    /// is pruned — the controller sends nothing after a successful download.
    finished: Arc<AtomicBool>,
    path: PathBuf,
    size: u64,
    started: bool,
}

pub struct FileTransfers {
    config: Arc<FileConfig>,
    peer: String,
    control: mpsc::UnboundedSender<Message>,
    data: mpsc::Sender<Message>,
    uploads: HashMap<u32, Upload>,
    downloads: HashMap<u32, Download>,
}

impl FileTransfers {
    pub fn new(config: Arc<FileConfig>, peer: impl Into<String>) -> (Self, Outbound) {
        let (control, control_rx) = mpsc::unbounded_channel();
        let (data, data_rx) = mpsc::channel(DATA_QUEUE);
        (
            FileTransfers {
                config,
                peer: peer.into(),
                control,
                data,
                uploads: HashMap::new(),
                downloads: HashMap::new(),
            },
            Outbound {
                control: control_rx,
                data: data_rx,
            },
        )
    }

    fn reply(&self, msg: Message) {
        let _ = self.control.send(msg);
    }

    fn end(&self, transfer_id: u32, status: FileStatus) {
        self.reply(Message::FileEnd(FileEnd {
            transfer_id,
            status,
        }));
    }

    fn refuse(&self, transfer_id: u32, reason: &str) {
        self.reply(Message::FileAccept(FileAccept {
            transfer_id,
            ok: false,
            reason: reason.into(),
        }));
    }

    /// Feed one file-transfer message from the controller.
    pub async fn handle(&mut self, msg: Message) {
        self.downloads
            .retain(|_, d| !d.finished.load(Ordering::Relaxed));
        match msg {
            Message::FileOffer(offer) => self.on_offer(offer).await,
            Message::FileChunk(chunk) => self.on_chunk(chunk).await,
            Message::FileEnd(end) => self.on_end(end).await,
            Message::FileRequest(req) => self.on_request(req).await,
            Message::FileAccept(accept) => self.on_accept(accept),
            _ => {}
        }
    }

    async fn on_offer(&mut self, offer: FileOffer) {
        let id = offer.transfer_id;
        if self.uploads.contains_key(&id) || self.downloads.contains_key(&id) {
            return self.refuse(id, "duplicate transfer id");
        }
        if self.uploads.len() >= MAX_ACTIVE {
            return self.refuse(id, "too many uploads in progress");
        }
        let name = match sanitize_file_name(&offer.name, cfg!(windows)) {
            Ok(n) => n,
            Err(reason) => {
                warn!(peer = %self.peer, name = ?offer.name, reason, "refused upload: bad name");
                return self.refuse(id, reason);
            }
        };
        if offer.size > self.config.max_upload_bytes {
            return self.refuse(
                id,
                &format!(
                    "file is larger than this host accepts ({} MB max)",
                    self.config.max_upload_bytes / (1024 * 1024)
                ),
            );
        }
        if let Err(e) = tokio::fs::create_dir_all(&self.config.dir).await {
            warn!(error = %e, dir = %self.config.dir.display(), "upload dir unavailable");
            return self.refuse(id, "the host's download folder is unavailable");
        }
        if let Some(free) = free_space(&self.config.dir)
            && free < offer.size.saturating_add(FREE_SPACE_MARGIN)
        {
            return self.refuse(id, "not enough free disk space on the host");
        }
        let mut nonce = [0u8; 6];
        getrandom::fill(&mut nonce).expect("system RNG must not fail");
        let tmp = self
            .config
            .dir
            .join(format!(".tether-{}.part", hex::encode(nonce)));
        let file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "could not create upload temp file");
                return self.refuse(id, "the host could not create the file");
            }
        };
        hide(&tmp);
        info!(peer = %self.peer, id, name = %name, size = offer.size, "upload accepted");
        self.uploads.insert(
            id,
            Upload {
                name,
                size: offer.size,
                received: 0,
                next_seq: 0,
                tmp,
                file: Some(file),
            },
        );
        self.reply(Message::FileAccept(FileAccept {
            transfer_id: id,
            ok: true,
            reason: String::new(),
        }));
    }

    async fn on_chunk(&mut self, chunk: FileChunk) {
        let id = chunk.transfer_id;
        let Some(up) = self.uploads.get_mut(&id) else {
            return; // unknown or already-finished transfer: drop
        };
        let bad = if chunk.seq != up.next_seq {
            Some("chunk out of order")
        } else if up.received + chunk.data.len() as u64 > up.size {
            Some("more data than offered")
        } else {
            None
        };
        let write = match (bad, up.file.as_mut()) {
            (None, Some(f)) => f.write_all(&chunk.data).await.err(),
            _ => None,
        };
        if let Some(reason) = bad {
            warn!(peer = %self.peer, id, reason, "upload failed");
            self.fail_upload(id).await;
        } else if let Some(e) = write {
            warn!(peer = %self.peer, id, error = %e, "upload write failed");
            self.fail_upload(id).await;
        } else if let Some(up) = self.uploads.get_mut(&id) {
            up.received += chunk.data.len() as u64;
            up.next_seq = up.next_seq.wrapping_add(1);
        }
    }

    async fn fail_upload(&mut self, id: u32) {
        if let Some(mut up) = self.uploads.remove(&id) {
            drop(up.file.take());
            let _ = tokio::fs::remove_file(&up.tmp).await;
        }
        self.end(id, FileStatus::Error);
    }

    async fn on_end(&mut self, end: FileEnd) {
        let id = end.transfer_id;
        if let Some(dl) = self.downloads.remove(&id) {
            // The controller cancelled (or failed) a download.
            dl.cancel.store(true, Ordering::Relaxed);
            info!(peer = %self.peer, id, path = %dl.path.display(), status = ?end.status, "download ended by controller");
            return;
        }
        let Some(mut up) = self.uploads.remove(&id) else {
            return;
        };
        let mut file = up.file.take();
        if end.status != FileStatus::Complete {
            drop(file);
            let _ = tokio::fs::remove_file(&up.tmp).await;
            info!(peer = %self.peer, id, name = %up.name, status = ?end.status, "upload abandoned");
            return;
        }
        let flushed = match file.as_mut() {
            Some(f) => f.sync_all().await.is_ok(),
            None => false,
        };
        drop(file);
        if !flushed || up.received != up.size {
            warn!(peer = %self.peer, id, received = up.received, size = up.size, "upload incomplete");
            let _ = tokio::fs::remove_file(&up.tmp).await;
            return self.end(id, FileStatus::Error);
        }
        let (tmp, dir, name) = (up.tmp.clone(), self.config.dir.clone(), up.name.clone());
        match tokio::task::spawn_blocking(move || publish(&tmp, &dir, &name)).await {
            Ok(Ok(path)) => {
                info!(peer = %self.peer, id, path = %path.display(), size = up.size, "upload saved");
                self.end(id, FileStatus::Complete);
            }
            other => {
                warn!(peer = %self.peer, id, result = ?other.map(|r| r.map(|_| ())), "upload rename failed");
                let _ = tokio::fs::remove_file(&up.tmp).await;
                self.end(id, FileStatus::Error);
            }
        }
    }

    async fn on_request(&mut self, req: FileRequest) {
        let id = req.transfer_id;
        if self.uploads.contains_key(&id) || self.downloads.contains_key(&id) {
            return self.end(id, FileStatus::Error);
        }
        if self.downloads.len() >= MAX_ACTIVE {
            return self.end(id, FileStatus::Error);
        }
        let path = PathBuf::from(req.path.trim().trim_matches('"'));
        info!(peer = %self.peer, id, path = %path.display(), "download requested");
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() => m,
            Ok(_) => {
                warn!(peer = %self.peer, id, path = %path.display(), "download refused: not a file");
                return self.end(id, FileStatus::Error);
            }
            Err(e) => {
                warn!(peer = %self.peer, id, path = %path.display(), error = %e, "download refused");
                return self.end(id, FileStatus::Error);
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        let name = truncate_utf8(&name, MAX_FILE_NAME_LEN);
        self.downloads.insert(
            id,
            Download {
                cancel: Arc::new(AtomicBool::new(false)),
                finished: Arc::new(AtomicBool::new(false)),
                path,
                size: meta.len(),
                started: false,
            },
        );
        self.reply(Message::FileOffer(FileOffer {
            transfer_id: id,
            size: meta.len(),
            name,
        }));
    }

    fn on_accept(&mut self, accept: FileAccept) {
        let id = accept.transfer_id;
        if !accept.ok {
            if let Some(dl) = self.downloads.remove(&id) {
                info!(peer = %self.peer, id, path = %dl.path.display(), reason = %accept.reason, "download declined by controller");
            }
            return;
        }
        let Some(dl) = self.downloads.get_mut(&id) else {
            return;
        };
        if dl.started {
            return;
        }
        dl.started = true;
        let (path, size, cancel, finished) = (
            dl.path.clone(),
            dl.size,
            dl.cancel.clone(),
            dl.finished.clone(),
        );
        let (data, control, peer) = (self.data.clone(), self.control.clone(), self.peer.clone());
        tokio::spawn(async move {
            let status = send_file(id, &path, size, &cancel, &data).await;
            match status {
                FileStatus::Complete => {
                    info!(%peer, id, path = %path.display(), size, "download sent");
                }
                other => {
                    warn!(%peer, id, path = %path.display(), status = ?other, "download stopped")
                }
            }
            // Free the slot before announcing the end, so a controller that
            // reacts to FileEnd with its next request is never refused.
            finished.store(true, Ordering::Relaxed);
            // Cancelled by the controller: it already knows, say nothing.
            if status != FileStatus::Cancelled {
                // After the last chunk, on the data queue so it can't overtake them.
                let end = Message::FileEnd(FileEnd {
                    transfer_id: id,
                    status,
                });
                if data.send(end.clone()).await.is_err() {
                    let _ = control.send(end);
                }
            }
        });
    }
}

async fn send_file(
    id: u32,
    path: &Path,
    size: u64,
    cancel: &AtomicBool,
    out: &mpsc::Sender<Message>,
) -> FileStatus {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return FileStatus::Error;
    };
    let mut sent: u64 = 0;
    let mut seq: u32 = 0;
    let mut buf = vec![0u8; FILE_CHUNK_LEN];
    while sent < size {
        if cancel.load(Ordering::Relaxed) {
            return FileStatus::Cancelled;
        }
        let want = (size - sent).min(FILE_CHUNK_LEN as u64) as usize;
        let n = match file.read(&mut buf[..want]).await {
            Ok(0) | Err(_) => return FileStatus::Error, // shrank, or unreadable
            Ok(n) => n,
        };
        let chunk = Message::FileChunk(FileChunk {
            transfer_id: id,
            seq,
            data: Bytes::copy_from_slice(&buf[..n]),
        });
        if out.send(chunk).await.is_err() {
            return FileStatus::Cancelled; // session gone
        }
        sent += n as u64;
        seq = seq.wrapping_add(1);
    }
    FileStatus::Complete
}

fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

impl Drop for FileTransfers {
    /// Session over: abandon everything in flight and delete partial files.
    fn drop(&mut self) {
        for (_, dl) in self.downloads.drain() {
            dl.cancel.store(true, Ordering::Relaxed);
        }
        for (_, mut up) in self.uploads.drain() {
            drop(up.file.take());
            let _ = std::fs::remove_file(&up.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_names_are_refused_or_sanitised() {
        // (input, windows result, unix result); None = refused
        let table: &[(&str, Option<&str>, Option<&str>)] = &[
            ("report.pdf", Some("report.pdf"), Some("report.pdf")),
            ("../../etc/passwd", None, None),
            ("..", None, None),
            (".", None, None),
            ("", None, None),
            ("   ", None, None),
            ("a/b.txt", None, None),
            ("a\\b.txt", None, None),
            ("C:\\Windows\\win.ini", None, None),
            ("/etc/shadow", None, None),
            ("nul\0byte", None, None),
            ("bell\x07", None, None),
            ("new\nline", None, None),
            ("trailing. . .", Some("trailing"), Some("trailing")),
            ("evil.exe.", Some("evil.exe"), Some("evil.exe")),
            ("evil.exe   ", Some("evil.exe"), Some("evil.exe")),
            ("...", None, None),
            ("CON", None, Some("CON")),
            ("con.txt", None, Some("con.txt")),
            ("Aux.tar.gz", None, Some("Aux.tar.gz")),
            ("nul", None, Some("nul")),
            ("COM1", None, Some("COM1")),
            ("lpt9.log", None, Some("lpt9.log")),
            ("COM0", Some("COM0"), Some("COM0")),
            ("CONSOLE.txt", Some("CONSOLE.txt"), Some("CONSOLE.txt")),
            ("file:stream", None, Some("file:stream")),
            ("what?.txt", None, Some("what?.txt")),
            ("pipe|name", None, Some("pipe|name")),
            ("emoji 🎯.png", Some("emoji 🎯.png"), Some("emoji 🎯.png")),
            (".hidden", Some(".hidden"), Some(".hidden")),
        ];
        for &(input, win, unix) in table {
            assert_eq!(
                sanitize_file_name(input, true).ok().as_deref(),
                win,
                "windows rules: {input:?}"
            );
            assert_eq!(
                sanitize_file_name(input, false).ok().as_deref(),
                unix,
                "unix rules: {input:?}"
            );
        }
        let long = "a".repeat(256);
        assert!(sanitize_file_name(&long, false).is_err());
        assert!(sanitize_file_name(&"a".repeat(255), true).is_ok());
    }

    #[test]
    fn collisions_number_before_the_extension() {
        assert_eq!(numbered_name("a.txt", 1), "a (1).txt");
        assert_eq!(numbered_name("archive.tar.gz", 2), "archive.tar (2).gz");
        assert_eq!(numbered_name("README", 3), "README (3)");
        assert_eq!(numbered_name(".bashrc", 1), ".bashrc (1)");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let mut buf = [0u8; 6];
        getrandom::fill(&mut buf).unwrap();
        let d = std::env::temp_dir().join(format!("tether-files-{tag}-{}", hex::encode(buf)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn publish_never_overwrites() {
        let dir = tmpdir("publish");
        std::fs::write(dir.join("a.txt"), b"original").unwrap();
        let tmp = dir.join(".t1.part");
        std::fs::write(&tmp, b"new").unwrap();
        let path = publish(&tmp, &dir, "a.txt").unwrap();
        assert_eq!(path, dir.join("a (1).txt"));
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"original");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!tmp.exists(), "temp file must be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn engine(dir: &Path, max: u64) -> (FileTransfers, Outbound) {
        FileTransfers::new(
            Arc::new(FileConfig {
                dir: dir.to_path_buf(),
                max_upload_bytes: max,
            }),
            "test",
        )
    }

    fn offer(id: u32, name: &str, size: u64) -> Message {
        Message::FileOffer(FileOffer {
            transfer_id: id,
            size,
            name: name.into(),
        })
    }

    fn chunk(id: u32, seq: u32, data: &[u8]) -> Message {
        Message::FileChunk(FileChunk {
            transfer_id: id,
            seq,
            data: Bytes::copy_from_slice(data),
        })
    }

    fn end(id: u32, status: FileStatus) -> Message {
        Message::FileEnd(FileEnd {
            transfer_id: id,
            status,
        })
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn upload_lands_byte_identical_under_its_name() {
        let dir = tmpdir("upload");
        let (mut eng, mut out) = engine(&dir, 1 << 20);
        eng.handle(offer(1, "hello.txt", 11)).await;
        assert!(matches!(
            out.control.recv().await,
            Some(Message::FileAccept(FileAccept { ok: true, .. }))
        ));
        eng.handle(chunk(1, 0, b"hello ")).await;
        eng.handle(chunk(1, 1, b"world")).await;
        eng.handle(end(1, FileStatus::Complete)).await;
        assert_eq!(out.control.recv().await, Some(end(1, FileStatus::Complete)));
        assert_eq!(
            std::fs::read(dir.join("hello.txt")).unwrap(),
            b"hello world"
        );
        assert_eq!(files_in(&dir), vec!["hello.txt"], "no temp file left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancelled_short_and_dropped_uploads_leave_nothing() {
        let dir = tmpdir("cancel");
        let (mut eng, mut out) = engine(&dir, 1 << 20);

        // cancelled mid-way
        eng.handle(offer(1, "a.bin", 10)).await;
        eng.handle(chunk(1, 0, b"12345")).await;
        eng.handle(end(1, FileStatus::Cancelled)).await;
        // claims complete but is short
        eng.handle(offer(2, "b.bin", 10)).await;
        eng.handle(chunk(2, 0, b"123")).await;
        eng.handle(end(2, FileStatus::Complete)).await;
        // out-of-order chunk
        eng.handle(offer(3, "c.bin", 10)).await;
        eng.handle(chunk(3, 1, b"123")).await;
        // more bytes than offered
        eng.handle(offer(4, "d.bin", 2)).await;
        eng.handle(chunk(4, 0, b"123")).await;
        // still in flight when the session ends
        eng.handle(offer(5, "e.bin", 10)).await;
        eng.handle(chunk(5, 0, b"1")).await;
        drop(eng);

        assert!(
            files_in(&dir).is_empty(),
            "left behind: {:?}",
            files_in(&dir)
        );
        let mut ends = Vec::new();
        while let Ok(m) = out.control.try_recv() {
            if let Message::FileEnd(e) = m {
                ends.push((e.transfer_id, e.status));
            }
        }
        assert!(ends.contains(&(2, FileStatus::Error)));
        assert!(ends.contains(&(3, FileStatus::Error)));
        assert!(ends.contains(&(4, FileStatus::Error)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oversized_and_badly_named_offers_are_refused_with_reasons() {
        let dir = tmpdir("refuse");
        let (mut eng, mut out) = engine(&dir, 100);
        eng.handle(offer(1, "big.iso", 101)).await;
        eng.handle(offer(2, "../escape.sh", 1)).await;
        eng.handle(offer(3, "huge.bin", 100)).await; // within cap: accepted
        for (id, ok) in [(1, false), (2, false), (3, true)] {
            match out.control.recv().await {
                Some(Message::FileAccept(a)) => {
                    assert_eq!((a.transfer_id, a.ok), (id, ok));
                    assert_eq!(a.reason.is_empty(), ok, "refusals carry a reason");
                }
                other => panic!("expected FileAccept, got {other:?}"),
            }
        }
        drop(eng);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_streams_chunks_then_end() {
        let dir = tmpdir("download");
        let content: Vec<u8> = (0..(FILE_CHUNK_LEN * 2 + 100)).map(|i| i as u8).collect();
        let src = dir.join("source.bin");
        std::fs::write(&src, &content).unwrap();
        let (mut eng, mut out) = engine(&dir, 0);

        eng.handle(Message::FileRequest(FileRequest {
            transfer_id: 9,
            path: src.to_string_lossy().into_owned(),
        }))
        .await;
        match out.control.recv().await {
            Some(Message::FileOffer(o)) => {
                assert_eq!(
                    (o.transfer_id, o.size, o.name.as_str()),
                    (9, content.len() as u64, "source.bin")
                );
            }
            other => panic!("expected FileOffer, got {other:?}"),
        }
        eng.handle(Message::FileAccept(FileAccept {
            transfer_id: 9,
            ok: true,
            reason: String::new(),
        }))
        .await;
        let mut got = Vec::new();
        let mut next_seq = 0;
        loop {
            match out.data.recv().await {
                Some(Message::FileChunk(c)) => {
                    assert_eq!(c.seq, next_seq);
                    next_seq += 1;
                    got.extend_from_slice(&c.data);
                }
                Some(Message::FileEnd(e)) => {
                    assert_eq!(e.status, FileStatus::Complete);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(got, content);

        // finished downloads free their slot: a session can do more than
        // MAX_ACTIVE of them back to back
        for round in 0..(MAX_ACTIVE as u32 + 2) {
            let id = 100 + round;
            eng.handle(Message::FileRequest(FileRequest {
                transfer_id: id,
                path: src.to_string_lossy().into_owned(),
            }))
            .await;
            assert!(
                matches!(out.control.recv().await, Some(Message::FileOffer(_))),
                "download {round} refused"
            );
            eng.handle(Message::FileAccept(FileAccept {
                transfer_id: id,
                ok: true,
                reason: String::new(),
            }))
            .await;
            loop {
                if let Some(Message::FileEnd(e)) = out.data.recv().await {
                    assert_eq!(e.status, FileStatus::Complete);
                    break;
                }
            }
            tokio::task::yield_now().await;
        }

        // a missing path is an immediate FileEnd error, no offer
        eng.handle(Message::FileRequest(FileRequest {
            transfer_id: 10,
            path: dir.join("nope").to_string_lossy().into_owned(),
        }))
        .await;
        assert_eq!(out.control.recv().await, Some(end(10, FileStatus::Error)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_dir_access_is_probed_and_classified() {
        let dir = tmpdir("access");
        assert_eq!(check_upload_dir(&dir), DirAccess::Ok);
        assert_eq!(files_in(&dir), Vec::<String>::new(), "probe file removed");
        assert_eq!(
            check_upload_dir(&dir.join("not-yet")),
            DirAccess::Missing,
            "uploads create a missing folder"
        );
        let file = dir.join("plain");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(check_upload_dir(&file), DirAccess::Error);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = dir.join("read-only");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            // root ignores mode bits; only meaningful for a normal user.
            // SAFETY: geteuid has no preconditions and cannot fail.
            if unsafe { libc::geteuid() } != 0 {
                assert_eq!(check_upload_dir(&ro), DirAccess::Denied);
            }
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(DirAccess::from_probe(&Err(denied)), DirAccess::Denied);
        // What macOS TCC returns for a denied Downloads folder (raw 1 is
        // EPERM only on Unix; on Windows it is ERROR_INVALID_FUNCTION).
        #[cfg(unix)]
        {
            let eperm = std::io::Error::from_raw_os_error(libc::EPERM);
            assert_eq!(DirAccess::from_probe(&Err(eperm)), DirAccess::Denied);
        }
        assert!(!DirAccess::Denied.advice().is_empty());
        assert!(DirAccess::Ok.advice().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_access_has_a_stable_json_shape() {
        assert_eq!(serde_json::to_string(&DirAccess::Ok).unwrap(), r#""ok""#);
        assert_eq!(
            serde_json::to_string(&DirAccess::TimedOut).unwrap(),
            r#""timed_out""#
        );
        assert_eq!(
            serde_json::from_str::<DirAccess>(r#""denied""#).unwrap(),
            DirAccess::Denied
        );
        assert_eq!(
            serde_json::from_str::<DirAccess>(r#""some_future_state""#).unwrap(),
            DirAccess::Unknown,
            "an older CLI must not choke on a newer daemon"
        );
    }
}
