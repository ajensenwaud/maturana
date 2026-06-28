// Agent chat: a Hermes-style 3-pane chat over the live session WebSocket.
// Left = conversations (one per agent's live session), center = the message
// thread (markdown, timestamps, copy), bottom = a composer with Enter-to-send,
// slash commands, and a model/worker readout. Talking to an agent here routes
// through the SAME shared front door as Telegram/Discord (channels::enqueue_turn),
// so it gets the agent's memory + model — and replies stream back over the WS.

import { marked } from "/assets/vendor/marked/marked.esm.js";
import { formDialog, toast } from "/assets/js/ui.js";

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

// Humanize a session_id into the channel it represents, so the conversation
// list distinguishes the same agent's Telegram vs Web threads instead of
// showing the agent id twice.
function channelLabel(sessionId) {
  const s = String(sessionId || "");
  if (s.startsWith("telegram")) return "Telegram";
  if (s.startsWith("web")) return "Web";
  if (s.startsWith("discord")) return "Discord";
  if (s.startsWith("slack")) return "Slack";
  if (s.startsWith("agentmail")) return "Mail";
  if (s.startsWith("orch-")) return "Orchestrator";
  if (s.endsWith("-main")) return "Main";
  return s;
}

// A centered, styled picker dialog (replaces window.prompt for choosing one of a
// known set — new chat, /model). items: [{title, sub, value}].
function pickerDialog(title, sub, items, onPick) {
  const overlay = elem("div", "pick-overlay");
  const card = elem("div", "pick-card");
  const head = elem("div", "pick-head");
  head.append(elem("div", "pick-title", title));
  if (sub) head.append(elem("div", "pick-sub", sub));
  const list = elem("div", "pick-list");
  const close = () => { overlay.remove(); document.removeEventListener("keydown", onKey); };
  function onKey(e) { if (e.key === "Escape") close(); }
  for (const it of items) {
    const row = elem("button", "pick-row");
    row.append(elem("span", "pick-row-title", it.title));
    if (it.sub) row.append(elem("span", "pick-row-sub", it.sub));
    row.addEventListener("click", () => { close(); onPick(it.value); });
    list.append(row);
  }
  if (!items.length) list.append(elem("div", "pick-sub", "nothing to choose"));
  card.append(head, list);
  overlay.append(card);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  document.addEventListener("keydown", onKey);
  document.body.append(overlay);
}

// A small styled text-input dialog (replaces window.prompt for free text).
function promptDialog(title, sub, initial, onSubmit) {
  const overlay = elem("div", "pick-overlay");
  const card = elem("div", "pick-card");
  const head = elem("div", "pick-head");
  head.append(elem("div", "pick-title", title));
  if (sub) head.append(elem("div", "pick-sub", sub));
  const body = elem("div", "pick-prompt");
  const input = elem("input", "model-input");
  input.value = initial || "";
  input.placeholder = "name (leave empty to clear)";
  const actions = elem("div", "pick-actions");
  const save = elem("button", "primary", "Save");
  const cancel = elem("button", "primary ghost", "Cancel");
  const close = () => overlay.remove();
  save.addEventListener("click", () => { close(); onSubmit(input.value); });
  cancel.addEventListener("click", close);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { close(); onSubmit(input.value); }
    if (e.key === "Escape") close();
  });
  actions.append(save, cancel);
  body.append(input, actions);
  card.append(head, body);
  overlay.append(card);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  document.body.append(overlay);
  setTimeout(() => input.focus(), 30);
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

// Files an agent attached to a reply, from the {"text":…,"files":[…]} outbound
// convention (also accepts a parsed object with a `files` array).
function filesFromContent(content) {
  let v = content;
  if (typeof content === "string") { try { v = JSON.parse(content); } catch { return []; } }
  const files = v && typeof v === "object" ? v.files : null;
  return Array.isArray(files) ? files.filter((f) => typeof f === "string") : [];
}

function baseName(p) { return String(p || "").replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "file"; }

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

