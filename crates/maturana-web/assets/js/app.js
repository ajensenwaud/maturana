// Cockpit shell: nav switching, link status, view mounting.
// Console (phase 2) is live; dashboards land in phase 3.

import { CockpitSocket } from "/assets/js/ws.js";
import { Console } from "/assets/js/console.js";
import { Chat } from "/assets/js/chat.js";
import * as dashboard from "/assets/js/dashboard.js";
import { mountThemeSwitcher } from "/assets/js/themes.js";

mountThemeSwitcher(document.getElementById("theme-switch"));

const socket = new CockpitSocket();
const linkStatus = document.getElementById("link-status");

const sbLink = document.getElementById("sb-link");
function setSbDot(itemId, kind, label) {
  const node = document.getElementById(itemId);
  if (!node) return;
  node.innerHTML = "";
  const dot = document.createElement("span");
  dot.className = `sb-dot ${kind}`;
  node.append(dot, document.createTextNode(label));
}

socket.onStatus((status) => {
  if (status === "open") {
    linkStatus.textContent = "[link ok]";
    linkStatus.className = "status-ok";
    setSbDot("sb-link", "ok", "link ok");
  } else if (status === "connecting") {
    linkStatus.textContent = "[link ..]";
    linkStatus.className = "status-dim";
    setSbDot("sb-link", "dim", "link…");
  } else if (status === "version-mismatch") {
    linkStatus.textContent = "[link v!]";
    linkStatus.className = "status-bad";
    setSbDot("sb-link", "bad", "version!");
  } else {
    linkStatus.textContent = "[link --]";
    linkStatus.className = "status-bad";
    setSbDot("sb-link", "bad", "link down");
  }
});

// ---- collapsible sidebar (persisted, pre-paint friendly) ----
const navToggle = document.getElementById("nav-toggle");
const NAV_KEY = "maturana.nav.collapsed";
function setNavCollapsed(collapsed) {
  document.documentElement.dataset.navCollapsed = collapsed ? "1" : "";
  navToggle.setAttribute("aria-expanded", String(!collapsed));
  navToggle.setAttribute("aria-label", collapsed ? "Expand sidebar" : "Collapse sidebar");
  try { localStorage.setItem(NAV_KEY, collapsed ? "1" : "0"); } catch {}
}
let navCollapsed = false;
try { navCollapsed = localStorage.getItem(NAV_KEY) === "1"; } catch {}
setNavCollapsed(navCollapsed);
navToggle?.addEventListener("click", () => { navCollapsed = !navCollapsed; setNavCollapsed(navCollapsed); });

// ---- bottom status bar (plane / agents / host), polled lightly ----
async function refreshStatusBar() {
  try {
    const res = await fetch("/api/overview", { headers: {} });
    const payload = await res.json().catch(() => null);
    const o = payload && payload.ok ? payload.data : null;
    if (!o) return;
    const c = o.counts || {};
    const host = o.host || {};
    setSbDot("sb-plane", o.plane?.up ? "ok" : "bad", o.plane?.up ? "plane up" : "plane down");
    const up = c.up ?? 0, total = c.agents ?? 0;
    setSbDot("sb-agents", up > 0 ? "ok" : (total ? "warn" : "dim"), `${up}/${total} agents`);
    const hostEl = document.getElementById("sb-host");
    if (hostEl) hostEl.textContent = host.hostname ? `${host.hostname} · ${host.cores ?? "?"} cores` : "";
  } catch {}
}
refreshStatusBar();
setInterval(refreshStatusBar, 5000);

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
