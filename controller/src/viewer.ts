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

export class Viewer {
  private readonly ctx: CanvasRenderingContext2D;
  private decoding = false;
  private pending: FrameData | null = null;
  private frameTimes: number[] = [];
  private lastTimestampMicros = 0;
  private h264: H264Stream | null = null;

  constructor(private readonly canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("no 2d context");
    this.ctx = ctx;
  }

  setResolution(resolution: Resolution): void {
    // Canvas backing store = capture pixels; CSS handles the fit.
    this.canvas.width = resolution.width;
    this.canvas.height = resolution.height;
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
