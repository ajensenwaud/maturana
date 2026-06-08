# maturana-agent-launch

Use this skill when a user wants to materialize or launch a Maturana agent from
`MATURANA.md`.

## Procedure

1. Validate the spec:

   ```powershell
   maturana spec validate MATURANA.md
   ```

2. Materialize a dry-run launch plan:

   ```powershell
   maturana agent launch MATURANA.md
   ```

3. Inspect `.maturana/agents/<agent-id>/launch-plan.json`.
4. Use `--apply` only when the Ubuntu base image, SSH key, hostd, and host
   hypervisor are ready.

## Windows Hyper-V MVP

Prepare the official Ubuntu VHDX and SSH key once, then reuse them:

```powershell
.\scripts\install-windows.ps1
```

Launch through the Rust CLI. The CLI talks to `maturana-hostd`; hostd performs
the privileged Hyper-V work. Do not use a queue or broker for launch.

```powershell
$env:MATURANA_HYPERV_FORCE = "true" # only when replacing an existing demo VM
.\scripts\maturana.ps1 agent launch .\examples\MATURANA.codex-hyperv.md --apply
```

For debugging only, the direct elevated launcher can be run by hand:

```powershell
.\scripts\launch-ubuntu-cloudimg-hyperv.ps1 `
  -AgentId codex-demo `
  -BaseVhdxPath .\.maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx `
  -SshUser ubuntu `
  -SshKeyPath .\.maturana\keys\maturana-agent-ed25519 `
  -Harness codex `
  -HarnessAuthSource .\.maturana\host-auth\codex `
  -HarnessAuthGuestPath /home/ubuntu/.codex `
  -InstallHarness `
  -StartHarness `
  -Force
```

`maturana-hostd` may be used to inspect VM state, but launch is synchronous and
direct. OAuth auth state is copied from `.maturana/host-auth/codex` or
`.maturana/host-auth/claude-code` into the VM. The guest systemd service runs
`/agent/run-command` when present, otherwise runs `codex exec` against
`/agent/prompt.txt`, writes logs under `/var/log/maturana`, and keeps a
heartbeat file current.

Hostd exposes fixed Maturana VM operations only. Do not add a generic command
execution endpoint. Privileged hostd operations require the token in
`.maturana/hostd/token`; the CLI reads this token automatically.

Inspect live VM state through the CLI when hostd is running:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

Submit a new live task to the guest harness through SSH:

```powershell
.\scripts\maturana.ps1 agent run codex-demo --prompt "Inspect /agent/MATURANA.md and report status." --wait
```

If the current shell cannot use hostd to discover the VM IP, provide `--ip`.
This still keeps hostd out of guest command execution; hostd manages VM
lifecycle, while the agent turn goes directly to the guest harness.

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

## Linux Firecracker MVP

On `aidev`, use specs with `vm.provider: firecracker` and explicit
`vm.firecracker.kernel_image`, `rootfs_image`, and `tap_name`.

```bash
sudo apt-get install -y qemu-utils libguestfs-tools
sudo ./scripts/firecracker-prepare-assets.sh
sudo ./scripts/firecracker-setup-tap.sh tap-maturana0 172.30.0.1/30 172.30.0.0/30
./scripts/firecracker-doctor.sh .maturana/images/firecracker/vmlinux.bin .maturana/images/firecracker/ubuntu-rootfs.ext4 tap-maturana0
maturana spec validate examples/MATURANA.firecracker-demo.md
maturana agent launch examples/MATURANA.firecracker-demo.md
maturana agent launch examples/MATURANA.firecracker-demo.md --apply
./scripts/firecracker-inspect.sh .maturana/agents/firecracker-demo
```

The provider writes `firecracker-config.json` and metadata under the agent state
directory. Keep image building out of launch; launch consumes the prepared
kernel/rootfs artifacts.
