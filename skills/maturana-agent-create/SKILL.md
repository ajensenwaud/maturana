# maturana-agent-create

Use this skill when a user wants to create a Maturana agent. Maturana is a
**personal agent framework**, so this is a guided, interactive **setup wizard** —
not a silent spec dump. You interview the user, author their agent's identity and
soul, choose runtime, wire the channels and tools they want, then launch it and
take it live. Do not invent a name or assume defaults the user hasn't given —
**ask, confirm, then build.**

End state: a named, running, reachable personal agent with `IDENTITY.md`,
`SOUL.md`, a validated `MATURANA.md`, paired channels, and enabled tools.

## Flow & tone — run it like an onboarding, not a form

This is the user's first impression of their agent, so make it feel crafted —
a friendly, confident setup wizard, not an interrogation or a config dump.

- **Open warmly and set the frame.** One or two lines: greet, say what you're
  about to do, and preview the few things you'll ask. e.g. *"Let's bring your
  agent to life. I'll ask a handful of quick things — its name, who you are, how
  you'll reach it, and what it can do — then build and launch it (a few minutes)."*
- **One idea per turn, kept short.** Ask in small, plain-language steps; always
  offer a sensible default the user can accept with a single word ("Codex —
  sound good?").
- **Narrate progress so it feels like motion.** A short marker between phases:
  *"✓ Identity set. Now — how do you want to reach it?"* → *"✓ Telegram wired."*
- **Reflect the agent back** once identity + soul are captured: a one-line "here's
  who I'm building" so the user feels seen before the slow steps begin.
- **Set expectations before anything slow.** Launch + guest provisioning takes a
  few minutes — say so, then report when it's up. Never go silent mid-step.
- **Land it.** Finish on a concrete, satisfying go-live moment, not a log dump:
  *"🎉 <name> is live. Message it on Telegram, or try `<one example>`. Here's what
  it can do for you: …"* — name how to reach it and one thing to try right now.
- **Resume gracefully.** If you're picking up a half-built agent, say so warmly
  ("Looks like we started <name> earlier — let's finish it") and fill only the
  gaps; don't re-interview.

Keep the security posture invisible-but-present: never ask for raw secrets in
chat, and store tokens via pipelock as you collect them.

## Grounding

1. Read `AGENTS.md` first.
2. Read the closest example under `examples/` for the target host
   (`MATURANA.codex-hyperv.md` on Windows, `MATURANA.codex-firecracker.md` on
   Linux) as the spec shape.
3. If an agent id is given, read any existing
   `.maturana/agents/<id>/{MATURANA.md,IDENTITY.md,SOUL.md}` and preserve them
   unless the user wants a reset.
4. Detect the host: Windows → Hyper-V, Linux → Firecracker.

## Preflight — harness auth (agents can't run without it)

Confirm the chosen harness is authenticated on the host before launching:
- codex → `~/.codex/auth.json` exists (else have the user run `codex login`).
- claude-code → `~/.claude/.credentials.json` exists (else have them authenticate
  `claude`).
If missing, stop and guide the user through auth first — a launched agent with no
harness auth cannot answer.

## Decision Path — the setup interview (a few quick questions, then build)

**Talk like a person, not a config file.** Ask each question in plain language and
translate the answers into `MATURANA.md` fields yourself. NEVER make the user
confirm raw field names or values (`on_launch`, `retain`, `egress_allowlist`,
`harness_auth`, `token_source`, kebab `id`, etc.) — those are your job. Use
sensible defaults silently and only ask when a choice genuinely matters to them.

**Keep it short and front-loaded.** Gather the few real decisions up front, then
build the whole agent in one go without stopping to re-confirm. Concretely:
- **Batch the quick ones.** Name, owner/timezone, purpose, runtime, and which
  channels can be asked together (or in two short messages) — don't drag a
  one-line answer into five round-trips.
- **Never re-ask what you already know.** If the user already said the name,
  timezone, harness, etc. earlier in the conversation, use it.
- **Resume, don't restart.** If `.maturana/agents/<id>/` already exists, read it
  and fill only the gaps (missing channel token, unvalidated spec, not launched
  yet) instead of re-interviewing from scratch.
- **Set expectations before slow steps.** The first launch downloads/boots a VM
  and provisions the guest — say "this takes a few minutes, I'll report when it's
  up" so a multi-minute step doesn't look stuck.

1. **Name & id.** Ask what they want to call the agent. Derive a kebab-case
   `id` (e.g. "Ada" → `ada`); confirm it.
2. **Identity → write `IDENTITY.md`.** Interview, then author
   `.maturana/agents/<id>/IDENTITY.md`:
   - Who the agent is — its role and what it does.
   - **Who the owner is** — the user's name, how to address them, timezone /
     working hours, and what they rely on the agent for. (This is a *personal*
     agent; it must know its person.)
   - Scope and boundaries — in-scope vs needs-approval.
3. **Soul → write `SOUL.md`.** Interview, then author
   `.maturana/agents/<id>/SOUL.md`: voice/tone, values, do's and don'ts,
   personality. Keep the security posture line (never request credentials
   directly; use declared sources).
4. **Runtime.** Ask the harness — `codex` (default), `claude-code`, or
   `opencode`. Provider is the host-native one (Hyper-V / Firecracker); set
   `harness_auth` source/guest paths for the subscription harness.
5. **Channels (how the user talks to it).** Ask which to set up:
   - **Console TUI** (always available, no setup): set `channels.tui: true`. The
     user talks to the agent from a terminal with `maturana agent chat <id>` — a
     full-screen chat (history, slash commands, multiline). Great for the first
     conversation and for verifying the agent live.
   - **Telegram** (primary, paired): store the bot token in pipelock
     (`maturana pipelock set telegram/bot-token --value <token>`), set
     `channels.telegram.token_source: pipelock:telegram/bot-token`. Pairing
     happens at go-live (step below).
   - **Discord** (full two-way bot, like Telegram). Assume the user has **no bot
     yet** — walk them through creating one, one step at a time, and wait for
     each result:
     1. Open <https://discord.com/developers/applications> → **New Application**
        (name it after the agent).
     2. **Bot** tab → **Reset Token** → copy the token (shown once). This is the
        secret you'll store.
     3. Still on the Bot tab, under **Privileged Gateway Intents**, turn on
        **MESSAGE CONTENT INTENT** and Save. Without it the agent receives empty
        messages and can't reply to anything. (Enable SERVER MEMBERS only if a
        tool needs it.)
     4. Invite the bot: **OAuth2 → URL Generator**, tick scope **`bot`**, then
        permissions **Send Messages** + **Read Message History** (add Attach
        Files / Add Reactions if wanted). Open the generated URL and add the bot
        to a server, or DM it directly.
     5. Store the token and wire the spec:
        ```
        maturana pipelock set discord/bot-token --value <token>
        ```
        set `channels.discord.bot_token_source: pipelock:discord/bot-token`.
     The agent reads and replies over the Discord gateway once `maturana up`
     runs — there is **no pairing step**. (A one-off outbound ping uses
     `maturana notify discord --webhook-source ...`, which is separate from this
     channel and not needed here.)
   Leave channels the user didn't choose disabled.
