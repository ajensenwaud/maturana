// Cockpit shell: nav switching, link status, view mounting.
// Console (phase 2) is live; dashboards land in phase 3.

import { CockpitSocket } from "/assets/js/ws.js";
import { Console } from "/assets/js/console.js";
import { Chat } from "/assets/js/chat.js";
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
const chatView = new Chat(socket);

const views = {
  overview: dashboard.renderOverview,
  agents: dashboard.renderAgents,
  system: dashboard.renderSystem,
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

// Let dashboard views jump into the chat for a specific agent (e.g. the
// Sessions "message" action) instead of the old per-panel composer.
window.cockpitOpenChat = (agentId) => {
  for (const other of nav.querySelectorAll("button")) {
    other.classList.toggle("active", other.dataset.view === "chat");
  }
  chatView.mount(panel, agentId);
};

async function renderView(name) {
  if (name === "chat") {
    chatView.mount(panel);
    return;
  }
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

renderView("overview");
socket.subscribe(["agents", "runtime"]);
