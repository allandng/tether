// Home-screen (PWA) concerns on iOS. Settings, pairing tokens and the Ed25519
// identity live in localStorage. In a Safari tab WebKit may clear that after
// seven days without a visit, and an installed home-screen app gets a storage
// container of its own — so a tab user who installs later has to set up and
// pair again. Hence: nudge iOS tab users to install (and to pair from the
// installed app), and ask for persistent storage once there's a pairing worth
// keeping.

export interface PlatformEnv {
  userAgent: string;
  maxTouchPoints: number;
}

/** iPhone/iPod/iPad, including iPadOS's desktop-class (Macintosh) user agent,
 * which only a touchscreen tells apart from a real Mac. */
export function isIos(env: PlatformEnv): boolean {
  if (/iPhone|iPad|iPod/.test(env.userAgent)) return true;
  return /Macintosh/.test(env.userAgent) && env.maxTouchPoints > 1;
}

export interface StandaloneEnv {
  /** `navigator.standalone` — WebKit-only, undefined elsewhere. */
  standalone?: boolean;
  /** `matchMedia("(display-mode: standalone)").matches` */
  displayModeStandalone: boolean;
}

/** Running as an installed home-screen app rather than in a browser tab. */
export function isStandalone(env: StandaloneEnv): boolean {
  return env.standalone === true || env.displayModeStandalone;
}

export interface InstallHintEnv extends PlatformEnv, StandaloneEnv {
  /** The user closed the hint before. */
  dismissed: boolean;
}

export function shouldShowInstallHint(env: InstallHintEnv): boolean {
  return isIos(env) && !isStandalone(env) && !env.dismissed;
}

export const INSTALL_HINT_KEY = "tether-install-hint-dismissed";

export interface StorageNavigator {
  storage?: {
    persist?(): Promise<boolean>;
    persisted?(): Promise<boolean>;
  };
}

/** Ask the browser not to evict our storage. Best effort: resolves whether
 * storage ended up persistent, never rejects. */
export async function requestPersistentStorage(nav: StorageNavigator): Promise<boolean> {
  try {
    const storage = nav.storage;
    if (typeof storage?.persist !== "function") return false;
    if (await storage.persisted?.()) return true;
    return await storage.persist();
  } catch {
    return false;
  }
}
