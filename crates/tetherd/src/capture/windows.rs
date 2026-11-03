//! Windows screen capture via DXGI Desktop Duplication.
//!
//! Chosen over Windows.Graphics.Capture as the primary path (the Phase 7 plan
//! left that open): it is pull-based like the [`ScreenCapturer`] trait, needs
//! no WinRT dispatcher, works on Windows 10 without WGC's undisableable yellow
//! capture border, and behaves the same on every Windows 10/11 build.
//!
//! Desktop Duplication only produces a frame when something on screen
//! changes. The session layer, though, relies on a steady trickle — a newly
//! connected controller needs a frame (and an H.264 decoder needs a
//! keyframe) even if the screen is static — so while idle the last image is
//! re-emitted every [`IDLE_REPEAT`], sharing its pixels with the frame it
//! repeats (so the pipeline can tell a repeat from a new image). Static
//! repeats are cheap for H.264.
//!
//! Mode changes (resolution, monitor hot-plug), the secure desktop (UAC, the
//! lock screen) and fullscreen-exclusive apps all surface as
//! `DXGI_ERROR_ACCESS_LOST` or a failed `DuplicateOutput`; the capturer
//! rebuilds itself in place and the pipeline notices the new resolution on
//! the next frame. While the duplication can't be rebuilt the last image keeps
//! repeating, so the remote screen freezes rather than the session dying.

pub mod cursor;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use tether_protocol::{DisplayInfo, Resolution};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIAdapter1, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use self::cursor::{PointerShape, ShapeKind};
use super::{RawFrame, ScreenCapturer};
use crate::win;

/// Re-emit the last image this often while the screen is static.
const IDLE_REPEAT: Duration = Duration::from_millis(250);
/// Back-off between attempts to rebuild a lost duplication.
const REBUILD_RETRY: Duration = Duration::from_millis(300);

/// No monitor could be duplicated at all (no session desktop yet, or every
/// output is on the secure desktop). Transient: the pipeline retries.
#[derive(Debug)]
pub struct NoDisplays(pub String);

impl std::fmt::Display for NoDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no display can be captured right now: {}", self.0)
    }
}

impl std::error::Error for NoDisplays {}

/// A live duplication of one output.
struct Duplication {
    dup: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

/// The most recent desktop image, tightly packed BGRA.
struct Desktop {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

#[derive(Default)]
struct Pointer {
    shape: Option<PointerShape>,
    visible: bool,
    x: i32,
    y: i32,
}

pub struct DxgiCapturer {
    frame_interval: Duration,
    display_id: u32,
    dup: Option<Duplication>,
    desktop: Option<Desktop>,
    pointer: Pointer,
    /// The last frame composed from `desktop` and `pointer`; an idle repeat
    /// re-emits it (sharing its pixels) until either changes.
    composed: Option<RawFrame>,
    last_emit: Option<Instant>,
    /// When the next frame is due, on a fixed 1/fps schedule — so time the
    /// caller spends encoding counts toward the interval instead of adding to it.
    next_due: Option<Instant>,
    lost_since: Option<Instant>,
}

impl DxgiCapturer {
    /// Begin capturing the primary display.
    pub fn primary(fps: u32) -> anyhow::Result<Self> {
        let monitor = win::primary_monitor()
            .ok_or_else(|| anyhow::Error::new(NoDisplays("no monitors attached".into())))?;
        Self::for_display(monitor.id, fps)
    }

    pub fn for_display(display_id: u32, fps: u32) -> anyhow::Result<Self> {
        let dup =
            duplicate(display_id).map_err(|e| anyhow::Error::new(NoDisplays(format!("{e:#}"))))?;
        info!(
            display_id,
            width = dup.width,
            height = dup.height,
            "desktop duplication started"
        );
        Ok(DxgiCapturer {
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps.max(1))),
            display_id,
            dup: Some(dup),
            desktop: None,
            pointer: Pointer::default(),
            composed: None,
            last_emit: None,
            next_due: None,
            lost_since: None,
        })
    }

