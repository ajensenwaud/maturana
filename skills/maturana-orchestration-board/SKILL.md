# maturana-orchestration-board

Use this skill when you want a **durable, editable** plan of work run across
several agents — an ongoing pipeline you author by hand, re-run, schedule, or
trigger — rather than a one-shot goal. (For "give me an answer to one big goal,
once," use `maturana-orchestrator-loop` instead; for the difference, see below.)

A **board** is a persistent list of **cards**. Each card is one task with a
title, optional detail, an **assignee** (a role like `developer`/`researcher`/
`reviewer`, or a concrete agent id), and **dependencies** (other card ids that
must finish first). Running the board dispatches every *ready* card (deps done)
to its assignee **in that agent's own VM over A2A**, in parallel where possible,
and writes each result back onto the board. Cards coordinate only through their
written results — there is no shared state. It is durable: the board survives
restarts, an interrupted run is reclaimed, and every step is logged.

## Grounding

1. Read `AGENTS.md` first.
2. The board store is `<home>/board/<name>.json`; the run log is
   `<name>.events.jsonl`. The engine + safety caps are the orchestrator's
   (`maturana-orchestrator-loop`, `maturana-a2a` for the wire).

## Build and run a board

```
maturana board add "Research the topic"   --assignee researcher
maturana board add "Write the brief"       --assignee developer --needs c1
maturana board add "Review it"             --assignee reviewer  --needs c2
maturana board list          # see the columns + deps
maturana board run           # dispatch ready cards across agents until drained
maturana board status        # todo/doing/done/blocked counts
```

- `--board <name>` selects a named board (default: `default`).
- A card's assignee may be a role (resolved to an agent, reused by default) or a
  concrete agent id. Omit it to default to `developer`.
- `maturana board move <card> <status>` and `maturana board reset` adjust state;
  `maturana board run` reclaims any card left `doing` by a previous crash.

## Run it on a schedule or by trigger

- **Cron:** `maturana schedule add <agent> <name> --cron "0 2 * * *" --board <board>`
  runs the board unattended (the cron is the trigger; the board is the work).
- **Trigger / web:** the cockpit **Orchestration** view creates/edits/runs boards
  and shows live status; `POST /api/boards/<name>/run` lets an external event fire one.

## Decision: board vs loop

- **Ongoing, editable, multi-owner task list, re-run/scheduled/triggered** → board.
- **One-shot goal, no need to keep the plan** → `maturana-orchestrator-loop`.

## Boundaries

- A card always runs in its assignee's VM over A2A — never a local/Docker/SSH
  shortcut. That is the zero-trust line.
- Caps (turns/wall/parallel/VMs) are host-enforced and cannot be raised from
  inside a card. A budget stop means the board was too big — split it.
- A card is a task *for* an agent, not a way to reconfigure the fleet. Do not
  give a card work that rewrites another agent's identity files.
- A card pointed at a non-running assignee will wait for that agent to claim it —
  make sure the assignee agent is up before running.