6. **Tools / capabilities.** Ask which to enable, and record them in the spec's
   installed skills/tools. Offer the common ones: `maturana-browse`,
   `maturana-web-search`, `maturana-github`, `maturana-notion`,
   `maturana-image-gen`, `maturana-voice`, `maturana-graph` / `maturana-wiki`,
   `maturana-schedule`. Only enable what they ask for (least privilege), and add
   the matching `network.egress_allowlist` hosts each tool needs.

   **Collect each tool's credential now — don't defer it.** A tool that needs an
   API key is dead weight until the key is in pipelock, so for every selected
   tool that requires one, ask for it in the same breath and store it via
   `maturana pipelock set <key> --value <token>`, then reference it in the spec
   (never paste the raw value in). Read the tool's own SKILL.md for the exact key
   and wiring. Common ones:
   - **Notion** → ask for the integration token (`ntn_…`):
     `maturana pipelock set notion/integration-token --value <ntn_…>`, and declare
     the MCP server in the spec: `mcp_servers: [{ name: notion, command: npx,
     args: ["-y","@notionhq/notion-mcp-server"], env: [{ name: NOTION_TOKEN,
     source: "pipelock:notion/integration-token" }], egress_hosts: ["api.notion.com"] }]`.
   - **Web search** → Brave or Tavily key (`pipelock:search/brave-key` or
     `search/tavily-key`).
   - **GitHub** → a PAT (`pipelock:github/token`).
   - **Image-gen / Voice** → the provider key (e.g. OpenAI, ElevenLabs) under the
     key the tool's skill documents.
   If the user doesn't have a key yet, point them at where to create it (e.g.
   Notion: Settings → Connections → develop/integrations), and only enable the
   tool once the key is stored. Read it with `read -s`-style secrecy — never echo
   tokens back or write them anywhere but pipelock.
