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

  const connection = new TetherConnection({
    onStatus(s, detail) {
      status = s;
      dot.className = s;
      connectBtn.textContent = s === "closed" ? "Connect" : "Disconnect";
      stats.textContent = detail ?? "";
      if (s === "connected") canvas.focus();
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
    if (status === "closed") {
      const hostPort = hostInput.value.trim();
      if (!hostPort) return;
      localStorage.setItem("tether-host", hostPort);
      connection.connect(hostPort);
    } else {
      connection.close();
    }
  };
  connectBtn.addEventListener("click", toggle);
  hostInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") toggle();
  });

  setInterval(() => {
    if (status === "connected") {
      stats.textContent = `${viewer.fps} fps · ${canvas.width}×${canvas.height}`;
    }
  }, 500);

  if (hostInput.value && params.has("host")) {
    connection.connect(hostInput.value);
  }
}

if (typeof document !== "undefined") {
  setup();
}
