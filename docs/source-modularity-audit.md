# Maturana Source Modularity Audit

Status: baseline audit and implementation plan. Initial Phase 1/2 work is now
in progress on `codex/modular-plugin-refactor`; this document remains the
source of the refactor rationale.

Date: 2026-06-29
Updated: 2026-07-01

## Executive Summary

At the baseline audit, Maturana was not a single-file monolith, but it was still
a single-control-plane monolith in practice. The biggest pressure was
concentrated in the CLI crate:

- `crates/maturana-cli/src/channels.rs`: about 8.9k LOC
- `crates/maturana-cli/src/main.rs`: about 8.1k LOC at baseline; about 4.4k LOC
  after the initial ops extraction; about 317 LOC after the command-module split
- `crates/maturana-cli/src/orchestrate.rs`: about 2.9k LOC
- `crates/maturana-core`: a broad shared crate containing specs, validation,
  providers, session storage, proxying, snapshots, tools, hooks, search,
  orchestration helpers, and guest worker renderers

The existing architecture documents are directionally right: Rust owns product
decisions, scripts are leaf adapters, and skills are the product workflow
surface. The next modularity step should not be "add many small abstractions."
It should be to introduce a small application/service boundary inside Rust so
the CLI, web cockpit, and future tools call shared operations instead of each
owning or spawning each other's workflows.

## Implementation Checkpoint

Initial work on `codex/modular-plugin-refactor` implements the first slice of
this plan:

- Added `maturana-ops` as the shared application/operations crate.
- Added `maturana-plugin` for plugin manifests, discovery, validation, and
  first-party/third-party catalog contracts.
- Added persisted plugin enablement under `<home>/plugins/config.json`.
- Connected enabled plugin-declared skills to `maturana skill codex-prompts`,
  with conflict checks to prevent third-party skill shadowing.
- Added feature-gated plugin asset resolution for skills, tools, and command
  declarations plus `maturana plugin assets` and `/api/plugins/assets`
  inspection.
- Added validated local plugin installation into `<home>/plugins` through
  `maturana-ops` and `maturana plugin install`, without executing plugin code.
- Moved CLI `plugin`, `status/list`, `skill`, `spec`, `improve`, `search`,
  `tool`, and `vm` commands into `crates/maturana-cli/src/commands/`.
- Started splitting `channels.rs` by moving Telegram API DTOs into
  `crates/maturana-cli/src/channels/telegram.rs`, OpenRouter model catalog
  handling into `crates/maturana-cli/src/channels/models.rs`, and persisted
  channel settings into `crates/maturana-cli/src/channels/settings.rs`.
- Expanded the channel model split so Codex/Claude/OpenRouter model selection,
  harness detection, and model-picker auth gating live in
  `crates/maturana-cli/src/channels/models.rs`.
- Continued splitting `channels.rs` by moving the AgentMail HTTP poll adapter,
  Slack Socket Mode adapter, and shared channel-state helpers into
  `crates/maturana-cli/src/channels/agentmail.rs`,
  `crates/maturana-cli/src/channels/slack.rs`, and
  `crates/maturana-cli/src/channels/state.rs`.
- Moved the Discord Gateway/REST adapter, Discord delivery-channel persistence,
  file upload/download handling, and Discord outbound sink into
  `crates/maturana-cli/src/channels/discord.rs`.
- Moved Telegram voice-note handling, host-side STT/TTS provider calls, and
  Telegram audio upload helpers into `crates/maturana-cli/src/channels/voice.rs`.
- Moved Telegram document/photo ingestion, OCR fallback, Telegram file download,
  filename sanitization, and knowledge-graph media helpers into
  `crates/maturana-cli/src/channels/media.rs`.
- Moved first-run onboarding markers, prompt construction, completion sentinel
  handling, and correctly routed onboarding enqueue into
  `crates/maturana-cli/src/channels/onboarding.rs`.
- Moved `/loop` channel command formatting into
  `crates/maturana-cli/src/channels/loops.rs` and moved detached loop start,
  run-id validation, abort markers, and loop status/list summaries into
  `maturana-ops::orchestration`.
- Moved Telegram Bot API transport helpers (`getMe`, `getUpdates`, message
  send/edit/delete, document upload, keyboard callbacks, chat actions, and
  live-edit classification) into
  `crates/maturana-cli/src/channels/telegram_api.rs`.
