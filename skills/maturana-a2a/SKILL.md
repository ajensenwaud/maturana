# maturana-a2a

Use this skill to understand and operate Maturana's Agent2Agent (A2A) layer —
the open-standard wire format every agent-to-agent call uses: the master
orchestrator talking to its workers, and one agent delegating to a peer in-band.

A2A here is JSON-RPC 2.0 over HTTP with an Agent Card for discovery. A host A2A
server (`maturana a2a serve`, supervised by `maturana up` on `0.0.0.0:47837`)
exposes, per agent: a card at `/a2a/<agent>/.well-known/agent-card.json` and a
`POST /a2a/<agent>` endpoint accepting `message/send`. A `message/send` delivers
the message to that agent as a turn, waits for the reply, and returns a completed
A2A Task whose first artifact carries the answer.

## Grounding

1. Read `AGENTS.md` first.
2. The wire types live in `crates/maturana-core/src/a2a.rs` (Message/Part, Task/
   TaskStatus/TaskState, Artifact, AgentCard, JSON-RPC envelopes). The server +
   client + shared dispatch core live in `crates/maturana-cli/src/a2a.rs`.
3. The server reuses sessiond's HTTP helpers and is thread-per-connection, so a
   blocking `message/send` never freezes it.
4. The same core (`a2a_dispatch`) backs both the orchestrator (in-process, over a
   loopback A2A server it starts per run) and in-band peers (over the wire).

## How it's used

- **Master orchestrator → workers:** `maturana orchestrator loop` sends every step
  (coordinator, workers, reviewer, synthesizer) as an A2A `message/send`.
- **In-band (agent → peer):** an agent POSTs `message/send` to the host A2A server
  at its sessiond host on port 47837 (see the "Delegating to another agent (A2A)"
  section the guest gets in its `AGENTS.md`).

## Limits (enforced host-side, not by the agent)

- Delegation nesting depth is capped (an agent passing `maturana_depth` in the
  message metadata; the host refuses past the max).
- An agent cannot delegate to itself (its single-flight worker would deadlock).
- The token (sessiond token) is required on every call; the server binds a public
  interface that guests reach over their TAP.

## Actions

Send a message to an agent over A2A (from the host, loopback):

```bash
TOKEN=$(cat .maturana/sessiond/token)
curl -s -X POST http://127.0.0.1:47837/a2a/claude-firecracker \
  -H "x-maturana-session-token: $TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"role":"user","parts":[{"kind":"text","text":"In one sentence, what is a monorepo?"}],"messageId":"m1","kind":"message"}}}'
```

Fetch an agent's card:

```bash
curl -s http://127.0.0.1:47837/a2a/codex-firecracker/.well-known/agent-card.json \
  -H "x-maturana-session-token: $TOKEN"
```

## Evidence

Before claiming the A2A layer is healthy, collect:

- `maturana status` shows the `a2a` process running.
- An Agent Card fetch returns JSON with `protocolVersion`, `name`, `url`, `skills`.
- A `message/send` returns a JSON-RPC `result` that is an A2A Task
  (`kind: "task"`, `status.state: "completed"`, an artifact with the answer).
- The depth cap and self-dispatch refusal return a `failed` Task with a reason.

## Boundaries

- Do not bypass the A2A server for agent-to-agent calls — it is where the depth,
  self-dispatch, and token checks live.
- Do not raise the nesting-depth cap to get past a refusal; deep delegation chains
  are the runaway risk the cap exists to stop.
- Do not expose the A2A port without the token (it can enqueue work to any agent).
