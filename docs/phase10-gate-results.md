# Phase 10 gate results — audio

Built per [phase10-plan.md](phase10-plan.md), Windows half. The macOS
capturer is not built yet (see below).

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
| 1 | macOS host audio plays without dropouts | ❌ Not built: the `screencapturekit` audio output path needs a Mac to develop against. `--audio` on macOS logs that it's unavailable and continues without. |
| 2 | Windows host audio plays on a phone | ✅ in a desktop browser: `inbound-rtp` audio, codec `audio/opus`, **99 packets in 2 s (50/s), 0 lost**. Phone pass pending. |
| 3 | Without `--audio`, nothing captured or attached; controller says so | ✅ No capture thread, no track, no speaker button. |
| 4 | New controller ↔ old host and old controller ↔ new host both connect | ✅ New controller → host without audio: video at 28 fps. Old controller (no audio m-line) → audio host: the host skips the track (logged); `webrtc_e2e` covers an offer with no audio section. |
| 5 | iOS first tap starts audio; survives backgrounding | ⏳ Needs an iPhone. |
| 6 | Output-device change recovers within seconds | ⏳ Implemented (2 s poll); not exercised. |
| 7 | Video fps/latency unchanged with audio on | ✅ Separate thread; stream stayed at full rate in the live test. |

The real-device check (`audio::windows::tests::real_loopback_device_opens`,
`--ignored`) opened this machine's 48 kHz stereo endpoint.
