//! Multi-agent orchestration: `maturana orchestrator loop "<goal>"`.
//!
//! One host-side program turns a single plain-English goal into a small list of
//! steps and runs them across multiple worker agents until the goal is met or a
//! hard limit stops it. The agents genuinely do each other's work — one agent's
//! result becomes another's input — but this program is always the single thing
//! moving the messages and holding the limits, which is what keeps a fan-out
//! across many agents from looping forever or costing without bound.
//!
//! It is deliberately NOT a supervised plane process (those get auto-restarted
//! forever); like `proactive serve` it is a normal program you launch, so when a
//! run finishes it actually ends.
//!
//! Safety lives in [`maturana_core::orchestrator_budget`]: the turn budget only
//! counts down, overrides only tighten, and a plan that could not finish in
//! budget is rejected before any step runs. This module owns the *scheduling*
//! (which steps are ready, how many to run at once) and the *protocol* (how a
//! coordinator's plan is parsed, how a reviewer's verdict is read).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use maturana_core::orchestrator_budget::{CapsOverride, OrchestratorCaps, RunBudget, SlotCounter};
use maturana_core::roles::{marker, RoleRegistry, RolePlacement};
use maturana_core::state::MaturanaHome;

// Worker step / coordinator / synthesizer timeouts are owned by the A2A client
// ([`crate::a2a`]), which has its own per-call deadline.

#[derive(Debug, Args)]
pub struct OrchestratorCommand {
    #[command(subcommand)]
    pub command: OrchestratorSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorSubcommand {
    /// Run a goal to completion across multiple worker agents, with hard limits.
    ///
    /// By DEFAULT this reuses the agents you already have running — no config, no
    /// VM spawning. It only spawns dedicated VMs when you ask for it with
    /// `--base-spec`. Where the result is written is set with `--output`.
    Loop {
        /// The goal, in plain English.
        goal: String,
        /// Where to write the result. A prose answer goes to this file (default
        /// `<run>/answer.md`); a file/code/game deliverable is written as real
        /// files into this directory (default `<run>/output/`).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Skip the verification pass. By default, when the run produces files, an
        /// agent actually runs/exercises them and the goal isn't "done" until they
        /// work (bounded by the review-cycle cap). Pass this to deliver unchecked.
        #[arg(long)]
        no_verify: bool,
        /// Reuse specific running agents, comma-separated and strongest-coder
        /// first (e.g. `codex-firecracker,claude-firecracker`). Roles are assigned
        /// across them. Omit to auto-reuse every running agent.
        #[arg(long)]
        agents: Option<String>,
        /// Override the run id (default: derived from the time + goal).
        #[arg(long)]
        run_id: Option<String>,
        /// Lower the model-turn budget for this run (can only tighten the cap).
        #[arg(long)]
        max_turns: Option<u32>,
        /// Lower the wall-clock budget, in seconds (can only tighten).
        #[arg(long)]
        max_wall_seconds: Option<u64>,
        /// Lower how many steps may run at once (can only tighten).
        #[arg(long)]
        max_parallel: Option<u32>,
        /// Lower how many worker VMs may be alive at once (can only tighten).
        #[arg(long)]
        max_vms: Option<u32>,
        /// Full per-role control via a roles.toml (advanced; overrides the
        /// reuse/spawn defaults).
        #[arg(long)]
        roles_file: Option<PathBuf>,
        /// Opt into on-demand specialized VMs: spawn a fresh worker VM per role by
        /// cloning this base agent/spec (slow, ~minutes per VM). Omit to reuse
        /// standing agents instead.
        #[arg(long)]
        base_spec: Option<String>,
        /// Internal: post progress + the final result back to the chat that
        /// started the run. Set by the `/loop` channel command; omit for a plain
        /// CLI run (which just prints).
        #[command(flatten)]
        chat: ChatTargetArgs,
    },
    /// Show the live step list and budget for a run.
    Status { run_id: String },
    /// Stop a run (takes effect between steps; in-flight steps finish their lease).
    Abort { run_id: String },
}

/// One step's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Not yet runnable, or runnable but not yet sent.
    Waiting,
    /// Sent to a worker, awaiting the reply.
    Running,
    /// Completed with a result.
    Done,
    /// Failed after exhausting retries.
    Failed,
}

fn default_status() -> StepStatus {
    StepStatus::Waiting
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Stable id within the plan, e.g. "s1".
    pub id: String,
    /// Which role does this step (must exist in the role registry).
    pub role: String,
    /// What to do, in plain English.
    pub task: String,
    /// Ids of steps whose results this step needs first.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Whether this step's result must pass a reviewer before it counts as done.
    #[serde(default)]
    pub review: bool,
    #[serde(default = "default_status")]
    pub status: StepStatus,
    /// The worker's result, once Done.
    #[serde(default)]
    pub result: Option<String>,
    /// How many times this step has been sent to a worker.
    #[serde(default)]
    pub attempts: u32,
    /// How many revise rounds the reviewer has asked for.
    #[serde(default)]
    pub review_cycles: u32,
}

/// A plan: the goal plus its steps. Persisted to the run directory and used as
/// the live task board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<Step>,
}

/// What the coordinator returns (no statuses yet — the host fills those in).
#[derive(Debug, Clone, Deserialize)]
struct PlanSpec {
    steps: Vec<PlanStepSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct PlanStepSpec {
    id: String,
    role: String,
    task: String,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    review: bool,
}

impl Plan {
    fn from_spec(goal: &str, spec: PlanSpec) -> Self {
        let steps = spec
            .steps
            .into_iter()
            .map(|s| Step {
                id: s.id,
                role: s.role,
                task: s.task,
                deps: s.deps,
                review: s.review,
                status: StepStatus::Waiting,
                result: None,
                attempts: 0,
                review_cycles: 0,
            })
            .collect();
        Self {
            goal: goal.to_string(),
            steps,
        }
    }

    fn status_of(&self, id: &str) -> Option<StepStatus> {
        self.steps.iter().find(|s| s.id == id).map(|s| s.status)
    }

