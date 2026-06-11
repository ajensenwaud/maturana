// Prompt console: CodeMirror editor (markdown + Vim toggle), turn timeline
// with streaming markdown output and phase-card animations.

import {
  EditorView,
  basicSetup,
  Compartment,
  keymap,
  markdown,
  vim,
} from "/assets/vendor/codemirror/codemirror.bundle.js";
import { marked } from "/assets/vendor/marked/marked.esm.js";
import { PhaseCards } from "/assets/js/anim.js";

let counter = 0;
const newTurnId = () => `turn-${Date.now()}-${++counter}`;

// Model output is rendered as markdown; strip anything executable before it
// touches the DOM (the model is not a trusted author).
function sanitizeInto(target, markdownText) {
  const html = marked.parse(markdownText);
  const doc = new DOMParser().parseFromString(html, "text/html");
  for (const bad of doc.querySelectorAll("script, iframe, object, embed, form")) {
    bad.remove();
  }
  for (const el of doc.body.querySelectorAll("*")) {
    for (const attr of [...el.attributes]) {
      if (attr.name.startsWith("on")) el.removeAttribute(attr.name);
      if (
        (attr.name === "href" || attr.name === "src") &&
        attr.value.trim().toLowerCase().startsWith("javascript:")
      ) {
        el.removeAttribute(attr.name);
      }
    }
  }
  target.replaceChildren(...doc.body.childNodes);
}

export class Console {
  constructor(socket) {
    this.socket = socket;
    this.turns = new Map(); // turn_id -> {output, outputEl, cards, footerEl, el}
    this.activeTurnId = null;
    this.vimCompartment = new Compartment();
    this.el = this.build();

    socket.on("turn_started", (msg) => this.onStarted(msg));
    socket.on("turn_delta", (msg) => this.onDelta(msg));
    socket.on("turn_phase", (msg) => this.onPhase(msg));
    socket.on("turn_completed", (msg) => this.onCompleted(msg));
    socket.on("error", (msg) => this.onError(msg));
  }

  build() {
    const root = document.createElement("div");
    root.className = "console";

    this.timeline = document.createElement("div");
    this.timeline.className = "timeline";

    const composer = document.createElement("div");
    composer.className = "composer";

    const toolbar = document.createElement("div");
    toolbar.className = "composer-bar";

    this.harnessSelect = document.createElement("select");
    for (const [value, label] of [
      ["codex", "codex (subscription)"],
      ["openrouter", "openrouter"],
    ]) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = label;
      this.harnessSelect.append(option);
    }

    this.modelInput = document.createElement("input");
    this.modelInput.placeholder = "model (openrouter)";
    this.modelInput.className = "model-input";
    this.modelInput.style.display = "none";
    this.harnessSelect.addEventListener("change", () => {
      this.modelInput.style.display =
        this.harnessSelect.value === "openrouter" ? "" : "none";
    });

    const vimToggle = document.createElement("label");
    vimToggle.className = "label vim-toggle";
    this.vimCheckbox = document.createElement("input");
    this.vimCheckbox.type = "checkbox";
    this.vimCheckbox.checked = localStorage.getItem("maturana.vim") === "1";
    this.vimCheckbox.addEventListener("change", () => {
      localStorage.setItem("maturana.vim", this.vimCheckbox.checked ? "1" : "0");
      this.editor.dispatch({
        effects: this.vimCompartment.reconfigure(
          this.vimCheckbox.checked ? vim() : [],
        ),
      });
    });
    vimToggle.append(this.vimCheckbox, document.createTextNode(" vim"));

    this.sendButton = document.createElement("button");
    this.sendButton.className = "primary";
    this.sendButton.textContent = "Run · ctrl+↵";
    this.sendButton.addEventListener("click", () => this.submit());

    this.cancelButton = document.createElement("button");
    this.cancelButton.className = "primary danger";
    this.cancelButton.textContent = "Cancel";
    this.cancelButton.style.display = "none";
    this.cancelButton.addEventListener("click", () => this.cancel());

    toolbar.append(
      this.harnessSelect,
      this.modelInput,
      vimToggle,
      this.sendButton,
      this.cancelButton,
    );

    const editorHost = document.createElement("div");
    editorHost.className = "editor-host";