    /// The last desktop image with the cursor drawn on, as a frame. It is
    /// composed afresh only when `changed` (a new desktop image or pointer
    /// update since the last composition); otherwise — an idle repeat — the
    /// last composition is re-emitted with a fresh timestamp, sharing its
    /// pixels, which is how the pipeline recognises a repeat.
    fn compose(&mut self, changed: bool) -> Option<RawFrame> {
        let frame = match &self.composed {
            Some(last) if !changed => RawFrame {
                timestamp_micros: unix_micros(),
                ..last.clone()
            },
            _ => {
                let desktop = self.desktop.as_ref()?;
                let mut bgra = desktop.bgra.clone();
                if self.pointer.visible
                    && let Some(shape) = &self.pointer.shape
                {
                    cursor::draw(
                        &mut bgra,
                        desktop.width,
                        desktop.height,
                        desktop.width as usize * 4,
                        shape,
                        self.pointer.x,
                        self.pointer.y,
                    );
                }
                let frame = RawFrame::from_bgra(
                    desktop.width,
                    desktop.height,
                    desktop.width as usize * 4,
                    bgra,
                    unix_micros(),
                );
                self.composed = Some(frame.clone()); // shares the pixels
                frame
            }
        };
        let now = Instant::now();
        self.last_emit = Some(now);
        let next = self.next_due.unwrap_or(now) + self.frame_interval;
        // Running behind (slow encode, or idle): restart the schedule from now
        // rather than bursting to catch up.
        self.next_due = Some(if next < now { now } else { next });
        Some(frame)
    }

