import { TetherConnection } from "./connection";
import { attachInput } from "./input";
import { Viewer } from "./viewer";

function setup(): void {
  const app = document.querySelector<HTMLDivElement>("#app");
  if (!app) return;

  app.innerHTML = `
    <div id="bar">
      <span id="dot" class="closed"></span>
      <input id="host" type="text" placeholder="host:port (e.g. 192.168.1.20:7878)" spellcheck="false" />
      <button id="connect">Connect</button>
      <span id="stats"></span>
    </div>
    <div id="stage"><canvas id="view" tabindex="0"></canvas></div>
  `;

  const hostInput = document.querySelector<HTMLInputElement>("#host")!;
  const connectBtn = document.querySelector<HTMLButtonElement>("#connect")!;
  const dot = document.querySelector<HTMLSpanElement>("#dot")!;
  const stats = document.querySelector<HTMLSpanElement>("#stats")!;
  const canvas = document.querySelector<HTMLCanvasElement>("#view")!;

  const viewer = new Viewer(canvas);
  let status = "closed";
  let wantConnection = false; // user intent: reconnect on unexpected drops
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  const connection = new TetherConnection({
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
          if (wantConnection) connection.connect(hostInput.value.trim());
        }, 1000);
      }
    },
    onResolution(r) {
      viewer.setResolution(r);
    },
    onFrame(f) {
      viewer.onFrame(f);
    },
  });

  attachInput(canvas, viewer, connection);

  // host:port from ?host=, falling back to the last successful value
  const params = new URLSearchParams(location.search);
  hostInput.value = params.get("host") ?? localStorage.getItem("tether-host") ?? "";

  const toggle = () => {
    if (!wantConnection) {
      const hostPort = hostInput.value.trim();
      if (!hostPort) return;
      localStorage.setItem("tether-host", hostPort);
      wantConnection = true;
      connection.connect(hostPort);
    } else {
      wantConnection = false;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      connection.close();
      dot.className = "closed";
      connectBtn.textContent = "Connect";
      stats.textContent = "";
    }
  };
  connectBtn.addEventListener("click", toggle);
  hostInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") toggle();
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

  if (hostInput.value && params.has("host")) {
    wantConnection = true;
    connection.connect(hostInput.value);
  }
}

if (typeof document !== "undefined") {
  setup();
}
