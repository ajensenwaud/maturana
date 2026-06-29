# maturana-web

Use this skill when you need to **enable, disable, start, expose, or operate**
the **Maturana web cockpit** — the browser control plane for the agent fleet
(`:47836`, WebSocket + REST, token login). It complements the Codex CLI and TUI;
it does not replace them. Because the cockpit has no TLS of its own, **the bind
address is the security boundary** — so this skill's core job is to choose where
it listens, always asking the operator when Tailscale is available.

**Enable** the cockpit = register the supervised `web` service
(`maturana service install web`); **disable** it = stop and unregister that
service (`maturana service uninstall web`). A one-off foreground run
(`maturana web`) is for trying it; the service is how it stays on across reboots.

## Grounding

1. Read `AGENTS.md` first; confirm the operator actually wants the cockpit
   started/exposed (vs. the CLI/TUI), and on which host.
2. Check for Tailscale: `tailscale ip -4`. A `100.x.y.z` address means the host
   is on a tailnet and the cockpit can be bound to it (reachable only from the
   tailnet, not the LAN/internet).
3. The login token lives at `<home>/web/token`; print it with `maturana web
   token`. The operator pastes it at `/login` to get a session cookie.

## Preflight

- Decide the bind, and **ask the operator when Tailscale is up** — never expose
  the cockpit beyond a trusted network without TLS in front:
  - tailnet only (recommended): `maturana web --tailnet`
  - all interfaces (only behind a TLS reverse proxy): `maturana web --bind 0.0.0.0:47836`
  - localhost only: `maturana web --bind 127.0.0.1:47836`
- Confirm port `47836` is free (`ss -ltnp | grep 47836`); a stale instance will
  hold it (`Address already in use`).
- Confirm the home is correct (`--home <path>`); the cockpit reads
  `<home>/agents`, `<home>/web/token`, `<home>/audit`.

## Decision Path

- Started **interactively** with no `--bind`: `maturana web` prints the tailnet/
  all/localhost menu and asks — let the human choose.
- Started **on the operator's behalf** (you run it): ask first, then pass the
  chosen flag explicitly (`--tailnet` or `--bind`).
- Run as a **service**: `maturana service install web` registers a supervised
  unit. To pin it to the tailnet, set its `ExecStart` to `maturana web
  --tailnet` (edit the unit), or front it with Tailscale Serve.
- **Enable the cockpit** (operator wants it on, surviving reboots):
  `maturana service install web`. Confirm the bind first if Tailscale is up.
- **Disable the cockpit** (operator wants it off): `maturana service uninstall
  web` — this stops the running cockpit AND unregisters the unit, so it does not
  come back at boot. Disabling does not touch agents, the plane, or the login
  token; re-enable later with `install web`.
- **Check whether it is enabled**: `maturana service status web`.
- Tailscale **not** up: default to localhost or an explicit `--bind`; do not
  expose `0.0.0.0` on an untrusted network.

## Actions

```bash
# Print/create the login token
maturana web token

# Start it, asking where to bind (Tailscale detected → interactive menu)
maturana web

# Bind to the tailnet only (recommended when Tailscale is up)
maturana web --tailnet

# Explicit bind (skips the prompt)
maturana web --bind 127.0.0.1:47836

# Enable the cockpit as a supervised service (restarts on failure + at boot)
maturana service install web

# Is it enabled / running?
maturana service status web

# Disable the cockpit (stop + unregister; will NOT return at boot)
maturana service uninstall web
```

The cockpit's left-nav exposes (in order): **Overview** (fleet + plane at a
glance), **Chat** (talk to an agent), **Agents** (fleet; spec validate → dry-run
→ apply, stop, inspect, and edit config — schedules/MCP/channels, validated),
**Sessions** (transcripts, search, export, prune, send a message), **Graph**
(GraphRAG + ingest), **Egress** (live allow/deny + approve), **Pipelock** (secret
names; set/delete — values never sent to the browser), **Tools**, **Skills**
(view/create), **System** (host stats, logs, activity, plane lifecycle + ops:
restart plane, backup), **Console** (drive a turn; tool-call cards).

## Evidence

Before reporting the cockpit up, collect:

- `curl -s http://<bind-host>:47836/health` returns `{"ok":true}`.
- The startup log prints the exact bind it chose, e.g. `Maturana web cockpit →
  http://100.93.69.127:47836` — and it matches what the operator approved.
- A login round-trip works: `POST /login` with `{token}` returns `{"ok":true}`
  and sets the `maturana_web_session` cookie.
- When `--tailnet` was used, the bind host is the `tailscale ip -4` address (not
  `0.0.0.0`), so the cockpit is NOT reachable from the LAN.
- After **enable**: `maturana service status web` shows the unit registered/
  active, and `/health` answers. After **disable**: `status web` shows it gone
  and `curl :47836/health` fails (connection refused) — the port is free.

## Recovery

- `Address already in use`: a cockpit (often the supervised service) already
  holds `:47836` — restart that service instead of starting a second copy, or
  stop it first.
- `404` on `/api/system/*` or other new routes: the running binary is stale —
  rebuild and restart the unit (the web service may point at an installed binary
  separate from `target/release`).
- `--tailnet` errors "Tailscale isn't up": `tailscale ip -4` returned nothing —
  start Tailscale or bind explicitly with `--bind`.
- Cockpit reachable but every `/api/*` is `401`: the session cookie was lost
  (server restart drops in-memory sessions) — log in again at `/login`.

## Boundaries

- Do not expose the cockpit on `0.0.0.0` or a public address without TLS in
  front — prefer `--tailnet`; the bind address is the only network gate.
- Do not start a second cockpit on a port a supervised one already owns — manage
  the existing service rather than spawning a competitor.
- Do not treat the cockpit as a remote shell: it has no UI shell-hooks, no
  self-update of the running host, and no `--insecure` mode — those are declined
  on zero-trust grounds.
- Do not edit identity / vm / runtime (the isolation boundary) from the browser;
  the Config panel only touches declarative blocks, validated before write.
