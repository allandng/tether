// Controller app shell (Phase 8): two views over one small state object.
//
//   list    — your machines, live online/offline from the signal directory,
//             one tap to connect; plus "connect by address" for the LAN path.
//   session — the remote screen and its controls (display picker, clipboard,
//             files, keyboard, pointer mode, fullscreen), with a back action;
//             compact with an overflow sheet on phones/tablets (sessionbar.ts).
//
// Settings (signal server + secret) are entered once and persisted; the
// signal URL defaults to the origin this page was served from, which is what
// the single-origin deployment in deploy/ provides.

import { HostClipboard, PasteFlow, PasteSheet, pasteModifiers, readLocalClipboard } from "./clipboard";
import { type ConnectionEvents, TetherConnection, type Transport } from "./connection";
import { Directory, type DirectoryState } from "./directory";
import { ExtraKeys, KeyToolbar, chord } from "./extrakeys";
import { type DownloadWriter, FileTransfers, type Transfer, baseName, formatBytes } from "./files";
import type { Mode } from "./gestures";
import { attachInput } from "./input";
import { SoftKeyboard } from "./keyboard";
import { ResumeProbe, resumeAction, watchLifecycle } from "./lifecycle";
import {
  type MachineRow,
  type SavedMachines,
  forget,
  lastSeenText,
  machineStore,
  reconcile,
  remember,
} from "./machines";
import { tokenStore } from "./pairing";
import type { InputEvent } from "./protocol";
import { INSTALL_HINT_KEY, isIos, requestPersistentStorage, shouldShowInstallHint } from "./pwa";
import { cmdActsAsCtrl, isAppleClient, remapInput } from "./remap";
import { relayoutSession, setRelayoutHandler, watchLayout } from "./relayout";
import { chooseSaveStrategy, fileFor, shareFile } from "./save";
import { setupSessionBar } from "./sessionbar";
import { type PeerInfo, signalUrl } from "./signaling";
import { Viewer } from "./viewer";
import { trackVisualViewport } from "./viewport";
import { ScreenWakeLock, syncOnUserActivation } from "./wakelock";
import { WebRtcTransport } from "./webrtc";

/** Coarse pointer + narrow viewport ⇒ phone/tablet; pick trackpad on phones. */
const isTouchDevice = () =>
  typeof matchMedia !== "undefined" && matchMedia("(pointer: coarse)").matches;
const isPhone = () => isTouchDevice() && Math.min(screen.width, screen.height) < 600;

const OS_LABEL: Record<string, string> = { windows: "Windows", macos: "macOS", linux: "Linux" };
const HOST_OS_NAME: Record<number, string> = { 1: "macos", 2: "windows", 3: "linux" };

type Target =
  | { kind: "rtc"; deviceId: string; name: string }
  | { kind: "lan"; hostPort: string };

const store = {
  get: (k: string) => {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set: (k: string, v: string) => {
    try {
      localStorage.setItem(k, v);
    } catch {
      // private mode: settings last for this page only
    }
  },
};

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);

