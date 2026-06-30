# Maturana

> Secure multi-platform agentic orchestration platform built on Codex

## Motivation
There is a ton of agent harnesses and agentic orchestration platform out there from industry leasers like OpenClaw and Hermes to commercial engineering focused multi-agent platforms like Devin. However, most of these platforms are built for features, not security. Platforms like OpenClaw and Hermes have almost unlimited flexbiltiy given their architecture, but it comes at the cost of an increased attack surface. Their software is now so complex that they need vetted shells from vendors such as NVIDIA and Microsoft to reduce the security exposure. 
Platform such as NanoClaw have attempted to find a whitespace by securing and isolating agents in Docker. However, I find NanoClaw less appealing given how tied to the Anthropic ecosystem it is. 
I have really enjoyed engineering using Codex. Therefore I thought it would be useful to build a highly secure, lightweight agent harness built on the Codex ecoystem, using Firecracker VMs for hardware-level agent isolation.

## Proposition
Maturan combines the elegance of Unix, the agentic engineering workflow of Codex, and the hardware isolation of Hyper-V (Windows) and Firecacker (Linux). The result is a lean, secure, composable muli-agent harness. It is: 

- Codex native -- Codex skills and tools is the main interface for defining and orchestrating agnets
- Skills-first -- every action is available as a skill in Codex
- Runtime-agnosic -- Maturana supports multiple runtimes in the VMs. OpenCode, Codex, and Claude Code are available by default
- Zero-trust -- Maturana employs zero-trust design principles throughout. Each agent has a bounded filesystem access, network access, centralised and encrypted secrets management, etc.
- Lean -- Maturana's runtime is built in Rust with a tiny modular core and skills built on top

## Getting started
1. Clone the repo from Github
2. Run `./install.sh` which fetches the necessary components, runtim, and hypervisor components and installs them
3. Open Codex in the folder
4. Codex initialises the Maturna framework (skills and tools) for engineering agents
5. Codex writes or edits a a `MATURANA.md` agent spec.
6. Maturana materializes the spec as a Firecracker (Linux) or Hyper-V- (Windows) -backed agent.
7. Codex configures the harnesses inside the VM and installs or builds any requested skills and tools
8. Codex launches, inspects, updates, snapshots, and governs the agent
9. Codex provides skills to build and deploy new tools inside the agent VMs

Codex is the product surface for defining, configuring, building, and running the agents. It is an agentic-first agent orchestration platform.

## Principles
- Codex is the base platform for Maturana
- Maturana is the framework that teaches Codex how to build and operate
  secure agents by exposing skills
- Maturana is built on zero-trust and paranoia by defalt
- Everything Maturana exposes to Codex is a skill or tool
- Skills may call Rust or Unix / Windows tools when they need side effects.
- `MATURANA.md` is the agent contract: identity, scope, harness,
  capabilities, memory, tools, skills, schedules, and channels.
- The Rust core is a small runtime
- Every agent runs in its own Hyper-VM or Firecracker VM.
- The VM agentic runtime is agnostic: Codex, Claude Code, and OpenCode are
  first-class guest harnesses, but it should be extensible to support other harnesses later on (e.g. Grok Build)

## Requirements
- Maturana provides a Codex skills framework for defining, building,
  launching, and governing highly secure VM-based agents.
- Every feature Maturana provides through Codex is represented as a
  skill combined with reusable tools
- `MATURANA.md` files ar specs for fast agent creation and repeatable
  deployment.
- First-class VM-based runtime support for Codex, Claude Code, and
  OpenCode.
- On Linux, Maturana used Firecracker. On Windows it uses native Hyper-V
- Firecracker microVM isolation for every agent.
- Every agent has its own mounted filesystem for storing MATRANA.md, AGENTS.md, and SOUL.md (behaviour)
- Maturana supports headless Chrome inside the microVM for browsing.
- Maturan provides a shared context graph based on Karpathy's LLM wiki pattern (see https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f). Every agent exposes a skill for updating the wiki. It loaded into agent memory on-demand.
- All agents are always-on agents with heartbeat, contex,t scheduling, and notifications.
- Agents communicate with users through a console TUI (`maturana agent chat <id>`), Telegram, and Discord.
- Credentials (except OAuth tokens for OpenAI and Claude Code) are handled through a Pipelock-style egress governance and credentials handling module.  
- Maturana can stop, snaphot and rewind an agent to a desired snapshot in case of a fault or compromise. This is done using skills that invoke system calls through Rust runtime.
- Maturana supports guest VM agent harnesses (Claude Code, Codex) that use OAuth (subscription) for authentication. OAuth credentials need to be injected into to the VMs from the hos at runtime.

## Architecture

### Codex skills framework — how to load and use skills

Maturana's capabilities are exposed to Codex as **skills**: one folder per skill
at `skills/<name>/SKILL.md`. Codex does **not** auto-load them — *you* load a
skill by **reading its `skills/<name>/SKILL.md` when the task matches it**, then
following that procedure exactly.

**Loading rule (do this every time):** before any Maturana action — creating,
launching, inspecting, or governing agents; handling secrets; wiring channels;
taking snapshots; building skills/tools — scan the index below, open the matching
`skills/<name>/SKILL.md`, and follow it. Skills are the contract for every
action; prefer them over improvising. When unsure where to start, read
`skills/maturana-cli-actions/SKILL.md` (general host operations) and
`skills/maturana-agent-create/SKILL.md` (the usual first task).

