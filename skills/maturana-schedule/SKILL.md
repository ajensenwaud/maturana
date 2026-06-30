# maturana-schedule

Use this skill when adding, listing, testing, debugging, or running Maturana
cron schedules.

Schedules are durable records that enqueue ordinary session messages when due.
They are not a separate command queue and should not bypass the normal
personal-agent/session path.

## Grounding

1. Read `AGENTS.md` first.
2. Identify the agent id and session id.
3. Inspect existing schedule state:
   - `.maturana/agents/<agent-id>/schedules/schedules.json`
   - `.maturana/agents/<agent-id>/schedules/last-run.json`
   - `.maturana/agents/<agent-id>/sessions/<session-id>/`
4. Inspect heartbeat/channel state when the schedule sends user-visible
   messages.
5. Confirm the prompt is safe to run repeatedly.

## Preflight

- Confirm the cron expression and session id are explicit.
- Confirm the prompt is idempotent enough for repeated scheduled execution.
- Confirm the target session exists or can be initialized normally.
- Run a due check with explicit `--now` before enabling long-running serving.
- Confirm missed-run debugging starts from schedule records, not service
  restarts.

## Decision Path

- Add or replace a schedule: use `schedule add`; schedule ids are slugged from
  names and replacement is intentional.
- Dry-run due behavior: use `schedule run-due` with an explicit `--now` when
  testing.
- Always-on scheduler: use `schedule serve` only after `run-due` works.
- User-visible delivery: choose a session/channel that already has a working
  runner and delivery path.
- Missed run: inspect schedule records and last-run history before changing the
  cron expression.

## Actions

Add a schedule:

```powershell
maturana schedule add <agent-id> <name> `
  --cron "* * * * *" `
  --prompt "Send a status brief" `
  --channel telegram
```

List schedules:

```powershell
maturana schedule list <agent-id>
```

Run due schedules once:

```powershell
maturana schedule run-due <agent-id> --session-id <session-id>
```

Run the simple scheduler loop:

```powershell
maturana schedule serve <agent-id> --session-id <session-id>
```

Test a specific time:

```powershell
maturana schedule run-due <agent-id> `
  --session-id <session-id> `
  --now 2026-06-09T09:00:00Z
```

## Evidence

Before claiming success, collect:

- `schedules.json` contains the schedule id, cron, prompt, channel, and
  `enabled: true`.
- `schedule list` shows the intended schedule.
- `run-due` creates one inbound session message for a due schedule.
- `last-run.json` records the schedule id and minute that ran.
- A second `run-due` in the same minute does not enqueue a duplicate.
- If channel delivery is expected, session outbox and channel transcript show
  the resulting turn.

## Recovery

- Invalid cron: fix the expression; keep to five-field cron syntax.
- Schedule did not run: check `last-run.json`, current time, timezone assumption,
  enabled flag, and session id.
- Duplicate-looking run: verify the run minute; the runner suppresses repeats
  per schedule per minute.
- Prompt unsafe to repeat: narrow the prompt or require manual confirmation.
- Channel delivery missing: debug session/channel delivery, not the scheduler
  record.

## Boundaries

- Do not introduce a separate queue for schedules.
- Do not bypass sessiond; due schedules enqueue normal session messages.
- Do not run destructive scheduled prompts without explicit user intent.
- Do not hide scheduler state in a daemon-only memory structure; records live
  under the agent schedules directory.
- Do not restart live agents blindly when a schedule misses; inspect schedule
  and session state first.
