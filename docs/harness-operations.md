# Harness operations

Maturana's Windows Hyper-V path has four moving parts:

- `maturana hostd serve`: exposes fixed Hyper-V lifecycle endpoints.
- `maturana session serve`: host-side session bridge used by guest workers.
- `maturana channel serve telegram`: one Telegram runner per agent.
- `/opt/maturana/bin/run-agent.sh`: guest worker loop that calls Codex, Claude Code, or OpenCode inside the VM.

Keep the model simple: channels receive messages, `sessiond` stores turns, guest workers run the selected harness, channels deliver replies.

Hyper-V launch is a synchronous operator action: Rust renders the launch
artifacts, hostd invokes the fixed Ubuntu launcher, and the CLI receives the
launcher result directly. Hostd does not queue generic jobs or execute arbitrary
commands.

`maturana agent run` follows that same path: it enqueues a CLI message into the
agent session and optionally waits for the matching outbound response. It does
not write `/agent/prompt.txt`, write `/agent/run-command`, or restart the guest
service for normal turns.

## Launch your first agent

`install.ps1` already provisioned hostd, the Ubuntu VHDX, the agent SSH key, and
the `up` boot service — so launching an agent is just:

```powershell
cd ~\maturana

# 1. Stage harness auth (Codex shown; Claude Code → host-auth\claude-code).
mkdir .maturana\host-auth -Force
Copy-Item -Recurse ~\.codex .maturana\host-auth\codex

# 2. The installer already started the runtime plane — confirm it.
maturana service status up

# 3. Validate, then launch the bundled demo (id codex-demo).
maturana spec validate examples\MATURANA.codex-hyperv.md
maturana agent launch examples\MATURANA.codex-hyperv.md --apply

# 4. Talk to it.
maturana agent run codex-demo --prompt "say hi" --wait
```

The CLI talks to hostd (running as SYSTEM), which creates and starts the Hyper-V
VM; Rust then provisions the guest worker, injects auth, and starts the in-guest
service over SSH. Re-launching an existing VM needs an explicit override — set
`$env:MATURANA_HYPERV_FORCE = "true"` before `--apply`. A second bundled spec,
`examples\MATURANA.claude-hyperv.md` (id `claude-demo`), runs the Claude Code
harness.

## Health

Run:

```powershell
maturana doctor
```

The doctor checks:

- hostd health and VM/IP state
- sessiond health
- Telegram pairing, runner PID, and channel heartbeat
- guest worker heartbeat reported through `sessiond`

Use JSON for automation:

```powershell
maturana doctor --json
```

Inspect one live VM and, when needed, its guest harness state:

```powershell
maturana agent inspect codex-demo --live
maturana agent inspect codex-demo --live --guest
```

`--guest` runs the narrow SSH diagnostic from Rust: harness versions, systemd
state, heartbeat, last message, agent log tail, and browser smoke output when
`browser.headless_chrome: true` is set in the materialized spec.

## Repair

Restart the local session bridge, refresh guest workers, start Telegram channel
runners, and run doctor:

```powershell
maturana setup windows-harnesses
```

Install persistent scheduled tasks instead of direct background processes:

```powershell
maturana setup windows-harnesses --register-tasks
```

The repair command does not rebuild VMs. It owns the repair decision path and
guest worker rendering in Rust, then uses narrow host adapters only for task
registration.

## Files

- Sessiond PID: `.maturana/sessiond/runner.pid`
- Telegram PID: `.maturana/agents/<agent-id>/channels/telegram/runner.pid`
- Telegram heartbeat: `.maturana/agents/<agent-id>/channels/telegram/heartbeat.json`
- Worker heartbeat: `.maturana/agents/<agent-id>/worker-status.json`
- Agent audit: `.maturana/audit/<agent-id>.jsonl`

## Rule of Thumb

If Telegram does not reply, run `maturana doctor` first, then
`maturana agent inspect <agent-id> --live --guest`. If the VM is running but
Telegram or worker heartbeat is stale, run
`maturana setup windows-harnesses`. If the VM itself is missing or has no IP,
relaunch that VM from its `MATURANA.md`.