7. **Memory, wiki, schedules.** Enable memory + wiki paths. Offer schedules
   (e.g. a morning brief) via the `maturana-schedule` skill.
8. **Backups (snapshots).** In plain terms: offer automatic backups so you can
   roll the agent back if something breaks — e.g. "Want me to keep automatic
   backups so we can undo if anything goes wrong? I'll keep the last few." Default
   to on, keeping the most recent five. Map this to `snapshots.on_launch` /
   `snapshots.retain` yourself; don't show those names.

## Actions

Write all three into `.maturana/agents/<id>/`:
- `IDENTITY.md` and `SOUL.md` from the interview (rich, not stubs).
- `MATURANA.md`: **copy the closest example file verbatim first**, then edit only
  the values you need (identity id/name/purpose, runtime, vm, harness_auth,
  filesystem mounts, egress allowlist, credentials as references, channels,
  snapshots). For example, on Windows:
  ```
  cp examples/MATURANA.codex-hyperv.md .maturana/agents/<id>/MATURANA.md
  ```
  **Do NOT invent or rename keys.** The spec is parsed with
  `deny_unknown_fields`: any unknown/misspelled key (or a section copied from the
  wrong provider) makes `spec validate` fail hard. Start from a known-valid
  example and change values, not structure. Only include `channels`,
  `mcp_servers`, etc. when the user asked for them; omit (don't stub) the rest.

Then validate:

```
maturana spec validate .maturana/agents/<id>/MATURANA.md
```

If validation fails, read the error, correct that exact field against the
example, and re-run — never weaken validation, and don't guess at new fields.

### Launch and go live

1. **Security review** — run the `maturana-security-review` skill over the spec.
2. **Launch** — use the `maturana-agent-launch` skill (`maturana agent launch
   .maturana/agents/<id>/MATURANA.md --apply` — the spec is a POSITIONAL
   argument; there is no `--spec` flag on `agent launch`). This materializes the
   agent (scaffolding `IDENTITY.md`/`SOUL.md` only if absent — your authored ones
   are preserved), boots the VM, AND provisions the guest (proxy CA, harness +
   auth, browser, `maturana-agent.service`). **Do NOT SSH in and provision by
   hand or write a PowerShell/bash wrapper** — `--apply` owns that. If it fails,
   report the exact error and stop.
3. **Personal scaffolding** — `maturana personal init <id> --spec
   .maturana/agents/<id>/MATURANA.md` (memory, context, schedules, wiki). It
   preserves your authored `IDENTITY.md`/`SOUL.md`/`MEMORY.md`.
4. **Pair Telegram:**
   ```
   maturana channel pair telegram start --agent-id <id>
   # ask the user to send the printed /pair CODE to the bot
   maturana channel pair telegram complete --agent-id <id>
   ```
   **Discord needs no pairing** — once the bot token is in pipelock and the spec
   has `channels.discord`, the gateway runner connects when `maturana up` starts.
   Verify it live below (message the bot) rather than with a one-off notify.
5. **Bring the agent online — supervised, not by hand.** Start the plane so the
   channel + proactivity + schedule runners are supervised together:
   ```
   maturana up --agent-id <id>            # foreground supervisor (one agent)
   # or, for an always-on agent that survives reboot:
   maturana service install up
   ```
   **Do NOT launch `channel serve` as a background/hidden process yourself** —
   `maturana up` (and the `up` service) own the long-running runners.
6. **Confirm it's live — self-verify the round-trip, don't make the user relay
   a message.** Pairing only proves the host-side channel plumbing (token valid,
   chat-id stored, runner polling). It does NOT prove the agent actually answers
   — that depends on the VM being up, the guest worker consuming the queue, and
   the harness being authenticated in the guest. Verify that yourself:
   ```
   maturana channel status telegram --agent-id <id>   # paired: true, presence active
   maturana agent run <id> --prompt "Confirm you are live in one sentence." --wait
   ```
   `agent run --wait` enqueues straight into sessiond and blocks for the guest's
   response, so it exercises the full sessiond -> guest worker -> harness ->
   outbound path with no human in the loop. If it returns an answer, the agent is
   live. If it hangs, THAT is the real failure (VM down, worker not consuming, or
   harness auth missing in the guest) — diagnose it (see Recovery), don't ask the
   user to message the bot.

   Only after `agent run --wait` succeeds, optionally ask the user to send one
   real Telegram message — and ONLY if you specifically want to confirm outbound
   *Telegram delivery* (the outbox thread pushing to the chat), which `agent run`
   doesn't cover. Don't make this a routine relay-and-say-"sent" step.

(On Windows you can use `.\scripts\maturana.ps1 …` if `maturana` isn't yet on
PATH in the current shell.)

## Evidence

Before claiming success, collect:
- Paths to the authored `IDENTITY.md`, `SOUL.md`, and `MATURANA.md`.
- Clean `maturana spec validate` output.
- Security-review result.
- Launch evidence (VM running, guest worker active).
- Channel pairing status true, plus a successful `agent run <id> --wait`
  round-trip (the agent answered). A manual Telegram message is optional and only
  to confirm outbound delivery — not required as proof of liveness.
- A summary: name, id, runtime, channels (paired), tools enabled, how the user
  talks to it.
- Evidence that no raw secrets were written into any spec/identity/soul/memory.

## Recovery

- Vague answers: ask for the missing identity/soul/runtime/channel detail before
  drafting broad permissions.
- Harness not authenticated: stop and guide auth (`codex login` / `claude`).
- Validation fails: fix spec fields, not validation.
- Pairing fails (pair code rejected / never stored): inspect pair status, channel
  heartbeat, transcript, runner logs (see `maturana-personal-agent`).
- **Paired but no reply** (the common one — `paired: true`, runner `polling`, yet
  the bot is silent): the channel is fine; the round-trip is broken. Check the VM
  is running (`maturana agent inspect <id> --live`), the guest worker is active,
  and the harness is authenticated in the guest. Confirm with
  `maturana agent run <id> --prompt "ping" --wait` — if that also hangs, the
  failure is guest-side, not the channel.
- Missing token: store it in pipelock and reference it; never paste it into the
  spec.

## Boundaries

- Do not skip the interview or invent a name/identity/soul the user didn't give.
- Do not launch from an unvalidated spec, or without harness auth present.
- Do not provision the guest, or run channel/proactive/schedule runners, by hand
  (no SSH provisioning, no backgrounded `channel serve`): `agent launch --apply`
  provisions; `maturana up` / the `up` service supervise the runners.
- Do not store OpenAI/Claude OAuth in pipelock; inject it directly into the VM.
- Do not paste raw secrets into `MATURANA.md`, `IDENTITY.md`, `SOUL.md`, memory,
  wiki, or logs.
- Do not grant filesystem/egress beyond what the chosen tools need.
- Do not overwrite an existing `IDENTITY.md`/`SOUL.md`/`MEMORY.md` without
  explicit user intent.
