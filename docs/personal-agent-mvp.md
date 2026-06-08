# Personal Agent MVP

Maturana's personal-agent layer is deliberately file-first and command-driven.
The VM remains the isolation boundary; the host stores durable agent state as
plain markdown and JSON under `.maturana`.

## Design Borrowed From NanoClaw

Useful NanoClaw patterns we keep:

- One durable context folder per agent.
- Human-readable markdown memory files.
- Host channel adapters do not run the agent directly.
- A warm runner owns the harness and reads inbound work from durable session
  state.
- Outbound channel delivery is tracked separately from agent execution.
- Shared global context plus per-agent context.
- Scheduled tasks as durable records, run in the agent's context.
- Channels are adapters, not the core product.
- New capabilities should be installed as skills or tools instead of expanding
  the runtime.

Maturana differences:

- The worker runs in Hyper-V or Firecracker, not Docker.
- Codex is the host-side orchestration surface.
- OAuth harness credentials are injected directly into the VM.
- Other credentials go through pipelock and the egress proxy.

## Files

Per agent:

```text
.maturana/agents/<agent-id>/
  MATURANA.md
  AGENTS.md
  SOUL.md
  HEARTBEAT.md
  HEARTBEAT.json
  context/README.md
  memory/MEMORY.md
  memory/daily/
  schedules/schedules.json
  skills/
  tools/
```

Shared context:

```text
.maturana/wiki/
  INDEX.md
  chunks/*.md
```

Session state:

```text
.maturana/agents/<agent-id>/sessions/<session-id>/
  inbound.sqlite
  outbound.sqlite
```

`inbound.sqlite` is host-owned. Channel adapters append messages there, and the
runner claims pending rows. `outbound.sqlite` is runner-owned. The host delivery
loop reads outbound rows and records delivered message ids back in
`inbound.sqlite`.

The invariant is simple: channels enqueue and deliver; runners think and write
responses. No channel path should restart systemd or shell out per message.

## Commands

Initialize personal-agent files:

```powershell
.\scripts\maturana.ps1 personal init codex-demo --spec .\examples\MATURANA.codex-hyperv.md
```

Initialize and ingest shared wiki context:

```powershell
.\scripts\maturana.ps1 wiki init
.\scripts\maturana.ps1 wiki ingest .\AGENTS.md --title Repo-Agents
.\scripts\maturana.ps1 wiki search secure --limit 5
```

Write and inspect heartbeat:

```powershell
.\scripts\maturana.ps1 heartbeat beat codex-demo --status alive --message "ready"
.\scripts\maturana.ps1 heartbeat status codex-demo
```

Add and list schedules:

```powershell
.\scripts\maturana.ps1 schedule add codex-demo morning `
  --cron "0 9 * * *" `
  --prompt "Send a morning brief" `
  --channel telegram

.\scripts\maturana.ps1 schedule list codex-demo
```

Deploy a skill or tool into a running guest:

```powershell
.\scripts\maturana.ps1 deploy skill codex-demo .\skills\maturana-pipelock `
  --ip 172.26.x.y

.\scripts\maturana.ps1 deploy tool codex-demo .\target\x86_64-pc-windows-gnu\debug\maturana.exe `
  --ip 172.26.x.y `
  --guest-path /agent/tools/maturana.exe
```

## Channels

Telegram:

```powershell
.\scripts\maturana.ps1 channel pair telegram start
# Send the printed `/pair CODE` message to the bot from Telegram.
.\scripts\maturana.ps1 channel pair telegram complete
.\scripts\maturana.ps1 channel serve telegram --agent-id codex-demo

# Local session smoke test without a harness:
.\scripts\maturana.ps1 channel serve telegram --agent-id codex-demo --run-once-provider echo

.\scripts\maturana.ps1 notify telegram `
  --token-source pipelock:telegram/bot-token `
  --message "Maturana is alive"
```

The pairing flow stores the authorized chat id in
`pipelock:telegram/chat-id`. Without pairing, Telegram notification refuses to
send unless an explicit `--chat-id-source` is passed.

`channel serve telegram` is the product path for an interactive bot. It polls
Telegram, ignores unpaired chats, audits inbound/outbound events, routes normal
messages into the selected agent session, and delivers replies from the session
outbox. `notify telegram` is only a low-level outbound utility.

Channel serving is stateful. For each paired Telegram chat, Maturana stores a
markdown transcript under:

```text
.maturana/agents/<agent-id>/channels/telegram/<chat-id>.md
```

Every agent turn is built from the current message plus:

- `AGENTS.md`
- `SOUL.md`
- `MATURANA.md`
- `memory/MEMORY.md`
- `context/README.md`
- `.maturana/wiki/INDEX.md`
- the recent Telegram transcript

Messages containing `remember` are appended to `memory/MEMORY.md` before the
agent turn, so the next turn sees the durable note even if the harness itself is
stateless.

Session CLI:

```powershell
.\scripts\maturana.ps1 session init codex-demo --session-id telegram-main
.\scripts\maturana.ps1 session enqueue codex-demo `
  --session-id telegram-main `
  --channel telegram `
  --platform-id 12345 `
  --text "hello"
.\scripts\maturana.ps1 session run-once codex-demo --session-id telegram-main --provider echo
.\scripts\maturana.ps1 session outbox codex-demo --session-id telegram-main --mark-delivered
```

`echo` is only a smoke-test provider. The real runner will be a long-lived guest
process that claims inbound rows, calls Codex/Claude Code/OpenCode, and writes
outbound rows.

Discord webhook:

```powershell
.\scripts\maturana.ps1 notify discord `
  --webhook-source pipelock:discord/webhook `
  --message "Maturana is alive"
```

## Current Boundary

This MVP stores schedule definitions but does not yet run a scheduler loop.
The next simple step is a hostd loop that periodically reads
`schedules/schedules.json`, launches agent turns, writes run history, and sends
channel messages through the existing notify commands.

Do not add a general command queue. Use direct `agent run` for manual VM work,
session DBs for channel turns, `deploy` for tool/skill installation, and
schedule records for cron-like behavior.
