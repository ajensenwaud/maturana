mod a2a;
mod channels;
mod commands;
mod graph;
mod orchestrate;
mod personal;
mod proactive;
mod service;
mod session;
mod tui;

use channels::{handle_channel, ChannelCommand};
use clap::{Args, Parser, Subcommand};
use commands::agent::{handle_agent, AgentCommand};
use commands::audit::{handle_audit, AuditCommand};
use commands::claude_refresh::{run_claude_refresh, ClaudeRefreshCommand};
use commands::doctor::{run_doctor, DoctorCommand};
use commands::hostd::{handle_hostd, HostdCommand};
use commands::improve::{run_improve_command, ImproveCommand};
use commands::notify::{handle_notify, NotifyCommand};
use commands::pipelock::{handle_pipelock, PipelockCommand};
use commands::plugin::{handle_plugin, PluginCommand};
use commands::repair::{run_repair, RepairCommand};
use commands::search::{run_search, SearchCommand};
use commands::skill::{handle_skill, SkillCommand};
use commands::snapshot::{handle_snapshot, SnapshotCommand};
use commands::spec::{handle_spec, SpecCommand};
use commands::status::{run_list, run_status, ListCommand, StatusCommand};
use commands::tool::{run_tool_command, ToolCommand};
use commands::up::{run_up, UpCommand};
use commands::vm::{handle_vm, VmCommand};
use commands::web::{run_web_command, WebCommand};
#[cfg(test)]
use maturana_core::spec::AgentSpec;
use maturana_core::state::MaturanaHome;
#[cfg(test)]
use maturana_core::worker::render_firecracker_proxy_env;
#[cfg(test)]
use maturana_ops::firecracker::{
    bind_port, builtin_firecracker_profiles, firecracker_profile_for,
    firecracker_profile_from_spec, render_firecracker_guest_artifacts,
    selected_firecracker_profiles, validate_firecracker_asset_manifest, FirecrackerHarnessProfile,
};
#[cfg(test)]
use maturana_ops::host_setup::{
    ensure_agent_ssh_key, expected_sha256_for_image, public_key_path, sha256_file_hex,
};
#[cfg(test)]
use maturana_ops::windows_harness::{
    quote_cmd_arg, repair_windows_config, safe_windows_task_suffix,
};
use personal::{
    handle_deploy, handle_develop, handle_heartbeat, handle_personal, handle_schedule, handle_wiki,
    DeployCommand, DevelopCommand, HeartbeatCommand, PersonalCommand, ScheduleCommand, WikiCommand,
};
use session::{handle_session, SessionCommand};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "maturana")]
#[command(about = "Secure Codex-native agent orchestration")]
struct Cli {
    #[arg(long, env = "MATURANA_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a MATURANA.md agent spec (pre-flight check).
    Spec(SpecCommand),
    /// Manage one agent: launch, inspect, stop, chat, run, logs, fetch/push.
    Agent(AgentCommand),
    /// List, take, or restore agent VM snapshots.
    Snapshot(SnapshotCommand),
    /// Copy-on-write VM storage: clone/snapshot/rewind a rootfs via filesystem
    /// reflink — instant + space-shared on Btrfs/XFS/ZFS-2.2+, full copy on ext4.
    Vm(VmCommand),
    /// Show an agent's governed audit-log events.
    Audit(AuditCommand),
    /// Windows SYSTEM daemon for Hyper-V VM lifecycle (machine-managed).
    #[command(hide = true)]
    Hostd(HostdCommand),
    /// Secret vault + egress proxy: init/set/get/list/delete/ca-cert.
    Pipelock(PipelockCommand),
    /// Send a one-off Telegram or Discord message.
    Notify(NotifyCommand),
    /// Scaffold a curated single-user personal agent (`personal init`).
    Personal(PersonalCommand),
    /// Per-agent document wiki: init, ingest, keyword search.
    Wiki(WikiCommand),
    /// Write or read an agent liveness record.
    Heartbeat(HeartbeatCommand),
    /// Cron-style scheduled agent tasks: add, list, run-due.
    Schedule(ScheduleCommand),
    /// Proactivity loop runner (machine-managed by `maturana up`).
    #[command(hide = true)]
    Proactive(proactive::ProactiveCommand),
    /// Run a goal across multiple worker agents in a bounded loop.
    Orchestrator(orchestrate::OrchestratorCommand),
    /// Durable orchestration board: define cards, then run them across agents.
    Board(orchestrate::BoardCommand),
    /// Serve Agent2Agent (A2A) endpoints for agent-to-agent calls.
    #[command(hide = true)]
    A2a(a2a::A2aCommand),
    /// Push a skill or tool to a live agent over SSH.
    Deploy(DeployCommand),
    /// Scaffold a new skill or tool locally under skills/ or tools/.
    Develop(DevelopCommand),
    /// Validate skills and install them as native Codex skills.
    Skill(SkillCommand),
    /// Discover, inspect, and validate first- and third-party plugins.
    Plugin(PluginCommand),
    /// Pair chat channels and check channel health (`pair`, `status`).
    Channel(ChannelCommand),
    /// Low-level session queue + sessiond (mostly machine-managed).
    Session(SessionCommand),
    /// MaturanaGraph knowledge graph: ingest and query (GraphRAG).
    Graph(graph::GraphCommand),
    Up(UpCommand),
    /// List materialized agents with a compact health snapshot (host-side
    /// only, no guest SSH). Aliases: `ls`, `agents`.
    #[command(visible_alias = "ls", visible_alias = "agents")]
    List(ListCommand),
    /// Plane + agents health dashboard: supervisor processes (sessiond / graph
    /// / channels / proxies) and a per-agent snapshot. Alias: `st`.
    #[command(visible_alias = "st")]
    Status(StatusCommand),
    /// Interactive console TUI with an agent selector. `maturana tui` opens the
    /// selector; pass an agent id to jump straight in. Cycle agents with
    /// Ctrl+←/→ and reopen the selector with Ctrl+P.
    Tui(TuiCommand),
    /// Serve the web cockpit: a browser control surface complementing the
    /// Codex CLI control plane.
    Web(WebCommand),
    /// Web search via Brave or Tavily (API keys from pipelock).
    Search(SearchCommand),
    /// Register/manage host services (systemd user units / scheduled tasks).
    Service(service::ServiceCommand),
    /// Host-owned Claude OAuth refresh (probe the endpoint, or run the daemon).
    ClaudeRefresh(ClaudeRefreshCommand),
    Tool(ToolCommand),
    Improve(ImproveCommand),
    /// Health-check the plane and agents (pass/fail audit; `--json` for
    /// scripts). For a glanceable dashboard use `maturana status`.
    Doctor(DoctorCommand),
    /// Prepare host/guest prerequisites (Ubuntu image, SSH key, harness
    /// provisioning). Named `setup`; `repair` is kept as a back-compat alias.
    #[command(name = "setup", visible_alias = "repair")]
    Repair(RepairCommand),
}

