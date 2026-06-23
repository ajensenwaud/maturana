# Maturana — personal-agent platform capabilities

Maturana is a secure, VM-isolated personal-agent platform. To see where it stands in
the field, this benchmarks it against a broad self-hosted agent platform (OpenClaw,
2026.6.9 — ~120k stars, a plugin marketplace, 23+ channels, durable orchestration,
companion apps) as a reference point. It is a map of our own capabilities and what's
worth building next — not a spec to copy; the design choices are ours, and several of
theirs we deliberately reject. Maturana's invariant holds throughout: hardware-VM
isolation, no secrets in the guest, governed **outbound-only** egress.

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
| **Orchestration** | **TaskFlow** — durable flow state; sub-agent spawns, scheduled tasks, multi-step workflows survive a restart, inspectable / recoverable / cancellable | orchestrator loop with **durable runs**: `orchestrator list` / `resume` / `abort` — a run survives a host restart and resumes from its saved plan (kept done steps, re-run unfinished) | **Closed** (this branch) |
| **Multi-agent routing** | route inbound channels / accounts / peers → isolated agents (per-agent workspaces + sessions) | `maturana route` — a host-side routing table (channel/sender/content → agent, most-specific wins, default fallback); resolver live-proven. Live channel hand-off (cross-agent reply delivery) = follow-on | **Closed (engine)** (this branch) |
| **Plugin / skill model** | SKILL.md dirs **+ ClawHub marketplace**: browse/install ~15k skills; plugins add channels, providers, tools, voice | SKILL.md dirs (in-repo) installed as Codex skills; MCP servers | **GAP — registry + remote install** |
| **Memory** | MEMORY.md + provenance-rich + **hybrid keyword+vector** (Qdrant, opt-in) | MaturanaGraph (keyword GraphRAG) + LLM-wiki + per-agent memory; remembered facts now **provenance-stamped** `(via <channel>)` + date in MEMORY.md and the graph note | **Provenance closed**; pluggable vector store = follow-on (needs embedding egress + store client) |
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

## D. Capabilities to build, ranked

1. **Durable / recoverable orchestration (TaskFlow-equivalent). — CLOSED on this
   branch.** `orchestrator list` shows every run + state (complete / incomplete /
   aborted / failed); `orchestrator resume <run_id>` reloads the saved plan, keeps
   completed steps, re-queues unfinished ones, and drives it to completion (skipping
   the planning turn); `abort` cancels. Live-proven: a run was killed mid-flight at
   1/4 steps and `resume` finished it to 4/4. Security-neutral.
2. **Skill / plugin registry + gated remote install (ClawHub-equivalent).** Browse +
   install skills from a registry — but every install runs through
   `maturana-security-review` and stays sandboxed (the charter's whole point:
   extensibility *without* importing a supply-chain attack surface).
3. **Multi-agent routing. — CLOSED (engine) on this branch.** `maturana route`
   add/list/remove/default/test/clear: a host-side table maps inbound (channel /
   sender / content) → agent, most-specific rule wins, default fallback. Resolver
   live-proven (telegram+sender→one agent, content "code"→another, else default). A
   route only picks which agent's front door an inbound uses — agents stay isolated.
   Wiring it into a live shared channel (cross-agent reply delivery) is the follow-on.
4. **Real named channels.** Concrete first-class connectors, Matrix first (open,
   self-hostable, outbound-only, zero-trust-clean), then enterprise (Teams / Google
   Chat / Mattermost). The honest "more channels" answer — not a generic webhook.
   *(Not built this round — agreed to skip in favour of 1/3/5.)*
5. **Memory provenance — CLOSED; + opt-in hybrid (vector) search — follow-on.**
   Provenance: a remembered fact is stamped `(via <channel>)` + date in MEMORY.md and
   the graph note, so recall carries where it came from. MaturanaGraph stays the
   DEFAULT memory; a pluggable external vector store (Qdrant/LanceDB) is a real
   subsystem — it needs an embedding egress + a store client + hybrid retrieval — so
   it's scoped as a focused follow-on, deliberately not stubbed.
6. **Provider routing / model failover — N/A by architecture.** The model call happens
   in the harness inside the VM (Codex→OpenAI, Claude→Anthropic, opencode→OpenRouter),
   not in Maturana. Provider breadth/failover already lives in opencode+OpenRouter;
   host-level routing exists at agent/role granularity (orchestrator routes role→agent
   →provider). A host LLM proxy for in-call failover would route model traffic through
   the host, weakening isolation for little gain — declined.

## E. Declined (zero-trust / scope)

- Weaker sandboxes (Docker/SSH/OpenShell) — VMs are the stronger substrate.
- Inbound internet listeners on the agent host — channels stay outbound-only.
- Mobile/desktop companion apps + voice wake-word — device territory, not a server
  platform's job.
- Canvas / visual A2UI — niche; the web cockpit covers the dashboard need.