    /// One `AcquireNextFrame` round. `Ok(true)` = the image or the cursor
    /// changed; `Ok(false)` = timed out with nothing new.
    fn acquire(&mut self, timeout: Duration) -> anyhow::Result<bool> {
        let dup = self.dup.as_mut().context("no duplication")?;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-pointers are live locals; the duplication is valid.
        let acquired = unsafe {
            dup.dup
                .AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource)
        };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        // From here the frame must be released whatever happens.
        let result = (|| -> anyhow::Result<bool> {
            let mut changed = false;
            if info.LastPresentTime != 0
                && let Some(resource) = &resource
            {
                let texture: ID3D11Texture2D = resource.cast()?;
                self.desktop = Some(read_texture(dup, &texture)?);
                changed = true;
            }
            if info.LastMouseUpdateTime != 0 {
                self.pointer.visible = info.PointerPosition.Visible.as_bool();
                self.pointer.x = info.PointerPosition.Position.x;
                self.pointer.y = info.PointerPosition.Position.y;
                changed = true;
            }
            if info.PointerShapeBufferSize > 0 {
                match read_pointer_shape(&dup.dup, info.PointerShapeBufferSize) {
                    Ok(shape) => self.pointer.shape = shape,
                    Err(e) => debug!(error = %e, "pointer shape unavailable"),
                }
                changed = true;
            }
            Ok(changed)
        })();
        // SAFETY: paired with the successful AcquireNextFrame above.
        unsafe {
            let _ = dup.dup.ReleaseFrame();
        }
        result
    }

    /// Drop the current duplication and try once to rebuild it.
    fn rebuild(&mut self) -> bool {
        self.dup = None;
        // The output may have been unplugged: fall back to the primary.
        let id = if win::monitor(self.display_id).is_some() {
            self.display_id
        } else {
            match win::primary_monitor() {
                Some(m) => m.id,
                None => return false,
            }
        };
        match duplicate(id) {
            Ok(dup) => {
                info!(
                    display_id = id,
                    width = dup.width,
                    height = dup.height,
                    "desktop duplication rebuilt"
                );
                self.display_id = id;
                self.dup = Some(dup);
                self.lost_since = None;
                true
            }
            Err(e) => {
                let since = *self.lost_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(10) {
                    warn!(error = %e, "desktop duplication still unavailable (secure desktop or locked?)");
                    self.lost_since = Some(Instant::now());
                }
                false
            }
        }
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn resolution(&self) -> Resolution {
        match (&self.desktop, &self.dup) {
            (Some(d), _) => Resolution {
                width: d.width,
                height: d.height,
            },
            (None, Some(d)) => Resolution {
                width: d.width,
                height: d.height,
            },
            (None, None) => Resolution {
                width: 0,
                height: 0,
            },
        }
    }

    fn next_frame(&mut self) -> anyhow::Result<RawFrame> {
        // Pace to the target rate: frames that land in between accumulate in
        // the duplication and arrive together as the latest image.
        if let Some(due) = self.next_due {
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
        loop {
            if self.dup.is_none() && !self.rebuild() {
                std::thread::sleep(REBUILD_RETRY);
                if let Some(frame) = self.idle_repeat_due() {
                    return Ok(frame);
                }
                continue;
            }
            let wait = self
                .last_emit
                .map(|at| IDLE_REPEAT.saturating_sub(at.elapsed()))
                .unwrap_or(IDLE_REPEAT)
                .max(Duration::from_millis(1));
            match self.acquire(wait) {
                Ok(true) if self.desktop.is_some() => {
                    if let Some(frame) = self.compose(true) {
                        return Ok(frame);
                    }
                }
                Ok(_) => {
                    if let Some(frame) = self.idle_repeat_due() {
                        return Ok(frame);
                    }
                }
                Err(e) => {
                    let lost = e
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|w| w.code() == DXGI_ERROR_ACCESS_LOST);
                    if lost {
                        debug!("desktop duplication lost (mode change or desktop switch)");
                    } else {
                        warn!(error = %e, "AcquireNextFrame failed; rebuilding");
                    }
                    self.dup = None;
                }
            }
        }
    }

    fn displays(&self) -> Vec<DisplayInfo> {
        let all = win::monitors();
        if all.is_empty() {
            let r = self.resolution();
            return vec![DisplayInfo {
                id: self.display_id,
                width: r.width,
                height: r.height,
                active: true,
                name: "Display".into(),
            }];
        }
        all.iter()
            .enumerate()
            .map(|(i, m)| DisplayInfo {
                id: m.id,
                width: m.width(),
                height: m.height(),
                active: m.id == self.display_id,
                name: if m.primary {
                    format!("Display {} (main)", i + 1)
                } else {
                    format!("Display {}", i + 1)
                },
            })
            .collect()
    }

    fn switch_display(&mut self, id: u32) -> anyhow::Result<()> {
        if id == self.display_id && self.dup.is_some() {
            return Ok(());
        }
        let dup = duplicate(id)?;
        // Never show the old display's image as the new one — but always
        // have *an* image: black at the new size until its first frame, so
        // the idle-repeat path can keep the pipeline moving even if this
        // duplication is lost before it delivers anything.
        self.desktop = Some(Desktop {
            width: dup.width,
            height: dup.height,
            bgra: vec![0; dup.width as usize * dup.height as usize * 4],
        });
        self.composed = None;
        self.display_id = id;
        self.dup = Some(dup);
        self.pointer = Pointer::default();
        Ok(())
    }

    fn active_display(&self) -> u32 {
        self.display_id
    }
}

impl DxgiCapturer {
    fn idle_repeat_due(&mut self) -> Option<RawFrame> {
        let due = self.last_emit.is_none_or(|at| at.elapsed() >= IDLE_REPEAT);
        if due { self.compose(false) } else { None }
    }
}