    /// Steps that are waiting AND whose every dependency is already Done — the
    /// only steps eligible to be sent this tick.
    pub fn ready_steps(&self) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| s.status == StepStatus::Waiting)
            .filter(|s| {
                s.deps
                    .iter()
                    .all(|d| self.status_of(d) == Some(StepStatus::Done))
            })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.steps.iter().all(|s| s.status == StepStatus::Done)
    }

    pub fn has_failure(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// The concatenated results of a step's dependencies, to hand the worker as
    /// context. Empty when the step has no dependencies.
    pub fn dependency_context(&self, step: &Step) -> String {
        let mut out = String::new();
        for dep_id in &step.deps {
            if let Some(dep) = self.steps.iter().find(|s| &s.id == dep_id) {
                if let Some(result) = &dep.result {
                    out.push_str(&format!("\n### Result of {} ({})\n{}\n", dep.id, dep.role, result));
                }
            }
        }
        out
    }

    /// Reject a plan that references unknown roles or unknown/self dependencies,
    /// or that contains a dependency cycle — before any step runs. A cyclic plan
    /// could deadlock (no step ever becomes ready); a bad reference can never
    /// complete. Returns the first problem found.
    pub fn validate(&self, registry: &RoleRegistry) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("plan has no steps".to_string());
        }
        let ids: std::collections::HashSet<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();
        if ids.len() != self.steps.len() {
            return Err("duplicate step ids".to_string());
        }
        for step in &self.steps {
            if registry.get(&step.role).is_none() {
                return Err(format!("step {} uses unknown role '{}'", step.id, step.role));
            }
            for dep in &step.deps {
                if dep == &step.id {
                    return Err(format!("step {} depends on itself", step.id));
                }
                if !ids.contains(dep.as_str()) {
                    return Err(format!("step {} depends on unknown step '{}'", step.id, dep));
                }
            }
        }
        self.check_acyclic()
    }

    /// Depth-first cycle detection over the dependency edges.
    fn check_acyclic(&self) -> Result<(), String> {
        use std::collections::HashMap;
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        let mut state: HashMap<&str, Mark> = HashMap::new();
        // Iterative DFS to avoid stack worries on pathological plans.
        for start in self.steps.iter() {
            if state.get(start.id.as_str()).is_some() {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(start.id.as_str(), 0)];
            state.insert(start.id.as_str(), Mark::Visiting);
            while let Some((id, idx)) = stack.last().copied() {
                let step = self.steps.iter().find(|s| s.id == id).unwrap();
                if idx < step.deps.len() {
                    *stack.last_mut().unwrap() = (id, idx + 1);
                    let dep = step.deps[idx].as_str();
                    match state.get(dep) {
                        Some(Mark::Visiting) => {
                            return Err(format!("dependency cycle through step '{dep}'"));
                        }
                        Some(Mark::Done) => {}
                        None => {
                            state.insert(dep, Mark::Visiting);
                            stack.push((dep, 0));
                        }
                    }
                } else {
                    state.insert(id, Mark::Done);
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

/// Pull the first balanced JSON object out of a model reply that may wrap it in
/// prose or a ```json fence. Returns the object text, or None if there isn't one.
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '"' if !escaped => in_string = !in_string,
            '\\' if in_string => {
                escaped = !escaped;
                continue;
            }
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
        escaped = false;
    }
    None
}

/// Parse a coordinator reply into a validated plan.
fn parse_plan(goal: &str, reply: &str, registry: &RoleRegistry) -> Result<Plan, String> {
    let json = extract_json_object(reply).ok_or("coordinator reply had no JSON plan")?;
    let spec: PlanSpec = serde_json::from_str(json).map_err(|e| format!("plan JSON invalid: {e}"))?;
    let plan = Plan::from_spec(goal, spec);
    plan.validate(registry)?;
    Ok(plan)
}

/// The framing the loop sends the coordinator to get a machine-readable plan.
fn coordinator_task(goal: &str, registry: &RoleRegistry, caps: &OrchestratorCaps) -> String {
    format!(
        "Goal:\n{goal}\n\n\
         Break this into at most {} steps. Available worker roles: {}.\n\
         Reply with ONLY a JSON object of this exact shape (no prose):\n\
         {{\"steps\":[{{\"id\":\"s1\",\"role\":\"researcher\",\"task\":\"...\",\"deps\":[],\"review\":false}}]}}\n\
         - `id` is a short unique string. `deps` lists ids whose results this step needs first.\n\
         - Set `review` true for steps whose output should be checked before it counts as done.\n\
         - Keep the plan as small as will satisfy the goal.",
        caps.max_steps,
        registry
            .names()
            .into_iter()
            .filter(|n| n != "coordinator" && n != "synthesizer")
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A stricter re-prompt sent after a coordinator reply we couldn't parse — some
/// models answer in prose or wrap the JSON, so this leaves no room for either.
fn coordinator_retry_task(goal: &str, registry: &RoleRegistry, caps: &OrchestratorCaps) -> String {
    format!(
        "{}\n\nIMPORTANT: your previous reply could not be parsed. Reply with ONLY \
         the JSON object and nothing else — no prose, no markdown fences, no code \
         block. Your reply must start with {{ and end with }}.",
        coordinator_task(goal, registry, caps)
    )
}

/// A short, single-line preview of a model reply for surfacing in an error so the
/// user/operator can see what the coordinator actually said.
fn reply_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "(empty reply — the coordinator agent likely timed out or returned nothing)".to_string()
    } else {
        trimmed.chars().take(240).collect::<String>().replace('\n', " ")
    }
}

/// Read a reviewer reply's verdict.
enum ReviewVerdict {
    Approve,
    Revise(String),
    /// No recognizable verdict marker — counts as one failed review cycle.
    Unclear,
}

fn parse_review(reply: &str) -> ReviewVerdict {
    if reply.contains(marker::REVIEW_APPROVE) {
        ReviewVerdict::Approve
    } else if let Some(idx) = reply.find(marker::REVIEW_REVISE) {
        let feedback = reply[idx + marker::REVIEW_REVISE.len()..].trim().to_string();
        ReviewVerdict::Revise(feedback)
    } else {
        ReviewVerdict::Unclear
    }
}

// ===== Run directory + persistence =====

fn run_dir(home: &MaturanaHome, run_id: &str) -> PathBuf {
    home.root().join("orchestration").join(run_id)
}

fn save_plan(home: &MaturanaHome, run_id: &str, plan: &Plan) -> anyhow::Result<()> {
    let dir = run_dir(home, run_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("plan.json"), serde_json::to_string_pretty(plan)?)?;
    Ok(())
}

fn abort_marker(home: &MaturanaHome, run_id: &str) -> PathBuf {
    run_dir(home, run_id).join("abort")
}

fn is_aborted(home: &MaturanaHome, run_id: &str) -> bool {
    abort_marker(home, run_id).exists()
}

// ===== Worker resolution =====

/// A resolved place to run a role's work: a concrete agent (A2A re-derives its
/// session from the agent's worker env, so we don't carry it here).
#[derive(Clone)]
struct Worker {
    agent_id: String,
    model: Option<String>,
}

/// A spawned worker VM to tear down at the end of a run.
struct SpawnedVm {
    agent_id: String,
    tap_name: String,
}

/// Resolves each role to a concrete worker — once per run, then cached — and
/// remembers spawned VMs so they are torn down when the run ends. A `reuse` role
/// maps straight to a standing agent; a `spawn` role brings up a dedicated VM on
/// demand (cloned from `base_spec`, bounded by the concurrent-VM cap), which is
/// what lets a run use more workers than happen to be running. A role's VM is
/// reused across all of that role's steps, so steps for one role serialize on its
/// single worker while different roles run in parallel.
struct WorkerPool<'a> {
    home: &'a MaturanaHome,
    registry: &'a RoleRegistry,
    run_id: String,
    base_spec: String,
    cache: std::collections::HashMap<String, Worker>,
    spawned: Vec<SpawnedVm>,
    vm_slots: SlotCounter,
    /// Total time spent booting/provisioning spawned VMs. Excluded from the run's
    /// wall-clock budget, which bounds agent WORK, not one-time infra setup (a
    /// slow 8GB rootfs copy must not eat the budget meant for the actual steps).
    spawn_elapsed: Duration,
}

impl<'a> WorkerPool<'a> {
    fn new(
        home: &'a MaturanaHome,
        registry: &'a RoleRegistry,
        run_id: String,
        base_spec: String,
        max_vms: u32,
    ) -> Self {
        Self {
            home,
            registry,
            run_id,
            base_spec,
            cache: std::collections::HashMap::new(),
            spawned: Vec::new(),
            vm_slots: SlotCounter::new(max_vms),
            spawn_elapsed: Duration::ZERO,
        }
    }

    /// Total time spent spawning VMs so far, excluded from the wall budget.
    fn spawn_elapsed(&self) -> Duration {
        self.spawn_elapsed
    }

    fn resolve(&mut self, role_name: &str) -> anyhow::Result<Worker> {
        if let Some(worker) = self.cache.get(role_name) {
            return Ok(worker.clone());
        }
        let role = self
            .registry
            .get(role_name)
            .ok_or_else(|| anyhow::anyhow!("unknown role '{role_name}'"))?;
        let model = role.model.clone();
        let worker = match role.placement.clone() {
            RolePlacement::Reuse { agent_id } => Worker { agent_id, model },
            RolePlacement::Spawn { .. } => {
                if !self.vm_slots.try_acquire() {
                    anyhow::bail!(
                        "the concurrent worker-VM cap is reached; raise --max-vms or use fewer roles"
                    );
                }
                // `used_host_octets` re-scans materialized specs each time, and a
                // just-spawned VM has already written its spec, so sequential
                // spawns never collide.
                let used = maturana_core::orchestrator_spawn::used_host_octets(
                    self.home,
                    maturana_core::orchestrator_spawn::DEFAULT_SUBNET,
                );
                let net = maturana_core::orchestrator_spawn::allocate_net(
                    maturana_core::orchestrator_spawn::DEFAULT_SUBNET,
                    &used,
                )
                .ok_or_else(|| anyhow::anyhow!("no free network address for a spawned worker VM"))?;
                let new_id = format!("orch-{}-{}", self.run_id, role_name);
                let session_id = format!("{new_id}-main");
                println!("  spawning a specialized VM for role '{role_name}' (cloning {})", self.base_spec);
                let spawn_start = Instant::now();
                crate::orchestrator_spawn_worker(self.home, &self.base_spec, &new_id, &session_id, &net)?;
                self.spawn_elapsed += spawn_start.elapsed();
                self.spawned.push(SpawnedVm {
                    agent_id: new_id.clone(),
                    tap_name: net.tap_name.clone(),
                });
                Worker { agent_id: new_id, model }
            }
        };
        self.cache.insert(role_name.to_string(), worker.clone());
        Ok(worker)
    }

    fn teardown(&mut self) {
        for vm in std::mem::take(&mut self.spawned) {
            println!("  tearing down spawned worker {}", vm.agent_id);
            let _ = crate::orchestrator_teardown_worker(self.home, &vm.agent_id, &vm.tap_name);
        }
    }
}

// ===== A2A dispatch =====

/// The A2A wire the orchestrator dispatches over: a loopback A2A server's base
/// URL + the sessiond token. The master orchestrator sends every worker step as
/// an A2A `message/send` to this — so it speaks the same protocol as an agent
/// delegating to a peer in-band.
#[derive(Clone)]
struct A2aWire {
    base: String,
    token: String,
}

/// Send one task to `agent` as an A2A `message/send` and return the reply text,
/// or an error if the task came back failed/timed-out. `context_id` is the run
/// id so all of a run's turns share an A2A context. The orchestrator is at
/// delegation depth 0.
fn a2a_send(
    a2a: &A2aWire,
    agent: &str,
    model: Option<&str>,
    context_id: &str,
    text: &str,
) -> anyhow::Result<String> {
    let mut message = maturana_core::a2a::Message::user_text(&maturana_core::a2a::gen_id(), text);
    message.context_id = Some(context_id.to_string());
    let mut metadata = serde_json::json!({ "maturana_depth": 0 });
    if let Some(m) = model {
        metadata["maturana_model"] = serde_json::json!(m);
    }
    message.metadata = Some(metadata);
    let task = crate::a2a::a2a_client_send(&a2a.base, agent, &a2a.token, message)?;
    match task.status.state {
        maturana_core::a2a::TaskState::Completed => task
            .result_text()
            .ok_or_else(|| anyhow::anyhow!("A2A task completed with no result")),
        _ => anyhow::bail!(
            "{}",
            task.status
                .message
                .map(|m| m.text())
                .unwrap_or_else(|| "A2A task failed".to_string())
        ),
    }
}

/// Charge one turn, then dispatch `task` to `worker` over A2A and wait for the
/// reply. Used for the coordinator, synthesizer, and the synchronous review of a
/// step. Charging before the call means an in-flight turn is always paid for.
fn dispatch_and_wait(
    home: &MaturanaHome,
    a2a: &A2aWire,
    worker: &Worker,
    run_id: &str,
    task: &str,
    budget: &mut RunBudget,
) -> anyhow::Result<String> {
    budget
        .spend_turn()
        .map_err(|_| anyhow::anyhow!("turn budget exhausted"))?;
    if is_aborted(home, run_id) {
        anyhow::bail!("run aborted");
    }
    a2a_send(a2a, &worker.agent_id, worker.model.as_deref(), run_id, task)
}

fn build_step_task(registry: &RoleRegistry, plan: &Plan, step: &Step) -> String {
    let base = registry
        .frame_task(&step.role, &step.task)
        .unwrap_or_else(|| step.task.clone());
    let ctx = plan.dependency_context(step);
    if ctx.trim().is_empty() {
        base
    } else {
        format!("{base}\n\n--- INPUTS FROM EARLIER STEPS ---{ctx}")
    }
}

// ===== The loop =====

pub fn handle_orchestrator(command: OrchestratorCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        OrchestratorSubcommand::Loop {
            goal,
            output,
            no_verify,
            agents,
            run_id,
            max_turns,
            max_wall_seconds,
            max_parallel,
            max_vms,
            roles_file,
            base_spec,
            chat,
        } => {
            let overrides = CapsOverride {
                max_total_turns: max_turns,
                max_wall_seconds,
                max_parallel,
                max_concurrent_vms: max_vms,
                max_steps: None,
            };
            let placement = PlacementChoice {
                roles_file,
                agents,
                base_spec,
            };
            run_loop(home, &goal, run_id, overrides, placement, output, !no_verify, chat.resolve())
        }
        OrchestratorSubcommand::Status { run_id } => status(home, &run_id),
        OrchestratorSubcommand::Abort { run_id } => {
            std::fs::create_dir_all(run_dir(home, &run_id))?;
            std::fs::write(abort_marker(home, &run_id), "aborted")?;
            println!("orchestrator: abort requested for {run_id} (in-flight steps finish their lease)");
            Ok(())
        }
    }
}

fn status(home: &MaturanaHome, run_id: &str) -> anyhow::Result<()> {
    let path = run_dir(home, run_id).join("plan.json");
    if !path.exists() {
        println!("orchestrator: no run '{run_id}'");
        return Ok(());
    }
    let plan: Plan = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    println!("orchestrator run {run_id} — goal: {}", plan.goal);
    for step in &plan.steps {
        println!(
            "  {:<6} {:<12} {:?}{}",
            step.id,
            step.role,
            step.status,
            if step.deps.is_empty() {
                String::new()
            } else {
                format!("  (after {})", step.deps.join(", "))
            }
        );
    }
    if is_aborted(home, run_id) {
        println!("  [abort requested]");
    }
    Ok(())
}

/// How the run picks its workers. Mutually-resolved in priority order:
/// `roles_file` (full control) > `base_spec` (spawn dedicated VMs) > `agents`
/// (reuse a named list) > default (reuse every running agent).
struct PlacementChoice {
    roles_file: Option<PathBuf>,
    agents: Option<String>,
    base_spec: Option<String>,
}

/// Reusable standing agents, strongest-coder first. Reads the materialized
/// specs under `<home>/agents`, skips orchestrator-spawned workers (`orch-*`),
/// and orders by harness so the heavy roles land on a capable coder by default
/// (codex, then claude-code, then everything else, ties broken by id).
fn discover_reusable_agents(home: &MaturanaHome) -> Vec<String> {
    use maturana_core::HarnessRuntime;
    let rank = |harness: Option<HarnessRuntime>| match harness {
        Some(HarnessRuntime::Codex) => 0u8,
        Some(HarnessRuntime::ClaudeCode) => 1,
        Some(HarnessRuntime::Opencode) => 2,
        None => 3,
    };
    let Ok(entries) = std::fs::read_dir(home.agents_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<(u8, String)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if id.starts_with("orch-") {
            continue; // a previous run's ephemeral spawned worker
        }
        let spec_path = entry.path().join("MATURANA.md");
        if !spec_path.exists() {
            continue;
        }
        let harness = maturana_core::AgentSpec::from_maturana_markdown(&spec_path)
            .ok()
            .map(|s| s.runtime.harness);
        found.push((rank(harness), id));
    }
    found.sort();
    found.into_iter().map(|(_, id)| id).collect()
}

fn resolve_registry(home: &MaturanaHome, placement: &PlacementChoice) -> anyhow::Result<RoleRegistry> {
    if let Some(path) = &placement.roles_file {
        // Advanced: full per-role control. Un-overridden default roles spawn, so
        // keep a base-spec fallback for them.
        let base = placement.base_spec.clone().unwrap_or_else(|| "worker-base".to_string());
        return RoleRegistry::load_or_default(path, &base);
    }
    if let Some(spec) = &placement.base_spec {
        // Opt-in: spawn a dedicated VM per role by cloning this base.
        println!("  placement: spawning dedicated VMs (cloning {spec})");
        return Ok(RoleRegistry::defaults(spec));
    }
    let agents: Vec<String> = match &placement.agents {
        Some(csv) => csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => discover_reusable_agents(home),
    };
    if agents.is_empty() {
        anyhow::bail!(
            "no running agents to reuse. Launch agents first (`maturana list` to see them), \
             pass --agents <id,id>, or spawn dedicated VMs with --base-spec <agent-or-spec>."
        );
    }
    println!("  placement: reusing agents {}", agents.join(", "));
    Ok(RoleRegistry::reuse_across(&agents))
}

#[allow(clippy::too_many_arguments)]
/// The chat a `/loop` run reports back to. Every field travels as a `--chat-*`
/// flag from the channel command; the orchestrator posts its progress + final
/// result to this chat's outbox (channel-agnostic — the channel's own delivery
/// thread sends it). A plain CLI run leaves these unset and only prints.
#[derive(Debug, Clone, Default, Args)]
pub struct ChatTargetArgs {
    #[arg(long = "chat-channel")]
    pub chat_channel: Option<String>,
    #[arg(long = "chat-platform-id")]
    pub chat_platform_id: Option<String>,
    #[arg(long = "chat-thread-id")]
    pub chat_thread_id: Option<String>,
    #[arg(long = "chat-agent")]
    pub chat_agent: Option<String>,
    #[arg(long = "chat-session")]
    pub chat_session: Option<String>,
}

#[derive(Debug, Clone)]
struct ChatTarget {
    channel: String,
    platform_id: String,
    thread_id: Option<String>,
    agent_id: String,
    session_id: String,
}

impl ChatTargetArgs {
    /// A target only when the channel actually addressed one (all required fields
    /// present together); otherwise this is a plain CLI run that just prints.
    fn resolve(self) -> Option<ChatTarget> {
        Some(ChatTarget {
            channel: self.chat_channel?,
            platform_id: self.chat_platform_id?,
            agent_id: self.chat_agent?,
            session_id: self.chat_session?,
            thread_id: self.chat_thread_id,
        })
    }
}

/// Post one literal status line to the originating chat's outbox; the channel's
/// running delivery thread sends it. Best-effort — a failed post never stops the
/// run, and a plain CLI run (chat = None) is a no-op.
fn post_chat(home: &MaturanaHome, chat: Option<&ChatTarget>, text: &str) {
    let _ = post_chat_result(home, chat, text);
}

