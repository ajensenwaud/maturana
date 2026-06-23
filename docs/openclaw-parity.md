# OpenClaw — functional comparison

OpenClaw (latest **2026.6.9**) is a self-hosted personal-agent platform: ~120k stars,
50+ integrations, a plugin marketplace (**ClawHub**, ~15k community skills), 23+
messaging channels, durable workflow orchestration (**TaskFlow**), and mobile/desktop
companion apps. This is an honest comparison of what it does vs Maturana — channels
**and** everything else — so we can agree what's worth closing. Maturana's invariant:
hardware-VM isolation, no secrets in the guest, governed **outbound-only** egress.

## A. Channels

**Maturana (6):** Telegram, Discord, Slack, AgentMail (email), Web cockpit, TUI.

**OpenClaw (23+):** the above + WhatsApp, Signal, iMessage, Microsoft Teams, Google
Chat, Matrix, IRC, Mattermost, Feishu, LINE, Nostr, Nextcloud Talk, Synology Chat,
Tlon, Twitch, Zalo, WeChat, QQ.

**Gap:** ~17 channels — consumer (WhatsApp/Signal/iMessage/WeChat/LINE), enterprise
(Teams/Google Chat/Mattermost), open (Matrix/Nostr/IRC). Most are "plugin channels".
Note: rich Telegram delivery (HTML/markdown/progress drafts) Maturana already mirrors.

## B. Functional comparison (the part that matters more)

| Capability | OpenClaw | Maturana | Verdict |
|---|---|---|---|
| **Isolation** | Docker / SSH / OpenShell sandboxes | Firecracker / Hyper-V **VMs** (hardware) | Maturana **stronger** |
| **Secrets / egress** | tokens in config + allowlists | pipelock (encrypted, never in guest) + MITM egress proxy + audit | Maturana **stronger** |
| **Orchestration** | **TaskFlow** — durable flow state; sub-agent spawns, scheduled tasks, multi-step workflows survive a restart, inspectable / recoverable / cancellable | orchestrator loop (ephemeral; dies with the process) | **GAP — durability/recovery** |
| **Multi-agent routing** | route inbound channels / accounts / peers → isolated agents (per-agent workspaces + sessions) | each agent has its own channel/bot; no sender→agent router | **GAP** |
| **Plugin / skill model** | SKILL.md dirs **+ ClawHub marketplace**: browse/install ~15k skills; plugins add channels, providers, tools, voice | SKILL.md dirs (in-repo) installed as Codex skills; MCP servers | **GAP — registry + remote install** |
| **Memory** | MEMORY.md + provenance-rich + **hybrid keyword+vector** (Qdrant, opt-in) | MaturanaGraph (keyword GraphRAG) + LLM-wiki + per-agent memory | **PARTIAL — no provenance surfaced, no vector** |
| **Scheduled jobs / heartbeat** | cron skill + HEARTBEAT.md background scheduler | `maturana-schedule` + heartbeat + proactive loop | **Have** |
| **Voice** | STT/TTS + wake-word + talk-mode (device apps) | STT/TTS skill (server-side) | **Partial** (device wake-word = out of scope) |
| **Browser / computer use** | Playwright browser, Canvas (visual A2UI workspace), mobile nodes (camera) | headless Chrome in-VM | **Partial** (Canvas/nodes niche) |
| **Provider routing** | model failover + auth-profile rotation across many providers | per-channel model override; Claude OAuth refresh | **Partial** |
| **Web dashboard** | Control UI (+ roadmap: plugin marketplace, memory viewer, scheduling calendar) | web cockpit (agents/runtime/sessions/graph/pipelock/tools/skills) | **Partial** |
| **Pairing / permissions** | DM pairing codes, mention rules, group restrictions | Telegram pairing codes | **Partial** |
| **Companion apps** | Windows Hub, macOS menu bar, iOS/Android nodes | — | **Out of scope** (server platform) |
| **Deployment** | Node daemon, Docker, Win/mac/Linux/iOS/Android | Rust binary, Firecracker/Hyper-V, systemd/Windows boot | different by design |

## C. Where Maturana is already ahead

Hardware isolation per agent, encrypted host-side secrets with a governed egress
proxy + audit, snapshot/rollback, and (on the Hermes branch) a budget-capped
multi-agent loop + Kanban + verify-it-runs. OpenClaw trades isolation for breadth;
Maturana's bet is the opposite.

## D. The real gaps, ranked (candidates to close)

1. **Durable / recoverable orchestration (TaskFlow-equivalent).** Background runs,
   schedules, and multi-step flows that survive a restart and are inspectable /
   resumable / cancellable. Highest value, security-neutral. The persistent board
   (Hermes branch) is the foundation; this adds durable state + resume + a flow view.
2. **Skill / plugin registry + gated remote install (ClawHub-equivalent).** Browse +
   install skills from a registry — but every install runs through
   `maturana-security-review` and stays sandboxed (the charter's whole point:
   extensibility *without* importing a supply-chain attack surface).
3. **Multi-agent routing.** A front router mapping an inbound sender / channel / peer
   to the right isolated agent — the personal-assistant-fleet ergonomic.
4. **Real named channels.** Concrete first-class connectors, Matrix first (open,
   self-hostable, outbound-only, zero-trust-clean), then enterprise (Teams / Google
   Chat / Mattermost). The honest "more channels" answer — not a generic webhook.
5. **Memory provenance + opt-in hybrid (vector) search.** Tag memories with their
   source and surface it on recall; add embeddings/vector retrieval behind a governed
   egress when wanted.
6. **Provider routing / model failover.** Multi-provider routing with failover + auth
   rotation.

## E. Declined (zero-trust / scope)

- Weaker sandboxes (Docker/SSH/OpenShell) — VMs are the stronger substrate.
- Inbound internet listeners on the agent host — channels stay outbound-only.
- Mobile/desktop companion apps + voice wake-word — device territory, not a server
  platform's job.
- Canvas / visual A2UI — niche; the web cockpit covers the dashboard need.
