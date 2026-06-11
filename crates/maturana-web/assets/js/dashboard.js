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
  panel.replaceChildren(wrap, health, doctor);
}

// ---- sessions ----

export async function renderSessions(panel, socket) {
  const wrap = section("Sessions");
  const detail = el("div");
  const sessions = await api("/api/sessions");
  const rows = sessions.map((s) => [
    s.agent_id,
    s.session_id,
    JSON.stringify(s.stats ?? {}),
    button("open", () => openSession(detail, socket, s.agent_id, s.session_id)),
  ]);
  wrap.append(table(["agent", "session", "queue", ""], rows));
  panel.replaceChildren(wrap, detail);
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
