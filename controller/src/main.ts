import { HostClipboard, PasteFlow } from "./clipboard";
import { TetherConnection, type ConnectionEvents, type Transport } from "./connection";
import { attachInput } from "./input";
import { SoftKeyboard } from "./keyboard";
import type { Mode } from "./gestures";
import { Viewer } from "./viewer";
import { WebRtcTransport } from "./webrtc";

/** Coarse pointer + narrow viewport ⇒ phone/tablet; pick trackpad on phones. */
const isTouchDevice = () =>
  typeof matchMedia !== "undefined" && matchMedia("(pointer: coarse)").matches;
const isPhone = () => isTouchDevice() && Math.min(screen.width, screen.height) < 600;

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
      <select id="display" hidden title="Active display"></select>
      <button id="clip" hidden title="Copy host clipboard to this device">📋</button>
      <button id="kbd" hidden title="Toggle keyboard">⌨</button>
      <button id="ptr" hidden title="Pointer mode">🖱</button>
      <button id="full" hidden title="Fullscreen">⛶</button>
      <span id="pair" hidden>
        <input id="paircode" type="text" placeholder="pairing code" spellcheck="false" autocomplete="off" />
        <button id="pairgo">Pair</button>
      </span>
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
      // touch-only affordances appear once connected on a touch device
      const showTouchUi = s === "connected" && isTouchDevice();
      kbdBtn.hidden = !showTouchUi;
      ptrBtn.hidden = !showTouchUi;
      fullBtn.hidden = s !== "connected" || !document.fullscreenEnabled;
      if (s !== "connected") {
        displaySelect.hidden = true; // repopulated by onDisplays
        input?.cancelGesture(); // hide the trackpad cursor + release any held button
      }
      if (s === "connected") {
        pairRow.hidden = true;
        canvas.focus();
      }
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
    onDisplays(displays) {
      // Only offer the picker when there's a real choice.
      displaySelect.hidden = displays.length < 2;
      displaySelect.innerHTML = "";
      for (const d of displays) {
        const opt = document.createElement("option");
        opt.value = String(d.id);
        opt.textContent = `${d.name} (${d.width}×${d.height})`;
        opt.selected = d.active;
        displaySelect.appendChild(opt);
      }
    },
    onPairingRequired() {
      showPairing("Enter the pairing code shown on the host");
    },
    onPairingFailed() {
      showPairing("Wrong or expired code — try again");
    },
  };

  const pairRow = $<HTMLDivElement>("#pair");
  const pairInput = $<HTMLInputElement>("#paircode");
  const pairBtn = $<HTMLButtonElement>("#pairgo");
  const showPairing = (msg: string) => {
    pairRow.hidden = false;
    pairInput.value = "";
    stats.textContent = msg;
    pairInput.focus();
  };
  const submitPair = () => {
    const code = pairInput.value.trim();
    if (code) active?.submitPairingCode(code);
    stats.textContent = "pairing…";
  };
  pairBtn.addEventListener("click", submitPair);
  pairInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") submitPair();
  });

  const clipBtn = $<HTMLButtonElement>("#clip");
  const hostClipboard = new HostClipboard((visible) => {
    clipBtn.hidden = !visible;
  });
  clipBtn.addEventListener("click", () => {
    void hostClipboard.copyNow().then(() => canvas.focus());
  });

  const displaySelect = $<HTMLSelectElement>("#display");
  displaySelect.addEventListener("change", () => {
    active?.selectDisplay(Number(displaySelect.value));
    canvas.focus();
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
    // release any held button on the still-open transport before swapping
    input?.cancelGesture();
    active?.close();
    if (modeSelect.value === "lan") {
      const t = new TetherConnection(events, { deviceId, deviceName: deviceId });
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

  // Touch UI: keyboard, pointer-mode toggle, fullscreen.
  const kbdBtn = $<HTMLButtonElement>("#kbd");
  const ptrBtn = $<HTMLButtonElement>("#ptr");
  const fullBtn = $<HTMLButtonElement>("#full");

  const softKeyboard = new SoftKeyboard({
    sendText: (text) => active?.sendText(text),
    sendKeyTap: (code) => {
      active?.sendInput({ type: "input", kind: "keydown", code, modifiers: 0 });
      active?.sendInput({ type: "input", kind: "keyup", code, modifiers: 0 });
    },
  });
  kbdBtn.addEventListener("click", () => {
    const open = softKeyboard.toggle(); // synchronous: inside the click gesture
    kbdBtn.classList.toggle("on", open);
  });

  // input events route to whatever transport is currently active. Phones
  // default to trackpad (relative cursor); tablets to absolute touch.
  let pointerMode: Mode =
    (localStorage.getItem("tether-ptr-mode") as Mode | null) ??
    (isPhone() ? "trackpad" : "touch");
  const input = attachInput(
    canvas,
    viewer,
    { sendInput: (ev) => active?.sendInput(ev) },
    { pasteFlow, zoomSink: viewer, touchMode: pointerMode },
  );
  const reflectPtr = () => {
    ptrBtn.textContent = pointerMode === "trackpad" ? "🖱" : "👆";
    ptrBtn.title = pointerMode === "trackpad" ? "Trackpad mode" : "Direct touch mode";
  };
  reflectPtr();
  ptrBtn.addEventListener("click", () => {
    pointerMode = pointerMode === "trackpad" ? "touch" : "trackpad";
    localStorage.setItem("tether-ptr-mode", pointerMode);
    input.setMode(pointerMode);
    reflectPtr();
    canvas.focus();
  });

  fullBtn.addEventListener("click", () => {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      void app.requestFullscreen().catch(() => {});
    }
    canvas.focus();
  });

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
      input?.cancelGesture(); // release any held button on the open transport
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