function setup(): void {
  const app = document.querySelector<HTMLDivElement>("#app");
  if (!app) return;

  app.innerHTML = `
    <section id="list-view">
      <header class="top">
        <h1>Tether</h1>
        <span id="dir-state" class="muted"></span>
        <button id="open-settings" class="icon" title="Settings" aria-label="Settings">⚙</button>
      </header>
      <div id="install-hint" class="hint" hidden>
        <p><strong>Install Tether:</strong> tap Share, then <em>Add to Home Screen</em>.
          Safari may clear a tab's settings and pairings after a week unused; the app keeps them.
          The installed app has storage of its own, so set up and pair your machines there.</p>
        <button id="install-dismiss" class="icon small" type="button" aria-label="Dismiss">✕</button>
      </div>
      <ul id="machines"></ul>
      <div id="empty" class="empty" hidden></div>
      <details id="lan-box">
        <summary>Connect by address (LAN)</summary>
        <form id="lan-form">
          <input id="lan-host" type="text" placeholder="192.168.1.20:7878" inputmode="url" enterkeyhint="go" autocorrect="off" autocapitalize="off" spellcheck="false" autocomplete="off" />
          <button class="primary" type="submit">Connect</button>
        </form>
      </details>
    </section>

    <section id="session-view" hidden>
      <div id="bar">
        <button id="back" class="icon" title="Back to machines" aria-label="Back">←</button>
        <span id="dot" class="closed"></span>
        <span id="ident"><span id="title"></span><span id="stats"></span></span>
        <button id="kbd" class="icon" hidden title="Toggle keyboard">⌨</button>
        <button id="ptr" class="icon" hidden title="Pointer mode">🖱</button>
        <div id="more">
          <select id="display" hidden title="Active display" aria-label="Active display"></select>
          <button id="clip" class="icon" hidden title="Copy host clipboard to this device" aria-label="Copy host clipboard" data-label="Copy host clipboard" data-attn>📋</button>
          <button id="files-btn" class="icon" hidden title="Files" aria-label="Files" data-label="Files">📁</button>
          <button id="speaker" class="icon" hidden title="Host audio (muted)" data-label="Host audio">🔇</button>
          <button id="full" class="icon" hidden title="Fullscreen" aria-label="Fullscreen" data-label="Fullscreen">⛶</button>
          <button id="autohide" class="icon sheet-only" title="Hide the bar after a few seconds; tap the tab at the top to bring it back" aria-label="Auto-hide bar" aria-pressed="false" data-label="Auto-hide bar">⤒</button>
          <button id="bar-settings" class="icon sheet-only" title="Settings" aria-label="Settings" data-label="Settings">⚙</button>
        </div>
        <div id="more-scrim"></div>
        <button id="more-btn" class="icon" title="More controls" aria-label="More controls" aria-controls="more" aria-expanded="false">⋯</button>
        <span id="pair" hidden>
          <input id="paircode" type="text" placeholder="pairing code" enterkeyhint="go" autocorrect="off" autocapitalize="characters" spellcheck="false" autocomplete="off" />
          <button id="pairgo" class="primary">Pair</button>
        </span>
      </div>
      <button id="bar-handle" hidden title="Show controls" aria-label="Show controls"></button>
      <div id="stage">
        <canvas id="view" tabindex="0"></canvas>
        <button id="zoom-reset" hidden title="Reset zoom" aria-label="Reset zoom">1×</button>
      </div>
      <div id="xkeys" hidden></div>
      <audio id="host-audio" autoplay playsinline muted></audio>
      <aside id="files" hidden>
        <header class="top">
          <h2>Files</h2>
          <button id="files-close" class="icon" aria-label="Close">✕</button>
        </header>
        <label class="primary upload">Upload to host…<input id="upload" type="file" multiple hidden /></label>
        <p class="muted small">Uploads land in the host's Downloads folder.</p>
        <form id="download-form">
          <input id="download-path" type="text" placeholder="Host file path, e.g. C:\\Users\\me\\report.pdf" enterkeyhint="go" autocorrect="off" autocapitalize="off" spellcheck="false" autocomplete="off" />
          <button class="primary" type="submit">Download</button>
        </form>
        <ul id="transfers"></ul>
      </aside>
    </section>

    <dialog id="settings">
      <form method="dialog" id="settings-form">
        <h2>Settings</h2>
        <label>Signal server
          <input id="set-signal" type="text" placeholder="tether.example.com" inputmode="url" autocorrect="off" autocapitalize="off" spellcheck="false" autocomplete="off" />
        </label>
        <label>Secret
          <input id="set-secret" type="password" autocomplete="off" />
        </label>
        <label>This device's name
          <input id="set-name" type="text" spellcheck="false" autocomplete="off" />
        </label>
        <h3>Saved machines</h3>
        <ul id="saved"></ul>
        <div class="row">
          <button value="cancel" formnovalidate>Cancel</button>
          <button id="settings-save" class="primary" value="save">Save</button>
        </div>
      </form>
    </dialog>
  `;

  const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel)!;
  const listView = $<HTMLElement>("#list-view");
  const sessionView = $<HTMLElement>("#session-view");
  const machinesEl = $<HTMLUListElement>("#machines");
  const emptyEl = $<HTMLDivElement>("#empty");
  const dirStateEl = $<HTMLSpanElement>("#dir-state");
  const dot = $<HTMLSpanElement>("#dot");
  const titleEl = $<HTMLSpanElement>("#title");
  const stats = $<HTMLSpanElement>("#stats");
  const canvas = $<HTMLCanvasElement>("#view");
  const settingsDlg = $<HTMLDialogElement>("#settings");

  // ---- persistent identity + settings -------------------------------------
  const deviceId =
    store.get("tether-device-id") ?? `ctl-${Math.random().toString(36).slice(2, 8)}`;
  store.set("tether-device-id", deviceId);
  const params = new URLSearchParams(location.search);
  if (params.has("signal")) store.set("tether-signal", params.get("signal") ?? "");
  const settings = {
    get signal() {
      return store.get("tether-signal") ?? location.host;
    },
    get secret() {
      return store.get("tether-secret") ?? "";
    },
    get name() {
      return store.get("tether-name") ?? deviceId;
    },
  };

  // ---- machine list --------------------------------------------------------
  let saved: SavedMachines = machineStore.load();
  let livePeers: PeerInfo[] | null = null;
  let dirState: DirectoryState = "off";
  let dirDetail = "";

  const renderList = () => {
    const now = Date.now();
    const result = reconcile(saved, livePeers, now);
    saved = result.saved;
    machineStore.save(saved);
    machinesEl.innerHTML = "";
    for (const row of result.rows) machinesEl.appendChild(machineItem(row, now));

    const configured = settings.secret !== "";
    dirStateEl.textContent = !configured
      ? ""
      : dirState === "online"
        ? "online"
        : dirState === "error"
          ? `signal error: ${dirDetail}`
          : dirState === "connecting"
            ? `connecting… ${dirDetail}`
            : "";
    dirStateEl.className = `muted state-${dirState}`;
    emptyEl.hidden = result.rows.length > 0;
    emptyEl.innerHTML = !configured
      ? `<p>Add your signal server and secret to see your machines.</p>
         <button class="primary" data-act="settings">Open settings</button>`
      : `<p>No machines yet. Install the host on a computer with the same signal server;
         it appears here. Tapping it asks for a pairing code — run <code>tetherd pair</code>
         on that computer to get one.</p>`;
  };

  const machineItem = (row: MachineRow, now: number) => {
    const li = document.createElement("li");
    const btn = document.createElement("button");
    btn.className = `machine ${row.state}`;
    btn.disabled = row.state === "offline";
    const sub =
      row.state === "online"
        ? "Online"
        : row.state === "new"
          ? "New — tap to pair"
          : `Offline · ${lastSeenText(row.lastSeen, now)}`;
    btn.innerHTML = `
      <span class="os os-${esc(row.os || "unknown")}">${esc(OS_LABEL[row.os] ?? "Host")}</span>
      <span class="name">${esc(row.name)}<small>${esc(sub)}</small></span>
      <span class="status ${row.state}"></span>`;
    btn.addEventListener("click", () =>
      connectTo({ kind: "rtc", deviceId: row.deviceId, name: row.name }),
    );
    li.appendChild(btn);
    return li;
  };

  emptyEl.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).dataset.act === "settings") openSettings();
  });

  const directory = new Directory({
    onHosts(peers) {
      livePeers = peers;
      renderList();
    },
    onState(state, detail) {
      dirState = state;
      dirDetail = detail ?? "";
      if (state !== "online") livePeers = state === "connecting" && livePeers ? livePeers : null;
      renderList();
    },
  });
  const startDirectory = () => {
    if (!settings.secret) {
      directory.stop();
      return;
    }
    let url: string;
    try {
      url = signalUrl(settings.signal);
    } catch {
      return;
    }
    directory.start({ url, secret: settings.secret, deviceId, deviceName: settings.name });
  };

  // ---- settings dialog ----------------------------------------------------
  const setSignal = $<HTMLInputElement>("#set-signal");
  const setSecret = $<HTMLInputElement>("#set-secret");
  const setName = $<HTMLInputElement>("#set-name");
  const savedEl = $<HTMLUListElement>("#saved");
  const renderSaved = () => {
    savedEl.innerHTML = "";
    const all = Object.values(saved);
    if (all.length === 0) savedEl.innerHTML = `<li class="muted">None yet.</li>`;
    for (const m of all) {
      const li = document.createElement("li");
      li.innerHTML = `<span>${esc(m.name)} <small class="muted">${esc(OS_LABEL[m.os] ?? "")}</small></span>`;
      const del = document.createElement("button");
      del.type = "button";
      del.textContent = "Forget";
      del.addEventListener("click", () => {
        saved = forget(saved, m.deviceId);
        tokenStore.set(m.deviceId, ""); // forget the pairing token too
        machineStore.save(saved);
        renderSaved();
        renderList();
      });
      li.appendChild(del);
      savedEl.appendChild(li);
    }
  };
  const openSettings = () => {
    setSignal.value = settings.signal;
    setSecret.value = settings.secret;
    setName.value = settings.name;
    renderSaved();
    settingsDlg.showModal();
  };
  $("#open-settings").addEventListener("click", openSettings);
  settingsDlg.addEventListener("close", () => {
    if (settingsDlg.returnValue !== "save") return;
    store.set("tether-signal", setSignal.value.trim());
    store.set("tether-secret", setSecret.value);
    store.set("tether-name", setName.value.trim() || deviceId);
    startDirectory();
    renderList();
  });

  // ---- install hint (iOS browser tab; see pwa.ts) --------------------------
  const ios = isIos(navigator);
  const installHint = $<HTMLDivElement>("#install-hint");
  installHint.hidden = !shouldShowInstallHint({
    userAgent: navigator.userAgent,
    maxTouchPoints: navigator.maxTouchPoints,
    standalone: (navigator as Navigator & { standalone?: boolean }).standalone,
    displayModeStandalone:
      typeof matchMedia !== "undefined" && matchMedia("(display-mode: standalone)").matches,
    dismissed: store.get(INSTALL_HINT_KEY) === "1",
  });
  $("#install-dismiss").addEventListener("click", () => {
    store.set(INSTALL_HINT_KEY, "1");
    installHint.hidden = true;
  });

  // ---- session --------------------------------------------------------------
  const viewer = new Viewer(canvas);
  const RECONNECT_BASE_MS = 1000;
  const RECONNECT_MAX_MS = 30_000;
  let status = "closed";
  let statusSince = Date.now(); // when `status` last changed (lifecycle.ts)
  let wantConnection = false; // user intent: reconnect on unexpected drops
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let reconnectDelay = RECONNECT_BASE_MS; // grows on repeated failure, resets on connect
  let active: Transport | null = null;
  let target: Target | null = null;
  let hostOs: number | null = null;
  let cmdAsCtrl = false;
  let files: FileTransfers | null = null;
  const appleClient = isAppleClient();
  // Held while connected so a phone that's only watching doesn't auto-lock.
  const wakeLock = new ScreenWakeLock(navigator, () => document.visibilityState === "visible");
  syncOnUserActivation(wakeLock, document); // WebKit only grants it inside a gesture

  /** Every input event leaves through here, after the per-host modifier policy
   * — unless `native`: the special-keys toolbar's chords already name host
   * keys (its Win key must stay Win, not become Ctrl). */
  const send = (ev: InputEvent, native = false) =>
    active?.sendInput(native ? ev : remapInput(ev, cmdAsCtrl));

  const showView = (view: "list" | "session") => {
    listView.hidden = view !== "list";
    sessionView.hidden = view !== "session";
    if (view === "list") renderList();
  };

  const events: ConnectionEvents = {
    onStatus(s, detail, fatal) {
      status = s;
      statusSince = Date.now();
      dot.className = s;
      // A fatal close (wrong secret, bad target, protocol mismatch) won't fix
      // itself — stop the auto-reconnect loop and leave the reason on screen.
      if (s === "closed" && fatal) {
        wantConnection = false;
        if (reconnectTimer) {
          clearTimeout(reconnectTimer);
          reconnectTimer = null;
        }
      }
      if (s === "connected") reconnectDelay = RECONNECT_BASE_MS; // reset backoff
      if (s === "connected") void wakeLock.acquire();
      else void wakeLock.release(); // a host that's down mustn't keep the screen on
      stats.textContent = detail ?? "";
      // touch-only affordances appear once connected on a touch device
      const showTouchUi = s === "connected" && isTouchDevice();
      kbdBtn.hidden = !showTouchUi;
      if (!showTouchUi) softKeyboard.close(); // its toggle is gone: don't strand it open
      ptrBtn.hidden = !showTouchUi;
      // Fullscreen API is iPad-only on iOS; installed (standalone) iPhones
      // are already chromeless.
      fullBtn.hidden = s !== "connected" || !document.fullscreenEnabled;
      filesBtn.hidden = s !== "connected";
      if (s !== "connected") {
        speakerBtn.hidden = true; // reappears when the next track arrives
        hostAudio.srcObject = null;
      }
      if (s !== "connected") {
        displaySelect.hidden = true; // repopulated by onDisplays
        input?.cancelGesture(); // hide the trackpad cursor + release any held button
        files?.abortAll();
      }
      if (s === "connected") {
        // A pairing just succeeded: ask that its token (and our identity)
        // survive storage eviction. Best effort.
        if (!pairRow.hidden) void requestPersistentStorage(navigator);
        pairRow.hidden = true;
        refocus();
        if (target?.kind === "rtc") {
          saved = remember(
            saved,
            { deviceId: target.deviceId, name: target.name, os: HOST_OS_NAME[hostOs ?? 0] ?? "" },
            Date.now(),
          );
          machineStore.save(saved);
        }
      }
      if (s === "closed" && wantConnection && !reconnectTimer) {
        // Exponential backoff with jitter so a persistently-down host isn't
        // hammered once a second (and the reason stays visible).
        const wait = Math.round(reconnectDelay * (0.7 + Math.random() * 0.6));
        reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
        stats.textContent = `${detail ?? "disconnected"} — retrying in ${Math.round(wait / 1000)}s`;
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          if (wantConnection) startTransport();
        }, wait);
      }
      sessionBar.sync(); // auto-hide only while connected
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
    onHostInfo(info) {
      hostOs = info.os;
      cmdAsCtrl = cmdActsAsCtrl(info.os, appleClient);
      toolbar.setHostOs(info.os);
      const os = OS_LABEL[HOST_OS_NAME[info.os] ?? ""];
      titleEl.textContent = os ? `${info.name} · ${os}` : info.name;
      if (target?.kind === "rtc") {
        saved = remember(
          saved,
          { deviceId: target.deviceId, name: target.name, os: HOST_OS_NAME[info.os] ?? "" },
          Date.now(),
        );
        machineStore.save(saved);
      }
    },
    onFileMessage(msg) {
      files?.onMessage(msg);
    },
    onAudioTrack(stream) {
      hostAudio.srcObject = stream;
      speakerBtn.hidden = false;
      playHostAudio();
    },
    onPairingRequired() {
      showPairing("Enter the pairing code shown on the host");
    },
    onPairingFailed() {
      showPairing("Wrong or expired code — try again");
    },
  };

  const pairRow = $<HTMLSpanElement>("#pair");
  const pairInput = $<HTMLInputElement>("#paircode");
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
  $("#pairgo").addEventListener("click", submitPair);
  pairInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") submitPair();
  });

  const clipBtn = $<HTMLButtonElement>("#clip");
  const hostClipboard = new HostClipboard((visible) => {
    clipBtn.hidden = !visible;
  });
  clipBtn.addEventListener("click", () => {
    void hostClipboard.copyNow().then(() => refocus());
  });

  const displaySelect = $<HTMLSelectElement>("#display");
  displaySelect.addEventListener("change", () => {
    active?.selectDisplay(Number(displaySelect.value));
    refocus();
  });

  const pasteFlow = new PasteFlow({
    sendClipboard: (text) => active?.sendClipboard(text),
    sendKeyTap: (code, modifiers) => {
      send({ type: "input", kind: "keydown", code, modifiers });
      send({ type: "input", kind: "keyup", code, modifiers });
    },
  });

  function startTransport(): void {
    if (!target) return;
    // release any held button on the still-open transport before swapping
    input?.cancelGesture();
    active?.close();
    files?.abortAll();
    viewer.resetStream(); // the new connection's video starts at its own keyframe
    hostOs = null;
    cmdAsCtrl = false;
    toolbar.setHostOs(null);
    let transport: Transport;
    if (target.kind === "lan") {
      const t = new TetherConnection(events, { deviceId, deviceName: settings.name });
      transport = t;
      t.connect(target.hostPort);
    } else {
      const t = new WebRtcTransport(events);
      transport = t;
      t.connect({
        signalUrl: signalUrl(settings.signal),
        secret: settings.secret,
        deviceId,
        deviceName: settings.name,
        targetHostId: target.deviceId,
      });
    }
    active = transport;
    files = new FileTransfers(
      { send: (m) => transport.sendFile(m), drain: () => transport.drainFiles() },
      renderTransfer,
      {
        maxMemoryDownload: (isTouchDevice() ? 100 : 1024) * 1024 * 1024,
        save: saveBlob,
      },
    );
  }

  function connectTo(t: Target): void {
    target = t;
    titleEl.textContent = t.kind === "rtc" ? t.name : t.hostPort;
    clearTransfers();
    filesPanel.hidden = true;
    showView("session");
    wantConnection = true;
    reconnectDelay = RECONNECT_BASE_MS;
    startTransport();
  }

  function disconnect(): void {
    wantConnection = false;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    input?.cancelGesture(); // release any held button on the open transport
    files?.abortAll("disconnected");
    active?.close();
    active = null;
    files = null;
    dot.className = "closed";
    stats.textContent = "";
    status = "closed";
    statusSince = Date.now();
    void wakeLock.release();
    clearTransfers(); // unreachable from the list: don't keep unsaved Blobs alive
    softKeyboard.blur();
    if (document.fullscreenElement) void document.exitFullscreen();
    sessionBar.closeSheet();
    sessionBar.sync();
  }

  $("#back").addEventListener("click", () => {
    disconnect();
    showView("list");
  });

  $<HTMLFormElement>("#lan-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const hostPort = $<HTMLInputElement>("#lan-host").value.trim();
    if (!hostPort) return;
    store.set("tether-host", hostPort);
    connectTo({ kind: "lan", hostPort });
  });
  $<HTMLInputElement>("#lan-host").value = store.get("tether-host") ?? "";

  // ---- files -----------------------------------------------------------------
  const filesBtn = $<HTMLButtonElement>("#files-btn");
  const filesPanel = $<HTMLElement>("#files");
  const transfersEl = $<HTMLUListElement>("#transfers");
  filesBtn.addEventListener("click", () => {
    filesPanel.hidden = !filesPanel.hidden;
  });
  $("#files-close").addEventListener("click", () => {
    filesPanel.hidden = true;
    refocus();
  });

  const transferRows = new Map<number, { li: HTMLLIElement; t: Transfer }>();
  // Finished downloads waiting on a "Save…" tap (iOS share sheet, save.ts).
  // Dropped once shared, when their row is dismissed, or with the list.
  const unsaved = new Map<number, { file: File; note?: string }>();
  function renderTransfer(t: Transfer): void {
    let row = transferRows.get(t.id);
    if (!row) {
      row = { li: document.createElement("li"), t };
      transferRows.set(t.id, row);
      transfersEl.prepend(row.li);
    }
    row.t = t;
    const li = row.li;
    const held = t.state === "complete" ? unsaved.get(t.id) : undefined;
    const pct = t.size > 0 ? Math.min(100, Math.round((t.done / t.size) * 100)) : 0;
    const arrow = t.direction === "upload" ? "↑" : "↓";
    const detail =
      t.state === "error"
        ? esc(t.error ?? "failed")
        : t.state === "cancelled"
          ? "cancelled"
          : held
            ? (held.note ?? `ready · ${formatBytes(t.size)}`)
            : t.state === "complete"
              ? `done · ${formatBytes(t.size)}`
              : t.state === "waiting"
                ? "waiting for host…"
                : `${formatBytes(t.done)} of ${formatBytes(t.size)}`;
    li.className = `transfer ${t.state}`;
    li.innerHTML = `
      <div class="t-head"><span>${arrow} ${esc(t.name)}</span>
        ${t.state === "active" || t.state === "waiting" ? `<button class="icon small" data-cancel="${t.id}" aria-label="Cancel">✕</button>` : ""}
        ${held ? `<button class="icon small" data-dismiss="${t.id}" aria-label="Discard">✕</button>` : ""}
      </div>
      <progress max="100" value="${t.state === "complete" ? 100 : pct}"></progress>
      <div class="muted small">${detail}</div>
      ${held ? `<button class="primary save" data-save="${t.id}">Save…</button>` : ""}`;
  }
  function clearTransfers(): void {
    transfersEl.innerHTML = "";
    transferRows.clear();
    unsaved.clear();
  }
  const saveHeld = (id: number) => {
    const held = unsaved.get(id);
    if (!held) return;
    // Straight from the tap: share() needs this gesture.
    void shareFile(navigator, held.file).then((outcome) => {
      if (outcome === "shared") unsaved.delete(id);
      held.note = outcome === "failed" ? "couldn't open the share sheet — try again" : undefined;
      const row = transferRows.get(id);
      if (row) renderTransfer(row.t);
    });
  };
  transfersEl.addEventListener("click", (e) => {
    const { cancel, save, dismiss } = (e.target as HTMLElement).dataset;
    if (cancel) files?.cancel(Number(cancel));
    if (save) saveHeld(Number(save));
    if (dismiss) {
      const id = Number(dismiss);
      unsaved.delete(id);
      transferRows.get(id)?.li.remove();
      transferRows.delete(id);
    }
  });

  const uploadFiles = (list: FileList | File[]) => {
    for (const f of Array.from(list)) files?.upload(f, f.name);
    filesPanel.hidden = false;
  };
  const uploadInput = $<HTMLInputElement>("#upload");
  uploadInput.addEventListener("change", () => {
    if (uploadInput.files) uploadFiles(uploadInput.files);
    uploadInput.value = "";
  });
  // desktop nicety: drop files onto the remote screen to upload them
  const stage = $<HTMLDivElement>("#stage");
  stage.addEventListener("dragover", (e) => {
    if (status === "connected" && e.dataTransfer?.types.includes("Files")) e.preventDefault();
  });
  stage.addEventListener("drop", (e) => {
    if (status !== "connected" || !e.dataTransfer?.files.length) return;
    e.preventDefault();
    uploadFiles(e.dataTransfer.files);
  });

  function saveBlob(name: string, blob: Blob, id: number): void {
    const file = fileFor(name, blob);
    if (chooseSaveStrategy({ ios, nav: navigator }, file) === "share") {
      unsaved.set(id, { file }); // its row renders the Save… button
      filesPanel.hidden = false; // that tap is the user's next step
      return;
    }
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }

  type SavePicker = (opts: { suggestedName: string }) => Promise<{
    createWritable(): Promise<DownloadWriter>;
  }>;
  $<HTMLFormElement>("#download-form").addEventListener("submit", (e) => {
    e.preventDefault();
    const pathInput = $<HTMLInputElement>("#download-path");
    const path = pathInput.value.trim();
    if (!path || !files) return;
    store.set("tether-last-download", path);
    // Chromium can stream straight to disk (no size cap); the picker must open
    // inside this click, before we know anything from the host.
    const picker = (window as unknown as { showSaveFilePicker?: SavePicker }).showSaveFilePicker;
    if (picker) {
      picker({ suggestedName: baseName(path) })
        .then((h) => h.createWritable())
        .then(
          (writer) => files?.download(path, writer),
          () => {}, // picker dismissed: nothing to do
        );
    } else {
      files.download(path);
    }
  });
  $<HTMLInputElement>("#download-path").value = store.get("tether-last-download") ?? "";

  // ---- host audio -------------------------------------------------------------
  // Starts muted: iOS refuses playback without a gesture, and a room's sound
  // shouldn't blare out of a phone unasked. The first tap both unmutes and is
  // that gesture.
  const hostAudio = $<HTMLAudioElement>("#host-audio");
  const speakerBtn = $<HTMLButtonElement>("#speaker");
  const reflectSpeaker = () => {
    speakerBtn.textContent = hostAudio.muted ? "🔇" : "🔊";
    speakerBtn.title = hostAudio.muted ? "Host audio (muted)" : "Host audio (playing)";
    speakerBtn.classList.toggle("on", !hostAudio.muted);
  };
  // Each (re)connect attaches a fresh stream that needs play() again. Muted
  // playback is always allowed; unmuted can be refused without a fresh gesture
  // (iOS, after a reconnect or a resume) — then drop back to muted, so the 🔇
  // button is the tap that starts it.
  const playHostAudio = () => {
    reflectSpeaker();
    hostAudio.play().catch((e: unknown) => {
      if ((e as { name?: string } | null)?.name !== "NotAllowedError" || hostAudio.muted) return;
      hostAudio.muted = true;
      reflectSpeaker();
    });
  };
  speakerBtn.addEventListener("click", () => {
    hostAudio.muted = !hostAudio.muted;
    if (!hostAudio.muted) void hostAudio.play().catch(() => {});
    reflectSpeaker();
    refocus();
  });

  // ---- touch UI: keyboard, pointer-mode toggle, fullscreen ------------------
  const kbdBtn = $<HTMLButtonElement>("#kbd");
  const ptrBtn = $<HTMLButtonElement>("#ptr");
  const fullBtn = $<HTMLButtonElement>("#full");

  // Everything the soft keyboard sends goes through the toolbar's sticky
  // modifiers; its chords press modifier keys explicitly (the Windows injector
  // ignores the mask) and name host keys, so they're sent `native`.
  const extraKeys = new ExtraKeys({
    key: (ev) => send(ev, true),
    text: (text) => active?.sendText(text),
    changed: () => toolbar.render(),
  });
  const softKeyboard = new SoftKeyboard({
    sendText: (text) => extraKeys.type(text),
    // Field keys carry the client's modifiers (an iPad keyboard's ⌘): apply
    // the host policy first, then chord with any toolbar latches.
    sendKeyTap: (code, modifiers) => {
      const ev = remapInput({ type: "input", kind: "keydown", code, modifiers }, cmdAsCtrl);
      if (ev.kind === "keydown") extraKeys.tap(ev.code, ev.modifiers);
    },
    modifiersLatched: () => extraKeys.latched,
  });
  // The field can lose focus on its own (iOS "Done", a tap elsewhere), so the
  // button mirrors the keyboard's state rather than its own clicks.
  softKeyboard.onChange((open) => kbdBtn.classList.toggle("on", open));
  kbdBtn.addEventListener("mousedown", (e) => e.preventDefault()); // don't blur the field first
  kbdBtn.addEventListener("click", () => {
    softKeyboard.toggle(); // synchronous: inside the click gesture
  });
  /** Hand keystroke focus back after a control is used: the soft keyboard's
   * field while it's open (focusing the canvas would dismiss it), else the canvas. */
  const refocus = () => {
    if (softKeyboard.isOpen) softKeyboard.open();
    else canvas.focus();
  };

  // ---- special keys + phone paste ---------------------------------------------
  // Shown while the soft keyboard's field has focus — the on-screen keyboard,
  // or an iPad hardware keyboard after ⌨. Latches don't outlive it.
  const toolbar = new KeyToolbar($("#xkeys"), extraKeys.sticky, {
    key: (code) => softKeyboard.sendKey(code), // moves the host caret: drops the context
    modifier: (mod) => {
      extraKeys.sticky.tap(mod, performance.now());
      softKeyboard.resetContext(); // the next keystroke is a chord, never an autocorrect edit
      toolbar.render();
    },
    paste: () => pasteFromDevice(),
  });
  softKeyboard.onChange((open) => {
    toolbar.visible = open;
    if (!open) extraKeys.clear();
  });

  // A button paste holds no physical modifier, so its V tap is a full chord;
  // through send(), Cmd+V becomes Ctrl+V on Windows hosts as from a keyboard.
  const buttonPaste = new PasteFlow({
    sendClipboard: (text) => active?.sendClipboard(text),
    sendKeyTap: (code, modifiers) => {
      for (const ev of chord(modifiers, code)) send(ev);
    },
  });
  const pasteText = (text: string) => {
    // The host caret moves past the paste: an autocorrect of the word typed
    // before it would otherwise Backspace into the pasted text.
    softKeyboard.resetContext();
    buttonPaste.pasteText(text, pasteModifiers(hostOs, cmdAsCtrl));
  };
  const pasteSheet = new PasteSheet(pasteText, () => {
    if (status === "connected") softKeyboard.open(); // still inside the tap
  });
  const pasteFromDevice = () => {
    const read = readLocalClipboard(); // synchronous: iOS asks to confirm now
    if (!read) {
      pasteSheet.open(); // no Clipboard API (plain-HTTP page)
      return;
    }
    // Refused (NotAllowedError) or failed: quietly offer the paste box instead.
    void read.then((text) => (text === null ? pasteSheet.open() : pasteText(text)));
  };

  // input events route to whatever transport is currently active. Phones
  // default to trackpad (relative cursor); tablets to absolute touch.
  let pointerMode: Mode =
    (store.get("tether-ptr-mode") as Mode | null) ?? (isPhone() ? "trackpad" : "touch");
  const input = attachInput(
    canvas,
    viewer,
    { sendInput: send },
    {
      pasteFlow,
      zoomSink: viewer,
      touchMode: pointerMode,
      // A tap keeps the open keyboard's focus, but may move the host caret,
      // so the autocorrect context is stale.
      shouldFocusCanvas: () => {
        if (!softKeyboard.isOpen) return true;
        softKeyboard.resetContext();
        return false;
      },
    },
  );
  const reflectPtr = () => {
    ptrBtn.textContent = pointerMode === "trackpad" ? "🖱" : "👆";
    ptrBtn.title = pointerMode === "trackpad" ? "Trackpad mode" : "Direct touch mode";
  };
  reflectPtr();
  ptrBtn.addEventListener("click", () => {
    pointerMode = pointerMode === "trackpad" ? "touch" : "trackpad";
    store.set("tether-ptr-mode", pointerMode);
    input.setMode(pointerMode);
    reflectPtr();
    refocus();
  });

  // ---- zoom reset chip + layout changes ---------------------------------------
  // The chip shows only while pinch-zoomed. It must not take focus (that
  // would steal it from the canvas or close the soft keyboard).
  const zoomReset = $<HTMLButtonElement>("#zoom-reset");
  viewer.onZoomedChange = (zoomed) => {
    zoomReset.hidden = !zoomed;
  };
  zoomReset.addEventListener("mousedown", (e) => e.preventDefault());
  zoomReset.addEventListener("click", () => {
    viewer.resetZoom();
    relayoutSession();
  });
  setRelayoutHandler(() => {
    viewer.relayout();
    input.relayout();
  });
  watchLayout(stage);

  fullBtn.addEventListener("click", () => {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      void sessionView.requestFullscreen().catch(() => {});
    }
    refocus();
  });

  // ---- session bar: compact sheet + auto-hide (sessionbar.ts) -----------------
  const sessionBar = setupSessionBar({
    view: sessionView,
    connected: () => status === "connected",
    holds: [pairRow, filesPanel, settingsDlg],
    refocus,
    store,
  });
  $("#bar-settings").addEventListener("click", openSettings);

  // ---- app lifecycle: backgrounded, locked, back again (lifecycle.ts) -------
  const reconnectNow = () => {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    reconnectDelay = RECONNECT_BASE_MS;
    startTransport();
  };
  const resumeProbe = new ResumeProbe();
  watchLifecycle(document, window, {
    onHidden() {
      // a finger mid-drag never lifts in a frozen page: release it now
      input.cancelGesture();
      resumeProbe.cancel(); // judged afresh on the next resume
    },
    onResume(_reason, frozenMs) {
      void wakeLock.sync(); // the system dropped it while hidden
      resumeProbe.cancel();
      const transport = active;
      const action = resumeAction({
        wantConnection,
        status,
        transportAlive: transport?.connected ?? false,
        statusAgeMs: Date.now() - statusSince,
        frozenMs,
        rxAgeMs: transport ? Date.now() - transport.lastRxAt : Infinity,
      });
      if (action === "reconnect") {
        reconnectNow();
        return;
      }
      if (hostAudio.srcObject && hostAudio.paused) playHostAudio(); // iOS may pause it
      if (action === "verify" && transport) {
        // Quiet since before the gap: give the host a moment to show it's
        // still there before tearing the session (and its transfers) down.
        resumeProbe.start(
          () => transport.lastRxAt,
          () => {
            if (active === transport && wantConnection && status === "connected") reconnectNow();
          },
        );
      }
    },
  });

  setInterval(() => {
    if (status === "connected") {
      // Frame age only means anything when host and controller clocks agree
      // (e.g. same machine); show it when it looks sane, omit otherwise.
      const ageMs = (Date.now() * 1000 - viewer.lastFrameTimestampMicros) / 1000;
      const age = ageMs > 0 && ageMs < 10_000 ? ` · ~${Math.round(ageMs)}ms` : "";
      stats.textContent = `${viewer.fps} fps · ${canvas.width}×${canvas.height}${age}`;
    }
  }, 500);
  // keep "last seen" labels fresh while the list is showing
  setInterval(() => {
    if (!listView.hidden) renderList();
  }, 30_000);

  // ---- start ----------------------------------------------------------------
  trackVisualViewport(); // keyboard-aware layout: CSS vars on :root
  startDirectory();
  // Deep links (kept from the single-bar UI): ?host=ip:port connects over LAN,
  // ?mode=rtc&target=<device id> straight to a machine.
  if (params.has("host")) {
    connectTo({ kind: "lan", hostPort: params.get("host") ?? "" });
  } else if (params.has("target")) {
    const id = params.get("target") ?? "";
    connectTo({ kind: "rtc", deviceId: id, name: saved[id]?.name ?? id });
  } else {
    showView("list");
  }
}

if (typeof document !== "undefined") {
  setup();
}
