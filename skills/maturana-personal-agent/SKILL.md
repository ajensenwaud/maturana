# maturana-personal-agent

Use this skill when turning a VM-backed coding agent into a personal Maturana
agent with durable identity, memory, context, heartbeat, schedules, channels,
and deployable capabilities.

Personal-agent behavior is host-managed. The guest harness should execute
turns; it should not own pairing, long-term memory policy, context selection,
or channel delivery.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the target agent id and read the materialized
   `.maturana/agents/<agent-id>/MATURANA.md` when it exists.
3. Inspect current personal-agent files before overwriting anything:
   - `AGENTS.md`
   - `SOUL.md`
   - `memory/MEMORY.md`
   - `context/README.md`
   - `HEARTBEAT.json`
   - `schedules/schedules.json`
   - `channels/telegram/*.md`
   - `channels/telegram/*.context.json`
4. Inspect live state with `maturana agent inspect <agent-id> --live` when the
   request depends on a running VM.
5. Inspect session inbox/outbox and channel heartbeat before changing channel
   code.

## Preflight

- Confirm whether this is first-time initialization, channel repair, memory
  update, schedule setup, or context reload.
- Preserve existing `MEMORY.md`, `SOUL.md`, and channel transcripts unless the
  user explicitly asks to reset them.
- Confirm `/new` should rotate only the conversation context, not durable
  memory/wiki state.
- Confirm sessiond and the guest runner are healthy before changing channel
  adapters.
- Confirm no raw tokens are present in memory, wiki, transcripts, or context.

## Decision Path

- First-time personal agent: run `personal init` with the intended spec.
- Existing personal agent: preserve identity and memory by default; use
  `--force` only when the user explicitly wants scaffolding overwritten.
- Memory update: write durable user facts to `memory/MEMORY.md`; do not depend
  on the harness context window.
- New conversation context: use `/new` in the channel. It archives the current
  transcript and reloads durable memory/wiki context on the next turn.
- Wiki/shared context: ingest markdown through the wiki skill, then let channel
  turns select relevant chunks from the current message plus recent transcript.
- Heartbeat: use `heartbeat beat|status` for liveness, not placeholder chat
  replies.
- Schedules: use `schedule add|list|run-due|serve`; schedule runs enqueue
  ordinary session messages and record run history.
- Telegram: pair before serving; ignore unpaired chats.
- Discord: treat current Discord support as outbound notification unless a
  dedicated inbound bridge has been implemented and tested.

## Actions

Initialize or preserve personal-agent files:

```powershell
.\scripts\maturana.ps1 personal init <agent-id> --spec .\examples\MATURANA.codex-hyperv.md
```

Use `--force` only for explicit identity/memory scaffold replacement.

Heartbeat:

```powershell
.\scripts\maturana.ps1 heartbeat beat <agent-id> --status alive --message "ready"
.\scripts\maturana.ps1 heartbeat status <agent-id>
```

Schedules:

```powershell
.\scripts\maturana.ps1 schedule add <agent-id> morning `
  --cron "0 9 * * *" `
  --prompt "Send a morning brief" `
  --channel telegram

.\scripts\maturana.ps1 schedule list <agent-id>
.\scripts\maturana.ps1 schedule run-due <agent-id> --session-id telegram-main
```

Telegram:

```powershell
.\scripts\maturana.ps1 channel pair telegram start --agent-id <agent-id>
# Ask the user to send the printed /pair CODE to the bot.
.\scripts\maturana.ps1 channel pair telegram complete --agent-id <agent-id>
.\scripts\maturana.ps1 channel serve telegram --agent-id <agent-id>
```

Discord notification:

```powershell
.\scripts\maturana.ps1 notify discord `
  --webhook-source pipelock:discord/webhook `
  --message "Maturana is alive"
```

## Evidence

Before claiming success, collect the evidence matching the operation:

- Initialization: the agent directory contains `AGENTS.md`, `SOUL.md`,
  `memory/MEMORY.md`, `context/README.md`, `schedules/`, `skills/`, and
  `tools/`.
- Memory/context: `memory/MEMORY.md` contains the durable note and the next
  channel turn's `.context.json` lists loaded memory/wiki files, context
  policy, query term sources, and matched terms per wiki chunk.
- `/new`: the old Telegram transcript and context manifest moved under
  `channels/telegram/archive/`; the new transcript contains the fresh-session
  marker and the next turn writes a fresh context manifest.
- Heartbeat: `HEARTBEAT.json` and `HEARTBEAT.md` show the latest status.
- Schedule: `schedules/schedules.json` contains the schedule and
  `schedules/last-run.json` records due runs.
- Telegram: pair status is true, heartbeat is current, transcript has inbound
  and outbound turns, and session outbox delivery is marked.

## Recovery

- Bot does not respond: inspect pair status, channel heartbeat, transcript,
  context manifest, session inbox/outbox, and runner logs before changing code.
- Agent forgets context: inspect the latest `.context.json`, `MEMORY.md`, wiki
  chunks, query term sources, matched terms, and transcript archive state.
- Wrong chat can message the bot: check pipelock paired chat id and deny
  unpaired chats; do not remove pairing checks.
- Schedules do not run: inspect schedule records and run history before editing
  prompts.
- Stale live VM: inspect provider state and session heartbeat before restarting
  services.

## Boundaries

- Do not use placeholder "working on it" chat messages; use Telegram chat
  actions for activity.
- Do not accept arbitrary inbound chats.
- Do not put OAuth credentials into pipelock. Inject Codex and Claude OAuth
  state directly into VMs.
- Do not invent a command queue for guest execution.
- Do not make the guest harness the owner of memory policy or channel pairing.
- Do not overwrite `AGENTS.md`, `SOUL.md`, or `MEMORY.md` without explicit user
  intent.
