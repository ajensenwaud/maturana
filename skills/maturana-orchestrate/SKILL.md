# maturana-orchestrate

Use this skill when bringing an agent's host runtime plane online or diagnosing
why a paired agent never replies.

It supervises the session queue, channel bridges, and schedule runners as one
restart-on-failure group with a single source of truth for session wiring.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/orchestration.md` for the runtime-plane model and message path.
3. Read the target agent `MATURANA.md` for runtime, channels, and schedules.
4. Confirm the guest worker's `MATURANA_SESSION_ID` and the channel session id.
5. Inspect current health with `maturana doctor` before changing anything.

## Preflight

- Confirm the agent is materialized and the guest worker is installed.
- Confirm Telegram pairing is complete for channel-served agents.
- Confirm `sessiond` bind address and token match the guest worker config.
- Confirm no second copy of the plane is already running for this agent.

## Decision Path

- Plane is down or unsupervised: start it with `maturana up`.
- Session ids might drift: run `maturana up --dry-run` and compare to the worker.
- Single component flaps: read its logs; the supervisor restarts non-critical
  processes with backoff, so fix the root cause rather than restart loops.
- A message is stuck or lost: inspect the queue dead-letter and requeue it.
- Provider/VM lifecycle issue: hand off to the launch/inspect/snapshot skills.

## Actions

1. Resolve the plan: `maturana up --dry-run` and verify the printed guest
   session id equals the worker's `MATURANA_SESSION_ID`.
2. Start the plane: `maturana up` (or `--agent-id <id>` for one agent).
3. Verify end to end with `maturana doctor` and a real Telegram round trip.
4. For stuck work, list dead-letters and requeue only after fixing the cause.

## Evidence

Before claiming success, collect:

- The `maturana up --dry-run` plan output showing consistent session ids.
- The `maturana doctor` result for hostd, sessiond, and each agent.
- A delivered Telegram reply (inbound accepted, outbound delivered) round trip.
- The audit entries for channel inbound/outbound and any restarts.
- Queue stats showing no growing `processing`/`failed` backlog.

## Recovery

- Session id mismatch: reinstall the guest worker with the session id from the
  plan, or relaunch the channel with the matching `--session-id`.
- Critical sessiond exit: read its bind error, free the port, and restart.
- Channel poll errors: check the Telegram token source and pairing state.
- Dead-lettered message: fix the failing turn, then `requeue_inbound`.
- Backoff restart storm: stop the plane, fix the failing component, restart.

## Boundaries

- Do not start more than one runtime plane per agent at a time.
- Do not hand-edit session SQLite while the plane is running.
- Do not paper over a session id mismatch by enqueuing to both queues.
- Do not run the channel and guest worker against different session ids.
- Do not bypass `maturana doctor` verification before declaring it healthy.
