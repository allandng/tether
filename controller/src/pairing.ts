// Controller-side pairing crypto + channel binding. Mirrors
// crates/tetherd/src/auth.rs byte-for-byte (cross-pinned in pairing.test.ts):
//   channel_binding = SHA256( normalize(lo) || "|" || normalize(hi) )
//   pairing_proof   = HMAC-SHA256(code_utf8, channel_binding)
// where lo/hi are the two normalized DTLS fingerprints sorted, and normalize
// strips whitespace + ':' and uppercases.

const subtle = (): SubtleCrypto => {
  const c = globalThis.crypto?.subtle;
  if (!c) {
    throw new Error("pairing needs WebCrypto — serve the controller over HTTPS or http://localhost");
  }
  return c;
};

/** Match the Rust `normalize_fp`: drop whitespace and ':', uppercase. */
export function normalizeFp(fp: string): string {
  return fp.replace(/[\s:]/g, "").toUpperCase();
}

/** Canonicalize a typed pairing code to the host's alphabet: strip grouping
 * dashes/spaces, uppercase, and apply Crockford base32 typo substitutions
 * (I/L→1, O→0) so a misread digit still verifies. */
export function normalizeCode(code: string): string {
  return code
    .replace(/[\s-]/g, "")
    .toUpperCase()
    .replace(/[IL]/g, "1") // Crockford: I, L → 1
    .replace(/O/g, "0"); // Crockford: O → 0
}

/** SHA-256 channel binding from two fingerprints, order-independent. */
export async function channelBinding(fpA: string, fpB: string): Promise<Uint8Array> {
  const a = normalizeFp(fpA);
  const b = normalizeFp(fpB);
  const [lo, hi] = a <= b ? [a, b] : [b, a];
  const enc = new TextEncoder();
  const msg = new Uint8Array([...enc.encode(lo), 0x7c /* | */, ...enc.encode(hi)]);
  const digest = await subtle().digest("SHA-256", msg as BufferSource);
  return new Uint8Array(digest);
}

/** The fixed binding for the direct LAN WebSocket transport (no relay). */
export function wsChannelBinding(): Promise<Uint8Array> {
  return channelBinding("tether-lan-direct", "tether-lan-direct");
}

/** HMAC-SHA256(code, channelBinding) — the pairing proof. */
export async function pairingProof(code: string, binding: Uint8Array): Promise<Uint8Array> {
  const key = await subtle().importKey(
    "raw",
    new TextEncoder().encode(code) as BufferSource,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await subtle().sign("HMAC", key, binding as BufferSource);
  return new Uint8Array(sig);
}

/** Per-host device-token persistence (keyed by host id / address). */
export const tokenStore = {
  get(host: string): string | null {
    try {
      return localStorage.getItem(`tether-token:${host}`);
    } catch {
      return null;
    }
  },
  set(host: string, token: string): void {
    try {
      localStorage.setItem(`tether-token:${host}`, token);
    } catch {
      // storage unavailable (private mode); pairing still works for this session
    }
  },
};

/** Extract the `a=fingerprint:` value from an SDP (first occurrence). */
export function sdpFingerprint(sdp: string): string | null {
  for (const line of sdp.split(/\r?\n/)) {
    const m = line.trim().match(/^a=fingerprint:(.+)$/);
    if (m) return m[1]!.trim();
  }
  return null;
}
