# maturana-agent-inspect

Use this skill when a user wants to inspect a materialized Maturana agent.

## Procedure

Run:

```powershell
.\scripts\maturana.ps1 agent inspect <agent-id>
```

Check the materialized `MATURANA.md`, generated `AGENTS.md`, `SOUL.md`,
`launch-plan.json`, audit log, and snapshot directory.

## Live Windows Guest

When `maturana-hostd` is running, inspect Hyper-V state through the CLI:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

If the current shell cannot use hostd or Hyper-V discovery, pass the guest IP
and inspect the VM directly over SSH:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live --ip 172.26.183.108
```

The SSH-backed inspect prints the guest hostname, Codex binary/version, systemd
service state, root filesystem size, heartbeat, and last harness message. Use it
for quick health checks after launch or `maturana agent run`.

Read known guest logs with:

```powershell
.\scripts\maturana.ps1 agent logs codex-demo --ip 172.26.183.108 --kind agent --lines 80
.\scripts\maturana.ps1 agent logs codex-demo --ip 172.26.183.108 --kind last-message
```

Allowed log kinds are `agent`, `error`, `stdout`, `stderr`, and
`last-message`. The command reads only known Maturana guest logs under
`/var/log/maturana`.

Live inspect and log reads append events to `.maturana/audit/<agent-id>.jsonl`.

Read recent audit events with:

```powershell
.\scripts\maturana.ps1 audit list codex-demo --limit 10
```
