// Cockpit shell: nav switching, link status, view mounting.
// Console (phase 2) is live; dashboards land in phase 3.

import { CockpitSocket } from "/assets/js/ws.js";
import { Console } from "/assets/js/console.js";
import * as dashboard from "/assets/js/dashboard.js";

const socket = new CockpitSocket();
const linkStatus = document.getElementById("link-status");

socket.onStatus((status) => {
  if (status === "open") {
    linkStatus.textContent = "[link ok]";
    linkStatus.className = "status-ok";
  } else if (status === "connecting") {
    linkStatus.textContent = "[link ..]";
    linkStatus.className = "status-dim";
  } else if (status === "version-mismatch") {
    linkStatus.textContent = "[link v!]";
    linkStatus.className = "status-bad";
  } else {
    linkStatus.textContent = "[link --]";
    linkStatus.className = "status-bad";
  }
});

const panel = document.getElementById("panel");
const nav = document.getElementById("nav");
const consoleView = new Console(socket);

const views = {
  agents: dashboard.renderAgents,
  runtime: dashboard.renderRuntime,
  sessions: dashboard.renderSessions,
  graph: dashboard.renderGraph,
  pipelock: dashboard.renderPipelock,
  egress: dashboard.renderEgress,
  tools: dashboard.renderTools,
  skills: dashboard.renderSkills,
};

nav.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-view]");
  if (!button) return;
  for (const other of nav.querySelectorAll("button")) {
    other.classList.toggle("active", other === button);
  }
  renderView(button.dataset.view);
});

async function renderView(name) {
  if (name === "console") {
    consoleView.mount(panel);
    return;
  }
  const render = views[name];
  if (!render) return;
  panel.innerHTML = "";
  const note = document.createElement("div");
  note.className = "label";
  note.textContent = "[ loading… ]";
  panel.append(note);
  try {
    await render(panel, socket);
  } catch (error) {
    panel.innerHTML = "";
    const fail = document.createElement("div");
    fail.className = "status-bad";
    fail.textContent = `[ ${error} ]`;
    panel.append(fail);
  }
}

renderView("console");
socket.subscribe(["agents", "runtime"]);
