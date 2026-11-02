//! Tether host daemon library. The binary in `main.rs` is a thin wrapper;
//! everything lives here so integration tests can drive a real server.

pub mod adaptive;
pub mod audio;
pub mod auth;
pub mod capture;
pub mod clipboard;
pub mod config;
pub mod control;
pub mod encode;
pub mod files;
pub mod input;
#[cfg(target_os = "macos")]
pub mod macos_perms;
pub mod pipeline;
pub mod server;
pub mod session;
pub mod webrtc;
#[cfg(windows)]
pub mod win;
