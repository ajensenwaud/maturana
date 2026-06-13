# Linux Firecracker harnesses

Host: `aj@aidev`

Repo path:

```bash
/var/tmp/maturana-aidev
```

## Deploy

Sync the repo to `aidev`, copy harness auth directories into `.maturana/host-auth`, build, then run:

```bash
cd /var/tmp/maturana-aidev
cargo build -p maturana-cli
target/debug/maturana repair firecracker-harnesses
```

To resume one agent:

```bash
target/debug/maturana repair firecracker-harnesses --agent-id opencode-firecracker
```

Agents:

- `codex-firecracker`: `172.30.10.2`, TAP `tap-mat-codex`, session `codex-main`
- `opencode-firecracker`: `172.30.10.6`, TAP `tap-mat-open`, session `opencode-main`
- `claude-firecracker`: `172.30.10.10`, TAP `tap-mat-claude`, session `claude-main`

## Systemd-managed plane

For a fleet that survives reboots, let the `maturana up` systemd service own the
runtime plane (sessiond, the MaturanaGraph service, and the per-agent channel +
schedule runners) and let `repair` only build assets, launch VMs, and install
guest workers:

```bash
target/debug/maturana service install up web
target/debug/maturana repair firecracker-harnesses --skip-services
```

`--skip-services` keeps `repair` from starting its own sessiond/graph (which
would collide on ports `47834`/`47835` with the `maturana up` service). It still
ensures the sessiond and graph tokens exist, so guest artifacts embed them and
`maturana up` knows to supervise the graph service.

`maturana up` derives each agent's `--session-id` from that agent's materialized
spec / Firecracker profile (so the supervised channel writes to the same queue
the guest worker claims from): `codex-firecracker` → `codex-main`,
`claude-firecracker` → `claude-main`, etc. Pass `maturana up --session-id <id>`
only when you want to force every agent onto one shared queue.

### Zero-touch reboot recovery

The `up`/`web` units only bring back the host plane and cockpit — not the
microVMs, whose TAP devices are wiped on reboot. Register the boot-time fleet
relauncher so the VMs come back too, with no interactive login:

```bash
target/debug/maturana service install fleet
```

This installs a systemd **oneshot** (`maturana-fleet.service`) ordered
`After=maturana-up.service` that runs `repair firecracker-harnesses
--skip-services --skip-assets`: it recreates each agent's TAP + NAT rule and
relaunches the VM from the baked rootfs (no libguestfs rebuild, no sessiond).
The enabled in-guest `maturana-agent.service` + stable sessiond token let the
worker self-recover. `service install` also runs `loginctl enable-linger` so the
user manager (and its units) start at boot without a login. `install.sh
--firecracker` registers `fleet` automatically.

