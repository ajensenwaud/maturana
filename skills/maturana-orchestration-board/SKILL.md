# maturana-orchestration-board

Use this skill when you want a **durable, editable** plan of work run across
several agents — an ongoing pipeline you author by hand, re-run, schedule, or
trigger — rather than a one-shot goal. For "answer one big goal, once," use
`maturana-orchestrator-loop` instead (see Decision Path).

A **board** is a persistent list of **cards**. Each card is one task with a
title, optional detail, an **assignee** (a role like `developer`/`researcher`/
`reviewer`, or a concrete agent id), and **dependencies**. Running the board
dispatches every *ready* card (deps done) to its assignee **in that agent's own
VM over A2A**, in parallel where possible, writing each result back onto the
board. Cards coordinate only through their written results — no shared state. It
is durable: the board survives restarts, an interrupted run is reclaimed, failed
cards auto-retry up to their limit, and every step is logged.

## Grounding

1. Read `AGENTS.md` first.
2. The board store is `<home>/board/<name>.json`; the run log is
   `<name>.events.jsonl`. The engine + host-enforced caps are the orchestrator's
   (see `maturana-orchestrator-loop`, and `maturana-a2a` for the wire).

## Preflight

- Confirm the assignee agents are running (`maturana list`) — a card pointed at a
  stopped agent waits until that agent claims it.
- Confirm `<home>/sessiond/token` exists (the A2A wire needs it).
- `maturana board list` to see the current cards + columns before changing them.

## Decision Path

- Ongoing, editable, multi-owner task list you re-run / schedule / trigger → board.
- One-shot goal, no need to keep the plan → `maturana-orchestrator-loop`.
- A rough idea you want an agent to break down → add it `--triage`, then
  `maturana board decompose <id>` (fan into child cards) or `specify <id>`.

## Actions

```
maturana board add "Research the topic"  --assignee researcher
maturana board add "Write the brief"      --assignee developer --needs c1 --max-retries 1
maturana board add "Review it"            --assignee reviewer  --needs c2 --goal
maturana board list                       # cards by column
maturana board run                        # dispatch ready cards across agents until drained
maturana board show c2                    # one card: detail, deps, result, comments, runs
maturana board comment c2 "note for the worker"
maturana board move c3 todo               # re-arm a blocked card
maturana board decompose c1               # LLM fans a triage card into children
```

- `--board <name>` selects a named board (default `default`); `--priority`,
  `--scheduled-at`, `--tenant`, `--goal-max-turns` further shape a card.
- Schedule a board: `maturana schedule add <agent> nightly --cron "0 2 * * *" --board <name>`.
- From the cockpit: the **Orchestration** view creates/edits/runs boards, opens a
  card drawer (comments, run history, attachments, decompose/specify), and shows
  live status. `POST /api/boards/<name>/run` is a programmatic trigger.

## Evidence

- A real parallel run prints multiple `-> card cN … -> <agent>` lines together
  *before* any `<- card done` (proves parallel dispatch across agents).
- `maturana board status` shows cards moving `todo → doing → done`, and the run
  ended by *draining* — not by hitting a budget line.
- File-producing cards print `wrote N file(s) to <dir>` with real files under the
  output dir (the deliverable is the agents' bytes, not a retype).
- `maturana board show <id>` shows the stored result + one run-history row per
  attempt; `<name>.events.jsonl` logs every claim / done / blocked.

## Recovery

- A failed card auto-retries up to `--max-retries`, then becomes `blocked` with a
  reason — read it (`board show <id>`), fix the cause, `board move <id> todo`, re-run.
- A crash mid-run leaves cards `doing`; the next `board run` reclaims them
  (`reclaimed N interrupted card(s)`) — no manual cleanup.
- A `[stuck …]` line means a card is blocked by a failed dependency — unblock or
  fix the dependency first, then re-run.
- Separate "the dispatch errored" (A2A layer — see `maturana-a2a`) from "the
  card's work failed" (the task) — they have different fixes.

## Boundaries

- Do not add a local/Docker/SSH execution shortcut — a card always runs in its
  assignee's own VM over A2A. That is the zero-trust line.
- Do not try to raise the host-enforced caps (turns/wall/parallel/VMs) from inside
  a card; a budget stop means the board was too big — split it.
- Do not give a card work that rewrites another agent's identity files — a card is
  a task for an agent, not a way to reconfigure the fleet.
