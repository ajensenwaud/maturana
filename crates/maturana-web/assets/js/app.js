// Cockpit shell: nav switching, link status, view mounting.
// Console (phase 2) is live; dashboards land in phase 3.

import { CockpitSocket } from "/assets/js/ws.js";
import { Console } from "/assets/js/console.js";

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

const placeholders = {
  agents: "Agent fleet — phase 3",
  runtime: "Runtime plane — phase 3",
  sessions: "Sessions — phase 3",
  graph: "Knowledge graph — phase 3",
  pipelock: "Pipelock — phase 3",
  tools: "Tools — phase 3",
  skills: "Skills — phase 3",
};

nav.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-view]");
  if (!button) return;
  for (const other of nav.querySelectorAll("button")) {
    other.classList.toggle("active", other === button);
  }
  renderView(button.dataset.view);
});

function renderView(name) {
  if (name === "console") {
    consoleView.mount(panel);
    return;
  }
  panel.innerHTML = "";
  const wrap = document.createElement("div");
  wrap.className = "placeholder";
  const title = document.createElement("div");
  title.className = "label-lg";
  title.textContent = name;
  const note = document.createElement("div");
  note.className = "label";
  note.textContent = `[ ${placeholders[name] ?? "unknown view"} ]`;
  wrap.append(title, note);
  panel.append(wrap);
}

renderView("console");
socket.subscribe(["agents", "runtime"]);
