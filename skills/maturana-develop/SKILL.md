# maturana-develop

Use this skill when developing a new Maturana skill, guest tool, or MCP bundle
in Codex before injecting it into an agent VM.

Codex is the development environment. The guest VM receives tested artifacts;
it should not be the place where host-side tool design is improvised.

## Grounding

1. Read `AGENTS.md` first.
2. Identify whether the user needs a Codex skill, guest tool, MCP bundle, or
   host-side helper.
3. Read the target agent `MATURANA.md` and confirm the capability is allowed.
4. Inspect existing `skills/`, `tools/`, and agent-deployed artifacts to avoid
   duplicates.
5. Identify required credentials and decide whether they are pipelock secrets
   or direct OAuth guest auth injection.

## Preflight

- Confirm the capability belongs in a skill, tool, or MCP bundle rather than
  Maturana core.
- Confirm the target agent contract permits the capability.
- Confirm local tests or smoke checks can be run before deployment.
- Confirm no raw credentials will be written into generated source.
- Confirm deployment is a separate step after host-side verification.

## Decision Path

- Agent-facing procedure or playbook: create a skill.
- Executable side-effect helper: create a tool.
- External protocol/server integration: create a tool or MCP bundle.
- Needs secrets: design env/config injection through pipelock; do not bake
  secrets into source.
- Needs guest access: build/test locally first, then use `maturana-deploy`.
- Risky capability: run a security review before deployment.

## Actions

Scaffold locally:

```powershell
maturana develop skill <name>
maturana develop tool <name>
```

`develop skill` creates a workflow skeleton with grounding, preflight, decision
path, actions, evidence, recovery, and boundaries. Keep those sections and fill
them with the target behavior instead of replacing the skill with a thin command
wrapper.

Build and test in the Codex host workspace. Use the repository's existing test
commands and the smallest focused tests that prove the tool behavior.

Inspect the target VM before deployment:

```powershell
maturana agent inspect <agent-id> --live
```

Deploy only after local verification:

```powershell
maturana deploy skill <agent-id> .\skills\<name> --ip <ip>
maturana deploy tool <agent-id> .\tools\<name> --ip <ip>
```

For fixed MCP paths, pass:

```powershell
--guest-path /agent/tools/mcp/<name>
```

## Evidence

Before claiming success, collect:

- Scaffolded files exist in `skills/<name>` or `tools/<name>`.
- Local tests or smoke checks pass.
- The artifact contains no raw secrets.
- The target agent contract permits the capability.
- Deploy evidence shows the artifact in `/agent/skills` or `/agent/tools`.
- The agent can invoke or see the skill/tool after deployment.

## Recovery

- Wrong artifact type: convert the design before deployment; do not overload a
  skill with side effects or a tool with human workflow instructions.
- Local tests fail: fix locally before touching the guest.
- Missing runtime dependency: package it with the tool or document/install the
  dependency through an allowed guest tool path.
- Secret found in source: remove it, rotate it, and use pipelock.
- Deployment fails: switch to the deploy skill and debug guest path/IP issues.

## Boundaries

- Do not deploy secrets with tools.
- Do not put OAuth harness credentials in pipelock.
- Do not edit guest state manually when a deploy command exists.
- Do not add new core runtime behavior for a capability that belongs in a skill
  or tool.
- Do not skip local tests before deployment.
