# maturana-agent-inspect

Use this skill when a user wants to inspect a materialized Maturana agent,
debug a slow or silent agent, verify VM lifecycle state, or gather evidence
before repair.

This is a state-aware workflow. Do not treat it as a single command wrapper.

## Grounding

1. Read `AGENTS.md` and preserve the KISS architecture.
2. Identify the agent id from the user request, `MATURANA.md`, or
   `.maturana/agents/<agent-id>`.
3. Read the materialized files first:
   - `.maturana/agents/<agent-id>/MATURANA.md`
   - `.maturana/agents/<agent-id>/AGENTS.md`
   - `.maturana/agents/<agent-id>/SOUL.md`
   - `.maturana/agents/<agent-id>/launch-plan.json`
   - `.maturana/audit/<agent-id>.jsonl`
4. Infer provider and harness from the spec and launch plan before choosing a
   live diagnostic path.

## Preflight

- Confirm the materialized agent directory exists before live diagnostics.
- Confirm the user is asking for local state, provider state, guest state, or
  channel/session behavior.
- Check whether hostd is reachable for Hyper-V or whether Firecracker metadata
  exists for Linux before deeper live inspection.
- Do not restart, stop, relaunch, or repair anything during preflight.

## Decision Path

- Start with local inspect. It is cheap, does not require elevation, and catches
  missing materialization, stale specs, snapshots, and audit state.
- Use provider-aware live inspect next when VM state matters.
- Add `--guest` only when the symptom is inside the guest harness or session
  runner.
- For channel symptoms, inspect pairing, transcript, session inbox/outbox,
  heartbeat, context manifest, and delivery state before changing harness code.
- For Firecracker symptoms, trust Rust provider inspect; do not call deleted
  shell lifecycle scripts.
- For Hyper-V symptoms, go through Rust hostd/CLI; use direct launcher
  scripts only as leaf debugging adapters after Rust has materialized state.

## Actions

Use the smallest inspect action that matches the symptom:

- Local materialized state: `maturana agent inspect <agent-id>`
- Provider state: `maturana agent inspect <agent-id> --live`
- Guest harness state: `maturana agent inspect <agent-id> --live --guest`
- Logs: `maturana agent logs <agent-id> --kind agent|error|stdout|stderr|last-message`
- Audit: `maturana audit list <agent-id> --limit 10`

## Local Inspect

Run local inspect first. It is cheap, does not require elevation, and catches
missing materialization, stale specs, snapshots, and audit state.

```powershell
maturana agent inspect <agent-id>
```

## Evidence

Evidence to collect across local/live/guest inspect:

- materialized spec path
- generated behavior files
- launch plan provider and harness
- snapshot directory entries
- last audit events
- provider live state
- Firecracker pid/socket/config/metrics tail when provider is Firecracker
- Hyper-V VM name/IP/integration state when provider is Hyper-V
- guest heartbeat, systemd status, harness version, and recent logs when
  `--guest` is used
- browser smoke output when `browser.headless_chrome: true` and `--guest` is
  used

## Live Inspect

Use provider-aware live inspect next:

```powershell
maturana agent inspect <agent-id> --live
```

Expected live inspect details:

- provider: `hyperv` or `firecracker`
- live state: running, running-api-unresponsive, running-missing-socket,
  untracked-api-socket, stale-socket, stopped, missing, or stale
- pid/socket/config for Firecracker
- VM name/IP/integration state for Hyper-V
- recent audit event for the inspect operation

## Windows Hyper-V

When Rust hostd is running, inspect Hyper-V state through the CLI:

```powershell
maturana agent inspect codex-demo --live
```

To include guest health, harness versions, systemd status, heartbeat, last
message, agent log tail, and browser smoke output when configured, ask Rust to
run the governed SSH diagnostic:

```powershell
maturana agent inspect codex-demo --live --guest
```

If hostd cannot discover the guest IP but the VM is running, pass a known guest
IP and inspect directly over SSH:

```powershell
maturana agent inspect codex-demo --live --guest --ip 172.26.183.108
```

The SSH-backed inspect prints the guest hostname, harness binary/version,
systemd service state, root filesystem size, heartbeat, last harness message,
and `live.browser_smoke_output` when `browser.headless_chrome: true`. Use it for
guest health, not as a replacement for provider inspect.