- Moved Telegram live progress rendering, active-stream markers, exactly-once
  reply finalization, streaming delivery, and animated tool runs into
  `crates/maturana-cli/src/channels/telegram_live.rs`.
- Moved the shared slash-command catalog, help text, picker button construction,
  and channel setting selection persistence into
  `crates/maturana-cli/src/channels/command_catalog.rs`.
- Moved shared slash-command execution, status/tool/subagent summaries, graph
  command text, stop/compact/session handling, and inline truncation into
  `crates/maturana-cli/src/channels/command_handler.rs`.
- Moved console transcript persistence, console slash-command dispatch result
  types, and the web console-command adapter into
  `crates/maturana-cli/src/channels/console_bridge.rs`.
- Moved the shared outbound delivery loop, Telegram delivery sink/backstop, and
  closure-based text-channel delivery adapter into
  `crates/maturana-cli/src/channels/delivery.rs`.
- Moved Telegram pairing-state keys, poll offset persistence, heartbeat writes,
  paired-chat lookup, and bridge liveness checks into
  `crates/maturana-cli/src/channels/telegram_state.rs`.
- Moved Telegram inbound update classification, pair-command normalization, and
  `/tool`/`/spawn` routing parsers into
  `crates/maturana-cli/src/channels/telegram_routing.rs`.
- Moved channel subagent task framing, persisted subagent records, and
  channel-safe slug generation into
  `crates/maturana-cli/src/channels/subagents.rs`.
- Moved Telegram pair start/complete/status workflows and pair-code generation
  into `crates/maturana-cli/src/channels/telegram_pairing.rs`.
- Moved Telegram inbound action execution, callback handling, and per-message
  prompt streaming/delivery into
  `crates/maturana-cli/src/channels/telegram_inbound.rs`.
- Moved the CLI `snapshot`, `audit`, and `notify` command families into
  `crates/maturana-cli/src/commands/`, leaving root `main.rs` to delegate those
  command branches instead of owning their workflows inline.
- Moved the CLI `pipelock` command family and `doctor` health report command
  into `crates/maturana-cli/src/commands/`, keeping secrets/proxy policy and
  health-report formatting out of root dispatch while preserving the existing
  command surface.
- Moved the CLI `web` cockpit command glue, bind selection, and injected web
  chat/graph adapters into `crates/maturana-cli/src/commands/web.rs`, leaving
  root `main.rs` to route the command instead of owning the cockpit setup.
- Moved the host-side `claude-refresh` command family into
  `crates/maturana-cli/src/commands/claude_refresh.rs`, keeping OAuth probe,
  refresh-daemon, idle-agent detection, and re-render/re-push wiring out of root
  dispatch.
- Moved the `up` runtime-plane command, process-plan redaction, heartbeat
  writer, child-process supervision, and per-agent runtime config derivation
  into `crates/maturana-cli/src/commands/up.rs`.
- Moved the hidden `hostd` command, elevated Windows host daemon HTTP server,
  Hyper-V PowerShell request routing, token issuance, and hostd-specific tests
  into `crates/maturana-cli/src/commands/hostd.rs`.
- Moved the `setup`/`repair` command schema, host setup handoff, Windows
  harness setup handoff, guest-worker refresh handoff, Firecracker harness
  setup handoff, and repair flag parser tests into
  `crates/maturana-cli/src/commands/repair.rs`.
- Moved the `agent` command schema and launch/inspect/stop/chat/run/logs/fetch/
  push dispatch, live guest inspection script, CLI chat enqueue/wait path,
  governed guest transfer helpers, channel image handoff, and agent-specific
  tests into `crates/maturana-cli/src/commands/agent.rs`, leaving root wrappers
  only where older modules still call `crate::...` helpers.
- Moved channel conversation/front-door wrappers, dispatch queue helpers,
  bounded dispatch prompt framing, and channel context test adapters into
  `crates/maturana-cli/src/channels/conversation.rs`.
- Moved the large channel test module into
  `crates/maturana-cli/src/channels/tests.rs`, leaving `channels.rs` as the
  production command/router surface plus submodule wiring.
- Moved the orchestrator loop and durable-board test modules into
  `crates/maturana-cli/src/orchestrate/tests.rs` and
  `crates/maturana-cli/src/orchestrate/board_tests.rs`, shrinking the runtime
  `orchestrate.rs` surface while keeping existing test coverage.