// Per-harness model quick-picks for the /model selector. The worker accepts any
// id via "/model <id>" (the Custom… row), these are the common ones — parity
// with the inline model keyboard on Telegram.
const MODEL_CHOICES = {
  codex: [{ title: "gpt-5.5", sub: "ChatGPT subscription", value: "gpt-5.5" }],
  "claude-code": [
    { title: "Opus", sub: "most capable", value: "opus" },
    { title: "Sonnet", sub: "balanced", value: "sonnet" },
    { title: "Haiku", sub: "fastest", value: "haiku" },
  ],
  opencode: [
    { title: "Claude Sonnet 4.5", sub: "anthropic/claude-sonnet-4.5", value: "anthropic/claude-sonnet-4.5" },
    { title: "GPT-5.5", sub: "openai/gpt-5.5", value: "openai/gpt-5.5" },
    { title: "Gemini 2.5 Pro", sub: "google/gemini-2.5-pro", value: "google/gemini-2.5-pro" },
    { title: "Llama 3.3 70B", sub: "meta-llama/llama-3.3-70b-instruct", value: "meta-llama/llama-3.3-70b-instruct" },
  ],
};

export class Chat {
  constructor(socket) {
    this.socket = socket;
    this.agents = [];
    this.convos = [];
    this.current = null; // { agent_id, session_id }
    this.seen = new Set();
    this.stream = null; // live in-flight reply: { agent_id, session_id, row, bubble, text, tool }
    socket.on("session_outbound", (msg) => this.onOutbound(msg));
    socket.on("session_progress", (msg) => this.onProgress(msg));
  }

