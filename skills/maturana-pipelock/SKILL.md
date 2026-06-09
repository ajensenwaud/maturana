# maturana-pipelock

Use this skill when a user wants to store, list, read, delete, inject, or audit
non-OAuth credentials for Maturana.

Pipelock is the governed credential and egress path for ordinary API tokens,
bot tokens, and scoped HTTP header injection. It is not the storage path for
Codex or Claude OAuth harness state.

## Grounding

1. Read `AGENTS.md` first.
2. Identify whether the request is vault management, proxy launch, egress
   debugging, or audit review.
3. Read the target `MATURANA.md` when proxy policy comes from a spec.
4. Inspect current pipelock state without printing secrets:
   - `.maturana/pipelock/`
   - `.maturana/pipelock/vault.json`
   - `.maturana/pipelock/mitm-ca-cert.pem`
   - `.maturana/audit/<agent-id>.jsonl` when tied to an agent
5. Confirm whether the credential is OAuth harness state. If it is Codex or
   Claude OAuth, do not store it in pipelock.

## Preflight

- Classify the credential: ordinary API/bot token, OAuth harness state, or no
  credential at all.
- Confirm ordinary secrets have a specific pipelock name and are not already
  present in specs, docs, skills, or logs.
- Confirm egress hosts are explicit before enabling proxy/header injection.
- Confirm the MITM CA public cert is enough for trust setup; never expose the
  private CA key.
- Confirm audit/log output will not include raw secret values.

## Decision Path

- New local vault: run `pipelock init`.
- Store ordinary API/bot token: run `pipelock set <name> --value ...`.
- Use from spec/tool: reference as `pipelock:<name>`.
- Need HTTPS header injection: ensure target host is explicitly allowlisted and
  configured for injection.
- Need browser/guest trust: provide the public MITM CA certificate path; never
  expose the private CA key.
- Need diagnostics: inspect audit logs and proxy output before changing policy.
- Need external credential manager: defer it. The MVP is local encrypted vault
  plus MITM proxy only.

## Actions

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

Run the proxy from a spec:

```powershell
.\scripts\maturana.ps1 pipelock proxy --spec .\examples\MATURANA.codex-hyperv.md
```

Run it with explicit one-off policy flags:

```powershell
.\scripts\maturana.ps1 pipelock proxy `
  --bind 127.0.0.1:47833 `
  --allow api.example.test `
  --inject-header api.example.test:Authorization=pipelock:api/token
```

## Evidence

Before claiming success, collect:

- Vault initialized: `.maturana/pipelock/key` and vault file exist.
- Secret stored: `pipelock list` shows the expected name, not the value.
- CA ready: `pipelock ca-cert` returns a public cert path.
- Proxy running: bind address is listening and requests are audited.
- Allowlist enforced: disallowed host is denied; allowed host passes.
- Header injection: audit shows the allowed target and injected header name,
  not the secret value.

## Recovery

- Secret missing: verify the exact pipelock name and list stored names.
- Proxy denies a required host: add the smallest allowlist entry for that host.
- Header injection missing: check host match, header config, and secret name.
- HTTPS client fails trust: install the public MITM CA cert in the guest/client
  trust store.
- OAuth state requested: stop and use direct guest auth injection instead.
- Secret printed by mistake: rotate the credential and scrub logs/docs.

## Boundaries

- Do not store Codex or Claude OAuth harness state in pipelock.
- Do not print raw secrets unless the user explicitly asks to read a value.
- Do not write raw secrets into `MATURANA.md`, `AGENTS.md`, `SOUL.md`, skills,
  docs, audit logs, or committed files.
- Do not add a queue or external credential manager for the MVP.
- Do not allow broad wildcard egress when a specific host is known.
- Do not expose the MITM private CA key.
