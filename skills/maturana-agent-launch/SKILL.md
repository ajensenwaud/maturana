# maturana-agent-launch

Use this skill when a user wants to materialize or launch a Maturana agent from
`MATURANA.md`.

This skill is the controlled path from spec to live VM. It should inspect,
validate, materialize, apply, and verify. Do not skip straight to live launch.

## Grounding

1. Read `AGENTS.md` first.
2. Read the source `MATURANA.md` and identify:
   - `agent.id`
   - guest harness: `codex`, `claude-code`, or `opencode`
   - VM provider: `hyperv` or `firecracker`
   - filesystem mounts
   - egress and credential policy
   - channels and schedules
3. Check whether `.maturana/agents/<agent-id>` already exists and whether this
   is a create, update, or forced replacement.

## Preflight

- Confirm the spec path is the one the user intends to launch.
- Confirm `maturana spec validate <spec>` is expected to pass before applying.
- Confirm required host prerequisites exist:
  - Windows: hostd scheduled task, Ubuntu VHDX, SSH key, and Hyper-V enabled.
  - Linux: Firecracker binary, `/dev/kvm`, TAP device, kernel, and rootfs.
- Confirm harness auth source exists for Codex/Claude/OpenCode OAuth-based
  runtimes before launch.
- Confirm a replacement launch is intentional before setting force flags.

## Decision Path

- Validate the spec first:

   ```powershell
   maturana spec validate MATURANA.md
   ```

- Materialize a dry-run launch plan before changing live state:

   ```powershell
   maturana agent launch MATURANA.md
   ```

- Inspect `.maturana/agents/<agent-id>/launch-plan.json`.
- Check provider-specific preflight below.
- Use `--apply` only when the host setup is ready.
- Verify with provider-aware live inspect.

## Actions

Use the Rust CLI as the control path:

```powershell
maturana spec validate MATURANA.md
maturana agent launch MATURANA.md
maturana agent launch MATURANA.md --apply
maturana agent inspect <agent-id> --live
```

Use direct host scripts only as leaf adapters after Rust has rendered the agent
state and the user explicitly needs host-specific debugging.

## Evidence

Before declaring launch successful, collect:

- successful `maturana spec validate`
- generated `launch-plan.json`
- live inspect output showing the expected provider and running state
- harness files present in the guest or materialized workspace
- if `browser.headless_chrome: true`, rendered `install-harness.sh` contains
  Playwright Chromium provisioning and the guest has
  `/opt/maturana/bin/browser-smoke.js`
- recent audit event for launch or inspect
- channel/session heartbeat when this is an always-on personal agent

## Windows Hyper-V

Prepare the official Ubuntu VHDX and SSH key once, then reuse them:

```powershell
.\scripts\install.ps1
```

Launch through the Rust CLI. The CLI talks to Rust hostd; hostd performs
the privileged Hyper-V work.

```powershell
$env:MATURANA_HYPERV_FORCE = "true" # only when replacing an existing demo VM
.\scripts\maturana.ps1 agent launch .\examples\MATURANA.codex-hyperv.md --apply
```

For debugging only, the direct elevated launcher can create/start the Hyper-V
VM after Rust has materialized cloud-init state. It does not provision the guest
worker, install harnesses, inject auth, or start the systemd service:

```powershell
.\scripts\launch-ubuntu-cloudimg-hyperv.ps1 `
  -AgentId codex-demo `
  -BaseVhdxPath .\.maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx `
  -SshUser ubuntu `
  -SshKeyPath .\.maturana\keys\maturana-agent-ed25519 `
  -CloudInitUserDataPath .\.maturana\agents\codex-demo\state\cloud-init\user-data `
  -CloudInitMetaDataPath .\.maturana\agents\codex-demo\state\cloud-init\meta-data `
  -Force