/// Find the DXGI output for a GDI display id and start duplicating it on a
/// D3D11 device created on that output's own adapter (hybrid-GPU laptops put
/// outputs on different adapters; duplication must use the owning one).
fn duplicate(display_id: u32) -> anyhow::Result<Duplication> {
    // SAFETY: COM factory creation; no preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapter_index = 0;
    loop {
        // SAFETY: enumerating until DXGI_ERROR_NOT_FOUND.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        adapter_index += 1;
        let mut output_index = 0;
        loop {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            output_index += 1;
            // SAFETY: plain descriptor read.
            let desc = unsafe { output.GetDesc()? };
            if !desc.AttachedToDesktop.as_bool()
                || win::display_number(&desc.DeviceName) != Some(display_id)
            {
                continue;
            }
            if desc.Rotation.0 > DXGI_MODE_ROTATION_IDENTITY.0 {
                warn!(
                    display_id,
                    "rotated display: the image is streamed unrotated"
                );
            }
            let output1: IDXGIOutput1 = output.cast()?;
            let (device, context) = create_device(&adapter)?;
            // SAFETY: device is a live D3D11 device on the owning adapter.
            let dup = unsafe { output1.DuplicateOutput(&device) }
                .context("DuplicateOutput (secure desktop, or too many duplications?)")?;
            // SAFETY: plain descriptor read.
            let dd = unsafe { dup.GetDesc() };
            return Ok(Duplication {
                dup,
                device,
                context,
                staging: None,
                width: dd.ModeDesc.Width,
                height: dd.ModeDesc.Height,
            });
        }
    }
    bail!("display {display_id} not found among DXGI outputs")
}

fn create_device(adapter: &IDXGIAdapter1) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    // SAFETY: out-pointers are live locals; an explicit adapter requires
    // D3D_DRIVER_TYPE_UNKNOWN.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?,
        context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?,
    ))
}

/// GPU frame → CPU bytes via a staging texture (the readback the Phase 7 plan
/// chose to take in v1). Rows are repacked tightly.
fn read_texture(dup: &mut Duplication, texture: &ID3D11Texture2D) -> anyhow::Result<Desktop> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: out-pointer to a live local.
    unsafe { texture.GetDesc(&mut desc) };
    let reuse = dup.staging.as_ref().is_some_and(|s| {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: as above.
        unsafe { s.GetDesc(&mut sd) };
        sd.Width == desc.Width && sd.Height == desc.Height && sd.Format == desc.Format
    });
    if !reuse {
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: desc.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: valid descriptor; out-pointer to a live local.
        unsafe {
            dup.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
        };
        dup.staging = staging;
    }
    let staging = dup.staging.as_ref().context("staging texture")?;
    let (w, h) = (desc.Width, desc.Height);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: both textures live on dup.device with identical size/format;
    // Map/Unmap are paired and the mapped rows are read within bounds.
    let bgra = unsafe {
        dup.context.CopyResource(staging, texture);
        dup.context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let row = w as usize * 4;
        // Only `pitch * (h - 1) + row` bytes are guaranteed mapped (the last
        // row needn't be padded out to the pitch).
        let len = if h == 0 {
            0
        } else {
            pitch * (h as usize - 1) + row
        };
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, len);
        let mut out = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            out.extend_from_slice(&src[y * pitch..y * pitch + row]);
        }
        dup.context.Unmap(staging, 0);
        out
    };
    dup.width = w;
    dup.height = h;
    Ok(Desktop {
        width: w,
        height: h,
        bgra,
    })
}

fn read_pointer_shape(
    dup: &IDXGIOutputDuplication,
    size: u32,
) -> anyhow::Result<Option<PointerShape>> {
    let mut buf = vec![0u8; size as usize];
    let mut required = 0u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    // SAFETY: buffer is `size` bytes as advertised by the frame info.
    unsafe {
        dup.GetFramePointerShape(size, buf.as_mut_ptr().cast(), &mut required, &mut info)?;
    }
    buf.truncate(required as usize);
    Ok(ShapeKind::from_dxgi(info.Type).map(|kind| PointerShape {
        kind,
        width: info.Width,
        height: info.Height,
        pitch: info.Pitch,
        data: buf,
    }))
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}
