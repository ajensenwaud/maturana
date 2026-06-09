# maturana-deploy

Use this skill when deploying a Codex-developed skill, tool, or MCP server
bundle into a running Maturana guest.

Deployment is the last step of a host-side development workflow. Build and test
locally first, then copy only the intended artifact into declared guest paths.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the target agent id and read its materialized `MATURANA.md`.
3. Inspect live state with `maturana agent inspect <agent-id> --live`.
4. Confirm the artifact exists locally and is not a secrets directory.
5. Confirm the guest destination is allowed:
   - `/agent/skills`
   - `/agent/tools`
   - a declared writable mount in the spec

## Preflight

- Confirm the artifact was built or smoke-tested locally.
- Confirm the target VM is running and has a known guest IP.
- Confirm the destination path is one of the declared deployment roots.
- Scan the artifact for raw secrets before copying it into the guest.
- Confirm runtime credentials are supplied through pipelock or direct OAuth
  auth injection, not embedded files.

## Decision Path

- Deploy a Codex skill: use `deploy skill`.
- Deploy a binary/script/MCP bundle: use `deploy tool`.
- Need a fixed MCP path: pass `--guest-path /agent/tools/mcp/<name>`.
- Guest IP missing: use provider inspect first; use `--ip` only as a recovery
  override.
- Artifact not tested: stop and test in the Codex host workspace first.
- Secret required at runtime: store it in pipelock and inject through env/tool
  config, not source files.

## Actions

Deploy a skill:

```powershell
.\scripts\maturana.ps1 deploy skill <agent-id> .\skills\my-skill --ip <guest-ip>
```

Default guest destination:

```text
/agent/skills/<folder-or-file-name>
```

Deploy a tool:

```powershell
.\scripts\maturana.ps1 deploy tool <agent-id> .\path\to\tool --ip <guest-ip>
```

Default guest destination:

```text
/agent/tools/<folder-or-file-name>
```

Use `--guest-path` for MCP bundles or binaries that need a fixed path:

```powershell
.\scripts\maturana.ps1 deploy tool <agent-id> .\mcp\my-server `
  --ip <guest-ip> `
  --guest-path /agent/tools/mcp/my-server
```

## Evidence

Before claiming success, collect:

- Local artifact path and test output.
- Live inspect showing the target VM is reachable.
- Deploy command success.
- Guest-side listing or fetch proving the artifact exists at the destination.
- Audit event for the deploy operation when emitted by the CLI.
- Harness-visible behavior if the deployment is meant to affect agent turns.

## Recovery

- Guest IP unknown: run provider-aware live inspect and retry with explicit
  `--ip` only if needed.
- Permission denied: check target path against declared writable roots.
- Tool fails in guest: inspect interpreter/runtime dependencies and logs.
- MCP bundle missing fixed path: redeploy with `--guest-path`.
- Secret accidentally included: remove artifact, rotate the secret, and
  redeploy without embedded credentials.

## Boundaries

- Do not deploy secrets with tools.
- Do not deploy to arbitrary guest paths outside declared roots.
- Do not use deploy as a replacement for guest filesystem mounts.
- Do not put channel credentials or OAuth tokens inside deployed tools.
- Do not bypass pipelock for non-OAuth secrets.