Two flags make this idempotent: `--skip-net` (leave it OFF for boot — the TAP is
ephemeral and must be recreated) and the un-baked guard (a profile with no
`.maturana/images/firecracker/<image>/ubuntu-rootfs.ext4` is skipped, so the
boot unit no-ops cleanly on a host that hasn't built images yet).

## Lifecycle

Firecracker lifecycle is Rust-owned. Use the CLI for launch and stop:

```bash
target/debug/maturana agent launch examples/MATURANA.codex-firecracker.md --apply
target/debug/maturana agent inspect codex-firecracker --live
target/debug/maturana agent stop codex-firecracker --live
```

Shell scripts remain host setup adapters for image preparation, TAP creation,
and live verification.

Launch validates host prerequisites before starting Firecracker: `firecracker`,
`curl`, and `ip` must exist, `/dev/kvm` must be readable and writable, the
kernel must be a non-empty ELF `vmlinux`, the rootfs must be a non-empty file,
and the configured TAP must exist and have the `UP` flag in `ip -j link` output.
If the TAP is missing or down, repair host networking before relaunching.

Launch and live inspect also validate the materialized Firecracker plan files
before trusting them. `state/firecracker-config.json` must match the current
`MATURANA.md` kernel path, rootfs path, vCPU count, memory size, dirty-page
tracking policy, guest MAC, and TAP name. `state/firecracker-metadata.json`
must match the current agent ID, runtime, TAP, host/guest IPs, guest MAC, proxy
settings, and its socket/config/pid/log/metrics paths must remain inside the
agent state directory. If either file has drifted or been edited, regenerate the
agent plan from the spec instead of starting stale state.

Stop sends TERM first, waits for the Firecracker process to exit, then
escalates to KILL if the process does not stop. PID files and API sockets are
removed only after the recorded process is gone or proven stale.

Launch is intentionally conservative. If a recorded Firecracker PID is still
running, the Rust provider preserves the existing API socket and verifies the
API before returning success. If the PID is running but the socket is missing,
launch fails and asks for an explicit stop or repair instead of deleting live
state. Stale PID/socket files are cleaned only after the provider has proved no
recorded process is running. If no PID file exists but the API socket responds,
launch and stop refuse to remove it and inspect reports `untracked-api-socket`.
That state means a live Firecracker control socket exists outside Maturana's
PID tracking and should be diagnosed before relaunch. If no PID file exists and
the socket does not answer, inspect reports `stale-socket` and an explicit stop
can clean it. Live inspect reports the missing-socket case as
`running-missing-socket`, so operators can distinguish a healthy VM from a live
process that has lost its control socket. If the PID and socket both exist but
the Firecracker API does not answer within the bounded health check, inspect
reports `running-api-unresponsive`; treat that as a live process with a wedged
control plane and use an explicit stop or repair before relaunching.

The old `scripts/deploy-aidev-firecracker-harnesses.sh` compatibility wrapper
has been removed. Use `maturana repair firecracker-harnesses` directly.

Do not run `scripts/firecracker-prepare-assets.sh` as an orchestration path.
`maturana repair firecracker-harnesses` renders the guest worker artifacts in
Rust, renders the guest netplan and cloud network-disable config, and renders
`proxy.env` from the typed spec when pipelock proxying is enabled. It passes
those files to the image-prep adapter, then launches/materializes the agent. The
script is only a leaf adapter for Ubuntu image customization, kernel extraction,
and rootfs export. The adapter must write `asset-manifest.json` beside the
prepared kernel/rootfs. Rust validates that manifest before launch continues:
agent ID, guest/host IPs, guest MAC, TAP name, kernel/rootfs/SSH key paths,
non-empty file sizes, kernel ELF magic, and kernel/rootfs SHA-256 values must
match the selected Firecracker profile.

## Refresh Worker

```bash
target/debug/maturana repair guest-worker \
  --agent-id codex-firecracker \
  --session-id codex-main \
  --harness codex \
  --ssh-key .maturana/images/firecracker/maturana-firecracker.id_rsa \
  --auth-source .maturana/host-auth/codex \
  --harness-auth-guest-path /home/ubuntu/.codex \
  --sessiond-url http://172.30.10.1:47834 \
  --sessiond-token-path .maturana/sessiond/token \
  --install-harness
```

`repair guest-worker` renders `sessiond.env` and `run-agent.sh` in Rust, infers
the guest IP from live provider inspect, copies the worker files to the guest,
and restarts `maturana-agent.service`. Use `--guest-ip` only to override
inspect when recovering a broken materialized spec.

## Test

```bash
target/debug/maturana session enqueue codex-firecracker \
  --session-id codex-main \
  --channel health \
  --platform-id doctor \
  --text "Reply exactly: linux-codex-health"

target/debug/maturana session outbox codex-firecracker --session-id codex-main
```

## Cron

```bash
target/debug/maturana schedule add codex-firecracker every-minute \
  --cron "* * * * *" \
  --prompt "Reply exactly: schedule-health" \
  --channel health

target/debug/maturana schedule run-due codex-firecracker --session-id codex-main
```
