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

  constructor(private readonly canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("no 2d context");
    this.ctx = ctx;
    this.canvas.style.transformOrigin = "0 0";
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
  }

  /** Pan can't expose empty space: clamp tx,ty so the scaled canvas covers the box. */
  private clampPan(): void {
    if (this.scale === 1) return;
    // clientWidth/Height are layout (transform-agnostic) → the untransformed box
    const vw = this.canvas.clientWidth;
    const vh = this.canvas.clientHeight;
    const tx = clamp(this.tx, vw * (1 - this.scale), 0);
    const ty = clamp(this.ty, vh * (1 - this.scale), 0);
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
      this.h264.push(frame.payload, frame.timestampMicros);
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
    const contentAspect = this.canvas.width / Math.max(1, this.canvas.height);
    const boxAspect = bounds.width / Math.max(1, bounds.height);
    if (boxAspect > contentAspect) {
      // pillarbox
      const width = bounds.height * contentAspect;
      return { left: bounds.left + (bounds.width - width) / 2, top: bounds.top, width, height: bounds.height };
    }
    // letterbox
    const height = bounds.width / contentAspect;
    return { left: bounds.left, top: bounds.top + (bounds.height - height) / 2, width: bounds.width, height };
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