impl Command {
    fn plugin_command_name(&self) -> &'static str {
        match self {
            Self::Spec(_) => "spec",
            Self::Agent(_) => "agent",
            Self::Snapshot(_) => "snapshot",
            Self::Vm(_) => "vm",
            Self::Audit(_) => "audit",
            Self::Hostd(_) => "hostd",
            Self::Pipelock(_) => "pipelock",
            Self::Notify(_) => "notify",
            Self::Personal(_) => "personal",
            Self::Wiki(_) => "wiki",
            Self::Heartbeat(_) => "heartbeat",
            Self::Schedule(_) => "schedule",
            Self::Proactive(_) => "proactive",
            Self::Orchestrator(_) => "orchestrator",
            Self::Board(_) => "board",
            Self::A2a(_) => "a2a",
            Self::Deploy(_) => "deploy",
            Self::Develop(_) => "develop",
            Self::Skill(_) => "skill",
            Self::Plugin(_) => "plugin",
            Self::Channel(_) => "channel",
            Self::Session(_) => "session",
            Self::Graph(_) => "graph",
            Self::Up(_) => "up",
            Self::List(_) => "list",
            Self::Status(_) => "status",
            Self::Tui(_) => "tui",
            Self::Web(_) => "web",
            Self::Search(_) => "search",
            Self::Service(_) => "service",
            Self::ClaudeRefresh(_) => "claude-refresh",
            Self::Tool(_) => "tool",
            Self::Improve(_) => "improve",
            Self::Doctor(_) => "doctor",
            Self::Repair(_) => "setup",
        }
    }
}