```

Rust hostd may be used to inspect VM state, but launch is synchronous and
direct. OAuth auth state is copied from `.maturana/host-auth/codex` or
`.maturana/host-auth/claude-code` into the VM. Rust renders cloud-init
`user-data`/`meta-data`, `sessiond.env`, `run-agent.sh`, the fixed systemd
service, and `proxy.env` when pipelock proxying is enabled. The PowerShell
launcher packages the seed VHDX and starts the VM only; Rust provisions guest
files over SSH after the VM reports an IP.

Hostd exposes fixed Maturana VM operations only. Do not add a generic command
execution endpoint. Privileged hostd operations require the token in
`.maturana/hostd/token`; the CLI reads this token automatically.

Inspect live VM state through the CLI when hostd is running:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

Submit a new live task to the guest harness through the session queue:

```powershell
.\scripts\maturana.ps1 agent run codex-demo --prompt "Inspect /agent/MATURANA.md and report status." --wait
```

`agent run` enqueues a CLI message into `sessiond` and waits for the matching
outbound response when `--wait` is set. Do not revive `/agent/prompt.txt`,
`/agent/run-command`, or host-side SSH prompt execution for normal runs.

Push inputs into the guest workspace with `agent push`:

```powershell
.\scripts\maturana.ps1 agent push codex-demo .\.maturana\agents\codex-demo\workspace\host-input.txt /workspace/host-input.txt --ip 172.26.183.108
```

Fetch artifacts from the guest workspace with `agent fetch`:

```powershell
.\scripts\maturana.ps1 agent fetch codex-demo /workspace/live-run.txt .\.maturana\agents\codex-demo\workspace\live-run.txt --ip 172.26.183.108
```

Use `agent push`, `agent run`, and `agent fetch` as the boring, direct work
loop. Do not add a queue for guest command execution.

Guest transfer paths for `agent push` and `agent fetch` are limited to
`/workspace`, `/memory`, `/wiki`, and declared
`filesystem.mounts[*].guest_path` roots.

Each successful live push, run, and fetch appends an event to
`.maturana/audit/<agent-id>.jsonl`.

## Linux Firecracker

On `aidev`, use specs with `vm.provider: firecracker` and explicit
`vm.firecracker.kernel_image`, `rootfs_image`, `tap_name`, `host_ip`, and
`guest_ip`. The spec is the source of truth for Firecracker addressing; do not
copy IP state into ad hoc scripts unless overriding a broken materialized spec.

For the standard three harnesses on `aidev`, use the Rust-owned repair/deploy
workflow:

```bash
maturana setup firecracker-harnesses
maturana setup firecracker-harnesses --agent-id codex-firecracker
```

That command starts `sessiond`, calls the narrow TAP/image-prep adapters,
materializes and applies the specs, waits for SSH, and refreshes the guest
worker. The old `scripts/deploy-aidev-firecracker-harnesses.sh` compatibility
wrapper has been removed. Use the Rust command directly.

```bash
sudo apt-get install -y qemu-utils libguestfs-tools
maturana setup firecracker-harnesses --agent-id codex-firecracker
maturana spec validate examples/MATURANA.firecracker-demo.md
maturana agent launch examples/MATURANA.firecracker-demo.md
maturana agent launch examples/MATURANA.firecracker-demo.md --apply
maturana agent inspect firecracker-demo --live
maturana agent stop firecracker-demo --live
```

The provider writes `firecracker-config.json` and metadata under the agent state
directory. Rust owns Firecracker lifecycle decisions: idempotent start,
provider-aware live inspect, pid tracking, Firecracker binary and `/dev/kvm`
validation, kernel/rootfs validation, TAP validation, guest IP reporting,
API readiness, stale-state cleanup, and stop. If a recorded Firecracker PID is
still running, launch must preserve the socket and prove the API is reachable.
If the PID is running but the socket is missing, stop or repair the agent
explicitly before relaunching. Before launch trusts generated state, Rust
checks that the config and metadata still match the current spec and that
metadata paths remain inside the agent state directory. If that check fails,
regenerate the plan from the spec; do not patch state JSON by hand. Keep image
building and TAP creation out of launch; those are host setup adapters.

Rust renders the guest worker files, netplan, cloud network-disable config, and
proxy env when the spec enables pipelock proxying.
`scripts/firecracker-prepare-assets.sh` is only an image customization/export
adapter that copies Rust-rendered files into the image.

## Recovery

- Validation fails: fix the spec. Do not bypass the validator.
- Existing Hyper-V demo VM blocks launch: use `MATURANA_HYPERV_FORCE=true` only
  when replacing that agent is intended.
- Hostd is not reachable: install or start the fixed hostd task, then retry.
- Hyper-V launch succeeds but no IP appears: inspect VM integration services and
  cloud-init logs before recreating the VM.
- Firecracker prereq check fails: fix the host setup, TAP, kernel, or rootfs.
- Firecracker stale pid/socket: inspect first. If the PID is running but the
  socket is missing, run `maturana agent stop <agent-id> --live` or repair the
  agent before relaunching. If the PID is stale, relaunch can clean it.
- Harness auth missing: inject Codex or Claude OAuth state directly into the
  guest auth path. Do not move OAuth into pipelock.

## Boundaries

- Do not implement launch orchestration in PowerShell or bash.
- **Do not provision the guest by hand.** Installing the proxy CA, injecting
  Codex/Claude auth, installing the harness/browser, bootstrapping directories,
  copying the agent contract, and enabling/starting `maturana-agent.service` are
  ALL done by `maturana agent launch … --apply` (Rust: `provision_hyperv_guest` /
  `install_guest_worker`). Never SSH into the guest to run apt/cp/systemctl
  yourself, and never author a PowerShell/bash wrapper to do it. If `--apply`
  fails, report its exact error and stop — do not reimplement provisioning.
- Do not add a queue or broker for command execution.
- Do not add generic host command execution to hostd.
- Do not copy host directories into the guest unless the spec declares them.
- Do not store raw secrets in specs, docs, skills, or audit logs.
