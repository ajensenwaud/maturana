# Maturana

Secure agentic orchestration built around Codex skills and hardware-isolated
worker agents.

## Install

One-liners. Both download the **signed prebuilt `maturana` binary** from the
latest [GitHub Release](https://github.com/ajensenwaud/maturana/releases) (no
Rust/C/MSYS2 toolchain on your machine), verify its SHA256, clone the repo for
the skills/scripts/orientation files, and register the `maturana up` runtime
plane + `maturana web` cockpit as services:

```sh
# Linux (control plane: CLI + web cockpit)
curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.sh | bash

# Linux that will also RUN isolated agents — add the Firecracker microVM host:
curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.sh | bash -s -- --firecracker

# Build locally from source instead of downloading the prebuilt binary:
curl -fsSL .../scripts/install.sh | bash -s -- --from-source
```

```powershell
# Windows (Hyper-V) — downloads the signed maturana.exe, then installs.
# Self-elevates (one UAC) and prompts for your Windows password (for the
# no-login boot tasks). See "Zero-touch reboot recovery" below.
irm https://www.maturana.sh/install.ps1 | iex
```

Or clone this repo and run `scripts/install.sh` / `scripts/install.ps1`
directly. The Windows installer downloads the prebuilt binary by default; pass
`-FromSource` to build with the Rust MSVC toolchain instead. On Linux,
`--firecracker` (or
running `scripts/install-firecracker-host.sh` standalone) provisions the microVM
substrate — the `firecracker` binary, KVM access, the libguestfs/qemu
image-build toolchain, and guest-egress NAT — then `maturana setup
firecracker-harnesses` builds the images and launches agents (see
[docs/linux-firecracker-harnesses.md](docs/linux-firecracker-harnesses.md)).

#### Script vs. tool — what *you* run

There are only **two things you ever run by hand**:

1. **`install.sh` / `install.ps1`** — the one-time bootstrap (download the
   binary, set up the host). Run it once per machine.
2. **`maturana <command>`** — everything after that: `maturana up`,
   `maturana agent …`, `maturana channel …`, `maturana setup …`, `maturana web`.
   The Rust CLI owns all the logic.

Every other file in `scripts/` (`install-hostd-task.ps1`, `set-vm-autostart.ps1`,
`install-firecracker-host.sh`, `firecracker-*.sh`, …) is an **internal adapter**
the installer or the CLI calls for you — you don't invoke them directly. Rule of
thumb: **if it's a `.ps1`/`.sh` you typed, it's only ever `install`/`uninstall`;
anything else is `maturana …`.** (`maturana setup` was historically `maturana
repair`, which still works as an alias.)

### Releases, verification & signing

Tagging `v*` runs `.github/workflows/release.yml`, which builds
`maturana-x86_64-unknown-linux-gnu.tar.gz` and
`maturana-x86_64-pc-windows-msvc.zip`, publishes them with a `SHA256SUMS`
manifest, and (when the signing secrets are configured) **Authenticode-signs the
Windows `.exe`** and **GPG-signs `SHA256SUMS`**. The installers always verify the
SHA256; signature verification is best-effort until the certs are wired in:

- Windows code signing: add repo secrets `WINDOWS_PFX_BASE64` (base64 of the
  code-signing `.pfx`) + `WINDOWS_PFX_PASSWORD`.
- Linux checksum signing: add `GPG_PRIVATE_KEY` (ASCII-armored) + `GPG_PASSPHRASE`;
  verify with `gpg --verify SHA256SUMS.asc SHA256SUMS`.

Manual verification:

```sh
sha256sum -c --ignore-missing SHA256SUMS         # Linux
```
```powershell
(Get-AuthenticodeSignature maturana.exe).Status  # Windows (Valid once signed)
```

> The public site will live at **www.maturana.sh**; the install URLs will move
> there once it's up. For now they point at GitHub.

Maturana is **Codex-native** — everything flows from Codex. After installing,
start building agents from Codex (it's oriented by the repo's `AGENTS.md` +
`skills/`):

```sh
cd <install-dir> && codex      # then ask it to create + launch your first agent
```

`codex login` authenticates your subscription on first run. The web cockpit at
`http://<host>:47836` (token via `maturana web token`; see
[docs/web-cockpit.md](docs/web-cockpit.md)) is an optional, complementary
surface. Manage services with `maturana service install|uninstall|status|restart
[up|web|fleet]`.

### Uninstall

Removes the services, running processes, and agent VMs. By default it **keeps
your data** (the repo + `.maturana`, which holds credentials/agents); add
`--purge` / `-Purge` to remove everything:

```sh
# Linux
curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall.sh | bash
curl -fsSL .../scripts/uninstall.sh | bash -s -- --purge
```
```powershell
# Windows (self-elevates)
irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall-windows.ps1 | iex
# from a clone, to also delete data:  .\scripts\uninstall-windows.ps1 -Purge
```

This repository is at MVP stage, but the product path is Rust-owned and
provider-aware. The current implementation provides:

- a Rust CLI named `maturana`
- `MATURANA.md` parsing with YAML front matter
- spec validation for runtimes, mounts, egress, credentials, channels, and VM
  settings
