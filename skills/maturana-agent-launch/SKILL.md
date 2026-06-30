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
- Linux only — run the launch OUTSIDE the harness sandbox. The launch opens
  `/dev/kvm` and runs `sudo` for the per-agent TAP, which a sandboxed `codex`/
  agent command cannot do. If you see `/dev/kvm does not exist` (or `is not
  visible to this process`) while the host clearly has KVM (`ls -l /dev/kvm`
  works in a plain shell), the sandbox is hiding the device node — re-run the
  launch unsandboxed (a plain shell, or the harness with its sandbox bypassed).
  Do NOT run `kvm-enable.sh`; KVM is already on.
- Linux only — the **very first launch after `install.sh` must be in a fresh
  login shell**. The installer adds you to the `kvm` group and drops the binary
  in `~/.local/bin`; neither applies to the shell that ran the install. Symptoms
  of skipping this: `maturana: command not found` (PATH) or `failed to open
  /dev/kvm: Permission denied` even though `ls -l /dev/kvm` shows the device
  (group not active). Fix without relogin: `newgrp kvm` and
  `. ~/.local/bin/env`. Sanity check before launch: `id` lists `kvm`, and
  `[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo kvm-ok`.
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
maturana agent launch .\examples\MATURANA.codex-hyperv.md --apply
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
maturana agent inspect codex-demo --live
```

Submit a new live task to the guest harness through the session queue:

```powershell
maturana agent run codex-demo --prompt "Inspect /agent/MATURANA.md and report status." --wait
```

`agent run` enqueues a CLI message into `sessiond` and waits for the matching
outbound response when `--wait` is set. Do not revive `/agent/prompt.txt`,
`/agent/run-command`, or host-side SSH prompt execution for normal runs.

Push inputs into the guest workspace with `agent push`:

```powershell
maturana agent push codex-demo .\.maturana\agents\codex-demo\workspace\host-input.txt /workspace/host-input.txt --ip 172.26.183.108
```

Fetch artifacts from the guest workspace with `agent fetch`:

```powershell
maturana agent fetch codex-demo /workspace/live-run.txt .\.maturana\agents\codex-demo\workspace\live-run.txt --ip 172.26.183.108
```

Use `agent push`, `agent run`, and `agent fetch` as the boring, direct work
loop. Do not add a queue for guest command execution.

Guest transfer paths for `agent push` and `agent fetch` are limited to
`/workspace`, `/memory`, `/wiki`, and declared
`filesystem.mounts[*].guest_path` roots.

Each successful live push, run, and fetch appends an event to
`.maturana/audit/<agent-id>.jsonl`.

## Linux Firecracker

**Verified first-run order on a fresh host** (each step is idempotent; do them in
this order):

1. **Host substrate (once):** `bash scripts/install-firecracker-host.sh` —
   installs the Firecracker binary, enables KVM, installs the libguestfs/qemu
   image toolchain, makes `/boot/vmlinuz-*` readable for the non-root image build,
   enables IPv4 forwarding, and grants scoped passwordless sudo for the per-agent
   TAP. Then install the control plane: `bash scripts/install.sh`. Needs sudo.
2. Fresh login shell (see Preflight — `kvm` group + `~/.local/bin` PATH).
3. Stage harness auth into `.maturana/host-auth/<harness>/` (e.g. `claude-code/`,
   `codex/`). For an **opencode** agent the live-LLM credential is the OpenRouter
   key — set `openrouter/api-key` in pipelock (plus `host-auth/opencode/`).
4. `maturana setup firecracker-harnesses --agent-id <profile>` — builds rootfs +
   kernel, creates the TAP, boots Firecracker, installs the guest worker, and —
   when it owns the plane (no `--skip-services`) — starts the agent's egress
   proxy so the guest can reach its allowlisted model API. Run **unsandboxed**.
   Re-runnable: the TAP is recreated fresh each launch, so a repeat never hits
   "Resource busy".
5. `maturana up` (or `maturana service install up`) — sessiond + channels +
   schedules, and supervises the per-agent egress proxies in steady state.
6. Prove a turn: `maturana agent run <profile> --prompt "say hi" --wait`.
7. (Optional) Pair a channel. **One bot per agent.** Two agents sharing the same
   Telegram token collide with a Telegram **409 Conflict** (two pollers, one
   token) and neither answers — give each agent its own bot/token.

A fresh host now needs **no manual workarounds** — the provider pre-creates
Firecracker's logger/metrics files, the image build hands the SSH key back to the
invoking user, and `setup` starts the egress proxy. If you still hit one of those
on an OLD binary, rebuild; do not hand-`touch`/`chown`/start-proxy on the host.

The three ready profiles are `codex-firecracker`, `claude-firecracker`,
`opencode-firecracker` — each owns a UNIQUE network slot (its own `tap_name`,
`host_ip`/`guest_ip`, `guest_mac`) and its own rootfs image.

### A NEW agent needs its OWN slot + OWN rootfs (do not copy a running agent)

Adding an agent that runs **alongside** the existing three is not turnkey yet —
the three profiles are the supported path. To stand up another one correctly:

- **Give it a unique network slot.** Maturana does NOT auto-allocate, and it now
  **rejects** a spec that reuses another agent's `tap_name` / `host_ip` /
  `guest_ip` / `guest_mac` / `rootfs_image` — at both `spec validate` and `agent
  launch`, naming the conflicting agent. So a copied profile fails fast with a
  clear message instead of a cryptic `TapOpen ResourceBusy` deep in Firecracker.
  Assign free values: `tap_name` must be **≤15 chars** (e.g. `tap-mat-<short>` —
  `tap-mat-humberto` is 16 and too long), and an unused host/guest IP pair + MAC
  (the existing pattern is host=odd/guest=even, +4 apart: `.1/.2`, `.5/.6`,
  `.9/.10` → next free `.13/.14`).
- **Never reuse another agent's rootfs image — not even a byte copy.** The guest's
  **SSH host key and network identity are baked into the rootfs at build**, so a
  copy of codex's disk presents codex's host key on the new agent's IP; host↔guest
  SSH then fails the pinned `known_hosts` check even though the VM boots to a login
  prompt. The new agent needs its OWN freshly-built rootfs (point
  `kernel_image`/`rootfs_image` at its own `.maturana/images/firecracker/<name>/`
  and build it), which bakes the correct per-agent key + network. Copying a
  *running* agent's disk also snapshots a mounted-rw filesystem.
- **Keep it supervised.** `launch` runs Firecracker as a child and returns; the
  standing agents stay up because the **plane/fleet** supervises them. A bare
  `agent launch --apply` over an interactive SSH session lets the VM get SIGHUP'd
  when the session closes — bring the agent up via `maturana up` / the fleet, or
  fully detach (`setsid nohup … </dev/null >/dev/null 2>&1 &`). **Never stop a
  working agent to free its TAP** — that doesn't fix a duplicate slot, it just
  breaks the working agent.

### Inspect before you relaunch (don't cycle a healthy VM)

Re-running `setup firecracker-harnesses` **stops and relaunches** the agent's
Firecracker VM. Don't do that to a working agent: a needless relaunch can hit a
transient Firecracker death (e.g. a guest RTC-port error flood), briefly taking
the agent down. First check live state and only (re)provision if it's actually
down:

```bash
maturana agent inspect <agent-id> --live        # look at live.state
# running        -> healthy; do NOT relaunch. Just submit turns.
# stale-pid / stopped / running-missing-socket -> then:
maturana setup firecracker-harnesses --agent-id <agent-id> --skip-services --skip-assets
```

If a relaunch does leave the VM dead (`inspect` shows `stale-pid`, TAP
`DOWN`/`NO-CARRIER`, guest unpingable), just relaunch once more — it is normally
a one-off; confirm the firecracker pid is stable for ~25s before declaring
success.

### Driving setup/verify through Codex

To drive the install/verify **through Codex** on the host, Codex must run
**unsandboxed** — the flow opens `/dev/kvm`, reaches `sessiond` on localhost, and
`sudo`s for the TAP, all of which the default sandbox blocks (you'd see the
misleading `/dev/kvm does not exist`). Use `codex exec
--dangerously-bypass-approvals-and-sandbox -C <repo>` (or a `[profiles.*]` with
`sandbox_mode = "danger-full-access"`, `network_access = true`,
`approval_policy = "never"`). **Over SSH, redirect stdin from `/dev/null`** —
otherwise `codex exec` prints `Reading additional input from stdin...` and hangs
until timeout reading the never-closed pipe:

```bash
ssh host 'cd <repo> && codex exec --dangerously-bypass-approvals-and-sandbox \
  -C <repo> "follow maturana-agent-launch to inspect + verify <agent-id>" < /dev/null'
