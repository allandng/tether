// Handing a finished in-memory download to the user. Elsewhere a synthetic
// `<a download>` click does it, but on iOS that click happens outside any user
// gesture (the transfer just finished), and an installed home-screen app has
// no download manager to receive it — it fails silently. Where the share
// sheet can take files, the download instead waits on its row for a "Save…"
// tap, whose gesture opens the sheet (Save to Files, Photos, AirDrop, …).

export type SaveStrategy = "share" | "anchor";

export interface ShareNavigator {
  canShare?(data: { files: File[] }): boolean;
  share?(data: { files: File[] }): Promise<void>;
}

export interface SaveEnv {
  ios: boolean;
  nav: ShareNavigator;
}

/** Share sheet on iOS when it accepts this file; a plain download otherwise
 * (desktop browsers download fine, and their share dialogs are no substitute). */
export function chooseSaveStrategy(env: SaveEnv, file: File): SaveStrategy {
  if (!env.ios || typeof env.nav.canShare !== "function" || typeof env.nav.share !== "function") {
    return "anchor";
  }
  try {
    return env.nav.canShare({ files: [file] }) ? "share" : "anchor";
  } catch {
    return "anchor";
  }
}

// The share sheet picks its actions by type (Save Image only for images, …),
// and a reassembled download's Blob has none.
const MIME: Record<string, string> = {
  pdf: "application/pdf",
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  heic: "image/heic",
  webp: "image/webp",
  mp4: "video/mp4",
  mov: "video/quicktime",
  mp3: "audio/mpeg",
  txt: "text/plain",
  csv: "text/csv",
  json: "application/json",
  zip: "application/zip",
};

export function mimeFor(name: string): string {
  const ext = /\.([^./\\]+)$/.exec(name)?.[1]?.toLowerCase() ?? "";
  return MIME[ext] ?? "application/octet-stream";
}

export function fileFor(name: string, blob: Blob): File {
  return new File([blob], name, { type: blob.type || mimeFor(name) });
}

export type ShareOutcome = "shared" | "cancelled" | "failed";

/** Open the share sheet for `file`. Call straight from a tap handler: share()
 * starts synchronously here, while the gesture still counts. */
export async function shareFile(nav: ShareNavigator, file: File): Promise<ShareOutcome> {
  try {
    if (typeof nav.share !== "function") return "failed";
    await nav.share({ files: [file] });
    return "shared";
  } catch (e) {
    // dismissing the sheet rejects with AbortError: not a failure
    return (e as { name?: string } | null)?.name === "AbortError" ? "cancelled" : "failed";
  }
}
