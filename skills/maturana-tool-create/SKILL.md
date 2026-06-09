# maturana-tool-create

Use this skill when creating a host or guest tool that a Maturana skill or
agent can invoke.

Tools own executable side effects. Skills explain when and why to invoke them.

## Grounding

1. Read `AGENTS.md` first.
2. Read the target skill that will call the tool.
3. Read the target agent `MATURANA.md` when the tool will run inside a guest.
4. Inspect existing tools, scripts, and Rust commands to avoid duplicates.
5. Identify required runtime dependencies, filesystem access, network access,
   and credentials.

## Preflight

- Confirm this is executable behavior, not a human workflow.
- Confirm the tool has a narrow input/output contract.
- Confirm the tool does not require broad host filesystem access.
- Confirm secrets enter through approved env/config paths.
- Confirm a local test or smoke command can prove behavior before deployment.

## Decision Path

- Host lifecycle or provider operation: prefer a Rust command or provider
  method.
- Guest-local helper: create a guest tool and deploy it under `/agent/tools`.
- External service integration: keep credentials out of source and use
  pipelock/env references.
- MCP integration: package the server with explicit config and deployment
  evidence.
- Reusable policy or playbook: create a skill that calls the tool instead of
  making the tool interactive.

## Actions

Scaffold:

```powershell
.\scripts\maturana.ps1 develop tool <name>
```

Implement the smallest executable contract. Add tests or a smoke command.

Deploy only after local verification:

```powershell
.\scripts\maturana.ps1 deploy tool <agent-id> .\tools\<name> --ip <ip>
```

Document the calling skill and expected environment variables.

## Evidence

Before claiming success, collect:

- The created tool path and entrypoint.
- The input/output contract.
- Local test or smoke output.
- Secret scan evidence or confirmation that credentials are referenced only.
- Target agent contract evidence permitting the tool.
- Guest deploy evidence and a guest-side smoke result when deployed.

## Recovery

- Tool should have been a skill: move the decision workflow into a skill and
  keep only side effects in the tool.
- Missing runtime dependency: vendor it, document it, or add an allowed install
  step.
- Local test fails: fix before deployment.
- Guest deploy fails: inspect SSH, guest path, permissions, and agent contract.
- Credential missing: configure pipelock/env reference instead of editing
  source.
- Tool needs host privileges: add a narrow Rust-facing host primitive rather
  than shelling out broadly.

## Boundaries

- Do not create generic shell command runners.
- Do not put human decision logic inside tools.
- Do not embed credentials or OAuth auth state.
- Do not deploy tools outside declared guest paths.
- Do not use PowerShell or bash for provider state machines.