- Windows Hyper-V launch planning and apply mode through fixed host adapters
- dry-run and applied agent materialization under `.maturana/agents/<agent-id>`
- generated guest `AGENTS.md`, `SOUL.md`, launch plan, audit log, workspace,
  memory, and snapshot directories
- Linux Firecracker launch, stop, inspect, and snapshot/restore paths owned by
  Rust, with shell scripts kept as image/TAP setup adapters
- Maturana skills shaped as operational workflows with grounding, preflight,
  evidence, recovery, and boundaries rather than thin CLI wrappers

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

For the current local deployment path, non-harness credentials use `env:`,
`file:`, or `pipelock:`
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

The local pipelock vault uses a random key at `.maturana/pipelock/key` and encrypted
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
personal-agent boundary.

Local runtime state is ignored in `.maturana/`.

## Windows Hyper-V

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
.\scripts\maturana.ps1 spec validate examples/MATURANA.codex-demo.md
.\scripts\maturana.ps1 agent launch examples/MATURANA.codex-demo.md
.\scripts\maturana.ps1 agent inspect codex-demo
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

Validate the bundled skill workflows directly:

```powershell
.\scripts\maturana.ps1 skill validate skills
```

Real Hyper-V launch is guarded behind `--apply`. On Windows, `maturana agent
launch --apply` talks to the privileged localhost Rust hostd, which then
runs the fixed Ubuntu cloud-image launcher. Codex remains non-elevated during
normal operation.

Prepare Windows once from a normal PowerShell session. The last step may show a
single UAC prompt to install the privileged scheduled-task host daemon:

```powershell
.\scripts\install.ps1
```

Or run the steps separately. Prepare the official Ubuntu cloud image once:

```powershell
winget install --id cloudbase.qemu-img --exact
.\scripts\maturana.ps1 setup ubuntu-cloudimg
.\scripts\maturana.ps1 setup ssh-key
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

The CLI validates and materializes the agent, renders the guest artifacts, asks
hostd to create/start the Hyper-V VM, then provisions the guest over SSH from
Rust. Hostd and PowerShell do not install harnesses or copy auth state.

The direct launcher is only a Hyper-V debugging adapter. It creates/starts the
VM, waits for IPv4 and SSH, expands the root filesystem, and prints
`MATURANA_RESULT_JSON` with the guest IP:

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

Verify the running VM and guest from the Rust CLI:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live --guest
```

Rust hostd exposes only narrow localhost operations such as health, VM
inspection, fixed Ubuntu launch, stop, and Hyper-V checkpoint operations. It
requires the local token in `.maturana/hostd/token` for privileged operations.
It is not a general command runner and it does not use a queue.

Check VM state later with:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo --live
```

If the current shell cannot use Hyper-V cmdlets or hostd to discover VM state,
inspect the guest directly over SSH through the Rust CLI:

```powershell
.\scripts\maturana.ps1 agent inspect codex-demo `
  --live `
  --guest `
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

Send another live task to the guest harness through the session queue. This is
a direct agent operation, not a hostd command queue:

```powershell
.\scripts\maturana.ps1 agent run codex-demo `
  --prompt "Inspect /agent/MATURANA.md and summarize your current status." `
  --wait
```

`agent run` writes to the same `sessiond` queue used by Telegram/Discord and
waits for a matching outbound response when `--wait` is set. It does not write
ad hoc prompt files into the guest.

```powershell
.\scripts\maturana.ps1 agent run codex-demo `
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
guest work loop for the Windows Hyper-V path.

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

## Linux Firecracker on aidev

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

Prepare the standard harness image/assets through the Rust-owned repair path:

```bash
sudo apt-get install -y qemu-utils libguestfs-tools
cargo build -p maturana-cli
target/debug/maturana setup firecracker-harnesses --agent-id codex-firecracker
```

The repair command starts `sessiond`, creates the host TAP device, renders the
guest worker artifacts in Rust, calls the image-prep adapter, materializes the
spec, launches Firecracker, waits for SSH, and refreshes the guest worker. The
image-prep and TAP scripts are leaf adapters; do not use them as the normal
orchestration path.

For a custom Firecracker spec, prepare matching image assets and TAP through a
small Rust repair command or a one-off adapter invocation with Rust-rendered
worker files, then launch from the spec:

```bash
cargo run -p maturana-cli -- spec validate examples/MATURANA.firecracker-demo.md
cargo run -p maturana-cli -- agent launch examples/MATURANA.firecracker-demo.md
cargo run -p maturana-cli -- agent launch examples/MATURANA.firecracker-demo.md --apply
cargo run -p maturana-cli -- agent inspect firecracker-demo --live
```

For the built-in aidev harnesses:

```bash
target/debug/maturana setup firecracker-harnesses
```

The Linux Firecracker image-prep adapter injects SSH, static guest networking,
the Maturana agent files, harness auth state from `.maturana/host-auth/*`, and
the Rust-rendered `sessiond.env`, `run-agent.sh`, and
`maturana-agent.service`.

Rust owns Firecracker lifecycle decisions: prerequisite checks, idempotent
start, stop, live inspect, pid tracking, socket cleanup, and launch metadata.
The remaining Firecracker scripts are host setup adapters for image prep and
TAP creation only.
