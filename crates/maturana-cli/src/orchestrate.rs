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

use maturana_core::orchestrator_budget::{BudgetExhausted, CapsOverride, OrchestratorCaps, RunBudget, SlotCounter};
use maturana_core::roles::{marker, RoleRegistry, RolePlacement};
use maturana_core::state::MaturanaHome;

/// How long to wait for a single worker step before giving up on it (seconds).
/// Set above the in-guest harness timeout so a slow-but-alive turn is not failed
/// prematurely.
const STEP_TIMEOUT_SECONDS: u64 = 300;
/// How long to wait for the coordinator's plan / the synthesizer's answer.
const COORDINATOR_TIMEOUT_SECONDS: u64 = 300;

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
struct Worker {
    agent_id: String,
    session_id: String,
    model: Option<String>,
}

/// Resolve a role to a concrete worker. `reuse` placement uses a standing agent
/// directly. `spawn` placement is the default and brings up a dedicated VM — but
/// that path is wired in a later step; until then it returns a clear error so a
/// user can map the role to a standing agent via roles.toml in the meantime.
fn resolve_worker(home: &MaturanaHome, registry: &RoleRegistry, role_name: &str) -> anyhow::Result<Worker> {
    let role = registry
        .get(role_name)
        .ok_or_else(|| anyhow::anyhow!("unknown role '{role_name}'"))?;
    match &role.placement {
        RolePlacement::Reuse { agent_id } => {
            let session_id = crate::infer_agent_session_id(home, agent_id)?;
            Ok(Worker {
                agent_id: agent_id.clone(),
                session_id,
                model: role.model.clone(),
            })
        }
        RolePlacement::Spawn { base_spec } => anyhow::bail!(
            "role '{role_name}' uses spawn placement (base spec '{base_spec}'); on-demand VM \
             spawning is not wired yet — for now map this role to a standing agent in roles.toml: \
             [roles.{role_name}.placement]\\nreuse = {{ agent_id = \"<agent>\" }}"
        ),
    }
}

// ===== Dispatch helpers =====

