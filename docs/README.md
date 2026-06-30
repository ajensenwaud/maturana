# Maturana documentation

Start with the [project README](../README.md) for what Maturana is and how to install it. This
directory goes deeper. Two files at the repo root are also worth reading early:

- [`AGENTS.md`](../AGENTS.md) — the orientation file Codex reads; principles, the skill index,
  and the architecture in the project's own words.
- [`skills/`](../skills) — the skill workflows that are Maturana's primary surface. Each
  `skills/<name>/SKILL.md` is an operational playbook.

## Recommended reading order

1. [../README.md](../README.md) — overview, install, first agent.
2. [maturana-spec.md](maturana-spec.md) — the `MATURANA.md` agent spec, field by field.
3. [orchestration.md](orchestration.md) — the host runtime plane and `maturana up`.
4. Your platform: [linux-firecracker-harnesses.md](linux-firecracker-harnesses.md) **or**
   [harness-operations.md](harness-operations.md) (Windows / Hyper-V).
5. Then the topical docs below, as needed.

## Reference

| Doc | What it covers |
| --- | --- |
| [maturana-spec.md](maturana-spec.md) | Complete `MATURANA.md` field reference with worked examples. |
| [orchestration.md](orchestration.md) | The runtime plane (sessiond, channels, schedules), `maturana up`, and the leased/dead-lettered session queue. |
| [personal-agent-mvp.md](personal-agent-mvp.md) | Per-agent file layout, durable memory, channels, and the session-runner boundary. |

## Operating a fleet

| Doc | What it covers |
| --- | --- |
| [linux-firecracker-harnesses.md](linux-firecracker-harnesses.md) | Build images, launch/stop, refresh workers, and zero-touch reboot recovery on Linux. |
| [harness-operations.md](harness-operations.md) | Windows / Hyper-V operations: `doctor`, repair, key files, rules of thumb. |
| [snapshot-operations.md](snapshot-operations.md) | Take, list, and restore agent snapshots (Firecracker + Hyper-V). |

## Security & secrets

| Doc | What it covers |
| --- | --- |
| [pipelock-live-verification.md](pipelock-live-verification.md) | Repeatable checks proving the pipelock egress proxy injects secrets and audits TLS interception. |

## Extending Maturana

| Doc | What it covers |
| --- | --- |
| [wasm-tools.md](wasm-tools.md) | The capability-gated WASM tool framework agents use to build their own tools. |
| [plugins.md](plugins.md) | Plugin manifest/discovery contract for first-party and third-party extensions. |
| [self-improvement-rl.md](self-improvement-rl.md) | The RL data flywheel: capture trajectories, reward, curate, redeploy behind a snapshot. |
| [skill-workflows.md](skill-workflows.md) | The required shape for a Maturana skill (grounding, preflight, evidence, recovery, boundaries). |
| [script-boundary.md](script-boundary.md) | The "Rust owns decisions, scripts are leaf adapters" rule and how scripts are classified. |

## Background

| Doc | What it covers |
| --- | --- |
| [mvp-plan.md](mvp-plan.md) | Historical milestone plan — useful for context, not a current guide. |
| [web-cockpit.md](web-cockpit.md) | The browser cockpit. **Currently disabled** — kept for reference; not part of the supported surface right now. |
