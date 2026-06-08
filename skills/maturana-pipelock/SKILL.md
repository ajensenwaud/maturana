# maturana-pipelock

Use this skill when a user wants to store, list, read, or delete non-OAuth
secrets for Maturana.

## Procedure

Initialize the local vault:

```powershell
.\scripts\maturana.ps1 pipelock init
```

Store a secret:

```powershell
.\scripts\maturana.ps1 pipelock set telegram/bot-token --value "<token>"
```

Reference it in specs or commands as:

```text
pipelock:telegram/bot-token
```

Create or print the public HTTPS MITM CA certificate path:

```powershell
.\scripts\maturana.ps1 pipelock ca-cert
```

List stored names:

```powershell
.\scripts\maturana.ps1 pipelock list
```

Read or delete a value only when the user explicitly asks:

```powershell
.\scripts\maturana.ps1 pipelock get telegram/bot-token
.\scripts\maturana.ps1 pipelock delete telegram/bot-token
```

Keep this simple. Pipelock is currently a local encrypted vault for ordinary
API tokens and bot tokens. Do not use it for Codex or Claude OAuth state, and do
not use it for harness OAuth state.

Run the simple HTTP egress proxy:

```powershell
.\scripts\maturana.ps1 pipelock proxy --spec .\examples\MATURANA.codex-hyperv.md
```

Or run it with explicit one-off policy flags:

```powershell
.\scripts\maturana.ps1 pipelock proxy `
  --bind 127.0.0.1:47833 `
  --allow api.example.test `
  --inject-header api.example.test:Authorization=pipelock:api/token
```

The MVP proxy supports HTTP proxy requests, allowlist enforcement, pipelock
header injection, HTTPS `CONNECT` tunneling for allowlisted hosts, targeted
HTTPS MITM for hosts with `inject_headers`, and JSONL audit logging. Do not add
a queue or external credential manager.