    // vim() must precede basicSetup so its keymap wins.
    this.editor = new EditorView({
      parent: editorHost,
      extensions: [
        this.vimCompartment.of(this.vimCheckbox.checked ? vim() : []),
        basicSetup,
        markdown(),
        keymap.of([
          {
            key: "Ctrl-Enter",
            mac: "Cmd-Enter",
            run: () => {
              this.submit();
              return true;
            },
          },
        ]),
        EditorView.theme(
          {
            "&": { backgroundColor: "var(--bg-inset)", color: "var(--ink)" },
            ".cm-content": { fontFamily: "var(--mono)", caretColor: "var(--accent)" },
            ".cm-cursor": { borderLeftColor: "var(--accent)" },
            ".cm-gutters": {
              backgroundColor: "var(--bg-inset)",
              color: "var(--dim)",
              border: "none",
            },
            "&.cm-focused .cm-selectionBackground, ::selection": {
              backgroundColor: "rgba(29, 180, 176, 0.25)",
            },
          },
          { dark: true },
        ),
      ],
    });

    composer.append(editorHost, toolbar);
    root.append(this.timeline, composer);
    return root;
  }

  mount(panel) {
    panel.replaceChildren(this.el);
    this.editor.focus();
  }

  submit() {
    const text = this.editor.state.doc.toString().trim();
    if (!text || this.activeTurnId) return;
    const turnId = newTurnId();
    const harness = this.harnessSelect.value;
    const model =
      harness === "openrouter" && this.modelInput.value.trim()
        ? this.modelInput.value.trim()
        : null;

    const el = document.createElement("section");
    el.className = "turn";
    const promptEl = document.createElement("div");
    promptEl.className = "turn-prompt";
    sanitizeInto(promptEl, text);
    const cardsEl = document.createElement("div");
    cardsEl.className = "turn-cards";
    const outputEl = document.createElement("div");
    outputEl.className = "turn-output";
    const footerEl = document.createElement("div");
    footerEl.className = "turn-footer label";
    footerEl.textContent = "[ queued ]";
    el.append(promptEl, cardsEl, outputEl, footerEl);
    this.timeline.append(el);
    el.scrollIntoView({ block: "end" });

    this.turns.set(turnId, {
      el,
      outputEl,
      footerEl,
      output: "",
      cards: new PhaseCards(cardsEl),
    });
    this.activeTurnId = turnId;
    this.setBusy(true);

    this.socket.send({
      type: "prompt_submit",
      turn_id: turnId,
      harness,
      model,
      text,
    });
    this.editor.dispatch({
      changes: { from: 0, to: this.editor.state.doc.length, insert: "" },
    });
  }

  cancel() {
    if (this.activeTurnId) {
      this.socket.send({ type: "prompt_cancel", turn_id: this.activeTurnId });
    }
  }

  setBusy(busy) {
    this.sendButton.disabled = busy;
    this.cancelButton.style.display = busy ? "" : "none";
  }

  turn(turnId) {
    return this.turns.get(turnId);
  }

  onStarted({ turn_id }) {
    const turn = this.turn(turn_id);
    if (turn) turn.footerEl.textContent = "[ running ]";
  }

  onDelta({ turn_id, text }) {
    const turn = this.turn(turn_id);
    if (!turn) return;
    turn.output += text;
    sanitizeInto(turn.outputEl, turn.output);
    turn.el.scrollIntoView({ block: "end" });
  }

  onPhase({ turn_id, span_id, phase }) {
    const turn = this.turn(turn_id);
    if (turn) turn.cards.apply(span_id, phase);
  }

  onCompleted({ turn_id, ok, detail }) {
    const turn = this.turn(turn_id);
    if (turn) {
      turn.cards.settleAll(ok);
      const stamp = ok ? "done" : "failed";
      turn.footerEl.textContent = detail ? `[ ${stamp} · ${detail} ]` : `[ ${stamp} ]`;
      turn.footerEl.classList.add(ok ? "status-ok" : "status-bad");
    }
    if (this.activeTurnId === turn_id) {
      this.activeTurnId = null;
      this.setBusy(false);
    }
  }

  onError({ code, message, turn_id }) {
    if (!turn_id) return;
    const turn = this.turn(turn_id);
    if (turn) {
      turn.footerEl.textContent = `[ error · ${code}: ${message} ]`;
      turn.footerEl.classList.add("status-bad");
    }
    if (this.activeTurnId === turn_id) {
      this.activeTurnId = null;
      this.setBusy(false);
    }
  }
}
