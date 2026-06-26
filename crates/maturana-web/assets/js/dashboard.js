// Dashboard views: agents, runtime, sessions, graph, pipelock, tools, skills.
// All REST mutations carry the x-maturana-web CSRF header; live updates ride
// the shared WebSocket (agents/runtime topics + session_outbound).

import { marked } from "/assets/vendor/marked/marked.esm.js";

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
  wrap.append(el("div", "label dash-title", title));
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

export async function renderSystem(panel) {
  // Host stats (auto-refresh every 5s while visible).
  const statsWrap = section("Host");
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
  const controls = el("div", "dash-actions");
  const sourceSel = el("select", "model-input");
  const linesInput = el("input", "model-input");
  linesInput.type = "number";
  linesInput.value = "200";
  linesInput.style.width = "90px";
  const logPre = el("pre", "dash-json");
  logPre.style.maxHeight = "42vh";
  logPre.style.overflow = "auto";
  const loadLog = async () => {
    try {
      const d = await api(`/api/system/logs?source=${encodeURIComponent(sourceSel.value)}&lines=${encodeURIComponent(linesInput.value || 200)}`);
      logPre.textContent = d.text || "(empty)";
      logPre.scrollTop = logPre.scrollHeight;
    } catch (error) {
      logPre.textContent = String(error);
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
  const tailChk = el("input");
  tailChk.type = "checkbox";
  const tailLabel = el("label", "label");
  tailLabel.append(tailChk, document.createTextNode(" auto-tail"));
  controls.append(sourceSel, linesInput, button("refresh", loadLog), tailLabel);
  logWrap.append(controls, logPre);
  await loadLog();
  const logTimer = setInterval(() => {
    if (!panel.contains(logWrap)) { clearInterval(logTimer); return; }
    if (tailChk.checked) loadLog();
  }, 5000);

  // Activity analytics (we don't meter token cost — that lives in the guest VMs).
  const aWrap = section("Activity");
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

  panel.replaceChildren(statsWrap, logWrap, aWrap);
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

export async function renderConfig(panel) {
  const wrap = section("Agent config");
  const agents = await api("/api/agents");
  const agentSel = el("select", "model-input");
  for (const a of agents) {
    const o = el("option", null, a.agent_id);
    o.value = a.agent_id;
    agentSel.append(o);
  }
  const sectionSel = el("select", "model-input");
  for (const s of CONFIG_SECTIONS) {
    const o = el("option", null, s);
    o.value = s;
    sectionSel.append(o);
  }
  const hint = el("div", "status-dim");
  const editor = el("textarea", "spec-editor");
  editor.style.minHeight = "38vh";
  const report = el("div");
  const load = async () => {
    report.replaceChildren();
    hint.textContent = CONFIG_HINTS[sectionSel.value] || "";
    try {
      const d = await api(`/api/agents/${agentSel.value}/config?section=${sectionSel.value}`);
      editor.value = JSON.stringify(d.value ?? null, null, 2);
    } catch (error) {
      report.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  const save = async () => {
    let value;
    try {
      value = JSON.parse(editor.value);
    } catch {
      report.replaceChildren(el("div", "status-bad", "invalid JSON"));
      return;
    }
    try {
      const d = await api(`/api/agents/${agentSel.value}/config`, {
        method: "PUT",
        body: JSON.stringify({ section: sectionSel.value, value }),
      });
      report.replaceChildren(el("div", "status-ok", `[ saved ${d.section} — spec valid; applies on next materialize/restart ]`));
    } catch (error) {
      report.replaceChildren(el("div", "status-bad", String(error)));
    }
  };
  agentSel.addEventListener("change", load);
  sectionSel.addEventListener("change", load);
  const controls = el("div", "dash-actions");
  controls.append(agentSel, sectionSel, button("load", load), button("save", save));
  wrap.append(
    controls,
    hint,
    el("div", "status-dim", "Validated before write. identity / vm / runtime (the isolation boundary) are not editable here — use the Agents → spec editor for those."),
    editor,
    report,
  );
  await load();
  panel.replaceChildren(wrap);
}

// ---- agents ----

export async function renderAgents(panel, socket) {
  const wrap = section("Agent fleet");
  const detail = el("div");
  const draw = (agents) => {
    const rows = agents.map((agent) => [
      agent.agent_id,
      agent.harness,
      agent.provider,
      agent.knowledge_graph ? `graph:${agent.graph_name}` : "—",
      agent.worker_status?.status ?? "—",
      (() => {
        const cell = el("div", "dash-actions");
        cell.append(
          button("status", async () => {
            detail.replaceChildren(el("div", "label", `[ ${agent.agent_id} status ]`));
            try {
              detail.append(jsonBlock(await api(`/api/agents/${agent.agent_id}/status`)));
            } catch (error) {
              detail.append(el("div", "status-bad", String(error)));
            }
          }),
          button("spec", () => editSpec(detail, agent.agent_id)),
          button("stop", async () => {
            if (!confirm(`Stop ${agent.agent_id}?`)) return;
            try {
              await api(`/api/agents/${agent.agent_id}/stop`, { method: "POST" });
              detail.replaceChildren(el("div", "status-ok", `[ stopped ${agent.agent_id} ]`));
            } catch (error) {
              detail.replaceChildren(el("div", "status-bad", String(error)));
            }
          }, true),
        );
        return cell;
      })(),
    ]);
    wrap.replaceChildren(
      el("div", "label dash-title", "Agent fleet"),
      table(["agent", "harness", "provider", "graph", "worker", "actions"], rows),
    );
  };
  socket.on("dash_update", (msg) => {
    if (msg.topic === "agents" && panel.contains(wrap)) draw(msg.data);
  });
  draw(await api("/api/agents"));
  panel.replaceChildren(wrap, detail);
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

// ---- runtime ----

export async function renderRuntime(panel, socket) {
  const wrap = section("Runtime plane");
  const draw = (up) => {
    const processes = (up.processes ?? []).map((p) => [
      p.name,
      String(p.pid),
      p.critical ? "critical" : "—",
      String(p.restarts),
      `${p.uptime_seconds}s`,
    ]);
    wrap.replaceChildren(
      el("div", "label dash-title", "Runtime plane"),
      el("div", up.running !== false ? "status-ok" : "status-bad",
        up.running !== false ? `[ up · supervisor pid ${up.pid ?? "?"} ]` : "[ maturana up is not running ]"),
      table(["process", "pid", "critical", "restarts", "uptime"], processes),
    );
  };
  socket.on("dash_update", (msg) => {
    if (msg.topic === "runtime" && panel.contains(wrap)) draw(msg.data);
  });
  draw(await api("/api/runtime/up"));

  const health = section("Services");
  const plan = await api("/api/runtime/plan");
  health.append(jsonBlock(plan));

  const doctor = section("Doctor");
  const out = el("div");
  doctor.append(
    button("run doctor", async () => {
      out.replaceChildren(el("div", "label", "[ running… ]"));
      try {
        out.replaceChildren(jsonBlock(await api("/api/doctor")));
      } catch (error) {
        out.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    out,
  );

  // Ops: plane lifecycle + config backup.
  const ops = section("Ops");
  const opsOut = el("div");
  const gw = (action, danger) => button(`${action} plane`, async () => {
    if (action !== "restart" && !confirm(`${action} the supervised plane?`)) return;
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
    gw("restart", false),
    gw("stop", true),
    gw("start", false),
    button("backup config", async () => {
      opsOut.replaceChildren(el("div", "label", "[ backing up… ]"));
      try {
        const r = await api("/api/ops/backup", { method: "POST" });
        opsOut.replaceChildren(el("div", "status-ok", `[ backup → ${r.path} (${r.bytes} bytes) ]`));
      } catch (error) {
        opsOut.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
  );
  ops.append(opsRow, opsOut);

  panel.replaceChildren(wrap, health, doctor, ops);
}

// ---- sessions ----

export async function renderSessions(panel, socket) {
  const wrap = section("Sessions");
  const detail = el("div");
  const listBox = el("div");

  const draw = (sessions) => {
    const rows = sessions.map((s) => [
      s.agent_id,
      labelCell(detail, s),
      s.last_active ? new Date(s.last_active).toLocaleString() : "—",
      `${s.stats?.completed ?? 0}✓ / ${s.stats?.pending ?? 0}⏳`,
      (() => {
        const cell = el("div", "dash-actions");
        cell.append(
          button("open", () => openSession(detail, socket, s.agent_id, s.session_id)),
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
        button("open", () => openSession(detail, socket, h.agent_id, h.session_id)),
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
      if (!confirm(`Delete sessions with no activity in ${days} days? This cannot be undone.`)) return;
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

function labelCell(detail, s) {
  const cell = el("span");
  cell.append(document.createTextNode((s.label ? `${s.label} ` : "") + s.session_id + " "));
  cell.append(button("rename", async () => {
    const label = prompt("Session label (empty to clear):", s.label || "");
    if (label === null) return;
    try {
      await api(`/api/sessions/${s.agent_id}/${s.session_id}/label`, { method: "PUT", body: JSON.stringify({ label }) });
      detail.replaceChildren(el("div", "status-ok", `[ relabeled ${s.session_id} ]`));
    } catch (error) {
      detail.replaceChildren(el("div", "status-bad", String(error)));
    }
  }));
  return cell;
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
    alert(String(error));
  }
}

async function openSession(detail, socket, agentId, sessionId) {
  detail.replaceChildren(el("div", "label dash-title", `[ ${agentId} / ${sessionId} ]`));
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
  await refresh();

  const composer = el("div", "dash-actions");
  const input = el("input", "model-input");
  input.placeholder = "message the agent…";
  input.style.flex = "1";
  const send = () => {
    if (!input.value.trim()) return;
    socket.send({
      type: "session_send",
      agent_id: agentId,
      session_id: sessionId,
      text: input.value.trim(),
    });
    input.value = "";
    setTimeout(refresh, 400);
  };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") send(); });
  composer.append(input, button("send", send));
  detail.append(log, composer);
}

// ---- graph ----

export async function renderGraph(panel) {
  const wrap = section("Knowledge graph");
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
  const file = el("input");
  file.type = "file";
  uploadRow.append(
    file,
    button("ingest", async () => {
      const picked = file.files?.[0];
      if (!picked) return;
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
  const wrap = section("Pipelock secrets (names only)");
  const out = el("div");
  const draw = async () => {
    const data = await api("/api/pipelock/secrets");
    const rows = data.names.map((name) => [
      name,
      button("delete", async () => {
        if (!confirm(`Delete secret ${name}?`)) return;
        await api(`/api/pipelock/secrets/${encodeURIComponent(name)}`, { method: "DELETE" });
        draw();
      }, true),
    ]);
    out.replaceChildren(table(["name", ""], rows));
  };
  await draw();

  const add = el("div", "dash-actions");
  const name = el("input", "model-input");
  name.placeholder = "name (e.g. brave/api-key)";
  const value = el("input", "model-input");
  value.type = "password";
  value.placeholder = "value";
  add.append(name, value, button("set", async () => {
    if (!name.value.trim() || !value.value) return;
    await api("/api/pipelock/secrets", {
      method: "POST",
      body: JSON.stringify({ name: name.value.trim(), value: value.value }),
    });
    value.value = "";
    draw();
  }));

  const egress = section("Egress allowlist editor");
  const agentPick = el("input", "model-input");
  agentPick.placeholder = "agent id";
  const hosts = el("textarea", "spec-editor");
  hosts.placeholder = "one host per line";
  hosts.style.minHeight = "90px";
  const egressOut = el("div");
  const egressRow = el("div", "dash-actions");
  egressRow.append(
    agentPick,
    button("load", async () => {
      try {
        const data = await api(`/api/agents/${agentPick.value.trim()}/egress`);
        hosts.value = data.egress_allowlist.join("\n");
        egressOut.replaceChildren(jsonBlock(data.inject_headers));
      } catch (error) {
        egressOut.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
    button("save", async () => {
      try {
        const data = await api(`/api/agents/${agentPick.value.trim()}/egress`, {
          method: "PUT",
          body: JSON.stringify({
            egress_allowlist: hosts.value.split("\n").map((h) => h.trim()).filter(Boolean),
          }),
        });
        egressOut.replaceChildren(el("div", "status-ok", "[ saved + validated ]"), jsonBlock(data.report));
      } catch (error) {
        egressOut.replaceChildren(el("div", "status-bad", String(error)));
      }
    }),
  );
  egress.append(egressRow, hosts, egressOut);

  wrap.append(add);
  panel.replaceChildren(wrap, out, egress);
}

// ---- tools / skills ----

export async function renderTools(panel) {
  const wrap = section("WASM tools");
  const tools = await api("/api/tools");
  const rows = (tools ?? []).map((t) => [
    t.name,
    t.version,
    t.description,
    JSON.stringify(t.capabilities ?? {}),
  ]);
  wrap.append(
    rows.length
      ? table(["name", "version", "description", "capabilities"], rows)
      : el("div", "label", "[ no tools registered — maturana tool register ]"),
  );
  panel.replaceChildren(wrap);
}

// ---- egress (live governance) ----

export async function renderEgress(panel, socket) {
  const wrap = section("Egress feed (live)");
  const note = el("div", "label", "[ proxy audit — allowed/denied egress as it happens ]");
  const feed = el("div", "session-log");
  feed.style.maxHeight = "70vh";
  wrap.append(note, feed);
  panel.replaceChildren(wrap);

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
  const wrap = section("Skill catalog");
  const detail = el("div", "turn-output");
  const skills = await api("/api/skills");
  const rows = skills.map((s) => [
    s.name,
    s.summary,
    button("view", async () => {
      const data = await api(`/api/skills/${s.name}`);
      detail.innerHTML = marked.parse(data.markdown);
    }),
  ]);
  wrap.append(table(["skill", "use when", ""], rows));
  panel.replaceChildren(wrap, detail);
}
