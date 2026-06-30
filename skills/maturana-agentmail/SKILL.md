# maturana-agentmail

Use this skill when giving a Maturana agent an email address through
AgentMail.to.

AgentMail is kept out of core. Treat it as a deployable tool or MCP bundle that
uses pipelock-managed credentials and writes assigned addresses into agent
memory/context.

## Grounding

1. Read `AGENTS.md` first.
2. Confirm the target agent contract allows email capability.
3. Inspect the target agent memory/context files:
   - `.maturana/agents/<agent-id>/memory/MEMORY.md`
   - `.maturana/agents/<agent-id>/context/README.md`
4. Confirm `agentmail/api-key` exists in pipelock without printing it.
5. Inspect existing deployed tools before adding a new AgentMail tool.

## Preflight

- Confirm the user wants this as an optional deployed integration, not a core
  Maturana feature.
- Confirm `agentmail/api-key` is named in pipelock and that no key material is
  present in the repo, spec, or tool source.
- Confirm the target agent can receive normal session/channel messages before
  adding email delivery.
- Confirm the local tool has a smoke path that can create or inspect an inbox
  without printing the API key.

## Decision Path

- No AgentMail key: store it with `pipelock set agentmail/api-key`.
- Need a new inbox: create/deploy an AgentMail tool or MCP bundle outside core.
- Need the agent to remember the address: write the assigned email to memory
  and context after inbox creation.
- Need inbound email handling: bridge AgentMail events into the normal session
  path; do not add a separate queue.
- Need secret access: inject `AGENTMAIL_API_KEY` from pipelock at tool runtime.

## Actions

Store the API key:

```powershell
maturana pipelock set agentmail/api-key --value <token>
```

Scaffold the local tool:

```powershell
maturana develop tool agentmail-inbox
```

Create an inbox from the tool using AgentMail's API:

```bash
curl -sS https://api.agentmail.to/v0/inboxes \
  -H "Authorization: Bearer $AGENTMAIL_API_KEY" \
  -H "content-type: application/json" \
  --data '{"display_name":"Maturana Agent","client_id":"<agent-id>"}'
```

The response should include `inbox_id` and `email`.

Deploy the tested tool:

```powershell
maturana deploy tool <agent-id> .\tools\agentmail-inbox --ip <guest-ip>
```

## Evidence

Before claiming success, collect:

- `pipelock list` shows `agentmail/api-key`.
- The AgentMail response includes `inbox_id` and `email`.
- The assigned email appears in `memory/MEMORY.md` and agent context.
- The tool is deployed under `/agent/tools` or a declared guest path.
- A smoke test can read the email value without printing the API key.

## Recovery

- AgentMail returns unauthorized: verify pipelock name and runtime env
  injection, then rotate the token if exposed.
- Inbox already exists: reuse the existing email if it belongs to the target
  agent; do not create duplicates blindly.
- Tool cannot reach AgentMail: check pipelock egress allowlist/proxy policy.
- Agent forgets address: inspect `MEMORY.md`, context, and latest context
  manifest.

## Boundaries

- Do not add AgentMail to Maturana core.
- Do not store the AgentMail API key in specs, docs, skills, audit logs, or
  committed files.
- Do not put AgentMail credentials inside deployed tool source.
- Do not bypass pipelock for non-OAuth API credentials.
- Do not create inbound email queues outside the normal session path.