/// Like [`post_chat`] but surfaces whether the outbox write actually succeeded, so
/// a caller that reports delivery (card result delivery) doesn't claim success when
/// the write silently failed. `Ok(false)` = no chat target (nothing to do).
fn post_chat_result(home: &MaturanaHome, chat: Option<&ChatTarget>, text: &str) -> anyhow::Result<bool> {
    let Some(c) = chat else { return Ok(false) };
    let paths = maturana_core::session_db::session_paths(&home.agent_dir(&c.agent_id), &c.session_id);
    // write_outbound opens the db with create, but won't make its parent dir — a
    // real chat's session dir already exists, but ensure it so a status post is
    // never silently dropped.
    if let Some(parent) = paths.outbound_db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({ "text": text }).to_string();
    maturana_core::session_db::write_outbound(
        &paths,
        None,
        "chat",
        &c.channel,
        &c.platform_id,
        c.thread_id.as_deref(),
        &body,
    )?;
    Ok(true)
}

/// Where a finished card's result should be delivered. Resolved to a concrete
/// chat on an agent whose bridge is actually serving the channel — NOT the worker
/// that ran the card. The worker only produces the content; it reaches the human
/// through the same outbound bridge any agent reply uses.
#[derive(Debug)]
struct DeliveryTarget {
    channel: String,
    platform_id: String,
    agent_id: String,
    session_id: String,
}

/// Resolve a card's `deliver` spec to a concrete delivery target, the same way a
/// normal agent reply leaves the system: pick the agent that actually *serves*
/// the requested channel (a live bridge with a known destination) and address its
/// main chat session. `deliver` is a channel name (`telegram`, `discord`, …) or
/// `channel:agent` to pin a specific agent. `prefer` (the card's worker) is tried
/// first when it also serves the channel. Returns `Err(reason)` — for honest
/// logging — when no agent can push on that channel from this host.
fn resolve_channel_delivery(
    home: &MaturanaHome,
    deliver: &str,
    prefer: Option<&str>,
) -> Result<DeliveryTarget, String> {
    let (channel, explicit_agent) = match deliver.split_once(':') {
        Some((c, a)) => (
            c.trim().to_ascii_lowercase(),
            Some(a.trim().to_string()).filter(|s| !s.is_empty()),
        ),
        None => (deliver.trim().to_ascii_lowercase(), None),
    };
    // Candidate agents, preferring an explicit pin, then the worker, then the rest
    // in a stable order so resolution is deterministic across runs.
    let candidates: Vec<String> = match &explicit_agent {
        Some(a) => vec![a.clone()],
        None => {
            let mut ids = crate::discover_agent_ids(home).unwrap_or_default();
            ids.sort();
            if let Some(p) = prefer {
                if let Some(pos) = ids.iter().position(|x| x == p) {
                    let pref = ids.remove(pos);
                    ids.insert(0, pref);
                }
            }
            ids
        }
    };
    // An explicitly pinned agent bypasses the liveness gate (the operator chose it;
    // its bridge may just be restarting and the row waits in the outbox).
    let require_live = explicit_agent.is_none();
    let session_of = |a: &str| {
        crate::infer_agent_session_id(home, a).unwrap_or_else(|_| format!("{a}-main"))
    };
    match channel.as_str() {
        "telegram" => {
            for a in &candidates {
                if require_live && !crate::channels::telegram_bridge_live(home, a) {
                    continue;
                }
                if let Some(chat_id) = crate::channels::current_paired_telegram_chat_id(home, a) {
                    return Ok(DeliveryTarget {
                        channel: "telegram".to_string(),
                        platform_id: chat_id.to_string(),
                        agent_id: a.clone(),
                        session_id: session_of(a),
                    });
                }
            }
            Err("no agent with a live Telegram bridge and a paired chat".to_string())
        }
        "discord" => {
            for a in &candidates {
                if let Some(channel_id) = crate::channels::current_discord_delivery_channel(home, a) {
                    return Ok(DeliveryTarget {
                        channel: "discord".to_string(),
                        platform_id: channel_id,
                        agent_id: a.clone(),
                        session_id: session_of(a),
                    });
                }
            }
            Err("no agent with a known Discord destination (the bot learns a channel only once someone messages it there)".to_string())
        }
        other => Err(format!(
            "channel '{other}' has no host-side push destination yet (it replies only to its last-seen conversation) — use telegram or discord, or pin an agent with channel:agent"
        )),
    }
}

/// Best-effort host-side delivery of a finished card's result, through the SAME
/// outbound bridge any agent reply uses (`post_chat` → the owner agent's running
/// channel bridge sends it). The worker that ran the card has no channel
/// credentials and never sends anything itself; the host routes the result to the
/// agent that serves the requested channel. Channel-agnostic — Telegram, Discord
/// and any future channel go through one path. Failures are logged loudly and
/// never fail the run; the result is always on the board regardless.
fn deliver_card_result(home: &MaturanaHome, worker_agent: &str, deliver: &str, text: &str) {
    // The worker produced nothing worth sending (empty, or the silence sentinel a
    // self-check emits) — never queue an empty/sentinel "result" as if it were one.
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == crate::proactive::SILENCE_SENTINEL {
        eprintln!("  deliver: card produced no deliverable content — nothing sent");
        return;
    }
    match resolve_channel_delivery(home, deliver, Some(worker_agent)) {
        Ok(target) => {
            let chat = ChatTarget {
                channel: target.channel.clone(),
                platform_id: target.platform_id.clone(),
                thread_id: None,
                agent_id: target.agent_id.clone(),
                session_id: target.session_id.clone(),
            };
            // Honest reporting: only claim the result reached the bridge if the
            // outbox write actually succeeded. The bridge then performs the send.
            match post_chat_result(home, Some(&chat), text) {
                Ok(true) => println!(
                    "  -> queued result for delivery via {} to {} ({}); the bridge will send it",
                    target.channel, target.agent_id, target.platform_id
                ),
                Ok(false) => eprintln!("  deliver: no chat target resolved — result kept on the board only"),
                Err(error) => eprintln!(
                    "  deliver: FAILED to write result to {}'s outbox ({error:#}) — result kept on the board only",
                    target.agent_id
                ),
            }
        }
        Err(reason) => {
            eprintln!("  deliver: {reason} — result kept on the board only");
        }
    }
}

/// Like [`post_chat`], but attaches host-side files; the channel's delivery sink
/// uploads them where supported (Telegram sendDocument) and otherwise names them.
/// Non-existent paths are dropped; if none remain it degrades to a text post.
fn post_chat_files(home: &MaturanaHome, chat: Option<&ChatTarget>, text: &str, files: &[String]) {
    let Some(c) = chat else { return };
    let existing: Vec<String> = files
        .iter()
        .filter(|f| std::path::Path::new(f).is_file())
        .cloned()
        .collect();
    if existing.is_empty() {
        post_chat(home, Some(c), text);
        return;
    }
    let paths = maturana_core::session_db::session_paths(&home.agent_dir(&c.agent_id), &c.session_id);
    if let Some(parent) = paths.outbound_db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({ "text": text, "files": existing }).to_string();
    let _ = maturana_core::session_db::write_outbound(
        &paths,
        None,
        "chat",
        &c.channel,
        &c.platform_id,
        c.thread_id.as_deref(),
        &body,
    );
}

