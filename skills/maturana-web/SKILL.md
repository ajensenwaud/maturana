# maturana-web

Use this skill to start, expose, and operate the **Maturana web cockpit** — the
browser control plane for the agent fleet (`:47836`, WebSocket + REST, token
login). It complements the Codex CLI and TUI; it does not replace them.

## Starting the cockpit

The binary is `maturana web`. **Before starting it, decide where it should
listen — always ask the operator when Tailscale is available**, because the
cockpit has no TLS of its own and the bind address is the security boundary:

1. Check for Tailscale: `tailscale ip -4` (a `100.x.y.z` address means it's up).
2. If Tailscale is up, **ask the operator** which to use:
   - **tailnet only** (recommended) — reachable only from their tailnet:
     `maturana web --tailnet`
   - **all interfaces** — LAN + tailnet; only if a TLS reverse proxy fronts it:
     `maturana web --bind 0.0.0.0:47836`
   - **localhost only** — same machine: `maturana web --bind 127.0.0.1:47836`
3. If Tailscale is NOT up, default to localhost or an explicit `--bind`; never
   expose `0.0.0.0` to an untrusted network without TLS in front.

Running `maturana web` interactively with **no `--bind`** prints this same menu
and asks — so a human at the terminal is prompted automatically. When you start
it on the operator's behalf, ask first and pass the chosen flag.

The login token is at `<home>/web/token`; print it with `maturana web token`.
The operator pastes it at `/login` to get a session cookie.

## Running it as a service

`maturana service install web` registers a supervised unit (Linux systemd user
service / Windows scheduled task) that restarts on failure and at boot. By
default the service binds `0.0.0.0:47836`; to pin it to the tailnet, install it
to run `maturana web --tailnet` (edit the unit's ExecStart) or front it with
Tailscale Serve.

## What the cockpit exposes

A single page with a left-nav: **Console** (drive an agent turn, streaming),
**Agents** (fleet table; per-agent spec validate → dry-run → apply, stop,
inspect), **Runtime** (supervisor + processes, health, doctor), **Sessions**
(transcripts; send a message into a session), **Graph** (GraphRAG stats/query +
document ingest), **Pipelock** (secret names; set/delete — values never sent to
the browser), **Egress** (live allow/deny feed + one-click approve), **Tools**
(WASM registry), **Skills** (catalog). Mutating calls require the `x-maturana-web`
header (CSRF) and a valid session cookie; the WebSocket upgrade checks Origin.

## Boundaries

- The bind address is the security boundary — never widen it (e.g. to `0.0.0.0`
  on a public box) without TLS in front. Prefer the tailnet.
- Secrets are never serialized to the browser (pipelock lists names only).
- The cockpit drives the SAME front door as every channel
  (`channels::enqueue_turn`), so a turn run here gets the same memory + routing.
