# Maturana

Secure agentic orchestration built around Codex skills and hardware-isolated
worker agents.

This repository is at MVP stage. The current implementation provides:

- a Rust CLI named `maturana`
- `MATURANA.md` parsing with YAML front matter
- spec validation for runtimes, mounts, egress, credentials, channels, and VM
  settings
- Windows-first Hyper-V launch planning
- dry-run agent materialization under `.maturana/agents/<agent-id>`
- generated guest `AGENTS.md`, `SOUL.md`, launch plan, audit log, workspace,
  memory, and snapshot directories
- Firecracker provider planning for Linux work on `aidev`
- Codex skill stubs that call the CLI

## Secret Handling

Do not place OAuth tokens, Telegram bot tokens, Discord webhooks, or other
secrets in `MATURANA.md`.

Harness OAuth for Codex and Claude Code is injected directly into the guest VM
filesystem because those CLIs expect local subscription auth state. Put host-side
copies under ignored paths such as:

```text
.maturana/host-auth/codex
.maturana/host-auth/claude-code
```

For the MVP, non-harness credentials use `env:`, `file:`, or `pipelock:`
references instead of raw secrets. `pipelock:` resolves from a local Maturana
vault under `.maturana/pipelock`. It is for ordinary API tokens and bot tokens,
not Codex or Claude OAuth state.

```yaml
credentials:
  - name: telegram-bot-token
    source: pipelock:telegram/bot-token

channels:
  telegram:
    token_source: pipelock:telegram/bot-token
    chat_id_source: env:MATURANA_TELEGRAM_CHAT_ID
```

Basic pipelock use:

```powershell
.\scripts\maturana.ps1 pipelock init
.\scripts\maturana.ps1 pipelock set telegram/bot-token --value "<token>"
.\scripts\maturana.ps1 pipelock list
.\scripts\maturana.ps1 channel pair telegram start
# Send the printed /pair CODE to the bot in Telegram.
.\scripts\maturana.ps1 channel pair telegram complete
.\scripts\maturana.ps1 channel serve telegram --agent-id codex-demo
.\scripts\maturana.ps1 notify telegram `
  --token-source pipelock:telegram/bot-token `
  --message "Maturana agent is alive" `
  --dry-run
```

The MVP vault uses a local random key at `.maturana/pipelock/key` and encrypted
entries in `.maturana/pipelock/vault.json`. Both files stay under ignored local
runtime state. A future egress proxy can use the same source names without
changing specs.

Run the simple HTTP egress proxy:

```powershell
.\scripts\maturana.ps1 pipelock proxy --spec .\examples\MATURANA.codex-hyperv.md
```

Or run it with explicit one-off policy flags:

```powershell
.\scripts\maturana.ps1 pipelock proxy `
  --bind 127.0.0.1:47833 `
  --allow api.example.test `
  --inject-header api.example.test:Authorization=pipelock:api/token
```

The proxy accepts ordinary HTTP proxy requests such as
`GET http://api.example.test/path HTTP/1.1`, enforces the allowlist, injects
configured headers from pipelock, and writes audit events to
`.maturana/audit/*-pipelock-proxy.jsonl` or
`.maturana/audit/pipelock-proxy.jsonl` for one-off runs. HTTPS `CONNECT`
traffic is normally tunneled, but hosts with `network.proxy.inject_headers`
are intercepted with a local Maturana CA so headers can be injected inside
TLS. Codex and Claude OAuth still stay directly injected into the guest.

Create or inspect the public Maturana pipelock CA:

```powershell
.\scripts\maturana.ps1 pipelock ca-cert
```

Test the proxy from the running Hyper-V guest:

```powershell
.\scripts\test-pipelock-proxy-live.ps1
```

The live test starts a fake HTTPS host upstream, starts the pipelock proxy, has
the guest call through the proxy with `curl`, and verifies the upstream received
the header value loaded from pipelock and that the audit log recorded TLS
interception.

Run the same live verification against the running Firecracker guest on `aidev`:

```powershell
.\scripts\test-pipelock-proxy-aidev.ps1
```

See `docs/pipelock-live-verification.md` for the exact Windows and Linux
success criteria.

## Session Runner

Interactive channels use a NanoClaw-style session boundary:

```text
channel adapter -> inbound.sqlite -> warm runner -> outbound.sqlite -> delivery
```

The host adapter owns pairing, polling, audit, and delivery. The runner owns the
harness call. This keeps Telegram/Discord out of the harness lifecycle and
avoids per-message service restarts.

Smoke-test the session layer without a real harness:

```powershell
.\scripts\maturana.ps1 session init codex-demo --session-id telegram-main
.\scripts\maturana.ps1 session enqueue codex-demo --session-id telegram-main --channel telegram --platform-id chat-1 --text "hello"
.\scripts\maturana.ps1 session run-once codex-demo --session-id telegram-main --provider echo
.\scripts\maturana.ps1 session outbox codex-demo --session-id telegram-main --mark-delivered
```

