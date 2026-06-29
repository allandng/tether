//! End-to-end pipeline test with a synthetic capturer: real JPEG encoding,
//! real pipeline thread, real WebSocket server, real client. Proves the whole
//! stack except the two TCC-gated pieces (ScreenCaptureKit, CGEvent post),
//! including the ≥15 fps gate criterion at the transport level.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tether_protocol::{
    Auth, CAP_CAN_CONTROL, Codec, Decoded, Hello, Message, PROTOCOL_VERSION, Resolution, Role,
};
use tetherd::capture::{RawFrame, ScreenCapturer};
use tetherd::encode::JpegEncoder;
use tetherd::pipeline;
use tetherd::server::{Server, ServerState};

/// Paces itself to ~30 fps and draws a moving gradient so successive frames
/// differ (as real capture does).
struct SyntheticCapturer {
    width: u32,
    height: u32,
    tick: u32,
}

impl ScreenCapturer for SyntheticCapturer {
    fn resolution(&self) -> Resolution {
        Resolution { width: self.width, height: self.height }
    }

    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        std::thread::sleep(Duration::from_millis(33));
        self.tick = self.tick.wrapping_add(7);
        let (w, h) = (self.width as usize, self.height as usize);
        let bytes_per_row = w * 4;
        let mut bgra = vec![255u8; bytes_per_row * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * bytes_per_row + x * 4;
                bgra[o] = ((x + self.tick as usize) % 256) as u8;
                bgra[o + 1] = ((y + self.tick as usize) % 256) as u8;
                bgra[o + 2] = 64;
            }
        }
        Ok(RawFrame {
            width: self.width,
            height: self.height,
            bytes_per_row,
            bgra,
            timestamp_micros: self.tick as u64,
        })
    }
}

#[tokio::test]
async fn full_pipeline_sustains_gate_framerate() {
    let pipeline = pipeline::start(
        || Ok(SyntheticCapturer { width: 640, height: 400, tick: 0 }),
        || JpegEncoder::new(75),
    )
    .expect("pipeline start");

    let (input_tx, _input_rx) = mpsc::channel(8);
    // senders must outlive the test or sessions treat the sources as closed
    let (_clipboard_out_tx, clipboard_out_rx) = tokio::sync::watch::channel(None);
    let (clipboard_in_tx, _clipboard_in_rx) = std::sync::mpsc::channel::<String>();
    let server = Server::bind(
        "127.0.0.1".parse().unwrap(),
        0,
        vec!["127.0.0.1".parse().unwrap()],
        ServerState {
            resolution: pipeline.resolution,
            frames: pipeline.frames,
            input_tx,
            clipboard_out: clipboard_out_rx,
            clipboard_in: clipboard_in_tx,
            auth: std::sync::Arc::new(tokio::sync::Mutex::new(
                tetherd::auth::PairingAuth::load_or_create(&std::env::temp_dir().join(format!(
                    "tether-e2e-auth-{}",
                    std::process::id()
                )))
                .expect("auth"),
            )),
            // gate off: this test exercises the streaming pipeline, not pairing
            auth_policy: tetherd::server::AuthPolicy { require_pairing: false, allow_unpaired: true },
            bitrate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            bitrate_ceiling_kbps: 4000,
            session_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .await
    .expect("bind");
    let addr = server.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");
    ws.send(WsMessage::Binary(
        Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            role: Role::Controller,
            capabilities: CAP_CAN_CONTROL,
        })
        .encode(),
    ))
    .await
    .unwrap();
    // auth gate is off (allow_unpaired) but the controller still sends Auth
    ws.send(WsMessage::Binary(
        Message::Auth(Auth { device_id: "e2e".into(), token: String::new() }).encode(),
    ))
    .await
    .unwrap();

    let mut frames = 0u32;
    let mut first_payload = None;
    let mut resolution = None;
    let started = Instant::now();
    let window = Duration::from_secs(2);

    while started.elapsed() < window {
        let msg = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("host stalled")
            .expect("stream ended")
            .expect("ws error");
        let WsMessage::Binary(bytes) = msg else { continue };
        match Message::decode(&bytes).expect("decode") {
            Decoded::Message { message: Message::FrameData(f), .. } => {
                assert_eq!(f.codec, Codec::Jpeg);
                frames += 1;
                first_payload.get_or_insert(f.payload);
            }
            Decoded::Message { message: Message::Resolution(r), .. } => resolution = Some(r),
            Decoded::Message { message: Message::Hello(_), .. } => {}
            Decoded::Message { message: Message::AuthResult(_), .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    server_task.abort();

    assert_eq!(
        resolution,
        Some(Resolution { width: 640, height: 400 }),
        "resolution must be announced"
    );

    let fps = frames as f64 / window.as_secs_f64();
    assert!(fps >= 15.0, "gate criterion: ≥15 fps, measured {fps:.1}");

    // The payload must be a real, decodable JPEG of the announced size.
    let payload = first_payload.expect("at least one frame");
    let header = turbojpeg::read_header(&payload).expect("valid JPEG");
    assert_eq!((header.width, header.height), (640, 400));
}