```

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
- **`--apply` fails partway (hostd 500, SSH exit 255, guest command error):**
  do NOT start editing `scripts/` or tweaking ssh flags to chase it. Capture the
  evidence and stop: `.maturana/logs/hyperv-launch-<agent-id>.log` (the leaf
  launcher) and `.maturana/logs/hostd.log` (the daemon), plus the exact failing
  step. Report those. The leaf scripts are Rust-rendered/owned adapters — a real
  fix goes into the Maturana source (e.g. `provider`/`worker` or the launcher in
  the repo), not into a live hand-edit on this host, which only drifts the
  install and masks the bug. (Example: an ssh exit 255 on a multi-line guest
  command is a PowerShell quoting/CRLF problem in the launcher, not something to
  fix by adding ssh options — it's fixed in the repo.)

## Boundaries

- Do not implement launch orchestration in PowerShell or bash.
- **Do not provision the guest by hand.** Installing the proxy CA, injecting
  Codex/Claude auth, installing the harness/browser, bootstrapping directories,
  copying the agent contract, and enabling/starting `maturana-agent.service` are
  ALL done by `maturana agent launch … --apply` (Rust: `provision_hyperv_guest` /
  `install_guest_worker`). Never SSH into the guest to run apt/cp/systemctl
  yourself, and never author a PowerShell/bash wrapper to do it. If `--apply`
  fails, report its exact error and stop — do not reimplement provisioning.
- **Do not edit the leaf scripts (`scripts/launch-ubuntu-cloudimg-hyperv.ps1`,
  `firecracker-*.sh`, …) or tweak ssh options on this host to get past a launch
  error.** They are Rust-rendered/owned adapters; a live hand-edit drifts the
  install from source and hides the real bug. Collect the logs, report the
  failing step, and fix it in the Maturana repo instead.
- Do not add a queue or broker for command execution.
- Do not add generic host command execution to hostd.
- Do not copy host directories into the guest unless the spec declares them.
- Do not store raw secrets in specs, docs, skills, or audit logs.