The NanoClaw reference used for this design is kept locally at
`.maturana/reference/nanoclaw`.

## Personal Agent Layer

Initialize durable personal-agent files:

```powershell
.\scripts\maturana.ps1 personal init codex-demo --spec .\examples\MATURANA.codex-hyperv.md
```

Ingest shared markdown context into the local LLM wiki:

```powershell
.\scripts\maturana.ps1 wiki ingest .\AGENTS.md --title Repo-Agents
.\scripts\maturana.ps1 wiki search secure --limit 5
```

Write heartbeat and schedule records:

```powershell
.\scripts\maturana.ps1 heartbeat beat codex-demo --status alive --message "ready"
.\scripts\maturana.ps1 schedule add codex-demo morning `
  --cron "0 9 * * *" `
  --prompt "Send a morning brief" `
  --channel telegram
```

Deploy new skills and tools into a running guest:

```powershell
.\scripts\maturana.ps1 deploy skill codex-demo .\skills\maturana-wiki --ip 172.26.x.y
.\scripts\maturana.ps1 deploy tool codex-demo .\target\x86_64-pc-windows-gnu\debug\maturana.exe --ip 172.26.x.y --guest-path /agent/tools/maturana.exe
```

See `docs/personal-agent-mvp.md` for the personal-agent file layout and current
MVP boundary.

Local runtime state is ignored in `.maturana/`.

## Windows MVP

Install Rust, then run:

```powershell
cargo build
.\scripts\maturana.ps1 spec validate examples/MATURANA.codex-demo.md
.\scripts\maturana.ps1 agent launch examples/MATURANA.codex-demo.md
.\scripts\maturana.ps1 agent inspect codex-demo
```

The default launch is a dry run. It materializes the agent and writes a
Hyper-V launch plan without creating a VM.

If MSVC Build Tools are not available yet, use the GNU toolchain path:

```powershell
.\scripts\build-windows-gnu.ps1
.\scripts\demo-windows.ps1
```

For day-to-day Windows commands, use the wrapper that builds/runs the GNU CLI
and avoids the default MSVC linker path:

```powershell
.\scripts\maturana.ps1 --help
```

If MSVC Build Tools are installed but `link.exe` is not on `PATH`, initialize
the Visual C++ environment with `vcvarsall.bat`:

```powershell
.\scripts\build-windows-msvc.ps1 -Test -CheckFormat

# Equivalent raw command:
cmd.exe /d /c 'call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64 && cargo test --target x86_64-pc-windows-msvc'
```

On this host, `cl.exe` and `link.exe` are under
`C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64`.

Run the full local repeatability check:

```powershell
.\scripts\ci.ps1
```

Real Hyper-V launch is guarded behind `--apply`. On Windows, `maturana agent
launch --apply` talks to the privileged localhost `maturana-hostd`, which then
runs the fixed Ubuntu cloud-image launcher. Codex remains non-elevated during
normal operation.

Prepare Windows once from a normal PowerShell session. The last step may show a
single UAC prompt to install the privileged scheduled-task host daemon:

```powershell
.\scripts\install-windows.ps1
```

Or run the steps separately. Prepare the official Ubuntu cloud image once:

```powershell
winget install --id cloudbase.qemu-img --exact
.\scripts\get-ubuntu-cloudimg.ps1
.\scripts\init-agent-ssh-key.ps1
```

Install the privileged host daemon once. From a normal shell this requests UAC;
from an elevated shell it installs directly:

```powershell
.\scripts\install-hostd-task.ps1
```

If Codex cannot see or approve the UAC prompt, open PowerShell as Administrator
in the repo and run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-hostd-task.ps1 -NoElevate
```

Check hostd from a normal shell:

```powershell
.\scripts\maturana.ps1 hostd status
.\scripts\maturana.ps1 hostd status --json
```

Launch the Codex Hyper-V demo VM from a normal shell through the Rust CLI:

```powershell
$env:MATURANA_HYPERV_FORCE = "true" # only when replacing an existing demo VM
.\scripts\maturana.ps1 agent launch .\examples\MATURANA.codex-hyperv.md --apply
```

The CLI validates and materializes the agent, then hostd copies the prepared
VHDX to the agent state directory, expands it, creates a NoCloud cloud-init seed
disk, creates a Hyper-V Generation 2 VM, starts it, waits for IPv4 and SSH,
injects agent files and OAuth auth state, installs Codex, and starts the systemd
harness. It is synchronous and does not use a queue.

The direct launcher remains available for debugging from an elevated shell:

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

Verify the running guest:

```powershell
.\scripts\inspect-ubuntu-hyperv-agent.ps1 -AgentId codex-demo `
  -SshUser ubuntu `
  -SshKeyPath .\.maturana\keys\maturana-agent-ed25519
```

`maturana-hostd` exposes only narrow localhost operations such as health, VM
inspection, fixed Ubuntu launch, stop, and Hyper-V checkpoint operations. It
requires the local token in `.maturana/hostd/token` for privileged operations.
It is not a general command runner and it does not use a queue.