- Moved the root CLI test module into `crates/maturana-cli/src/tests.rs`, so
  `main.rs` now stays focused on command wiring and narrow compatibility
  delegates.
- Moved orchestration run directory validation, plan persistence, and abort
  status checks into `maturana-ops::orchestration`; the CLI loop and board
  paths now use the shared validated run-state boundary.
- Moved orchestration run list/detail/tally views into
  `maturana-ops::orchestration`; the web cockpit and CLI status command now
  read persisted run state through the shared operations boundary.
- Moved reusable artifact path normalization, remote output naming, recursive
  copy/count helpers, provider-aware guest SSH key selection, and run output
  directory selection into `maturana-ops::artifacts`; orchestrator loop, board
  delivery, and agent image handoff now use that shared artifact boundary.
- Moved guest transfer policy, live transfer IP resolution, bounded guest fetch,
  and best-effort step artifact collection into `maturana-ops::artifacts`; the
  agent command and orchestrator now share path-safety/SCP behavior, and the CLI
  root no longer forwards transfer helpers.
- Moved the orchestration planner model, plan validation, coordinator prompt
  framing, balanced JSON extraction, step task framing, and review verdict
  parsing, and chat-friendly plan summary rendering into
  `maturana-ops::planner`; the CLI loop and board paths now share the same plan
  contract.
- Moved synthesizer deliverable manifest parsing and prose/file materialization
  into `maturana-ops::deliverables`; the CLI orchestrator now delegates output
  writing to the shared ops layer.
- Moved orchestration verification verdicts, verifier task framing, verifier
  marker parsing, failure-detail extraction, and artifact summary rendering into
  `maturana-ops::verification`; the CLI orchestrator now keeps only live
  dispatch and collection around that shared verification contract.
- Moved reusable-agent discovery and role placement resolution into
  `maturana-ops::placement`; loop runs, board runs, and board LLM helpers now
  use the same tested rules for roles files, dedicated worker specs, explicit
  reusable agents, and auto-discovered standing agents.
- Moved orchestration worker-pool resolution, role-worker caching, spawned VM
  elapsed-time accounting, and spawned worker teardown into
  `maturana-ops::placement`; CLI loop and board execution now share the same
  worker lifecycle boundary.
- Moved board/card assignee resolution into `WorkerPool` so role-backed
  assignees, concrete agent ids, and default developer fallback are handled by
  the shared placement boundary rather than a CLI-local helper.
- Moved generic chat outbox posting into `maturana-ops::conversation` with a
  shared `OutboxTarget` and text/file helpers; the CLI orchestrator now keeps
  channel routing only and delegates session-db outbox writes to the shared ops
  layer.
- Moved reusable agent list/status/session snapshot and HTTP health probes into
  `maturana-ops`.
- Moved reusable hostd client status, token lookup, VM list, and live-agent IP
  lookup into `maturana-ops::hostd`; CLI agent transfer paths and the doctor
  report now share that hostd client boundary instead of duplicating it.
- Updated the web cockpit to consume shared ops for agent snapshots,
  live inspect/stop, plugin catalog, and runtime health probes.
- Replaced the web runtime doctor subprocess bridge with the shared
  `maturana-ops` doctor report.
- Moved deploy skill/tool logic into `maturana-ops`, including SSH host-key
  pinning and audit writes, and updated the web deploy endpoint to call it
  directly.
- Moved orchestrator abort marker writes into `maturana-ops` and updated the web
  abort endpoint to avoid spawning the CLI.
- Moved detached board run/decompose/specify launch decisions into
  `maturana-ops`, leaving the web cockpit as a request/response front end over
  the shared board operation boundary.
- Moved durable-board card output directory naming and role-aware card task
  framing plus goal-judge prompt/reply parsing into `maturana-ops::boards`; the
  CLI board runner now delegates the prompt contracts for card execution and
  goal-mode review to the shared ops layer.
- Moved durable-board decompose/specify prompt construction, specification JSON
  parsing, and board mutation rules into `maturana-ops::boards`; the CLI now
  keeps only A2A worker selection, persistence, and event logging for those
  board LLM actions.
- Moved the Firecracker restart endpoint behind a narrow
  `maturana-ops` lifecycle bridge; `maturana-web/src/api` no longer calls
  `current_exe()`.
