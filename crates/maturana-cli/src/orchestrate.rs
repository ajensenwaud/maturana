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

use maturana_core::orchestrator_budget::{CapsOverride, OrchestratorCaps, RunBudget};
use maturana_core::roles::{marker, RoleRegistry};
use maturana_core::state::MaturanaHome;
use maturana_ops::agents::{infer_agent_session_id, list_agent_ids};
use maturana_ops::artifacts::{
    collect_step_artifacts, copy_tree, count_files, out_basename, output_dir_for, remote_out_dir,
};
use maturana_ops::boards::{
    apply_card_specification, apply_decomposition, build_card_task, build_decompose_task,
    build_goal_judge_task, build_specify_task, card_out_dir, parse_card_specification,
    parse_goal_judge_reply,
};
use maturana_ops::conversation::{post_outbox_files, post_outbox_text, OutboxTarget};
use maturana_ops::deliverables::{write_deliverable, Deliverable};
use maturana_ops::orchestration::{is_aborted, run_dir, save_plan};
use maturana_ops::placement::{resolve_role_registry, PlacementChoice, Worker, WorkerPool};
#[cfg(test)]
use maturana_ops::planner::Step;
use maturana_ops::planner::{
    build_step_task, coordinator_retry_task, coordinator_task, parse_plan, parse_review,
    plan_chat_summary, reply_preview, Plan, ReviewVerdict, StepStatus,
};
use maturana_ops::verification::{
    build_run_summary, parse_verify, verify_detail, verify_task, VerifyOutcome,
};
#[cfg(test)]
use maturana_ops::{artifacts::safe_relative_path, deliverables::extract_file_manifest};

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
    if is_aborted(home, run_id)? {
        anyhow::bail!("run aborted");
    }
    a2a_send(a2a, &worker.agent_id, worker.model.as_deref(), run_id, task)
}

// ===== The loop =====

pub fn handle_orchestrator(
    command: OrchestratorCommand,
    home: &MaturanaHome,
) -> anyhow::Result<()> {
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
            run_loop(
                home,
                &goal,
                run_id,
                overrides,
                placement,
                output,
                !no_verify,
                chat.resolve(),
            )
        }
        OrchestratorSubcommand::Status { run_id } => status(home, &run_id),
        OrchestratorSubcommand::Abort { run_id } => {
            maturana_ops::orchestration::request_abort(home, &run_id)?;
            println!(
                "orchestrator: abort requested for {run_id} (in-flight steps finish their lease)"
            );
            Ok(())
        }
    }
}

