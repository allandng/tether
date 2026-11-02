import { describe, expect, it } from "vitest";
import { forget, lastSeenText, reconcile, remember, type SavedMachines } from "./machines";
import type { PeerInfo } from "./signaling";

const host = (id: string, name = id, os = "windows"): PeerInfo => ({
  device_id: id,
  name,
  os,
  caps: { can_host: true, can_control: true },
});
const controller = (id: string): PeerInfo => ({
  device_id: id,
  name: id,
  caps: { can_host: false, can_control: true },
});

describe("machine list reconciliation", () => {
  const saved: SavedMachines = {
    desk: { deviceId: "desk", name: "Desk PC", os: "windows", lastSeen: 100 },
    mac: { deviceId: "mac", name: "MacBook", os: "macos", lastSeen: 50 },
  };

  it("paired + present → online, paired + absent → offline, unknown host → new", () => {
    const { rows } = reconcile(saved, [host("desk", "Desk PC"), host("laptop", "Laptop"), controller("phone")], 1000);
    expect(rows.map((r) => [r.deviceId, r.state])).toEqual([
      ["desk", "online"],
      ["laptop", "new"],
      ["mac", "offline"],
    ]);
    expect(rows.find((r) => r.deviceId === "mac")?.lastSeen).toBe(50);
  });

  it("controllers are never listed", () => {
    const { rows } = reconcile({}, [controller("phone"), controller("tablet")], 1);
    expect(rows).toEqual([]);
  });

  it("refreshes saved entries seen online", () => {
    const { saved: next } = reconcile(saved, [host("desk", "Renamed", "windows")], 2000);
    expect(next.desk).toEqual({ deviceId: "desk", name: "Renamed", os: "windows", lastSeen: 2000 });
    expect(next.mac?.lastSeen).toBe(50);
  });

  it("an unreachable directory shows every saved machine offline", () => {
    const { rows } = reconcile(saved, null, 1);
    expect(rows.every((r) => r.state === "offline")).toBe(true);
    expect(rows).toHaveLength(2);
  });

  it("remember / forget", () => {
    const added = remember({}, { deviceId: "x", name: "", os: "linux" }, 5);
    expect(added.x).toEqual({ deviceId: "x", name: "x", os: "linux", lastSeen: 5 });
    expect(forget(added, "x")).toEqual({});
  });

  it("last-seen text", () => {
    expect(lastSeenText(null, 0)).toBe("");
    expect(lastSeenText(0, 30_000)).toBe("just now");
    expect(lastSeenText(0, 5 * 60_000)).toBe("5 min ago");
    expect(lastSeenText(0, 3 * 3_600_000)).toBe("3 h ago");
    expect(lastSeenText(0, 2 * 86_400_000)).toBe("2 days ago");
  });
});