/// A compact, chat-friendly rendering of the plan (one line per step).
fn plan_chat_summary(plan: &Plan) -> String {
    plan.steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let task: String = step.task.chars().take(80).collect();
            format!("{}. [{}] {}", i + 1, step.role, task.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_loop(
    home: &MaturanaHome,
    goal: &str,
    run_id: Option<String>,
    overrides: CapsOverride,
    placement: PlacementChoice,
    output: Option<PathBuf>,
    verify: bool,
    chat: Option<ChatTarget>,
) -> anyhow::Result<()> {
    let caps = OrchestratorCaps::default().tighten_with(&overrides);
    let registry = resolve_registry(home, &placement)?;
    // Only spawn-placement roles consume this; reuse runs never touch it.
    let base_spec = placement.base_spec.clone().unwrap_or_default();
    let run_id = run_id.unwrap_or_else(|| format!("run-{}", chrono::Utc::now().timestamp()));
    std::fs::create_dir_all(run_dir(home, &run_id))?;
    println!("orchestrator: run {run_id}");
    println!("  goal: {goal}");
    post_chat(
        home,
        chat.as_ref(),
        &format!("🔄 On it — {goal}\nRun `{run_id}`. Breaking the goal into steps…"),
    );
    println!(
        "  caps: {} turns / {}s wall / {} parallel / {} VMs",
        caps.max_total_turns, caps.max_wall_seconds, caps.max_parallel, caps.max_concurrent_vms
    );

    // The orchestrator dispatches every worker step over the A2A protocol. Start
    // a loopback A2A server for this run (dies with the process) and send to it,
    // so the master orchestrator speaks the SAME protocol as an agent delegating
    // to a peer in-band.
    let token = std::fs::read_to_string(home.root().join("sessiond/token"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        anyhow::bail!(
            "the A2A wire needs the sessiond token at {}/sessiond/token",
            home.root().display()
        );
    }
    let a2a = A2aWire {
        base: crate::a2a::start_local_a2a_server(home, &token)?,
        token,
    };

    // Run the loop with a worker pool, then ALWAYS tear down any spawned VMs —
    // whatever way run_inner returns (success, a budget stop, or an error).
    let mut pool = WorkerPool::new(
        home,
        &registry,
        run_id.clone(),
        base_spec,
        caps.max_concurrent_vms,
    );
    let result = run_inner(
        home, goal, &run_id, &registry, &caps, &mut pool, &a2a, output.as_deref(), verify,
        chat.as_ref(),
    );
    pool.teardown();
    if let Err(error) = &result {
        post_chat(home, chat.as_ref(), &format!("⚠️ Loop `{run_id}` stopped: {error:#}"));
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    home: &MaturanaHome,
    goal: &str,
    run_id: &str,
    registry: &RoleRegistry,
    caps: &OrchestratorCaps,
    pool: &mut WorkerPool,
    a2a: &A2aWire,
    output: Option<&std::path::Path>,
    verify: bool,
    chat: Option<&ChatTarget>,
) -> anyhow::Result<()> {
    let mut budget = RunBudget::new(caps.clone());
    let started = Instant::now();
    let wall = Duration::from_secs(caps.max_wall_seconds);

    // Where workers write real files, and where we stage what we fetch back. The
    // deliverable is the actual bytes a worker produced in its VM — copied out
    // over scp — not a final agent's retyping of them from a text summary.
    let out_remote = remote_out_dir(run_id);
    let staging_dir = run_dir(home, run_id).join("staging");
    let mut collected_total = 0usize;
    // The worker whose VM holds the most produced files — where verification runs
    // it, since the files are already there.
    let mut builder: Option<String> = None;
    let mut builder_files = 0usize;

    // --- Plan: ask the coordinator to break the goal into steps ---
    let coordinator = pool.resolve("coordinator")?;
    let mut plan_reply = dispatch_and_wait(
        home,
        a2a,
        &coordinator,
        run_id,
        &coordinator_task(goal, registry, caps),
        &mut budget,
    )?;
    let mut plan = match parse_plan(goal, &plan_reply, registry) {
        Ok(plan) => plan,
        Err(first_err) => {
            // Models occasionally answer in prose, wrap the JSON, or return an empty
            // preamble. Retry ONCE with a stricter JSON-only re-prompt before giving
            // up — and if it still fails, surface what the coordinator actually said.
            eprintln!(
                "orchestrator: first plan unusable ({first_err}); retrying the coordinator with a stricter prompt"
            );
            post_chat(home, chat, "📋 The first plan wasn't usable — re-asking the coordinator…");
            if budget.turns_remaining() > 0 && !is_aborted(home, run_id) {
                plan_reply = dispatch_and_wait(
                    home,
                    a2a,
                    &coordinator,
                    run_id,
                    &coordinator_retry_task(goal, registry, caps),
                    &mut budget,
                )?;
            }
            parse_plan(goal, &plan_reply, registry).map_err(|e| {
                anyhow::anyhow!(
                    "planning failed: {e}. The coordinator ({}) replied: {}",
                    coordinator.agent_id,
                    reply_preview(&plan_reply)
                )
            })?
        }
    };
    if !budget.admits_plan(plan.steps.len() as u32) {
        anyhow::bail!(
            "the {}-step plan could exceed the {} remaining turn budget; simplify the goal or raise --max-turns",
            plan.steps.len(),
            budget.turns_remaining()
        );
    }
    save_plan(home, run_id, &plan)?;
    println!("  plan: {} steps", plan.steps.len());
    post_chat(
        home,
        chat,
        &format!(
            "📋 Plan — {} steps:\n{}\n\nRunning now…",
            plan.steps.len(),
            plan_chat_summary(&plan)
        ),
    );

    // --- Execute: run each ready step over the A2A wire until done or stopped ---
    let mut stop_reason = "completed";
    loop {
        // Liveness backstop first, then wall-clock, then budget, then abort — all
        // independent of whether any progress happened.
        if budget.tick().is_err() {
            stop_reason = "tick ceiling reached";
            break;
        }
        if started.elapsed().saturating_sub(pool.spawn_elapsed()) >= wall {
            stop_reason = "wall-clock budget reached";
            break;
        }
        if budget.turns_remaining() == 0 {
            stop_reason = "turn budget exhausted";
            break;
        }
        if is_aborted(home, run_id) {
            stop_reason = "aborted";
            break;
        }
        if plan.is_complete() {
            break;
        }
        if plan.has_failure() {
            stop_reason = "a step failed";
            break;
        }

        let ready_ids: Vec<String> = plan.ready_steps().iter().map(|s| s.id.clone()).collect();
        if ready_ids.is_empty() {
            stop_reason = "no runnable steps left";
            break;
        }
        for sid in ready_ids {
            let step = plan.steps.iter().find(|s| s.id == sid).unwrap().clone();
            let worker = match pool.resolve(&step.role) {
                Ok(w) => w,
                Err(error) => {
                    eprintln!("orchestrator: step {sid} role '{}' unresolved: {error:#}", step.role);
                    if let Some(s) = plan.step_mut(&sid) {
                        s.status = StepStatus::Failed;
                    }
                    continue;
                }
            };
            if let Some(s) = plan.step_mut(&sid) {
                s.status = StepStatus::Running;
                s.attempts += 1;
            }
            let framed = format!(
                "{}\n\n--- PRODUCING FILES ---\n\
                 If this step creates any files (code, a webpage, a script, data, an \
                 image), WRITE them as real files into the directory {out_remote}/ \
                 (create it). Keep your text reply a brief summary of what you produced — \
                 do NOT paste full file contents into your reply; the files are collected \
                 from that directory automatically.",
                build_step_task(registry, &plan, &step)
            );
            println!("  -> step {sid} ({}) -> {} via A2A", step.role, worker.agent_id);
            match dispatch_and_wait(home, a2a, &worker, run_id, &framed, &mut budget) {
                Ok(reply) => {
                    let result = finish_step(
                        home, a2a, registry, run_id, &mut plan, &sid, &worker, reply, &mut budget, pool,
                    )?;
                    if let Some(s) = plan.step_mut(&sid) {
                        s.result = Some(result);
                        s.status = StepStatus::Done;
                    }
                    // Pull the real files this worker wrote out of its VM (best-effort).
                    let got = collect_step_artifacts(home, &worker.agent_id, &out_remote, &staging_dir);
                    if got > 0 {
                        collected_total += got;
                        if got >= builder_files {
                            builder_files = got;
                            builder = Some(worker.agent_id.clone());
                        }
                        println!("  <- step {sid} done (+{got} file(s) collected)");
                    } else {
                        println!("  <- step {sid} done");
                    }
                    let done = plan.steps.iter().filter(|s| s.status == StepStatus::Done).count();
                    post_chat(
                        home,
                        chat,
                        &format!("✓ {done}/{} — {} ({sid})", plan.steps.len(), step.role),
                    );
                }
                Err(error) => {
                    eprintln!("orchestrator: step {sid} failed: {error:#}");
                    if let Some(s) = plan.step_mut(&sid) {
                        s.status = StepStatus::Failed;
                    }
                }
            }
            save_plan(home, run_id, &plan)?;
        }
    }

    save_plan(home, run_id, &plan)?;

    if !plan.is_complete() {
        anyhow::bail!("orchestrator run {run_id} stopped before completion: {stop_reason}");
    }

    // --- Deliver ---
    // If the workers actually produced files, THOSE are the deliverable: the real
    // bytes, copied straight out of the agents' VMs. We do NOT ask another agent
    // to retype them from a summary (that loses binaries, truncates big files, and
    // pays a turn to recreate something we already have). The synthesizer only runs
    // as a fallback when the goal was prose and nobody wrote a file.
    let collected_root = staging_dir.join(out_basename(&out_remote));
    if collected_total > 0 && count_files(&collected_root) > 0 {
        // Before calling the goal done, actually RUN the deliverable: an agent
        // exercises the files in its VM (the builder's, where they already are),
        // fixes them if broken, and we re-verify — bounded by the review cap.
        let verdict = if verify {
            run_verification(
                home,
                a2a,
                run_id,
                goal,
                &out_remote,
                &staging_dir,
                builder.as_deref(),
                caps,
                &mut budget,
            )
        } else {
            VerifyOutcome::Skipped
        };

        let dir = output_dir_for(home, run_id, output);
        std::fs::create_dir_all(&dir)?;
        // Copy AFTER verification so the delivered bytes include any fixes.
        let mut names = copy_tree(&collected_root, &dir)?;
        names.sort();
        std::fs::write(
            dir.join("SUMMARY.md"),
            build_run_summary(goal, &plan, &names, &verdict),
        )?;
        println!(
            "\n=== orchestrator run {run_id}: wrote {} file(s) to {} [{}] ===",
            names.len(),
            dir.display(),
            verdict.label()
        );
        for name in &names {
            println!("  - {name}");
        }
        println!("{}", verdict.detail());
        let file_paths: Vec<String> = names
            .iter()
            .take(10)
            .map(|n| dir.join(n).display().to_string())
            .collect();
        post_chat_files(
            home,
            chat,
            &format!(
                "✅ Done — Loop `{run_id}` [{}] — {} file(s)",
                verdict.label(),
                names.len()
            ),
            &file_paths,
        );
        return Ok(());
    }

    // No files were produced — synthesize a prose (or, as a last resort, manifest)
    // answer from the step results.
    let synthesizer = pool.resolve("synthesizer")?;
    let mut summary = format!("Goal:\n{goal}\n\nCompleted step results:\n");
    for step in &plan.steps {
        if let Some(result) = &step.result {
            summary.push_str(&format!("\n## {} ({})\n{}\n", step.id, step.role, result));
        }
    }
    summary.push_str(DELIVERABLE_FORMAT);
    let synth_task = registry
        .frame_task("synthesizer", &summary)
        .unwrap_or(summary);
    let reply = dispatch_and_wait(home, a2a, &synthesizer, run_id, &synth_task, &mut budget)?;
    let reply = reply
        .replace(marker::DONE, "")
        .replace(marker::BLOCKED, "")
        .trim()
        .to_string();

    match write_deliverable(home, run_id, output, &reply)? {
        Deliverable::Files { dir, names } => {
            println!(
                "\n=== orchestrator run {run_id}: wrote {} file(s) to {} ===",
                names.len(),
                dir.display()
            );
            for name in &names {
                println!("  - {name}");
            }
            let file_paths: Vec<String> = names
                .iter()
                .take(10)
                .map(|n| dir.join(n).display().to_string())
                .collect();
            post_chat_files(
                home,
                chat,
                &format!("✅ Done — Loop `{run_id}` — {} file(s)", names.len()),
                &file_paths,
            );
        }
        Deliverable::Prose { path } => {
            println!(
                "\n=== orchestrator run {run_id}: final answer ({}) ===\n{reply}",
                path.display()
            );
            let answer: String = reply.chars().take(3500).collect();
            post_chat(home, chat, &format!("✅ Done — Loop `{run_id}`\n\n{answer}"));
        }
    }
    Ok(())
}

/// Appended to the synthesizer's task: tells it to emit a file manifest when the
/// goal is a concrete artifact, or prose otherwise. The host materializes the
/// manifest into real files (see [`write_deliverable`]).
const DELIVERABLE_FORMAT: &str = "\n\n--- OUTPUT FORMAT ---\n\
If the goal asks for one or more FILES (code, a game, a script, a webpage, a \
document set), reply with ONLY a single JSON object and nothing else, in exactly \
this shape:\n\
{\"files\":[{\"path\":\"index.html\",\"content\":\"<full file contents>\"}]}\n\
Use complete, working file contents and sensible relative paths (forward slashes, \
never absolute or ..). Do NOT wrap the JSON in markdown fences. Otherwise, reply \
with the prose answer directly. End your reply with [[ORCH_DONE]] on its own final line.";

/// What the run produced and where it landed.
enum Deliverable {
    Files { dir: PathBuf, names: Vec<String> },
    Prose { path: PathBuf },
}

#[derive(Deserialize)]
struct FileManifest {
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
struct ManifestFile {
    path: String,
    content: String,
}

/// Pull a `{"files":[...]}` manifest out of the synthesizer reply, if present.
/// Accepts a bare JSON object or one inside a ```json fence; returns `None` for
/// a prose answer so the caller falls back to writing the text as-is.
fn extract_file_manifest(reply: &str) -> Option<Vec<ManifestFile>> {
    let candidate = if let Some(start) = reply.find("```json") {
        let rest = &reply[start + "```json".len()..];
        rest.find("```").map(|end| rest[..end].trim().to_string())
    } else {
        let start = reply.find('{')?;
        let end = reply.rfind('}')?;
        if end > start {
            Some(reply[start..=end].to_string())
        } else {
            None
        }
    }?;
    let manifest: FileManifest = serde_json::from_str(&candidate).ok()?;
    if manifest.files.is_empty() {
        return None;
    }
    Some(manifest.files)
}

/// Normalize a manifest path to a safe relative path under the output dir:
/// strip a leading slash, drop `.`/`..` and empty components. Returns `None` if
/// nothing usable is left.
fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in raw.replace('\\', "/").split('/') {
        match part {
            "" | "." | ".." => continue,
            p => out.push(p),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Write the synthesizer's deliverable. A file manifest becomes real files under
/// the output directory (`--output` or `<run>/output/`); prose becomes a single
/// file (`--output` or `<run>/answer.md`).
fn write_deliverable(
    home: &MaturanaHome,
    run_id: &str,
    output: Option<&std::path::Path>,
    reply: &str,
) -> anyhow::Result<Deliverable> {
    if let Some(files) = extract_file_manifest(reply) {
        let dir = output
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| run_dir(home, run_id).join("output"));
        std::fs::create_dir_all(&dir)?;
        let mut names = Vec::new();
        for file in files {
            let Some(rel) = safe_relative_path(&file.path) else {
                continue;
            };
            let dest = dir.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, file.content)?;
            names.push(rel.to_string_lossy().to_string());
        }
        // Keep a copy of the raw synthesis next to the run for debugging.
        let _ = std::fs::write(run_dir(home, run_id).join("answer.md"), reply);
        return Ok(Deliverable::Files { dir, names });
    }

    let path = match output {
        Some(p) if p.is_dir() || p.to_string_lossy().ends_with('/') => {
            std::fs::create_dir_all(p)?;
            p.join("answer.md")
        }
        Some(p) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p.to_path_buf()
        }
        None => run_dir(home, run_id).join("answer.md"),
    };
    std::fs::write(&path, reply)?;
    Ok(Deliverable::Prose { path })
}

// ===== Real-artifact collection =====
//
// The deliverable for a build task is the actual files an agent wrote, not a
// second agent's retyping of them. Each worker is told to write its files into a
// per-run directory in its VM; after the step we scp that directory out and the
// collected files ARE the result. Everything here is best-effort: if a worker is
// unreachable or wrote nothing, we fall back to the prose/synthesizer path.

/// The directory inside a worker's VM where it writes the files it produces.
/// Per-run, so a reused agent never serves a previous run's leftovers.
fn remote_out_dir(run_id: &str) -> String {
    format!("/workspace/maturana-out-{run_id}")
}

/// The trailing component of a remote out dir — what `scp -r` creates locally.
fn out_basename(remote: &str) -> String {
    remote.rsplit('/').find(|s| !s.is_empty()).unwrap_or("out").to_string()
}

/// The private key for SSHing into a worker's guest, by provider. Firecracker
/// guests use the baked image key; anything else the default agent key.
pub(crate) fn guest_ssh_key(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    let provider = maturana_core::AgentSpec::from_maturana_markdown(
        home.agent_dir(agent_id).join("MATURANA.md"),
    )
    .ok()
    .map(|spec| spec.vm.provider);
    match provider {
        Some(maturana_core::HostProvider::Firecracker) => home
            .root()
            .join("images/firecracker/maturana-firecracker.id_rsa"),
        _ => home.root().join("keys/maturana-agent-ed25519"),
    }
}

/// Copy the files a worker wrote into `remote_dir` out of its VM and into
/// `staging_dir`. Returns how many NEW files landed. Best-effort — any failure
/// (agent unreachable, empty dir, missing key) returns 0 without aborting.
fn collect_step_artifacts(
    home: &MaturanaHome,
    agent_id: &str,
    remote_dir: &str,
    staging_dir: &std::path::Path,
) -> usize {
    // Resolve the IP without the strict whole-plan validation `inspect` runs:
    // a file-producing card must not lose its deliverable to unrelated config
    // drift. Infra failures below are surfaced loudly; only a genuinely empty
    // output dir (the worker wrote nothing) stays quiet — that is normal for an
    // analysis card.
    let ip = match crate::resolve_transfer_ip(home, agent_id) {
        Ok(ip) => ip,
        Err(error) => {
            eprintln!("  (could not resolve {agent_id} guest IP to collect files: {error})");
            return 0;
        }
    };
    let key = guest_ssh_key(home, agent_id);
    if !key.exists() {
        eprintln!(
            "  (no guest SSH key for {agent_id} at {}; cannot collect files)",
            key.display()
        );
        return 0;
    }
    let host_key = match crate::GuestHostKey::resolve(home, agent_id, &ip) {
        Ok(host_key) => host_key,
        Err(error) => {
            eprintln!("  (could not prepare host key for {agent_id}: {error})");
            return 0;
        }
    };
    // Skip the copy entirely if the worker produced nothing.
    let probe = format!("ls -A {remote_dir} 2>/dev/null | head -1");
    match crate::run_ssh_with_stdin(&ip, "ubuntu", &key, &host_key, &probe, None, crate::SSH_TIMEOUT_QUICK) {
        Ok(listing) if !listing.trim().is_empty() => {}
        Ok(_) => return 0, // worker wrote nothing — normal, stay quiet
        Err(error) => {
            eprintln!("  (could not reach {agent_id} guest to collect files: {error})");
            return 0;
        }
    }
    if std::fs::create_dir_all(staging_dir).is_err() {
        return 0;
    }
    let roots = crate::agent_transfer_roots(home, agent_id, false)
        .unwrap_or_else(|_| vec!["/workspace".to_string()]);
    let landing = staging_dir.join(out_basename(remote_dir));
    let before = count_files(&landing);
    match crate::fetch_live_path(
        &ip,
        "ubuntu",
        &key,
        &host_key,
        remote_dir,
        &staging_dir.to_path_buf(),
        &roots,
        true,
    ) {
        Ok(()) => count_files(&landing).saturating_sub(before),
        Err(error) => {
            eprintln!("  (could not collect files from {agent_id}: {error})");
            0
        }
    }
}

/// Count regular files under `dir`, recursively (0 if it doesn't exist).
fn count_files(dir: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files(&path);
            } else if path.is_file() {
                count += 1;
            }
        }
    }
    count
}

/// Recursively copy every file under `src` into `dst`, preserving layout.
/// Returns the relative paths written (forward-slashed).
fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    copy_tree_inner(src, src, dst, &mut names)?;
    Ok(names)
}

