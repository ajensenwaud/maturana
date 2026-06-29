// Shared modal UI — form dialogs, confirm dialogs, and toasts. Replaces the
// native prompt()/confirm()/alert() (which look out of place in the cockpit).
// Flat, theme-token styled; keyboard + overlay dismiss.

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

function mount(overlay) {
  const onKey = (e) => { if (e.key === "Escape") close(); };
  function close() { overlay.remove(); document.removeEventListener("keydown", onKey); }
  overlay.addEventListener("mousedown", (e) => { if (e.target === overlay) close(); });
  document.addEventListener("keydown", onKey);
  document.body.append(overlay);
  return close;
}

// fields: [{name,label,type,value,placeholder,options:[{value,label}],rows,required,hint,accept}]
// type: text | textarea | number | select | checkbox | multiselect | file
export function formDialog({ title, sub, fields = [], submitLabel = "Save", onSubmit }) {
  const overlay = el("div", "modal-overlay");
  const card = el("div", "modal-card");
  const head = el("div", "modal-head");
  head.append(el("div", "modal-title", title));
  if (sub) head.append(el("div", "modal-sub", sub));
  card.append(head);

  const form = el("form", "modal-form");
  const inputs = {};
  const advancedRows = [];
  for (const f of fields) {
    const row = el("label", "modal-field");
    if (f.type !== "checkbox") row.append(el("span", "modal-label", f.label));
    let input;
    if (f.type === "textarea") {
      input = el("textarea", "modal-input");
      input.rows = f.rows || 4;
      if (f.value != null) input.value = f.value;
      if (f.placeholder) input.placeholder = f.placeholder;
    } else if (f.type === "select") {
      input = el("select", "modal-input");
      for (const o of f.options || []) { const opt = el("option", null, o.label); opt.value = o.value; input.append(opt); }
      if (f.value != null) input.value = f.value;
    } else if (f.type === "checkbox") {
      input = el("input"); input.type = "checkbox"; input.checked = !!f.value;
      row.append(input, el("span", "modal-label", f.label));
    } else if (f.type === "multiselect") {
      input = el("div", "modal-multi");
      for (const o of f.options || []) {
        const l = el("label", "modal-multi-item");
        const cb = el("input"); cb.type = "checkbox"; cb.value = o.value;
        if ((f.value || []).includes(o.value)) cb.checked = true;
        l.append(cb, document.createTextNode(" " + o.label));
        input.append(l);
      }
    } else if (f.type === "file") {
      input = el("input", "modal-input");
      input.type = "file";
      if (f.accept) input.accept = f.accept;
    } else {
      input = el("input", "modal-input");
      input.type = f.type === "number" ? "number" : "text";
      if (f.value != null) input.value = f.value;
      if (f.placeholder) input.placeholder = f.placeholder;
    }
    inputs[f.name] = { input, type: f.type, required: f.required };
    if (f.type !== "checkbox") row.append(input);
    if (f.hint) row.append(el("span", "modal-hint", f.hint));
    if (f.advanced) advancedRows.push(row); else form.append(row);
  }

  // Optional fields flagged `advanced: true` fold into a collapsed section so the
  // form leads with the essentials instead of a wall of power-user knobs.
  if (advancedRows.length) {
    const det = el("details", "modal-advanced");
    const sum = document.createElement("summary");
    sum.textContent = `Advanced (${advancedRows.length})`;
    det.append(sum, ...advancedRows);
    form.append(det);
  }

  const errLine = el("div", "modal-err");
  const actions = el("div", "modal-actions");
  const submit = el("button", "primary", submitLabel); submit.type = "submit";
  const cancel = el("button", "primary ghost", "Cancel"); cancel.type = "button";
  actions.append(submit, cancel);
  form.append(errLine, actions);
  card.append(form);
  overlay.append(card);
  const close = mount(overlay);
  cancel.addEventListener("click", close);

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const values = {};
    for (const [name, { input, type, required }] of Object.entries(inputs)) {
      if (type === "checkbox") values[name] = input.checked;
      else if (type === "multiselect") values[name] = [...input.querySelectorAll("input:checked")].map((c) => c.value);
      else if (type === "file") values[name] = input.files && input.files[0] ? input.files[0] : null;
      else values[name] = typeof input.value === "string" ? input.value.trim() : input.value;
      if (required && (values[name] === "" || values[name] == null)) {
        errLine.textContent = `${name} is required`;
        return;
      }
    }
    submit.disabled = true; errLine.textContent = "";
    try { await onSubmit(values); close(); }
    catch (ex) { errLine.textContent = String(ex && ex.message ? ex.message : ex); submit.disabled = false; }
  });

  const first = card.querySelector("input, textarea, select");
  if (first) setTimeout(() => first.focus(), 30);
}

export function confirmDialog({ title = "Confirm", message = "", danger = false, confirmLabel = "Confirm" } = {}) {
  return new Promise((resolve) => {
    const overlay = el("div", "modal-overlay");
    const card = el("div", "modal-card");
    card.append(el("div", "modal-title", title));
    if (message) card.append(el("div", "modal-sub", message));
    const actions = el("div", "modal-actions");
    const yes = el("button", danger ? "primary danger" : "primary", confirmLabel);
    const no = el("button", "primary ghost", "Cancel");
    actions.append(yes, no);
    card.append(actions);
    overlay.append(card);
    const onKey = (e) => { if (e.key === "Escape") done(false); };
    function done(v) { overlay.remove(); document.removeEventListener("keydown", onKey); resolve(v); }
    yes.addEventListener("click", () => done(true));
    no.addEventListener("click", () => done(false));
    overlay.addEventListener("mousedown", (e) => { if (e.target === overlay) done(false); });
    document.addEventListener("keydown", onKey);
    document.body.append(overlay);
    setTimeout(() => yes.focus(), 30);
  });
}

let toastHost = null;
export function toast(message, kind = "info") {
  if (!toastHost) { toastHost = el("div", "toast-host"); document.body.append(toastHost); }
  const t = el("div", `toast ${kind}`, String(message));
  toastHost.append(t);
  setTimeout(() => { t.classList.add("out"); setTimeout(() => t.remove(), 300); }, 3400);
}
