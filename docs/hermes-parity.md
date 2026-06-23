# Hermes Agent — feature parity

Hermes Agent (Nous Research, open-source self-hosted agent framework, 2026) is the
reference point for this work. This document maps every Hermes capability to
Maturana's status and records, for anything we deliberately do **not** copy, the
zero-trust reason. Maturana's stance: hardware-VM isolation, no secrets in the
guest, governed egress, host-enforced budgets. Parity never comes at the cost of
those.

Legend: **Have** = already shipped · **Partial** = exists but narrower ·
**Add** = built on the `ajw/hermes-parity` branch · **Declined** = intentionally
not copied (security).

## Multi-agent orchestration

| Hermes | Maturana | Status |
|---|---|---|
| Orchestrator decomposes a goal, spawns specialist workers with tailored context | `orchestrator loop` (coordinator → roles → workers; each worker gets only its step's framed task) | **Have** |
| Agent-to-agent message passing, typed result objects | A2A (JSON-RPC 2.0, typed `Task`/`Artifact`); orchestrator→worker and in-band peer→peer | **Have** |
| Resource-aware scheduling / concurrency limits | host-enforced `OrchestratorCaps` (turns/wall/steps/parallel/VMs, unraiseable ceilings) | **Have** |
| Spawn each worker in its own clean workspace | on-demand Firecracker VM per role, or reuse standing agents | **Have** (stronger: a VM, not a dir) |
| **Parallel execution** — fire multiple workers at once, wait for all | the loop honored a `max_parallel` cap but ran steps **serially** | **Add** — true parallel dispatch |
| **Kanban board coordination** — persistent cards (title, assignee, status); a dispatcher loop claims ready cards and spawns the assigned agent | no persistent, user-editable board; orchestration was ephemeral (goal→plan→done) | **Add** — `maturana board` |

## Self-improvement & skills

| Hermes | Maturana | Status |
|---|---|---|
| Persistent memory | MaturanaGraph (GraphRAG) + LLM-wiki + durable per-agent memory | **Have** |
| Closed learning loop (trajectories → improvement) | trajectory capture + reward → curated examples / preference pairs → offline SFT/DPO → eval gate → snapshot-safe rollout | **Have** |
| Build/run new capabilities on the fly | self-forge (sandboxed WASM, fuel/memory/timeout bounded) | **Have** (stronger sandbox) |
| **Auto-skill induction** — detect a repeated workflow, write a reusable skill file | skills are authored (skill-create) or model-level; no "observe repetition → propose skill" | **Partial** — candidate next step; see Boundaries |

## Channels, providers, MCP

| Hermes | Maturana | Status |
|---|---|---|
| 16+ messaging platforms | Telegram, Discord, Slack, AgentMail, web cockpit, TUI (6) | **Partial** — same front-door (`channels::enqueue_turn`) makes more channels additive; not the differentiator |
| 17+ LLM providers | runtime-agnostic guests (codex / claude-code / opencode); opencode+OpenRouter reaches many models | **Partial** |
| Native MCP client | MCP servers per spec (stdio + HTTP), rendered into each harness | **Have** |

## Execution backends — **Declined (zero-trust)**

Hermes runs work across seven backends: local, Docker, SSH, Singularity, Modal,
Daytona, Vercel Sandbox. Maturana runs every agent in its **own Firecracker or
Hyper-V VM** on purpose — hardware-level isolation, no shared kernel, no ambient
network, secrets injected at runtime and never written to the guest. Adding
local/Docker/SSH execution would trade that away for convenience. We **decline**
these backends and keep the VM as the only execution substrate. (The board's
dispatcher still gives the same "clean workspace per task" ergonomic — it just
backs it with a VM.)

## What this branch adds

1. **`maturana board`** — a persistent multi-agent Kanban: cards with a title,
   an assignee (a role or a specific agent), a status, and dependencies. A
   `board run` dispatcher claims every ready card (deps satisfied) and runs it on
   its assignee over A2A, reusing the orchestrator's host-enforced budgets,
   real-artifact collection, and run-it verification. This is Maturana's
   zero-trust realization of Hermes' headline coordination layer.
2. **Parallel worker execution** — the dispatcher runs ready cards concurrently up
   to `max_parallel`, bounded by the same caps (the A2A server is already
   thread-per-connection).

## Boundaries (kept intentionally)

- No execution backend other than the VM. Isolation is the product.
- The board's caps are host-enforced and unraiseable by an agent, exactly like the
  orchestrator's — a board can't widen its own budget.
- Auto-skill induction, if added, must route a proposed skill through
  `maturana-security-review` before it can be installed — an agent observing its
  own repetition must not be able to silently grant itself new automation.
- More channels/providers are additive and welcome, but they are breadth, not the
  differentiator, and each must keep secrets host-side and egress governed.

Sources: [Hermes multi-agent](https://hermes-agent.ai/features/multi-agent),
[issue #344](https://github.com/NousResearch/hermes-agent/issues/344),
[Hermes docs](https://hermes-agent.nousresearch.com/docs/),
[ecosystem 2026](https://the-agent-report.com/2026/06/hermes-agent-ecosystem-2026-pillar/).
