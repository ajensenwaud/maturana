# Web UI: Maturana cockpit vs Hermes Agent dashboard

A side-by-side of the **web interfaces** (not the orchestration engines — that's
`multi-agent-orchestration.md`). Maturana side is from the `maturana-web` crate
(currently disabled; bringing it back is the trigger for this comparison). Hermes
side is from Nous Research's official dashboard docs (June 2026).

Sources: Hermes [web-dashboard docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/web-dashboard),
[repo](https://github.com/NousResearch/hermes-agent).

## What each one is

- **Maturana cockpit** (`maturana-web`, `:47836`, WebSocket + REST, token auth):
  a **control plane for a fleet of VM-isolated agents** — manage each agent's
  spec, watch the supervisor, govern egress, query the knowledge graph. Console
  drives `codex exec` / `opencode run` as a streaming view.
- **Hermes dashboard** (`:9119`, loopback by default): a **machine-level admin
  panel for one host's agent** — config/secrets/sessions/logs/cron/channels, plus
  the real `hermes --tui` embedded in the browser over a PTY.

## Side-by-side

| Area | Maturana now | Hermes | Read |
|---|---|---|---|
| **Chat console** | Custom streaming view (CodeMirror prompt, Ctrl+Enter, phase cards, markdown, harness=codex/openrouter, model field, mic→STT, cancel). One turn at a time. | **Full TUI embedded in browser** (xterm.js over PTY WS): slash commands, model picker, tool-call cards, approval prompts, resume. | Hermes leads (it *is* the live agent); ours is a bespoke view |
| **Sessions** | List (agent/session/queue), transcript, composer to send, live updates | Search (FTS5), per-session token/cost/model/msg counts, rename, export JSON, prune, color-coded history | Hermes much deeper |
| **Agent/spec mgmt** | **Fleet table; per-agent spec edit → validate → dry-run → apply; stop; inspect** | Profiles (clone/switch); no per-agent VM lifecycle | **Maturana leads** |
| **Secrets** | Pipelock — names only, set/delete, **values never sent to browser** | `.env` keys by category + redacted preview + delete; **rotation pool** | Maturana safer; Hermes richer UI + key rotation |
| **Egress governance** | **Live allow/deny feed + one-click approve, promote-to-spec** | — (not a feature) | **Maturana only** |
| **Knowledge graph** | **Stats + GraphRAG query + document ingest (upload→chunk→upsert)** | "Memory": pick provider / reset stores (lighter) | **Maturana leads** |
| **MCP servers** | Not in UI (per-spec only) | Page: list/enable/test/remove + catalog one-click install | Hermes only |
| **Skills** | Read-only catalog + view SKILL.md | Toggle on/off, toolsets, **hub search+install**, curator | Hermes only (mgmt) |
| **Scheduled jobs (cron)** | Not in UI | Create/edit/pause/trigger/delete, last/next run, delivery target | Hermes only |
| **Channels / pairing / webhooks** | Not in UI (per-spec) | Connect every platform from browser; approve/revoke users; webhook subs | Hermes only |
| **Logs** | None (runtime processes + doctor JSON) | Agent/gateway/error files, filter by level/component, live tail | Hermes only |
| **Analytics / cost** | None | Tokens, est/actual cost, cache-hit %, daily charts, per-model | Hermes only |
| **Runtime/ops** | Supervisor + process table, health probes, doctor | Host stats (CPU/mem/disk/uptime), self-update, gateway start/stop, backup/restore, checkpoints, shell hooks | Hermes broader; Maturana has supervisor view |
| **Tools** | WASM registry (read-only) | Toolsets (read-only) | ~Even |
| **Auth** | Single token + HttpOnly cookie + CSRF + Origin check (front w/ Tailscale) | Loopback default; non-loopback gate w/ **Nous OAuth / user-pass / OIDC**, audit log, rebinding guard | Hermes richer (multi-user/OIDC) |
| **Theming / extensibility** | One look (design tokens) | 6 themes + font + **plugin themes/tabs/slots/routes** | Hermes only |

## Where Maturana already leads (keep these front-and-center)

1. **VM-per-agent fleet management** — validate/dry-run/apply a spec, stop an
   agent, inspect its processes. Hermes is single-host; it has no equivalent.
2. **Egress governance in the UI** — the live deny/allow feed with hot-approve
   (and promote-to-spec) is a zero-trust capability Hermes simply doesn't have.
3. **Knowledge-graph console** — ingest a document and run GraphRAG queries from
   the browser. Hermes "memory" is a provider toggle.
4. **Zero-trust secrets** — pipelock values are never serialized to the browser.

## Gap themes (raw material for the goal-seek)

- **A. Observability** — log viewer, usage/cost analytics, host stats, per-session
  token/cost. *We have almost none of this.*
- **B. Browser-managed config** — MCP servers, channels/pairing/webhooks, cron
  jobs, skill toggle/install. *We edit specs but expose no dedicated panels.*
- **C. Sessions depth** — full-text search, export, rename/prune, richer transcript.
- **D. Embedded interactive agent** — Hermes runs the real TUI in-browser over a
  PTY. Ours is a one-shot streaming console. Biggest UX delta.
- **E. Ops from the UI** — backup/restore, self-update, gateway lifecycle, checkpoints.
- **F. Auth/multi-user** — OIDC/OAuth, audit log (we have single-token + Tailscale).
- **G. Theming/extensibility** — themes, plugin tabs.

## Design stance (do not copy blindly)

Maturana's product is the VM-isolated fleet + governed egress + host-enforced
budgets. Several Hermes conveniences (shell hooks from the UI, `--insecure` mode,
self-update applying to a running host) cut against that. Adopt the *capability*
(e.g. observability, scheduled jobs) but keep it inside the zero-trust model.
