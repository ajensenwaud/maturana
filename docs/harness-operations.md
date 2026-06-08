# Harness operations

Maturana's Windows MVP has four moving parts:

- `maturana-hostd.ps1`: tracks and launches Hyper-V VMs.
- `maturana session serve`: host-side session bridge used by guest workers.
- `maturana channel serve telegram`: one Telegram runner per agent.
- `/opt/maturana/bin/run-agent.sh`: guest worker loop that calls Codex, Claude Code, or OpenCode inside the VM.

Keep the model simple: channels receive messages, `sessiond` stores turns, guest workers run the selected harness, channels deliver replies.

## Health

Run:

```powershell
.\target\x86_64-pc-windows-msvc\debug\maturana.exe doctor
```

The doctor checks:

- hostd health and VM/IP state
- sessiond health
- Telegram pairing, runner PID, and channel heartbeat
- guest worker heartbeat reported through `sessiond`

Use JSON for automation:

```powershell
.\target\x86_64-pc-windows-msvc\debug\maturana.exe doctor --json
```

## Repair

Restart the local session bridge and all configured Telegram channel runners:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\repair-windows-harnesses.ps1
```

Install persistent scheduled tasks instead of direct background processes:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\repair-windows-harnesses.ps1 -RegisterTasks
```

The repair script does not rebuild VMs. It only restarts host-side harness plumbing.

## Files

- Sessiond PID: `.maturana/sessiond/runner.pid`
- Telegram PID: `.maturana/agents/<agent-id>/channels/telegram/runner.pid`
- Telegram heartbeat: `.maturana/agents/<agent-id>/channels/telegram/heartbeat.json`
- Worker heartbeat: `.maturana/agents/<agent-id>/worker-status.json`
- Agent audit: `.maturana/audit/<agent-id>.jsonl`

## Rule of Thumb

If Telegram does not reply, run `maturana doctor` first. If the VM is running but Telegram or worker heartbeat is stale, run `repair-windows-harnesses.ps1`. If the VM itself is missing or has no IP, relaunch that VM from its `MATURANA.md` or launch script.
