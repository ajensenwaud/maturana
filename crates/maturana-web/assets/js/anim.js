// OpenClaw-style phase cards: one card per span (tool/skill execution),
// braille spinner while running — the same 8 frames the Telegram bridge
// renders via core's animation.rs — and a swipe-away exit once the phase goes
// terminal (done/failed).

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const SWIPE_DELAY_MS = 900; // let the final state register before it leaves

export class PhaseCards {
  constructor(container) {
    this.container = container;
    this.cards = new Map(); // span_id -> {el, spinnerEl, labelEl, statusEl, timer, tick}
  }

  apply(spanId, phase) {
    let card = this.cards.get(spanId);
    if (!card) {
      card = this.create(spanId);
      this.cards.set(spanId, card);
    }
    const { kind } = phase;
    if (kind === "queued") {
      card.labelEl.textContent = "queued";
      card.statusEl.textContent = "[....]";
    } else if (kind === "building" || kind === "running") {
      card.labelEl.textContent = phase.tool ?? kind;
      card.statusEl.textContent = kind === "building" ? "[bld]" : "[run]";
    } else if (kind === "done" || kind === "failed") {
      this.finish(spanId, card, phase);
    }
  }

  create(spanId) {
    const el = document.createElement("div");
    el.className = "phase-card";
    el.dataset.span = spanId;

    const spinnerEl = document.createElement("span");
    spinnerEl.className = "phase-spinner";
    spinnerEl.textContent = SPINNER[0];

    const labelEl = document.createElement("span");
    labelEl.className = "phase-label";

    const statusEl = document.createElement("span");
    statusEl.className = "phase-status bracket";

    el.append(spinnerEl, labelEl, statusEl);
    this.container.append(el);

    const card = { el, spinnerEl, labelEl, statusEl, tick: 0, timer: null };
    card.timer = setInterval(() => {
      card.tick = (card.tick + 1) % SPINNER.length;
      spinnerEl.textContent = SPINNER[card.tick];
    }, 120);
    return card;
  }

  finish(spanId, card, phase) {
    clearInterval(card.timer);
    const failed = phase.kind === "failed";
    card.spinnerEl.textContent = failed ? "✗" : "✓";
    card.spinnerEl.classList.add(failed ? "status-bad" : "status-ok");
    if (phase.detail) {
      card.statusEl.textContent = `[${phase.detail}]`;
    } else {
      card.statusEl.textContent = failed ? "[failed]" : "[done]";
    }
    setTimeout(() => {
      card.el.classList.add("swipe-away");
      card.el.addEventListener(
        "animationend",
        () => {
          card.el.remove();
          this.cards.delete(spanId);
        },
        { once: true },
      );
    }, SWIPE_DELAY_MS);
  }

  // Force-complete any cards a finished turn left running (e.g. parser never
  // saw a completion for that span).
  settleAll(ok) {
    for (const [spanId, card] of [...this.cards]) {
      if (!card.el.classList.contains("swipe-away")) {
        this.finish(spanId, card, { kind: ok ? "done" : "failed", detail: null });
      }
    }
  }
}
