// Agent chat: a Hermes-style 3-pane chat over the live session WebSocket.
// Left = conversations (one per agent's live session), center = the message
// thread (markdown, timestamps, copy), bottom = a composer with Enter-to-send,
// slash commands, and a model/worker readout. Talking to an agent here routes
// through the SAME shared front door as Telegram/Discord (channels::enqueue_turn),
// so it gets the agent's memory + model — and replies stream back over the WS.

import { marked } from "/assets/vendor/marked/marked.esm.js";

async function api(path, options = {}) {
  const headers = { ...(options.headers ?? {}) };
  if (options.method && options.method !== "GET") {
    headers["x-maturana-web"] = "1";
    if (options.body && typeof options.body === "string") headers["content-type"] = "application/json";
  }
  const res = await fetch(path, { ...options, headers });
  const payload = await res.json().catch(() => ({ ok: false, error: "bad json" }));
  if (!payload.ok) throw new Error(payload.error ?? `http ${res.status}`);
  return payload.data;
}

function elem(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

// Sanitize marked output: strip scripts/iframes/event handlers, then insert.
function renderMarkdownInto(node, text) {
  const html = marked.parse(text ?? "", { breaks: true });
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll("script, iframe, object, embed, form, link, style").forEach((e) => e.remove());
  doc.querySelectorAll("*").forEach((e) => {
    for (const attr of [...e.attributes]) {
      if (/^on/i.test(attr.name) || (attr.name === "href" && /^javascript:/i.test(attr.value))) {
        e.removeAttribute(attr.name);
      }
    }
  });
  // Add a copy button to each code block.
  doc.querySelectorAll("pre").forEach((pre) => {
    const btn = doc.createElement("button");
    btn.className = "code-copy";
    btn.textContent = "copy";
    btn.dataset.copy = pre.textContent;
    pre.appendChild(btn);
  });
  node.replaceChildren(...doc.body.childNodes);
}

function fmtTime(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return isNaN(d) ? "" : d.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function textFromContent(content) {
  if (content == null) return "";
  if (typeof content !== "string") return JSON.stringify(content);
  try {
    const v = JSON.parse(content);
    if (v && typeof v === "object" && typeof v.text === "string") return v.text;
    if (typeof v === "string") return v;
  } catch {}
  return content;
}

const SLASH_HINTS = [
  "/model", "/reasoning", "/status", "/help", "/new", "/clear",
  "/stop", "/compact", "/skill", "/emerge", "/onboard", "/good", "/bad",
];

export class Chat {
  constructor(socket) {
    this.socket = socket;
    this.agents = [];
    this.convos = [];
    this.current = null; // { agent_id, session_id }
    this.seen = new Set();
    socket.on("session_outbound", (msg) => this.onOutbound(msg));
  }

  async mount(panel, agentId) {
    panel.replaceChildren();
    const wrap = elem("div", "chat");

    // ---- left: conversation list ----
    const side = elem("div", "chat-side");
    const sideHead = elem("div", "chat-side-head");
    const newBtn = elem("button", "primary", "＋ New chat");
    newBtn.addEventListener("click", () => this.newChat());
    sideHead.append(elem("div", "chat-side-title", "Conversations"), newBtn);
    this.search = elem("input", "model-input");
    this.search.placeholder = "filter…";
    this.search.addEventListener("input", () => this.drawList());
    this.listEl = elem("div", "chat-convos");
    side.append(sideHead, this.search, this.listEl);

    // ---- center: thread + composer ----
    const main = elem("div", "chat-main");
    this.headerEl = elem("div", "chat-header");
    this.threadEl = elem("div", "chat-thread");
    this.threadEl.addEventListener("click", (e) => {
      const b = e.target.closest(".code-copy");
      if (b) { navigator.clipboard?.writeText(b.dataset.copy || ""); b.textContent = "copied"; setTimeout(() => (b.textContent = "copy"), 1200); }
    });

    const composer = elem("div", "chat-composer");
    this.input = elem("textarea", "chat-input");
    this.input.placeholder = "Message the agent…  (Enter to send · Shift+Enter newline · / for commands)";
    this.input.rows = 1;
    this.input.addEventListener("input", () => this.autoGrow());
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); this.send(); }
    });
    const sendBtn = elem("button", "chat-send", "➤");
    sendBtn.title = "Send (Enter)";
    sendBtn.addEventListener("click", () => this.send());
    this.hint = elem("div", "chat-slash-hint");
    composer.append(this.hint, this.input, sendBtn);

    main.append(this.headerEl, this.threadEl, composer);

    // ---- right: agent files (host-side, read-only) ----
    this.filesEl = elem("div", "chat-files");
    const filesHead = elem("div", "chat-files-head");
    filesHead.append(elem("div", "chat-side-title", "Agent files"));
    this.filesList = elem("div", "chat-files-list");
    this.filesEl.append(filesHead, this.filesList);

    wrap.append(side, main, this.filesEl);
    panel.append(wrap);

    await this.refreshList();
    if (agentId) {
      // Opened for a specific agent (e.g. the Agents/Sessions "message" action):
      // jump straight into its conversation, starting its main session if none.
      const existing = this.convos.find((c) => c.agent_id === agentId);
      this.open(existing || { agent_id: agentId, session_id: `${agentId}-main`, fresh: true });
    } else if (this.convos.length) {
      this.open(this.convos[0]);
    } else {
      this.headerEl.replaceChildren(elem("div", "chat-empty", "No conversations yet — pick ＋ New chat."));
    }
  }

  async refreshList() {
    [this.agents, this.convos] = await Promise.all([
      api("/api/agents").catch(() => []),
      api("/api/sessions").catch(() => []),
    ]);
    // Newest first.
    this.convos.sort((a, b) => String(b.last_active ?? "").localeCompare(String(a.last_active ?? "")));
    this.drawList();
  }

  drawList() {
    const q = (this.search?.value || "").toLowerCase();
    this.listEl.replaceChildren();
    for (const c of this.convos) {
      const title = c.label || c.agent_id;
      if (q && !`${c.agent_id} ${title} ${c.session_id}`.toLowerCase().includes(q)) continue;
      const row = elem("div", "chat-convo");
      if (this.current && c.agent_id === this.current.agent_id && c.session_id === this.current.session_id) {
        row.classList.add("active");
      }
      row.append(
        elem("div", "chat-convo-title", title),
        elem("div", "chat-convo-sub", `${c.agent_id} · ${fmtTime(c.last_active) || "no activity"}`),
      );
      row.addEventListener("click", () => this.open(c));
      this.listEl.append(row);
    }
  }

  async newChat() {
    const ids = this.agents.map((a) => a.agent_id);
    if (!ids.length) { alert("No agents available."); return; }
    const pick = ids.length === 1 ? ids[0] : prompt(`Chat with which agent?\n${ids.join(", ")}`, ids[0]);
    if (!pick || !ids.includes(pick)) return;
    // Route to the agent's existing conversation (its worker answers one session);
    // if none exists yet, start its canonical "<id>-main".
    const existing = this.convos.find((c) => c.agent_id === pick);
    this.open(existing || { agent_id: pick, session_id: `${pick}-main`, fresh: true });
  }

  async open(convo) {
    this.current = { agent_id: convo.agent_id, session_id: convo.session_id };
    this.seen = new Set();
    this.drawList();
    const agent = this.agents.find((a) => a.agent_id === convo.agent_id);
    const worker = agent?.worker_status?.status ?? "—";
    const model = agent?.model ? ` · ${agent.model}` : "";
    this.headerEl.replaceChildren(
      elem("div", "chat-title", convo.label || convo.agent_id),
      elem("div", "chat-subtitle", `${convo.session_id}${model} · worker ${worker}`),
    );
    this.loadFiles(convo.agent_id);
    this.threadEl.replaceChildren(elem("div", "chat-empty", "loading…"));
    if (convo.fresh) { this.threadEl.replaceChildren(elem("div", "chat-empty", "Say hi to start the conversation.")); this.input.focus(); return; }
    try {
      const data = await api(`/api/sessions/${convo.agent_id}/${convo.session_id}/messages?limit=200`);
      const merged = [
        ...data.inbound.map((m) => ({ ...m, dir: "user" })),
        ...data.outbound.map((m) => ({ ...m, dir: "agent" })),
      ].sort((a, b) => String(a.created_at).localeCompare(String(b.created_at)));
      this.threadEl.replaceChildren();
      if (!merged.length) this.threadEl.append(elem("div", "chat-empty", "No messages yet."));
      for (const m of merged) this.appendMessage(m.dir, textFromContent(m.content), m.created_at, m.id);
      this.scrollDown();
    } catch (e) {
      this.threadEl.replaceChildren(elem("div", "status-bad", String(e)));
    }
    this.input.focus();
  }

  appendMessage(dir, text, at, id) {
    if (id) { if (this.seen.has(id)) return; this.seen.add(id); }
    const row = elem("div", `chat-msg ${dir}`);
    const bubble = elem("div", "chat-bubble");
    if (dir === "agent") renderMarkdownInto(bubble, text);
    else bubble.textContent = text;
    const meta = elem("div", "chat-meta", `${dir === "user" ? "you" : (this.current?.agent_id ?? "agent")} · ${fmtTime(at) || "now"}`);
    row.append(bubble, meta);
    const empty = this.threadEl.querySelector(".chat-empty");
    if (empty) empty.remove();
    this.threadEl.append(row);
  }

  scrollDown() { this.threadEl.scrollTop = this.threadEl.scrollHeight; }

  // Right-rail: the agent's host-side files (read-only). The in-VM workspace is
  // isolated and not exposed; this shows the spec, AGENTS.md, worker status, etc.
  async loadFiles(agentId) {
    if (!this.filesList) return;
    this.filesList.replaceChildren(elem("div", "chat-convo-sub", "loading…"));
    try {
      const files = await api(`/api/agents/${agentId}/files`);
      const visible = files.filter((f) => !f.dir);
      this.filesList.replaceChildren();
      if (!visible.length) { this.filesList.append(elem("div", "chat-convo-sub", "no files")); return; }
      for (const f of visible) {
        const row = elem("div", "file-row", f.path);
        row.title = `${f.size} bytes`;
        row.addEventListener("click", () => this.viewFile(agentId, f.path));
        this.filesList.append(row);
      }
    } catch (e) {
      this.filesList.replaceChildren(elem("div", "status-bad", String(e)));
    }
  }

  async viewFile(agentId, path) {
    const overlay = elem("div", "file-overlay");
    const card = elem("div", "file-card");
    const head = elem("div", "file-card-head");
    const closeBtn = elem("button", "primary", "close");
    closeBtn.addEventListener("click", () => overlay.remove());
    head.append(elem("div", "chat-title", path), closeBtn);
    const body = elem("pre", "file-card-body", "loading…");
    card.append(head, body);
    overlay.append(card);
    overlay.addEventListener("click", (e) => { if (e.target === overlay) overlay.remove(); });
    document.body.append(overlay);
    try {
      const data = await api(`/api/agents/${agentId}/files/read?path=${encodeURIComponent(path)}`);
      body.textContent = data.text || "(empty)";
    } catch (e) {
      body.textContent = String(e);
    }
  }

  send() {
    const text = this.input.value.trim();
    if (!text || !this.current) return;
    this.appendMessage("user", text, new Date().toISOString(), null);
    this.scrollDown();
    this.socket.send({
      type: "session_send",
      agent_id: this.current.agent_id,
      session_id: this.current.session_id,
      text,
    });
    this.input.value = "";
    this.autoGrow();
    // A pending indicator until the reply streams back.
    this.pending = elem("div", "chat-msg agent pending");
    this.pending.append(elem("div", "chat-bubble", "…"));
    this.threadEl.append(this.pending);
    this.scrollDown();
  }

  onOutbound(msg) {
    if (!this.current || msg.agent_id !== this.current.agent_id || msg.session_id !== this.current.session_id) {
      // A reply for another conversation — bump its position on the next refresh.
      this.refreshList();
      return;
    }
    if (this.pending) { this.pending.remove(); this.pending = null; }
    const m = msg.message || {};
    // Outbound poller sends either {queued:id} (our echo) or a full message.
    if (m.queued) return;
    const text = textFromContent(m.content ?? m.text ?? "");
    // The agent's "say nothing" reply to a proactive/heartbeat check is internal
    // — never render it as a chat bubble.
    if (!text || text.trim() === "[[MATURANA_SILENT]]") return;
    this.appendMessage("agent", text, m.created_at, m.id);
    this.scrollDown();
  }

  autoGrow() {
    this.input.style.height = "auto";
    this.input.style.height = Math.min(this.input.scrollHeight, 160) + "px";
    const v = this.input.value.trimStart();
    if (v.startsWith("/")) {
      const matches = SLASH_HINTS.filter((s) => s.startsWith(v.split(/\s/)[0]));
      this.hint.textContent = matches.length ? `commands: ${matches.join("  ")}` : "";
    } else {
      this.hint.textContent = "";
    }
  }
}
