# maturana-deploy

Use this skill when deploying a Codex-developed skill, tool, or MCP server
bundle into a running Maturana guest.

## Deploy A Skill

```powershell
.\scripts\maturana.ps1 deploy skill <agent-id> .\skills\my-skill --ip <guest-ip>
```

Default guest destination:

```text
/agent/skills/<folder-or-file-name>
```

## Deploy A Tool

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

## Rules

- Build and test tools in the Codex host environment first.
- Deploy only to `/agent/skills` or `/agent/tools` unless the spec declares a
  different writable mount.
- Do not put channel credentials or OAuth tokens inside deployed tools.
- Use pipelock for non-OAuth secrets and guest harness auth injection for
  Codex/Claude OAuth.