  async mount(panel, agentId) {
    panel.replaceChildren();
    const wrap = elem("div", "chat");
    this.wrapEl = wrap;
    if (this.showFiles === undefined) this.showFiles = false;
    wrap.classList.toggle("show-files", this.showFiles);

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
    // Attach a file (uploaded to the agent + ingested into its graph, like Telegram).
    this.attachments = [];
    this.fileInput = elem("input", "chat-file-hidden");
    this.fileInput.type = "file";
    this.fileInput.addEventListener("change", () => {
      const f = this.fileInput.files?.[0];
      if (f) this.uploadAttachment(f);
      this.fileInput.value = "";
    });
    const attachBtn = elem("button", "chat-attach", "📎");
    attachBtn.title = "Attach a file";
    attachBtn.addEventListener("click", () => { if (this.current) this.fileInput.click(); });
    const sendBtn = elem("button", "chat-send", "➤");
    sendBtn.title = "Send (Enter)";
    sendBtn.addEventListener("click", () => this.send());
    // Tab-completion menu for slash commands (hidden until "/" is typed).
    this.slashMenu = elem("div", "slash-menu");
    this.slashMenu.hidden = true;
    this.slashItems = [];
    this.acIndex = 0;
    composer.append(this.slashMenu, attachBtn, this.input, sendBtn, this.fileInput);

    // A strip of pending upload chips, shown above the composer when present.
    this.attachStrip = elem("div", "chat-attach-strip");
    this.attachStrip.hidden = true;

    main.append(this.headerEl, this.threadEl, this.attachStrip, composer);

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
        elem("div", "chat-convo-sub", `${channelLabel(c.session_id)} · ${fmtTime(c.last_active) || "no activity"}`),
      );
      row.addEventListener("click", () => this.open(c));
      row.addEventListener("dblclick", () => this.renameConvo(c));
      this.listEl.append(row);
    }
  }

  async newChat() {
    if (!this.agents.length) {
      this.headerEl.replaceChildren(elem("div", "chat-empty", "No agents yet — launch one from the Agents panel."));
      return;
    }
    // Styled in-app picker over the known agents (no native prompt()).
    pickerDialog("New conversation", "Choose an agent to talk to", this.agents.map((a) => ({
      title: a.name || a.agent_id,
      sub: a.harness || a.provider || "",
      value: a.agent_id,
    })), (agentId) => {
      const existing = this.convos.find((c) => c.agent_id === agentId);
      this.open(existing || { agent_id: agentId, session_id: `${agentId}-main`, fresh: true });
    });
  }

  // Rename a conversation via the existing label endpoint (non-destructive: the
  // session_id stays the canonical key; an empty value clears the custom label).
  async renameConvo(convo) {
    const current = convo.label || "";
    promptDialog("Rename conversation", `${convo.agent_id} · ${channelLabel(convo.session_id)}`, current, async (label) => {
      try {
        await api(`/api/sessions/${convo.agent_id}/${convo.session_id}/label`, {
          method: "PUT",
          body: JSON.stringify({ label: label.trim() }),
        });
        convo.label = label.trim();
        const inMem = this.convos.find((c) => c.agent_id === convo.agent_id && c.session_id === convo.session_id);
        if (inMem) inMem.label = label.trim();
        this.drawList();
        if (this.current && this.current.agent_id === convo.agent_id && this.current.session_id === convo.session_id) {
          this.open(inMem || convo);
        }
      } catch (e) {
        toast(`Rename failed: ${e}`, "bad");
      }
    });
  }

  async open(convo) {
    // Drop any in-flight stream from the conversation we're leaving — its row is
    // about to be wiped by replaceChildren and its final reply will reload from
    // the DB when this thread is next opened.
    this.stream = null;
    this.clearResponding();
    this.current = { agent_id: convo.agent_id, session_id: convo.session_id };
    this.seen = new Set();
    this.drawList();
    const agent = this.agents.find((a) => a.agent_id === convo.agent_id);
    const worker = agent?.worker_status?.status ?? "—";
    const model = agent?.model ? ` · ${agent.model}` : "";
    const info = elem("div", "chat-header-info");
    const titleRow = elem("div", "chat-title-row");
    titleRow.append(elem("div", "chat-title", convo.label || convo.agent_id));
    const renameBtn = elem("button", "chat-rename", "✎");
    renameBtn.title = "Rename conversation";
    renameBtn.addEventListener("click", () => this.renameConvo(convo));
    titleRow.append(renameBtn);
    info.append(
      titleRow,
      elem("div", "chat-subtitle", `${channelLabel(convo.session_id)} · ${convo.session_id}${model} · worker ${worker}`),
    );
    const filesToggle = elem("button", "chat-files-toggle" + (this.showFiles ? " on" : ""), "⌗ Files");
    filesToggle.addEventListener("click", () => {
      this.showFiles = !this.showFiles;
      this.wrapEl.classList.toggle("show-files", this.showFiles);
      filesToggle.classList.toggle("on", this.showFiles);
      if (this.showFiles) this.loadFiles(convo.agent_id);
    });
    this.headerEl.replaceChildren(info, filesToggle);
    if (this.showFiles) this.loadFiles(convo.agent_id);
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
      for (const m of merged) this.appendMessage(m.dir, textFromContent(m.content), m.created_at, m.id, filesFromContent(m.content));
      this.scrollDown();
    } catch (e) {
      this.threadEl.replaceChildren(elem("div", "status-bad", String(e)));
    }
    this.input.focus();
  }

  appendMessage(dir, text, at, id, files) {
    if (id) { if (this.seen.has(id)) return; this.seen.add(id); }
    const row = elem("div", `chat-msg ${dir}`);
    const bubble = elem("div", "chat-bubble");
    if (dir === "agent") renderMarkdownInto(bubble, text);
    else bubble.textContent = text;
    row.append(bubble);
    if (files && files.length) row.append(this.downloadChips(files));
    const meta = elem("div", "chat-meta", `${dir === "user" ? "you" : (this.current?.agent_id ?? "agent")} · ${fmtTime(at) || "now"}`);
    row.append(meta);
    const empty = this.threadEl.querySelector(".chat-empty");
    if (empty) empty.remove();
    this.threadEl.append(row);
  }

  // Download chips for files an agent attached to a reply — a real <a download>
  // hitting the guarded download endpoint (the session cookie rides along).
  downloadChips(files) {
    const box = elem("div", "chat-downloads");
    const agent = this.current?.agent_id;
    for (const path of files) {
      const a = document.createElement("a");
      a.className = "chat-download";
      a.textContent = `⬇ ${baseName(path)}`;
      a.href = `/api/agents/${agent}/chat/download?path=${encodeURIComponent(path)}`;
      a.setAttribute("download", baseName(path));
      a.title = path;
      box.append(a);
    }
    return box;
  }

  // ---- composer file attachments (upload to the agent) ----

  async uploadAttachment(file) {
    const agent = this.current?.agent_id;
    if (!agent) return;
    const entry = { name: file.name, state: "uploading", error: null };
    this.attachments.push(entry);
    this.renderAttachStrip();
    try {
      const res = await fetch(`/api/agents/${agent}/chat/upload?name=${encodeURIComponent(file.name)}`, {
        method: "POST",
        headers: { "x-maturana-web": "1", "content-type": "application/octet-stream" },
        body: file,
      });
      const payload = await res.json().catch(() => ({ ok: false, error: "bad json" }));
      if (!payload.ok) throw new Error(payload.error ?? `http ${res.status}`);
      entry.state = "ready";
      entry.name = payload.data?.name || file.name;
      entry.ingested = payload.data?.ingested_chunks ?? null;
      entry.error = payload.data?.ingest_error ?? null;
    } catch (e) {
      entry.state = "error";
      entry.error = String(e);
    }
    this.renderAttachStrip();
  }

  removeAttachment(i) { this.attachments.splice(i, 1); this.renderAttachStrip(); }

  renderAttachStrip() {
    if (!this.attachStrip) return;
    this.attachStrip.replaceChildren();
    if (!this.attachments.length) { this.attachStrip.hidden = true; return; }
    this.attachStrip.hidden = false;
    this.attachments.forEach((a, i) => {
      const chip = elem("div", `attach-chip ${a.state}`);
      let label = a.name;
      if (a.state === "uploading") label = `${a.name} · uploading…`;
      else if (a.state === "error") label = `${a.name} · failed`;
      else if (a.ingested != null) label = `${a.name} · ${a.ingested} chunks`;
      else if (a.error) label = `${a.name} · attached (not ingested)`;
      chip.append(elem("span", "attach-name", label));
      if (a.error) chip.title = a.error;
      const x = elem("button", "attach-x", "×");
      x.addEventListener("click", () => this.removeAttachment(i));
      chip.append(x);
      this.attachStrip.append(chip);
    });
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
    formDialog({
      title: "New document",
      sub: "A host-side doc for this agent (e.g. SOUL.md). Written on first Save.",
      fields: [{ name: "name", label: "Filename", type: "text", value: "SOUL.md", required: true }],
      submitLabel: "Create",
      onSubmit: (v) => {
        const name = v.name.trim();
        if (/[\\/]/.test(name)) throw new Error("Use a plain filename (no folders).");
        if (!/\.(md|txt|json|yaml|yml|toml|csv|log)$/i.test(name)) {
          throw new Error("Use a .md / .txt / .json / .yaml / .toml / .csv / .log filename.");
        }
        this.viewFile(agentId, name, { create: true });
      },
    });
  }

  sendCommand(cmd) { this.input.value = cmd; this.send(); }

  openModelPicker() {
    const agent = this.agents.find((a) => a.agent_id === this.current?.agent_id);
    const harness = agent?.harness || "codex";
    const items = (MODEL_CHOICES[harness] || []).map((c) => ({ title: c.title, sub: c.sub, value: c.value }));
    items.push({ title: "Custom…", sub: "enter a model id", value: "__custom__" });
    pickerDialog(`Model · ${harness}`, "Switch the model for this agent", items, (val) => {
      if (val === "__custom__") {
        promptDialog("Custom model", "model id the harness accepts", "", (m) => { if (m.trim()) this.sendCommand(`/model ${m.trim()}`); });
        return;
      }
      this.sendCommand(`/model ${val}`);
    });
  }

  openReasoningPicker() {
    const items = [
      { title: "Low", sub: "fast, cheap", value: "low" },
      { title: "Medium", sub: "balanced", value: "medium" },
      { title: "High", sub: "deepest", value: "high" },
    ];
    pickerDialog("Reasoning effort", "Set the agent's reasoning effort", items, (val) => this.sendCommand(`/reasoning ${val}`));
  }

  send() {
    const text = this.input.value.trim();
    if (!this.current) return;
    // Only ready (uploaded) attachments ride along with this turn.
    const ready = (this.attachments || []).filter((a) => a.state === "ready");
    if (!text && !ready.length) return;
    // Bare selector commands open a styled picker (parity with the Telegram
    // inline keyboard) rather than sending the word as a message.
    if (/^\/model$/i.test(text)) { this.input.value = ""; this.closeSlash(); this.openModelPicker(); return; }
    if (/^\/reasoning$/i.test(text)) { this.input.value = ""; this.closeSlash(); this.openReasoningPicker(); return; }
    // /clear and /new reset the conversation — wipe the visible thread right away;
    // the server resets context and streams back a confirmation into the cleared view.
    const isReset = /^\/(clear|new)\b/i.test(text);
    // Tell the agent which files were attached (they're already in its graph).
    const note = ready.map((a) => `[attached file: ${a.name}]`).join("\n");
    const outgoing = [text, note].filter(Boolean).join("\n\n");
    const shown = [text, note].filter(Boolean).join("\n");
    this.appendMessage("user", shown, new Date().toISOString(), null);
    this.scrollDown();
    this.socket.send({
      type: "session_send",
      agent_id: this.current.agent_id,
      session_id: this.current.session_id,
      text: outgoing,
    });
    this.input.value = "";
    this.attachments = [];
    this.renderAttachStrip();
    this.closeSlash();
    this.autoGrow();
    if (isReset) {
      this.discardStream();
      this.seen = new Set();
      this.threadEl.replaceChildren(elem("div", "chat-empty", "Conversation cleared."));
    } else {
      this.beginStream();
    }
  }

  // ---- live streaming reply ----
  //
  // A turn opens a single agent bubble that updates in place: pulsing dots →
  // tool/activity lines → the answer text as it's generated (the worker's
  // progress side-lane, the same feed Telegram reads) → the authoritative final
  // reply. One bubble for the whole turn, so the indicator never flashes-and-
  // vanishes (the old bug: the {queued} echo used to clear the dots instantly).

  // Open the in-flight bubble with pulsing dots; progress fills it in.
  beginStream() {
    this.discardStream();
    const row = elem("div", "chat-msg agent typing");
    const bubble = elem("div", "chat-bubble");
    this.streamDots(bubble);
    row.append(bubble);
    const empty = this.threadEl.querySelector(".chat-empty");
    if (empty) empty.remove();
    this.threadEl.append(row);
    this.stream = {
      agent_id: this.current.agent_id,
      session_id: this.current.session_id,
      row, bubble, text: "", tool: "",
    };
    if (this.headerEl && !this.respondingEl) {
      this.respondingEl = elem("div", "chat-responding", "responding…");
      this.headerEl.append(this.respondingEl);
    }
    this.scrollDown();
  }

  streamDots(bubble) {
    bubble.replaceChildren();
    for (let i = 0; i < 3; i++) bubble.append(elem("span", "chat-typing-dot"));
  }

  // Re-render the in-flight bubble: answer text wins, else the latest tool/
  // activity line, else the pulsing dots.
  renderStream() {
    const s = this.stream;
    if (!s) return;
    if (s.text) { renderMarkdownInto(s.bubble, s.text); s.row.classList.remove("typing"); }
    else if (s.tool) { s.bubble.replaceChildren(elem("div", "chat-activity", s.tool)); s.row.classList.remove("typing"); }
    else { this.streamDots(s.bubble); s.row.classList.add("typing"); }
  }

  clearResponding() { if (this.respondingEl) { this.respondingEl.remove(); this.respondingEl = null; } }
  // Throw away the in-flight bubble entirely (reset / silent reply).
  discardStream() { if (this.stream) { this.stream.row.remove(); this.stream = null; } this.clearResponding(); }

  // Live progress for an in-flight turn (tool/thinking/cumulative answer text).
  onProgress(msg) {
    const s = this.stream;
    if (!s || msg.agent_id !== s.agent_id || msg.session_id !== s.session_id) return;
    // Only paint into the currently-open thread (the row belongs to it).
    if (!this.current || this.current.agent_id !== s.agent_id || this.current.session_id !== s.session_id) return;
    if (msg.kind === "text") {
      if (msg.text) { s.text = msg.text; this.renderStream(); this.scrollDown(); }
    } else if (msg.kind === "tool") {
      // Worker tool line is "key<US>detail" (codex; US = 0x1f) or a plain "using: x".
      const parts = String(msg.text || "").split("\u001f");
      s.tool = parts.length > 1 ? `${parts[0]} ${parts[1]}`.trim() : String(msg.text || "");
      this.renderStream(); this.scrollDown();
    } else if (msg.kind === "status") {
      if (msg.text === "error" && !s.text) { s.text = "⚠ the agent hit an error."; this.renderStream(); }
      // Turn is terminal; stop the "responding…" pulse. Keep the streamed text —
      // the authoritative final reply replaces it via onOutbound shortly.
      this.clearResponding();
    }
    // "thinking" is intentionally not shown (parity with Telegram).
  }

  onOutbound(msg) {
    if (!this.current || msg.agent_id !== this.current.agent_id || msg.session_id !== this.current.session_id) {
      // A reply for another conversation — bump its position on the next refresh.
      this.refreshList();
      return;
    }
    const m = msg.message || {};
    // Outbound poller sends either {queued:id} (our send echo) or a full message.
    // The echo must NOT touch the in-flight bubble (that was the vanishing-dots bug).
    if (m.queued) return;
    const text = textFromContent(m.content ?? m.text ?? "");
    const files = filesFromContent(m.content ?? m.text ?? "");
    const silent = (!text || text.trim() === "[[MATURANA_SILENT]]") && !files.length;
    const s = this.stream;
    if (s && s.agent_id === msg.agent_id && s.session_id === msg.session_id) {
      // Finalize the streaming bubble in place with the authoritative text.
      if (silent) { this.discardStream(); return; }
      if (m.id) this.seen.add(m.id);
      renderMarkdownInto(s.bubble, text);
      s.row.classList.remove("typing");
      if (files.length) s.row.append(this.downloadChips(files));
      s.row.append(elem("div", "chat-meta", `${this.current?.agent_id ?? "agent"} · ${fmtTime(m.created_at) || "now"}`));
      this.stream = null;
      this.clearResponding();
      this.scrollDown();
      return;
    }
    // No in-flight bubble (e.g. a reply arriving in an open Telegram thread).
    if (silent) return;
    this.appendMessage("agent", text, m.created_at, m.id, files);
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
    let selRow = null;
    this.slashItems.forEach((s, i) => {
      const row = elem("div", "slash-row" + (i === this.acIndex ? " sel" : ""));
      if (i === this.acIndex) selRow = row;
      row.append(elem("span", "cmd", s.c), elem("span", "desc", s.d));
      // mousedown (not click) so it fires before the input's blur handler.
      row.addEventListener("mousedown", (e) => { e.preventDefault(); this.acIndex = i; this.acceptSlash(); });
      this.slashMenu.append(row);
    });
    const foot = elem("div", "slash-menu-foot");
    foot.innerHTML = "<kbd>Tab</kbd> complete · <kbd>↑</kbd><kbd>↓</kbd> move · <kbd>Esc</kbd> dismiss";
    this.slashMenu.append(foot);
    // Keep the highlighted row visible as arrow-keys move past the scroll window.
    if (selRow) selRow.scrollIntoView({ block: "nearest" });
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
