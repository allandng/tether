// Canvas renderer: latest-wins frame decoding, aspect-fit display, and the
// displayed-rect math that input mapping builds on. JPEG decodes via
// createImageBitmap; H.264 streams through a WebCodecs VideoDecoder.

import { H264Stream, webCodecsSupported } from "./decode";
import { Codec, type FrameData, type Resolution } from "./protocol";

export interface DisplayedRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** Maximum local pinch-zoom factor. */
const MAX_ZOOM = 4;

/**
 * Where content of size contentW×contentH sits inside a boxW×boxH box under
 * `object-fit: contain` (letterbox/pillarbox bars), relative to the box.
 */
export function containRect(boxW: number, boxH: number, contentW: number, contentH: number): DisplayedRect {
  const contentAspect = contentW / Math.max(1, contentH);
  const boxAspect = boxW / Math.max(1, boxH);
  if (boxAspect > contentAspect) {
    // pillarbox
    const width = boxH * contentAspect;
    return { left: (boxW - width) / 2, top: 0, width, height: boxH };
  }
  // letterbox
  const height = boxW / contentAspect;
  return { left: 0, top: (boxH - height) / 2, width: boxW, height };
}

/**
 * Clamp a zoom pan (`translate(tx,ty) scale(scale)`, origin 0 0) so it never
 * exposes empty space: on an axis where the scaled `content` (untransformed
 * box coordinates, from {@link containRect}) is smaller than the box it stays
 * centered, otherwise its edges may not cross into the box.
 */
export function clampPan(
  scale: number,
  tx: number,
  ty: number,
  boxW: number,
  boxH: number,
  content: DisplayedRect,
): { tx: number; ty: number } {
  if (scale === 1) return { tx: 0, ty: 0 };
  const axis = (t: number, off: number, len: number, box: number) => {
    const scaledOff = off * scale;
    const scaledLen = len * scale;
    if (scaledLen <= box) {
      // content still smaller than the box on this axis → keep it centered
      return (box - scaledLen) / 2 - scaledOff;
    }
    // content covers the box → keep its edges from crossing inward
    return clamp(t, box - scaledOff - scaledLen, -scaledOff) || 0; // no -0
  };
  return { tx: axis(tx, content.left, content.width, boxW), ty: axis(ty, content.top, content.height, boxH) };
}

export class Viewer {
  private readonly ctx: CanvasRenderingContext2D;
  private decoding = false;
  private pending: FrameData | null = null;
  private frameTimes: number[] = [];
  private lastTimestampMicros = 0;
  private h264: H264Stream | null = null;

  // Local pinch-zoom transform (committed), applied to the canvas element.
  // The coordinate mapping is transform-invariant (displayedRect measures the
  // transformed canvas via getBoundingClientRect), so this never touches the
  // wire — it only magnifies the local view for touch precision.
  private scale = 1;
  private tx = 0;
  private ty = 0;
  // per-pinch anchor state
  private pinch: { s0: number; fx0: number; fy0: number; pfx: number; pfy: number } | null = null;

  private cursorEl: HTMLDivElement | null = null;
  // the dot's content point, as a fraction of the displayed rect, so it can
  // follow the content through zoom, pan and rotation
  private cursorAt: { fx: number; fy: number } | null = null;

  /** Called when the view goes in or out of zoom (scale ≠ 1), e.g. to show
   * a reset affordance. */
  onZoomedChange: ((zoomed: boolean) => void) | null = null;

  constructor(private readonly canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("no 2d context");
    this.ctx = ctx;
    this.canvas.style.transformOrigin = "0 0";
  }

  /** Current local zoom factor (1 = fit). */
  get zoom(): number {
    return this.scale;
  }

  /**
   * Draw a client-side cursor dot at the given client-pixel point (for
   * trackpad mode, where the only other feedback is the host cursor composited
   * into the capture — which lags a frame). Client px is already zoom-correct.
   */
  setCursor(clientX: number, clientY: number): void {
    if (typeof document === "undefined") return;
    const r = this.displayedRect();
    this.cursorAt = {
      fx: (clientX - r.left) / Math.max(1, r.width),
      fy: (clientY - r.top) / Math.max(1, r.height),
    };
    if (!this.cursorEl) {
      const el = document.createElement("div");
      Object.assign(el.style, {
        position: "fixed",
        width: "14px",
        height: "14px",
        marginLeft: "-7px",
        marginTop: "-7px",
        borderRadius: "50%",
        border: "2px solid #fff",
        boxShadow: "0 0 0 1.5px rgba(0,0,0,0.6)",
        pointerEvents: "none",
        zIndex: "9999",
      } satisfies Partial<CSSStyleDeclaration>);
      document.body.appendChild(el);
      this.cursorEl = el;
    }
    this.cursorEl.style.left = `${clientX}px`;
    this.cursorEl.style.top = `${clientY}px`;
    this.cursorEl.style.display = "block";
  }