Check VM state later with:

```powershell
.\scripts\hyperv-status.ps1 -AgentId codex-demo
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

If the current shell cannot use Hyper-V cmdlets or hostd to discover VM state,
inspect the guest directly over SSH:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo `
  --live `
  --ip 172.26.183.108
```

Read known guest logs without arbitrary file access:

```powershell
.\scripts\maturana.ps1 agent logs codex-demo `
  --ip 172.26.183.108 `
  --kind agent `
  --lines 80

.\scripts\maturana.ps1 agent logs codex-demo `
  --ip 172.26.183.108 `
  --kind last-message
```

Send another live task to the guest harness over SSH. This is a direct
agent operation, not a hostd command queue:

```powershell
.\scripts\maturana.ps1 agent run codex-demo `
  --prompt "Inspect /agent/MATURANA.md and summarize your current status." `
  --wait
```

If hostd is not available for IP discovery in the current shell, pass the guest
IP explicitly:

```powershell
.\scripts\maturana.ps1 agent run codex-demo `
  --ip 172.26.183.108 `
  --prompt "Write Maturana live run OK to /workspace/live-run.txt." `
  --wait
```

Push input files into the guest workspace:

```powershell
.\scripts\maturana.ps1 agent push codex-demo `
  .\.maturana\agents\codex-demo\workspace\host-input.txt `
  /workspace/host-input.txt `
  --ip 172.26.183.108
```

Fetch an artifact back from the guest workspace:

```powershell
.\scripts\maturana.ps1 agent fetch codex-demo `
  /workspace/live-run.txt `
  .\.maturana\agents\codex-demo\workspace\live-run.txt `
  --ip 172.26.183.108
```

Together, `agent push`, `agent run`, and `agent fetch` form the simple live
guest work loop for the Windows MVP.

Live file transfers are intentionally bounded to `/workspace`, `/memory`,
`/wiki`, and declared `filesystem.mounts[*].guest_path` roots inside the guest.

Live inspect, logs, run, push, fetch, stop, and snapshot operations append
per-agent audit events to `.maturana/audit/<agent-id>.jsonl`.

Read recent per-agent audit events through the CLI:

```powershell
.\scripts\maturana.ps1 audit list codex-demo --limit 10
.\scripts\maturana.ps1 audit list codex-demo --limit 10 --json
```

Stop, checkpoint, list checkpoints, and restore through hostd:

```powershell
.\scripts\maturana.ps1 snapshot take codex-demo before-change --live
.\scripts\maturana.ps1 snapshot list codex-demo --live
.\scripts\maturana.ps1 snapshot restore codex-demo before-change --live
.\scripts\maturana.ps1 agent stop codex-demo --live
```

Telegram notifications use secret references:

```powershell
.\scripts\maturana.ps1 channel pair telegram start
# Send the printed /pair CODE to the bot in Telegram.
.\scripts\maturana.ps1 channel pair telegram complete
.\scripts\maturana.ps1 channel serve telegram --agent-id codex-demo
.\scripts\maturana.ps1 notify telegram `
  --token-source pipelock:telegram/bot-token `
  --message "Maturana agent is alive"
```

## Linux MVP on aidev

Use `vm.provider: firecracker` in the spec and run the same CLI on `aidev`.
The Firecracker provider materializes a concrete config under
`.maturana/agents/<agent-id>/state/firecracker-config.json` plus metadata and a
launch plan.

Prepare a Firecracker-compatible kernel and rootfs at the paths declared in the
spec:

```yaml
vm:
  provider: firecracker
  firecracker:
    kernel_image: .maturana/images/firecracker/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/ubuntu-rootfs.ext4
    tap_name: tap-maturana0
```

Prepare the official Ubuntu cloud image on the Linux host:

```bash
sudo apt-get install -y qemu-utils libguestfs-tools
sudo ./scripts/firecracker-prepare-assets.sh
```

Create the host tap device once:

```bash
sudo ./scripts/firecracker-setup-tap.sh tap-maturana0 172.30.0.1/30 172.30.0.0/30
```

Check the Linux host and declared assets:

```bash
./scripts/firecracker-doctor.sh \
  .maturana/images/firecracker/vmlinux.bin \
  .maturana/images/firecracker/ubuntu-rootfs.ext4 \
  tap-maturana0
```

Materialize and launch:

```bash
cargo run -p maturana-cli -- spec validate examples/MATURANA.firecracker-demo.md
cargo run -p maturana-cli -- agent launch examples/MATURANA.firecracker-demo.md
cargo run -p maturana-cli -- agent launch examples/MATURANA.firecracker-demo.md --apply
./scripts/firecracker-inspect.sh .maturana/agents/firecracker-demo
```

The Linux MVP image prep injects SSH, static guest networking, the Maturana
agent files, optional Codex OAuth state from `.maturana/host-auth/codex`, and
the same simple systemd runner used by the Hyper-V path.