- Moved Firecracker launch profile resolution, built-in fleet profiles, and
  materialized-agent selection into `maturana-ops::firecracker`, reducing CLI
  ownership of setup/repair decisions.
- Moved host runtime-plane token creation and Linux sessiond/graph service
  starters into `maturana-ops::runtime_plane`.
- Moved Firecracker TAP/NAT setup command construction into
  `maturana-ops::firecracker` and wired both repair and orchestration worker
  spawn through the shared operation.
- Moved Firecracker asset preparation, guest artifact rendering, manifest
  validation, and baked SSH host-key pinning into `maturana-ops::firecracker`.
- Added a shared `maturana-ops::ssh` adapter for pinned guest SSH, bounded SSH
  commands, bounded SCP, and shell quoting.
- Moved guest-worker SSH refresh/install, Claude auth re-seed guarding, MCP
  config installation, and resident MCP package preinstall into
  `maturana-ops::guest_worker`.
- Moved the high-level Firecracker harness repair sequence into
  `maturana-ops::firecracker`; the CLI now constructs a repair request instead
  of owning the per-agent decision loop.
- Moved spawned Firecracker worker clone provisioning and teardown into
  `maturana-ops::firecracker`; the orchestrator loop now calls the shared ops
  boundary for live VM spawn/cleanup.
- Moved agent SSH key repair and Ubuntu cloud image download/checksum/VHDX
  conversion into `maturana-ops::host_setup`.
- Moved Windows harness setup/repair orchestration into
  `maturana-ops::windows_harness`; the CLI now preserves the same flags and
  final doctor output while delegating the host lifecycle work.
- Moved shared graph HTTP helpers and conversation enqueue/context/memory
  mechanics into `maturana-ops::graph` and `maturana-ops::conversation`; the CLI
  channel module now keeps adapter-specific slash-command and delivery behavior
  while delegating the common chat front door.
- Expanded the `maturana-builtins` first-party plugin from a feature placeholder
  into a command-family catalog, and tightened plugin validation so command
  entrypoints must be descriptors under `commands/` or paths inside declared
  plugin tools.
- Added effective plugin permission reporting in the shared plugin status model,
  so enabled valid plugins expose the filesystem, egress, and secret grants they
  request while disabled or invalid plugins contribute no effective grants.
- Wired top-level CLI dispatch through the `maturana-builtins` first-party
  plugin catalog. Built-in command families now obey the same feature
  enablement state as plugin assets, while `maturana plugin` remains a modular
  core escape hatch so disabled features can be re-enabled without manual config
  edits.

Still pending:

- Continue opportunistically moving shared channel behavior into
  `maturana-ops`; the former catch-all `channels.rs` is now a thin command/router
  surface over platform modules, but channel-independent policy should keep
  migrating out of CLI modules as it becomes reusable.
- Continue moving any remaining CLI-only operation glue behind reusable ops
  boundaries; the major setup/repair, deploy, web, board, orchestration, runtime,
  guest-worker, Firecracker, host setup, hostd, graph, and conversation paths now
  use `maturana-ops`.
- Defer broader core crate splits until the ops boundary and plugin contracts
  have settled further.

## What I Reviewed

I scanned:

- Workspace manifests: `Cargo.toml`, crate-level `Cargo.toml` files
- Rust crates: `maturana-cli`, `maturana-core`, `maturana-web`,
  `maturana-graph`, `maturana-ingest`
- High-pressure files: CLI `main.rs`, `channels.rs`, `orchestrate.rs`; core
  `materialize.rs`, `worker.rs`, provider modules, snapshots, session DB, proxy
- Web API and server modules, especially places that shell back into the CLI
- `docs/script-boundary.md`, `docs/skill-workflows.md`,
  `docs/orchestration.md`, `docs/mvp-plan.md`
- Script and skill inventory

I deliberately did not open secret-bearing files under `.maturana/pipelock`.

## Current Shape

The workspace started with five Rust crates:

- `maturana-core`: shared runtime/domain crate
- `maturana-cli`: the `maturana` binary and much host orchestration logic
- `maturana-web`: web cockpit server, REST API, WebSocket protocol, static UI
- `maturana-graph`: graph store/RAG primitives
- `maturana-ingest`: document ingestion

