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
    Loop {
        /// The goal, in plain English.
        goal: String,
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
        /// Optional roles.toml overriding the default role set.
        #[arg(long)]
        roles_file: Option<PathBuf>,
        /// Spec template new role VMs are spawned from when a role's placement
        /// is `spawn` (the default). Reuse-placement roles ignore this.
        #[arg(long, default_value = "worker-base")]
        base_spec: String,
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

/// A resolved place to run a role's work: a concrete agent + its session.
#[derive(Clone)]
struct Worker {
    agent_id: String,
    session_id: String,
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
            RolePlacement::Reuse { agent_id } => {
                let session_id = crate::infer_agent_session_id(self.home, &agent_id)?;
                Worker { agent_id, session_id, model }
            }
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
                Worker { agent_id: new_id, session_id, model }
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
            run_id,
            max_turns,
            max_wall_seconds,
            max_parallel,
            max_vms,
            roles_file,
            base_spec,
        } => {
            let overrides = CapsOverride {
                max_total_turns: max_turns,
                max_wall_seconds,
                max_parallel,
                max_concurrent_vms: max_vms,
                max_steps: None,
            };
            run_loop(home, &goal, run_id, overrides, roles_file, &base_spec)
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

fn run_loop(
    home: &MaturanaHome,
    goal: &str,
    run_id: Option<String>,
    overrides: CapsOverride,
    roles_file: Option<PathBuf>,
    base_spec: &str,
) -> anyhow::Result<()> {
    let caps = OrchestratorCaps::default().tighten_with(&overrides);
    let registry = match &roles_file {
        Some(path) => RoleRegistry::load_or_default(path, base_spec)?,
        None => RoleRegistry::defaults(base_spec),
    };
    let run_id = run_id.unwrap_or_else(|| format!("run-{}", chrono::Utc::now().timestamp()));
    std::fs::create_dir_all(run_dir(home, &run_id))?;
    println!("orchestrator: run {run_id}");
    println!("  goal: {goal}");
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
        base_spec.to_string(),
        caps.max_concurrent_vms,
    );
    let result = run_inner(home, goal, &run_id, &registry, &caps, &mut pool, &a2a);
    pool.teardown();
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
) -> anyhow::Result<()> {
    let mut budget = RunBudget::new(caps.clone());
    let started = Instant::now();
    let wall = Duration::from_secs(caps.max_wall_seconds);

    // --- Plan: ask the coordinator to break the goal into steps ---
    let coordinator = pool.resolve("coordinator")?;
    let plan_reply = dispatch_and_wait(
        home,
        a2a,
        &coordinator,
        run_id,
        &coordinator_task(goal, registry, caps),
        &mut budget,
    )?;
    let mut plan =
        parse_plan(goal, &plan_reply, registry).map_err(|e| anyhow::anyhow!("planning failed: {e}"))?;
    if !budget.admits_plan(plan.steps.len() as u32) {
        anyhow::bail!(
            "the {}-step plan could exceed the {} remaining turn budget; simplify the goal or raise --max-turns",
            plan.steps.len(),
            budget.turns_remaining()
        );
    }
    save_plan(home, run_id, &plan)?;
    println!("  plan: {} steps", plan.steps.len());

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
            let framed = build_step_task(registry, &plan, &step);
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
                    println!("  <- step {sid} done");
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

    // --- Synthesize: combine the step results into the final answer ---
    let synthesizer = pool.resolve("synthesizer")?;
    let mut summary = format!("Goal:\n{goal}\n\nCompleted step results:\n");
    for step in &plan.steps {
        if let Some(result) = &step.result {
            summary.push_str(&format!("\n## {} ({})\n{}\n", step.id, step.role, result));
        }
    }
    let synth_task = registry
        .frame_task("synthesizer", &summary)
        .unwrap_or(summary);
    let answer = dispatch_and_wait(home, a2a, &synthesizer, run_id, &synth_task, &mut budget)?;
    let answer = answer.replace(marker::DONE, "").trim().to_string();
    std::fs::write(run_dir(home, run_id).join("answer.md"), &answer)?;
    println!("\n=== orchestrator run {run_id}: final answer ===\n{answer}");
    Ok(())
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
