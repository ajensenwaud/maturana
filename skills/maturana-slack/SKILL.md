# maturana-slack

Use this skill when integrating a Maturana agent with Slack.

Slack is a channel adapter, not core runtime behavior. Follow the NanoClaw-style
pattern: validate credentials, receive Slack events in an adapter, enqueue
normal session messages, and deliver outbox replies with Slack APIs.

## Grounding

1. Read `AGENTS.md` first.
2. Confirm the target agent contract allows Slack.
3. Inspect existing channel state and session paths for the agent.
4. Confirm required Slack credentials are present in pipelock:
   - `slack/bot-token`
   - `slack/signing-secret`
5. Inspect the local NanoClaw reference if present:
   - `.maturana/reference/nanoclaw/setup/add-slack.sh`
   - `.maturana/reference/nanoclaw/setup/channels/slack.ts`
   - `.maturana/reference/nanoclaw/src/channels/chat-sdk-bridge.ts`

## Preflight

- Confirm Slack should be interactive, not just outbound notification.
- Confirm `slack/bot-token` and `slack/signing-secret` exist in pipelock
  without printing either value.
- Confirm signature validation is designed before accepting inbound events.
- Confirm inbound messages enqueue through sessiond and replies drain from the
  normal outbox.
- Confirm local adapter tests cover URL verification and message events before
  deployment.

## Decision Path

- Outbound-only notification: use an existing notification path if sufficient.
- Interactive Slack channel: build/deploy a Slack adapter outside core.
- Credential validation: use Slack `auth.test` before serving events.
- Direct messages: open DMs with `conversations.open`.
- Inbound events: accept `message.im` and `app_mention`, validate signing
  secret, then enqueue through `maturana session enqueue`.
- Replies: deliver session outbox messages with `chat.postMessage`.

## Actions

Store credentials:

```powershell
maturana pipelock set slack/bot-token --value <xoxb-token>
maturana pipelock set slack/signing-secret --value <secret>
```

Adapter behavior:

```text
Slack event -> validate signature -> maturana session enqueue
session outbox -> chat.postMessage
```

Required Slack app capabilities:

- Bot token scopes for DM/channel posting and relevant event reads.
- Event subscriptions for `message.im` and `app_mention`.
- Public HTTPS request URL routed to the Maturana Slack adapter.

## Evidence

Before claiming success, collect:

- `pipelock list` shows Slack credential names.
- Slack `auth.test` succeeds.
- A test DM or app mention creates an inbound session message.
- Session outbox delivery produces a Slack message timestamp.
- Adapter logs/audit do not print raw tokens or signing secret.

## Recovery

- `auth.test` fails: verify token name/scope and rotate if exposed.
- Signature validation fails: verify signing secret and request timestamp.
- Events not arriving: inspect Slack event subscription URL and public routing.
- Inbound exists but no reply: debug session runner/outbox, not Slack first.
- Outbox exists but Slack did not receive it: inspect `chat.postMessage`
  response and channel/DM id.

## Boundaries

- Do not put Slack adapter logic in Maturana core.
- Do not store Slack secrets outside pipelock.
- Do not accept unvalidated Slack events.
- Do not create a separate Slack command queue; use sessiond.
- Do not print raw Slack tokens or signing secrets.
