//! Full WebRTC end-to-end: a real tether-signal server, tetherd's run_host,
//! and a controller-side peer built with webrtc-rs the way the browser
//! builds its (same channel labels/options, same chunk format). Proves
//! signaling, ICE over loopback, DTLS, both data channels, the Hello
//! handshake, chunked frame delivery, and the input path — everything but a
//! literal browser.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use tether_protocol::{
    Auth, AuthResult, CAP_CAN_CONTROL, ClipboardData, Codec, Decoded, Hello, InputEvent, Message,
    PROTOCOL_VERSION, Resolution, Role, TextInput,
};
use tether_signal::protocol::{Caps, ClientMessage, ServerMessage};
use tetherd::capture::{EncodedFrame, RawFrame, ScreenCapturer};
use tetherd::encode::JpegEncoder;
use tetherd::input::InjectCommand;
use tetherd::server::ServerState;
use tetherd::webrtc::{FrameReassembler, RtcConfig, run_host};

const SECRET: &str = "e2e-secret";

struct SyntheticCapturer {
    tick: u32,
}

impl ScreenCapturer for SyntheticCapturer {
    fn resolution(&self) -> Resolution {
        Resolution { width: 640, height: 400 }
    }
    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        std::thread::sleep(Duration::from_millis(33));
        self.tick = self.tick.wrapping_add(7);
        let (w, h) = (640usize, 400usize);
        let mut bgra = vec![255u8; w * 4 * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * w * 4 + x * 4;
                bgra[o] = ((x + self.tick as usize) % 256) as u8;
                bgra[o + 1] = (y % 256) as u8;
                bgra[o + 2] = 64;
            }
        }
        Ok(RawFrame {
            width: 640,
            height: 400,
            bytes_per_row: w * 4,
            bgra,
            timestamp_micros: 1,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn webrtc_end_to_end_frames_and_input() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("tetherd=debug")
        .try_init();
    // --- signal server (in process)
    let signal_state = tether_signal::server::AppState::new(SECRET.into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let signal_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, tether_signal::server::router(signal_state)).await;
    });

    // --- host: synthetic pipeline + run_host
    let pipeline = tetherd::pipeline::start(
        || Ok(SyntheticCapturer { tick: 0 }),
        || JpegEncoder::new(75),
    )
    .expect("pipeline");
    let (input_tx, mut input_rx) = mpsc::channel(64);
    let (clipboard_out_tx, clipboard_out_rx) = tokio::sync::watch::channel(None);
    let (clipboard_in_tx, clipboard_in_rx) = std::sync::mpsc::channel::<String>();
    let state = ServerState {
        resolution: pipeline.resolution,
        frames: pipeline.frames,
        input_tx,
        clipboard_out: clipboard_out_rx,
        clipboard_in: clipboard_in_tx,
        auth: std::sync::Arc::new(tokio::sync::Mutex::new(
            tetherd::auth::PairingAuth::load_or_create(
                &std::env::temp_dir().join(format!("tether-wrtc-auth-{}", std::process::id())),
            )
            .expect("auth"),
        )),
        // gate off: this test exercises the data-channel transport, not pairing
        auth_policy: tetherd::server::AuthPolicy { require_pairing: false, allow_unpaired: true },
        bitrate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        bitrate_ceiling_kbps: 4000,
            session_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    tokio::spawn(run_host(
        RtcConfig {
            signal_url: format!("ws://{signal_addr}/ws"),
            secret: SECRET.into(),
            device_id: "mac".into(),
            device_name: "mac".into(),
            stun: vec![], // loopback: host candidates suffice
        },
        state,
    ));
    tokio::time::sleep(Duration::from_millis(300)).await; // let the host register

    // --- controller side, shaped exactly like the browser's
    let api = APIBuilder::new().build();
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default()).await.unwrap(),
    );
    let ctl = pc
        .create_data_channel("tether-ctl", Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() }))
        .await
        .unwrap();
    let media = pc
        .create_data_channel(
            "tether-media",
            Some(RTCDataChannelInit {
                ordered: Some(false),
                max_retransmits: Some(0),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Bytes>();
    ctl.on_message(Box::new(move |m| {
        let ctl_tx = ctl_tx.clone();
        Box::pin(async move {
            let _ = ctl_tx.send(m.data);
        })
    }));
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<Bytes>();
    media.on_message(Box::new(move |m| {
        let media_tx = media_tx.clone();
        Box::pin(async move {
            let _ = media_tx.send(m.data);
        })
    }));
    let bulk = pc
        .create_data_channel("tether-bulk", Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() }))
        .await
        .unwrap();
    let (bulk_tx, mut bulk_rx) = mpsc::unbounded_channel::<Bytes>();
    bulk.on_message(Box::new(move |m| {
        let bulk_tx = bulk_tx.clone();
        Box::pin(async move {
            let _ = bulk_tx.send(m.data);
        })
    }));
    {
        let ctl = ctl.clone();
        let opened = ctl.clone();
        ctl.on_open(Box::new(move || {
            Box::pin(async move {
                let hello = Message::Hello(Hello {
                    version: PROTOCOL_VERSION,
                    role: Role::Controller,
                    capabilities: CAP_CAN_CONTROL,
                })
                .encode();
                let _ = opened.send(&hello).await;
            })
        }));
    }

    // --- signaling from the controller side
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{signal_addr}/ws"))
        .await
        .unwrap();
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<ClientMessage>();
    tokio::spawn(async move {
        while let Some(m) = sig_rx.recv().await {
            let _ = ws_sink
                .send(WsMessage::Text(serde_json::to_string(&m).unwrap().into()))
                .await;
        }
    });
    {
        let sig_tx = sig_tx.clone();
        pc.on_ice_candidate(Box::new(move |c| {
            let sig_tx = sig_tx.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(init) = c.to_json() {
                        let _ = sig_tx.send(ClientMessage::Ice {
                            target: "mac".into(),
                            candidate: serde_json::to_string(&init).unwrap(),
                        });
                    }
                }
            })
        }));
    }

    sig_tx
        .send(ClientMessage::Register {
            device_id: "ipad".into(),
            name: "ipad".into(),
            caps: Caps { can_host: false, can_control: true },
            auth: SECRET.into(),
        })
        .unwrap();

    let offer = pc.create_offer(None).await.unwrap();
    pc.set_local_description(offer.clone()).await.unwrap();
    sig_tx
        .send(ClientMessage::Offer { target: "mac".into(), sdp: offer.sdp })
        .unwrap();

    // pump signaling until the answer + candidates are in
    let pc_sig = pc.clone();
    tokio::spawn(async move {
        while let Some(Ok(WsMessage::Text(text))) = ws_stream.next().await {
            match serde_json::from_str::<ServerMessage>(&text) {
                Ok(ServerMessage::Answer { sdp, .. }) => {
                    let desc = RTCSessionDescription::answer(sdp).unwrap();
                    let _ = pc_sig.set_remote_description(desc).await;
                }
                Ok(ServerMessage::Ice { candidate, .. }) => {
                    if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&candidate) {
                        let _ = pc_sig.add_ice_candidate(init).await;
                    }
                }
                _ => {}
            }
        }
    });

    let deadline = Duration::from_secs(15);

    // --- handshake over ctl: host Hello, then auth, then Resolution
    let first = tokio::time::timeout(deadline, ctl_rx.recv())
        .await
        .expect("timed out waiting for host hello")
        .expect("ctl closed");
    let Ok(Decoded::Message { message: Message::Hello(h), .. }) = Message::decode(&first) else {
        panic!("expected host Hello");
    };
    assert_eq!(h.role, Role::Host);

    // REGRESSION (Phase 5 critical review finding): media must NOT stream before
    // the ctl channel authenticates — give the pump a moment, then assert silence.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        media_rx.try_recv().is_err(),
        "media channel leaked frames before authentication"
    );

    // auth gate is off (allow_unpaired); send Auth and expect AuthResult{ok}
    ctl.send(&Message::Auth(Auth { device_id: "ipad".into(), token: String::new() }).encode())
        .await
        .unwrap();
    let ar = tokio::time::timeout(deadline, ctl_rx.recv()).await.unwrap().unwrap();
    assert!(matches!(
        Message::decode(&ar),
        Ok(Decoded::Message { message: Message::AuthResult(AuthResult { ok: true }), .. })
    ));

    let second = tokio::time::timeout(deadline, ctl_rx.recv()).await.unwrap().unwrap();
    let Ok(Decoded::Message { message: Message::Resolution(r), .. }) = Message::decode(&second)
    else {
        panic!("expected Resolution");
    };
    assert_eq!(r, Resolution { width: 640, height: 400 });

    // --- frames over media: reassemble chunks into a decodable FrameData
    let mut reassembler = FrameReassembler::default();
    let mut frame = None;
    let start = tokio::time::Instant::now();
    while start.elapsed() < deadline {
        let chunk = tokio::time::timeout(deadline, media_rx.recv())
            .await
            .expect("timed out waiting for media chunks")
            .expect("media closed");
        if let Some(wire) = reassembler.on_chunk(&chunk) {
            let Ok(Decoded::Message { message: Message::FrameData(f), .. }) =
                Message::decode(&wire)
            else {
                panic!("reassembled wire was not FrameData");
            };
            frame = Some(f);
            break;
        }
    }
    let frame = frame.expect("no complete frame within deadline");
    assert_eq!(frame.codec, Codec::Jpeg);
    let header = turbojpeg::read_header(&frame.payload).expect("valid JPEG");
    assert_eq!((header.width, header.height), (640, 400));

    // --- input path: controller -> ctl channel -> injector queue
    let ev = InputEvent::KeyDown { code: "KeyZ".into(), modifiers: 0 };
    ctl.send(&Message::InputEvent(ev.clone()).encode()).await.unwrap();
    let received = tokio::time::timeout(deadline, input_rx.recv())
        .await
        .expect("timed out waiting for input event")
        .expect("input channel closed");
    assert!(matches!(received, InjectCommand::Event(e) if e == ev));

    // --- soft-keyboard text path: controller -> ctl channel -> injector queue
    ctl.send(&Message::TextInput(TextInput { text: "señor 🎯".into() }).encode())
        .await
        .unwrap();
    let text_cmd = tokio::time::timeout(deadline, input_rx.recv())
        .await
        .expect("timed out waiting for text input")
        .expect("input channel closed");
    assert!(matches!(text_cmd, InjectCommand::Text(t) if t == "señor 🎯"));

    // --- clipboard, both directions over the bulk channel, sized past the
    // single-message SCTP limit to prove the chunked path.
    // (In-process there is no RTT between the host learning of the channel
    // and our first send; give its handler registration a moment.)
    tokio::time::sleep(Duration::from_millis(500)).await;
    let big_up = "u".repeat(100_000);
    let wire = Message::ClipboardData(ClipboardData { text: big_up.clone() }).encode();
    for chunk in tetherd::webrtc::chunk_frame(1, &wire) {
        bulk.send(&chunk).await.unwrap();
    }
    let clip = tokio::task::spawn_blocking(move || {
        clipboard_in_rx.recv_timeout(Duration::from_secs(5))
    })
    .await
    .unwrap()
    .expect("clipboard not relayed to host");
    assert_eq!(clip, big_up);

    let big_down = "d".repeat(100_000);
    clipboard_out_tx.send(Some(big_down.clone())).unwrap();
    let mut bulk_reassembler = FrameReassembler::default();
    let got = tokio::time::timeout(deadline, async {
        loop {
            let bytes = bulk_rx.recv().await.expect("bulk closed");
            let Some(wire) = bulk_reassembler.on_chunk(&bytes) else { continue };
            if let Ok(Decoded::Message { message: Message::ClipboardData(c), .. }) =
                Message::decode(&wire)
            {
                return c.text;
            }
        }
    })
    .await
    .expect("host clipboard never arrived on bulk");
    assert_eq!(got, big_down);

    pc.close().await.ok();
}