/// Interactive console TUI with an agent selector and live agent switching.
#[derive(Debug, Args)]
struct TuiCommand {
    /// Start chatting with this agent. Omit to open the agent selector first.
    agent_id: Option<String>,
    /// Seconds to wait for each reply before showing a timeout.
    #[arg(long, default_value_t = 180)]
    timeout_seconds: u64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let home = cli
        .home
        .map(MaturanaHome::new)
        .unwrap_or_else(|| MaturanaHome::default_for_cwd(&cwd));

    maturana_ops::plugins::ensure_builtin_command_enabled(
        &home,
        cli.command.plugin_command_name(),
    )?;

    match cli.command {
        Command::Spec(command) => handle_spec(command, &home)?,
        Command::Agent(command) => handle_agent(&home, command)?,
        Command::Vm(command) => handle_vm(command, &home)?,
        Command::Snapshot(command) => handle_snapshot(command, &home)?,
        Command::Audit(command) => handle_audit(command, &home)?,
        Command::Hostd(command) => handle_hostd(command)?,
        Command::Pipelock(command) => handle_pipelock(command, &home)?,
        Command::Notify(command) => handle_notify(command, &home)?,
        Command::Personal(command) => handle_personal(command, &home)?,
        Command::Wiki(command) => handle_wiki(command, &home)?,
        Command::Heartbeat(command) => handle_heartbeat(command, &home)?,
        Command::Schedule(command) => handle_schedule(command, &home)?,
        Command::Proactive(command) => proactive::handle_proactive(command, &home)?,
        Command::Orchestrator(command) => orchestrate::handle_orchestrator(command, &home)?,
        Command::Board(command) => orchestrate::handle_board(command, &home)?,
        Command::A2a(command) => a2a::handle_a2a(command, &home)?,
        Command::Deploy(command) => handle_deploy(command, &home)?,
        Command::Develop(command) => handle_develop(command)?,
        Command::Skill(command) => handle_skill(command, &home)?,
        Command::Plugin(command) => handle_plugin(command, &home)?,
        Command::Channel(command) => handle_channel(command, &home)?,
        Command::Session(command) => handle_session(command, &home)?,
        Command::Graph(command) => graph::handle_graph(command, &home)?,
        Command::Up(command) => run_up(&home, command)?,
        Command::List(command) => run_list(&home, command)?,
        Command::Status(command) => run_status(&home, command)?,
        Command::Tui(command) => {
            tui::run_tui(&home, command.agent_id.as_deref(), command.timeout_seconds)?
        }
        Command::Web(command) => run_web_command(&home, command)?,
        Command::Search(command) => run_search(&home, command)?,
        Command::Service(command) => service::handle_service(command, &home)?,
        Command::ClaudeRefresh(command) => run_claude_refresh(&home, command)?,
        Command::Tool(command) => run_tool_command(&home, command)?,
        Command::Improve(command) => run_improve_command(&home, command)?,
        Command::Doctor(command) => run_doctor(&home, command)?,
        Command::Repair(command) => run_repair(&home, command)?,
    }

    Ok(())
}

pub(crate) fn deliver_image_to_guest(
    home: &MaturanaHome,
    agent_id: &str,
    local_path: &Path,
) -> anyhow::Result<String> {
    commands::agent::deliver_image_to_guest(home, agent_id, local_path)
}

pub(crate) fn vision_prompt_text(caption: Option<&str>, guest_path: &str) -> String {
    commands::agent::vision_prompt_text(caption, guest_path)
}

pub(crate) fn agent_chat_turn(
    home: &MaturanaHome,
    agent_id: &str,
    prompt: &str,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    commands::agent::agent_chat_turn(home, agent_id, prompt, timeout_seconds)
}

#[cfg(test)]
mod tests;