fn copy_tree_inner(
    root: &std::path::Path,
    cur: &std::path::Path,
    dst: &std::path::Path,
    names: &mut Vec<String>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(cur)? {
        let path = entry?.path();
        if path.is_dir() {
            copy_tree_inner(root, &path, dst, names)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let target = dst.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target)?;
            names.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Where collected files (and the SUMMARY) are written: `--output` if given,
/// else `<run>/output`.
fn output_dir_for(home: &MaturanaHome, run_id: &str, output: Option<&std::path::Path>) -> PathBuf {
    match output {
        Some(path) => path.to_path_buf(),
        None => run_dir(home, run_id).join("output"),
    }
}

/// A short human summary placed beside the real files: the goal, the list of
/// files produced, the verification verdict, and each step's own brief report.
/// Not a rewrite of the files.
fn build_run_summary(goal: &str, plan: &Plan, files: &[String], verdict: &VerifyOutcome) -> String {
    let mut out = format!("# {goal}\n\n## Verification\n{}\n", verdict.detail().trim());
    out.push_str("\n## Files produced\n");
    if files.is_empty() {
        out.push_str("- (none)\n");
    }
    for file in files {
        out.push_str(&format!("- {file}\n"));
    }
    out.push_str("\n## How it was built\n");
    for step in &plan.steps {
        if let Some(result) = &step.result {
            out.push_str(&format!("\n### {} ({})\n{}\n", step.id, step.role, result.trim()));
        }
    }
    out
}

// ===== Verification: actually run the deliverable before calling it done =====

/// The outcome of running the produced files.
enum VerifyOutcome {
    /// An agent ran the deliverable and it works.
    Passed,
    /// It still failed after the bounded repair attempts (with the last reason).
    Failed(String),
    /// Verification could not be completed (no builder, budget out, no verdict).
    Inconclusive(String),
    /// `--no-verify`, or there was nothing runnable to check.
    Skipped,
}

impl VerifyOutcome {
    fn label(&self) -> &'static str {
        match self {
            VerifyOutcome::Passed => "verified: runs",
            VerifyOutcome::Failed(_) => "NOT verified",
            VerifyOutcome::Inconclusive(_) => "unverified",
            VerifyOutcome::Skipped => "verification skipped",
        }
    }

    fn detail(&self) -> String {
        match self {
            VerifyOutcome::Passed => {
                "verification: an agent ran the deliverable and confirmed it works.".to_string()
            }
            VerifyOutcome::Failed(why) => {
                format!("verification: FAILED after repair attempts — {}", why.trim())
            }
            VerifyOutcome::Inconclusive(why) => {
                format!("verification: inconclusive — {}", why.trim())
            }
            VerifyOutcome::Skipped => "verification: skipped.".to_string(),
        }
    }
}

/// Run the produced files in the builder's VM, fixing and re-checking up to the
/// review-cycle cap, and report the verdict. The files live in `out_remote` in
/// that agent's VM already, so it tests them in place; after each attempt we
/// re-collect so any fixes reach the delivered bytes.
#[allow(clippy::too_many_arguments)]
fn run_verification(
    home: &MaturanaHome,
    a2a: &A2aWire,
    run_id: &str,
    goal: &str,
    out_remote: &str,
    staging_dir: &std::path::Path,
    builder: Option<&str>,
    caps: &OrchestratorCaps,
    budget: &mut RunBudget,
) -> VerifyOutcome {
    let Some(agent_id) = builder else {
        return VerifyOutcome::Inconclusive("no agent produced runnable files".to_string());
    };
    let worker = Worker {
        agent_id: agent_id.to_string(),
        model: None,
    };
    let mut last_fail = String::from("still failing");
    // One initial check plus up to `max_review_cycles` re-checks after fixes.
    for _ in 0..=caps.max_review_cycles {
        if budget.turns_remaining() == 0 {
            return VerifyOutcome::Inconclusive(
                "turn budget ran out before verification finished".to_string(),
            );
        }
        println!("  -> verifying deliverable on {agent_id}…");
        let reply = match dispatch_and_wait(home, a2a, &worker, run_id, &verify_task(goal, out_remote), budget) {
            Ok(reply) => reply,
            Err(error) => {
                return VerifyOutcome::Inconclusive(format!("verifier dispatch failed: {error}"))
            }
        };
        // The verifier may have edited files in place — re-collect so we deliver
        // the fixed versions.
        collect_step_artifacts(home, agent_id, out_remote, staging_dir);
        match parse_verify(&reply) {
            Some(true) => {
                println!("  <- verification passed");
                return VerifyOutcome::Passed;
            }
            Some(false) => {
                last_fail = verify_detail(&reply);
                println!("  <- verification failed; repairing: {last_fail}");
            }
            None => {
                return VerifyOutcome::Inconclusive(
                    "verifier did not report PASS or FAIL".to_string(),
                )
            }
        }
    }
    VerifyOutcome::Failed(last_fail)
}

/// The verifier's task: exercise whatever was built, and fix it in place if it
/// doesn't work. Deliberately type-agnostic — the agent decides how to run it.
fn verify_task(goal: &str, out_remote: &str) -> String {
    format!(
        "You are the VERIFIER. The files produced for this goal are in {out_remote} in your \
         workspace.\n\nGoal: {goal}\n\n\
         ACTUALLY EXERCISE the deliverable the way a user would: run the program, execute the \
         script, open the page in a headless browser, call the endpoint — whatever fits what was \
         built. Decide whether it genuinely works and meets the goal.\n\
         - If it works, reply with {pass} on its own line plus ONE line on what you checked.\n\
         - If it does NOT work, FIX the files in place inside {out_remote}, then reply with {fail} \
         on its own line followed by what was broken and what you changed.\n\
         Keep your reply brief; do not paste full file contents.",
        pass = marker::VERIFY_PASS,
        fail = marker::VERIFY_FAIL,
    )
}

/// `Some(true)` on PASS, `Some(false)` on FAIL, `None` if neither marker is present.
fn parse_verify(reply: &str) -> Option<bool> {
    if reply.contains(marker::VERIFY_PASS) {
        Some(true)
    } else if reply.contains(marker::VERIFY_FAIL) {
        Some(false)
    } else {
        None
    }
}

/// The text after the FAIL marker (what was wrong / changed), trimmed and capped.
fn verify_detail(reply: &str) -> String {
    let tail = reply
        .split(marker::VERIFY_FAIL)
        .nth(1)
        .unwrap_or("")
        .trim();
    let one_line = tail.replace('\n', " ");
    if one_line.is_empty() {
        "no detail given".to_string()
    } else if one_line.chars().count() > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}

/// Complete one step, running the bounded reviewer loop synchronously if the step
/// asked for review. Returns the accepted result text. Each reviewer turn and
/// each revise turn charges the budget, so review ping-pong has a hard ceiling.
#[allow(clippy::too_many_arguments)]
fn finish_step(
    home: &MaturanaHome,
    a2a: &A2aWire,
    registry: &RoleRegistry,
    run_id: &str,
    plan: &mut Plan,
    sid: &str,
    worker: &Worker,
    worker_reply: String,
    budget: &mut RunBudget,
    pool: &mut WorkerPool,
) -> anyhow::Result<String> {
    let step = plan.steps.iter().find(|s| s.id == sid).unwrap().clone();
    if !step.review {
        return Ok(worker_reply);
    }
    let max_cycles = budget.caps().max_review_cycles;
    let mut current = worker_reply;
    let mut cycles = 0u32;
    while cycles < max_cycles {
        let reviewer = match pool.resolve("reviewer") {
            Ok(w) => w,
            Err(_) => break, // no reviewer available; accept what we have
        };
        let review_task = registry
            .frame_task(
                "reviewer",
                &format!(
                    "Acceptance criteria (the step's task):\n{}\n\nResult to check:\n{}",
                    step.task, current
                ),
            )
            .unwrap_or_default();
        let verdict = match dispatch_and_wait(home, a2a, &reviewer, run_id, &review_task, budget) {
            Ok(v) => v,
            Err(_) => break,
        };
        match parse_review(&verdict) {
            ReviewVerdict::Approve => return Ok(current),
            ReviewVerdict::Revise(feedback) => {
                cycles += 1;
                if let Some(s) = plan.step_mut(sid) {
                    s.review_cycles = cycles;
                }
                let revise_task = registry
                    .frame_task(
                        &step.role,
                        &format!(
                            "Revise your result for this task.\nTask:\n{}\nReviewer feedback:\n{}\nYour previous result:\n{}",
                            step.task, feedback, current
                        ),
                    )
                    .unwrap_or_default();
                current = dispatch_and_wait(home, a2a, worker, run_id, &revise_task, budget)?;
            }
            ReviewVerdict::Unclear => {
                cycles += 1;
                if let Some(s) = plan.step_mut(sid) {
                    s.review_cycles = cycles;
                }
            }
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> RoleRegistry {
        RoleRegistry::defaults("worker-base")
    }

    fn step(id: &str, deps: &[&str], status: StepStatus) -> Step {
        Step {
            id: id.to_string(),
            role: "researcher".to_string(),
            task: "t".to_string(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            review: false,
            status,
            result: None,
            attempts: 0,
            review_cycles: 0,
        }
    }

    #[test]
    fn manifest_extracted_from_bare_json_and_fenced() {
        // Bare JSON object.
        let files = extract_file_manifest(
            r#"{"files":[{"path":"index.html","content":"<h1>hi</h1>"},{"path":"game.js","content":"x=1"}]}"#,
        )
        .expect("bare json manifest");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "index.html");

        // Fenced, with chatter around it.
        let fenced = "Here you go:\n```json\n{\"files\":[{\"path\":\"a.py\",\"content\":\"print(1)\"}]}\n```\nDone.";
        let files = extract_file_manifest(fenced).expect("fenced manifest");
        assert_eq!(files[0].path, "a.py");
    }

    #[test]
    fn channel_delivery_resolution_is_honest_when_unreachable() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("maturana-deliver-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&root).unwrap();
        let home = MaturanaHome::new(root.clone());

        // No live bridges → telegram resolution fails with a clear reason, never panics.
        let err = resolve_channel_delivery(&home, "telegram", None).unwrap_err();
        assert!(err.contains("Telegram"), "got: {err}");

        // A channel with no host-side push path is reported honestly (and the
        // `channel:agent` form still parses to the channel arm).
        let err = resolve_channel_delivery(&home, "slack:claude-firecracker", None).unwrap_err();
        assert!(err.contains("no host-side push destination"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prose_is_not_a_manifest() {
        assert!(extract_file_manifest("The answer is 42. Paris has ~2.1M people.").is_none());
        // A JSON object without a files array is prose, not a manifest.
        assert!(extract_file_manifest(r#"{"answer":"42"}"#).is_none());
        // An empty files array is not a deliverable.
        assert!(extract_file_manifest(r#"{"files":[]}"#).is_none());
    }

    #[test]
    fn safe_relative_path_blocks_traversal_and_absolutes() {
        assert_eq!(safe_relative_path("a/b.txt").unwrap(), PathBuf::from("a/b.txt"));
        // Leading slash, .. and . components are stripped — nothing escapes the dir.
        assert_eq!(safe_relative_path("/etc/passwd").unwrap(), PathBuf::from("etc/passwd"));
        assert_eq!(safe_relative_path("../../x").unwrap(), PathBuf::from("x"));
        assert_eq!(safe_relative_path("src/./main.rs").unwrap(), PathBuf::from("src/main.rs"));
        // Nothing usable left.
        assert!(safe_relative_path("../..").is_none());
        assert!(safe_relative_path("").is_none());
    }

    #[test]
    fn out_basename_is_the_last_segment() {
        assert_eq!(out_basename("/workspace/maturana-out-run-123"), "maturana-out-run-123");
        assert_eq!(out_basename("/workspace/maturana-out-run-123/"), "maturana-out-run-123");
    }

    #[test]
    fn copy_tree_preserves_layout_and_counts_files() {
        let base = std::env::temp_dir().join(format!("orch-copytree-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("css")).unwrap();
        std::fs::write(src.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(src.join("css/style.css"), "body{}").unwrap();
        assert_eq!(count_files(&src), 2);
        assert_eq!(count_files(&base.join("nope")), 0);

        let mut names = copy_tree(&src, &dst).unwrap();
        names.sort();
        assert_eq!(names, vec!["css/style.css".to_string(), "index.html".to_string()]);
        // The real bytes are copied, not regenerated.
        assert_eq!(std::fs::read_to_string(dst.join("index.html")).unwrap(), "<h1>hi</h1>");
        assert_eq!(std::fs::read_to_string(dst.join("css/style.css")).unwrap(), "body{}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_summary_lists_files_and_step_reports() {
        let mut dev = step("s1", &[], StepStatus::Done);
        dev.role = "developer".to_string();
        dev.result = Some("Wrote index.html with the board and win logic.".to_string());
        let plan = Plan { goal: "build a game".to_string(), steps: vec![dev] };
        let md = build_run_summary(
            "build a game",
            &plan,
            &["index.html".to_string()],
            &VerifyOutcome::Passed,
        );
        assert!(md.contains("## Files produced"));
        assert!(md.contains("- index.html"));
        assert!(md.contains("Wrote index.html")); // the step's own words, not a rewrite
        assert!(md.contains("## Verification"));
        assert!(md.contains("confirmed it works"));
    }

    #[test]
    fn parse_verify_reads_the_markers() {
        assert_eq!(parse_verify("all good\n[[VERIFY: PASS]]\nchecked the board"), Some(true));
        assert_eq!(parse_verify("[[VERIFY: FAIL]] the reset button throws"), Some(false));
        assert_eq!(parse_verify("I think it's probably fine"), None);
    }

    #[test]
    fn verify_detail_extracts_the_failure_reason() {
        let reply = "[[VERIFY: FAIL]] script.js referenced a missing id; added it.";
        assert_eq!(verify_detail(reply), "script.js referenced a missing id; added it.");
        // No body after the marker still yields something printable.
        assert_eq!(verify_detail("[[VERIFY: FAIL]]"), "no detail given");
    }

    #[test]
    fn verify_task_names_the_dir_goal_and_markers() {
        let task = verify_task("build a game", "/workspace/maturana-out-run-1");
        assert!(task.contains("/workspace/maturana-out-run-1"));
        assert!(task.contains("build a game"));
        assert!(task.contains(marker::VERIFY_PASS));
        assert!(task.contains(marker::VERIFY_FAIL));
        // It must tell the agent to actually run it and fix in place.
        assert!(task.contains("EXERCISE"));
        assert!(task.contains("FIX the files in place"));
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let plan = Plan {
            goal: "g".to_string(),
            steps: vec![
                step("s1", &[], StepStatus::Done),
                step("s2", &["s1"], StepStatus::Waiting), // ready: dep done
                step("s3", &["s2"], StepStatus::Waiting), // not ready: dep waiting
            ],
        };
        let ready: Vec<&str> = plan.ready_steps().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ready, vec!["s2"]);
    }

    #[test]
    fn validate_rejects_cycles_unknown_roles_and_bad_deps() {
        let reg = registry();
        // A -> B -> A cycle.
        let cyclic = Plan {
            goal: "g".to_string(),
            steps: vec![step("a", &["b"], StepStatus::Waiting), step("b", &["a"], StepStatus::Waiting)],
        };
        assert!(cyclic.validate(&reg).is_err(), "cycle must be rejected");

        // Unknown role.
        let mut bad_role = Plan {
            goal: "g".to_string(),
            steps: vec![step("s1", &[], StepStatus::Waiting)],
        };
        bad_role.steps[0].role = "wizard".to_string();
        assert!(bad_role.validate(&reg).is_err(), "unknown role must be rejected");

        // Dangling dependency.
        let dangling = Plan {
            goal: "g".to_string(),
            steps: vec![step("s1", &["ghost"], StepStatus::Waiting)],
        };
        assert!(dangling.validate(&reg).is_err(), "unknown dep must be rejected");

        // A legal linear plan passes.
        let ok = Plan {
            goal: "g".to_string(),
            steps: vec![step("s1", &[], StepStatus::Waiting), step("s2", &["s1"], StepStatus::Waiting)],
        };
        assert!(ok.validate(&reg).is_ok());
    }

    #[test]
    fn parse_plan_extracts_json_from_prose() {
        let reg = registry();
        let reply = "Sure! Here is the plan:\n```json\n\
            {\"steps\":[{\"id\":\"s1\",\"role\":\"researcher\",\"task\":\"find X\",\"deps\":[],\"review\":false}]}\n\
            ```\nHope that helps.";
        let plan = parse_plan("the goal", reply, &reg).expect("should parse");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].role, "researcher");
        assert_eq!(plan.steps[0].status, StepStatus::Waiting);
    }

    #[test]
    fn dependency_context_includes_upstream_results() {
        let mut plan = Plan {
            goal: "g".to_string(),
            steps: vec![step("s1", &[], StepStatus::Done), step("s2", &["s1"], StepStatus::Waiting)],
        };
        plan.steps[0].result = Some("the answer is 42".to_string());
        let ctx = plan.dependency_context(&plan.steps[1].clone());
        assert!(ctx.contains("the answer is 42"));
        assert!(ctx.contains("s1"));
    }

    #[test]
    fn review_verdict_is_read_from_markers() {
        assert!(matches!(parse_review("looks good [[REVIEW: APPROVE]]"), ReviewVerdict::Approve));
        match parse_review("[[REVIEW: REVISE]] fix the title") {
            ReviewVerdict::Revise(fb) => assert_eq!(fb, "fix the title"),
            _ => panic!("expected revise"),
        }
        assert!(matches!(parse_review("no marker here"), ReviewVerdict::Unclear));
    }
}

// ===== Durable orchestration board =====
//
// A persistent, user-editable board of cards that a dispatcher runs across
// agents — the durable cousin of `orchestrator loop` (which is a one-shot
// goal->plan->done run). The user authors the cards (title, assignee, deps); the
// dispatcher claims every ready card and runs it on its assignee over A2A, in
// parallel up to max_parallel, with the SAME host-enforced budgets, VM-per-card
// isolation, and real-artifact collection as the loop. The board never becomes a
// new (weaker) execution substrate: a card always runs in its assignee's own VM
// over A2A — there is no local/Docker/SSH shortcut. State lives on the board
// (board/<name>.json, atomic writes); cards coordinate only through their written
// results (dependency_context), never shared memory; an interrupted run is
// reclaimed (Doing->Todo) on the next run; every transition is appended to a run
// log (board/<name>.events.jsonl) for audit + live cockpit tailing.

#[derive(Debug, Args)]
pub struct BoardCommand {
    #[command(subcommand)]
    pub command: BoardSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum BoardSubcommand {
    /// Add a card to the board.
    Add {
        /// What to do (the card's title).
        title: String,
        /// Longer detail / acceptance criteria.
        #[arg(long)]
        detail: Option<String>,
        /// Who runs it: a role (developer/researcher/reviewer/...) or an agent id.
        #[arg(long)]
        assignee: Option<String>,
        /// Card ids this one depends on, comma-separated (e.g. c1,c2).
        #[arg(long, value_delimiter = ',')]
        needs: Vec<String>,
        /// Higher runs first when more cards are ready than max_parallel.
        #[arg(long)]
        priority: Option<i64>,
        /// Optional namespace tag.
        #[arg(long)]
        tenant: Option<String>,
        /// Don't dispatch until this RFC3339 time.
        #[arg(long)]
        scheduled_at: Option<String>,
        /// Auto-retry a failed card up to N times before blocking.
        #[arg(long)]
        max_retries: Option<u32>,
        /// Goal mode: re-run with an acceptance judge until it passes.
        #[arg(long)]
        goal: bool,
        #[arg(long)]
        goal_max_turns: Option<u32>,
        /// Park in triage (awaiting decompose/specify) instead of todo.
        #[arg(long)]
        triage: bool,
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Show the board's cards by column.
    List {
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Show one card in full (detail, deps, result, comments, run history).
    Show {
        card: String,
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Move a card to a status: triage | todo | doing | done | blocked | archived.
    Move {
        card: String,
        status: String,
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Append a comment to a card's thread.
    Comment {
        card: String,
        text: String,
        #[arg(long, default_value = "human")]
        author: String,
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Archive a card (hide from the active board).
    Archive {
        card: String,
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Decompose a card into child cards via the coordinator agent (LLM).
    Decompose {
        card: String,
        #[arg(long, default_value = "default")]
        board: String,
        #[arg(long)]
        agents: Option<String>,
    },
    /// Flesh out a terse card into a detailed title + body via an agent (LLM).
    Specify {
        card: String,
        #[arg(long, default_value = "default")]
        board: String,
        #[arg(long)]
        agents: Option<String>,
    },
    /// Reset every finished/failed card back to todo for a clean re-run.
    Reset {
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Run the board: dispatch every ready card across its assignee until drained.
    Run {
        #[arg(long, default_value = "default")]
        board: String,
        /// Where to copy any files the cards produced.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Reuse a specific set of agents (comma-separated); default = all running.
        #[arg(long)]
        agents: Option<String>,
        /// Spawn dedicated VMs per role by cloning this base (opt-in; slow).
        #[arg(long)]
        base_spec: Option<String>,
        /// Full per-role control via a roles.toml.
        #[arg(long)]
        roles_file: Option<PathBuf>,
        #[arg(long)]
        max_turns: Option<u32>,
        #[arg(long)]
        max_parallel: Option<u32>,
        #[arg(long)]
        max_wall_seconds: Option<u64>,
        #[arg(long)]
        max_vms: Option<u32>,
    },
    /// Show card counts.
    Status {
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Remove every card from the board.
    Clear {
        #[arg(long, default_value = "default")]
        board: String,
    },
}

pub fn handle_board(command: BoardCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    use maturana_core::board::{Board, CardStatus};
    match command.command {
        BoardSubcommand::Add {
            title,
            detail,
            assignee,
            needs,
            priority,
            tenant,
            scheduled_at,
            max_retries,
            goal,
            goal_max_turns,
            triage,
            board,
        } => {
            let mut b = Board::load(home, &board)?;
            let deps: Vec<String> = needs
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            for dep in &deps {
                if b.card(dep).is_none() {
                    anyhow::bail!("card depends on unknown card '{dep}' (add it first)");
                }
            }
            let scheduled = match scheduled_at {
                Some(s) => Some(
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map_err(|e| anyhow::anyhow!("invalid --scheduled-at: {e}"))?
                        .with_timezone(&chrono::Utc),
                ),
                None => None,
            };
            let id = b.add(&title, detail.as_deref().unwrap_or(""), assignee, deps);
            if let Some(c) = b.card_mut(&id) {
                if let Some(p) = priority {
                    c.priority = p;
                }
                c.tenant = tenant;
                c.scheduled_at = scheduled;
                c.max_retries = max_retries.unwrap_or(0);
                c.goal = goal;
                c.goal_max_turns = goal_max_turns.unwrap_or(0);
                if triage {
                    c.status = CardStatus::Triage;
                }
            }
            b.validate().map_err(|e| anyhow::anyhow!(e))?;
            b.save(home)?;
            println!("added {id}: {title}");
            Ok(())
        }
        BoardSubcommand::List { board } => {
            print_board(&Board::load(home, &board)?);
            Ok(())
        }
        BoardSubcommand::Show { card, board } => {
            let b = Board::load(home, &board)?;
            let c = b.card(&card).ok_or_else(|| anyhow::anyhow!("no card '{card}'"))?;
            println!("{} [{}] {}", c.id, c.status.label(), c.title);
            if !c.detail.is_empty() {
                println!("\n{}", c.detail);
            }
            println!("\nassignee: {}", c.assignee.as_deref().unwrap_or("(default)"));
            if !c.deps.is_empty() {
                println!("deps: {}", c.deps.join(", "));
            }
            if c.priority != 0 {
                println!("priority: {}", c.priority);
            }
            if let Some(k) = &c.block_kind {
                println!("block kind: {k}");
            }
            if let Some(r) = &c.result {
                println!("\nresult:\n{r}");
            }
            if !c.comments.is_empty() {
                println!("\ncomments:");
                for cm in &c.comments {
                    println!("  [{}] {}", cm.author, cm.body);
                }
            }
            if !c.runs.is_empty() {
                println!("\nruns:");
                for r in &c.runs {
                    println!("  #{} {} ({})", r.attempt, r.outcome, r.agent.as_deref().unwrap_or("?"));
                }
            }
            Ok(())
        }
        BoardSubcommand::Move { card, status, board } => {
            let st = CardStatus::parse(&status).ok_or_else(|| {
                anyhow::anyhow!("unknown status '{status}' (triage|todo|doing|done|blocked|archived)")
            })?;
            let mut b = Board::load(home, &board)?;
            b.card_mut(&card)
                .ok_or_else(|| anyhow::anyhow!("no card '{card}' on board {board}"))?
                .status = st;
            b.save(home)?;
            println!("{card} -> {}", st.label());
            Ok(())
        }
        BoardSubcommand::Comment { card, text, author, board } => {
            let mut b = Board::load(home, &board)?;
            if !b.comment(&card, &author, &text) {
                anyhow::bail!("no card '{card}' on board {board}");
            }
            b.save(home)?;
            println!("commented on {card}");
            Ok(())
        }
        BoardSubcommand::Archive { card, board } => {
            let mut b = Board::load(home, &board)?;
            b.card_mut(&card)
                .ok_or_else(|| anyhow::anyhow!("no card '{card}'"))?
                .status = CardStatus::Archived;
            b.save(home)?;
            println!("archived {card}");
            Ok(())
        }
        BoardSubcommand::Decompose { card, board, agents } => {
            decompose_card(home, &board, &card, agents)
        }
        BoardSubcommand::Specify { card, board, agents } => {
            specify_card(home, &board, &card, agents)
        }
        BoardSubcommand::Reset { board } => {
            let mut b = Board::load(home, &board)?;
            let n = b.reset_for_rerun();
            b.save(home)?;
            maturana_core::board::clear_events(home, &b.name);
            println!("reset {n} card(s) on board {} to todo", b.name);
            Ok(())
        }
        BoardSubcommand::Status { board } => {
            let b = Board::load(home, &board)?;
            let (todo, doing, done, blocked) = b.counts();
            println!(
                "board {} — {todo} todo · {doing} doing · {done} done · {blocked} blocked",
                b.name
            );
            Ok(())
        }
        BoardSubcommand::Clear { board } => {
            let mut b = Board::load(home, &board)?;
            let n = b.cards.len();
            b.cards.clear();
            b.save(home)?;
            maturana_core::board::clear_events(home, &b.name);
            println!("cleared {n} card(s) from board {}", b.name);
            Ok(())
        }
        BoardSubcommand::Run {
            board,
            output,
            agents,
            base_spec,
            roles_file,
            max_turns,
            max_parallel,
            max_wall_seconds,
            max_vms,
        } => {
            let overrides = CapsOverride {
                max_total_turns: max_turns,
                max_wall_seconds,
                max_parallel,
                max_concurrent_vms: max_vms,
                max_steps: None,
            };
            let placement = PlacementChoice {
                roles_file,
                agents,
                base_spec,
            };
            run_board(home, &board, overrides, placement, output.as_deref())
        }
    }
}

fn print_board(b: &maturana_core::board::Board) {
    use maturana_core::board::CardStatus;
    println!("board {} ({} cards)", b.name, b.cards.len());
    for status in [
        CardStatus::Triage,
        CardStatus::Todo,
        CardStatus::Doing,
        CardStatus::Blocked,
        CardStatus::Done,
        CardStatus::Archived,
    ] {
        let cards: Vec<_> = b.cards.iter().filter(|c| c.status == status).collect();
        if cards.is_empty() {
            continue;
        }
        println!("\n[{}]", status.label());
        for c in cards {
            let who = c.assignee.as_deref().unwrap_or("(default)");
            let deps = if c.deps.is_empty() {
                String::new()
            } else {
                format!("  after {}", c.deps.join(","))
            };
            println!("  {:<5} {:<44} @{}{}", c.id, c.title, who, deps);
        }
    }
}

fn run_board(
    home: &MaturanaHome,
    board_name: &str,
    overrides: CapsOverride,
    placement: PlacementChoice,
    output: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use maturana_core::board::{log_event, Board};
    let mut board = Board::load(home, board_name)?;
    if board.cards.is_empty() {
        anyhow::bail!("board '{board_name}' is empty — add cards first (`maturana board add ...`)");
    }
    board.validate().map_err(|e| anyhow::anyhow!(e))?;

    // Reclaim a previous run interrupted by a crash/restart: any card stuck in
    // Doing is reset to Todo so this pass picks it up again (Hermes "a dead task
    // gets reclaimed and respawned").
    let reclaimed = board.reclaim_in_flight();
    if reclaimed > 0 {
        println!("board {board_name}: reclaimed {reclaimed} interrupted card(s)");
        log_event(home, board_name, "reclaim", None, &format!("{reclaimed} card(s) reset doing->todo"));
        board.save(home)?;
    }

    let caps = OrchestratorCaps::default().tighten_with(&overrides);
    let registry = resolve_registry(home, &placement)?;
    let run_id = format!("board-{}-{}", board_name, chrono::Utc::now().timestamp());

    let token = std::fs::read_to_string(home.root().join("sessiond/token"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        anyhow::bail!(
            "the A2A wire needs the sessiond token at {}/sessiond/token",
            home.root().display()
        );
    }
    let a2a = A2aWire {
        base: crate::a2a::start_local_a2a_server(home, &token)?,
        token,
    };

    let base_spec = placement.base_spec.clone().unwrap_or_default();
    let mut pool = WorkerPool::new(home, &registry, run_id.clone(), base_spec, caps.max_concurrent_vms);
    let staging_dir = run_dir(home, &run_id).join("staging");
    std::fs::create_dir_all(run_dir(home, &run_id))?;

    println!("board {board_name}: running ({} cards)", board.cards.len());
    println!(
        "  caps: {} turns / {}s wall / {} parallel",
        caps.max_total_turns, caps.max_wall_seconds, caps.max_parallel
    );
    log_event(home, board_name, "run_start", None, &format!("run {run_id} ({} cards)", board.cards.len()));

    let result = run_board_inner(home, &mut board, &registry, &caps, &mut pool, &a2a, &run_id, &staging_dir);
    pool.teardown();
    let _ = board.save(home);

    if staging_dir.exists() && count_files(&staging_dir) > 0 {
        let dir = output_dir_for(home, &run_id, output);
        std::fs::create_dir_all(&dir)?;
        let names = copy_tree(&staging_dir, &dir)?;
        println!("\nboard {board_name}: wrote {} file(s) to {}", names.len(), dir.display());
    }
    let (_, _, done, blocked) = board.counts();
    log_event(home, board_name, "run_end", None, &format!("{done} done, {blocked} blocked"));
    println!();
    print_board(&board);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_board_inner(
    home: &MaturanaHome,
    board: &mut maturana_core::board::Board,
    registry: &RoleRegistry,
    caps: &OrchestratorCaps,
    pool: &mut WorkerPool,
    a2a: &A2aWire,
    run_id: &str,
    staging_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use maturana_core::board::{log_event, CardStatus};
    let board_name = board.name.clone();
    let mut budget = RunBudget::new(caps.clone());
    let started = Instant::now();
    let wall = Duration::from_secs(caps.max_wall_seconds);
    // Goal-mode re-queue counter per card (how many revise rounds it has had).
    let mut goal_turns: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let has_reviewer = registry.get("reviewer").is_some();

    loop {
        if budget.tick().is_err() {
            println!("  [tick ceiling reached]");
            break;
        }
        if started.elapsed() >= wall {
            println!("  [wall-clock budget reached]");
            break;
        }
        if budget.turns_remaining() == 0 {
            println!("  [turn budget exhausted]");
            break;
        }
        if board.is_complete() {
            break;
        }

        let ready: Vec<maturana_core::board::Card> =
            board.ready().into_iter().cloned().collect();
        if ready.is_empty() {
            if board.cards.iter().any(|c| c.status == CardStatus::Todo) {
                println!("  [stuck: remaining cards are blocked by a failed dependency]");
            }
            break;
        }

        // Build a batch up to max_parallel, spending a turn per card up front so
        // the budget stays single-threaded; the A2A I/O then fans out.
        let mut batch: Vec<(String, Worker, String)> = Vec::new();
        let mut agent_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut claimed_at: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
            std::collections::HashMap::new();
        for card in ready.iter().take(caps.max_parallel.max(1) as usize) {
            if budget.spend_turn().is_err() {
                break;
            }
            let worker = resolve_assignee(pool, registry, card.assignee.as_deref())?;
            let task = build_card_task(registry, board, card, &card_out_dir(run_id, &card.id));
            agent_of.insert(card.id.clone(), worker.agent_id.clone());
            claimed_at.insert(card.id.clone(), chrono::Utc::now());
            if let Some(c) = board.card_mut(&card.id) {
                c.status = CardStatus::Doing;
                c.attempts += 1;
            }
            println!("  -> card {} ({}) -> {}", card.id, card.title, worker.agent_id);
            log_event(home, &board_name, "claim", Some(&card.id), &format!("{} -> {}", card.title, worker.agent_id));
            batch.push((card.id.clone(), worker, task));
        }
        if batch.is_empty() {
            println!("  [turn budget exhausted]");
            break;
        }
        board.save(home)?;

        for (id, res) in parallel_dispatch(a2a, run_id, batch) {
            let agent = agent_of.get(&id).cloned();
            let started_at = claimed_at.get(&id).copied().unwrap_or_else(chrono::Utc::now);
            match res {
                Ok(reply) => {
                    let mut note = reply.replace(marker::DONE, "").trim().to_string();
                    if let Some(a) = &agent {
                        let got = collect_step_artifacts(home, a, &card_out_dir(run_id, &id), staging_dir);
                        if got > 0 {
                            note.push_str(&format!("\n[+{got} file(s) collected]"));
                        }
                    }
                    // GOAL MODE: judge the result; if it doesn't meet the goal and
                    // there's turn budget + goal rounds left, re-queue the card to
                    // Todo with the judge's feedback as a comment (the worker reads
                    // its comments on the next run) — a board-native acceptance loop.
                    let (is_goal, goal_max) = board
                        .card(&id)
                        .map(|c| (c.goal, if c.goal_max_turns == 0 { 5 } else { c.goal_max_turns }))
                        .unwrap_or((false, 5));
                    if is_goal {
                        let used = *goal_turns.get(&id).unwrap_or(&0);
                        if used < goal_max && budget.spend_turn().is_ok() {
                            let judge = if has_reviewer {
                                resolve_assignee(pool, registry, Some("reviewer"))
                            } else {
                                resolve_assignee(pool, registry, agent.as_deref())
                            };
                            let (title, detail) = board
                                .card(&id)
                                .map(|c| (c.title.clone(), c.detail.clone()))
                                .unwrap_or_default();
                            if let Ok(jw) = judge {
                                let (pass, feedback) = judge_card(a2a, run_id, &jw, &title, &detail, &note);
                                if !pass {
                                    goal_turns.insert(id.clone(), used + 1);
                                    board.comment(
                                        &id,
                                        "goal-judge",
                                        &format!("Not yet ({}/{goal_max}). {feedback}", used + 1),
                                    );
                                    if let Some(c) = board.card_mut(&id) {
                                        c.status = CardStatus::Todo;
                                        c.result = Some(note.clone());
                                    }
                                    board.record_run(&id, agent.clone(), "revise", &feedback, started_at);
                                    println!("  <- card {id} goal: revise ({}/{goal_max})", used + 1);
                                    log_event(home, &board_name, "goal_revise", Some(&id), &feedback);
                                    continue;
                                }
                            }
                        }
                    }
                    if let Some(c) = board.card_mut(&id) {
                        c.result = Some(note.clone());
                        c.status = CardStatus::Done;
                        c.block_kind = None;
                    }
                    board.record_run(&id, agent.clone(), "completed", &note, started_at);
                    println!("  <- card {id} done");
                    log_event(home, &board_name, "done", Some(&id), "");
                    // Optional host-side delivery of the result to a channel.
                    if let (Some(channel), Some(a)) =
                        (board.card(&id).and_then(|c| c.deliver.clone()), agent.as_deref())
                    {
                        deliver_card_result(home, a, &channel, &note);
                    }
                }
                Err(error) => {
                    // AUTO-RETRY: a failed card goes back to Todo until it has used
                    // up max_retries, then it's Blocked (gave_up).
                    let attempts = board.card(&id).map(|c| c.attempts).unwrap_or(0);
                    let max_retries = board.card(&id).map(|c| c.max_retries).unwrap_or(0);
                    if attempts <= max_retries {
                        if let Some(c) = board.card_mut(&id) {
                            c.status = CardStatus::Todo;
                            c.block_kind = Some("transient".to_string());
                            c.result = Some(format!("attempt {attempts} failed: {error:#}"));
                        }
                        board.record_run(&id, agent.clone(), "crashed", &format!("{error:#}"), started_at);
                        eprintln!("  <- card {id} failed (attempt {attempts}/{}), retrying", max_retries + 1);
                        log_event(home, &board_name, "retry", Some(&id), &format!("{error:#}"));
                    } else {
                        if let Some(c) = board.card_mut(&id) {
                            c.status = CardStatus::Blocked;
                            c.block_kind = Some("transient".to_string());
                            c.result = Some(format!("failed: {error:#}"));
                        }
                        board.record_run(&id, agent.clone(), "gave_up", &format!("{error:#}"), started_at);
                        eprintln!("  <- card {id} blocked: {error:#}");
                        log_event(home, &board_name, "blocked", Some(&id), &format!("{error:#}"));
                    }
                }
            }
        }
        board.save(home)?;
    }
    Ok(())
}

/// Goal-mode acceptance check: ask a judge worker whether `result` meets the
/// goal. Returns (pass, feedback). A judge dispatch error accepts (never blocks
/// a finished card on a judge hiccup).
fn judge_card(
    a2a: &A2aWire,
    run_id: &str,
    worker: &Worker,
    title: &str,
    detail: &str,
    result: &str,
) -> (bool, String) {
    let prompt = format!(
        "Acceptance check. The goal was:\n\n{title}\n{detail}\n\nThe worker produced this result:\n\n{result}\n\n\
         Reply with EXACTLY `PASS` on the first line if the result fully meets the goal. \
         Otherwise reply `REVISE` on the first line, then say specifically what to fix."
    );
    match a2a_send(a2a, &worker.agent_id, worker.model.as_deref(), run_id, &prompt) {
        Ok(reply) => {
            let t = reply.trim();
            if t.to_ascii_uppercase().starts_with("PASS") {
                (true, String::new())
            } else {
                let fb = t
                    .trim_start_matches(|c: char| c.is_alphabetic())
                    .trim_start_matches([':', '-', ' ', '\n'])
                    .trim()
                    .to_string();
                (false, if fb.is_empty() { "revise".to_string() } else { fb })
            }
        }
        Err(_) => (true, String::new()),
    }
}

fn card_out_dir(run_id: &str, card_id: &str) -> String {
    format!("{}-{}", remote_out_dir(run_id), card_id)
}

/// Stand up the A2A wire + role registry for a one-off board LLM action.
fn board_llm_setup(
    home: &MaturanaHome,
    agents: Option<String>,
) -> anyhow::Result<(A2aWire, RoleRegistry)> {
    let placement = PlacementChoice { roles_file: None, agents, base_spec: None };
    let registry = resolve_registry(home, &placement)?;
    let token = std::fs::read_to_string(home.root().join("sessiond/token"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        anyhow::bail!(
            "the A2A wire needs the sessiond token at {}/sessiond/token",
            home.root().display()
        );
    }
    let a2a = A2aWire { base: crate::a2a::start_local_a2a_server(home, &token)?, token };
    Ok((a2a, registry))
}

/// Decompose a card into child cards via the coordinator agent (LLM). Reuses the
/// orchestrator's plan format + parser; each step becomes a card (deps wired).
fn decompose_card(
    home: &MaturanaHome,
    board_name: &str,
    card_id: &str,
    agents: Option<String>,
) -> anyhow::Result<()> {
    use maturana_core::board::{Board, CardStatus};
    let mut board = Board::load(home, board_name)?;
    let card = board.card(card_id).ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?.clone();
    let (a2a, registry) = board_llm_setup(home, agents)?;
    let run_id = format!("board-decompose-{}", chrono::Utc::now().timestamp());
    let mut pool = WorkerPool::new(home, &registry, run_id.clone(), String::new(), 1);
    let worker = if registry.get("coordinator").is_some() {
        pool.resolve("coordinator")?
    } else {
        resolve_assignee(&mut pool, &registry, None)?
    };
    let prompt = format!(
        "Break this task into a small list (2-6) of concrete subtasks for a team of agents.\n\nTask:\n{}\n{}\n\n\
         Reply ONLY with JSON: {{\"steps\": [{{\"id\": \"s1\", \"role\": \"developer\", \"task\": \"...\", \"deps\": []}}]}}. \
         role is one of developer|researcher|reviewer|synthesizer; deps reference earlier step ids. No prose.",
        card.title, card.detail
    );
    let reply = a2a_send(&a2a, &worker.agent_id, worker.model.as_deref(), &run_id, &prompt);
    pool.teardown();
    let reply = reply?;
    let plan = parse_plan(&card.title, &reply, &registry)
        .map_err(|e| anyhow::anyhow!("decompose failed: {e}\n{reply}"))?;
    if plan.steps.is_empty() {
        anyhow::bail!("decompose produced no subtasks");
    }
    let mut step_to_card: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut new_ids = Vec::new();
    for step in &plan.steps {
        let deps: Vec<String> = step.deps.iter().filter_map(|d| step_to_card.get(d).cloned()).collect();
        let id = board.add(&step.task, "", Some(step.role.clone()), deps);
        step_to_card.insert(step.id.clone(), id.clone());
        new_ids.push(id);
    }
    if let Some(c) = board.card_mut(card_id) {
        c.status = CardStatus::Done;
        c.result = Some(format!("decomposed into {}", new_ids.join(", ")));
    }
    board.validate().map_err(|e| anyhow::anyhow!(e))?;
    board.save(home)?;
    maturana_core::board::log_event(
        home,
        board_name,
        "decompose",
        Some(card_id),
        &format!("-> {}", new_ids.join(", ")),
    );
    println!("decomposed {card_id} into {}", new_ids.join(", "));
    Ok(())
}

/// Flesh out a terse card into a clear title + detail via an agent (LLM).
fn specify_card(
    home: &MaturanaHome,
    board_name: &str,
    card_id: &str,
    agents: Option<String>,
) -> anyhow::Result<()> {
    use maturana_core::board::{Board, CardStatus};
    let mut board = Board::load(home, board_name)?;
    let card = board.card(card_id).ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?.clone();
    let (a2a, registry) = board_llm_setup(home, agents)?;
    let run_id = format!("board-specify-{}", chrono::Utc::now().timestamp());
    let mut pool = WorkerPool::new(home, &registry, run_id.clone(), String::new(), 1);
    let worker = resolve_assignee(&mut pool, &registry, card.assignee.as_deref().or(Some("developer")))?;
    let prompt = format!(
        "Rewrite this rough task into a clear, actionable spec.\n\nTask:\n{}\n{}\n\n\
         Reply ONLY with JSON: {{\"title\": \"concise imperative title\", \"detail\": \"what to do, plus acceptance criteria\"}}. No prose.",
        card.title, card.detail
    );
    let reply = a2a_send(&a2a, &worker.agent_id, worker.model.as_deref(), &run_id, &prompt);
    pool.teardown();
    let reply = reply?;
    let json = extract_json_object(&reply)
        .ok_or_else(|| anyhow::anyhow!("specify reply had no JSON:\n{reply}"))?;
    #[derive(serde::Deserialize)]
    struct Spec {
        #[serde(default)]
        title: String,
        #[serde(default)]
        detail: String,
    }
    let spec: Spec = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("bad specify JSON: {e}\n{json}"))?;
    if let Some(c) = board.card_mut(card_id) {
        if !spec.title.trim().is_empty() {
            c.title = spec.title.trim().to_string();
        }
        if !spec.detail.trim().is_empty() {
            c.detail = spec.detail.trim().to_string();
        }
        if c.status == CardStatus::Triage {
            c.status = CardStatus::Todo;
        }
    }
    board.save(home)?;
    println!("specified {card_id}");
    Ok(())
}

/// Resolve a card's assignee to a worker: a known role goes through the pool
/// (reuse or spawn its placement); anything else is treated as a concrete agent
/// id and reused directly; an unassigned card defaults to the `developer` role.
fn resolve_assignee(
    pool: &mut WorkerPool,
    registry: &RoleRegistry,
    assignee: Option<&str>,
) -> anyhow::Result<Worker> {
    match assignee {
        Some(a) if registry.get(a).is_some() => pool.resolve(a),
        Some(a) => Ok(Worker {
            agent_id: a.to_string(),
            model: None,
        }),
        None => pool.resolve("developer"),
    }
}

fn build_card_task(
    registry: &RoleRegistry,
    board: &maturana_core::board::Board,
    card: &maturana_core::board::Card,
    out_remote: &str,
) -> String {
    let mut body = format!("Task: {}\n", card.title);
    if !card.detail.is_empty() {
        body.push_str(&format!("\n{}\n", card.detail));
    }
    if card.deliver.is_some() {
        body.push_str(
            "\n[Delivery is handled for you: produce the finished result as your reply and the host will deliver it to the requested channel. Do NOT attempt to send it yourself.]\n",
        );
    }
    let ctx = board.dependency_context(card);
    if !ctx.trim().is_empty() {
        body.push_str(&format!("\n--- INPUTS FROM EARLIER CARDS ---{ctx}\n"));
    }
    // Card notes (comments) — including any goal-mode revise feedback — so the
    // worker sees the running thread on this card.
    if !card.comments.is_empty() {
        body.push_str("\n--- NOTES ON THIS CARD ---\n");
        for cm in &card.comments {
            let who = if cm.author.is_empty() { "note" } else { &cm.author };
            body.push_str(&format!("[{who}] {}\n", cm.body));
        }
    }
    // Attachments — inline small text files so the worker can read them; larger
    // or binary ones are named with their host path.
    if !card.attachments.is_empty() {
        body.push_str("\n--- ATTACHMENTS ---\n");
        for path in &card.attachments {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            match std::fs::metadata(path) {
                Ok(meta) if meta.len() <= 64 * 1024 => match std::fs::read_to_string(path) {
                    Ok(text) => body.push_str(&format!("\n## {name}\n{text}\n")),
                    Err(_) => body.push_str(&format!("- {name} (binary, at {path})\n")),
                },
                _ => body.push_str(&format!("- {name} (at {path})\n")),
            }
        }
    }
    body.push_str(&format!(
        "\n--- PRODUCING FILES ---\nIf this task creates any files (code, a page, a script, data), \
         WRITE them into the directory {out_remote}/ (create it). Keep your reply a brief summary; \
         do NOT paste full file contents.\n"
    ));
    match card.assignee.as_deref() {
        Some(a) if registry.get(a).is_some() => registry.frame_task(a, &body).unwrap_or(body),
        Some(_) => body,
        None => registry.frame_task("developer", &body).unwrap_or(body),
    }
}

/// Dispatch a batch of cards over A2A concurrently (one thread each); returns
/// (card_id, reply-or-error) per card. The A2A server is thread-per-connection,
/// so simultaneous message/send calls are safe.
fn parallel_dispatch(
    a2a: &A2aWire,
    run_id: &str,
    batch: Vec<(String, Worker, String)>,
) -> Vec<(String, anyhow::Result<String>)> {
    if batch.len() == 1 {
        let (id, worker, task) = &batch[0];
        let res = a2a_send(a2a, &worker.agent_id, worker.model.as_deref(), run_id, task);
        return vec![(id.clone(), res)];
    }
    let mut handles: Vec<(String, std::thread::JoinHandle<anyhow::Result<String>>)> = Vec::new();
    for (id, worker, task) in batch {
        let a2a = a2a.clone();
        let run_id = run_id.to_string();
        let agent = worker.agent_id.clone();
        let model = worker.model.clone();
        let handle = std::thread::spawn(move || a2a_send(&a2a, &agent, model.as_deref(), &run_id, &task));
        handles.push((id, handle));
    }
    handles
        .into_iter()
        .map(|(id, h)| {
            (
                id,
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("worker thread panicked"))),
            )
        })
        .collect()
}

#[cfg(test)]
mod board_tests {
    use super::*;
    use maturana_core::board::Board;
    use maturana_core::roles::RoleRegistry;

    #[test]
    fn card_out_dir_is_per_card() {
        assert_eq!(card_out_dir("board-x-1", "c3"), "/workspace/maturana-out-board-x-1-c3");
    }

    #[test]
    fn build_card_task_frames_a_role_and_adds_the_file_instruction() {
        let reg = RoleRegistry::reuse_across(&["codex-firecracker".to_string()]);
        let mut b = Board::new("t");
        b.add("Build the page", "two players", Some("developer".into()), vec![]);
        let card = b.card("c1").unwrap().clone();
        let task = build_card_task(&reg, &b, &card, "/workspace/out-c1");
        assert!(task.contains("Build the page"));
        assert!(task.contains("/workspace/out-c1"));
        assert!(task.contains("DEVELOPER"));
    }

    #[test]
    fn build_card_task_for_a_concrete_agent_has_no_role_prefix() {
        let reg = RoleRegistry::reuse_across(&["codex-firecracker".to_string()]);
        let mut b = Board::new("t");
        b.add("Do it", "", Some("claude-firecracker".into()), vec![]);
        let card = b.card("c1").unwrap().clone();
        let task = build_card_task(&reg, &b, &card, "/workspace/out");
        assert!(task.starts_with("Task: Do it"));
    }
}
