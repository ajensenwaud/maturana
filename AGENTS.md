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
- Agents communicate with users through a consoloe TUI, Telegram, and Discord.
- Credentials (except OAuth tokens for OpenAI and Claude Code) are handled through a Pipelock-style egress governance and credentials handling module.  
- Maturana can stop, snaphot and rewind an agent to a desired snapshot in case of a fault or compromise. This is done using skills that invoke system calls through Rust runtime.
- Maturana supports guest VM agent harnesses (Claude Code, Codex) that use OAuth (subscription) for authentication. OAuth credentials need to be injected into to the VMs from the hos at runtime.

## Architecture

### Codex skills framework
Maturana ships a set of Codex skill. The skill pack is the main product
interface. It gives Codex procedures, templates, validations, and tool
bindings for agent engineering.

Initial skills:

- `maturana-agent-create`: turn a user goal into a `MATURANA.md` spec.
- `maturana-agent-validate`: check capabilities, runtime, mounts,
  egress, secrets, channels, and schedules with `maturana spec validate`.
- `maturana-agent-launch`: materialize a spec as a Firecracker or Hyper-V agent.
- `maturana-agent-inspect`: read materialized spec readiness, health,
  logs, audit entries, and runtime status.
- `maturana-agent-update`: modify an agent spec and apply the change.
- `maturana-skill-create`: create a new Codex skill for an agent or the
  Maturana framework.
- `maturana-tool-create`: create a host or guest tool.
- `maturana-skill-deploy`: install skills and tools into a target
  agent.
- `maturana-security-review`: review a spec, skill, or tool before it
  is launched.
- `maturana-snapshot`: take, list, and rewind agent snapshots.

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
2. Treat Codex skills as the primary user-facing surface.
3. Keep the core small and move extension behavior into skills or tools
4. Keep it simple - composable Unix-like tool over complex monoliths
5. Keep it simple, stupid. Prefer direct, boring, skill-invoked workflows
   over clever automation. Use known-good VM images in their native format
   before building conversion or provisioning machinery.
