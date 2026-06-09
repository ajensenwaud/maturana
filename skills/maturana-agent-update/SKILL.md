# maturana-agent-update

Use this skill when a user wants to modify an existing Maturana agent contract
and apply the change safely.

An update is a spec change plus verification. Treat the current materialized
agent as state that must be inspected before and after the edit.

## Grounding

1. Read `AGENTS.md` first.
2. Read the source spec and the materialized
   `.maturana/agents/<agent-id>/MATURANA.md`.
3. Inspect current live state with `maturana agent inspect <agent-id> --live`
   when the VM exists.
4. Read recent audit entries, snapshot records, channel state, schedule state,
   and heartbeat state relevant to the requested change.
5. Identify whether the update affects provider, harness, mounts, credentials,
   egress, tools, skills, channels, schedules, browser policy, or snapshots.

## Preflight

- Confirm the target agent ID and spec path.
- Confirm whether the change requires relaunch, guest repair, skill deploy, or
  only a spec edit.
- Confirm a rollback point exists or take a live snapshot for risky changes.
- Confirm new credentials are references and are available through the approved
  path.
- Confirm the user understands any downtime or VM restart needed.

## Decision Path

- Metadata, channels, schedules, memory, or wiki path change: edit spec,
  validate, and apply the narrow runtime update where supported.
- Harness, image, provider, rootfs, or mount change: validate and relaunch after
  taking a snapshot.
- Skill or tool change: use the create/deploy skills after spec validation.
- Egress or pipelock change: validate allowlist and audit behavior before
  restarting workers.
- OAuth auth path change: confirm direct guest injection and avoid pipelock.
- Failed or uncertain live state: inspect and repair before applying unrelated
  changes.

## Actions

Compare current state:

```powershell
.\scripts\maturana.ps1 agent inspect <agent-id> --live
```

Edit the spec with the smallest change that satisfies the request.

Validate:

```powershell
.\scripts\maturana.ps1 spec validate MATURANA.md
```

Apply using the appropriate narrow path: launch/relaunch for provider changes,
deploy for skill/tool changes, schedule commands for schedules, and channel
commands for channel state.

## Evidence

Before claiming success, collect:

- The exact spec path and a summary of changed fields.
- Clean validation output after the edit.
- Snapshot or rollback evidence for risky runtime changes.
- Live inspect output before and after relaunch or repair.
- Audit entries created by launch, snapshot, deploy, or channel operations.
- Guest evidence when the update affects worker files, browser support, or
  harness behavior.

## Recovery

- Validation fails: revert only the intended edit or repair the invalid field;
  do not weaken validation.
- Live inspect fails: check hostd/Firecracker status before relaunching.
- Relaunch fails: inspect provider logs and keep the previous snapshot intact.
- Channel update breaks replies: inspect session inbox, outbox, heartbeat, and
  channel runner logs before editing bot code.
- Credential path missing: configure the approved source and retry.
- User asks for broad capability: update the spec explicitly and run security
  review before applying.

## Boundaries

- Do not edit materialized state as the source of truth without updating the
  spec.
- Do not apply runtime changes before validation.
- Do not restart services blindly without inspecting state.
- Do not bypass snapshot policy for risky provider or filesystem changes.
- Do not introduce script-owned orchestration to apply an update.
