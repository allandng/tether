import { describe, expect, it } from "vitest";
import { type ShareNavigator, chooseSaveStrategy, fileFor, mimeFor, shareFile } from "./save";

const file = new File([new Uint8Array([1, 2, 3])], "report.pdf", { type: "application/pdf" });

function nav(canShare: boolean | "throws" | undefined, share?: () => Promise<void>): ShareNavigator {
  const n: ShareNavigator = { share: share ?? (() => Promise.resolve()) };
  if (canShare !== undefined) {
    n.canShare = () => {
      if (canShare === "throws") throw new TypeError("bad data");
      return canShare;
    };
  }
  return n;
}

describe("chooseSaveStrategy", () => {
  it("uses the share sheet on iOS when it takes the file", () => {
    expect(chooseSaveStrategy({ ios: true, nav: nav(true) }, file)).toBe("share");
  });

  it("downloads on iOS when files can't be shared (older iOS, refused type)", () => {
    expect(chooseSaveStrategy({ ios: true, nav: nav(false) }, file)).toBe("anchor");
    expect(chooseSaveStrategy({ ios: true, nav: nav(undefined) }, file)).toBe("anchor");
    expect(chooseSaveStrategy({ ios: true, nav: nav("throws") }, file)).toBe("anchor");
    expect(chooseSaveStrategy({ ios: true, nav: { canShare: () => true } }, file)).toBe("anchor");
  });

  it("downloads off iOS even where sharing files works", () => {
    expect(chooseSaveStrategy({ ios: false, nav: nav(true) }, file)).toBe("anchor");
  });

  it("calls canShare as a method (a detached navigator.canShare throws)", () => {
    const n = {
      share: () => Promise.resolve(),
      canShare(this: unknown) {
        return this === n;
      },
    };
    expect(chooseSaveStrategy({ ios: true, nav: n }, file)).toBe("share");
  });
});

describe("fileFor", () => {
  it("types an untyped download by its extension", () => {
    expect(fileFor("IMG_0001.JPG", new Blob([new Uint8Array(4)])).type).toBe("image/jpeg");
    expect(fileFor("notes", new Blob([new Uint8Array(4)])).type).toBe("application/octet-stream");
    expect(fileFor("x.bin", new Blob([], { type: "text/plain" })).type).toBe("text/plain");
    expect(fileFor("a.pdf", new Blob([new Uint8Array(4)])).name).toBe("a.pdf");
  });

  it("mimeFor goes by the last extension, any case", () => {
    expect(mimeFor("archive.tar.ZIP")).toBe("application/zip");
    expect(mimeFor("no-extension")).toBe("application/octet-stream");
  });
});

describe("shareFile", () => {
  it("reports shared, cancelled (AbortError) and failed", async () => {
    expect(await shareFile(nav(true), file)).toBe("shared");
    const abort = () => Promise.reject(new DOMException("dismissed", "AbortError"));
    expect(await shareFile(nav(true, abort), file)).toBe("cancelled");
    const denied = () => Promise.reject(new DOMException("no gesture", "NotAllowedError"));
    expect(await shareFile(nav(true, denied), file)).toBe("failed");
    expect(await shareFile({}, file)).toBe("failed");
  });

  it("starts the share synchronously, inside the caller's gesture", () => {
    let started = false;
    void shareFile(
      nav(true, () => {
        started = true;
        return Promise.resolve();
      }),
      file,
    );
    expect(started).toBe(true);
  });
});