  hideCursor(): void {
    if (this.cursorEl) this.cursorEl.style.display = "none";
  }

  get cursorVisible(): boolean {
    return this.cursorEl?.style.display === "block";
  }

  /** Re-place a shown dot over its content point after the view moved,
   * held inside the visible stage (where the trackpad cursor re-clamps to). */
  private repositionCursor(): void {
    if (!this.cursorEl || !this.cursorAt || !this.cursorVisible) return;
    const r = this.displayedRect();
    const v = this.visibleRect();
    const x = clamp(r.left + this.cursorAt.fx * r.width, v.left, v.left + v.width);
    const y = clamp(r.top + this.cursorAt.fy * r.height, v.top, v.top + v.height);
    this.cursorEl.style.left = `${x}px`;
    this.cursorEl.style.top = `${y}px`;
  }

  /**
   * The part of the screen the remote view can be seen in, client px: the
   * stage box (which clips a zoomed canvas) cut by the visual viewport (soft
   * keyboard, browser chrome).
   */
  visibleRect(): DisplayedRect {
    // the canvas fills its stage; the stage's box is the untransformed view
    const host = this.canvas.parentElement ?? this.canvas;
    let r: DisplayedRect = host.getBoundingClientRect();
    const vv = typeof window !== "undefined" ? window.visualViewport : null;
    if (vv) {
      const left = Math.max(r.left, vv.offsetLeft);
      const top = Math.max(r.top, vv.offsetTop);
      const right = Math.min(r.left + r.width, vv.offsetLeft + vv.width);
      const bottom = Math.min(r.top + r.height, vv.offsetTop + vv.height);
      if (right > left && bottom > top) r = { left, top, width: right - left, height: bottom - top };
    }
    return { left: r.left, top: r.top, width: r.width, height: r.height };
  }

  /**
   * Re-fit after the stage changed size (rotation, resize, soft keyboard):
   * re-clamp the pan to the new box and put the cursor dot back over its
   * content point. A live pinch is left alone; its end clamps.
   */
  relayout(): void {
    // a hidden view (0×0 box) would "clamp" the pan away; keep it for later
    const shown = this.canvas.clientWidth > 1 && this.canvas.clientHeight > 1;
    if (!this.pinch && shown) this.clampPan();
    this.repositionCursor();
  }

  /** Back to the fitted 1× view. */
  resetZoom(): void {
    this.pinch = null;
    this.setTransform(1, 0, 0);
  }

  /**
   * Shift the zoomed view by (dx, dy) client px, clamped like any pan;
   * returns the shift actually applied (zero when not zoomed).
   */
  panBy(dx: number, dy: number): { dx: number; dy: number } {
    if (this.scale === 1 || this.pinch) return { dx: 0, dy: 0 };
    const { tx, ty } = this.clampedPan(this.tx + dx, this.ty + dy);
    const applied = { dx: tx - this.tx, dy: ty - this.ty };
    this.setTransform(this.scale, tx, ty);
    return applied;
  }

  /**
   * Call before a new connection's frames arrive (reconnect, another host):
   * its H.264 stream must start at a keyframe, not continue the decoder state
   * the previous connection left behind — those deltas would reference
   * pictures this decoder never saw and draw garbage until the next keyframe.
   */
  resetStream(): void {
    this.h264?.restart();
    this.pending = null;
  }

  setResolution(resolution: Resolution): void {
    // Canvas backing store = capture pixels; CSS handles the fit.
    this.canvas.width = resolution.width;
    this.canvas.height = resolution.height;
  }

  /**
   * Apply a pinch gesture's cumulative `scale` about the live `focal` point,
   * keeping the content point under the fingers fixed (pinch-to-pan). The
   * transform is live (not yet committed) until {@link endZoom}.
   */
  applyZoom(scale: number, focalX: number, focalY: number): void {
    if (!this.pinch) {
      // anchor: which content point sits under the focal at pinch start
      this.pinch = {
        s0: this.scale,
        fx0: focalX,
        fy0: focalY,
        pfx: (focalX - this.tx) / this.scale,
        pfy: (focalY - this.ty) / this.scale,
      };
    }
    const target = clamp(this.pinch.s0 * scale, 1, MAX_ZOOM);
    // keep the anchored content point under the current focal
    const liveTx = focalX - target * this.pinch.pfx;
    const liveTy = focalY - target * this.pinch.pfy;
    this.setTransform(target, liveTx, liveTy);
  }

