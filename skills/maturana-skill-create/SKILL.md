# maturana-skill-create

Use this skill when creating a new Codex skill for Maturana or for an agent.

Skills are operational workflows. They may call tools, but the skill itself
should explain grounding, decisions, evidence, recovery, and boundaries.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/skill-workflows.md`.
3. Inspect existing skills under `skills/` for overlap.
4. Read the target agent `MATURANA.md` when the skill will be deployed into a
   guest VM.
5. Identify the side effects the skill may trigger and which Rust command or
   guest tool should own them.

## Preflight

- Confirm the requested behavior belongs in a skill, not a Rust core feature or
  executable tool.
- Confirm the skill name is specific and follows the `maturana-*` pattern when
  part of the framework.
- Confirm the skill has a bounded operating context and does not become a
  catch-all command list.
- Confirm no secrets or OAuth material will be embedded.
- Confirm validation can run before deployment.

## Decision Path

- Human-facing decision workflow: create a skill.
- Repeatable side-effect logic: create a tool and let the skill call it.
- Provider lifecycle behavior: add or call Rust code, not PowerShell/bash
  orchestration.
- Agent-specific capability: create under a deployable skill path and document
  the target contract.
- Framework capability: add it under `skills/` and include it in skill
  validation.

## Actions

Scaffold:

```powershell
.\scripts\maturana.ps1 develop skill <name>
```

Fill out the required sections:

- Grounding
- Preflight
- Decision Path
- Actions
- Evidence
- Recovery
- Boundaries

Validate:

```powershell
.\scripts\maturana.ps1 skill validate skills
```

Deploy only after validation when the skill is intended for a guest.

## Evidence

Before claiming success, collect:

- The created `skills/<name>/SKILL.md` path.
- Confirmation that the skill trigger sentence is specific.
- Clean `maturana skill validate skills` output.
- Evidence that the skill is not a command-only wrapper.
- Evidence that side effects are delegated to Rust commands or tools.
- Deployment evidence if the skill was installed into a guest VM.

## Recovery

- Skill overlaps an existing one: extend the existing skill or rename the new
  workflow.
- Validation fails: add missing sections, evidence, recovery, or boundaries.
- Skill drifts into code: move executable behavior into a tool.
- Skill is too broad: split it into smaller workflows.
- Secret appears in examples: remove it and use `env:`, `file:`, or `pipelock:`
  references.
- Guest deployment fails: use `maturana-skill-deploy` and inspect guest paths.

## Boundaries

- Do not create heading-only skills.
- Do not replace grounding/evidence/recovery with a command list.
- Do not embed raw credentials.
- Do not make a skill own provider state machines.
- Do not deploy an unvalidated skill.
