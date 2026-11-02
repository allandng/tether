// Generates the PWA icons in public/icons from one procedural design, so the
// PNGs are reproducible without an image editor:  node scripts/gen-icons.mjs
//
// The mark: two nodes joined by a line on a dark rounded square — a tether.
// Rendered with signed-distance shapes and 4x4 supersampling, encoded as PNG
// with node's zlib (no dependencies).

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { deflateSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "..", "public", "icons");
mkdirSync(out, { recursive: true });

const BG = [17, 17, 19];
const BLUE = [53, 120, 246];
const WHITE = [236, 236, 240];
const LINE = [120, 160, 250];

// Shapes in a 0..1 unit square.
const A = [0.33, 0.67]; // blue node, lower left
const B = [0.67, 0.33]; // white node, upper right
const R = 0.115;

function segDist(p, a, b) {
  const pa = [p[0] - a[0], p[1] - a[1]];
  const ba = [b[0] - a[0], b[1] - a[1]];
  const h = Math.max(0, Math.min(1, (pa[0] * ba[0] + pa[1] * ba[1]) / (ba[0] ** 2 + ba[1] ** 2)));
  return Math.hypot(pa[0] - ba[0] * h, pa[1] - ba[1] * h);
}

function roundRectDist(p, inset, radius) {
  const q = [Math.abs(p[0] - 0.5) - (0.5 - inset - radius), Math.abs(p[1] - 0.5) - (0.5 - inset - radius)];
  return Math.hypot(Math.max(q[0], 0), Math.max(q[1], 0)) + Math.min(Math.max(q[0], q[1]), 0) - radius;
}

/** Colour (rgba) at unit point p. `maskable` fills the full square (the OS
 * applies its own mask); otherwise the background is a rounded square. */
function shade(p, maskable) {
  const inBg = maskable || roundRectDist(p, 0.02, 0.2) <= 0;
  if (!inBg) return [0, 0, 0, 0];
  // shrink the mark into the maskable safe zone (inner 80%)
  const s = maskable ? 0.8 : 1;
  const q = [(p[0] - 0.5) / s + 0.5, (p[1] - 0.5) / s + 0.5];
  if (Math.hypot(q[0] - A[0], q[1] - A[1]) <= R) return [...BLUE, 255];
  if (Math.hypot(q[0] - B[0], q[1] - B[1]) <= R) return [...WHITE, 255];
  if (segDist(q, A, B) <= 0.035) return [...LINE, 255];
  return [...BG, 255];
}

function render(size, maskable = false) {
  const px = Buffer.alloc(size * size * 4);
  const SS = 4;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const acc = [0, 0, 0, 0];
      for (let sy = 0; sy < SS; sy++) {
        for (let sx = 0; sx < SS; sx++) {
          const c = shade([(x + (sx + 0.5) / SS) / size, (y + (sy + 0.5) / SS) / size], maskable);
          // premultiply so transparent edges don't darken
          acc[0] += c[0] * c[3];
          acc[1] += c[1] * c[3];
          acc[2] += c[2] * c[3];
          acc[3] += c[3];
        }
      }
      const o = (y * size + x) * 4;
      const a = acc[3] / (SS * SS);
      px[o] = a ? Math.round(acc[0] / acc[3]) : 0;
      px[o + 1] = a ? Math.round(acc[1] / acc[3]) : 0;
      px[o + 2] = a ? Math.round(acc[2] / acc[3]) : 0;
      px[o + 3] = Math.round(a);
    }
  }
  return png(size, size, px);
}

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}
function png(w, h, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (w * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (w * 4 + 1) + 1, y * w * 4, (y + 1) * w * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

writeFileSync(join(out, "icon-192.png"), render(192));
writeFileSync(join(out, "icon-512.png"), render(512));
writeFileSync(join(out, "maskable-512.png"), render(512, true));
writeFileSync(join(out, "apple-touch-icon.png"), render(180, true)); // iOS rounds it itself
writeFileSync(
  join(out, "icon.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect x="2" y="2" width="96" height="96" rx="20" fill="#111113"/>
  <line x1="33" y1="67" x2="67" y2="33" stroke="#78a0fa" stroke-width="7" stroke-linecap="round"/>
  <circle cx="33" cy="67" r="11.5" fill="#3578f6"/>
  <circle cx="67" cy="33" r="11.5" fill="#ececf0"/>
</svg>
`,
);
console.log(`icons written to ${out}`);
