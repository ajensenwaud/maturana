# Script Boundary

Maturana's product control plane belongs in Rust. Scripts are allowed only when
they are thin adapters around host primitives that Rust cannot call directly in
a portable way.

## Rule

```text
Codex skill -> maturana CLI/hostd -> Rust decision -> leaf script/host primitive
```

Do not put lifecycle state machines, retry policy, credential policy, channel
policy, or snapshot semantics in PowerShell or bash.

## Allowed Leaf Adapters

These scripts are acceptable as host setup or host primitive wrappers:

- `scripts/maturana.ps1`: Windows convenience wrapper for the Rust CLI.
- `scripts/build-windows-msvc.ps1`, `scripts/build-windows-gnu.ps1`: build
  wrappers.
- `scripts/ci.ps1`: local CI aggregator and script-boundary guard. It keeps an
  explicit allowlist of scripts and syntax-checks every remaining PowerShell and
  bash script. It also rejects known orchestration/policy strings in leaf
  scripts, including Hyper-V cloud-init generation and proxy-policy parsing.
- `scripts/install.ps1`: the single Windows installer (downloads the prebuilt
  binary, or `-FromSource` builds it; self-elevates once; sets up hostd, the
  Ubuntu image, and the up/web boot services).
- `scripts/install-hostd-task.ps1`, `scripts/uninstall-hostd-task.ps1`:
  scheduled task registration for the Rust `maturana hostd serve` daemon.
- Windows session/channel runners are started and optionally registered by the
  Rust CLI. Do not add PowerShell task helpers for these generic runner
  lifecycles.
- `scripts/run-elevated.ps1`: UAC helper for one-off host setup.
- `scripts/firecracker-setup-tap.sh`: Linux TAP setup.
- `scripts/firecracker-prepare-assets.sh`: Linux image/kernel/rootfs
  preparation.

## Migration Candidates

These scripts should shrink or move behind Rust commands as the provider paths
harden:

- `scripts/launch-ubuntu-cloudimg-hyperv.ps1`: keep only the Hyper-V cmdlet
  calls, seed VHDX packaging, VM start, IPv4 discovery, SSH readiness, and root
  filesystem expansion. Rust renders and installs guest files after the VM is
  reachable. The launcher must not choose apt packages, npm packages, harness
  install commands, worker behavior, cloud-init policy, SSH authorization
  policy, proxy policy, guest file layout, or guest service state.
Recently migrated:
- Windows hostd daemon: `maturana hostd serve` owns the local HTTP listener,
  token auth, fixed endpoint routing, request validation, and response shape.
  It calls PowerShell only for Hyper-V cmdlets and the create/start leaf
  launcher. The old PowerShell hostd daemon was removed.
- Hyper-V guest worker generation: `launch-ubuntu-cloudimg-hyperv.ps1` no
  longer embeds the worker loop or systemd unit. Rust renders the worker files,
  guest bootstrap script, and selected harness install script, so the
  PowerShell launcher does not contain the Codex/Claude/OpenCode package
  mapping.
- Hyper-V cloud-init policy: Rust renders `user-data` and `meta-data`; the
  launcher requires those files and only packages them into the NoCloud seed
  VHDX. It must not derive SSH public keys or fall back to generated
  cloud-init.
- Hyper-V proxy policy: Rust decides whether proxying and MITM CA installation
  are required. Rust copies `proxy.env` and installs the CA over SSH after the
  VM is reachable. The launcher must not parse or install proxy policy.
- Firecracker guest worker generation: `firecracker-prepare-assets.sh` no
  longer embeds the worker loop or systemd unit. Rust renders `sessiond.env`,
  `run-agent.sh`, `firecracker-bootstrap.sh`, `install-harness.sh`,
  `maturana-agent.service`, netplan, and cloud network-disable config. When a
  spec enables the pipelock proxy, Rust also renders `proxy.env` from the typed
  spec and passes the MITM CA path. The image-prep adapter only copies those
  files into the rootfs and runs the Rust-rendered bootstrap/install scripts.
  The adapter does not contain the base guest package list or the
  Codex/Claude/OpenCode package mapping. Provider adapters no longer write
  one-shot `/agent/prompt.txt` or `/agent/run-command` files; agent turns enter
  through `sessiond`.