  /** Commit the live pinch transform and clamp pan to keep content on screen. */
  endZoom(): void {
    this.pinch = null;
    this.clampPan();
  }

  private setTransform(scale: number, tx: number, ty: number): void {
    const wasZoomed = this.scale !== 1;
    this.scale = scale;
    this.tx = tx;
    this.ty = ty;
    if (scale === 1) {
      this.tx = 0;
      this.ty = 0;
      this.canvas.style.transform = "";
    } else {
      this.canvas.style.transform = `translate(${this.tx}px, ${this.ty}px) scale(${scale})`;
    }
    if (wasZoomed !== (scale !== 1)) this.onZoomedChange?.(scale !== 1);
    this.repositionCursor();
  }

  /**
   * Pan can't expose empty space. Clamp against the *displayed content* rect
   * (the letterboxed video inside the element box), not the element box — else
   * a portrait phone showing a landscape desktop could pan into the black bars.
   * Works in untransformed layout space (clientWidth/Height are transform-
   * agnostic), mirroring the displayedRect aspect math.
   */
  private clampedPan(tx: number, ty: number): { tx: number; ty: number } {
    const boxW = this.canvas.clientWidth;
    const boxH = this.canvas.clientHeight;
    const content = containRect(boxW, boxH, this.canvas.width, this.canvas.height);
    return clampPan(this.scale, tx, ty, boxW, boxH, content);
  }

  private clampPan(): void {
    if (this.scale === 1) return;
    const { tx, ty } = this.clampedPan(this.tx, this.ty);
    this.setTransform(this.scale, tx, ty);
  }

  /**
   * JPEG: latest-wins — if a decode is in flight, the newest frame replaces
   * any queued one. H.264: every access unit feeds the stream decoder in
   * order (delta frames need their predecessors); rendering is paced by the
   * decoder's output callback.
   */
  onFrame(frame: FrameData): void {
    if (frame.codec === Codec.H264) {
      if (!webCodecsSupported()) {
        console.error("host is sending H.264 but this browser lacks WebCodecs");
        return;
      }
      this.h264 ??= new H264Stream(
        (videoFrame, captureMicros) => this.drawVideoFrame(videoFrame, captureMicros),
        (detail) => console.warn(detail),
      );
      this.h264.push(frame.payload, frame.timestampMicros, frame.seq);
      return;
    }
    if (this.decoding) {
      this.pending = frame;
      return;
    }
    void this.decodeAndDraw(frame);
  }

  private drawVideoFrame(videoFrame: VideoFrame, timestampMicros: number): void {
    if (this.canvas.width !== videoFrame.displayWidth || this.canvas.height !== videoFrame.displayHeight) {
      this.canvas.width = videoFrame.displayWidth;
      this.canvas.height = videoFrame.displayHeight;
    }
    this.ctx.drawImage(videoFrame, 0, 0);
    videoFrame.close();
    this.frameTimes.push(performance.now());
    this.lastTimestampMicros = timestampMicros;
  }

  /** Frames drawn in the last second. */
  get fps(): number {
    const cutoff = performance.now() - 1000;
    this.frameTimes = this.frameTimes.filter((t) => t > cutoff);
    return this.frameTimes.length;
  }

  /** Capture timestamp of the most recent drawn frame (host clock, micros). */
  get lastFrameTimestampMicros(): number {
    return this.lastTimestampMicros;
  }

  /**
   * Where the video content actually sits inside the canvas element's CSS box
   * (object-fit: contain leaves letterbox bars). Input mapping must use this
   * rect, not the element bounds.
   */
  displayedRect(): DisplayedRect {
    const bounds = this.canvas.getBoundingClientRect();
    const r = containRect(bounds.width, bounds.height, this.canvas.width, this.canvas.height);
    return { left: bounds.left + r.left, top: bounds.top + r.top, width: r.width, height: r.height };
  }

  private async decodeAndDraw(frame: FrameData): Promise<void> {
    this.decoding = true;
    try {
      const bitmap = await createImageBitmap(
        new Blob([frame.payload as BlobPart], { type: "image/jpeg" }),
      );
      if (this.canvas.width !== bitmap.width || this.canvas.height !== bitmap.height) {
        this.canvas.width = bitmap.width;
        this.canvas.height = bitmap.height;
      }
      this.ctx.drawImage(bitmap, 0, 0);
      bitmap.close();
      this.frameTimes.push(performance.now());
      this.lastTimestampMicros = frame.timestampMicros;
    } catch (e) {
      console.warn("frame decode failed:", e);
    } finally {
      this.decoding = false;
      const next = this.pending;
      this.pending = null;
      if (next) void this.decodeAndDraw(next);
    }
  }
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}
