# maturana-snapshot

Use this skill when a user wants to take, list, or restore Maturana snapshots —
either full VM **memory** snapshots (`maturana snapshot`, pause/restore a live
VM) or fast **copy-on-write disk** snapshots of a Firecracker rootfs
(`maturana vm`, instant reflink clone/snapshot/rewind on Btrfs/XFS/ZFS).

Snapshots are provider-aware safety operations. Treat them as a workflow with
preflight and evidence, not as a bare command wrapper.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the target agent id and read
   `.maturana/agents/<agent-id>/MATURANA.md`.
3. Identify the provider:
   - Hyper-V on Windows
   - Firecracker on Linux
4. Inspect current live state before live take or restore:
   - `maturana agent inspect <agent-id> --live`
   - recent audit entries with `maturana audit list <agent-id> --json`
5. For restore, list snapshots and inspect the snapshot record before changing
   VM state.

## Preflight

- Confirm the snapshot name is a simple path segment.
- Confirm whether the user needs a restorable live VM snapshot or only a local
  audit marker.
- Confirm live provider state before `--live` take or restore.
- Confirm restore target kind/provider matches the current agent provider.
- Confirm `snapshot.json` parses before trusting a snapshot directory.

## Decision Path

- Need only an audit marker: create a local marker without `--live`.
- Need VM rollback capability: use `--live`.
- Hyper-V live snapshot: use fixed Rust hostd checkpoint operations via
  the Rust CLI.
- Firecracker live snapshot: use the Rust snapshot manager. It pauses the VM,
  calls the Firecracker snapshot API, copies the writable rootfs, and always
  attempts to resume.
- Firecracker restore: restore only `FirecrackerFull` records. Local markers
  and Hyper-V checkpoint records are not restorable Firecracker snapshots.
- Snapshot names must be simple path segments. Reject names containing path
  separators, drive prefixes, `.`, or `..`.

## Actions

List known local/provider snapshots:

```powershell
maturana snapshot list <agent-id>
```

Create a local marker only when the user wants a named checkpoint in the
agent's audit trail but does not need VM restore:

```powershell
maturana snapshot take <agent-id> <snapshot-name>
```

Use `--live` for restorable VM snapshots:

```powershell
maturana snapshot take <agent-id> <snapshot-name> --live
maturana snapshot list <agent-id> --live
maturana snapshot restore <agent-id> <snapshot-name> --live
```

Copy-on-write **disk** snapshots of a Firecracker rootfs (Linux; instant +
space-shared on Btrfs/XFS/ZFS-2.2+, full copy on ext4). Stop the agent first —
these operate on the rootfs file:

```bash
maturana vm fstype <path>                       # report reflink capability of a filesystem
maturana vm clone <src> <dest>                  # reflink clone a rootfs image (prints reflink vs full-copy)
maturana vm snapshot <agent-id> --name <name>   # CoW snapshot the (stopped) agent rootfs
maturana vm snapshots <agent-id>                # list CoW rootfs snapshots
maturana vm rollback <agent-id> --name <name>   # rewind rootfs to a snapshot, then relaunch
```

## Evidence

Before claiming success, collect provider-appropriate proof:

- Local marker: `.maturana/agents/<agent-id>/snapshots/<name>/snapshot.json`
  with `kind: local-marker`.
- Hyper-V live snapshot: `snapshot list --live` shows the checkpoint name,
  `.maturana/agents/<agent-id>/snapshots/<name>/snapshot.json` exists with
  `kind: hyper-v-checkpoint`, and audit includes the snapshot event.
- Hyper-V restore: if a local `snapshot.json` exists, it must be
  `provider: hyperv` and `kind: hyper-v-checkpoint` before hostd restore is
  attempted.
- Firecracker live snapshot: snapshot directory contains `vm-state.snap`,
  `memory.mem`, `rootfs.ext4`, and `snapshot.json` with
  `kind: firecracker-full`.
- Snapshot records are written atomically; if `snapshot.json` is missing or
  cannot parse, treat the snapshot as incomplete and do not restore it.
- Firecracker restore constrains materialized PID/socket metadata to the
  agent `state` directory before it stops a process or removes a socket.
- Firecracker restore also verifies the rootfs drive in
  `firecracker-config.json` matches `vm.firecracker.rootfs_image` in
  `MATURANA.md` before it overwrites disk state.
- Restore: command returned success, live inspect succeeds afterward, and audit
  includes `snapshot.restore.live`.
- Failed list, take, or restore attempts leave a matching `snapshot.*.failed`
  audit entry; read it before retrying or changing provider state.

## Recovery

- Invalid snapshot name: choose a simple name such as `before-upgrade`.
- Snapshot already exists: choose a new name. Do not overwrite an existing
  snapshot directory.
- Local marker restore requested: explain that local markers are not
  restorable; ask whether to take a live snapshot next time.
- Hyper-V restore rejected by local metadata: choose a real Hyper-V checkpoint
  or remove/fix the incorrect local snapshot record only after verifying hostd
  checkpoint state.
- Hostd unreachable for Hyper-V: start/install the fixed hostd task once, then
  retry through the Rust CLI.
- Firecracker API unavailable: inspect live state, PID/socket state, and
  Firecracker stderr before retrying.
- Firecracker snapshot failed after pause: read the error carefully; Maturana
  attempts resume and reports if resume also failed.
- Firecracker restore failed after replacing rootfs: Maturana attempts to stop
  the restore process and roll the original rootfs back. Inspect the combined
  error before retrying.
- Firecracker stop failed during restore: treat the VM as still live. Maturana
  does not delete PID/socket state unless the process has exited; inspect the
  PID and Firecracker stderr before another restore attempt.
- Firecracker rootfs contract mismatch: do not edit the snapshot record to work
  around it. Re-materialize the agent from `MATURANA.md` or fix the spec/config
  drift before restoring.
- Snapshot component missing: do not restore. Re-take a live snapshot or choose
  another complete snapshot.
- Snapshot component path escape: do not edit the record to bypass it. Restore
  only accepts files that resolve inside the snapshot directory.

## Boundaries

- Do not implement snapshotting through a generic guest command runner.
- Do not call deleted Firecracker shell lifecycle scripts.
- Do not manually copy rootfs or memory files as a substitute for
  `maturana snapshot`.
- Do not restore a snapshot whose provider/kind does not match the target
  provider.
- Do not bypass name validation.
