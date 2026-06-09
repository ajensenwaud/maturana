# maturana-skill-deploy

Use this skill when installing a tested skill or tool into a target Maturana
agent VM.

Deployment is not development. Build and validate artifacts first, then copy the
smallest approved directory into the guest.

## Grounding

1. Read `AGENTS.md` first.
2. Read the target agent `MATURANA.md`.
3. Inspect the artifact to deploy and confirm whether it is a skill, tool, or
   MCP bundle.
4. Inspect live agent state and guest IP.
5. Read the deploy skill/tool documentation and recent audit entries.

## Preflight

- Confirm the artifact exists and has already passed local tests.
- Confirm the target agent contract permits the skill or tool.
- Confirm the guest path is under `/agent/skills`, `/agent/tools`, or another
  declared path.
- Confirm no raw secrets are present in the artifact.
- Confirm the guest VM is reachable and the SSH key is the expected Maturana
  agent key.

## Decision Path

- Skill directory: deploy under `/agent/skills/<name>` unless the spec declares
  another path.
- Tool directory or executable: deploy under `/agent/tools/<name>`.
- MCP bundle: deploy under `/agent/tools/mcp/<name>` and include config.
- Missing live IP: inspect provider state before retrying SSH.
- Artifact needs a runtime dependency: install through an approved guest tool
  path or update the agent contract.

## Actions

Inspect first:

```powershell
.\scripts\maturana.ps1 agent inspect <agent-id> --live
```

Deploy a skill:

```powershell
.\scripts\maturana.ps1 deploy skill <agent-id> .\skills\<name> --ip <ip>
```

Deploy a tool:

```powershell
.\scripts\maturana.ps1 deploy tool <agent-id> .\tools\<name> --ip <ip>
```

Run the smallest guest-side smoke check after copying.

## Evidence

Before claiming success, collect:

- Local validation or test output for the artifact.
- Live inspect output showing the target VM and IP.
- Deploy command output.
- Guest file listing or smoke output proving the artifact is present.
- Audit entry or session evidence if deployment triggers agent-visible state.
- Confirmation that no raw secrets were deployed.

## Recovery

- VM has no IP: inspect provider status and network diagnostics before retrying.
- SSH fails: verify key path, user, guest state, and host firewall.
- Wrong guest path: remove the misplaced artifact and redeploy to the declared
  path.
- Artifact validation fails: return to create/develop workflow before deploy.
- Runtime dependency missing: package or install it through a governed path.
- Agent cannot see the skill/tool: inspect guest permissions and worker
  environment.

## Boundaries

- Do not deploy untested artifacts.
- Do not deploy raw credentials.
- Do not use manual guest edits when the deploy command covers the path.
- Do not deploy outside declared guest directories.
- Do not make deployment a provider lifecycle operation.
