# maturana-security-review

Use this skill when reviewing a Maturana spec, skill, tool, provider change, or
credential path before launch or deployment.

Security review is a blocking gate for broad permissions, credentials, network
egress, snapshots, provider lifecycle changes, and guest tools with side
effects.

## Grounding

1. Read `AGENTS.md` first.
2. Read the artifact under review: `MATURANA.md`, `SKILL.md`, tool source,
   provider code, or script.
3. Read relevant docs: script boundary, skill workflow, snapshot operations,
   pipelock verification, and harness operations.
4. Inspect tests and CI guards that should cover the artifact.
5. Identify all credentials, filesystem paths, network destinations, VM
   lifecycle operations, channels, schedules, and audit outputs touched.

## Preflight

- Confirm the review target and intended deployment or launch path.
- Confirm whether the artifact can affect host state, guest state, network
  egress, credentials, or snapshots.
- Confirm current tests have run or identify the smallest missing test.
- Confirm raw secrets are absent from source, docs, memory, wiki, and logs.
- Confirm PowerShell/bash changes are leaf adapters only.

## Decision Path

- Spec review: check mounts, egress, credentials, channels, schedules, browser,
  snapshots, and harness auth paths.
- Skill review: check grounding, preflight, evidence, recovery, and boundaries.
- Tool review: check input validation, secret handling, filesystem scope,
  network scope, and deploy path.
- Provider review: check Rust owns state transitions and scripts are leaf
  adapters.
- Pipelock review: check allowlist, injection scope, MITM CA handling, and audit
  records.
- Snapshot review: check provider/kind matching, path confinement, rollback,
  and restore evidence.

## Actions

Run targeted validation first:

```powershell
.\scripts\maturana.ps1 spec validate MATURANA.md
.\scripts\maturana.ps1 skill validate skills
```

Run focused tests for changed Rust modules and then the normal CI gate.

Review diffs for:

- raw secrets
- broad filesystem mounts
- broad egress
- generic command runners
- script-owned orchestration
- missing audit or restore evidence

Document findings by severity and block launch/deploy on high-risk unresolved
issues.

## Evidence

Before claiming review completion, collect:

- The reviewed artifact paths and commit/worktree context.
- Validation and focused test output.
- Secret scan or explicit raw-secret search evidence.
- Findings list with severity and file references.
- Confirmation that required audit/log/snapshot evidence exists for runtime
  operations.
- Any residual risk or missing live test that prevents a stronger claim.

## Recovery

- Raw secret found: remove it, rotate it, and replace with an approved
  reference.
- Script owns orchestration: move decisions into Rust and leave a narrow adapter.
- Overbroad spec permission: narrow the mount, egress, or capability and
  revalidate.
- Missing test: add a focused Rust or script-boundary test before deploy.
- Snapshot restore unproven: take and restore a live provider snapshot before
  claiming coverage.
- Skill is a wrapper: rewrite it as a workflow and rerun skill validation.

## Boundaries

- Do not approve artifacts that fail validation.
- Do not treat docs as proof of runtime behavior.
- Do not ignore missing Linux validation for Firecracker changes.
- Do not let scripts become control planes.
- Do not accept unaudited credential injection or broad egress.
