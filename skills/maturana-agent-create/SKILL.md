# maturana-agent-create

Use this skill when a user wants to create a Maturana agent. Maturana is a
**personal agent framework**, so this is a guided, interactive **setup wizard** —
not a silent spec dump. You interview the user, author their agent's identity and
soul, choose runtime, wire the channels and tools they want, then launch it and
take it live. Do not invent a name or assume defaults the user hasn't given —
**ask, confirm, then build.**

End state: a named, running, reachable personal agent with `IDENTITY.md`,
`SOUL.md`, a validated `MATURANA.md`, paired channels, and enabled tools.

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

## Decision Path — the setup interview (ask one topic at a time, confirm each)

**Talk like a person, not a config file.** Ask each question in plain language and
translate the answers into `MATURANA.md` fields yourself. NEVER make the user
confirm raw field names or values (`on_launch`, `retain`, `egress_allowlist`,
`harness_auth`, `token_source`, kebab `id`, etc.) — those are your job. Use
sensible defaults silently and only ask when a choice genuinely matters to them.

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
   - **Telegram** (primary, paired): store the bot token in pipelock
     (`maturana pipelock set telegram/bot-token --value <token>`), set
     `channels.telegram.token_source: pipelock:telegram/bot-token`. Pairing
     happens at go-live (step below).
   - **Discord** (outbound notifications): store the webhook
     (`maturana pipelock set discord/webhook --value <url>`); deliver via
     `maturana notify discord --webhook-source pipelock:discord/webhook`.
   Leave channels the user didn't choose disabled.
6. **Tools / capabilities.** Ask which to enable, and record them in the spec's
   installed skills/tools. Offer the common ones: `maturana-browse`,
   `maturana-web-search`, `maturana-github`, `maturana-notion`,
   `maturana-image-gen`, `maturana-voice`, `maturana-graph` / `maturana-wiki`,
   `maturana-schedule`. Only enable what they ask for (least privilege), and add
   the matching `network.egress_allowlist` hosts each tool needs.
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
- `MATURANA.md` from the closest example, filled with the interview answers:
  identity (id/name/purpose), runtime, vm, harness_auth, filesystem mounts
  (bounded), network egress allowlist, credentials (references only), memory,
  browser, channels, snapshots.

Then validate:

```
maturana spec validate .maturana/agents/<id>/MATURANA.md
```

Fix spec fields until validation is clean — never weaken validation.

### Launch and go live

1. **Security review** — run the `maturana-security-review` skill over the spec.
2. **Launch** — use the `maturana-agent-launch` skill (`maturana agent launch
   --spec .maturana/agents/<id>/MATURANA.md`). This materializes the agent
   (scaffolding `IDENTITY.md`/`SOUL.md` only if absent — your authored ones are
   preserved) and boots the VM.
3. **Personal scaffolding** — `maturana personal init <id> --spec
   .maturana/agents/<id>/MATURANA.md` (memory, context, schedules, wiki). It
   preserves your authored `IDENTITY.md`/`SOUL.md`/`MEMORY.md`.
4. **Pair channels:**
   ```
   maturana channel pair telegram start --agent-id <id>
   # ask the user to send the printed /pair CODE to the bot
   maturana channel pair telegram complete --agent-id <id>
   maturana channel serve telegram --agent-id <id>
   ```
   For Discord, send a test `maturana notify discord` and confirm it arrives.
5. **Confirm it's live** — send a real message through the paired channel and
   confirm the agent replies; check `maturana heartbeat status <id>`.

(On Windows you can use `.\scripts\maturana.ps1 …` if `maturana` isn't yet on
PATH in the current shell.)

## Evidence

Before claiming success, collect:
- Paths to the authored `IDENTITY.md`, `SOUL.md`, and `MATURANA.md`.
- Clean `maturana spec validate` output.
- Security-review result.
- Launch evidence (VM running, guest worker active).
- Channel pairing status true + a real round-trip message answered.
- A summary: name, id, runtime, channels (paired), tools enabled, how the user
  talks to it.
- Evidence that no raw secrets were written into any spec/identity/soul/memory.

## Recovery

- Vague answers: ask for the missing identity/soul/runtime/channel detail before
  drafting broad permissions.
- Harness not authenticated: stop and guide auth (`codex login` / `claude`).
- Validation fails: fix spec fields, not validation.
- Pairing fails: inspect pair status, channel heartbeat, transcript, runner logs
  (see `maturana-personal-agent`).
- Missing token: store it in pipelock and reference it; never paste it into the
  spec.

## Boundaries

- Do not skip the interview or invent a name/identity/soul the user didn't give.
- Do not launch from an unvalidated spec, or without harness auth present.
- Do not store OpenAI/Claude OAuth in pipelock; inject it directly into the VM.
- Do not paste raw secrets into `MATURANA.md`, `IDENTITY.md`, `SOUL.md`, memory,
  wiki, or logs.
- Do not grant filesystem/egress beyond what the chosen tools need.
- Do not overwrite an existing `IDENTITY.md`/`SOUL.md`/`MEMORY.md` without
  explicit user intent.