This branch adds two more boundary crates:

- `maturana-ops`: reusable application/operation workflows shared by CLI and
  web
- `maturana-plugin`: plugin manifest, discovery, validation, and asset catalog
  contracts

The scripts are already relatively contained. `docs/script-boundary.md` says:

```text
Codex skill -> maturana CLI/hostd -> Rust decision -> leaf script/host primitive
```

That boundary is mostly respected now. The remaining monolith is therefore not
primarily bash/PowerShell. It is the Rust CLI becoming the place where many
product workflows accumulate.

## Main Coupling Points

### 1. CLI `main.rs` mixes public command schema, dispatch, domain workflows, and host adapters

`main.rs` defines the top-level command tree, but also contains substantial
workflow implementations:

- setup/repair command wiring
- process supervision helpers
- service and doctor probes
- SSH/SCP utilities and guest transfer policy
- web bind selection
- agent run/chat enqueue/wait paths
- hostd server handling
- platform process calls (`ssh`, `scp`, `sudo`, `bash`, `virt-copy-in`,
  `qemu-img`, `powershell`, `schtasks`, `tailscale`, etc.)

This makes `main.rs` a command parser, an application service layer, and a host
adapter layer at the same time.

### 2. `channels.rs` is several modules in one file

`channels.rs` currently combines:

- channel command structs and dispatch
- Telegram pairing, polling, callbacks, keyboards, streaming edits, and delivery
- Discord, Slack, and AgentMail serve/poll/send logic
- context assembly from identity, soul, contract, memory, transcript, wiki, graph
- slash command parsing and handling
- model selection UI logic
- onboarding state
- speech-to-text and text-to-speech helpers
- media/document handling, OCR, guest image delivery
- session queue insertion and outbox delivery

The file has good local tests, which is a strength. But the module boundary no
longer matches the domain boundary.

### 3. `maturana-web` sometimes calls the CLI as a subprocess

At baseline, several cockpit actions used `std::env::current_exe()` and spawned
the current binary to run CLI commands, for example:

- restart agent through `repair firecracker-harnesses` (now behind a narrow ops
  lifecycle bridge)
- deploy skill through `deploy skill` (now moved to ops)
- run boards/orchestrator flows (board launch and orchestrator abort now moved
  to ops)
- runtime/doctor commands (now moved to ops)

This bridge made the CLI more than a front end: it became the application API.
The web API subprocess calls have now been removed. Remaining work is mainly to
keep moving host operation glue behind reusable ops boundaries and out of CLI
front-end code.

### 4. The web crate depends on CLI-owned closures for channel and graph behavior

`maturana-web` accepts injected closures for web chat enqueue and graph ingest
because the needed logic lives in `maturana-cli::channels` / `maturana-cli::graph`.
This avoids a direct cycle, but it reveals that the shared "conversation front
door" belongs in a reusable Rust module, not in the CLI.

### 5. `maturana-core` is broad, not layered

`maturana-core` is reasonably modular by file, but the crate itself exposes many
unrelated concerns:

- spec types and validation
- provider interfaces and provider implementations
- guest worker shell renderers
- session queue storage
- snapshots
- pipelock vault and proxy
- tool registry and WASM runtime
- search
- roles, boards, orchestrator budget/spawn
- hooks, improvement, A2A

This is acceptable early, but it makes dependency hygiene difficult. For
example, enabling the default CLI pulls in the web crate, graph/ingest, TUI,
WebSocket client code, and the core WASM runtime stack.

### 6. Provider and provisioning responsibilities are split unevenly

Core providers own `plan_launch`, `launch`, `stop`, and `inspect`, while CLI
repair/setup code still owns some important provisioning details:

- per-agent proxy/session/graph process starts

Some of this belongs in host-operation services rather than in providers, but it
should still be reusable outside the CLI command file.

## Recommendations

### 1. Add a small `maturana-ops` application crate

Create a new crate that sits between front ends and primitives:

```text
maturana-cli  ┐
maturana-web  ├── maturana-ops ── maturana-core / graph / ingest
future tools  ┘
```

Suggested scope for `maturana-ops`:

- agent lifecycle operations: validate, materialize, launch, stop, inspect
- setup/repair operations: SSH key, Ubuntu image, Firecracker harnesses, Windows
  harnesses
