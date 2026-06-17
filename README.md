# Maturana

> *A secure agent harness that runs every agent in its own hardware-isolated microVM. Lightweight, built to be read and understood, and completely yours to customize from Codex.*

Maturana turns a single `MATURANA.md` file into a running, always-on AI agent inside its own
Firecracker (Linux) or Hyper-V (Windows) microVM — with a bounded filesystem, an egress
allowlist, encrypted secrets, durable shared memory, and one-command snapshot/rewind. You
define and operate everything from Codex.

---

## Motivation

There is no shortage of agent harnesses. Most optimize for features, not security. The powerful
ones have grown so large and so flexible that their attack surface is enormous — large enough
that vendors now ship hardened shells just to make them safe to run. Others isolate agents in
containers, which is the right instinct, but bind themselves tightly to a single vendor's
ecosystem.

I wanted something different: a harness I can actually read, that is secure because of how it is
*built*, not because of a wall of permission checks bolted on afterward. I also just enjoy
engineering with Codex. So Maturana is a lean harness on the Codex ecosystem, with
**hardware-level** isolation for every agent — Firecracker on Linux, Hyper-V on Windows — and
zero-trust wired through the whole thing.

It combines the elegance of Unix, the agentic workflow of Codex, and the isolation of a
hypervisor. The core is a small Rust runtime; everything else is a skill or a tool you can read,
swap, or write yourself.

**Maturana is not** a chat UI competing with Codex, a generic multi-control-plane framework,
Docker orchestration, or multi-tenant SaaS. It is a single-operator, security-first agent
harness.

---

## Why Maturana

**Secure by design.** Agents are isolated with real hardware virtualization — a Firecracker or
Hyper-V microVM per agent — for maximum security, not just a container.

**Zero-trust.** Pipelock keeps secrets like API keys and credit-card numbers out of an agent's
reach, and an egress proxy controls exactly which systems it can talk to. Treat every agent as an
adversary and lock it down.

**Build anything.** Maturana is built on and for Codex, the premier OpenAI agent-engineering
environment. Everything is a skill — from agent creation to tools — so you customise your agents
with prompts and nothing else.

**Self-evolving.** An internal WASM engine lets agents build their own tools on the fly, safely
sandboxed with no ambient authority.

**Shared knowledge.** Maturana ships with a shared knowledge graph that scales past markdown
files. Agents build their own memory, so you don't have to.

**Lean and fast.** Maturana is built in Rust with a modular core from the start. Skills are
extensions to that core — you run only what you need.

---

## Getting started

### Install

One line. It downloads the signed prebuilt `maturana` binary (no Rust toolchain needed),
verifies its checksum, clones the repo for the skills/examples, and registers the runtime plane
as a service.

```sh
# Linux — control plane only
curl -fsSL https://www.maturana.sh/install.sh | bash

# Linux that will also RUN isolated agents — add the Firecracker microVM host
curl -fsSL https://www.maturana.sh/install.sh | bash -s -- --firecracker
```

```powershell
# Windows (Hyper-V) — self-elevates once, prompts for your Windows password (for the no-login boot tasks)
irm https://www.maturana.sh/install.ps1 | iex
```

Build from source instead with `--from-source` (Linux) / `-FromSource` (Windows). Uninstall any
time with `scripts/uninstall.sh` / `scripts/uninstall-windows.ps1` — add `--purge` / `-Purge` to
also delete your agents and secrets.

### Your first agent (Linux / Firecracker)

Maturana is **Codex-native** — you don't hand-assemble an agent from CLI flags. You tell Codex to
build one, and it runs the **`maturana-agent-create`** skill as a guided setup wizard: it
interviews you (the agent's name, who you are, how you'll reach it, what it can do), writes its
`IDENTITY.md` / `SOUL.md` / `MATURANA.md`, then launches it into a Firecracker microVM and
validates a live turn — driving the `maturana-agent-create → -launch → -validate` skills end to
end. That conversation **is** the product.

