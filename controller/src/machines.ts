// Saved machines (Phase 8): the hosts this browser has paired with or
// connected to, reconciled against the signal server's live directory into
// the rows the machine list shows. Pure functions + a thin localStorage
// wrapper, so the reconciliation rules are unit-tested.

import type { PeerInfo } from "./signaling";

export interface SavedMachine {
  deviceId: string;
  name: string;
  os: string;
  /** Epoch ms this machine was last seen online (or connected to). */
  lastSeen: number;
}

export type MachineState = "online" | "offline" | "new";

export interface MachineRow {
  deviceId: string;
  name: string;
  os: string;
  state: MachineState;
  lastSeen: number | null;
}

export type SavedMachines = Record<string, SavedMachine>;

/**
 * Merge the live directory (`peers`; `null` while it is unreachable) into the
 * saved set. Saved hosts that are present refresh their name/os/lastSeen;
 * present hosts never seen before show as "new" (tapping one pairs it);
 * saved hosts that are absent show as offline.
 */
export function reconcile(
  saved: SavedMachines,
  peers: PeerInfo[] | null,
  now: number,
): { rows: MachineRow[]; saved: SavedMachines } {
  const next: SavedMachines = { ...saved };
  const rows: MachineRow[] = [];
  const present = new Set<string>();
  for (const p of peers ?? []) {
    if (!p.caps.can_host) continue; // controllers don't list each other
    present.add(p.device_id);
    const known = next[p.device_id];
    if (known) {
      next[p.device_id] = { ...known, name: p.name || known.name, os: p.os || known.os, lastSeen: now };
    }
    rows.push({
      deviceId: p.device_id,
      name: p.name || p.device_id,
      os: p.os ?? known?.os ?? "",
      state: known ? "online" : "new",
      lastSeen: known ? now : null,
    });
  }
  for (const m of Object.values(next)) {
    if (present.has(m.deviceId)) continue;
    rows.push({ deviceId: m.deviceId, name: m.name, os: m.os, state: "offline", lastSeen: m.lastSeen });
  }
  const rank: Record<MachineState, number> = { online: 0, new: 1, offline: 2 };
  rows.sort((a, b) => rank[a.state] - rank[b.state] || a.name.localeCompare(b.name));
  return { rows, saved: next };
}

/** Record a successful connection (pairing or token) to a host. */
export function remember(saved: SavedMachines, m: Omit<SavedMachine, "lastSeen">, now: number): SavedMachines {
  const prev = saved[m.deviceId];
  return {
    ...saved,
    [m.deviceId]: {
      deviceId: m.deviceId,
      name: m.name || prev?.name || m.deviceId,
      os: m.os || prev?.os || "",
      lastSeen: now,
    },
  };
}

export function forget(saved: SavedMachines, deviceId: string): SavedMachines {
  const next = { ...saved };
  delete next[deviceId];
  return next;
}

/** "just now", "5 min ago", "3 h ago", "2 days ago". */
export function lastSeenText(lastSeen: number | null, now: number): string {
  if (lastSeen === null) return "";
  const s = Math.max(0, Math.round((now - lastSeen) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86_400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86_400)} days ago`;
}

const KEY = "tether-machines-v1";

export const machineStore = {
  load(): SavedMachines {
    try {
      const parsed = JSON.parse(localStorage.getItem(KEY) ?? "{}");
      return parsed && typeof parsed === "object" ? (parsed as SavedMachines) : {};
    } catch {
      return {};
    }
  },
  save(saved: SavedMachines): void {
    try {
      localStorage.setItem(KEY, JSON.stringify(saved));
    } catch {
      // storage unavailable: the list just won't persist
    }
  },
};
