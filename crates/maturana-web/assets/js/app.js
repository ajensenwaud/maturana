// Cockpit shell: nav switching + link status. Views fill in across phases
// (console: phase 2; dashboards: phase 3).

import { CockpitSocket } from "/assets/js/ws.js";

const socket = new CockpitSocket();
const linkStatus = document.getElementById("link-status");

socket.onStatus((status) => {
  if (status === "open") {
    linkStatus.textContent = "[link ok]";
    linkStatus.className = "status-ok";
    bootProgress(100);
  } else if (status === "connecting") {
    linkStatus.textContent = "[link ..]";
    linkStatus.className = "status-dim";
    bootProgress(60);
  } else if (status === "version-mismatch") {
    linkStatus.textContent = "[link v!]";
    linkStatus.className = "status-bad";
  } else {
    linkStatus.textContent = "[link --]";
    linkStatus.className = "status-bad";
  }
});

function bootProgress(pct) {
  const fill = document.getElementById("boot-fill");
  const counter = document.getElementById("boot-pct");
  if (fill) fill.style.width = `${pct}%`;
  if (counter) counter.textContent = String(pct).padStart(3, "0");
}

// ---- nav ----

const views = {
  console: "Prompt console — phase 2",
  agents: "Agent fleet — phase 3",
  runtime: "Runtime plane — phase 3",
  sessions: "Sessions — phase 3",
  graph: "Knowledge graph — phase 3",
  pipelock: "Pipelock — phase 3",
  tools: "Tools — phase 3",
  skills: "Skills — phase 3",
};

const panel = document.getElementById("panel");
const nav = document.getElementById("nav");

nav.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-view]");
  if (!button) return;
  for (const other of nav.querySelectorAll("button")) {
    other.classList.toggle("active", other === button);
  }
  renderView(button.dataset.view);
});

function renderView(name) {
  panel.innerHTML = "";
  const wrap = document.createElement("div");
  wrap.className = "placeholder";
  const title = document.createElement("div");
  title.className = "label-lg";
  title.textContent = name;
  const note = document.createElement("div");
  note.className = "label";
  note.textContent = `[ ${views[name] ?? "unknown view"} ]`;
  wrap.append(title, note);
  panel.append(wrap);
}

socket.subscribe(["agents", "runtime"]);
