//! Tether signaling server: a presence directory and SDP/ICE relay. It never
//! carries media — WebRTC's DTLS gives the media path end-to-end encryption;
//! this server only introduces peers.

pub mod protocol;
pub mod server;
