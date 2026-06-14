# maturana-hostd

Use this skill when a user wants to check, install, diagnose, or reason about
the Windows Rust hostd daemon (`maturana hostd serve`).

Hostd is a narrow privileged VM lifecycle daemon. It exists so normal Codex
host operations can call fixed Hyper-V actions without repeated ad hoc UAC
prompts. It is not a general command runner.

## Grounding

1. Read `AGENTS.md` first.
2. Identify whether the request is status, installation, Hyper-V launch/stop,
   snapshot operation, or debugging.
3. Check hostd status from the Rust CLI before touching scheduled tasks:
   - `maturana hostd status`
   - `maturana hostd status --json`
4. Inspect `.maturana/hostd/token` and `.maturana/logs/hostd.log` when hostd is
   unreachable.
5. Inspect the target agent `MATURANA.md` before using hostd for VM lifecycle.

## Preflight

- Confirm the host is Windows and the operation truly requires Hyper-V.
- Check `maturana hostd status --json` before installing or restarting tasks.
- Confirm the fixed token path exists and is not printed.
- Confirm the request maps to a fixed hostd endpoint, not arbitrary command
  execution.
- Confirm hostd logs are inspected before editing or replacing scripts.

## Decision Path

- Hostd reachable: use normal Rust CLI commands for launch, inspect, stop, and
  Hyper-V snapshots.
- Hostd unreachable: install or restart the fixed Windows task once through
  `install-hostd-task.ps1` (or re-run `install.ps1`); then retry the Rust CLI
  command.
- Hyper-V operation failed: inspect hostd response, log file, Hyper-V state,
  and the materialized launch plan before editing scripts.
- Guest command requested: do not add a hostd command endpoint. Use
  `maturana agent run` through sessiond.
- Firecracker request: hostd is not involved; use Linux/aidev provider paths.

## Actions

Check hostd from a normal shell:

```powershell
.\scripts\maturana.ps1 hostd status
```

Use JSON output when another tool or script needs structured status:

```powershell
.\scripts\maturana.ps1 hostd status --json
```

If `reachable` is false, install or restart the elevated scheduled task:

```powershell
.\scripts\install-hostd-task.ps1
```

Then retry the original Rust CLI command, for example:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

## Evidence

Before claiming success, collect:

- `hostd status` shows reachable.
- The hostd URL and token path are the expected local values.
- `.maturana/logs/hostd.log` has no fresh fatal error.
- The intended agent operation succeeds through the Rust CLI.
- For launch/stop/snapshot operations, `.maturana/audit/<agent-id>.jsonl`
  records the governed action.

## Recovery

- Scheduled task missing: run `install-hostd-task.ps1` from an elevated shell once.
- Health endpoint unreachable: inspect `.maturana/logs/hostd.log`, then restart
  the fixed scheduled task.
- Token mismatch: regenerate/install hostd through the Windows installer rather
  than hard-coding tokens.
- Hyper-V cmdlet error: fix Hyper-V state or image paths; do not bypass hostd
  with a new orchestration script.
- Repeated UAC prompts: ensure the scheduled task is installed and use the Rust
  CLI path afterward.

## Boundaries

- Do not add generic command execution to hostd.
- Do not add queues or brokers to hostd.
- Do not move guest harness turns into hostd.
- Do not store raw secrets in hostd logs or request bodies.
- Do not use hostd for Firecracker.
- Do not make PowerShell own orchestration decisions; hostd may call narrow
  Hyper-V adapters only.
