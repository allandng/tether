//! `tetherd-agent` — the Windows logon launcher (Phase 7 M6 / Phase 9).
//!
//! The logon Scheduled Task runs this instead of `tetherd` directly. It is a
//! GUI-subsystem program, so nothing flashes on screen at sign-in, and it
//! starts `tetherd.exe` (same folder) with no console window, copying its
//! output into `%LOCALAPPDATA%\Tether\tetherd.log` (rotated at 10 MB). If
//! tetherd exits it is restarted with backoff — the LaunchAgent `KeepAlive`
//! equivalent — and a job object ends it whenever the agent ends.
//!
//! Any arguments are passed through to tetherd; with none, tetherd reads
//! everything (signal server, secret, codec, …) from its config file, so no
//! secret ever appears in a command line.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe().expect("own path");
    let tetherd = exe.with_file_name("tetherd.exe");
    let log_dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Tether");
    let _ = std::fs::create_dir_all(&log_dir);
    let log = Arc::new(Mutex::new(log::RotatingLog::new(log_dir)));
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    // tetherd lives exactly as long as this agent: if the agent is ended
    // (Task Scheduler "End", sign-out), the job closes and kills it too, so
    // a restart never finds an orphaned second daemon.
    let job = job::kill_on_close();

    let mut backoff = Duration::from_secs(2);
    loop {
        let mut cmd = std::process::Command::new(&tetherd);
        cmd.args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .env("NO_COLOR", "1") // no ANSI colour codes in a log file
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let status = match cmd.spawn() {
            Ok(mut child) => {
                if let Some(job) = &job {
                    job.assign(child.as_raw_handle());
                }
                // Copy output through us so the log rotates while tetherd runs.
                let pumps: Vec<_> = [
                    child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                    child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                ]
                .into_iter()
                .flatten()
                .map(|stream| {
                    let log = Arc::clone(&log);
                    std::thread::spawn(move || log::pump(stream, &log))
                })
                .collect();
                let status = child.wait();
                for p in pumps {
                    let _ = p.join();
                }
                status.ok()
            }
            Err(e) => {
                log.lock().unwrap().line(&format!(
                    "tetherd-agent: cannot start {}: {e}",
                    tetherd.display()
                ));
                None
            }
        };
        if status.is_some_and(|s| s.success()) {
            return; // clean stop
        }
        // Crashed or failed to start. A run that lasted a while resets the
        // backoff; a crash loop (bad config, no display yet) slows down.
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(2);
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

#[cfg(windows)]
mod log {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Start a fresh file past this size; one previous file is kept.
    const MAX_LOG: u64 = 10 * 1024 * 1024;

    pub struct RotatingLog {
        dir: PathBuf,
        file: Option<std::fs::File>,
        written: u64,
    }

    impl RotatingLog {
        pub fn new(dir: PathBuf) -> Self {
            RotatingLog {
                dir,
                file: None,
                written: 0,
            }
        }

        pub fn line(&mut self, text: &str) {
            let path = self.dir.join("tetherd.log");
            if self.file.is_none() {
                self.written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .ok();
            }
            if self.written > MAX_LOG {
                self.file = None;
                let _ = std::fs::rename(&path, self.dir.join("tetherd.log.old"));
                self.file = std::fs::File::create(&path).ok();
                self.written = 0;
            }
            if let Some(f) = &mut self.file {
                let _ = writeln!(f, "{text}");
                self.written += text.len() as u64 + 1;
            }
        }
    }

    pub fn pump(stream: Box<dyn Read + Send>, log: &Mutex<RotatingLog>) {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            log.lock().unwrap().line(&line);
        }
    }
}

#[cfg(windows)]
mod job {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub struct Job(HANDLE);

    /// A job object that kills its processes when its last handle closes —
    /// i.e. when this agent exits, however it exits.
    pub fn kill_on_close() -> Option<Job> {
        // SAFETY: anonymous job; the info struct is fully initialised and
        // sized; the handle is intentionally held for the process lifetime.
        unsafe {
            let job = CreateJobObjectW(None, None).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(Job(job))
        }
    }

    impl Job {
        pub fn assign(&self, process: std::os::windows::io::RawHandle) {
            // SAFETY: a live child-process handle owned by std's Child.
            unsafe {
                let _ = AssignProcessToJobObject(self.0, HANDLE(process));
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("tetherd-agent is the Windows logon launcher; on macOS use the LaunchAgent.");
    std::process::exit(2);
}
