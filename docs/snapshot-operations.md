# Snapshot Operations

Maturana snapshot decisions live in Rust. Shell and PowerShell stay as host
adapters only where the hypervisor requires them.

## Commands

```powershell
.\scripts\maturana.ps1 snapshot list <agent-id>
.\scripts\maturana.ps1 snapshot take <agent-id> <snapshot-name>
.\scripts\maturana.ps1 snapshot take <agent-id> <snapshot-name> --live
.\scripts\maturana.ps1 snapshot restore <agent-id> <snapshot-name> --live
```

Without `--live`, `snapshot take` creates a local marker record only. Local
markers are useful for audit and operator notes, but they are not restorable.
Snapshot names must be simple path segments; names containing path separators,
drive prefixes, `.` or `..` are rejected before any provider call. Snapshot
creation also refuses an existing name so a failed or repeated take cannot mix
new Firecracker state with old snapshot components.

With `--live`, the Rust snapshot manager dispatches by provider:

- Hyper-V: reserve a local snapshot directory, call fixed Rust hostd
  checkpoint endpoints, and write a structured `snapshot.json` record for the
  checkpoint.
- Firecracker: pause the VM through the Firecracker API, create a full VM and
  memory snapshot, copy the writable rootfs, and resume the VM.

Firecracker restore stops the current Firecracker process, restores the rootfs
copy, starts a fresh Firecracker API socket, waits for that API to answer,
loads the snapshot, and resumes the VM. This keeps disk state aligned with VM
memory state. Restore refuses local marker records and non-Firecracker snapshot
records before touching the current VM. Snapshot component paths in
`snapshot.json` must resolve inside that snapshot directory; edited records
cannot point restore at arbitrary host files.
Firecracker PID and API socket paths in materialized metadata must also resolve
inside the agent's `state` directory, so restore cannot be tricked into
deleting or replacing unrelated host files through edited metadata.
Before restore removes a Firecracker socket, it verifies the socket is not a
responding untracked Firecracker API. If the PID is missing or stale but the API
socket still answers, restore refuses to continue instead of deleting live state
that Maturana does not own.
The current rootfs path in `firecracker-config.json` must match
`vm.firecracker.rootfs_image` in the materialized `MATURANA.md`; restore refuses
the operation before stopping the VM if the config has drifted or been edited to
point at another host file.
The current rootfs is backed up before replacement and rolled back if the
Firecracker API does not successfully load the snapshot.
Snapshot records are written through a temporary file and atomic rename so
interrupted writes do not leave a partial `snapshot.json`.

If Firecracker snapshot creation fails after the VM was paused, Maturana still
attempts to resume the VM before returning the original error. If resume also
fails, the error reports both conditions.

## Files

Provider-created local files live under:

```text
.maturana/agents/<agent-id>/snapshots/<snapshot-name>/
```

Firecracker snapshots contain:

- `vm-state.snap`
- `memory.mem`
- `rootfs.ext4`
- `snapshot.json`

Hyper-V live snapshots contain a local `snapshot.json` record with
`kind: hyper-v-checkpoint`. The actual checkpoint remains owned by Hyper-V and
is addressed through hostd during live list/restore. During restore, Maturana
validates the local record when one exists and refuses to restore names whose
local metadata is a marker or a different provider/kind. Hostd-only checkpoints
without local metadata remain restorable through `snapshot restore --live`.

Snapshot operations append audit events to:

```text
.maturana/audit/<agent-id>.jsonl
```

Successful snapshot operations use `snapshot.*` audit actions. Failed list,
take, and restore attempts append matching `snapshot.*.failed` actions before
the CLI returns the error, so recovery has durable evidence even when a live
provider operation did not complete.