- channel front door: enqueue a turn with the same transcript/context/model
  behavior for CLI, TUI, Telegram, Discord, and web
- deploy operations: deploy skill/tool to a live agent
- runtime/doctor/status operations
- board/orchestrator start/status wrappers

`maturana-cli` should mostly parse arguments and print results. `maturana-web`
should call `maturana-ops` functions instead of spawning `current_exe()` for
ordinary internal actions.

Keep this crate boring. It should not become a framework. It should be a home
for current CLI workflows that are already product operations.

### 2. Split `channels.rs` by platform and shared conversation services

Keep the external CLI behavior unchanged, but split the file into modules:

```text
crates/maturana-cli/src/channels/
  mod.rs
  command.rs
  conversation.rs
  context.rs
  slash.rs
  onboarding.rs
  telegram.rs
  telegram_stream.rs
  discord.rs
  slack.rs
  agentmail.rs
  media.rs
  voice.rs
  settings.rs
```

Then move the channel-independent pieces into `maturana-ops` over time:

- `enqueue_turn`
- slash command dispatch/apply
- stable chat keys
- transcript/context manifest writing
- onboarding finalization
- wiki/graph context loading policy

This gives the web cockpit and future channel adapters one shared conversation
front door.

### 3. Split `main.rs` into command modules before changing behavior

Do a mechanical split first, preserving command names and behavior:

```text
crates/maturana-cli/src/commands/
  mod.rs
  agent.rs
  spec.rs
  snapshot.rs
  vm.rs
  pipelock.rs
  notify.rs
  skill.rs
  setup.rs
  doctor.rs
  status.rs
  web.rs
  tool.rs
  improve.rs
  hostd.rs
```

The first pass should only move code. After that, pull reusable workflows from
those modules into `maturana-ops`.

This lowers review risk: one PR can be a file-layout refactor with tests, then
later PRs can move behavior behind cleaner APIs.

### 4. Introduce host adapter traits for process/SSH/filesystem side effects

Right now process calls are spread across the CLI and providers. Introduce small
interfaces, probably in `maturana-ops` or a thin `maturana-host` module:

- `CommandRunner`
- `SshClient`
- `FileTransfer`
- `ServiceManager`
- `HostNetwork`
- `Clock` where timing/leases matter

The goal is not dependency injection everywhere. The goal is to make high-risk
operations testable without running real `ssh`, `sudo`, `scp`, `powershell`, or
`systemctl`, and to stop scattering command-construction policy across files.

### 5. Extract spec/schema into its own lightweight crate

Create a small crate, for example `maturana-spec`, containing:

- `AgentSpec` and nested spec structs
- markdown frontmatter parsing/rendering
- validation
- path/id/secret-source validation helpers

Then `maturana-core`, `maturana-web`, `maturana-ops`, and tests can depend on
the spec contract without pulling in provider/proxy/session/tool code.

This also makes it easier to treat `MATURANA.md` as the durable public contract.

### 6. Split core by capability, not by tiny file

After `maturana-spec`, consider extracting only natural dependency islands:

- `maturana-session`: SQLite session queue, progress side-lane, queue policies
- `maturana-pipelock`: vault, secret-source resolution, proxy config/audit
- `maturana-provider`: provider trait plus Firecracker/Hyper-V implementations
- `maturana-guest`: guest worker renderers and harness metadata
- `maturana-tools`: tool registry and optional WASM runtime

Do not split every existing module into a crate. That would make Maturana feel
larger, not leaner.

### 7. Feature-gate heavy surfaces

The CLI currently defaults to the WASM runtime and links the web cockpit. That
fits a batteries-included binary, but it makes the core less lean.

Consider features such as:

- `web`: enables `maturana-web`
- `wasm-runtime`: current optional core runtime
- `voice`: OpenAI audio helpers
- `channel-slack`: tungstenite/socket mode
- `graph-ingest`: PDF/Office ingest

Keep the release binary full-featured if desired, but allow development,
testing, and constrained installs to build smaller surfaces.

### 8. Move reusable web/CLI duplication into shared modules

Examples found during the scan:

- ID validation exists in both web API helpers and core validation style
- web sessions keep a `SILENCE_SENTINEL` copy because the CLI owns the constant
- web agent actions now call shared ops for restart, board launch, deploy,
  doctor, and orchestrator abort
- web file safety policy is local to the web API

