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
