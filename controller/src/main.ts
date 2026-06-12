import { HostClipboard, PasteFlow } from "./clipboard";
import { TetherConnection, type ConnectionEvents, type Transport } from "./connection";
import { attachInput } from "./input";
import { Viewer } from "./viewer";
import { WebRtcTransport } from "./webrtc";

function setup(): void {
  const app = document.querySelector<HTMLDivElement>("#app");
  if (!app) return;

  app.innerHTML = `
    <div id="bar">
      <span id="dot" class="closed"></span>
      <select id="mode">
        <option value="lan">LAN</option>
        <option value="rtc">Signaled</option>
      </select>
      <span id="lan-fields">
        <input id="host" type="text" placeholder="host:port (e.g. 192.168.1.20:7878)" spellcheck="false" />
      </span>
      <span id="rtc-fields" hidden>
        <input id="signal" type="text" placeholder="signal host:port" spellcheck="false" />
        <input id="secret" type="password" placeholder="secret" />
        <input id="target" type="text" placeholder="host device id" spellcheck="false" />
      </span>
      <button id="connect">Connect</button>
      <button id="clip" hidden title="Copy host clipboard to this device">📋</button>
      <span id="stats"></span>
    </div>
    <div id="stage"><canvas id="view" tabindex="0"></canvas></div>
  `;

  const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;
  const modeSelect = $<HTMLSelectElement>("#mode");
  const hostInput = $<HTMLInputElement>("#host");
  const signalInput = $<HTMLInputElement>("#signal");
  const secretInput = $<HTMLInputElement>("#secret");
  const targetInput = $<HTMLInputElement>("#target");
  const connectBtn = $<HTMLButtonElement>("#connect");
  const dot = $<HTMLSpanElement>("#dot");
  const stats = $<HTMLSpanElement>("#stats");
  const canvas = $<HTMLCanvasElement>("#view");

  const viewer = new Viewer(canvas);
  let status = "closed";
  let wantConnection = false; // user intent: reconnect on unexpected drops
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let active: Transport | null = null;

  const events: ConnectionEvents = {
    onStatus(s, detail) {
      status = s;
      dot.className = s;
      connectBtn.textContent = wantConnection ? "Disconnect" : "Connect";
      stats.textContent = detail ?? "";
      if (s === "connected") canvas.focus();
      if (s === "closed" && wantConnection && !reconnectTimer) {
        stats.textContent = detail ?? "reconnecting…";
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          if (wantConnection) startTransport();
        }, 1000);
      }
    },
    onResolution(r) {
      viewer.setResolution(r);
    },
    onFrame(f) {
      viewer.onFrame(f);
    },
    onClipboard(text) {
      // debug handle: lets tests and consoles inspect the last received
      // clipboard without fighting browser clipboard-read permissions
      (window as unknown as Record<string, unknown>).__tetherLastClipboard = text;
      void hostClipboard.receive(text);
    },
  };

  const clipBtn = $<HTMLButtonElement>("#clip");
  const hostClipboard = new HostClipboard((visible) => {
    clipBtn.hidden = !visible;
  });
  clipBtn.addEventListener("click", () => {
    void hostClipboard.copyNow().then(() => canvas.focus());
  });

  const pasteFlow = new PasteFlow({
    sendClipboard: (text) => active?.sendClipboard(text),
    sendKeyTap: (code, modifiers) => {
      active?.sendInput({ type: "input", kind: "keydown", code, modifiers });
      active?.sendInput({ type: "input", kind: "keyup", code, modifiers });
    },
  });

  // device id for signaling: stable per browser, no setup required
  const deviceId =
    localStorage.getItem("tether-device-id") ??
    `ctl-${Math.random().toString(36).slice(2, 8)}`;
  localStorage.setItem("tether-device-id", deviceId);

  function startTransport(): void {
    active?.close();
    if (modeSelect.value === "lan") {
      const t = new TetherConnection(events);
      active = t;
      t.connect(hostInput.value.trim());
    } else {
      const t = new WebRtcTransport(events);
      active = t;
      t.connect({
        signalUrl: `ws://${signalInput.value.trim()}/ws`,
        secret: secretInput.value,
        deviceId,
        deviceName: deviceId,
        targetHostId: targetInput.value.trim(),
      });
    }
  }

  // input events route to whatever transport is currently active
  attachInput(
    canvas,
    viewer,
    { sendInput: (ev) => active?.sendInput(ev) },
    pasteFlow,
  );

  // field persistence + ?host= / ?mode= shortcuts
  const params = new URLSearchParams(location.search);
  hostInput.value = params.get("host") ?? localStorage.getItem("tether-host") ?? "";
  signalInput.value = params.get("signal") ?? localStorage.getItem("tether-signal") ?? "";
  secretInput.value = localStorage.getItem("tether-secret") ?? "";
  targetInput.value = params.get("target") ?? localStorage.getItem("tether-target") ?? "";
  modeSelect.value = params.get("mode") ?? localStorage.getItem("tether-mode") ?? "lan";

  const syncMode = () => {
    $("#lan-fields").hidden = modeSelect.value !== "lan";
    $("#rtc-fields").hidden = modeSelect.value !== "rtc";
  };
  modeSelect.addEventListener("change", syncMode);
  syncMode();

  const toggle = () => {
    if (!wantConnection) {
      const required =
        modeSelect.value === "lan"
          ? [hostInput.value]
          : [signalInput.value, secretInput.value, targetInput.value];
      if (required.some((v) => !v.trim())) return;
      localStorage.setItem("tether-host", hostInput.value.trim());
      localStorage.setItem("tether-signal", signalInput.value.trim());
      localStorage.setItem("tether-secret", secretInput.value);
      localStorage.setItem("tether-target", targetInput.value.trim());
      localStorage.setItem("tether-mode", modeSelect.value);
      wantConnection = true;
      startTransport();
    } else {
      wantConnection = false;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      active?.close();
      active = null;
      dot.className = "closed";
      connectBtn.textContent = "Connect";
      stats.textContent = "";
      status = "closed";
    }
  };
  connectBtn.addEventListener("click", toggle);
  for (const input of [hostInput, signalInput, secretInput, targetInput]) {
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") toggle();
    });
  }

  setInterval(() => {
    if (status === "connected") {
      // Frame age only means anything when host and controller clocks agree
      // (e.g. same machine); show it when it looks sane, omit otherwise.
      const ageMs = (Date.now() * 1000 - viewer.lastFrameTimestampMicros) / 1000;
      const age = ageMs > 0 && ageMs < 10_000 ? ` · ~${Math.round(ageMs)}ms` : "";
      stats.textContent = `${viewer.fps} fps · ${canvas.width}×${canvas.height}${age}`;
    }
  }, 500);

  if (params.has("host") || (params.get("mode") === "rtc" && params.has("target"))) {
    wantConnection = true;
    startTransport();
  }
}

if (typeof document !== "undefined") {
  setup();
}
