# Agent Orchestration

This document explains how Maturana's host runtime plane is wired, what was
broken, and how `maturana up` and the hardened session queue fix it.

## The runtime plane

A working agent needs several long-lived host processes plus the in-VM worker:

| Process | Command | Role |
| --- | --- | --- |
| Session queue | `maturana session serve` | HTTP queue (`/session/claim`, `/outbound`, `/complete`, `/heartbeat`) over per-agent SQLite |
| Channel bridge | `maturana channel serve telegram` | Polls Telegram, enqueues inbound, delivers outbound |
| Schedule runner | `maturana schedule serve` | Fires cron schedules into the queue |
| Guest worker | `run-agent.sh` (in the VM) | Claims messages over HTTP, runs the harness, posts replies |

The message path for one Telegram turn:

```
Telegram → channel bridge → inbound.sqlite ← (HTTP) ← guest worker → harness
                                   ↑                        ↓
                              outbound.sqlite ← (HTTP /session/outbound)
   Telegram ← channel bridge ← outbound.sqlite (delivered on next poll)
```

## What was broken

1. **Nothing started or supervised the plane.** The pieces existed but an
   operator had to launch three to five `serve` processes by hand and keep
   their `--session-id`, bind address, and token in sync. The classic failure:
   the channel writes to session `telegram-main` while the guest worker claims
   from `default` — the message is enqueued to one queue and claimed from
   another, so the agent silently never replies.

2. **The queue leaked in-flight work.** `claim_pending_inbound` set a message
   to `processing` but nothing ever reclaimed it if the worker crashed
   mid-turn. There was no visibility timeout and no dead-letter, so a single
   crashed turn wedged that message forever and the user's request vanished.

## The fix

### One supervised process group: `maturana up`

`maturana up` builds an internally consistent plan from a single
[`OrchestratorConfig`](../crates/maturana-core/src/orchestrator.rs) and
supervises every process as a restart-on-failure group:

```
maturana up                       # supervise every materialized agent
maturana up --agent-id personal   # one agent
maturana up --dry-run             # print the resolved plan + guest session ids
```

The plan derives the channel bridge's `--session-id` and the schedule runner's
`--session-id` from the *same* `session_id` field, and exposes
`orchestrator::guest_session_id` so the guest-worker installer claims from that
exact queue. The session id can no longer drift between producer and consumer.
`sessiond` is marked critical (its failure stops the plane); channel and
schedule processes restart with exponential backoff and their restart budget
resets after they stay up for a minute.

### Lease, retry, and dead-letter in the queue

The inbound queue now applies a [`ClaimPolicy`](../crates/maturana-core/src/session_db.rs):

- A claimed message is **leased** for `lease_seconds` (default 300).
- On the next claim, any message whose lease expired is **recovered**:
  requeued with a backoff while it still has retries left, or **dead-lettered**
  to `failed` once it has been attempted `max_tries` times (default 5).
- `queue_stats`, `list_dead_letters`, and `requeue_inbound` make stuck work
  visible and recoverable instead of invisible and permanent.

This means a crashed guest turn is retried automatically, and a
persistently-failing message stops blocking the queue and surfaces to the
operator rather than disappearing.

## Operating checklist

1. `maturana agent launch MATURANA.md --apply` and install the guest worker.
2. `maturana channel pair telegram start` / `complete`.
3. `maturana up --dry-run` and confirm the guest session id matches the
   worker's `MATURANA_SESSION_ID`.
4. `maturana up` and verify with `maturana doctor`.

## Durable orchestration boards

Two front doors sit over one engine (A2A dispatch + roles + host-enforced
budgets, every card/step running in an agent's own VM):

- **`orchestrator loop` (`/loop`)** — give it a goal; an LLM coordinator
  decomposes it into steps and runs them once. Ephemeral: goal → plan → done.
- **orchestration boards (`maturana board`)** — *you* author the work as a
  durable, editable board of cards. Use this for an ongoing, multi-step,
  multi-owner pipeline you re-run, schedule, or trigger.

### Model

A **board** is a named JSON file at `<home>/board/<name>.json` — the single
source of truth. A **card** is one unit of work:

| Field | Meaning |
| --- | --- |
| `title` / `detail` | what to do (+ acceptance criteria) |
| `assignee` | a **role** (`developer`/`researcher`/`reviewer`/`coordinator`/`synthesizer`) or a concrete agent id; empty ⇒ `developer` |
| `deps` | card ids that must be `done` before this one is `ready` |
| `status` | `todo` → `doing` → `done` (or `blocked` on failure) |
| `result` | the worker's reply, stored back on the board |

**Coordinate through the board, not shared memory.** A card's only view of its
upstreams is their stored `result` (folded into its prompt as "inputs from
earlier cards"). Agents never share process state or talk directly — exactly the
"state on the board" model.

### Running it — the dispatcher

`maturana board run <name>` claims every **ready** card (todo + all deps done),
marks it `doing`, and dispatches it to its assignee **over A2A — in that agent's
own VM**, never a local/Docker shortcut. Ready cards run in **parallel** up to
`max_parallel`; as cards finish they unblock dependents, and the loop drains the
board. Same host-enforced caps as the loop (turns / wall-clock / parallel /
concurrent-VMs — tighten-only, never raisable by an agent) and the same
real-artifact collection (files a card writes to its out-dir are scp'd off the VM
into the run's output).

### Durable

- **Atomic store** — each save writes a temp file + fsync + rename, so a crash
  mid-write can never corrupt the board.
- **Reclaim** — a run interrupted by a crash/restart leaves cards in `doing`; the
  next `board run` resets those back to `todo` and resumes (a dead task is
  reclaimed, not stuck). `attempts` is preserved.
- **Run log** — every claim / done / blocked / reclaim is appended to
  `<home>/board/<name>.events.jsonl` for audit and the cockpit's live activity feed.

### Three ways to fire a board

1. **Manually** — the cockpit **Orchestration** view's *Run* button, or
   `maturana board run <name>`.
2. **On a schedule (cron)** — a schedule with a board target runs it unattended:
   `maturana schedule add <agent> nightly --cron "0 2 * * *" --board <name>`
   (or set the *board* field when adding a schedule in the cockpit). The cron is
   the trigger; the board is the work.
3. **By trigger** — `POST /api/boards/<name>/run` (cockpit-authenticated) lets any
   external event start a board.

### Cockpit

The **Orchestration** view is a board editor + live monitor: create/delete
boards, add/edit/delete cards (title, detail, assignee dropdown of roles +
agents, dependency picker), then *Run* and watch the To-do / Doing / Done /
Blocked columns and the run log update live. *Reset* clears results for a clean
re-run.

### Boundaries (zero-trust)

A card always runs in its assignee's VM over A2A — there is no weaker execution
substrate. Caps are host-enforced and unraiseable from inside a card. A board is
a task list for agents, not a way to reconfigure the fleet; don't give a card
work that rewrites another agent's identity files.
