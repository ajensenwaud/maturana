# maturana-hostd

Use this skill when a user wants to check or diagnose the Windows
`maturana-hostd` daemon.

## Procedure

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
.\scripts\install-windows.ps1
```

Hostd remains a narrow privileged VM lifecycle daemon. Do not add generic
command execution or queues to it.