/// Send one task to a worker and block (polling) until its reply or a timeout.
/// Charges exactly one turn against the budget BEFORE sending, so an in-flight
/// turn is always already paid for. Used for the coordinator and synthesizer,
/// and inside the synchronous review of a single step.
fn dispatch_and_wait(
    home: &MaturanaHome,
    worker: &Worker,
    run_id: &str,
    task: &str,
    budget: &mut RunBudget,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    budget
        .spend_turn()
        .map_err(|_| anyhow::anyhow!("turn budget exhausted"))?;
    let handle = crate::channels::enqueue_dispatch_turn(
        home,
        &worker.agent_id,
        &worker.session_id,
        run_id,
        task,
        worker.model.as_deref(),
    )?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        if is_aborted(home, run_id) {
            anyhow::bail!("run aborted");
        }
        if let Some(reply) = crate::channels::try_collect_dispatch(home, &worker.agent_id, &handle)? {
            return Ok(reply);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    anyhow::bail!("timed out waiting for {} after {timeout_seconds}s", worker.agent_id)
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
    let mut budget = RunBudget::new(caps.clone());
    let started = Instant::now();
    let wall = Duration::from_secs(caps.max_wall_seconds);

    println!("orchestrator: run {run_id}");
    println!("  goal: {goal}");
    println!(
        "  caps: {} turns / {}s wall / {} parallel / {} VMs",
        caps.max_total_turns, caps.max_wall_seconds, caps.max_parallel, caps.max_concurrent_vms
    );

    // --- Plan: ask the coordinator to break the goal into steps ---
    let coordinator = resolve_worker(home, &registry, "coordinator")?;
    let plan_reply = dispatch_and_wait(
        home,
        &coordinator,
        &run_id,
        &coordinator_task(goal, &registry, &caps),
        &mut budget,
        COORDINATOR_TIMEOUT_SECONDS,
    )?;
    let mut plan =
        parse_plan(goal, &plan_reply, &registry).map_err(|e| anyhow::anyhow!("planning failed: {e}"))?;
    if !budget.admits_plan(plan.steps.len() as u32) {
        anyhow::bail!(
            "the {}-step plan could exceed the {} remaining turn budget; simplify the goal or raise --max-turns",
            plan.steps.len(),
            budget.turns_remaining()
        );
    }
    save_plan(home, &run_id, &plan)?;
    println!("  plan: {} steps", plan.steps.len());

    // --- Execute: run ready steps across workers until done or a limit stops us ---
    let mut in_flight: Vec<(String, Worker, crate::channels::DispatchHandle)> = Vec::new();
    let mut slots = SlotCounter::new(caps.max_parallel);
    let mut stop_reason = "completed";

    loop {
        // Liveness backstop first, then wall-clock, then abort — all independent
        // of whether any progress happened.
        if budget.tick().is_err() {
            stop_reason = "tick ceiling reached";
            break;
        }
        if started.elapsed() >= wall {
            stop_reason = "wall-clock budget reached";
            break;
        }
        if is_aborted(home, &run_id) {
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

        // Dispatch ready steps to free workers (one in-flight step per agent, so
        // a worker's single-flight VM is never double-booked).
        let ready_ids: Vec<String> = plan.ready_steps().iter().map(|s| s.id.clone()).collect();
        for sid in ready_ids {
            if slots.available() == 0 {
                break;
            }
            let step = plan.steps.iter().find(|s| s.id == sid).unwrap().clone();
            let worker = match resolve_worker(home, &registry, &step.role) {
                Ok(w) => w,
                Err(error) => {
                    eprintln!("orchestrator: step {sid} role '{}' unresolved: {error:#}", step.role);
                    if let Some(s) = plan.step_mut(&sid) {
                        s.status = StepStatus::Failed;
                    }
                    continue;
                }
            };
            if in_flight.iter().any(|(_, w, _)| w.agent_id == worker.agent_id) {
                continue; // that agent is busy with another step this tick
            }
            if budget.spend_turn().is_err() {
                stop_reason = "turn budget exhausted";
                break;
            }
            let framed = build_step_task(&registry, &plan, &step);
            let handle = crate::channels::enqueue_dispatch_turn(
                home,
                &worker.agent_id,
                &worker.session_id,
                &run_id,
                &framed,
                worker.model.as_deref(),
            )?;
            slots.try_acquire();
            if let Some(s) = plan.step_mut(&sid) {
                s.status = StepStatus::Running;
                s.attempts += 1;
            }
            println!("  -> step {sid} ({}) sent to {}", step.role, worker.agent_id);
            in_flight.push((sid, worker, handle));
        }

        // Poll every in-flight step once (non-blocking), process replies.
        let mut still = Vec::new();
        for (sid, worker, handle) in std::mem::take(&mut in_flight) {
            match crate::channels::try_collect_dispatch(home, &worker.agent_id, &handle)? {
                Some(reply) => {
                    slots.release();
                    let result = finish_step(home, &registry, &run_id, &mut plan, &sid, &worker, reply, &mut budget)?;
                    if let Some(s) = plan.step_mut(&sid) {
                        s.result = Some(result);
                        s.status = StepStatus::Done;
                    }
                    println!("  <- step {sid} done");
                }
                None => still.push((sid, worker, handle)),
            }
        }
        in_flight = still;
        save_plan(home, &run_id, &plan)?;
        std::thread::sleep(Duration::from_secs(2));
    }

    save_plan(home, &run_id, &plan)?;

    if !plan.is_complete() {
        anyhow::bail!("orchestrator run {run_id} stopped before completion: {stop_reason}");
    }

    // --- Synthesize: combine the step results into the final answer ---
    let synthesizer = resolve_worker(home, &registry, "synthesizer")?;
    let mut summary = format!("Goal:\n{goal}\n\nCompleted step results:\n");
    for step in &plan.steps {
        if let Some(result) = &step.result {
            summary.push_str(&format!("\n## {} ({})\n{}\n", step.id, step.role, result));
        }
    }
    let synth_task = registry
        .frame_task("synthesizer", &summary)
        .unwrap_or(summary);
    let answer = dispatch_and_wait(home, &synthesizer, &run_id, &synth_task, &mut budget, COORDINATOR_TIMEOUT_SECONDS)?;
    let answer = answer.replace(marker::DONE, "").trim().to_string();
    std::fs::write(run_dir(home, &run_id).join("answer.md"), &answer)?;
    println!("\n=== orchestrator run {run_id}: final answer ===\n{answer}");
    Ok(())
}

/// Complete one step, running the bounded reviewer loop synchronously if the step
/// asked for review. Returns the accepted result text. Each reviewer turn and
/// each revise turn charges the budget, so review ping-pong has a hard ceiling.
fn finish_step(
    home: &MaturanaHome,
    registry: &RoleRegistry,
    run_id: &str,
    plan: &mut Plan,
    sid: &str,
    worker: &Worker,
    worker_reply: String,
    budget: &mut RunBudget,
) -> anyhow::Result<String> {
    let step = plan.steps.iter().find(|s| s.id == sid).unwrap().clone();
    if !step.review {
        return Ok(worker_reply);
    }
    let max_cycles = budget.caps().max_review_cycles;
    let mut current = worker_reply;
    let mut cycles = 0u32;
    while cycles < max_cycles {
        let reviewer = match resolve_worker(home, registry, "reviewer") {
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
        let verdict = match dispatch_and_wait(home, &reviewer, run_id, &review_task, budget, STEP_TIMEOUT_SECONDS) {
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
                current = dispatch_and_wait(home, worker, run_id, &revise_task, budget, STEP_TIMEOUT_SECONDS)?;
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