```sh
# 1. Open a fresh login shell so the `kvm` group + ~/.local/bin PATH apply
#    (sanity: `ls -l /dev/kvm` is group-readable, `maturana --help` resolves).

# 2. Log in to the harness your agent will run on (at least one):
codex login          # or:  claude   (then /login inside it)

# 3. Hand Codex the wheel — it's oriented by AGENTS.md + the skills/ pack:
cd ~/maturana && codex
```

Then just tell it what you want:

> **create and launch a new agent**

…or invoke the skill directly — type `/skills`, or `$maturana-agent-create`. Codex runs the
wizard, builds the image, boots the microVM, and tells you when your agent is up and reachable (a
few minutes). All 31 skills ship as Codex skills under `~/.agents/skills`.

**Note:** run this in a **plain shell**, not inside a sandboxed agent — Firecracker needs
`/dev/kvm`, which a sandbox hides.

<details>
<summary>Rather drive the CLI yourself? The skill just orchestrates these steps.</summary>

```sh
cd ~/maturana
mkdir -p .maturana/host-auth && cp -r ~/.codex .maturana/host-auth/codex   # stage harness auth
maturana setup firecracker-harnesses --agent-id codex-firecracker          # build image + boot microVM (idempotent)
maturana service status up                                                 # runtime plane already runs as a service
maturana agent run codex-firecracker --prompt "say hi" --wait              # talk to it
```

</details>

See [docs/linux-firecracker-harnesses.md](docs/linux-firecracker-harnesses.md) for the full Linux
guide and [docs/harness-operations.md](docs/harness-operations.md) for Windows / Hyper-V.

---

## Using it

A Maturana agent is one `MATURANA.md` spec — identity, runtime, VM, mounts, egress, memory,
channels, schedules, snapshots. Codex writes it; you can read and edit it. (Full field
reference: [docs/maturana-spec.md](docs/maturana-spec.md).)

```sh
maturana spec validate examples/MATURANA.codex-firecracker.md   # check before launch
maturana agent launch examples/MATURANA.codex-firecracker.md --apply
maturana agent inspect codex-firecracker --live                 # health, logs, status
```

**Talk to an agent**

- Console TUI: `maturana tui` (agent picker) or `maturana agent chat <id>`
- Host turn: `maturana agent run <id> --prompt "…" --wait`
- Telegram / Discord — pair a bot, then chat from your phone (one bot per agent):

  ```sh
  maturana pipelock set telegram/bot-token --value-file ./token
  maturana channel pair telegram start --agent-id <id> --token-source pipelock:telegram/bot-token
  # send the printed  /pair <CODE>  to your bot
  ```

**Always-on** — agents have a heartbeat, run cron-style schedules, and push notifications:

```sh
maturana schedule add <id> morning --cron "0 9 * * *" --prompt "Send a morning brief" --channel telegram
```

**Capabilities** — skills give agents the web and your tools: browse (headless Chrome), web
search, image generation, voice (speech-to-text / text-to-speech), and GitHub / Notion / Slack /
email integrations.

**Govern** — read the audit trail, then snapshot and rewind:

```sh
maturana audit list <id> --limit 10
maturana snapshot take <id> before-change --live
maturana snapshot restore <id> before-change --live
```

---

## Customising it

**Tailor it to your exact needs with Codex.** Because every capability is a skill, extending
Maturana is a conversation: ask Codex to write a new skill or tool, test it, and deploy it into
a running guest. The skill pack already includes `maturana-skill-create`, `maturana-tool-create`,
`maturana-develop`, and `maturana-skill-deploy` for exactly this.

**Self-mutation with WASM.** Agents can author, build, register, and run their own tools at
runtime — no host rebuild. A tool is one WebAssembly module plus a manifest, executed in a
capability-gated sandbox with **no ambient authority**: fuel metering, a wall-clock timeout, a
memory ceiling, and only the filesystem/network the manifest opts into. It is the Maturana
answer to on-the-fly tool creation, made safe by default.

