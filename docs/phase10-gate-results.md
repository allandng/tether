# Phase 10 gate results — audio

Built per [phase10-plan.md](phase10-plan.md), Windows half. The macOS
capturer followed in Phase 12 ([below](#macos-capturer-phase-12)).

## What shipped

- **Capture → Opus thread** (`audio.rs`): its own thread, never the video
  thread. Streaming linear resampler to 48 kHz stereo (mono duplicated,
  surround → front L/R), 20 ms frames, Opus at 96 kbps, published on a
  broadcast channel; peers that lag skip packets instead of stalling others,
  and with no subscribers the encoder is skipped. Loopback delivers nothing
  while nothing plays, so silence is padded to keep the audio clock on wall
  time. A capture error or default-device change rebuilds the capturer.
- **WASAPI loopback** (`audio/windows.rs`): default render endpoint,
  `AUDCLNT_STREAMFLAGS_LOOPBACK`, float32 / int16 / int32 mix formats
  (extensible or plain), endpoint id compared every 2 s to follow headphones
  being plugged in.
- **Negotiation.** The controller's offer always carries a `recvonly` audio
  transceiver. The host registers Opus in its media engine regardless, and
  attaches a track only with `--audio` *and* an audio m-line in the offer. The
  track is added *before* the offer is applied: webrtc-rs only matches a local
  send transceiver to the offer's section at that point (adding it afterwards
  produced an answer with no `a=ssrc` and the audio went nowhere — found in
  the live test). RTP flows only after the ctl channel authenticates the peer,
  like the media and bulk pumps. `Hello.capabilities` bit 2 (`CAP_AUDIO`) on
  both sides.
- **Controller**: hidden `<audio>`, a speaker button that starts muted (the
  first tap is the iOS gesture) and appears only once the track actually
  carries audio (its `unmute` event) — a host without `--audio` shows none.
- **Opt-in** `--audio` / `audio = true`. LAN sessions get no audio, as planned.

## Gate criteria

| # | Criterion | Result |
|---|---|---|
| 1 | macOS host audio plays without dropouts | ✅ Built in Phase 12 ([below](#macos-capturer-phase-12)). **50.5 Opus packets/s** through the pipeline on a real Mac. In the live end-to-end run, a browser received the Opus track at **50.2 packets/s, 0 lost**, alongside H.264 video. ⏳ Not yet listened to on a phone for a several-minute session (the live run's audio element was muted). At Phase 10 it was not built: `--audio` on macOS logged that it was unavailable and continued without. |
| 2 | Windows host audio plays on a phone | ✅ in a desktop browser: `inbound-rtp` audio, codec `audio/opus`, **99 packets in 2 s (50/s), 0 lost**. Phone pass pending. |
| 3 | Without `--audio`, nothing captured or attached; controller says so | ✅ No capture thread, no track, no speaker button. |
| 4 | New controller ↔ old host and old controller ↔ new host both connect | ✅ New controller → host without audio: video at 28 fps. Old controller (no audio m-line) → audio host: the host skips the track (logged); `webrtc_e2e` covers an offer with no audio section. |
| 5 | iOS first tap starts audio; survives backgrounding | ⏳ Needs an iPhone. |
| 6 | Output-device change recovers within seconds | ⏳ Implemented (2 s poll); not exercised. |
| 7 | Video fps/latency unchanged with audio on | ✅ Separate thread; stream stayed at full rate in the live test. |

The real-device check (`audio::windows::tests::real_loopback_device_opens`,
`--ignored`) opened this machine's 48 kHz stereo endpoint.

## macOS capturer (Phase 12)

**Result:** built in Phase 12 and measured on a real Mac. Details in
[phase12-gate-results.md](phase12-gate-results.md#macos-system-audio).

**Design** (`audio/macos.rs`). The plan's open question resolved in favour of
the crate: `screencapturekit` 7.0 exposes the Audio output, so no raw `objc2`
was needed.

- An audio-only `SCStream` on the main display feeds the same Opus thread as
  WASAPI. It carries a 2×2, 1 fps video nothing consumes, plus the Audio
  output; 48 kHz stereo; tether's own process excluded.
- It needs only the Screen Recording grant (no microphone permission), and no
  device-change handling, because SCK taps the mix before the device.
- Float32 planar 20 ms buffers are interleaved in the callback.
- A 5 s no-buffer watchdog rebuilds a stuck stream. A capturer that fails
  within 10 s of starting backs off 2 s → 16 s.
- The blocking SCK calls run on helper threads with 5 / 5 / 2 s deadlines.

**Measured** on an M3 (macOS 26.5.1):

- **Real-capture tests** (`--ignored`): 47,940 frames/s captured (48 kHz),
  with a Tink alert peaking at 0.36. Through the pipeline,
  **50.5 Opus packets/s**, 3–392 B (3 B for silence). Video and audio capture
  ran concurrently.
- **Live end-to-end run:** a browser received the Opus track over WebRTC at
  50.2 packets/s with 0 lost. Silence ~39 B per 250 ms; Tink/Glass alerts
  2.2–3.1 KB per 250 ms.
- **Display asleep:** SCK fails ("No capture source provided"), and the
  capturer retries every 2 s. Locked with the display on, it still captures.
