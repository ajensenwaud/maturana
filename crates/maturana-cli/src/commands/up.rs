use anyhow::Context;
use clap::Args;
use maturana_core::{
    orchestrator::{plan_processes, AgentRuntime, OrchestratorConfig, SupervisedProcess},
    spec::{AgentSpec, HarnessRuntime, HostProvider},
    state::MaturanaHome,
};
use maturana_ops::{
    agents::{infer_agent_session_id, list_agent_ids},
    firecracker::firecracker_profile_for,
    runtime_plane::{ensure_graph_token, ensure_sessiond_token},
};
use std::{
    fs,
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Supervise the whole host runtime plane as one restart-on-failure process
/// group: sessiond (:47834), optional MaturanaGraph (:47835) and claude-refresh,
/// plus per-agent channel bridges, schedule + proactivity runners, and egress
/// proxies. Writes <home>/up/state.json (read by `maturana status`). Does NOT
/// boot the agent VMs (that's `agent launch`). Use --dry-run to print the plan.
#[derive(Debug, Args)]
pub(crate) struct UpCommand {
    /// Agents to run. Defaults to every materialized agent under the home.
    #[arg(long = "agent-id")]
    pub(crate) agent_ids: Vec<String>,
    #[arg(long, default_value = "0.0.0.0:47834")]
    pub(crate) sessiond_bind: String,
    #[arg(long, env = "MATURANA_SESSIOND_TOKEN")]
    pub(crate) sessiond_token: Option<String>,
    /// Override every agent's session id. When omitted (the default), each
    /// agent's session id is derived from its materialized spec / Firecracker
    /// profile via [`maturana_ops::agents::infer_agent_session_id`], so the supervised channel
    /// writes to the same queue the guest worker claims from.
    #[arg(long)]
    pub(crate) session_id: Option<String>,
    #[arg(long, default_value = "pipelock:telegram/bot-token")]
    pub(crate) telegram_token_source: String,
    #[arg(long)]
    pub(crate) no_telegram: bool,
    #[arg(long)]
    pub(crate) no_schedules: bool,
    #[arg(long)]
    pub(crate) no_proactive: bool,
    #[arg(long, default_value_t = 5)]
    pub(crate) channel_poll_seconds: u64,
    #[arg(long, default_value_t = 60)]
    pub(crate) schedule_poll_seconds: u64,
    /// Print the resolved process plan and the canonical guest session ids,
    /// then exit without launching anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

pub(crate) fn build_orchestrator_config(
    home: &MaturanaHome,
    command: &UpCommand,
) -> anyhow::Result<OrchestratorConfig> {
    let agent_ids = if command.agent_ids.is_empty() {
        list_agent_ids(home)?
    } else {
        command.agent_ids.clone()
    };
    if agent_ids.is_empty() {
        // Fresh install with no agents yet: keep the plane healthy (supervise
        // sessiond, idle-waiting) instead of exiting with an error. Maturana is
        // Codex-native - the user builds their first agent from Codex, then
        // restarts `maturana up` to wire its channels/schedules.
        eprintln!(
            "up: no agents configured yet - supervising sessiond only (idle). \
             Build an agent from Codex (`cd <repo> && codex`), then restart `maturana up`."
        );
    }
    // claude-code agents get the host-owned OAuth refresh daemon, except
    // Firecracker guests, which keep their own token alive.
    let claude_refresh_agents = agent_ids
        .iter()
        .filter(|id| {
            let spec =
                AgentSpec::from_maturana_markdown(&home.agent_dir(id).join("MATURANA.md")).ok();
            let is_claude = spec
                .as_ref()
                .map(|s| s.runtime.harness == HarnessRuntime::ClaudeCode)
                .unwrap_or(false);
            let is_firecracker = spec
                .as_ref()
                .map(|s| s.vm.provider == HostProvider::Firecracker)
                .unwrap_or(false);
            is_claude && !is_firecracker
        })
        .cloned()
        .collect::<Vec<_>>();
    let graph_opt_in = agent_ids.iter().any(|id| {
        AgentSpec::from_maturana_markdown(&home.agent_dir(id).join("MATURANA.md"))
            .map(|spec| spec.knowledge_graph.enabled)
            .unwrap_or(false)
    });
    let agents = agent_ids
        .into_iter()
        .map(|agent_id| -> anyhow::Result<AgentRuntime> {
            let spec =
                AgentSpec::from_maturana_markdown(&home.agent_dir(&agent_id).join("MATURANA.md"))
                    .ok();
            let slack = spec
                .as_ref()
                .and_then(|s| s.channels.slack.clone())
                .map(|s| maturana_core::orchestrator::SlackRuntime {
                    bot_token_source: s.bot_token_source,
                    app_token_source: s.app_token_source,
                });
            let discord = spec
                .as_ref()
                .and_then(|s| s.channels.discord.clone())
                .map(|d| maturana_core::orchestrator::DiscordRuntime {
                    bot_token_source: d.bot_token_source,
                });
            let agentmail = spec
                .as_ref()
                .and_then(|s| s.channels.agentmail.clone())
                .map(|m| maturana_core::orchestrator::AgentMailRuntime {
                    api_key_source: m.api_key_source,
                    inbox: m.inbox,
                });
            let session_id = match &command.session_id {
                Some(session_id) => session_id.clone(),
                None => infer_agent_session_id(home, &agent_id)?,
            };
            let own_telegram_token = spec
                .as_ref()
                .and_then(|s| s.channels.telegram.as_ref())
                .map(|t| t.token_source.clone())
                .or_else(|| firecracker_profile_for(&agent_id).map(|p| p.telegram_token_source));
            let telegram = !command.no_telegram && own_telegram_token.is_some();
            let telegram_token_source =
                own_telegram_token.unwrap_or_else(|| command.telegram_token_source.clone());
            let proxy = spec
                .as_ref()
                .and_then(|s| s.network.proxy.as_ref())
                .map(|p| p.enabled)
                .unwrap_or(false);
            Ok(AgentRuntime {
                agent_id,
                session_id,
                telegram,
                telegram_token_source,
                schedules: !command.no_schedules,
                proactive: !command.no_proactive,
                slack,
                discord,
                agentmail,
                proxy,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let sessiond_token = match command.sessiond_token.clone() {
        Some(token) => Some(token),
        None => Some(ensure_sessiond_token(&home.root().join("sessiond/token"))?),
    };
    let graph_token = if graph_opt_in {
        Some(ensure_graph_token(home)?)
    } else {
        maturana_core::worker::read_graph_token(home.root())
    };
    Ok(OrchestratorConfig {
        sessiond_bind: command.sessiond_bind.clone(),
        sessiond_token,
        channel_poll_seconds: command.channel_poll_seconds,
        schedule_poll_seconds: command.schedule_poll_seconds,
        agents,
        graph_bind: "0.0.0.0:47835".to_string(),
        graph_token,
        claude_refresh_agents,
    })
}

pub(crate) fn run_up(home: &MaturanaHome, command: UpCommand) -> anyhow::Result<()> {
    let config = build_orchestrator_config(home, &command)?;
    let plan = plan_processes(&config);

    if command.dry_run {
        println!("{}", serde_json::to_string_pretty(&redact_plan(&plan))?);
        println!("\nguest workers must claim from these session ids:");
        for agent in &config.agents {
            println!("  {} -> {}", agent.agent_id, agent.session_id);
        }
        return Ok(());
    }

    supervise_plan(home, &plan)
}

/// Redact the value following any `--token` flag in a process plan, so
/// `up --dry-run` never prints the sessiond/graph bearer tokens.
fn redact_plan(plan: &[SupervisedProcess]) -> Vec<SupervisedProcess> {
    plan.iter()
        .map(|process| {
            let mut redacted = process.clone();
            let mut next_is_secret = false;
            for arg in redacted.args.iter_mut() {
                if next_is_secret {
                    *arg = "<redacted>".to_string();
                    next_is_secret = false;
                } else if arg == "--token" {
                    next_is_secret = true;
                }
            }
            redacted
        })
        .collect()
}

struct Supervised {
    process: SupervisedProcess,
    child: std::process::Child,
    started_at: Instant,
    restarts: u32,
}

/// Write the supervisor heartbeat (`<home>/up/state.json`, schema v1). Best
/// effort: supervision must never die because an observer file write failed.
fn write_up_state(home: &MaturanaHome, supervised: &[Supervised]) {
    let state = serde_json::json!({
        "v": 1,
        "pid": std::process::id(),
        "at": chrono::Utc::now(),
        "processes": supervised.iter().map(|slot| serde_json::json!({
            "name": slot.process.name,
            "pid": slot.child.id(),
            "critical": slot.process.critical,
            "restarts": slot.restarts,
            "uptime_seconds": slot.started_at.elapsed().as_secs(),
        })).collect::<Vec<_>>(),
    });
    let dir = home.root().join("up");
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(
        dir.join("state.json"),
        serde_json::to_vec_pretty(&state).unwrap_or_default(),
    );
}

fn spawn_supervised(
    home: &MaturanaHome,
    process: &SupervisedProcess,
) -> anyhow::Result<std::process::Child> {
    let exe = std::env::current_exe().context("failed to resolve maturana executable path")?;
    ProcessCommand::new(exe)
        .arg("--home")
        .arg(home.root())
        .args(&process.args)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", process.name))
}

fn supervise_plan(home: &MaturanaHome, plan: &[SupervisedProcess]) -> anyhow::Result<()> {
    let mut supervised = Vec::new();
    for process in plan {
        let child = spawn_supervised(home, process)?;
        println!("up: started {} (pid {})", process.name, child.id());
        supervised.push(Supervised {
            process: process.clone(),
            child,
            started_at: Instant::now(),
            restarts: 0,
        });
    }

    loop {
        write_up_state(home, &supervised);
        for slot in supervised.iter_mut() {
            match slot.child.try_wait() {
                Ok(Some(status)) => {
                    if slot.process.critical {
                        anyhow::bail!(
                            "critical process {} exited with {status}; shutting down the plane",
                            slot.process.name
                        );
                    }
                    if slot.started_at.elapsed() > Duration::from_secs(60) {
                        slot.restarts = 0;
                    }
                    let backoff = Duration::from_secs((1u64 << slot.restarts.min(4)).min(16));
                    eprintln!(
                        "up: {} exited with {status}; restarting in {}s",
                        slot.process.name,
                        backoff.as_secs()
                    );
                    thread::sleep(backoff);
                    slot.child = spawn_supervised(home, &slot.process)?;
                    slot.started_at = Instant::now();
                    slot.restarts = slot.restarts.saturating_add(1);
                    println!(
                        "up: restarted {} (pid {})",
                        slot.process.name,
                        slot.child.id()
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("up: failed to poll {}: {error}", slot.process.name);
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}