Some duplication is healthy for defense-in-depth, especially web file safety.
But protocol constants and product operations should move to shared crates so
the cockpit is not subtly different from the CLI.

### 9. Keep scripts as leaf adapters, but move CLI-side script orchestration into ops modules

`docs/script-boundary.md` is strong and should stay. The next improvement is to
make the Rust side follow the same layering:

```text
Codex skill -> CLI/web -> maturana-ops decision -> host adapter -> leaf script/primitive
```

This preserves the "Rust owns decisions" rule while avoiding "CLI owns all Rust
decisions."

### 10. Add architecture fitness checks

Lightweight checks can prevent the monolith from reforming:

- Fail CI if `maturana-cli/src/main.rs` grows past a small threshold after the
  split.
- Fail CI if `channels.rs` reappears as a large catch-all file.
- Check that `maturana-web/src/api` does not add new `current_exe()` command
  spawning; long-running jobs should be narrow ops functions.
- Keep the existing script-boundary guard.
- Add a dependency-direction check once `maturana-ops` exists:

```text
spec -> core capability crates -> ops -> front ends
```

## Suggested Implementation Sequence

### Phase 1: Mechanical decomposition

Goal: reduce file gravity without changing behavior.

- Move CLI command implementations out of `main.rs` into `commands/*`.
- Move `channels.rs` into a `channels/` module tree.
- Keep public command names and tests unchanged.
- Run the current Rust tests after each move.

Approval size: low risk, mostly file moves.

### Phase 2: Shared application services

Goal: stop CLI/web coupling through subprocesses and closures.

- Add `maturana-ops`.
- Move deploy/restart/doctor/status/agent lifecycle operations from CLI into ops
  functions. Deploy, doctor, status/list, live inspect/stop, board launch,
  Firecracker restart, and orchestrator abort are now covered at the web/API
  boundary; Firecracker profile selection, runtime-plane helpers, TAP setup,
  asset preparation, guest artifact rendering, manifest validation, and
  host-key pinning are in ops; guest-worker install/refresh, auth re-seed
  guarding, MCP config installation, and shared guest SSH helpers are also in
  ops; the high-level Firecracker repair sequence and spawned worker
  provisioning/teardown are ops-owned as well; SSH key repair and Ubuntu cloud
  image preparation now live in `maturana-ops::host_setup`; Windows harness
  setup/repair now lives in `maturana-ops::windows_harness`.
- Move conversation enqueue/slash/context front-door behavior into ops.
- Update web to call ops directly for normal operations.

Approval size: medium risk, behavior-preserving but touches call paths.

### Phase 3: Core crate slimming

Goal: reduce dependency blast radius.

- Extract `maturana-spec`.
- Extract `maturana-session` if useful after ops work.
- Extract `maturana-pipelock` only if proxy/vault dependencies keep spreading.
- Keep provider implementations together until the provider API stabilizes.

Approval size: medium/high risk; best done after command and ops boundaries are
stable.

### Phase 4: Feature gating and fitness checks

Goal: make "lean" enforceable.

- Add optional CLI features for web, wasm, graph ingest, and channel families.
- Keep release defaults as needed.
- Add CI checks for file-size and dependency-direction regressions.

Approval size: low/medium risk if features default to current behavior.

## Things I Would Not Do Yet

- Do not rewrite the provider model first. The current provider trait is small
  and understandable; the problem is where surrounding provisioning workflows
  live.
- Do not split every core module into a crate. That creates ceremony without
  reducing conceptual load.
- Do not move channel logic into scripts or external services. Keep the product
  behavior in Rust.
- Do not make `maturana-web` a separate control plane. It should remain a front
  end over the same operations as CLI/Codex skills.
- Do not change CLI behavior during the first decomposition pass.

## Proposed Approval Decision

Approve Phase 1 as a behavior-preserving decomposition:

1. Split `main.rs` into command modules.
2. Split `channels.rs` into a channel module tree.
3. Preserve command names, output shape, and tests.
4. Do not introduce new crates until after the large files are decomposed.

Then approve Phase 2 separately:

1. Add `maturana-ops`.
2. Move reusable product operations behind functions.
3. Replace ordinary web subprocess calls with direct ops calls.

This keeps the work incremental and reviewable while moving Maturana toward the
lean Unix-like shape described in `AGENTS.md`.
