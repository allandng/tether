//! Tether host daemon library. The binary in `main.rs` is a thin wrapper;
//! everything lives here so integration tests can drive a real server.

pub mod capture;
pub mod clipboard;
pub mod config;
pub mod encode;
pub mod input;
pub mod pipeline;
pub mod server;
pub mod session;
pub mod webrtc;