```sh
maturana tool register weather --wasm weather.wasm --manifest tool.json
maturana tool run weather --input '{"city":"oslo"}'
```

See [docs/wasm-tools.md](docs/wasm-tools.md) and the `maturana-self-forge` skill.

---

## Requirements

|              | Linux                              | Windows                                   |
| ------------ | ---------------------------------- | ----------------------------------------- |
| OS           | x86_64 with KVM                    | 11 Pro / Enterprise / Workstations        |
| Hypervisor   | Firecracker (`--firecracker`)      | Hyper-V                                    |
| Guest harness| A Codex, Claude Code, or OpenCode subscription (OAuth injected into the VM at runtime) | same |
| Build        | Prebuilt signed binary by default; Rust toolchain only for `--from-source` | same |
| Optional     | Telegram/Discord tokens, integration API keys — all via pipelock | same |

macOS is not supported yet.

---

## Architecture

Codex orchestrates from the host. A small set of long-lived Rust processes — the **runtime
plane**, supervised as one restart-on-failure group by `maturana up` — own channels, schedules,
the session queue, egress, and shared memory. Each agent runs inside its own VM, where the
selected harness executes the turn.

```
        you ── Codex (control plane) ──────────────────────────────┐
                                                                   │ define / launch / govern
  ┌──────────────────────────── host runtime plane ────────────────┴─────────────┐
  │  maturana up  (supervises every process, restarts on failure)                 │
  │                                                                               │
  │   sessiond :47834        channel bridges          schedule runners            │
  │   per-agent SQLite       (Telegram / Discord)     (cron → queue)              │
  │                                                                               │
  │   pipelock proxy :47833      MaturanaGraph :47835      hostd :47832 (Windows) │
  │   egress allowlist +         knowledge graph +         fixed Hyper-V          │
  │   credential injection       GraphRAG                  lifecycle              │
  └───────────────┬───────────────────────────────────────────────────────────────┘
                  │   session queue (HTTP)   +   governed SSH
        ┌─────────┴──────────┐   ┌────────────────────┐   ┌─ … one microVM per agent
        │  microVM: agent A  │   │  microVM: agent B  │
        │  harness            │   │  harness …         │
        │  (codex / claude-   │   │                    │
        │   code / opencode)  │   │                    │
        │  run-agent.sh loop  │   │                    │
        └─────────────────────┘   └────────────────────┘
   Firecracker (Linux) / Hyper-V (Windows) — hardware isolation per agent
```

One Telegram turn travels the queue and back, so channels never touch the harness lifecycle:

```
Telegram → channel bridge → inbound (sqlite) ← (HTTP) ← guest worker → harness
                                  ↑                          ↓
Telegram ← channel bridge ← outbound (sqlite) ← (HTTP) ──────┘
```

**Ports**

| Port  | Service                     | Bind         |
| ----- | --------------------------- | ------------ |
| 47832 | hostd (Hyper-V, Windows)    | 127.0.0.1    |
| 47833 | pipelock egress proxy       | guest-facing |
| 47834 | sessiond (session queue)    | 0.0.0.0      |
| 47835 | MaturanaGraph               | 0.0.0.0      |

The host never casually exposes its filesystem to a guest: workspace, memory, wiki, schedules,
tools, audit, and snapshots all live under per-agent directories with governed mounts. Deeper
detail in [docs/orchestration.md](docs/orchestration.md),
[docs/script-boundary.md](docs/script-boundary.md), and the
[documentation index](docs/README.md).

---

## Community

Questions, ideas, or want to share an agent? **Join the Discord — find the invite at
[maturana.sh](https://maturana.sh).**

- **Docs:** start with the [documentation index](docs/README.md).
- **License:** BSD 3-Clause — see [LICENSE](LICENSE).
