// Dashboard views: agents, runtime, sessions, graph, pipelock, tools, skills.
// All REST mutations carry the x-maturana-web CSRF header; live updates ride
// the shared WebSocket (agents/runtime topics + session_outbound).

import { marked } from "/assets/vendor/marked/marked.esm.js";
import { formDialog, confirmDialog, toast } from "/assets/js/ui.js";

async function api(path, options = {}) {
  const headers = { ...(options.headers ?? {}) };
  if (options.method && options.method !== "GET") {
    headers["x-maturana-web"] = "1";
    if (options.body && typeof options.body === "string") {
      headers["content-type"] = "application/json";
    }
  }
  const response = await fetch(path, { ...options, headers });
  const payload = await response.json().catch(() => ({ ok: false, error: "bad json" }));
  if (!payload.ok) throw new Error(payload.error ?? `http ${response.status}`);
  return payload.data;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function section(title) {
  const wrap = el("div", "dash-section");
  wrap.append(el("div", "dash-title", title));
  return wrap;
}

function jsonBlock(value) {
  const pre = el("pre", "dash-json");
  pre.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return pre;
}

function table(headers, rows) {
  const t = el("table", "dash-table");
  const head = el("tr");
  for (const h of headers) head.append(el("th", "label", h));
  t.append(head);
  for (const cells of rows) {
    const tr = el("tr");
    for (const cell of cells) {
      const td = el("td");
      if (cell instanceof Node) td.append(cell);
      else td.textContent = cell ?? "—";
      tr.append(td);
    }
    t.append(tr);
  }
  return t;
}

function button(label, onClick, danger = false) {
  const b = el("button", danger ? "primary danger" : "primary", label);
  b.addEventListener("click", onClick);
  return b;
}

// One-line description under a panel title, so every view says what it is for.
function desc(text) {
  return el("div", "panel-desc", text);
}

function badge(text, kind) {
  return el("span", `pill ${kind || ""}`, text);
}

// A row of pill badges from a string array (or "none").
function chipsOf(arr) {
  const box = el("div", "chips");
  if (!arr || !arr.length) { box.append(el("span", "panel-desc", "none")); return box; }
  for (const x of arr) box.append(badge(typeof x === "string" ? x : JSON.stringify(x)));
  return box;
}

// Markdown → sanitized HTML string (strips script/handlers/javascript: hrefs).
function renderMd(src) {
  const html = marked.parse(src || "", { breaks: true });
  const tpl = document.createElement("template");
  tpl.innerHTML = html;
  tpl.content.querySelectorAll("script, iframe, object, embed, link, style").forEach((n) => n.remove());
  tpl.content.querySelectorAll("*").forEach((n) => {
    for (const attr of [...n.attributes]) {
      if (/^on/i.test(attr.name) || (attr.name === "href" && /^javascript:/i.test(attr.value.trim()))) {
        n.removeAttribute(attr.name);
      }
    }
  });
  return tpl.innerHTML;
}

// ---- overview (cockpit landing) ----

const WORKER_TEXT = {
  idle: "waiting for a turn",
  running: "answering a turn",
  starting: "booting",
  error: "needs attention",
};

export async function renderOverview(panel) {
  const wrap = section("Overview");
  wrap.append(desc("Your fleet at a glance — who's deployed, what they're doing, and whether the host plane is healthy."));
  const cards = el("div", "ov-cards");
  const body = el("div");
  wrap.append(cards, body);
  panel.replaceChildren(wrap);

  const card = (label, value, kind) => {
    const c = el("div", "ov-card");
    c.append(el("div", `ov-card-val ${kind || ""}`, String(value)), el("div", "ov-card-label", label));
    return c;
  };

  const draw = async () => {
    let o;
    try {
      o = await api("/api/overview");
    } catch (error) {
      body.replaceChildren(el("div", "status-bad", String(error)));
      return;
    }
    const c = o.counts || {};
    const host = o.host || {};
    const memPct = host.mem_total_bytes && host.mem_available_bytes != null
      ? Math.round(100 * (1 - host.mem_available_bytes / host.mem_total_bytes)) : null;
    cards.replaceChildren(
      card("agents", c.agents ?? 0),
      card("up", c.up ?? 0, ((c.up ?? 0) > 0 ? "good" : "bad")),
      card("busy", c.busy ?? 0),
      card("with graph", c.graphs ?? 0),
      card("plane", o.plane?.up ? "up" : "down", o.plane?.up ? "good" : "bad"),
      card("load", (host.loadavg?.[0] ?? 0).toFixed(2)),
      card("memory", memPct != null ? `${memPct}%` : "—", memPct > 90 ? "bad" : ""),
    );

    const agents = (o.agents || []);
    const rows = agents.map((a) => {
      const st = a.status || a.worker_status?.status || "unknown";
      const doing = a.worker_status?.message || WORKER_TEXT[st] || "—";
      return [
        el("strong", null, a.agent_id),
        a.harness || "—",
        statusPill(st, a.live),
        doing,
        a.knowledge_graph ? `graph:${a.graph_name}` : "—",
      ];
    });
    body.replaceChildren(
      el("div", "label dash-title", "Agents"),
      agents.length
        ? table(["agent", "harness", "worker", "doing", "graph"], rows)
        : el("div", "panel-desc", "No agents deployed yet — add one from the Agents panel."),
      el("div", "panel-desc",
        `Host: ${host.hostname || "?"} (${host.os || "?"}/${host.arch || "?"}) · ${host.cores ?? "?"} cores · up ${fmtUptime(host.uptime_seconds)}`),
    );
  };
  await draw();
  const timer = setInterval(() => {
    if (panel.contains(wrap)) draw();
    else clearInterval(timer);
  }, 5000);
}

// Liveness pill. `live` (fresh heartbeat) is what decides up vs offline; the
// raw status only distinguishes busy (a turn in flight) from idle. The guest
// worker never writes "running", so we synthesize the human labels here:
//   error → "error" (red) · stale/no heartbeat → "offline" (dim)
//   claimed → "busy" (green) · idle+fresh → "up" (green).
// `live` is optional so legacy callers (Agents view) that pass only a status
// still get a sensible pill.
function statusPill(status, live) {
  if (status === "error") return badge("error", "bad");
  if (live === false) return badge(status ? `offline (${status})` : "offline", "dim");
  if (status === "claimed") return badge("busy", "good");
  if (status === "idle" || live === true) return badge("up", "good");
  return badge(status || "—", "dim");
}

// ---- system (observability) ----

function fmtBytes(n) {
  if (n == null) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function fmtUptime(s) {
  if (s == null) return "—";
  s = Math.floor(s);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  return d > 0 ? `${d}d ${h}h` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}

export async function renderSystem(panel, socket) {
  // Runtime plane: the supervised host processes that run the whole fleet.
  const planeWrap = section("Plane");
  planeWrap.append(desc("The host control plane — supervised processes that run the fleet (sessiond, graph, channels, schedules, claude-refresh). Live."));
  const planeBody = el("div");
  planeWrap.append(planeBody);
  const drawPlane = (up) => {
    const processes = (up.processes ?? []).map((p) => [
      p.name, String(p.pid), p.critical ? "critical" : "—", String(p.restarts), `${p.uptime_seconds}s`,
    ]);
    planeBody.replaceChildren(
      el("div", up.running !== false ? "status-ok" : "status-bad",
        up.running !== false ? `[ up · supervisor pid ${up.pid ?? "?"} ]` : "[ maturana up is not running ]"),
      table(["process", "pid", "critical", "restarts", "uptime"], processes),
    );
  };
  if (socket) socket.on("dash_update", (msg) => { if (msg.topic === "runtime" && panel.contains(planeWrap)) drawPlane(msg.data); });
  try { drawPlane(await api("/api/runtime/up")); } catch (e) { planeBody.replaceChildren(el("div", "status-bad", String(e))); }

  // Ops: plane lifecycle + config backup.
  const opsWrap = section("Ops");
  opsWrap.append(desc("Operate the plane: restart/stop/start the supervisor, or snapshot the config to a timestamped backup."));
  const opsOut = el("div");
  const gw = (action, danger) => button(`${action} plane`, async () => {
    if (action !== "restart" && !(await confirmDialog({ title: "Gateway", message: `${action} the supervised plane?`, danger: true, confirmLabel: action }))) return;
    opsOut.replaceChildren(el("div", "label", `[ ${action}… ]`));
    try {
      await api(`/api/ops/gateway/${action}`, { method: "POST" });
      opsOut.replaceChildren(el("div", "status-ok", `[ ${action} ok ]`));
    } catch (error) {
      opsOut.replaceChildren(el("div", "status-bad", String(error)));
    }
  }, danger);
  const opsRow = el("div", "dash-actions");
  opsRow.append(
    gw("restart", false), gw("stop", true), gw("start", false),
    button("backup config", async () => {
      opsOut.replaceChildren(el("div", "label", "[ backing up… ]"));
      try {
        const r = await api("/api/ops/backup", { method: "POST" });
        opsOut.replaceChildren(el("div", "status-ok", `[ backup → ${r.path} (${r.bytes} bytes) ]`));
      } catch (error) {
        opsOut.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    button("run doctor", async () => {
      opsOut.replaceChildren(el("div", "label", "[ running diagnostics… ]"));
      try {
        opsOut.replaceChildren(el("div", "status-ok", "[ doctor: health checks across the install ]"), jsonBlock(await api("/api/doctor")));
      } catch (error) {
        opsOut.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
  );
  opsWrap.append(opsRow, opsOut);

  // Host stats (auto-refresh every 5s while visible).
  const statsWrap = section("Host");
  statsWrap.append(desc("The physical/VM host this control plane runs on."));
  const statsBody = el("div");
  statsWrap.append(statsBody);
  const drawStats = async () => {
    try {
      const s = await api("/api/system/stats");
      const memUsed = s.mem_total_bytes != null && s.mem_available_bytes != null
        ? s.mem_total_bytes - s.mem_available_bytes : null;
      const diskUsed = s.disk_total_bytes != null && s.disk_available_bytes != null
        ? s.disk_total_bytes - s.disk_available_bytes : null;
      const pairs = [
        ["host", `${s.hostname} (${s.os}/${s.arch})`],
        ["cpu cores", String(s.cores)],
        ["uptime", fmtUptime(s.uptime_seconds)],
        ["load avg", (s.loadavg || []).map((x) => x.toFixed(2)).join("  ") || "—"],
        ["memory", memUsed != null ? `${fmtBytes(memUsed)} / ${fmtBytes(s.mem_total_bytes)}` : "—"],
        ["disk", diskUsed != null ? `${fmtBytes(diskUsed)} / ${fmtBytes(s.disk_total_bytes)}` : "—"],
      ];
      statsBody.replaceChildren(table(["metric", "value"], pairs));
    } catch (error) {
      statsBody.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  await drawStats();
  const statsTimer = setInterval(() => {
    if (panel.contains(statsWrap)) drawStats();
    else clearInterval(statsTimer);
  }, 5000);

  // Logs (source dropdown + optional auto-tail).
  const logWrap = section("Logs");
  logWrap.append(desc("Tail the plane/web/fleet units or any agent's egress audit log. JSON lines render as readable rows — time, action, message; toggle Raw for the verbatim text, auto-tail to follow live."));
  const controls = el("div", "dash-actions");
  const sourceSel = el("select", "model-input");
  const linesInput = el("input", "model-input");
  linesInput.type = "number";
  linesInput.value = "200";
  linesInput.style.width = "90px";
  const logView = el("div", "log-view");
  const logPre = el("pre", "dash-json");
  logPre.style.display = "none";
  // Parse each line as JSON → a readable row; non-JSON (journalctl) passes through.
  const renderLogLines = (text) => {
    logView.replaceChildren();
    const lines = (text || "").split("\n").filter((l) => l.trim());
    if (!lines.length) { logView.append(el("div", "panel-desc", "(empty)")); return; }
    for (const line of lines) {
      let obj = null;
      try { obj = JSON.parse(line); } catch {}
      if (obj && typeof obj === "object" && !Array.isArray(obj)) {
        const row = el("div", "log-row");
        const ts = obj.at || obj.timestamp || obj.time || "";
        const tstr = typeof ts === "string" && ts.length >= 19 ? ts.slice(11, 19) : String(ts).slice(0, 8);
        row.append(el("span", "log-time", tstr));
        const action = obj.action || obj.level || obj.kind || "";
        const bad = /deny|denied|error|fail|block|warn/i.test(`${action} ${obj.reason || ""}`);
        if (action) row.append(badge(String(action), bad ? "bad" : "good"));
        const msg = obj.message || obj.msg || [obj.method, obj.host, obj.target].filter(Boolean).join(" ") || "";
        row.append(el("span", "log-msg", msg));
        const skip = new Set(["at", "timestamp", "time", "action", "level", "kind", "message", "msg", "method", "host", "target"]);
        const extra = Object.entries(obj)
          .filter(([k, v]) => !skip.has(k) && v != null && v !== "" && typeof v !== "object")
          .map(([k, v]) => `${k}=${v}`)
          .join("  ");
        if (extra) row.append(el("span", "log-extra", extra));
        logView.append(row);
      } else {
        logView.append(el("div", "log-row log-plain", line));
      }
    }
    logView.scrollTop = logView.scrollHeight;
  };
  let rawOn = false;
  let lastText = "";
  const loadLog = async () => {
    try {
      const d = await api(`/api/system/logs?source=${encodeURIComponent(sourceSel.value)}&lines=${encodeURIComponent(linesInput.value || 200)}`);
      lastText = d.text || "";
      renderLogLines(lastText);
      logPre.textContent = lastText || "(empty)";
      logPre.scrollTop = logPre.scrollHeight;
    } catch (error) {
      logView.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  try {
    const srcs = (await api("/api/system/logs/sources")).sources || ["plane"];
    for (const s of srcs) {
      const o = el("option", null, s);
      o.value = s;
      sourceSel.append(o);
    }
  } catch {
    const o = el("option", null, "plane");
    o.value = "plane";
    sourceSel.append(o);
  }
  sourceSel.addEventListener("change", loadLog);
  const rawChk = el("input");
  rawChk.type = "checkbox";
  const rawLabel = el("label", "label");
  rawLabel.append(rawChk, document.createTextNode(" raw"));
  rawChk.addEventListener("change", () => {
    rawOn = rawChk.checked;
    logPre.style.display = rawOn ? "" : "none";
    logView.style.display = rawOn ? "none" : "";
  });
  const tailChk = el("input");
  tailChk.type = "checkbox";
  const tailLabel = el("label", "label");
  tailLabel.append(tailChk, document.createTextNode(" auto-tail"));
  controls.append(sourceSel, linesInput, button("refresh", loadLog), rawLabel, tailLabel);
  logWrap.append(controls, logView, logPre);
  await loadLog();
  const logTimer = setInterval(() => {
    if (!panel.contains(logWrap)) { clearInterval(logTimer); return; }
    if (tailChk.checked) loadLog();
  }, 5000);

  // Activity analytics (we don't meter token cost — that lives in the guest VMs).
  const aWrap = section("Activity");
  aWrap.append(desc("Per-agent activity across all sessions — turns and message volume. Token/cost metering happens inside each agent's VM and isn't visible to the host."));
  const daysSel = el("select", "model-input");
  for (const d of [7, 30, 90]) {
    const o = el("option", null, `${d} days`);
    o.value = String(d);
    daysSel.append(o);
  }
  daysSel.value = "30";
  const aBody = el("div");
  const loadA = async () => {
    aBody.replaceChildren(el("div", "label", "[ loading… ]"));
    try {
      const a = await api(`/api/system/analytics?days=${daysSel.value}`);
      const rows = a.per_agent.map((p) => [
        p.agent_id,
        String(p.sessions),
        String(p.inbound),
        String(p.completed_turns),
        p.last_active ? new Date(p.last_active).toLocaleString() : "—",
      ]);
      aBody.replaceChildren(
        el("div", "label", `totals: ${a.totals.sessions} sessions · ${a.totals.inbound} inbound · ${a.totals.completed_turns} completed`),
        table(["agent", "sessions", "inbound", "completed", "last active"], rows),
        el("div", "status-dim", a.note),
      );
    } catch (error) {
      aBody.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  daysSel.addEventListener("change", loadA);
  aWrap.append(daysSel, aBody);
  await loadA();

  panel.replaceChildren(planeWrap, opsWrap, statsWrap, logWrap, aWrap);
}

// ---- config (browser-managed spec sections) ----

const CONFIG_SECTIONS = ["schedules", "mcp_servers", "channels", "skills", "tools", "capabilities"];

const CONFIG_HINTS = {
  schedules: 'Scheduled jobs (cron). e.g. [{"cron":"0 9 * * *","prompt":"daily summary","channel":"telegram"}]',
  mcp_servers: "Model-Context-Protocol servers this agent connects to.",
  channels: 'Messaging surfaces. e.g. {"telegram":{"token_source":"pipelock:telegram/bot-token"}}',
  skills: 'Skills enabled for this agent. e.g. ["maturana-web-search","maturana-browse"]',
  tools: "WASM tool names enabled for this agent.",
  capabilities: 'Opt-in capabilities, e.g. {"image_gen":true,"self_forge":false}',
};

// ---- agents ----

export async function renderAgents(panel, socket) {
  const wrap = section("Agents");
  wrap.append(desc("Every deployed agent, what it can do, and its single source of truth — its spec. Select one to see details, edit its config, restart, or message it."));
  const listBox = el("div");
  const detail = el("div", "agent-detail");
  let selected = null;

  const draw = (agents) => {
    const rows = agents.map((agent) => {
      const open = () => { selected = agent.agent_id; showAgent(detail, socket, agent.agent_id); };
      const name = el("a", "row-link", agent.agent_id);
      name.addEventListener("click", open);
      const st = agent.worker_status?.status || "unknown";
      return [
        name,
        agent.harness || "—",
        agent.name || "—",
        statusPill(st),
        agent.knowledge_graph ? `graph:${agent.graph_name}` : "—",
        button("open", open),
      ];
    });
    listBox.replaceChildren(
      table(["agent", "harness", "display name", "worker", "graph", ""], rows),
    );
  };

  const addRow = el("div", "dash-actions");
  addRow.append(button("+ add agent", () => addAgent(detail, () => refresh())));
  const refresh = async () => draw(await api("/api/agents"));

  socket.on("dash_update", (msg) => {
    if (msg.topic === "agents" && panel.contains(wrap)) {
      draw(msg.data);
      // Keep an open detail in sync with the worker status.
      if (selected) {
        const a = msg.data.find((x) => x.agent_id === selected);
        const pill = detail.querySelector(".agent-worker-pill");
        if (a && pill) pill.replaceWith(workerPill(a.worker_status));
      }
    }
  });
  await refresh();
  wrap.append(addRow, listBox);
  panel.replaceChildren(wrap, detail);
}

function workerPill(ws) {
  const st = ws?.status || "unknown";
  const p = statusPill(st);
  p.classList.add("agent-worker-pill");
  if (ws?.message) p.append(document.createTextNode(` · ${ws.message}`));
  return p;
}

// Readable agent detail: identity + capabilities + spec actions, no raw dumps.
async function showAgent(detail, socket, agentId) {
  detail.replaceChildren(el("div", "label", `[ loading ${agentId}… ]`));
  let d;
  try {
    d = await api(`/api/agents/${agentId}/detail`);
  } catch (error) {
    detail.replaceChildren(el("div", "status-bad", String(error)));
    return;
  }

  const head = el("div", "agent-head");
  head.append(
    el("div", "agent-title", d.name || d.agent_id),
    workerPill(d.worker_status),
  );
  const sub = el("div", "panel-desc", d.purpose || "");

  // Readable summary grid.
  const chips = (arr) => {
    const box = el("div", "chips");
    if (!arr || !arr.length) { box.append(el("span", "panel-desc", "none")); return box; }
    for (const x of arr) box.append(badge(typeof x === "string" ? x : JSON.stringify(x)));
    return box;
  };
  const capList = Object.entries(d.capabilities || {})
    .filter(([, v]) => v === true).map(([k]) => k);
  const grid = el("div", "kv-grid");
  const kv = (k, v) => { grid.append(el("div", "kv-k", k)); grid.append(v instanceof Node ? v : el("div", "kv-v", v ?? "—")); };
  kv("harness", `${d.harness} · ${d.provider}`);
  kv("resources", `${d.vcpu} vCPU · ${d.memory_mib} MiB`);
  kv("knowledge graph", d.knowledge_graph ? `on · ${d.graph_name}` : "off");
  kv("skills", chips(d.skills));
  kv("tools", chips(d.tools));
  kv("MCP servers", chips(d.mcp_servers));
  kv("channels", chips(d.channels));
  kv("capabilities", chips(capList));
  kv("egress hosts", `${(d.egress_allowlist || []).length} allowed (see Egress panel)`);
  kv("schedules", String(d.schedules ?? 0));

  // Action bar.
  const actions = el("div", "dash-actions");
  const out = el("div");
  actions.append(
    button("message", () => window.cockpitOpenChat(agentId)),
    button("config", () => agentConfig(out, agentId)),
    button("edit spec", () => editSpec(out, agentId)),
    button("restart", async () => {
      if (!(await confirmDialog({ title: "Restart agent", message: `Restart ${agentId}? Relaunches its microVM from the baked image.`, confirmLabel: "Restart" }))) return;
      out.replaceChildren(el("div", "label", "[ restarting… this can take a moment ]"));
      try {
        const r = await api(`/api/agents/${agentId}/restart`, { method: "POST" });
        out.replaceChildren(el("div", "status-ok", `[ restarted ${agentId} ]`), jsonBlock((r.output || []).join("\n")));
      } catch (error) {
        out.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    button("stop", async () => {
      if (!(await confirmDialog({ title: "Stop agent", message: `Stop ${agentId}?`, danger: true, confirmLabel: "Stop" }))) return;
      try {
        await api(`/api/agents/${agentId}/stop`, { method: "POST" });
        out.replaceChildren(el("div", "status-ok", `[ stopped ${agentId} ]`));
      } catch (error) {
        out.replaceChildren(el("div", "status-bad", String(error)));
      }
    }, true),
  );

  detail.replaceChildren(head, sub, grid, actions, out);
}

// Add-agent scaffold: creates a validated starter spec, then opens it for
// refinement. No VM is provisioned until the operator runs dry-run → apply.
function addAgent(detail, onCreated) {
  const wrap = section("Add agent");
  wrap.append(desc("Scaffold a new agent's spec. This only writes the declarative spec — provision the VM afterwards with dry-run → apply (slow: copies the rootfs)."));
  const id = el("input", "model-input"); id.placeholder = "id (a-z 0-9 - _)";
  const name = el("input", "model-input"); name.placeholder = "display name";
  const purpose = el("input", "model-input"); purpose.placeholder = "one-line purpose"; purpose.style.flex = "1";
  const harness = el("select", "model-input");
  for (const h of ["codex", "claude", "opencode"]) { const o = el("option", null, h); o.value = h; harness.append(o); }
  const out = el("div");
  const row1 = el("div", "dash-actions"); row1.append(id, name, harness);
  const row2 = el("div", "dash-actions"); row2.append(purpose, button("create", async () => {
    if (!id.value.trim()) { out.replaceChildren(el("div", "status-bad", "id is required")); return; }
    out.replaceChildren(el("div", "label", "[ creating… ]"));
    try {
      await api("/api/agents", { method: "POST", body: JSON.stringify({
        id: id.value.trim(), name: name.value.trim(), purpose: purpose.value.trim(), harness: harness.value,
      }) });
      out.replaceChildren(el("div", "status-ok", `[ created ${id.value.trim()} — edit its spec, then dry-run → apply to provision ]`));
      onCreated?.();
      editSpec(out, id.value.trim());
    } catch (error) {
      out.replaceChildren(el("div", "status-bad", String(error)));
    }
  }));
  wrap.append(row1, row2, out);
  detail.replaceChildren(wrap);
}

// Per-agent config: form over the declarative spec sections (the old standalone
// "Config" panel, now living where it belongs — on the agent).
async function agentConfig(out, agentId) {
  const wrap = section(`Config — ${agentId}`);
  wrap.append(desc("Edit the agent's declarative spec sections. Validated before write; a running agent applies channel/MCP/schedule changes on its next restart. Identity / VM / runtime (the isolation boundary) are edited via 'edit spec'."));
  const sectionSel = el("select", "model-input");
  for (const s of CONFIG_SECTIONS) { const o = el("option", null, s); o.value = s; sectionSel.append(o); }
  const hint = el("div", "panel-desc");
  const editor = el("textarea", "spec-editor"); editor.style.minHeight = "32vh";
  const report = el("div");
  const load = async () => {
    report.replaceChildren();
    hint.textContent = CONFIG_HINTS[sectionSel.value] || "";
    try {
      const dd = await api(`/api/agents/${agentId}/config?section=${sectionSel.value}`);
      editor.value = JSON.stringify(dd.value ?? null, null, 2);
    } catch (error) {
      report.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  const save = async () => {
    let value;
    try { value = JSON.parse(editor.value); }
    catch { report.replaceChildren(el("div", "status-bad", "invalid JSON")); return; }
    try {
      const dd = await api(`/api/agents/${agentId}/config`, {
        method: "PUT", body: JSON.stringify({ section: sectionSel.value, value }),
      });
      report.replaceChildren(el("div", "status-ok", `[ saved ${dd.section} — spec valid; applies on next restart ]`));
    } catch (error) {
      report.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  sectionSel.addEventListener("change", load);
  const controls = el("div", "dash-actions");
  controls.append(sectionSel, button("load", load), button("save", save));
  wrap.append(controls, hint, editor, report);
  await load();
  out.replaceChildren(wrap);
}

async function editSpec(detail, agentId) {
  detail.replaceChildren(el("div", "label", `[ ${agentId} spec — validate, dry-run, then apply ]`));
  const editor = el("textarea", "spec-editor");
  editor.value = (await api(`/api/agents/${agentId}/spec`)).markdown;
  const report = el("div");
  let dryRunDone = false;
  const applyBtn = button("apply", async () => {
    if (!dryRunDone) {
      report.replaceChildren(el("div", "status-bad", "[ run dry-run first ]"));
      return;
    }
    try {
      const result = await api(`/api/agents/${agentId}/apply`, {
        method: "POST",
        body: JSON.stringify({ dry_run: false }),
      });
      report.replaceChildren(el("div", "status-ok", "[ applied ]"), jsonBlock(result));
    } catch (error) {
      report.replaceChildren(el("div", "status-bad", String(error)));
    }
  }, true);
  detail.append(
    editor,
    el("div", "dash-actions"),
  );
  detail.lastChild.append(
    button("validate", async () => {
      try {
        const data = await api(`/api/agents/${agentId}/spec/validate`, {
          method: "POST",
          body: JSON.stringify({ markdown: editor.value }),
        });
        report.replaceChildren(jsonBlock(data.report));
      } catch (error) {
        report.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    button("save", async () => {
      try {
        const data = await api(`/api/agents/${agentId}/spec`, {
          method: "PUT",
          body: JSON.stringify({ markdown: editor.value }),
        });
        report.replaceChildren(
          el("div", data.written ? "status-ok" : "status-bad",
            data.written ? "[ saved ]" : "[ rejected by validation — not saved ]"),
          jsonBlock(data.report),
        );
      } catch (error) {
        report.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    button("dry-run", async () => {
      try {
        const result = await api(`/api/agents/${agentId}/apply`, {
          method: "POST",
          body: JSON.stringify({ dry_run: true }),
        });
        dryRunDone = true;
        report.replaceChildren(el("div", "status-ok", "[ dry-run ok — apply unlocked ]"), jsonBlock(result));
      } catch (error) {
        report.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    applyBtn,
  );
  detail.append(report);
}

// ---- sessions ----

export async function renderSessions(panel, socket) {
  const wrap = section("Sessions");
  wrap.append(desc("Conversation history per agent. Open one to read it, export it, or jump into the chat to continue it. Search runs across every message; prune clears out idle sessions."));
  const detail = el("div");
  const listBox = el("div");

  const draw = (sessions) => {
    const rows = sessions.map((s) => [
      s.agent_id,
      (s.label ? `${s.label} · ` : "") + s.session_id,
      s.last_active ? new Date(s.last_active).toLocaleString() : "—",
      `${s.stats?.completed ?? 0}✓ / ${s.stats?.pending ?? 0}⏳`,
      (() => {
        const cell = el("div", "dash-actions");
        cell.append(
          button("open", () => openSession(detail, socket, s)),
          button("message", () => window.cockpitOpenChat(s.agent_id)),
          button("export", () => exportSession(s.agent_id, s.session_id)),
        );
        return cell;
      })(),
    ]);
    listBox.replaceChildren(table(["agent", "session", "last active", "queue", ""], rows));
  };

  // Search across every session's messages.
  const searchRow = el("div", "dash-actions");
  const searchInput = el("input", "model-input");
  searchInput.placeholder = "search all messages…";
  searchInput.style.flex = "1";
  const searchOut = el("div");
  const doSearch = async () => {
    const q = searchInput.value.trim();
    if (!q) { searchOut.replaceChildren(); return; }
    searchOut.replaceChildren(el("div", "label", "[ searching… ]"));
    try {
      const { hits } = await api(`/api/sessions/search?q=${encodeURIComponent(q)}`);
      if (!hits.length) { searchOut.replaceChildren(el("div", "status-dim", "no matches")); return; }
      searchOut.replaceChildren(table(["agent", "session", "dir", "snippet", ""], hits.map((h) => [
        h.agent_id, h.session_id, h.direction, h.snippet,
        button("open", () => openSession(detail, socket, { agent_id: h.agent_id, session_id: h.session_id })),
      ])));
    } catch (error) {
      searchOut.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  searchInput.addEventListener("keydown", (e) => { if (e.key === "Enter") doSearch(); });
  searchRow.append(searchInput, button("search", doSearch));

  // Prune idle sessions.
  const pruneRow = el("div", "dash-actions");
  const pruneDays = el("input", "model-input");
  pruneDays.type = "number";
  pruneDays.value = "30";
  pruneDays.style.width = "90px";
  pruneRow.append(
    el("span", "label", "prune sessions idle >"),
    pruneDays,
    el("span", "label", "days"),
    button("prune", async () => {
      const days = parseInt(pruneDays.value || "30", 10);
      if (!(await confirmDialog({ title: "Prune sessions", message: `Delete sessions with no activity in ${days} days? This cannot be undone.`, danger: true, confirmLabel: "Prune" }))) return;
      try {
        const r = await api("/api/sessions/prune", { method: "POST", body: JSON.stringify({ days }) });
        detail.replaceChildren(el("div", "status-ok", `[ pruned ${r.count} session(s) ]`));
        draw(await api("/api/sessions"));
      } catch (error) {
        detail.replaceChildren(el("div", "status-bad", String(error)));
      }
    }, true),
  );

  draw(await api("/api/sessions"));
  wrap.append(searchRow, searchOut, pruneRow, listBox);
  panel.replaceChildren(wrap, detail);
}

async function exportSession(agentId, sessionId) {
  try {
    const data = await api(`/api/sessions/${agentId}/${sessionId}/export`);
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${agentId}-${sessionId}.json`;
    a.click();
    URL.revokeObjectURL(url);
  } catch (error) {
    toast(String(error), "bad");
  }
}

async function openSession(detail, socket, s) {
  const agentId = s.agent_id;
  const sessionId = s.session_id;
  const title = el("div", "label dash-title", `${agentId} / ${s.label ? s.label + " · " : ""}${sessionId}`);
  const head = el("div", "dash-actions");
  head.append(
    title,
    button("continue in chat", () => window.cockpitOpenChat(agentId)),
    button("rename", () => {
      formDialog({
        title: "Rename conversation",
        sub: `${agentId} / ${sessionId}`,
        fields: [{ name: "label", label: "Label (empty to clear)", type: "text", value: s.label || "" }],
        submitLabel: "Save",
        onSubmit: async (v) => {
          await api(`/api/sessions/${agentId}/${sessionId}/label`, { method: "PUT", body: JSON.stringify({ label: v.label }) });
          s.label = v.label;
          title.textContent = `${agentId} / ${v.label ? v.label + " · " : ""}${sessionId}`;
        },
      });
    }),
  );
  const log = el("div", "session-log");
  const refresh = async () => {
    const data = await api(`/api/sessions/${agentId}/${sessionId}/messages`);
    const merged = [
      ...data.inbound.map((m) => ({ ...m, dir: "in" })),
      ...data.outbound.map((m) => ({ ...m, dir: "out" })),
    ].sort((a, b) => a.created_at.localeCompare(b.created_at));
    log.replaceChildren(
      ...merged.map((m) => {
        const row = el("div", `session-msg ${m.dir}`);
        let text = m.content;
        try { text = JSON.parse(m.content).text ?? m.content; } catch {}
        row.append(
          el("span", "label", m.dir === "in" ? `${m.channel} →` : `← ${m.channel}`),
          el("span", undefined, ` ${text}`),
        );
        return row;
      }),
    );
    log.scrollTop = log.scrollHeight;
  };
  socket.on("session_outbound", (msg) => {
    if (msg.agent_id === agentId && detail.isConnected) refresh();
  });
  detail.replaceChildren(head, log);
  await refresh();
}

// ---- graph ----

export async function renderGraph(panel) {
  const wrap = section("Knowledge graph");
  wrap.append(desc("MaturanaGraph — the agents' shared memory store. Check a graph's size, run a retrieval query (returns the top relevant entities, not the whole document), or ingest a file (PDF/DOCX/PPTX/MD/TXT)."));
  const graphInput = el("input", "model-input");
  graphInput.value = "personal";
  const out = el("div");

  const controls = el("div", "dash-actions");
  controls.append(
    el("span", "label", "graph"),
    graphInput,
    button("stats", async () => {
      try {
        out.replaceChildren(jsonBlock(await api("/api/graph/stats", {
          method: "POST",
          body: JSON.stringify({ graph: graphInput.value }),
        })));
      } catch (error) {
        out.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
  );

  const queryRow = el("div", "dash-actions");
  const terms = el("input", "model-input");
  terms.placeholder = "query terms…";
  terms.style.flex = "1";
  const runQuery = async () => {
    try {
      const data = await api("/api/graph/query", {
        method: "POST",
        body: JSON.stringify({
          graph: graphInput.value,
          query_terms: terms.value.split(/\s+/).filter(Boolean),
        }),
      });
      out.replaceChildren(jsonBlock(data.result?.rendered_context ?? data));
    } catch (error) {
      out.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  terms.addEventListener("keydown", (e) => { if (e.key === "Enter") runQuery(); });
  queryRow.append(terms, button("query", runQuery));

  const uploadRow = el("div", "dash-actions");
  const file = el("input", "file-input");
  file.type = "file";
  file.id = "graph-file";
  const pickLabel = el("label", "file-pick", "Choose file");
  pickLabel.htmlFor = "graph-file";
  const fileName = el("span", "file-name", "no file selected");
  file.addEventListener("change", () => { fileName.textContent = file.files?.[0]?.name || "no file selected"; });
  uploadRow.append(
    pickLabel,
    file,
    fileName,
    button("ingest", async () => {
      const picked = file.files?.[0];
      if (!picked) { toast("Choose a file to ingest first", "bad"); return; }
      out.replaceChildren(el("div", "label", "[ ingesting… ]"));
      try {
        const data = await api("/api/graph/ingest", {
          method: "POST",
          headers: {
            "x-maturana-filename": picked.name,
            "x-maturana-graph": graphInput.value,
          },
          body: picked,
        });
        out.replaceChildren(el("div", "status-ok", `[ ingested ${data.file} · ${data.chunks} chunks ]`), jsonBlock(data));
      } catch (error) {
        out.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
  );

  wrap.append(controls, queryRow, uploadRow, out);
  panel.replaceChildren(wrap);
}

// ---- pipelock ----

export async function renderPipelock(panel) {
  const wrap = section("Pipelock secrets");
  wrap.append(desc(
    "Host-side secret vault. Agents reference a secret by name (e.g. pipelock:brave/api-key); the host proxy injects the value into outbound requests — it is NEVER sent to the browser or into a VM. You can set or delete a value here, but not read it back, by design."));
  const out = el("div");
  const draw = async () => {
    const data = await api("/api/pipelock/secrets");
    const rows = data.names.map((name) => [
      name,
      button("delete", async () => {
        if (!(await confirmDialog({ title: "Delete secret", message: `Delete secret ${name}?`, danger: true, confirmLabel: "Delete" }))) return;
        await api(`/api/pipelock/secrets/${encodeURIComponent(name)}`, { method: "DELETE" });
        draw();
      }, true),
    ]);
    out.replaceChildren(rows.length ? table(["name", ""], rows) : el("div", "panel-desc", "no secrets stored"));
  };
  await draw();

  const add = el("div", "dash-actions");
  const name = el("input", "model-input");
  name.placeholder = "name (e.g. brave/api-key)";
  const value = el("input", "model-input");
  value.type = "password";
  value.placeholder = "value (write-only)";
  const addOut = el("div");
  add.append(name, value, button("set", async () => {
    if (!name.value.trim() || !value.value) { toast("Name and value are both required", "bad"); return; }
    try {
      await api("/api/pipelock/secrets", {
        method: "POST",
        body: JSON.stringify({ name: name.value.trim(), value: value.value }),
      });
      addOut.replaceChildren(el("div", "status-ok", `[ set ${name.value.trim()} ]`));
      value.value = "";
      draw();
    } catch (error) {
      addOut.replaceChildren(el("div", "status-bad", String(error)));
    }
  }));

  wrap.append(
    add,
    el("div", "panel-desc", "Set writes (or silently overwrites) a value — there is no read-back to confirm the previous one. To rotate a key, just set the new value."),
    out,
  );
  panel.replaceChildren(wrap);
}

// ---- tools / skills ----

export async function renderTools(panel) {
  // Tools = host-registered WASM modules an agent can call (sandboxed, no
  // ambient authority). This page is about THOSE tools — the catalog, defining
  // new ones, and wiring them to agents. Skills, MCP servers and capabilities
  // have their own pages / live under Agents → config.
  const wrap = section("Tools");
  wrap.append(desc("Host-registered WASM tools — sandboxed modules an agent can call over a stdin/stdout JSON contract. Define a tool below, then wire it to an agent."));
  const body = el("div");
  body.append(el("div", "panel-desc", "loading…"));
  wrap.append(body);
  panel.replaceChildren(wrap);

  // 1) The catalog: what tools exist on this host.
  let tools = [];
  try {
    tools = (await api("/api/tools")) || [];
    const rows = tools.map((t) => [
      el("strong", null, t.name),
      t.version,
      t.description || "—",
      capSummary(t.capabilities),
    ]);
    body.replaceChildren(
      rows.length
        ? table(["tool", "version", "description", "capabilities"], rows)
        : el("div", "panel-desc", "No tools registered yet — use “Define a tool” below."),
    );
  } catch (error) {
    body.replaceChildren(el("div", "status-bad", String(error)));
  }

  // 2) Define a tool: upload a compiled .wasm + manifest → host registry.
  const defWrap = section("Define a tool");
  defWrap.append(desc("Register a compiled WebAssembly module as a tool. Declared capabilities are the ONLY authority it gets (none = pure compute); network is enforced by the egress proxy. Mirrors `maturana tool register`."));
  defWrap.append(button("Define a tool…", () => openDefineTool(panel)));
  panel.append(defWrap);

  // 3) Wire a tool to an agent (adds it to spec.tools; effective on restart).
  const enableWrap = section("Wire a tool to an agent");
  enableWrap.append(desc("Adds a tool to the agent's spec.tools so the agent may call it. Takes effect on the agent's next restart."));
  let agents = [];
  try { agents = (await api("/api/agents")) || []; } catch {}
  const enRow = el("div", "dash-actions");
  const agentSel = el("select", "model-input");
  for (const a of agents) { const o = el("option", null, a.agent_id); o.value = a.agent_id; agentSel.append(o); }
  const toolSel = el("select", "model-input");
  { const o = el("option", null, "— pick a registered tool —"); o.value = ""; toolSel.append(o); }
  for (const t of tools) { const o = el("option", null, `${t.name} (v${t.version})`); o.value = t.name; toolSel.append(o); }
  const toolInput = el("input", "model-input");
  toolInput.placeholder = "or type a tool name";
  toolSel.addEventListener("change", () => { if (toolSel.value) toolInput.value = toolSel.value; });
  const enStatus = el("span", "panel-desc");
  enRow.append(agentSel, toolSel, toolInput, button("wire", async () => {
    const id = agentSel.value;
    const tool = (toolInput.value || toolSel.value).trim();
    if (!id || !tool) { toast("Pick an agent and a tool", "bad"); return; }
    enStatus.textContent = "saving…";
    try {
      const cur = await api(`/api/agents/${id}/config?section=tools`);
      const list = Array.isArray(cur.value) ? cur.value.slice() : [];
      if (!list.includes(tool)) list.push(tool);
      await api(`/api/agents/${id}/config`, { method: "PUT", body: JSON.stringify({ section: "tools", value: list }) });
      enStatus.textContent = `wired "${tool}" to ${id} — applies on next restart`;
      toolInput.value = "";
      renderTools(panel);
    } catch (e) { enStatus.textContent = ""; toast(String(e), "bad"); }
  }), enStatus);
  enableWrap.append(enRow);

  // Which tools each agent currently has wired (tools only — not skills).
  try {
    const details = await Promise.all(agents.map((a) => api(`/api/agents/${a.agent_id}/detail`).catch(() => null)));
    const rows = details.filter(Boolean).map((d) => [el("strong", null, d.agent_id), chipsOf(d.tools)]);
    if (rows.length) enableWrap.append(table(["agent", "wired tools"], rows));
  } catch {}
  panel.append(enableWrap);
}

// One-line capabilities summary for a tool manifest.
function capSummary(c) {
  if (!c) return "pure compute";
  const parts = [];
  if (c.net && c.net.length) parts.push(`net: ${c.net.join(", ")}`);
  if (c.fs_read && c.fs_read.length) parts.push(`read: ${c.fs_read.join(", ")}`);
  if (c.fs_write && c.fs_write.length) parts.push(`write: ${c.fs_write.join(", ")}`);
  if (c.env && c.env.length) parts.push(`env: ${c.env.join(", ")}`);
  return parts.length ? parts.join(" · ") : "pure compute";
}

// Modal: define (register) a tool by uploading a compiled .wasm + manifest.
function openDefineTool(panel) {
  formDialog({
    title: "Define a tool",
    sub: "Upload a compiled WebAssembly module and declare what it may touch.",
    fields: [
      { name: "name", label: "Name", required: true, placeholder: "lowercase-dashes", hint: "lowercase letters, digits, dashes" },
      { name: "version", label: "Version", value: "0.1.0" },
      { name: "description", label: "Description", type: "textarea", rows: 2 },
      { name: "wasm", label: "WASM module (.wasm)", type: "file", accept: ".wasm,application/wasm", required: true },
      { name: "net", label: "Network hosts", placeholder: "api.example.com, other.com", hint: "comma-separated; enforced by the egress proxy" },
      { name: "env", label: "Env var names", placeholder: "API_KEY", hint: "comma-separated; values passed through at call time" },
      { name: "fs_read", label: "Readable dirs", placeholder: "/data", hint: "comma-separated host dirs" },
      { name: "fs_write", label: "Writable dirs", placeholder: "/tmp/out", hint: "comma-separated host dirs" },
    ],
    submitLabel: "Register",
    onSubmit: async (v) => {
      if (!v.wasm) throw new Error("a .wasm module is required");
      const params = new URLSearchParams();
      params.set("name", v.name);
      if (v.version) params.set("version", v.version);
      if (v.description) params.set("description", v.description);
      for (const k of ["net", "env", "fs_read", "fs_write"]) if (v[k]) params.set(k, v[k]);
      const bytes = await v.wasm.arrayBuffer();
      const res = await fetch(`/api/tools?${params.toString()}`, {
        method: "POST",
        headers: { "x-maturana-web": "1", "content-type": "application/wasm" },
        body: bytes,
      });
      const payload = await res.json().catch(() => ({ ok: false, error: "bad json" }));
      if (!payload.ok) throw new Error(payload.error || `http ${res.status}`);
      toast(`Registered tool "${payload.data.name}" v${payload.data.version}`, "ok");
      renderTools(panel);
    },
  });
}

// ---- egress (live governance) ----

export async function renderEgress(panel, socket) {
  // 1) Per-agent allowlist editor — the single place egress hosts are managed.
  const editor = section("Allowlist");
  editor.append(desc("The hosts each agent is allowed to reach; everything else is blocked by the host proxy. Edits re-validate the whole spec before writing — a running agent picks them up on its next restart."));
  const agentSel = el("select", "model-input");
  try {
    for (const a of await api("/api/agents")) { const o = el("option", null, a.agent_id); o.value = a.agent_id; agentSel.append(o); }
  } catch {}
  const hosts = el("textarea", "spec-editor");
  hosts.placeholder = "one host per line, e.g. api.openai.com";
  hosts.style.minHeight = "120px";
  // Allow-all toggle: open egress entirely (still proxied + audited as allow_all).
  const allowAll = el("input");
  allowAll.type = "checkbox";
  const allowAllLabel = el("label", "panel-desc");
  allowAllLabel.style.display = "flex";
  allowAllLabel.style.alignItems = "center";
  allowAllLabel.style.gap = "6px";
  allowAllLabel.append(allowAll, document.createTextNode(
    "Allow all egress — this agent may reach ANY host (governance off; still proxied + audited). Prefer a scoped list when hosts are known."));
  const syncHostsState = () => { hosts.disabled = allowAll.checked; hosts.style.opacity = allowAll.checked ? "0.5" : "1"; };
  allowAll.addEventListener("change", syncHostsState);
  const editorOut = el("div");
  const loadHosts = async () => {
    if (!agentSel.value) return;
    try {
      const data = await api(`/api/agents/${agentSel.value}/egress`);
      hosts.value = (data.egress_allowlist || []).join("\n");
      allowAll.checked = !!data.egress_allow_all;
      syncHostsState();
      editorOut.replaceChildren(el("div", "panel-desc",
        `${(data.inject_headers || []).length} header injection(s) configured for this agent (managed in its spec).`));
    } catch (error) { editorOut.replaceChildren(el("div", "status-bad", String(error))); }
  };
  agentSel.addEventListener("change", loadHosts);
  const editorRow = el("div", "dash-actions");
  editorRow.append(agentSel, button("reload", loadHosts), button("save", async () => {
    try {
      await api(`/api/agents/${agentSel.value}/egress`, {
        method: "PUT",
        body: JSON.stringify({
          egress_allowlist: hosts.value.split("\n").map((h) => h.trim()).filter(Boolean),
          egress_allow_all: allowAll.checked,
        }),
      });
      editorOut.replaceChildren(el("div", "status-ok", "[ saved + validated — applies on next restart ]"));
    } catch (error) { editorOut.replaceChildren(el("div", "status-bad", String(error))); }
  }));
  editor.append(editorRow, allowAllLabel, hosts, editorOut);
  await loadHosts();

  // 2) Live feed of proxy decisions, with hot-approve for denials.
  const wrap = section("Live feed");
  wrap.append(desc("Egress decisions from the host proxy as they happen — allowed or denied. Approve a denied host to grant it (tick 'perm' to also write it to the agent's spec)."));
  const feed = el("div", "session-log");
  feed.style.maxHeight = "55vh";
  wrap.append(feed);
  panel.replaceChildren(editor, wrap);

  socket.subscribe(["egress"]);

  const seen = new Set(); // de-dupe approved denials so the button disappears
  socket.on("dash_update", (msg) => {
    if (msg.topic !== "egress" || !panel.contains(wrap)) return;
    const e = msg.data;
    const denied = e.action === "pipelock.proxy.denied";
    const row = el("div", `session-msg ${denied ? "out" : "in"}`);
    const badge = denied ? "DENY" : `OK·${e.grant_source ?? "spec"}`;
    const when = (e.at ?? "").slice(11, 19);
    row.append(
      el("span", denied ? "status-bad" : "status-ok", `[${badge}] `),
      el("span", "status-dim", `${when} `),
      el("span", undefined, `${e.agent_id ?? "—"}  ${e.method ?? ""} ${e.host ?? ""}`),
    );
    if (denied && e.host && !seen.has(e.host)) {
      const perm = document.createElement("input");
      perm.type = "checkbox";
      perm.title = "make permanent (write to spec)";
      const approve = button("approve", async () => {
        try {
          await api("/api/egress/approve", {
            method: "POST",
            body: JSON.stringify({
              host: e.host,
              permanent: perm.checked,
              agent_id: e.agent_id ?? null,
            }),
          });
          seen.add(e.host);
          row.append(el("span", "status-ok", "  [granted]"));
          approve.remove();
          perm.remove();
        } catch (error) {
          row.append(el("span", "status-bad", `  [${error}]`));
        }
      });
      approve.style.marginLeft = "10px";
      const permLabel = el("label", "label");
      permLabel.style.marginLeft = "8px";
      permLabel.append(perm, document.createTextNode(" perm"));
      row.append(approve, permLabel);
    }
    feed.append(row);
    while (feed.childElementCount > 300) feed.firstChild.remove();
    feed.scrollTop = feed.scrollHeight;
  });
}

export async function renderSkills(panel) {
  const wrap = section("Skills");
  wrap.append(desc("Skills are Markdown procedures (SKILL.md) the host's Codex console follows — operator-level playbooks for running Maturana, under skills/<name>. They are NOT pushed into agent VMs; an agent gets its own skills from its spec at launch."));
  const detail = el("div", "skill-view");
  const listBox = el("div");

  const draw = async () => {
    const skills = await api("/api/skills");
    const rows = skills.map((s) => [
      el("strong", null, s.name),
      s.summary || "—",
      button("view", async () => {
        detail.replaceChildren(el("div", "label", `[ loading ${s.name}… ]`));
        try {
          const data = await api(`/api/skills/${s.name}`);
          const md = el("div", "turn-output");
          md.innerHTML = renderMd(data.markdown);
          detail.replaceChildren(el("div", "label dash-title", s.name), md);
          detail.scrollIntoView({ behavior: "smooth", block: "nearest" });
        } catch (error) {
          detail.replaceChildren(el("div", "status-bad", String(error)));
        }
      }),
    ]);
    listBox.replaceChildren(
      rows.length ? table(["skill", "use when", ""], rows) : el("div", "panel-desc", "no skills defined yet"),
    );
  };

  // Define a new skill.
  const create = section("Define a skill");
  create.append(desc("Write a SKILL.md. The first paragraph after the heading is the 'use this when' summary shown above."));
  const nameI = el("input", "model-input");
  nameI.placeholder = "skill-name (a-z 0-9 - _)";
  const md = el("textarea", "spec-editor");
  md.style.minHeight = "30vh";
  md.placeholder = "# My Skill\n\nUse this when …\n\n## Steps\n1. …";
  const createOut = el("div");
  const row = el("div", "dash-actions");
  row.append(nameI, button("create skill", async () => {
    if (!nameI.value.trim() || !md.value.trim()) {
      createOut.replaceChildren(el("div", "status-bad", "name and body are required"));
      return;
    }
    try {
      await api("/api/skills", { method: "POST", body: JSON.stringify({ name: nameI.value.trim(), markdown: md.value }) });
      createOut.replaceChildren(el("div", "status-ok", `[ created skill ${nameI.value.trim()} ]`));
      nameI.value = "";
      md.value = "";
      draw();
    } catch (error) {
      createOut.replaceChildren(el("div", "status-bad", String(error)));
    }
  }));
  create.append(row, md, createOut);

  await draw();
  wrap.append(listBox);
  panel.replaceChildren(wrap, create, detail);
}

// ---- channels (lives everywhere) ----

export async function renderChannels(panel) {
  const wrap = section("Channels");
  wrap.append(desc("Every chat surface each agent exposes — one agent, one memory, every surface. “live” means the supervisor is running that bridge right now."));
  let rows = [];
  try {
    rows = await api("/api/channels");
  } catch (e) {
    wrap.append(el("div", "status-bad", String(e)));
    panel.replaceChildren(wrap);
    return;
  }
  const order = ["web", "tui", "telegram", "discord", "slack", "agentmail"];
  const cell = (ch) => {
    if (!ch || !ch.configured) return el("span", "panel-desc", "—");
    const b = ch.live ? badge("live", "good") : badge("down", "warn");
    if (ch.detail) b.title = ch.detail;
    return b;
  };
  const trows = rows.map((r) => {
    const byName = Object.fromEntries((r.channels || []).map((c) => [c.name, c]));
    return [el("strong", null, r.agent_id), ...order.map((n) => cell(byName[n]))];
  });
  if (!trows.length) wrap.append(el("div", "panel-desc", "No agents."));
  else wrap.append(table(["agent", ...order], trows));
  panel.replaceChildren(wrap);
}

// ---- schedules (focused automation) ----

export async function renderSchedules(panel) {
  const wrap = section("Schedules");
  wrap.append(desc("Recurring agent tasks the plane fires unattended — reports, backups, briefings. Same store as `maturana schedule`; cron is 5-field (min hour dom month dow)."));
  const listBox = el("div");

  async function draw() {
    listBox.replaceChildren(el("div", "panel-desc", "loading…"));
    let items = [];
    try {
      items = await api("/api/schedules");
    } catch (e) {
      listBox.replaceChildren(el("div", "status-bad", String(e)));
      return;
    }
    if (!items.length) {
      listBox.replaceChildren(el("div", "panel-desc", "No schedules yet — add one below."));
      return;
    }
    const rows = items.map((s) => {
      const toggle = button(s.enabled ? "disable" : "enable", async () => {
        try { await api(`/api/schedules/${s.agent_id}/${s.id}/toggle`, { method: "POST" }); draw(); }
        catch (e) { toast(String(e), "bad"); }
      });
      const del = button("delete", async () => {
        if (!(await confirmDialog({ title: "Delete schedule", message: `Delete schedule “${s.name}”?`, danger: true, confirmLabel: "Delete" }))) return;
        try { await api(`/api/schedules/${s.agent_id}/${s.id}`, { method: "DELETE" }); draw(); }
        catch (e) { toast(String(e), "bad"); }
      }, true);
      const acts = el("div", "row-actions");
      acts.append(toggle, del);
      const lastRun = typeof s.last_run === "string" ? s.last_run : (s.last_run ? JSON.stringify(s.last_run) : "—");
      const action = s.board ? `▶ board: ${s.board}` : (s.prompt || "").slice(0, 60);
      return [
        s.agent_id,
        s.name,
        el("code", null, s.cron),
        s.channel || "—",
        s.enabled ? badge("on", "good") : badge("off", "dim"),
        action,
        acts,
      ];
    });
    listBox.replaceChildren(table(["agent", "name", "cron", "channel", "enabled", "prompt / board", "actions"], rows));
  }

  // ---- add card ----
  const add = section("Add a schedule");
  let agents = [];
  try { agents = await api("/api/agents"); } catch { agents = []; }
  const agentSel = el("select", "model-input");
  for (const a of agents) {
    const o = el("option", null, a.agent_id);
    o.value = a.agent_id;
    agentSel.append(o);
  }
  const nameIn = el("input", "model-input"); nameIn.placeholder = "name (e.g. morning-brief)";
  const cronIn = el("input", "model-input"); cronIn.placeholder = "cron: 0 8 * * 1-5";
  const promptIn = el("input", "model-input"); promptIn.placeholder = "prompt the agent runs";
  const channelIn = el("input", "model-input"); channelIn.placeholder = "channel (optional, e.g. telegram)";
  const boardIn = el("input", "model-input"); boardIn.placeholder = "or run a board (optional, board name)";
  const addStatus = el("span", "panel-desc");
  const addBtn = button("Add schedule", async () => {
    const agent = agentSel.value;
    if (!agent) { toast("Pick an agent for the schedule", "bad"); return; }
    addStatus.textContent = "saving…";
    try {
      await api(`/api/schedules/${agent}`, {
        method: "POST",
        body: JSON.stringify({
          name: nameIn.value.trim(),
          cron: cronIn.value.trim(),
          prompt: promptIn.value.trim(),
          channel: channelIn.value.trim() || null,
          board: boardIn.value.trim() || null,
        }),
      });
      nameIn.value = ""; cronIn.value = ""; promptIn.value = ""; channelIn.value = ""; boardIn.value = "";
      addStatus.textContent = "added";
      draw();
    } catch (e) { addStatus.textContent = ""; toast(String(e), "bad"); }
  });
  const addRow = el("div", "opt-grid");
  for (const node of [agentSel, nameIn, cronIn, promptIn, channelIn, boardIn]) addRow.append(node);
  add.append(addRow, el("div", "row-actions"));
  add.lastChild.append(addBtn, addStatus);

  await draw();
  wrap.append(listBox);
  panel.replaceChildren(wrap, add);
}

// ============================================================
// Orchestration — durable, user-defined boards run across agents.
// Board editor + live monitor: drag-drop columns, a full card drawer
// (edit / comments / run history / attachments / decompose / specify),
// per-column inline create, filters, multi-board. No native dialogs.
// ============================================================

const ORCH_ROLES = ["developer", "researcher", "reviewer", "coordinator", "synthesizer"];
const ORCH_COLUMNS = [
  ["triage", "Triage"],
  ["todo", "To do"],
  ["doing", "Doing"],
  ["done", "Done"],
  ["blocked", "Blocked"],
];

function cardPill(st) {
  if (st === "done") return badge("done", "good");
  if (st === "doing") return badge("doing", "warn");
  if (st === "blocked") return badge("blocked", "bad");
  if (st === "triage") return badge("triage", "dim");
  if (st === "archived") return badge("archived", "dim");
  return badge("todo", "dim");
}

export async function renderOrchestration(panel, socket) {
  const wrap = section("Orchestration");
  wrap.append(desc("Break a job into cards — each a task for one agent — give them assignees and dependencies, then Run. Cards run in parallel across your agents as soon as their dependencies finish; click a card to edit it or read its result."));
  // Legend: what the columns mean. This is the single biggest source of
  // confusion — you author cards in Triage/To do; the runner owns Doing/Done/
  // Blocked while a Run is in progress (so dragging there is rarely what you want).
  const legend = el("div", "orch-legend");
  const lg = (label, rest) => { const s = el("span"); s.append(el("b", null, label), document.createTextNode(" " + rest)); return s; };
  legend.append(
    lg("Triage", "rough ideas — use ⚗ Decompose / ✨ Specify to flesh out"),
    lg("To do", "ready to run"),
    lg("Doing · Done · Blocked", "set by the runner during a Run"),
  );
  const keys = el("span", "orch-legend-keys");
  keys.textContent = "card badges:  p# priority · after deps · goal judge-loop · ⟳ runs · 📎 files";
  legend.append(keys);
  const bar = el("div", "orch-bar");
  const filters = el("div", "orch-filters");
  const body = el("div");
  wrap.append(legend, bar, filters, body);
  panel.replaceChildren(wrap);

  const state = {
    boards: [],
    current: null,
    agents: [],
    detail: null,
    pollTimer: null,
    drawer: null, // open card id
    q: "",
    assignee: "",
    archived: false,
  };

  try { state.agents = await api("/api/agents"); } catch { state.agents = []; }

  function assigneeOptions(includeAny) {
    const opts = [];
    if (includeAny) opts.push({ value: "", label: "(any assignee)" });
    else opts.push({ value: "", label: "(default: developer)" });
    for (const r of ORCH_ROLES) opts.push({ value: r, label: `role: ${r}` });
    for (const a of state.agents) opts.push({ value: a.agent_id, label: a.agent_id });
    return opts;
  }

  function stopPoll() { if (state.pollTimer) { clearInterval(state.pollTimer); state.pollTimer = null; } }
  async function loadBoards() { try { state.boards = await api("/api/boards"); } catch { state.boards = []; } }

  // ---------- toolbar ----------
  function drawBar() {
    bar.replaceChildren();
    if (!state.current && state.boards.length) state.current = state.boards[0].name;
    const sel = el("select", "model-input");
    if (!state.boards.length) { const o = el("option", null, "(no boards yet)"); sel.append(o); }
    for (const b of state.boards) {
      const o = el("option", null, `${b.name}  (${b.done}/${b.total}${b.running ? " · running" : ""})`);
      o.value = b.name;
      if (b.name === state.current) o.selected = true;
      sel.append(o);
    }
    sel.addEventListener("change", () => { state.current = sel.value; state.drawer = null; drawBoard(); });
    bar.append(el("span", "panel-desc", "Board:"), sel);
    bar.append(button("＋ New", () => {
      formDialog({
        title: "New board",
        fields: [{ name: "name", label: "Board name", type: "text", placeholder: "letters, digits, - _", required: true }],
        submitLabel: "Create",
        onSubmit: async (v) => {
          await api("/api/boards", { method: "POST", body: JSON.stringify({ name: v.name }) });
          state.current = v.name; await loadBoards(); drawBar(); drawBoard(); toast("board created", "ok");
        },
      });
    }));
    if (state.current) {
      bar.append(button("Rename", () => {
        formDialog({
          title: `Rename board "${state.current}"`,
          fields: [{ name: "name", label: "New name", type: "text", value: state.current, required: true }],
          submitLabel: "Rename",
          onSubmit: async (v) => {
            await api(`/api/boards/${state.current}/rename`, { method: "POST", body: JSON.stringify({ name: v.name }) });
            state.current = v.name; await loadBoards(); drawBar(); drawBoard(); toast("renamed", "ok");
          },
        });
      }));
      bar.append(button("Delete", async () => {
        if (!(await confirmDialog({ title: "Delete board", message: `Delete "${state.current}" and all its cards?`, danger: true, confirmLabel: "Delete" }))) return;
        try { await api(`/api/boards/${state.current}`, { method: "DELETE" }); state.current = null; await loadBoards(); drawBar(); drawBoard(); toast("deleted", "ok"); }
        catch (e) { toast(String(e), "bad"); }
      }, true));
    }
  }

  function drawFilters() {
    filters.replaceChildren();
    if (!state.current) return;
    const q = el("input", "model-input orch-search"); q.placeholder = "filter cards…"; q.value = state.q;
    q.addEventListener("input", () => { state.q = q.value.toLowerCase(); if (state.detail) renderColumns(state.detail); });
    const asg = el("select", "model-input");
    for (const o of assigneeOptions(true)) { const opt = el("option", null, o.label); opt.value = o.value; asg.append(opt); }
    asg.value = state.assignee;
    asg.addEventListener("change", () => { state.assignee = asg.value; if (state.detail) renderColumns(state.detail); });
    const arch = el("label", "orch-toggle");
    const cb = el("input"); cb.type = "checkbox"; cb.checked = state.archived;
    cb.addEventListener("change", () => { state.archived = cb.checked; if (state.detail) renderColumns(state.detail); });
    arch.append(cb, document.createTextNode(" show archived"));
    filters.append(q, asg, arch);
  }

  // ---------- board ----------
  async function drawBoard() {
    stopPoll();
    drawFilters();
    if (!state.current) {
      body.replaceChildren(el("div", "panel-desc", "Create a board to get started — then add cards and run them across your agents."));
      return;
    }
    let d;
    try { d = await api(`/api/boards/${state.current}`); }
    catch (e) { body.replaceChildren(el("div", "status-bad", String(e))); return; }
    state.detail = d;
    renderToolbar2(d);
    renderColumns(d);
    if (state.drawer) openDrawer(state.drawer);
    if (d.running) {
      state.pollTimer = setInterval(async () => {
        if (!panel.contains(body)) { stopPoll(); return; }
        try {
          const nd = await api(`/api/boards/${state.current}`);
          state.detail = nd; renderToolbar2(nd); renderColumns(nd);
          if (state.drawer) openDrawer(state.drawer);
          if (!nd.running) { stopPoll(); loadBoards().then(drawBar); }
        } catch { stopPoll(); }
      }, 1500);
    }
  }

  let toolbar2 = null;
  function renderToolbar2(d) {
    if (!toolbar2) { toolbar2 = el("div", "row-actions orch-runbar"); }
    toolbar2.replaceChildren();
    const cards = (d.cards || []).filter((c) => c.status !== "archived");
    const total = cards.length;
    const done = cards.filter((c) => c.status === "done").length;
    const doing = cards.filter((c) => c.status === "doing");
    const runBtn = button(d.running ? `running · ${done}/${total}` : "▶ Run board", async () => {
      try { await api(`/api/boards/${state.current}/run`, { method: "POST" }); toast("dispatching…", "ok"); drawBoard(); }
      catch (e) { toast(String(e), "bad"); }
    });
    if (d.running) runBtn.disabled = true;
    toolbar2.append(runBtn);
    // Live progress: which cards are running, or a static done/total when idle.
    if (d.running && doing.length) {
      toolbar2.append(el("span", "orch-running", `▷ ${doing.map((c) => `${c.id} @${c.assignee || "default"}`).join(", ")}`));
    } else if (d.running) {
      toolbar2.append(el("span", "orch-running", "dispatching…"));
    } else if (total) {
      toolbar2.append(el("span", "panel-desc", `${done}/${total} done`));
    }
    // Reset is destructive (wipes results) — pushed to the far right, away from Run.
    toolbar2.append(el("span", "orch-spacer"));
    toolbar2.append(button("↺ Reset (drops results)", async () => {
      if (!(await confirmDialog({ title: "Reset board", message: "Set every card back to 'to do' and drop all prior results? This can't be undone.", danger: true, confirmLabel: "Reset" }))) return;
      try { await api(`/api/boards/${state.current}/reset`, { method: "POST" }); toast("reset", "ok"); drawBoard(); }
      catch (e) { toast(String(e), "bad"); }
    }, true));
    if (!body.contains(toolbar2)) body.replaceChildren(toolbar2);
  }

  function passesFilter(c) {
    if (state.q && !`${c.id} ${c.title} ${c.assignee || ""}`.toLowerCase().includes(state.q)) return false;
    if (state.assignee && c.assignee !== state.assignee) return false;
    return true;
  }

  function renderColumns(d) {
    // keep toolbar2 (first child), replace the rest
    const cols = el("div", "board-cols");
    const columns = state.archived ? [...ORCH_COLUMNS, ["archived", "Archived"]] : ORCH_COLUMNS;
    for (const [st, label] of columns) {
      const col = el("div", "board-col");
      col.dataset.status = st;
      const cards = (d.cards || []).filter((c) => c.status === st && passesFilter(c));
      const head = el("div", "board-col-head-row");
      head.append(el("span", "board-col-head", `${label} · ${cards.length}`));
      const addBtn = el("button", "board-col-add", "＋");
      addBtn.title = `Add a card to ${label}`;
      addBtn.addEventListener("click", () => addCardForm(st === "triage" ? "triage" : "todo"));
      head.append(addBtn);
      col.append(head);
      for (const c of cards) col.append(cardTile(c, d.cards));
      // drag-drop target
      col.addEventListener("dragover", (e) => { e.preventDefault(); col.classList.add("drop"); });
      col.addEventListener("dragleave", () => col.classList.remove("drop"));
      col.addEventListener("drop", async (e) => {
        e.preventDefault(); col.classList.remove("drop");
        const id = e.dataTransfer.getData("text/card");
        if (!id) return;
        const card = (state.detail.cards || []).find((x) => x.id === id);
        if (!card || card.status === st) return;
        try { await api(`/api/boards/${state.current}/cards/${id}`, { method: "PUT", body: JSON.stringify({ status: st }) }); drawBoard(); }
        catch (err) { toast(String(err), "bad"); }
      });
      cols.append(col);
    }
    // rebuild body = toolbar2 + cols
    body.replaceChildren(toolbar2, cols);
    if (d.events && d.events.length) body.append(runLog(d.events));
  }

  function runLog(events) {
    const log = section("Run log");
    const lv = el("div", "log-view");
    for (const e of events.slice(-40)) {
      const row = el("div", "log-row");
      row.append(el("span", "log-time", new Date(e.at).toLocaleTimeString()), el("span", "log-msg", `${e.kind}${e.card ? " " + e.card : ""} ${e.text || ""}`.trim()));
      lv.append(row);
    }
    log.append(lv);
    return log;
  }

  function cardTile(c, allCards) {
    const tile = el("div", "board-card");
    tile.draggable = true;
    tile.addEventListener("dragstart", (e) => { e.dataTransfer.setData("text/card", c.id); e.dataTransfer.effectAllowed = "move"; });
    tile.addEventListener("click", () => { state.drawer = c.id; openDrawer(c.id); });
    const head = el("div", "board-card-head");
    head.append(el("span", "board-card-id", c.id), el("span", "board-card-title", c.title));
    tile.append(head);
    const meta = el("div", "board-card-meta");
    meta.append(el("span", "board-card-asg", `@${c.assignee || "default"}`));
    if (c.priority) meta.append(badge(`p${c.priority}`, "dim"));
    if (c.goal) meta.append(badge("goal", "warn"));
    if (c.block_kind && c.status === "blocked") meta.append(badge(c.block_kind, "bad"));
    if (c.deps && c.deps.length) meta.append(el("span", "board-card-dep", `after ${c.deps.join(",")}`));
    tile.append(meta);
    const counts = [];
    if (c.comments && c.comments.length) counts.push(`💬${c.comments.length}`);
    if (c.runs && c.runs.length) counts.push(`⟳${c.runs.length}`);
    if (c.attachments && c.attachments.length) counts.push(`📎${c.attachments.length}`);
    if (counts.length) tile.append(el("div", "board-card-counts", counts.join("  ")));
    return tile;
  }

  // ---------- create / edit forms ----------
  function cardFields(card, allCards) {
    const depOpts = (allCards || []).filter((x) => !card || x.id !== card.id).map((x) => ({ value: x.id, label: `${x.id} ${x.title.slice(0, 24)}` }));
    return [
      { name: "title", label: "Title", type: "text", value: card?.title, placeholder: "what the agent should do", required: true },
      { name: "detail", label: "Detail / acceptance criteria", type: "textarea", value: card?.detail, rows: 4, hint: "the full instructions + how to tell it's done (markdown)" },
      { name: "assignee", label: "Assignee", type: "select", value: card?.assignee || "", options: assigneeOptions(false) },
      { name: "deps", label: "Depends on", type: "multiselect", value: card?.deps || [], options: depOpts, hint: "this card waits until these finish; their results are fed in" },
      { name: "priority", label: "Priority (higher runs first)", type: "number", value: card?.priority ?? 0, advanced: true },
      { name: "max_retries", label: "Auto-retries on failure", type: "number", value: card?.max_retries ?? 0, advanced: true },
      { name: "goal", label: "Goal mode (re-run with an acceptance judge until it passes)", type: "checkbox", value: card?.goal || false, advanced: true },
      { name: "goal_max_turns", label: "Goal max rounds (0 = default 5)", type: "number", value: card?.goal_max_turns ?? 0, advanced: true },
      { name: "tenant", label: "Tenant (optional tag)", type: "text", value: card?.tenant || "", advanced: true },
      { name: "scheduled_at", label: "Don't run before (RFC3339, optional)", type: "text", value: card?.scheduled_at || "", placeholder: "2026-07-01T09:00:00Z", advanced: true },
    ];
  }

  function addCardForm(forceStatus) {
    formDialog({
      title: "Add a card",
      fields: cardFields(null, state.detail?.cards),
      submitLabel: "Add card",
      onSubmit: async (v) => {
        const body = {
          title: v.title, detail: v.detail, assignee: v.assignee || null, needs: v.deps,
          priority: Number(v.priority) || 0, max_retries: Number(v.max_retries) || 0,
          goal: !!v.goal, goal_max_turns: Number(v.goal_max_turns) || 0,
          tenant: v.tenant || null, scheduled_at: v.scheduled_at || null,
          triage: forceStatus === "triage",
        };
        await api(`/api/boards/${state.current}/cards`, { method: "POST", body: JSON.stringify(body) });
        drawBoard(); toast("card added", "ok");
      },
    });
  }

  function editCardForm(card) {
    formDialog({
      title: `Edit ${card.id}`,
      fields: [
        ...cardFields(card, state.detail?.cards),
        { name: "status", label: "Status", type: "select", value: card.status, options: ["triage", "todo", "doing", "done", "blocked", "archived"].map((s) => ({ value: s, label: s })) },
      ],
      submitLabel: "Save",
      onSubmit: async (v) => {
        const body = {
          title: v.title, detail: v.detail, assignee: v.assignee || null, deps: v.deps, status: v.status,
          priority: Number(v.priority) || 0, max_retries: Number(v.max_retries) || 0,
          goal: !!v.goal, goal_max_turns: Number(v.goal_max_turns) || 0,
          tenant: v.tenant || null, scheduled_at: v.scheduled_at,
        };
        await api(`/api/boards/${state.current}/cards/${card.id}`, { method: "PUT", body: JSON.stringify(body) });
        drawBoard(); toast("saved", "ok");
      },
    });
  }

  // ---------- card drawer ----------
  function openDrawer(cardId) {
    const c = (state.detail?.cards || []).find((x) => x.id === cardId);
    if (!c) { closeDrawer(); return; }
    let overlay = document.querySelector(".board-drawer-overlay");
    if (!overlay) {
      overlay = el("div", "board-drawer-overlay");
      overlay.addEventListener("mousedown", (e) => { if (e.target === overlay) closeDrawer(); });
      document.body.append(overlay);
    }
    const dr = el("div", "board-drawer");
    // header
    const hd = el("div", "board-drawer-head");
    const titleRow = el("div", "board-drawer-titlerow");
    titleRow.append(el("span", "board-card-id", c.id), cardPill(c.status), el("span", "board-drawer-title", c.title));
    const x = el("button", "board-drawer-x", "✕");
    x.addEventListener("click", closeDrawer);
    hd.append(titleRow, x);
    dr.append(hd);
    // meta + edit
    const meta = el("div", "panel-desc");
    const bits = [`@${c.assignee || "default"}`];
    if (c.priority) bits.push(`priority ${c.priority}`);
    if (c.deps?.length) bits.push(`after ${c.deps.join(",")}`);
    if (c.tenant) bits.push(`tenant ${c.tenant}`);
    if (c.max_retries) bits.push(`retries ${c.max_retries}`);
    if (c.goal) bits.push(`goal (≤${c.goal_max_turns || 5})`);
    if (c.scheduled_at) bits.push(`scheduled ${c.scheduled_at}`);
    if (c.block_kind && c.status === "blocked") bits.push(`blocked: ${c.block_kind}`);
    meta.textContent = bits.join(" · ");
    dr.append(meta);
    dr.append(el("div", "board-drawer-sub", "Task"));
    if (c.detail) { const d = el("div", "board-drawer-detail"); d.innerHTML = renderMd(c.detail); dr.append(d); }
    else dr.append(el("div", "panel-desc", "No detail yet — “Edit” to describe what to do and how to tell it's done."));

    // actions
    const acts = el("div", "row-actions");
    acts.append(button("Edit", () => editCardForm(c)));
    if (c.status === "triage") {
      acts.append(button("⚗ Decompose", async () => {
        try { await api(`/api/boards/${state.current}/cards/${c.id}/decompose`, { method: "POST" }); toast("decomposing… (refreshes when done)", "ok"); pollOnce(6); }
        catch (e) { toast(String(e), "bad"); }
      }));
      acts.append(button("✨ Specify", async () => {
        try { await api(`/api/boards/${state.current}/cards/${c.id}/specify`, { method: "POST" }); toast("specifying… (refreshes when done)", "ok"); pollOnce(6); }
        catch (e) { toast(String(e), "bad"); }
      }));
    }
    if (c.status !== "archived") {
      acts.append(button("Archive", async () => {
        try { await api(`/api/boards/${state.current}/cards/${c.id}`, { method: "PUT", body: JSON.stringify({ status: "archived" }) }); drawBoard(); }
        catch (e) { toast(String(e), "bad"); }
      }));
    }
    acts.append(button("Delete", async () => {
      if (!(await confirmDialog({ title: "Delete card", message: `Delete ${c.id}?`, danger: true, confirmLabel: "Delete" }))) return;
      try { await api(`/api/boards/${state.current}/cards/${c.id}`, { method: "DELETE" }); state.drawer = null; closeDrawer(); drawBoard(); }
      catch (e) { toast(String(e), "bad"); }
    }, true));
    dr.append(acts);

    // result
    if (c.result) {
      dr.append(el("div", "board-drawer-sub", "Result"));
      const r = el("div", "board-drawer-result"); r.innerHTML = renderMd(c.result); dr.append(r);
    }

    // attachments
    dr.append(el("div", "board-drawer-sub", "Attachments"));
    const att = el("div", "board-drawer-att");
    for (const p of c.attachments || []) {
      const a = document.createElement("a");
      a.className = "chat-download";
      a.textContent = `⬇ ${p.split(/[\\/]/).pop()}`;
      a.href = `/api/boards/${state.current}/attachment?path=${encodeURIComponent(p)}`;
      a.setAttribute("download", p.split(/[\\/]/).pop());
      att.append(a);
    }
    const fileInput = el("input", "chat-file-hidden"); fileInput.type = "file";
    fileInput.addEventListener("change", async () => {
      const f = fileInput.files?.[0]; if (!f) return;
      try {
        const res = await fetch(`/api/boards/${state.current}/cards/${c.id}/attach?name=${encodeURIComponent(f.name)}`, { method: "POST", headers: { "x-maturana-web": "1" }, body: f });
        const j = await res.json(); if (!j.ok) throw new Error(j.error);
        toast("attached", "ok"); drawBoard();
      } catch (e) { toast(String(e), "bad"); }
      fileInput.value = "";
    });
    const upBtn = button("📎 Attach file", () => fileInput.click());
    att.append(upBtn, fileInput);
    dr.append(att);

    // run history
    if (c.runs && c.runs.length) {
      dr.append(el("div", "board-drawer-sub", "Run history"));
      const rl = el("div", "log-view");
      for (const r of c.runs) {
        const row = el("div", "log-row");
        row.append(el("span", "log-time", `#${r.attempt}`), el("span", "log-msg", `${r.outcome} · ${r.agent || "?"} · ${(r.summary || "").slice(0, 80)}`));
        rl.append(row);
      }
      dr.append(rl);
    }

    // comments
    dr.append(el("div", "board-drawer-sub", "Comments"));
    const thread = el("div", "board-comments");
    for (const cm of c.comments || []) {
      const row = el("div", "board-comment");
      row.append(el("span", "board-comment-author", cm.author || "note"), el("span", "board-comment-body", cm.body));
      thread.append(row);
    }
    dr.append(thread);
    const commentRow = el("div", "board-comment-add");
    const ci = el("input", "model-input"); ci.placeholder = "add a comment (Enter to post)…";
    const post = async () => {
      const text = ci.value.trim(); if (!text) return;
      try { await api(`/api/boards/${state.current}/cards/${c.id}/comment`, { method: "POST", body: JSON.stringify({ body: text }) }); ci.value = ""; drawBoard(); }
      catch (e) { toast(String(e), "bad"); }
    };
    ci.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); post(); } });
    commentRow.append(ci, button("Post", post));
    dr.append(commentRow);

    overlay.replaceChildren(dr);
  }

  function closeDrawer() {
    state.drawer = null;
    const o = document.querySelector(".board-drawer-overlay");
    if (o) o.remove();
  }

  // a few quick extra polls (for decompose/specify which run detached ~seconds)
  function pollOnce(times) {
    let n = 0;
    const t = setInterval(async () => {
      n++;
      if (!panel.contains(body) || n > times) { clearInterval(t); return; }
      try { const nd = await api(`/api/boards/${state.current}`); state.detail = nd; renderColumns(nd); if (state.drawer) openDrawer(state.drawer); } catch {}
    }, 2500);
  }

  await loadBoards();
  drawBar();
  drawBoard();
}