- Firecracker guest artifact ownership: `maturana-core::worker` owns the
  Firecracker bootstrap script, netplan, cloud network-disable config, proxy
  environment, harness installer, service, and run loop renderers. The CLI may
  choose a profile and call those renderers, but must not define its own guest
  artifact policy.
- Firecracker asset manifest: `firecracker-prepare-assets.sh` must report the
  kernel, rootfs, SSH key, network identity, sizes, and SHA-256 values it
  produced in `asset-manifest.json`. Rust validates that manifest before
  continuing launch/repair. The adapter is not trusted as the source of
  lifecycle truth.
- Firecracker OAuth/auth injection: Rust selects the harness auth source from
  the profile/spec and passes it explicitly to `firecracker-prepare-assets.sh`.
  The adapter must not default to Codex or any other harness credential path.
- Browser provisioning: `browser.headless_chrome: true` is honored by the
  Rust-rendered guest installer. It installs Playwright Chromium into
  `/opt/maturana/browsers`, sets `PLAYWRIGHT_BROWSERS_PATH`, and writes a
  narrow browser smoke script. PowerShell and bash adapters must not decide
  browser packages or browser policy.
- Windows personal-agent daemon repair: `maturana repair windows-harnesses`
  now creates sessiond/channel token, log, pid, process, and optional scheduled
  task state directly in Rust. It does not call PowerShell for generic runner
  lifecycles.
- Hyper-V hostd launch queue removal: Rust hostd runs the fixed Ubuntu launcher
  synchronously and returns the launcher result directly. The previous launch
  job/status files and generated hostd runner scripts are not a product control
  path.
- Hyper-V guest provisioning ownership: hostd invokes the Ubuntu launcher as a
  create/start adapter. The leaf launcher reports the guest IPv4 address. Rust
  then copies `MATURANA.md`, `AGENTS.md`, `SOUL.md`, proxy files, harness
  OAuth/auth state, `sessiond.env`, the worker, harness installer, and systemd
  unit over SSH/SCP and starts the service when the spec requests it. CI rejects
  launcher content or hostd calls that pass guest provisioning back into
  PowerShell.

## Test Scripts

Live test scripts may stay while they exercise real host behavior:

- `scripts/test-pipelock-proxy-live.ps1`
- `scripts/test-pipelock-proxy-aidev.ps1`
- `scripts/test-pipelock-proxy-firecracker-live.sh`

They should not become product control paths.

## Removed Diagnostics

These scripts have been removed from the product surface:

- `scripts/hyperv-status.ps1`: replaced by
  `maturana agent inspect <agent-id> --live`.
- `scripts/inspect-ubuntu-hyperv-agent.ps1`: replaced by
  `maturana agent inspect <agent-id> --live --guest`.
- `scripts/repair-windows-harnesses.ps1`: replaced by
  `maturana repair windows-harnesses`.
- `scripts/invoke-hostd-ubuntu-launch.ps1`: replaced by
  `maturana agent launch <spec> --apply`.
- `scripts/start-hyperv-agent.ps1`: replaced by
  `maturana agent launch <spec> --apply`.
- `scripts/demo-hyperv-real.ps1`: replaced by
  `maturana agent launch <spec> --apply`.
- `scripts/demo-windows.ps1`: replaced by direct `maturana spec validate`,
  `maturana agent launch`, and `maturana agent inspect` commands.
- `scripts/refresh-guest-worker.ps1`: replaced by
  `maturana repair windows-harnesses`, which renders the worker in Rust.
- `scripts/refresh-firecracker-worker.sh`: replaced by
  `maturana repair guest-worker`, which renders the worker in Rust.
- `scripts/deploy-aidev-firecracker-harnesses.sh`: replaced by
  `maturana repair firecracker-harnesses`.
- `scripts/init-agent-ssh-key.ps1`: replaced by `maturana repair ssh-key`.
- `scripts/get-ubuntu-cloudimg.ps1`: replaced by
  `maturana repair ubuntu-cloudimg`.
- `scripts/maturana-hostd.ps1`: replaced by `maturana hostd serve`; the
  scheduled task installer starts the Rust binary directly.

## Review Checklist

Before adding or expanding a script, ask:

- Is this strictly host-specific?
- Can Rust own the decision and call this as a leaf?
- Is it idempotent?
- Does it avoid raw secret output?
- Is quoting/path handling explicit?
- Does a Rust test or CI syntax check cover it?

If the answer is no, add or extend a Rust command instead.