Read known guest logs with:

```powershell
maturana agent logs codex-demo --ip 172.26.183.108 --kind agent --lines 80
maturana agent logs codex-demo --ip 172.26.183.108 --kind last-message
```

Allowed log kinds are `agent`, `error`, `stdout`, `stderr`, and
`last-message`. The command reads only known Maturana guest logs under
`/var/log/maturana`.

Live inspect and log reads append events to `.maturana/audit/<agent-id>.jsonl`.

Read recent audit events with:

```powershell
maturana audit list codex-demo --limit 10
```

## Linux Firecracker

Firecracker live inspect is owned by Rust, not shell scripts:

```bash
maturana agent inspect firecracker-demo --live
```

Check:

- `live.provider` is `firecracker`
- `live.ipv4` matches `vm.firecracker.guest_ip`
- pid exists and the process is alive
- API socket path exists
- `firecracker-config.json` references the expected kernel, rootfs, and TAP
- config and metadata still match the current spec; Rust live inspect fails
  instead of reporting stale or tampered generated state as healthy
- metadata and metrics tails do not show repeated start failures

Do not call deleted Firecracker shell lifecycle scripts. Rust owns launch,
stop, and inspect. Image prep and TAP setup remain host adapters.

## Channel And Session Debugging

When the symptom is "the bot does not reply" or "the agent is slow", inspect the
session boundary before changing harness code:

1. Check channel pairing and latest inbound transcript/session files.
2. Check session inbox/outbox under `.maturana/agents/<agent-id>/sessions`.
3. Check heartbeat freshness.
4. Check runner logs and recent harness stderr/stdout.
5. Only restart the fixed service after the evidence points to a stale process.

Do not send placeholder "working on it" messages as a repair. Telegram should
use chat action activity indicators and then deliver the final reply.

## Recovery

- Missing materialized agent: validate the source spec, then run launch without
  `--apply` to regenerate the plan.
- Hyper-V hostd unreachable: install/start the fixed hostd scheduled task once,
  then rerun live inspect.
- Hyper-V VM running but no IP: inspect integration services and guest logs
  before recreating the VM.
- Firecracker `running-missing-socket`: treat it as a live process with a
  broken control plane. Stop explicitly with `maturana agent stop <agent-id>
  --live`, verify the PID/socket are gone, then relaunch with `--apply`.
- Firecracker `untracked-api-socket`: a responsive API socket exists without a
  Maturana PID file. Do not relaunch or delete the socket. Inspect host process
  state, recover or recreate the PID record if the process belongs to this
  agent, or stop the process intentionally outside the normal provider path only
  after confirming ownership.
- Firecracker `stale-socket`: no PID file exists and the socket does not
  answer. Run `maturana agent stop <agent-id> --live` to let the provider clean
  the stale socket, then relaunch.
- Firecracker `running-api-unresponsive`: treat it as a live process whose API
  socket exists but is not answering. Stop explicitly with `maturana agent stop
  <agent-id> --live`, inspect stderr/metrics if stop fails, then relaunch with
  `--apply`.
- Firecracker stale pid or stale socket: run `maturana agent stop <agent-id>
  --live`, then relaunch with `--apply`.
- Firecracker stop sends TERM and escalates to KILL if needed. Do not remove
  pid/socket files by hand unless the provider cannot prove process state and
  you have independently verified the recorded PID is not running.
- Firecracker missing `/dev/kvm`, TAP, kernel, or rootfs: fix host setup; do
  not hide it behind retries.
- Channel inbound exists but no outbound: inspect runner heartbeat and harness
  logs.
- Outbound exists but user did not receive it: inspect channel delivery and
  audit before touching the harness.

## Boundaries

- Do not add a queue for command execution.
- Do not add a generic hostd command endpoint.
- Do not bypass spec validation.
- Do not paste or commit raw secrets.
- Do not restart services blindly.
- Do not use PowerShell or bash for provider state machines.
- Do not call `hyperv-status.ps1` or `inspect-ubuntu-hyperv-agent.ps1`; use
  `maturana agent inspect --live` and `--guest`.
