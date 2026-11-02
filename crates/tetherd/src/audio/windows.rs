//! WASAPI loopback capture of the default output device (Phase 10 M5).
//!
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` on a shared-mode render endpoint hands back
//! exactly what the machine is playing, in the endpoint's mix format — no
//! driver, no elevation. That format is whatever the user's device runs at
//! (often 48 kHz float stereo, but 44.1 kHz, 16-bit or 5.1 all happen), so
//! samples are converted to f32 here and resampled by the shared thread.
//!
//! Plugging in headphones changes the default device; the old client keeps
//! capturing the old (now silent) device, so the endpoint id is compared
//! periodically and the capturer rebuilt to follow.

use anyhow::{Context, bail};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};

use super::SystemAudioCapturer;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// 1 s of buffer, in 100 ns units: plenty of slack for a 10 ms poll.
const BUFFER_100NS: i64 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    F32,
    I16,
    I32,
}

pub struct WasapiLoopback {
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    device_id: String,
    rate: u32,
    channels: usize,
    format: SampleFormat,
}

fn default_device_id(enumerator: &IMMDeviceEnumerator) -> anyhow::Result<String> {
    // SAFETY: plain COM calls; the id string is freed with CoTaskMemFree.
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let id = device.GetId()?;
        let s = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));
        Ok(s)
    }
}

impl WasapiLoopback {
    /// Loopback-capture the current default output device.
    pub fn default_device() -> anyhow::Result<Self> {
        // SAFETY: COM initialisation for this (the audio) thread; repeated
        // calls on the same thread are harmless (S_FALSE).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // SAFETY: standard WASAPI setup; the mix format is freed after use.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("creating the audio device enumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no default audio output device")?;
            let device_id = default_device_id(&enumerator)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let parsed = parse_format(&*mix);
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_100NS,
                0,
                mix,
                None,
            );
            let (rate, channels) = ((*mix).nSamplesPerSec, (*mix).nChannels as usize);
            CoTaskMemFree(Some(mix as *const _));
            init.context("initialising loopback capture")?;
            let format = parsed?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(WasapiLoopback {
                enumerator,
                client,
                capture,
                device_id,
                rate,
                channels,
                format,
            })
        }
    }
}

/// Which sample encoding a mix format uses (float32, int16 or int32).
fn parse_format(f: &WAVEFORMATEX) -> anyhow::Result<SampleFormat> {
    let tag = f.wFormatTag;
    let bits = f.wBitsPerSample;
    let kind = if tag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
        // SAFETY: cbSize says the extensible tail is present.
        let ext = unsafe { &*(f as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
        // KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT differ only in data1.
        ext.SubFormat.data1 as u16
    } else {
        tag
    };
    match (kind, bits) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => Ok(SampleFormat::F32),
        (WAVE_FORMAT_PCM, 16) => Ok(SampleFormat::I16),
        (WAVE_FORMAT_PCM, 32) => Ok(SampleFormat::I32),
        _ => bail!("unsupported audio mix format (tag {kind}, {bits} bits)"),
    }
}

impl SystemAudioCapturer for WasapiLoopback {
    fn format(&self) -> (u32, usize) {
        (self.rate, self.channels)
    }

    fn read(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        // SAFETY: GetBuffer/ReleaseBuffer are paired; the buffer holds
        // `frames * channels` samples of the negotiated format.
        unsafe {
            loop {
                let available = self.capture.GetNextPacketSize()?;
                if available == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * self.channels;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    out.extend(std::iter::repeat_n(0.0, n));
                } else {
                    match self.format {
                        SampleFormat::F32 => {
                            out.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n))
                        }
                        SampleFormat::I16 => out.extend(
                            std::slice::from_raw_parts(data as *const i16, n)
                                .iter()
                                .map(|&s| f32::from(s) / 32768.0),
                        ),
                        SampleFormat::I32 => out.extend(
                            std::slice::from_raw_parts(data as *const i32, n)
                                .iter()
                                .map(|&s| s as f32 / 2_147_483_648.0),
                        ),
                    }
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }

    fn device_changed(&mut self) -> bool {
        default_device_id(&self.enumerator).is_ok_and(|id| id != self.device_id)
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // SAFETY: stopping a started client; failure is irrelevant at drop.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_formats_parse() {
        let mut f = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            wBitsPerSample: 32,
            ..Default::default()
        };
        assert_eq!(parse_format(&f).unwrap(), SampleFormat::F32);
        f.wFormatTag = WAVE_FORMAT_PCM;
        f.wBitsPerSample = 16;
        assert_eq!(parse_format(&f).unwrap(), SampleFormat::I16);
        f.wBitsPerSample = 24;
        assert!(parse_format(&f).is_err(), "packed 24-bit is not handled");

        let mut ext = WAVEFORMATEXTENSIBLE::default();
        ext.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
        ext.Format.wBitsPerSample = 32;
        ext.Format.cbSize = 22;
        ext.SubFormat = windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
        assert_eq!(
            parse_format(&ext.Format).unwrap(),
            SampleFormat::F32,
            "KSDATAFORMAT_SUBTYPE_IEEE_FLOAT"
        );
    }

    /// Opens the real default output device: run with `--ignored` on a
    /// machine with audio hardware.
    #[test]
    #[ignore]
    fn real_loopback_device_opens() {
        let mut cap = WasapiLoopback::default_device().unwrap();
        let (rate, ch) = cap.format();
        assert!(rate >= 8000 && ch >= 1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut out = Vec::new();
        cap.read(&mut out).unwrap();
        println!("{rate} Hz × {ch}: read {} samples", out.len());
    }
}
