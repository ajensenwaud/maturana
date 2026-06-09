# maturana-spawn

Use this skill when spawning a sub-agent from a paired channel command or host
operator action.

Sub-agents are records plus isolated session namespaces. They may become
persistent agent workstreams later, but the MVP should stay simple and route
work through sessiond.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the parent agent id and read its `MATURANA.md`.
3. Confirm the requested sub-agent purpose is within the parent agent contract.
4. Inspect existing sub-agent records under:
   - `.maturana/agents/<parent-agent>/subagents/`
5. Inspect existing sessions to avoid id collisions:
   - `.maturana/agents/<parent-agent>/sessions/`

## Preflight

- Confirm the channel or operator is authorized to spawn work.
- Confirm the requested sub-agent stays within the parent contract.
- Confirm the sub-agent id will not collide with existing records or sessions.
- Confirm ephemeral versus persistent behavior is explicit.
- Confirm the prompt can be routed through sessiond without adding a new
  command queue.

## Decision Path

- Short one-off investigation: spawn `ephemeral`.
- Durable watcher or recurring role: spawn `persistent`.
- Needs broader permissions than parent: reject or require a new full agent
  spec; do not inherit unbounded permissions.
- Needs separate VM isolation: create a normal Maturana agent instead of a
  lightweight sub-agent record.
- Channel command: parse `/spawn <mode?> <name> -- <prompt>` and enqueue the
  prompt into `subagent-<subagent-id>`.

## Actions

From Telegram or another paired channel:

```text
/spawn ephemeral researcher -- investigate the bug and report findings
/spawn persistent reviewer -- watch this repository for risky changes
```

Maturana records sub-agents under:

```text
.maturana/agents/<parent-agent>/subagents/<subagent-id>.json
```

Each spawn gets an isolated session namespace:

```text
subagent-<subagent-id>
```

Persistent sub-agents are durable records. Ephemeral sub-agents can be cleaned
up after their work is complete.

## Evidence

Before claiming success, collect:

- Sub-agent JSON record exists with id, parent id, mode, prompt, and timestamp.
- Session directory exists for `subagent-<subagent-id>`.
- Initial inbound message is queued in that session.
- Parent channel transcript/audit records the spawn request.
- For persistent sub-agents, the record remains after the first turn.

## Recovery

- Bad `/spawn` syntax: show the expected command format.
- Duplicate sub-agent id: choose a clearer name or inspect existing record.
- Prompt outside parent scope: reject and ask for a new agent spec if needed.
- Sub-agent never runs: inspect the isolated session inbox/outbox and runner
  heartbeat.
- Persistent record should be removed: archive/delete the sub-agent record only
  after user confirmation.

## Boundaries

- Do not create a separate command queue for sub-agents.
- Do not grant permissions beyond the parent agent contract.
- Do not treat lightweight sub-agents as VM-isolated agents.
- Do not spawn from unpaired channels.
- Do not keep ephemeral records forever when cleanup is requested.
