// Tether controller. Viewer lands in Module 4; this is the Module 0 scaffold.

const PROTOCOL_VERSION = 1;

// Guarded so importing this module under vitest (Node, no DOM) is side-effect free.
if (typeof document !== "undefined") {
  const app = document.querySelector<HTMLDivElement>("#app");
  if (app) {
    app.textContent = `tether controller scaffold (protocol v${PROTOCOL_VERSION})`;
  }
}

export { PROTOCOL_VERSION };
