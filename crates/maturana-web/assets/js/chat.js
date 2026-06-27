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

// Slash commands offered by the composer's Tab-completion menu. Keep in sync
// with the shared dispatch_slash_command set so the web surface matches the
// other channels (Telegram/Discord/TUI).
const SLASH_COMMANDS = [
  { c: "/model", d: "switch the model" },
  { c: "/reasoning", d: "set reasoning effort (low/medium/high)" },
  { c: "/status", d: "agent + worker status" },
  { c: "/help", d: "list available commands" },
  { c: "/new", d: "start a fresh conversation" },
  { c: "/clear", d: "clear this session's history" },
  { c: "/stop", d: "abort the in-flight reply" },
  { c: "/compact", d: "summarize & shrink the context" },
  { c: "/skill", d: "run a skill by name" },
  { c: "/loop", d: "run a goal on a loop" },
  { c: "/emerge", d: "self-improvement pass" },
  { c: "/onboard", d: "(re)run onboarding" },
  { c: "/good", d: "mark the last reply good" },
  { c: "/bad", d: "mark the last reply bad" },
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
    this.input.addEventListener("input", () => { this.autoGrow(); this.updateSlashMenu(); });
    this.input.addEventListener("keydown", (e) => this.onComposerKey(e));
    this.input.addEventListener("blur", () => setTimeout(() => this.closeSlash(), 120));
    const sendBtn = elem("button", "chat-send", "➤");
    sendBtn.title = "Send (Enter)";
    sendBtn.addEventListener("click", () => this.send());
    // Tab-completion menu for slash commands (hidden until "/" is typed).
    this.slashMenu = elem("div", "slash-menu");
    this.slashMenu.hidden = true;
    this.slashItems = [];
    this.acIndex = 0;
    composer.append(this.slashMenu, this.input, sendBtn);

    main.append(this.headerEl, this.threadEl, composer);

    // ---- right: agent files (host-side; prose docs are editable) ----
    this.filesEl = elem("div", "chat-files");
    const filesHead = elem("div", "chat-files-head");
    const newFileBtn = elem("button", "file-new-btn", "＋");
    newFileBtn.title = "New document (e.g. SOUL.md)";
    newFileBtn.addEventListener("click", () => { const a = this.current?.agent_id; if (a) this.newFile(a); });
    filesHead.append(elem("div", "chat-side-title", "Agent files"), newFileBtn);
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

  async viewFile(agentId, path, opts = {}) {
    const create = !!opts.create;
    // The spec (MATURANA.md) and the machine-written status file are not edited
    // through this generic editor — the spec has its own validated flow.
    const editable = !/(^|\/)(MATURANA\.md|worker-status\.json)$/.test(path);

    const overlay = elem("div", "file-overlay");
    const card = elem("div", "file-card");
    const head = elem("div", "file-card-head");
    const actions = elem("div", "file-card-actions");
    const status = elem("span", "file-card-status");
    const saveBtn = elem("button", "primary", "Save");
    const closeBtn = elem("button", "primary", "Close");
    closeBtn.addEventListener("click", () => overlay.remove());
    if (editable) actions.append(status, saveBtn);
    actions.append(closeBtn);
    head.append(elem("div", "chat-title", path), actions);

    let body;
    if (editable) {
      body = elem("textarea", "file-edit");
      body.spellcheck = false;
      if (!create) { body.value = "loading…"; body.disabled = true; }
    } else {
      body = elem("pre", "file-card-body", "loading…");
    }
    card.append(head, body);
    overlay.append(card);
    overlay.addEventListener("click", (e) => { if (e.target === overlay) overlay.remove(); });
    document.body.append(overlay);

    if (!create) {
      try {
        const data = await api(`/api/agents/${agentId}/files/read?path=${encodeURIComponent(path)}`);
        if (editable) { body.value = data.text || ""; body.disabled = false; }
        else body.textContent = data.text || "(empty)";
      } catch (e) {
        if (editable) { body.value = ""; body.disabled = false; status.textContent = String(e); status.className = "file-card-status bad"; }
        else body.textContent = String(e);
      }
    }
    if (!editable) return;

    body.focus();
    const save = async () => {
      saveBtn.disabled = true;
      status.textContent = "saving…"; status.className = "file-card-status";
      try {
        const res = await api(`/api/agents/${agentId}/files/write`, {
          method: "POST",
          body: JSON.stringify({ path, text: body.value }),
        });
        status.textContent = `saved · ${res.size} bytes`; status.className = "file-card-status ok";
        this.loadFiles(agentId); // refresh sizes; a newly-created file appears in the list
      } catch (e) {
        status.textContent = `save failed: ${e}`; status.className = "file-card-status bad";
      } finally {
        saveBtn.disabled = false;
      }
    };
    saveBtn.addEventListener("click", save);
    body.addEventListener("keydown", (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") { e.preventDefault(); save(); }
    });
  }

  // Create a new host-side document (e.g. SOUL.md) — opens the editor empty; the
  // file is written on first Save (the backend confirms it's a safe doc path).
  newFile(agentId) {
    const name = (prompt("New document filename (e.g. SOUL.md):", "SOUL.md") || "").trim();
    if (!name) return;
    if (/[\\/]/.test(name)) { alert("Use a plain filename (no folders)."); return; }
    if (!/\.(md|txt|json|yaml|yml|toml|csv|log)$/i.test(name)) {
      alert("Use a .md / .txt / .json / .yaml / .toml / .csv / .log filename.");
      return;
    }
    this.viewFile(agentId, name, { create: true });
  }

  send() {
    const text = this.input.value.trim();
    if (!text || !this.current) return;
    // /clear and /new reset the conversation — wipe the visible thread right away;
    // the server resets context and streams back a confirmation into the cleared view.
    const isReset = /^\/(clear|new)\b/i.test(text);
    this.appendMessage("user", text, new Date().toISOString(), null);
    this.scrollDown();
    this.socket.send({
      type: "session_send",
      agent_id: this.current.agent_id,
      session_id: this.current.session_id,
      text,
    });
    this.input.value = "";
    this.closeSlash();
    this.autoGrow();
    if (isReset) {
      this.hideTyping();
      this.seen = new Set();
      this.threadEl.replaceChildren(elem("div", "chat-empty", "Conversation cleared."));
    } else {
      this.showTyping();
    }
  }

  // Animated "responding…" indicator — three pulsing dots in an agent bubble,
  // plus a breathing "responding" line in the header, until the reply streams
  // back (cleared in onOutbound).
  showTyping() {
    this.hideTyping();
    const row = elem("div", "chat-msg agent typing");
    const bubble = elem("div", "chat-bubble");
    for (let i = 0; i < 3; i++) bubble.append(elem("span", "chat-typing-dot"));
    row.append(bubble);
    this.threadEl.append(row);
    this.pending = row;
    if (this.headerEl) {
      this.respondingEl = elem("div", "chat-responding", "responding…");
      this.headerEl.append(this.respondingEl);
    }
    this.scrollDown();
  }

  hideTyping() {
    if (this.pending) { this.pending.remove(); this.pending = null; }
    if (this.respondingEl) { this.respondingEl.remove(); this.respondingEl = null; }
  }

  onOutbound(msg) {
    if (!this.current || msg.agent_id !== this.current.agent_id || msg.session_id !== this.current.session_id) {
      // A reply for another conversation — bump its position on the next refresh.
      this.refreshList();
      return;
    }
    this.hideTyping();
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
    this.input.style.height = Math.min(this.input.scrollHeight, 180) + "px";
  }

  // ---- slash-command autocomplete ----

  // The menu is live only while the user is typing the *command token*: the
  // text starts with "/" and has no space yet (once they type an argument the
  // command is chosen, so the menu gets out of the way).
  slashQuery() {
    const v = this.input.value;
    if (!v.startsWith("/") || /\s/.test(v)) return null;
    return v;
  }

  updateSlashMenu() {
    const q = this.slashQuery();
    if (q === null) return this.closeSlash();
    const matches = SLASH_COMMANDS.filter((s) => s.c.startsWith(q));
    if (!matches.length) return this.closeSlash();
    this.slashItems = matches;
    if (this.acIndex >= matches.length) this.acIndex = 0;
    this.renderSlashMenu();
    this.slashMenu.hidden = false;
  }

  renderSlashMenu() {
    this.slashMenu.replaceChildren();
    this.slashItems.forEach((s, i) => {
      const row = elem("div", "slash-row" + (i === this.acIndex ? " sel" : ""));
      row.append(elem("span", "cmd", s.c), elem("span", "desc", s.d));
      // mousedown (not click) so it fires before the input's blur handler.
      row.addEventListener("mousedown", (e) => { e.preventDefault(); this.acIndex = i; this.acceptSlash(); });
      this.slashMenu.append(row);
    });
    const foot = elem("div", "slash-menu-foot");
    foot.innerHTML = "<kbd>Tab</kbd> complete · <kbd>↑</kbd><kbd>↓</kbd> move · <kbd>Esc</kbd> dismiss";
    this.slashMenu.append(foot);
  }

  closeSlash() {
    if (this.slashMenu) this.slashMenu.hidden = true;
    this.slashItems = [];
    this.acIndex = 0;
  }

  slashOpen() {
    return this.slashMenu && !this.slashMenu.hidden && this.slashItems.length;
  }

  acceptSlash() {
    const pick = this.slashItems[this.acIndex];
    if (!pick) return;
    this.input.value = pick.c + " ";
    this.closeSlash();
    this.input.focus();
    this.autoGrow();
  }

  onComposerKey(e) {
    if (this.slashOpen()) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        this.acIndex = (this.acIndex + 1) % this.slashItems.length;
        this.renderSlashMenu();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        this.acIndex = (this.acIndex - 1 + this.slashItems.length) % this.slashItems.length;
        this.renderSlashMenu();
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        e.preventDefault();
        this.acceptSlash();
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        this.closeSlash();
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); this.send(); }
  }
}
