# MVP Plan

## Milestone 1: Windows Control Plane

- Parse and validate `MATURANA.md`.
- Materialize agent directories and generated guest files.
- Produce Hyper-V launch plans.
- Support guarded real Hyper-V apply mode.
- Store audit logs and snapshot markers.
- Keep secrets out of specs and git.

## Milestone 2: Windows Demo Agent

- Add a bootable Linux image path to `vm.boot_image`.
- Enable Hyper-V and create a private switch.
- Launch one Codex harness guest.
- Inject OAuth directly into the guest harness auth directory.
- Send a Telegram notification through `env:MATURANA_TELEGRAM_BOT_TOKEN`.

## Milestone 3: Linux on aidev

- Build the CLI on `aidev`.
- Replace Firecracker planning with API socket configuration.
- Configure kernel, rootfs, tap networking, and governed SSH access.
- Launch the Codex harness first.
- Add Claude Code harness validation.

## Milestone 4: OpenCode

- Add OpenCode harness materialization after Codex and Claude Code are working.
