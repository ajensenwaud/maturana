// Palette switcher for the cockpit. A theme is just a `data-theme` value on
// <html>; tokens.css derives every shade from each palette's base colors, so
// switching is a single attribute write. The choice persists in localStorage
// and is applied pre-paint by a tiny inline script in index.html (no flash).

const STORAGE_KEY = "maturana.theme";

// Names are our own — intentionally not the upstream theme names. `bg`/`accent`
// are only for the little swatch in the menu (the real colors live in CSS).
export const THEMES = [
  { id: "teal", name: "Deep Teal", bg: "#041c1c", accent: "#ffbd38" },
  { id: "indigo", name: "Indigo", bg: "#0b0b1f", accent: "#a78bfa" },
  { id: "crimson", name: "Crimson", bg: "#1a0a06", accent: "#fb7233" },
  { id: "graphite", name: "Graphite", bg: "#101012", accent: "#d4d4d8" },
  { id: "neon", name: "Neon", bg: "#050805", accent: "#39ff14" },
  { id: "solar-dark", name: "Solarized Dark", bg: "#002b36", accent: "#268bd2" },
  { id: "solar-light", name: "Solarized Light", bg: "#fdf6e3", accent: "#268bd2" },
];

const DEFAULT_THEME = "teal";

export function currentTheme() {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved && THEMES.some((t) => t.id === saved)) return saved;
  } catch {}
  return DEFAULT_THEME;
}

export function applyTheme(id) {
  const theme = THEMES.find((t) => t.id === id) ? id : DEFAULT_THEME;
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem(STORAGE_KEY, theme); } catch {}
  return theme;
}

// Build the palette button + dropdown into `host`.
export function mountThemeSwitcher(host) {
  if (!host) return;
  host.classList.add("theme-switch");
  host.replaceChildren();

  const btn = document.createElement("button");
  btn.className = "theme-btn";
  btn.type = "button";
  btn.title = "Theme";
  btn.setAttribute("aria-label", "Switch theme");
  btn.textContent = "◑"; // palette/contrast glyph
  host.append(btn);

  const menu = document.createElement("div");
  menu.className = "theme-menu";
  menu.hidden = true;
  const title = document.createElement("div");
  title.className = "theme-menu-title";
  title.textContent = "Palette";
  menu.append(title);

  const draw = () => {
    const active = currentTheme();
    for (const child of [...menu.querySelectorAll(".theme-item")]) child.remove();
    for (const t of THEMES) {
      const item = document.createElement("div");
      item.className = "theme-item" + (t.id === active ? " active" : "");
      const swatch = document.createElement("span");
      swatch.className = "theme-swatch";
      swatch.style.background = `linear-gradient(135deg, ${t.bg} 0 52%, ${t.accent} 52% 100%)`;
      const label = document.createElement("span");
      label.textContent = t.name;
      const check = document.createElement("span");
      check.className = "theme-check";
      check.textContent = "✓";
      item.append(swatch, label, check);
      item.addEventListener("click", () => {
        applyTheme(t.id);
        draw();
        close();
      });
      menu.append(item);
    }
  };

  const open = () => { draw(); menu.hidden = false; };
  const close = () => { menu.hidden = true; };
  const toggle = () => (menu.hidden ? open() : close());

  btn.addEventListener("click", (e) => { e.stopPropagation(); toggle(); });
  menu.addEventListener("click", (e) => e.stopPropagation());
  document.addEventListener("click", () => close());
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") close(); });

  host.append(menu);
}
