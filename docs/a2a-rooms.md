# A2A Rooms: Multi-Agent Group Conversations

Rooms give Maturana agents a shared conversation so they can self-organise on
a problem ("develop and market a new website") while you watch and steer from
a Discord channel or Telegram group chat.

## Architecture

```
Telegram group ◄──┐                       ┌──► agent planner  (session inbound/outbound)
                  ├──► Room store (host) ─┼──► agent builder
Discord channel ◄─┘    .maturana/rooms/   └──► agent marketer
                       <room-id>/room.sqlite
```

The **room store** — not the chat platform — is the A2A transport. This is
deliberate: Telegram bots cannot see other bots' messages, so a group chat
full of per-agent bots can never carry agent-to-agent traffic. Instead:

- The room is an ordered, SQLite-backed message log on the host
  (`.maturana/rooms/<room-id>/room.sqlite`), in the same style as the
  per-agent session queues.
- The **room runner** (`maturana room serve <room-id>`) fans new room
  messages into each member's existing session inbound as a single digest
  prompt, and collects replies from each member's session outbound. Guest VMs
  need **no changes and no new egress**: the digest arrives through the same
  sessiond claim loop as a Telegram prompt would.
- One bot per platform **mirrors** the room into the group chat
  (`sender: message`) and ingests human messages from the chat into the room.
  Humans appear to agents as `user:<name>` senders, and agents are told to
  treat them as authoritative.

## The agent protocol

Each digest an agent receives is a full prompt containing the room goal, the
member roster (with roles), the recent transcript, the new messages, and the
rules:

- Reply with exactly the one message to post to the room.
- Address members with `@agent-id`, everyone with `@all`.
- Reply exactly `PASS` to abstain (consumed silently, never posted).
- Claim work explicitly before doing it; report results back to the room.

Self-organisation is conversational — agents divide work by talking, exactly
like a human team chat.

## Loop prevention

Unmoderated agent groups feedback-loop. Rooms prevent this structurally:

| Mechanism | Effect |
| --- | --- |
| **Hop budget** | Every message carries a relay depth. User messages are hop 0; an agent reply is `digest hop + 1`. Agent messages at/past `hop_limit` (default 8) are mirrored to the chat but never fanned to other agents, so pure agent↔agent cascades always terminate. A fresh human message resets the budget. |
| **PASS** | Agents with nothing to add produce no traffic. |
| **One digest in flight** | A member with an unfinished room turn gets no new digest; a busy room batches into one digest instead of swamping the queue. |
| **Cooldown** | Optional `agent_cooldown_seconds` floor between an agent's turns. |
| **mention-only mode** | Optionally, members only see messages that `@`-mention them (or unaddressed user broadcasts). |

## Setup

### 1. Create the room

```bash
maturana room init launch \
  --goal "Develop and market a new website for artisanal coffee" \
  --member "planner:room-main:project lead" \
  --member "builder:room-main:full-stack engineer" \
  --member "marketer:room-main:growth marketing" \
  --telegram-chat-id -1001234567890 \
  --discord-channel-id 1234567890123456789
```

Member spec is `agent-id[:session-id[:role]]`. Each member must be a
materialized agent whose guest worker claims from the given session id
(default `room-main`). Config lands in `.maturana/rooms/launch/room.json` and
can be edited by hand.

### 2. Telegram bridge (one bot for the whole room)

1. Create a bot with @BotFather; **disable privacy mode** (`/setprivacy` →
   Disable) so it sees all group messages.
2. `maturana pipelock set telegram/bot-token --value "<token>"`
3. Add the bot to your group. Get the chat id (e.g. forward a group message
   to @userinfobot, group ids are negative) and pass it as
   `--telegram-chat-id`.

### 3. Discord bridge

1. Create an application + bot at https://discord.com/developers, enable the
   **Message Content** intent, and invite it to your server with *View
   Channel*, *Send Messages*, and *Read Message History* permissions.
2. `maturana pipelock set discord/bot-token --value "<token>"`
3. Right-click your channel → *Copy Channel ID* (enable developer mode) and
   pass it as `--discord-channel-id`.

The bridges poll over REST (`getUpdates` / `GET channels/{id}/messages`), so
no webhook endpoint or gateway websocket is needed. Both run host-side; guest
egress allowlists are untouched.

### 4. Run it

```bash
maturana room serve launch          # standalone, or:
maturana up                         # auto-discovers rooms under .maturana/rooms/
```

`maturana up` supervises each room runner next to sessiond and the per-agent
channels (skip with `--no-rooms`). Then post the kickoff from the group chat,
or from the host:

```bash
maturana room post launch --text "@all kick off: propose a plan" --from user:anders
maturana room status launch         # cursors, in-flight digests, bridges
maturana room transcript launch     # full conversation with hop counts
```

Every post is also appended to `.maturana/rooms/<room-id>/transcript.md` and
audited to `.maturana/audit/room-<room-id>.jsonl`.

## Operational notes

- **Session ids must match.** A member's `session_id` is the queue its guest
  worker claims from. If the worker claims `telegram-main` but the room
  writes `room-main`, the agent never sees the room. One agent can be in a
  room *and* keep a private Telegram session by using separate session ids.
- **Echo smoke test** (no VMs needed): replace a member's harness with the
  local echo provider — `maturana session run-once <agent-id> --session-id
  room-main --provider echo` — between two `room serve --once` ticks.
- **Bridges skip history.** On first contact the runner records the current
  platform offset and only ingests messages sent after the room started.
- **Hop budget tuning.** `hop_limit 8` allows a propose → assign → execute →
  report → review cycle with slack. Raise it for longer autonomous runs;
  lower it (or use `mention-only` + cooldowns) for cheaper, tighter rooms.
