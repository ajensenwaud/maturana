# Maturana Skill Workflow Pattern

Maturana skills are the main product surface. They should be operational
playbooks, not thin aliases for CLI commands.

Use this pattern when creating or reviewing a skill.

Validate the bundled skill pack with:

```powershell
.\scripts\maturana.ps1 skill validate skills
```

CI runs the same validator so skill workflow drift fails the normal local gate.

The validator enforces more than headings. A bundled skill must include at
least four concrete evidence bullets, at least four recovery bullets, and at
least three explicit `Do not` boundary bullets. This is intentionally mechanical:
it blocks heading-only command wrappers from looking like real workflows.

The validator also enforces the named initial skill contract from `AGENTS.md`:
agent create, validate, launch, inspect, update, skill create, tool create,
skill deploy, security review, and snapshot. Adjacent helper skills may exist,
but these names must remain present because Codex uses them as the stable
product surface.

## Required Shape

Every skill should include:

- **Intent:** the user situations that trigger the skill.
- **Grounding:** the files, specs, logs, and live state to read before acting.
- **Preflight:** cheap idempotent checks that prevent repeated failed work.
- **Decision path:** how to choose provider, harness, channel, snapshot, or
  repair behavior from current state.
- **Actions:** the smallest commands or tool calls needed to change state.
- **Evidence:** the concrete output that proves the action worked.
- **Recovery:** known failure modes and the simple repair path for each.
- **Boundaries:** what the skill must not do.

Do not use a catch-all `## Procedure` section. A skill with one generic
procedure tends to become a CLI wrapper; split the workflow into the required
sections above so grounding, preflight, evidence, and recovery remain explicit.

## Design Rules

- Read `AGENTS.md` first and preserve the KISS architecture.
- Prefer Rust CLI or Rust hostd for stateful decisions.
- Use PowerShell and bash only as host-specific adapters.
- Keep hostd fixed-purpose. Do not add generic host command execution.
- Do not introduce queues for guest command execution.
- Do not restart services blindly. Inspect session state, heartbeat, logs, and
  provider status first.
- Do not bypass validation; fix specs and state so validation passes.
- Do not paste or commit raw secrets.
- Treat OAuth harness credentials as direct guest auth injection, not pipelock
  secrets.

## Evidence Checklist

When a skill claims success, it should identify at least one authoritative
piece of evidence:

- `maturana spec validate` success for spec changes.
- `.maturana/agents/<agent-id>/launch-plan.json` for materialization.
- `maturana agent inspect <agent-id> --live` for live VM state.
- `.maturana/audit/<agent-id>.jsonl` for governed operations.
- `.maturana/agents/<agent-id>/sessions/...` for channel/session flow.
- Snapshot metadata plus restore test output for snapshot behavior.
- Rust test output for library or CLI behavior.
- Linux `aidev` output for Firecracker changes.

## Recovery Style

Recovery should be direct and boring:

- stale Firecracker pid: `maturana agent stop <id> --live`, then relaunch.
- Hyper-V hostd unreachable: install or start the fixed hostd task once.
- no guest IP: inspect provider state before switching to SSH diagnostics.
- channel does not reply: inspect inbound, outbound, heartbeat, and runner logs
  before changing bot code.
- scheduler missed a run: inspect schedule records and last-run state before
  editing schedules.

If a recovery path needs a new host primitive, add the smallest Rust-facing
operation and keep the script side as a leaf adapter.
