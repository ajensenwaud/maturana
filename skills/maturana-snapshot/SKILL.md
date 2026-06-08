# maturana-snapshot

Use this skill when a user wants to take or list Maturana snapshots.

## MVP Procedure

List snapshot markers:

```powershell
.\scripts\maturana.ps1 snapshot list <agent-id>
```

Create an MVP snapshot marker:

```powershell
.\scripts\maturana.ps1 snapshot take <agent-id> <snapshot-name>
```

When `maturana-hostd` is running on Windows, use live Hyper-V checkpoints:

```powershell
.\scripts\maturana.ps1 snapshot take <agent-id> <snapshot-name> --live
.\scripts\maturana.ps1 snapshot list <agent-id> --live
.\scripts\maturana.ps1 snapshot restore <agent-id> <snapshot-name> --live
```

Live snapshot operations are fixed hostd operations against `maturana-*` VMs.
Do not implement snapshotting through a generic command runner.

Successful live snapshot list, take, and restore operations append events to
`.maturana/audit/<agent-id>.jsonl`.
