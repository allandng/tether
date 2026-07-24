# Phase 10 Plan — audio

**Scope:** stream the host's system audio to the controller. Host to
controller only — Chrome Remote Desktop did not send microphone audio to the
host either, and it is not what this is for.

tether has no audio path at all today, so this phase adds capture on two
platforms, an Opus encoder, and — the structurally interesting part — the first
real media track on a WebRTC connection that has so far carried only data
channels.

---

## 1. Use a real audio track, not a data channel

The tempting shortcut is to send Opus packets down `tether-media` alongside
video frames. Do not. Sending audio as a proper WebRTC track means the browser
supplies the jitter buffer, packet-loss concealment, decode, and playback
scheduling. Over a data channel, all four become our problem, in TypeScript,
and audio is far less forgiving of a bad jitter buffer than video is of a
dropped frame.

`webrtc-rs` provides `TrackLocalStaticSample`; the host attaches one with
`audio/opus` and writes encoded frames to it.

**This changes SDP negotiation, which is the part to be careful about.** The
peer connection currently negotiates data channels only. The controller creates
the offer, so it must add a `recvonly` audio transceiver for the host to have
anything to attach a track to. Both directions of version skew have to
degrade quietly:

- New controller, old host: the offer carries an audio m-line, the host never
  attaches a track, nothing plays. Fine.
- Old controller, new host: no audio m-line in the offer, so the host has
  nowhere to attach. It must notice and skip, not fail the negotiation.

## 2. Two limitations to accept up front

**Audio is WebRTC-only.** The LAN WebSocket transport has no concept of media
tracks. Carrying Opus over the binary protocol there would mean building the
jitter buffer this plan just argued against. LAN sessions get no audio, and
that is documented rather than solved.

**A/V sync will be approximate.** Video travels over a data channel with no RTP
timestamps; audio travels over RTP with its own clock. Nothing correlates them,
so lip-sync is best-effort — fine for a notification chime or music, visibly
off for video playback. Genuinely fixing it would mean moving video onto an RTP
track too, which is a much larger change with its own trade-offs (losing the
reliable-delivery property the H.264 path currently depends on). Not this
phase, possibly not ever.

## 3. Capture

**macOS:** `SCStreamConfiguration` can capture system audio on macOS 13+,
delivering audio sample buffers through the same `SCStream` the video capture
already runs — no virtual audio device, no driver install, no extra permission
beyond the Screen Recording grant that already exists.

The open question is whether the `screencapturekit` 7.0 crate surfaces the
audio output type. If it does not, there is a clean precedent in this codebase:
`encode/h264.rs` already drives VideoToolbox through raw `objc2` bindings
because no safe wrapper existed. The same approach applies here. Confirm which
before M1 — it is the difference between a small module and a large one.

**Windows:** WASAPI loopback — `IAudioClient` initialised with
`AUDCLNT_STREAMFLAGS_LOOPBACK` on the default render endpoint. No driver, no
elevation. Handle default-device changes (headphones plugged in mid-session)
by rebuilding the client, the same shape as the capturer's mode-change
recovery.

A `SystemAudioCapturer` trait alongside `ScreenCapturer`, with the same
`#[cfg(target_os)]` module split.

## 4. Encode and plumbing

- Opus at 48 kHz stereo, 20 ms frames. 96 kbps is more than enough for system
  audio and is noise next to a 4 Mbps video stream.
- Resample if the platform hands back something other than 48 kHz — WASAPI in
  particular gives whatever the endpoint's mix format is.
- Audio capture and encode run on their **own thread**, not the capture thread.
  The video pipeline's `watch` channel is latest-wins by design, which is
  correct for frames and catastrophic for audio: a dropped audio buffer is an
  audible click, not a skipped frame. Audio needs a small bounded queue that
  blocks or drops with intent.
- **Opt-in on the host: `--audio`.** Defaulting to on means a machine in a room
  with people starts streaming that room's sound to a phone the first time
  someone connects. Off by default, with a controller-side mute toggle on top.

## 5. Controller

Attach the inbound track to a hidden `<audio>` element via the `ontrack`
handler.

**iOS Safari will not start playback without a user gesture.** So a speaker
toggle in the session view, defaulting to muted, where the first tap both
unmutes and satisfies the gesture requirement. This is the same shape as the
existing clipboard chip — a deliberate tap where the platform refuses to let us
be automatic.

Advertise support with `Hello.capabilities` bit 2 (bits 2–7 are free), so the
host can log why audio is absent rather than leaving it a mystery.

## 6. Module order

1. **M1 — macOS capture:** confirm crate support, `SystemAudioCapturer`, raw
   PCM out, unit-tested framing.
2. **M2 — Opus + track:** encoder, dedicated thread and bounded queue,
   `TrackLocalStaticSample`, `--audio` flag.
3. **M3 — negotiation:** `recvonly` transceiver on the controller, skew
   handling both directions, capability bit.
4. **M4 — controller playback:** `ontrack`, hidden element, speaker toggle,
   iOS gesture path.
5. **M5 — Windows capture:** WASAPI loopback, resampling, device-change
   recovery.
6. **M6 — gate.**

## 7. Gate criteria (proposed)

1. Audio from a macOS host plays on a phone with no audible dropouts over a
   several-minute session.
2. The same from a Windows host.
3. Without `--audio`, no audio is captured and no track is attached; the
   controller says so rather than sitting silent.
4. A new controller against an old host, and an old controller against a new
   host, both still connect and stream video. Neither combination breaks
   negotiation.
5. On iOS, the first speaker tap starts audio and it survives backgrounding and
   returning.
6. Changing the host's output device mid-session recovers within a few seconds.
7. Video frame rate and latency are unchanged with audio enabled — the audio
   thread does not disturb the capture thread.

## 8. Risks

- **`screencapturekit` may not expose audio.** The raw-`objc2` fallback is
  known-good in this codebase but is meaningfully more work. Establish which
  before committing to the phase's size.
- **Adding an m-line to a working peer connection** is the highest-risk edit
  here, because it touches a negotiation path that currently works and is
  covered by `tests/webrtc_e2e.rs`. That test should grow an audio case.
- **Two congestion controllers on one connection.** The video path's AIMD loop
  watches data-channel buffer depth; the audio track is managed by the browser
  and libwebrtc independently. They do not coordinate. On a constrained link
  the interaction is unmodelled — watch for video bitrate collapsing while
  audio holds, and treat it as a documented behaviour unless it is severe.
- **Privacy.** This streams a room's ambient audio if anything is playing. The
  opt-in default is the mitigation and should not be quietly reversed for
  convenience.

---

**Status: planned, not started. Depends on Phase 7 for the Windows half.
Independent of Phases 8 and 9.**
