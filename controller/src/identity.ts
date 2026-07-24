// Per-device identity for the signal directory (Phase 6).
//
// The server issues a nonce on connect and pins `device_id -> pubkey` on first
// use, so nobody else holding the shared secret can claim this device's id.
// Mirrors crates/tether-signal/src/identity.rs — the payload bytes are pinned
// by a vector test on both sides.
//
// Ed25519 in WebCrypto is available in Safari 17+, Chrome 137+ and Firefox
// 129+. Older browsers get `null` from `loadOrCreateIdentity` and register
// without an identity, which the server allows for a controller id nobody has
// claimed yet. Hosts always sign; they are native and have no such constraint.

const STORAGE_KEY = "tether-identity-v1";
const REGISTER_CONTEXT = "tether-signal-register-v1";

/** The exact bytes both ends sign: context \0 nonce \0 device_id. */
export function registerPayload(nonce: string, deviceId: string): Uint8Array {
  const enc = new TextEncoder();
  const ctx = enc.encode(REGISTER_CONTEXT);
  const n = enc.encode(nonce);
  const d = enc.encode(deviceId);
  const out = new Uint8Array(ctx.length + n.length + d.length + 2);
  out.set(ctx, 0);
  out[ctx.length] = 0;
  out.set(n, ctx.length + 1);
  out[ctx.length + 1 + n.length] = 0;
  out.set(d, ctx.length + n.length + 2);
  return out;
}

export function toHex(bytes: ArrayBuffer | Uint8Array): string {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  return Array.from(view, (b) => b.toString(16).padStart(2, "0")).join("");
}

export interface DeviceIdentity {
  /** Hex-encoded 32-byte Ed25519 public key. */
  readonly pubkey: string;
  /** Hex-encoded 64-byte signature over `registerPayload`. */
  sign(nonce: string, deviceId: string): Promise<string>;
}

function base64(bytes: ArrayBuffer): string {
  return btoa(String.fromCharCode(...new Uint8Array(bytes)));
}

function unbase64(s: string): Uint8Array {
  return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
}

async function exportIdentity(pair: CryptoKeyPair): Promise<DeviceIdentity> {
  const raw = await crypto.subtle.exportKey("raw", pair.publicKey);
  const priv = pair.privateKey;
  return {
    pubkey: toHex(raw),
    async sign(nonce, deviceId) {
      const sig = await crypto.subtle.sign(
        "Ed25519",
        priv,
        registerPayload(nonce, deviceId) as unknown as BufferSource,
      );
      return toHex(sig);
    },
  };
}

/**
 * The browser's identity key, generated once and kept in localStorage.
 *
 * Returns `null` when Ed25519 is unavailable, when storage is blocked (private
 * browsing), or when the stored key is unusable — never throws. A missing
 * identity degrades to an unsigned registration rather than blocking the
 * connection outright.
 */
export async function loadOrCreateIdentity(): Promise<DeviceIdentity | null> {
  if (typeof crypto === "undefined" || !crypto.subtle) return null;

  const stored = readStored();
  if (stored) {
    try {
      const privateKey = await crypto.subtle.importKey(
        "pkcs8",
        stored.priv as unknown as BufferSource,
        "Ed25519",
        true,
        ["sign"],
      );
      const publicKey = await crypto.subtle.importKey(
        "raw",
        stored.pub as unknown as BufferSource,
        "Ed25519",
        true,
        ["verify"],
      );
      return await exportIdentity({ privateKey, publicKey });
    } catch {
      // Corrupt or algorithm-unsupported: fall through and mint a fresh one.
      // Note this changes our pubkey, so a server that pinned the old one will
      // refuse us — but the alternative is being permanently stuck.
    }
  }

  try {
    const pair = (await crypto.subtle.generateKey("Ed25519", true, [
      "sign",
      "verify",
    ])) as CryptoKeyPair;
    const priv = await crypto.subtle.exportKey("pkcs8", pair.privateKey);
    const pub = await crypto.subtle.exportKey("raw", pair.publicKey);
    try {
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({ priv: base64(priv), pub: base64(pub) }),
      );
    } catch {
      // Storage blocked: the identity works for this session but won't survive
      // a reload, so the server will pin a key we can't reproduce. Better than
      // failing to connect.
    }
    return await exportIdentity(pair);
  } catch {
    return null; // no Ed25519 in this browser
  }
}

function readStored(): { priv: Uint8Array; pub: Uint8Array } | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as { priv?: string; pub?: string };
    if (!parsed.priv || !parsed.pub) return null;
    return { priv: unbase64(parsed.priv), pub: unbase64(parsed.pub) };
  } catch {
    return null;
  }
}