The installer also installs each skill as a **native Codex skill** (under
`~/.agents/skills/<name>/SKILL.md` with `name`/`description` frontmatter), so
Codex surfaces them via the `/skills` menu, a `$name` mention (e.g.
`$maturana-agent-create`), or implicit selection. Regenerate after adding or
editing skills with `maturana skill codex-prompts` (alias `codex`).

**Available skills** (read `skills/<name>/SKILL.md` to load the procedure):

Agent lifecycle
- `maturana-agent-create` — guided personal-agent **setup wizard**: name →
  IDENTITY.md → SOUL.md → runtime → channels (+pairing) → tools → launch → live
- `maturana-agent-validate` — validate a `MATURANA.md` spec before launch
- `maturana-agent-launch` — materialize/launch an agent (Firecracker or Hyper-V)
- `maturana-agent-inspect` — inspect a live agent: health, logs, audit, status
- `maturana-agent-update` — modify an existing agent contract and apply it
- `maturana-snapshot` — take, list, or restore agent snapshots
- `maturana-spawn` — spawn a sub-agent from a channel command or the host

Host runtime & operations
- `maturana-cli-actions` — operate Maturana from Codex on the host (start here)
- `maturana-orchestrate` — bring an agent's host runtime plane online / diagnose it
- `maturana-orchestrator-loop` — run a goal across multiple worker agents in a bounded loop (spawns/reuses VMs)
- `maturana-a2a` — operate the Agent2Agent (A2A) layer agents use to call each other
- `maturana-hostd` — check/install/diagnose the privileged host daemon (Windows)
- `maturana-schedule` — add/list/test/debug/run agent schedules
- `maturana-personal-agent` — turn a VM-backed agent into a personal assistant

Security & secrets
- `maturana-pipelock` — store/list/read/inject/audit secrets + egress governance
- `maturana-security-review` — review a spec/skill/tool/provider change before launch

Knowledge & memory
- `maturana-graph` — read/write MaturanaGraph (shared LLM-wiki knowledge graph)
- `maturana-wiki` — add shared markdown context to the LLM-wiki store

Agent capabilities & integrations
- `maturana-browse` — read/screenshot/interact with web pages (headless Chrome)
- `maturana-web-search` — current info from the public web (Brave/Tavily)
- `maturana-image-gen` — generate an image from a text prompt
- `maturana-voice` — transcribe audio to text / synthesize text to speech
- `maturana-github` — work with GitHub repositories from an agent
- `maturana-notion` — read/write Notion (search, pages)
- `maturana-slack` — integrate an agent with Slack
- `maturana-agentmail` — give an agent an email address (AgentMail)
- `maturana-self-improve` — run the self-improvement flywheel over trajectories

Building & extending Maturana
- `maturana-develop` — develop a new skill, guest tool, or MCP bundle
- `maturana-plugin` — discover, validate, and design first-party or third-party plugins
- `maturana-skill-create` — create a new Codex skill (framework or agent)
- `maturana-tool-create` — create a host or guest tool
- `maturana-wasm-tool` — build a new executable capability on the fly (WASM)
- `maturana-self-forge` — let an agent build + run its own WASM capabilities on
  the fly (self-mutation), gated by the `self_forge` capability
- `maturana-skill-deploy` — install a tested skill/tool into a target agent
- `maturana-deploy` — deploy a Codex-developed skill/tool/MCP server

(If this index drifts from `skills/`, the directory is the source of truth —
list it with `ls skills/` and read the relevant `SKILL.md`.)

### Agent specs
`MATURANA.md` is the durable unit of agent definition. Codex is expected
to write it, but humans can read and edit it.

A spec defines:

- identity and purpose
- guest runtime: `codex`, `claude-code`, or `opencode`
- workspace mount policy
- filesystem permissions
- egress allowlist
- credential requirements
- memory and context paths
- browser policy
- installed skills
- installed tools
- schedules
- notification channels
- snapshot policy

Maturana validates the spec before creating or updating the agent.

### Runtime plane
The host runs a small set of long-lived Rust processes:

- a router for agent control, channels, and Codex-invoked operations
- a Firecracker/HyperV VM manager for per-agent lifecycle and snapshots
- an egress proxy for allowlists, credential injection, and audit

Each worker agent runs inside its own VM. Inside the
VM, the selected harness runs the agent turn: Codex, Claude Code,
or OpenCode. The harness is a runtime detail of the worker. Codex
remains the orchestration layer outside the worker plane.

The host filesystem is never casually exposed to a guest. Agent state
is explicit: workspace, memory, wiki, schedules, tools, audit records,
and snapshots live under per-agent directories with governed mounts.

On Linux, the Firecracker guest OS of course Linux. On Windows, the Hyper-V guest OS is Linux or Windows, depending on what is fatest. 

## What Maturana is not
- Not a standalone chat UI competing with Codex.
- Not a generic agent framework with many equal control planes.
- Not Docker-based container orchestration platform
- Not multi-tenant SaaS.
- Not macOS (yet)
- Not Claude-first, even though Claude Code is a first-class guest
  runtime.


## Working in this repo
1. Read this file first.
2. Skills are the primary surface. For any Maturana task, find the matching skill
   in the index above and **read `skills/<name>/SKILL.md` before acting** — that
   file is the procedure. Codex does not auto-load skills; you load them by
   reading them. `ls skills/` is the source of truth.
3. Keep the core small and move extension behavior into skills or tools
4. Keep it simple - composable Unix-like tool over complex monoliths
5. Keep it simple, stupid. Prefer direct, boring, skill-invoked workflows
   over clever automation. Use known-good VM images in their native format
   before building conversion or provisioning machinery.
