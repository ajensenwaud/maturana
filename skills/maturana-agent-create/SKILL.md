# maturana-agent-create

Use this skill when a user wants to turn an agent goal into a durable
`MATURANA.md` contract.

This skill is a design workflow. The output is a readable, repeatable spec that
can pass validation before any VM is launched.

## Grounding

1. Read `AGENTS.md` first.
2. Read any existing `MATURANA.md`, `SOUL.md`, and project README files in the
   target workspace.
3. Identify the intended agent identity, purpose, owner, harness, provider,
   channels, schedules, memory/wiki paths, tools, skills, browser policy,
   snapshot policy, and credential requirements.
4. Inspect existing examples under `examples/` for the closest known-good
   Hyper-V or Firecracker shape.
5. Identify whether OAuth harness credentials are required and keep them as
   direct guest auth injection.

## Preflight

- Confirm the user goal is specific enough to define scope and permissions.
- Confirm the target host provider: Hyper-V for Windows or Firecracker for
  Linux.
- Confirm the preferred harness: `codex`, `claude-code`, or `opencode`.
- Confirm filesystem mounts are explicit and bounded.
- Confirm network egress is allowlist-based unless the user intentionally
  chooses a development-only exception.
- Confirm credentials are references, not literal secret values.

## Decision Path

- New personal assistant: include memory, wiki, heartbeat, channel, and schedule
  sections.
- Engineering worker: include workspace mount policy, skill/tool deployment
  paths, GitHub capability, and snapshot policy.
- Browser-capable agent: set browser policy and make egress allowlist explicit.
- Subscription harness: declare host auth source and guest path rather than
  pipelock.
- API-key tool: use `pipelock:` or `env:` secret references.
- Unsure provider: choose the host-native provider and a known-good image
  before designing conversion machinery.

## Actions

Draft or update `MATURANA.md` using the closest example as a starting point.
Keep the spec explicit rather than clever.

Validate the candidate:

```powershell
.\scripts\maturana.ps1 spec validate MATURANA.md
```

For a repository example, validate the exact file:

```powershell
.\scripts\maturana.ps1 spec validate .\examples\MATURANA.codex-hyperv.md
```

If the spec is for Linux/Firecracker, also validate from aidev when possible.

## Evidence

Before claiming success, collect:

- The path to the created or updated `MATURANA.md`.
- Clean `maturana spec validate` output for that file.
- A short summary of provider, harness, mounts, egress, credentials, channels,
  schedules, memory/wiki, and snapshots.
- Evidence that no raw secrets were written into the spec.
- The example or existing spec used as the starting point.
- Any open choices that need explicit user confirmation before launch.

## Recovery

- Goal too vague: ask for purpose, runtime, host, and external services before
  drafting broad permissions.
- Validation fails: fix the spec fields rather than weakening validation.
- Missing credential source: add a reference and document the setup step; do not
  paste the secret.
- Overbroad filesystem scope: narrow mounts to the minimal host and guest paths.
- Unknown provider or harness: use supported values from `AGENTS.md`.
- Conflicting channels or schedules: keep them disabled until the user confirms
  the operating model.

## Boundaries

- Do not launch an agent from an unvalidated spec.
- Do not infer broad filesystem or network access from a vague goal.
- Do not store OpenAI or Claude Code OAuth credentials in pipelock.
- Do not paste raw secrets into `MATURANA.md`, memory, wiki, docs, or logs.
- Do not design custom image conversion when a known-good native image exists.
