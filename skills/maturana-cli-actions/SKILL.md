# maturana-cli-actions

Use this skill when operating Maturana from Codex on the host.

This skill is the host-operator router. It is not a command catalog by itself:
first identify the user's intent and the current agent state, then choose the
smallest Maturana command that changes or proves that state.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the target agent id and read
   `.maturana/agents/<agent-id>/MATURANA.md` when it exists.
3. If the user gave a spec path, read that spec before touching materialized
   state.
4. Check current state before mutation:
   - `maturana doctor --agent-id <agent-id> --json` when the agent exists
   - `maturana agent inspect <agent-id> --live` for VM state
   - `maturana audit list <agent-id> --json` for recent governed operations
   - session, channel, heartbeat, or schedule files when the request concerns
     personal-agent behavior

## Preflight

- Confirm the requested operation maps to an existing Maturana CLI command.
- Validate specs before launch or update.
- Inspect provider state before stop, restore, repair, or guest diagnostics.
- Prefer provider-discovered IP addresses over manually supplied IPs.
- Confirm the command will not print or persist raw secrets.

## Decision Path

- Spec change: use `maturana spec validate <spec>` first. Fix validation
  failures before launch or update.
- VM lifecycle: use `maturana agent launch|inspect|stop`. Rust owns provider
  decisions; PowerShell and bash are leaf adapters only.
- Guest task: use `maturana agent run <agent-id> --prompt ... --wait`. This
  enqueues through `sessiond`; do not use SSH as the normal execution path.
- File transfer: use `agent push` or `agent fetch`, and only within declared
  guest roots such as `/workspace`, `/memory`, `/wiki`, `/agent/skills`, or
  `/agent/tools`.
- Personal agent channel: inspect pairing, heartbeat, transcript, context
  manifest, session inbox/outbox, and runner logs before changing code.
- Snapshot: use `maturana snapshot list|take|restore`; local markers are not
  restorable, live restore requires `--live`.
- Firecracker repair: prefer `maturana repair firecracker-harnesses` for the
  known Linux harness set, then inspect live state.
- Hyper-V repair: prefer `maturana repair windows-harnesses`; one UAC/elevated
  setup is acceptable, repeated ad hoc elevated scripts are not.
- Tool/skill development: build and test on the Codex host, then deploy with
  `maturana deploy skill|tool`.
- Secrets: use pipelock for non-OAuth credentials. Inject Codex and Claude
  OAuth state directly into VMs.

## Actions

Common commands:

- Validate specs: `maturana spec validate <spec> [--json]`
- Materialize or launch: `maturana agent launch <spec> [--apply]`
- Inspect: `maturana agent inspect <agent-id> [--live] [--guest]`
- Stop: `maturana agent stop <agent-id> --live`
- Run: `maturana agent run <agent-id> --prompt "<prompt>" --wait`
- Logs: `maturana agent logs <agent-id> --kind agent|error|stdout|stderr|last-message`
- Fetch: `maturana agent fetch <agent-id> <remote> <local> --ip <ip>`
- Push: `maturana agent push <agent-id> <local> <remote> --ip <ip>`
- Snapshots: `maturana snapshot list|take|restore <agent-id> ...`
- Audit: `maturana audit list <agent-id> [--json]`
- Hostd: `maturana hostd status [--json]`
- Pipelock: `maturana pipelock init|set|get|list|delete|ca-cert|proxy`
- Notify: `maturana notify telegram|discord ...`
- Personal agent: `maturana personal init <agent-id>`
- Wiki: `maturana wiki init|ingest|search`
- Heartbeat: `maturana heartbeat beat|status <agent-id>`
- Schedule: `maturana schedule add|list|run-due|serve <agent-id>`
- Deploy: `maturana deploy skill|tool <agent-id> <path> --ip <ip>`
- Develop: `maturana develop skill|tool <name>`
- Channel: `maturana channel pair|serve|status`
- Session: `maturana session init|enqueue|run-once|outbox|serve`
- Doctor: `maturana doctor [--agent-id <id>] [--json]`
- Repair: `maturana repair windows-harnesses|firecracker-harnesses`

## Evidence

Before claiming success, collect the evidence that matches the action:

- Spec: clean `maturana spec validate` output.
- Launch: generated `launch-plan.json` and live inspect showing running state.
- Guest turn: matching session outbox row or `agent run --wait` response.
- Channel turn: paired chat id, heartbeat, transcript, context manifest, and
  outbox delivery state.
- Snapshot: `snapshot.json`, provider live list, and restore command output
  when restore was requested.
- Deploy: destination path plus guest-side listing or harness-visible file.
- Repair: before/after inspect output and recent audit entry.

## Recovery

- Validation fails: fix the spec, not the validator.
- Hostd unreachable: install/start the fixed hostd task once, then retry the
  Rust CLI command.
- Firecracker PID/socket stale: inspect live state, then `maturana agent stop
  <agent-id> --live` before relaunch.
- Guest does not answer: inspect session inbox/outbox, runner heartbeat, and
  harness logs before restarting anything.
- Telegram does not answer: inspect pair status, heartbeat, transcript,
  context manifest, session outbox, and delivery errors.
- Missing guest IP: use provider inspect first; use `--ip` only as an explicit
  recovery override.

## Boundaries

- Do not put OAuth credentials into pipelock. Inject Codex and Claude OAuth
  state directly into VMs.
- Do not add a generic command queue or broker for guest execution.
- Do not add generic host command execution to hostd.
- Do not implement provider state machines in PowerShell or bash.
- Do not restart services blindly.
- Do not copy host directories into the guest unless the spec declares them.
- Do not paste, print, commit, or audit raw secrets.
