# maturana-personal-agent

Use this skill when turning a VM-backed coding agent into a personal Maturana
agent with durable identity, memory, context, heartbeat, schedules, and
deployable capabilities.

## Initialize

Read `AGENTS.md` first. Then initialize the personal-agent files:

```powershell
.\scripts\maturana.ps1 personal init <agent-id> --spec .\examples\MATURANA.codex-hyperv.md
```

This creates or preserves:

- `.maturana/agents/<agent-id>/AGENTS.md`
- `.maturana/agents/<agent-id>/SOUL.md`
- `.maturana/agents/<agent-id>/memory/MEMORY.md`
- `.maturana/agents/<agent-id>/context/README.md`
- `.maturana/agents/<agent-id>/schedules/schedules.json`
- `.maturana/wiki/INDEX.md`

Use `--force` only when the user explicitly wants to overwrite local agent
identity or memory scaffolding.

## Heartbeat

```powershell
.\scripts\maturana.ps1 heartbeat beat <agent-id> --status alive --message "ready"
.\scripts\maturana.ps1 heartbeat status <agent-id>
```

Heartbeat writes both markdown and JSON:

- `.maturana/agents/<agent-id>/HEARTBEAT.md`
- `.maturana/agents/<agent-id>/HEARTBEAT.json`

## Schedules

Store schedule definitions:

```powershell
.\scripts\maturana.ps1 schedule add <agent-id> morning `
  --cron "0 9 * * *" `
  --prompt "Send a morning brief" `
  --channel telegram

.\scripts\maturana.ps1 schedule list <agent-id>
```

The MVP stores schedules but does not yet run a scheduler daemon. Do not invent
a queue. The scheduler loop should later call direct agent-run and notify
commands.

## Channels

Telegram:

```powershell
.\scripts\maturana.ps1 channel pair telegram start
# Ask the user to send the printed /pair CODE to the bot.
.\scripts\maturana.ps1 channel pair telegram complete
.\scripts\maturana.ps1 channel serve telegram --agent-id <agent-id> --ip <guest-ip>

.\scripts\maturana.ps1 notify telegram `
  --token-source pipelock:telegram/bot-token `
  --message "Maturana is alive"
```

Do not accept arbitrary inbound Telegram chats. Only send to the paired chat id
stored in `pipelock:telegram/chat-id`, unless the user explicitly overrides
`--chat-id-source`.

Use `channel serve telegram` for the interactive bot loop. `notify telegram` is
only for explicit outbound messages.

Discord:

```powershell
.\scripts\maturana.ps1 notify discord `
  --webhook-source pipelock:discord/webhook `
  --message "Maturana is alive"
```

Keep non-OAuth channel credentials in pipelock where possible.