fn status(home: &MaturanaHome, run_id: &str) -> anyhow::Result<()> {
    match maturana_ops::orchestration::orchestration_run_status_lines(home, run_id)? {
        Some(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        None => println!("orchestrator: no run '{run_id}'"),
    }
    Ok(())
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

impl ChatTargetArgs {
    /// A target only when the channel actually addressed one (all required fields
    /// present together); otherwise this is a plain CLI run that just prints.
    fn resolve(self) -> Option<OutboxTarget> {
        Some(OutboxTarget {
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
fn post_chat(home: &MaturanaHome, chat: Option<&OutboxTarget>, text: &str) {
    let _ = post_outbox_text(home, chat, text);
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
            let mut ids = list_agent_ids(home).unwrap_or_default();
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
    let session_of =
        |a: &str| infer_agent_session_id(home, a).unwrap_or_else(|_| format!("{a}-main"));
    match channel.as_str() {
        "telegram" => {
            for a in &candidates {
                if require_live && !crate::channels::telegram_bridge_live(home, a) {
                    continue;
                }
                if let Some(chat_id) =
                    maturana_ops::conversation::current_paired_telegram_chat_id(home, a)
                {
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
            let chat = OutboxTarget {
                channel: target.channel.clone(),
                platform_id: target.platform_id.clone(),
                thread_id: None,
                agent_id: target.agent_id.clone(),
                session_id: target.session_id.clone(),
            };
            // Honest reporting: only claim the result reached the bridge if the
            // outbox write actually succeeded. The bridge then performs the send.
            match post_outbox_text(home, Some(&chat), text) {
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
fn post_chat_files(home: &MaturanaHome, chat: Option<&OutboxTarget>, text: &str, files: &[String]) {
    let _ = post_outbox_files(home, chat, text, files);
}

fn run_loop(
    home: &MaturanaHome,
    goal: &str,
    run_id: Option<String>,
    overrides: CapsOverride,
    placement: PlacementChoice,
    output: Option<PathBuf>,
    verify: bool,
    chat: Option<OutboxTarget>,
) -> anyhow::Result<()> {
    let caps = OrchestratorCaps::default().tighten_with(&overrides);
    let placement = resolve_role_registry(home, &placement)?;
    if let Some(line) = &placement.status_line {
        println!("  {line}");
    }
    let registry = placement.registry;
    // Only spawn-placement roles consume this; reuse runs never touch it.
    let base_spec = placement.base_spec;
    let run_id = run_id.unwrap_or_else(|| format!("run-{}", chrono::Utc::now().timestamp()));
    std::fs::create_dir_all(run_dir(home, &run_id)?)?;
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
        home,
        goal,
        &run_id,
        &registry,
        &caps,
        &mut pool,
        &a2a,
        output.as_deref(),
        verify,
        chat.as_ref(),
    );
    pool.teardown();
    if let Err(error) = &result {
        post_chat(
            home,
            chat.as_ref(),
            &format!("⚠️ Loop `{run_id}` stopped: {error:#}"),
        );
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
    chat: Option<&OutboxTarget>,
) -> anyhow::Result<()> {
    let mut budget = RunBudget::new(caps.clone());
    let started = Instant::now();
    let wall = Duration::from_secs(caps.max_wall_seconds);

    // Where workers write real files, and where we stage what we fetch back. The
    // deliverable is the actual bytes a worker produced in its VM — copied out
    // over scp — not a final agent's retyping of them from a text summary.
    let out_remote = remote_out_dir(run_id);
    let staging_dir = run_dir(home, run_id)?.join("staging");
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
            post_chat(
                home,
                chat,
                "📋 The first plan wasn't usable — re-asking the coordinator…",
            );
            if budget.turns_remaining() > 0 && !is_aborted(home, run_id)? {
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
        if is_aborted(home, run_id)? {
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
                    eprintln!(
                        "orchestrator: step {sid} role '{}' unresolved: {error:#}",
                        step.role
                    );
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
            println!(
                "  -> step {sid} ({}) -> {} via A2A",
                step.role, worker.agent_id
            );
            match dispatch_and_wait(home, a2a, &worker, run_id, &framed, &mut budget) {
                Ok(reply) => {
                    let result = finish_step(
                        home,
                        a2a,
                        registry,
                        run_id,
                        &mut plan,
                        &sid,
                        &worker,
                        reply,
                        &mut budget,
                        pool,
                    )?;
                    if let Some(s) = plan.step_mut(&sid) {
                        s.result = Some(result);
                        s.status = StepStatus::Done;
                    }
                    // Pull the real files this worker wrote out of its VM (best-effort).
                    let got =
                        collect_step_artifacts(home, &worker.agent_id, &out_remote, &staging_dir);
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
                    let done = plan
                        .steps
                        .iter()
                        .filter(|s| s.status == StepStatus::Done)
                        .count();
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

        let dir = output_dir_for(home, run_id, output)?;
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
            post_chat(
                home,
                chat,
                &format!("✅ Done — Loop `{run_id}`\n\n{answer}"),
            );
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

// ===== Verification: actually run the deliverable before calling it done =====

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
        let reply = match dispatch_and_wait(
            home,
            a2a,
            &worker,
            run_id,
            &verify_task(goal, out_remote),
            budget,
        ) {
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
mod tests;

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
            let c = b
                .card(&card)
                .ok_or_else(|| anyhow::anyhow!("no card '{card}'"))?;
            println!("{} [{}] {}", c.id, c.status.label(), c.title);
            if !c.detail.is_empty() {
                println!("\n{}", c.detail);
            }
            println!(
                "\nassignee: {}",
                c.assignee.as_deref().unwrap_or("(default)")
            );
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
                    println!(
                        "  #{} {} ({})",
                        r.attempt,
                        r.outcome,
                        r.agent.as_deref().unwrap_or("?")
                    );
                }
            }
            Ok(())
        }
        BoardSubcommand::Move {
            card,
            status,
            board,
        } => {
            let st = CardStatus::parse(&status).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown status '{status}' (triage|todo|doing|done|blocked|archived)"
                )
            })?;
            let mut b = Board::load(home, &board)?;
            b.card_mut(&card)
                .ok_or_else(|| anyhow::anyhow!("no card '{card}' on board {board}"))?
                .status = st;
            b.save(home)?;
            println!("{card} -> {}", st.label());
            Ok(())
        }
        BoardSubcommand::Comment {
            card,
            text,
            author,
            board,
        } => {
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
        BoardSubcommand::Decompose {
            card,
            board,
            agents,
        } => decompose_card(home, &board, &card, agents),
        BoardSubcommand::Specify {
            card,
            board,
            agents,
        } => specify_card(home, &board, &card, agents),
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
        log_event(
            home,
            board_name,
            "reclaim",
            None,
            &format!("{reclaimed} card(s) reset doing->todo"),
        );
        board.save(home)?;
    }

    let caps = OrchestratorCaps::default().tighten_with(&overrides);
    let placement = resolve_role_registry(home, &placement)?;
    if let Some(line) = &placement.status_line {
        println!("  {line}");
    }
    let registry = placement.registry;
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

    let base_spec = placement.base_spec;
    let mut pool = WorkerPool::new(
        home,
        &registry,
        run_id.clone(),
        base_spec,
        caps.max_concurrent_vms,
    );
    let staging_dir = run_dir(home, &run_id)?.join("staging");
    std::fs::create_dir_all(run_dir(home, &run_id)?)?;

    println!("board {board_name}: running ({} cards)", board.cards.len());
    println!(
        "  caps: {} turns / {}s wall / {} parallel",
        caps.max_total_turns, caps.max_wall_seconds, caps.max_parallel
    );
    log_event(
        home,
        board_name,
        "run_start",
        None,
        &format!("run {run_id} ({} cards)", board.cards.len()),
    );

    let result = run_board_inner(
        home,
        &mut board,
        &registry,
        &caps,
        &mut pool,
        &a2a,
        &run_id,
        &staging_dir,
    );
    pool.teardown();
    let _ = board.save(home);

    if staging_dir.exists() && count_files(&staging_dir) > 0 {
        let dir = output_dir_for(home, &run_id, output)?;
        std::fs::create_dir_all(&dir)?;
        let names = copy_tree(&staging_dir, &dir)?;
        println!(
            "\nboard {board_name}: wrote {} file(s) to {}",
            names.len(),
            dir.display()
        );
    }
    let (_, _, done, blocked) = board.counts();
    log_event(
        home,
        board_name,
        "run_end",
        None,
        &format!("{done} done, {blocked} blocked"),
    );
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

        let ready: Vec<maturana_core::board::Card> = board.ready().into_iter().cloned().collect();
        if ready.is_empty() {
            if board.cards.iter().any(|c| c.status == CardStatus::Todo) {
                println!("  [stuck: remaining cards are blocked by a failed dependency]");
            }
            break;
        }

        // Build a batch up to max_parallel, spending a turn per card up front so
        // the budget stays single-threaded; the A2A I/O then fans out.
        let mut batch: Vec<(String, Worker, String)> = Vec::new();
        let mut agent_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut claimed_at: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> =
            std::collections::HashMap::new();
        for card in ready.iter().take(caps.max_parallel.max(1) as usize) {
            if budget.spend_turn().is_err() {
                break;
            }
            let worker = pool.resolve_assignee(card.assignee.as_deref())?;
            let task = build_card_task(registry, board, card, &card_out_dir(run_id, &card.id));
            agent_of.insert(card.id.clone(), worker.agent_id.clone());
            claimed_at.insert(card.id.clone(), chrono::Utc::now());
            if let Some(c) = board.card_mut(&card.id) {
                c.status = CardStatus::Doing;
                c.attempts += 1;
            }
            println!(
                "  -> card {} ({}) -> {}",
                card.id, card.title, worker.agent_id
            );
            log_event(
                home,
                &board_name,
                "claim",
                Some(&card.id),
                &format!("{} -> {}", card.title, worker.agent_id),
            );
            batch.push((card.id.clone(), worker, task));
        }
        if batch.is_empty() {
            println!("  [turn budget exhausted]");
            break;
        }
        board.save(home)?;

        for (id, res) in parallel_dispatch(a2a, run_id, batch) {
            let agent = agent_of.get(&id).cloned();
            let started_at = claimed_at
                .get(&id)
                .copied()
                .unwrap_or_else(chrono::Utc::now);
            match res {
                Ok(reply) => {
                    let mut note = reply.replace(marker::DONE, "").trim().to_string();
                    if let Some(a) = &agent {
                        let got = collect_step_artifacts(
                            home,
                            a,
                            &card_out_dir(run_id, &id),
                            staging_dir,
                        );
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
                        .map(|c| {
                            (
                                c.goal,
                                if c.goal_max_turns == 0 {
                                    5
                                } else {
                                    c.goal_max_turns
                                },
                            )
                        })
                        .unwrap_or((false, 5));
                    if is_goal {
                        let used = *goal_turns.get(&id).unwrap_or(&0);
                        if used < goal_max && budget.spend_turn().is_ok() {
                            let judge = if has_reviewer {
                                pool.resolve_assignee(Some("reviewer"))
                            } else {
                                pool.resolve_assignee(agent.as_deref())
                            };
                            let (title, detail) = board
                                .card(&id)
                                .map(|c| (c.title.clone(), c.detail.clone()))
                                .unwrap_or_default();
                            if let Ok(jw) = judge {
                                let (pass, feedback) =
                                    judge_card(a2a, run_id, &jw, &title, &detail, &note);
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
                                    board.record_run(
                                        &id,
                                        agent.clone(),
                                        "revise",
                                        &feedback,
                                        started_at,
                                    );
                                    println!(
                                        "  <- card {id} goal: revise ({}/{goal_max})",
                                        used + 1
                                    );
                                    log_event(
                                        home,
                                        &board_name,
                                        "goal_revise",
                                        Some(&id),
                                        &feedback,
                                    );
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
                    if let (Some(channel), Some(a)) = (
                        board.card(&id).and_then(|c| c.deliver.clone()),
                        agent.as_deref(),
                    ) {
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
                        board.record_run(
                            &id,
                            agent.clone(),
                            "crashed",
                            &format!("{error:#}"),
                            started_at,
                        );
                        eprintln!(
                            "  <- card {id} failed (attempt {attempts}/{}), retrying",
                            max_retries + 1
                        );
                        log_event(home, &board_name, "retry", Some(&id), &format!("{error:#}"));
                    } else {
                        if let Some(c) = board.card_mut(&id) {
                            c.status = CardStatus::Blocked;
                            c.block_kind = Some("transient".to_string());
                            c.result = Some(format!("failed: {error:#}"));
                        }
                        board.record_run(
                            &id,
                            agent.clone(),
                            "gave_up",
                            &format!("{error:#}"),
                            started_at,
                        );
                        eprintln!("  <- card {id} blocked: {error:#}");
                        log_event(
                            home,
                            &board_name,
                            "blocked",
                            Some(&id),
                            &format!("{error:#}"),
                        );
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
    let prompt = build_goal_judge_task(title, detail, result);
    match a2a_send(
        a2a,
        &worker.agent_id,
        worker.model.as_deref(),
        run_id,
        &prompt,
    ) {
        Ok(reply) => parse_goal_judge_reply(&reply),
        Err(_) => (true, String::new()),
    }
}

/// Stand up the A2A wire + role registry for a one-off board LLM action.
fn board_llm_setup(
    home: &MaturanaHome,
    agents: Option<String>,
) -> anyhow::Result<(A2aWire, RoleRegistry)> {
    let placement = PlacementChoice {
        roles_file: None,
        agents,
        base_spec: None,
    };
    let placement = resolve_role_registry(home, &placement)?;
    if let Some(line) = &placement.status_line {
        println!("  {line}");
    }
    let registry = placement.registry;
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
    use maturana_core::board::Board;
    let mut board = Board::load(home, board_name)?;
    let card = board
        .card(card_id)
        .ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?
        .clone();
    let (a2a, registry) = board_llm_setup(home, agents)?;
    let run_id = format!("board-decompose-{}", chrono::Utc::now().timestamp());
    let mut pool = WorkerPool::new(home, &registry, run_id.clone(), String::new(), 1);
    let worker = if registry.get("coordinator").is_some() {
        pool.resolve("coordinator")?
    } else {
        pool.resolve_assignee(None)?
    };
    let prompt = build_decompose_task(&card.title, &card.detail);
    let reply = a2a_send(
        &a2a,
        &worker.agent_id,
        worker.model.as_deref(),
        &run_id,
        &prompt,
    );
    pool.teardown();
    let reply = reply?;
    let new_ids = apply_decomposition(&mut board, card_id, &reply, &registry)?;
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
    use maturana_core::board::Board;
    let mut board = Board::load(home, board_name)?;
    let card = board
        .card(card_id)
        .ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?
        .clone();
    let (a2a, registry) = board_llm_setup(home, agents)?;
    let run_id = format!("board-specify-{}", chrono::Utc::now().timestamp());
    let mut pool = WorkerPool::new(home, &registry, run_id.clone(), String::new(), 1);
    let worker = pool.resolve_assignee(card.assignee.as_deref().or(Some("developer")))?;
    let prompt = build_specify_task(&card.title, &card.detail);
    let reply = a2a_send(
        &a2a,
        &worker.agent_id,
        worker.model.as_deref(),
        &run_id,
        &prompt,
    );
    pool.teardown();
    let reply = reply?;
    let spec = parse_card_specification(&reply)?;
    apply_card_specification(&mut board, card_id, &spec)?;
    board.save(home)?;
    println!("specified {card_id}");
    Ok(())
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
        let handle =
            std::thread::spawn(move || a2a_send(&a2a, &agent, model.as_deref(), &run_id, &task));
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
