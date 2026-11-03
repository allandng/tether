import { describe, expect, it } from "vitest";
import { type InstallHintEnv, isIos, requestPersistentStorage, shouldShowInstallHint } from "./pwa";

const UA = {
  iphone:
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1",
  ipadMobile:
    "Mozilla/5.0 (iPad; CPU OS 16_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1",
  // iPadOS 13+ asks for desktop sites by default: indistinguishable from a Mac
  // except for the touchscreen
  macDesktop:
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15",
  windows:
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
  android:
    "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36",
};

const tab = (userAgent: string, maxTouchPoints: number): InstallHintEnv => ({
  userAgent,
  maxTouchPoints,
  standalone: false,
  displayModeStandalone: false,
  dismissed: false,
});

describe("isIos", () => {
  it("knows iPhone, iPad, and an iPad posing as a Mac", () => {
    expect(isIos({ userAgent: UA.iphone, maxTouchPoints: 5 })).toBe(true);
    expect(isIos({ userAgent: UA.ipadMobile, maxTouchPoints: 5 })).toBe(true);
    expect(isIos({ userAgent: UA.macDesktop, maxTouchPoints: 5 })).toBe(true);
  });

  it("does not mistake a Mac or other platforms for iOS", () => {
    expect(isIos({ userAgent: UA.macDesktop, maxTouchPoints: 0 })).toBe(false);
    expect(isIos({ userAgent: UA.windows, maxTouchPoints: 10 })).toBe(false);
    expect(isIos({ userAgent: UA.android, maxTouchPoints: 5 })).toBe(false);
  });
});

describe("shouldShowInstallHint", () => {
  it("shows in an iOS browser tab", () => {
    expect(shouldShowInstallHint(tab(UA.iphone, 5))).toBe(true);
    expect(shouldShowInstallHint(tab(UA.ipadMobile, 5))).toBe(true);
    expect(shouldShowInstallHint(tab(UA.macDesktop, 5))).toBe(true);
  });

  it("hides once installed, by either signal", () => {
    expect(shouldShowInstallHint({ ...tab(UA.iphone, 5), standalone: true })).toBe(false);
    expect(shouldShowInstallHint({ ...tab(UA.ipadMobile, 5), displayModeStandalone: true })).toBe(false);
    // navigator.standalone absent (non-WebKit iOS shell): the media query decides
    expect(shouldShowInstallHint({ ...tab(UA.iphone, 5), standalone: undefined })).toBe(true);
  });

  it("stays dismissed", () => {
    expect(shouldShowInstallHint({ ...tab(UA.iphone, 5), dismissed: true })).toBe(false);
  });

  it("never shows off iOS", () => {
    expect(shouldShowInstallHint(tab(UA.macDesktop, 0))).toBe(false);
    expect(shouldShowInstallHint(tab(UA.windows, 0))).toBe(false);
    expect(shouldShowInstallHint(tab(UA.android, 5))).toBe(false);
  });
});

describe("requestPersistentStorage", () => {
  it("asks only when not already persistent", async () => {
    let asked = 0;
    const persist = () => {
      asked++;
      return Promise.resolve(true);
    };
    expect(await requestPersistentStorage({ storage: { persist, persisted: () => Promise.resolve(true) } })).toBe(true);
    expect(asked).toBe(0);
    expect(await requestPersistentStorage({ storage: { persist, persisted: () => Promise.resolve(false) } })).toBe(true);
    expect(asked).toBe(1);
  });

  it("is best effort: missing API, refusal and errors all resolve", async () => {
    expect(await requestPersistentStorage({})).toBe(false);
    expect(await requestPersistentStorage({ storage: {} })).toBe(false);
    expect(await requestPersistentStorage({ storage: { persist: () => Promise.resolve(false) } })).toBe(false);
    expect(await requestPersistentStorage({ storage: { persist: () => Promise.reject(new Error("x")) } })).toBe(false);
  });
});
