# Maturana Web Cockpit

`maturana web` serves a browser-based control surface for the platform. It
**complements** the Codex CLI control plane — it never replaces it. Both
surfaces drive the same contract: the cockpit's prompt console spawns the
same `codex exec` (oriented by `AGENTS.md` + `skills/` at the repo root) that
an interactive `codex` session uses, so nothing forks.

## Architecture

- `crates/maturana-web` is the **only** async crate in the workspace
  (axum + tokio). Everything below it stays sync; core calls run via
  `spawn_blocking`. Realtime is **WebSockets only** (one socket per client,
  all topics multiplexed; protocol v1 in `src/ws/protocol.rs`).
- The "host never calls model APIs" invariant applies to platform services
  (sessiond, graph, channels). The cockpit is the *operator's* seat:
  spawning `codex exec` host-side automates the existing human workflow
  (Codex subscription), and the OpenRouter adapter (`opencode run -m
  openrouter/<model>`, key from `pipelock:openrouter/api-key`) is likewise an
  operator choice. Platform services remain model-free.
- All frontend assets (including vendored CodeMirror 6 with Vim mode and
  marked) embed in the binary — no Node toolchain at build or run time.

## Ports

| Port  | Service            | Bind            |
|-------|--------------------|-----------------|
| 47832 | hostd (Hyper-V)    | 127.0.0.1       |
| 47833 | pipelock proxy     | guest-facing    |
| 47834 | sessiond           | 0.0.0.0         |
| 47835 | MaturanaGraph      | 0.0.0.0         |
| 47836 | web cockpit        | 0.0.0.0         |

## Access + security

- Token at `<home>/web/token` (created on first run) is exchanged at
  `/login` for an HttpOnly `SameSite=Strict` session cookie (in-memory
  sessions; re-login after a server restart).
- CSRF posture: SameSite=Strict + a required `x-maturana-web: 1` header on
  mutating REST calls + an Origin==Host check on the WebSocket upgrade. No
  CORS headers are served.
- Pipelock secret *values* never serialize to the browser — names only.
  Model/search keys resolve host-side into child process env or proxy
  injection.
- **No TLS in v1.** The bind assumes a trusted LAN/Tailscale network. Front
  with Tailscale Serve or a reverse proxy before exposing further.

## Panels

Console (prompt → codex/OpenRouter with streamed output, tool/skill phase
cards that swipe away, markdown editor with Vim toggle) · Agents (fleet,
status, stop, spec validate→dry-run→apply flow) · Runtime (`maturana up`
heartbeat from `<home>/up/state.json`, service probes, doctor) · Sessions
(queues + chat with guests over the `web` channel) · Graph (stats, GraphRAG
query, document ingest) · Pipelock (secret names, egress allowlist editor) ·
Tools · Skills.

## Install + services

One-liners (idempotent; both register `maturana up` + `maturana web` as
services via `maturana service install`):

```sh
# Linux
curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.sh | bash
```

```powershell
# Windows
irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.ps1 | iex
```

Service management is Rust-owned: `maturana service install|uninstall|
status|restart [up|web]` (systemd user units on Linux — run `loginctl
enable-linger $USER` for boot-time start; Scheduled Tasks on Windows). The
privileged Hyper-V hostd keeps its dedicated elevated installer
(`scripts/install-hostd-task.ps1`).

## Orientation (both surfaces are equals)

1. **Codex CLI**: `cd ~/maturana && codex` — `AGENTS.md` and `skills/` are
   the product contract that orients the session.
2. **Web cockpit**: open `http://<host>:47836`, paste the token from
   `<home>/web/token`. The console drives the same skills through the same
   `codex exec`.
