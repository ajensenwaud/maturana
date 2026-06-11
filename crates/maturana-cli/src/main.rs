mod channels;
mod graph;
mod personal;
mod session;

use anyhow::Context;
use channels::{handle_channel, paired_telegram_chat_source, ChannelCommand};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use maturana_core::{
    audit::{append_event, AuditEvent},
    inspect_agent, materialize_agent,
    orchestrator::{plan_processes, AgentRuntime, OrchestratorConfig, SupervisedProcess},
    pipelock::PipelockVault,
    improvement::TrajectoryStore,
    tools::{run_tool, Capabilities, ResourceLimits, ToolManifest, ToolRegistry},
    pipelock_proxy::{ensure_mitm_ca_cert, run_proxy, HeaderInjection, ProxyConfig},
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, insert_inbound, list_undelivered, mark_delivered, session_paths},
    snapshots::{list_snapshots, restore_snapshot, take_snapshot, SnapshotRecord},
    spec::{AgentSpec, HarnessRuntime},
    state::MaturanaHome,
    stop_agent, validate_spec,
    worker::{
        render_firecracker_bootstrap, render_firecracker_cloud_cfg, render_firecracker_netplan,
        render_firecracker_proxy_env, render_harness_install, render_harness_install_service,
        render_run_agent, render_session_env, render_systemd_service, GuestWorkerConfig,
    },
    LaunchMode, LiveAgentStatus,
};
use personal::{
    handle_deploy, handle_develop, handle_heartbeat, handle_personal, handle_schedule, handle_wiki,
    DeployCommand, DevelopCommand, HeartbeatCommand, PersonalCommand, ScheduleCommand, WikiCommand,
};
use rand::{distributions::Alphanumeric, Rng};
use session::{handle_session, SessionCommand};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

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
    Spec(SpecCommand),
    Agent(AgentCommand),
    Snapshot(SnapshotCommand),
    Audit(AuditCommand),
    Hostd(HostdCommand),
    Pipelock(PipelockCommand),
    Notify(NotifyCommand),
    Personal(PersonalCommand),
    Wiki(WikiCommand),
    Heartbeat(HeartbeatCommand),
    Schedule(ScheduleCommand),
    Deploy(DeployCommand),
    Develop(DevelopCommand),
    Skill(SkillCommand),
    Channel(ChannelCommand),
    Session(SessionCommand),
    Graph(graph::GraphCommand),
    Up(UpCommand),
    Tool(ToolCommand),
    Improve(ImproveCommand),
    Doctor(DoctorCommand),
    Repair(RepairCommand),
}

/// Self-improvement flywheel: capture agent trajectories, attach reward
/// signals, and curate training/preference datasets.
#[derive(Debug, Args)]
struct ImproveCommand {
    #[command(subcommand)]
    command: ImproveSubcommand,
}

#[derive(Debug, Subcommand)]
enum ImproveSubcommand {
    /// Record one agent turn as a trajectory.
    Record {
        agent_id: String,
        #[arg(long, default_value = "telegram-main")]
        session_id: String,
        #[arg(long, default_value = "chat")]
        kind: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        output: String,
        #[arg(long, default_value = "[]")]
        tool_calls: String,
    },
    /// Attach a reward signal to a trajectory (or the latest one for an agent).
    Reward {
        #[arg(long, conflicts_with_all = ["agent_id", "session_id"])]
        trajectory_id: Option<String>,
        #[arg(long)]
        agent_id: Option<String>,
        #[arg(long, default_value = "telegram-main")]
        session_id: String,
        #[arg(long, default_value = "user")]
        source: String,
        #[arg(long, allow_hyphen_values = true)]
        value: f64,
        #[arg(long)]
        note: Option<String>,
    },
    /// List curated examples at or above a reward threshold.
    Curate {
        #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
        min_reward: f64,
        #[arg(long)]
        jsonl: bool,
    },
    /// Summary of the trajectory/reward corpus.
    Report {
        #[arg(long)]
        json: bool,
    },
}

/// Author, register, and run sandboxed WebAssembly tools that agents build on
/// the fly. Tools live under `<home>/tools/<name>/`.
#[derive(Debug, Args)]
struct ToolCommand {
    #[command(subcommand)]
    command: ToolSubcommand,
}

#[derive(Debug, Subcommand)]
enum ToolSubcommand {
    /// Register a compiled `.wasm` module under a manifest into the registry.
    Register {
        name: String,
        #[arg(long)]
        wasm: PathBuf,
        /// Optional manifest JSON; defaults to a pure-compute manifest.
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long, default_value = "0.1.0")]
        version: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// List registered tools.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a tool's manifest.
    Inspect { name: String },
    /// Run a registered tool with a JSON input (requires the wasm-runtime build).
    Run {
        name: String,
        #[arg(long, default_value = "{}")]
        input: String,
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Record this run as a self-improvement trajectory step.
        #[arg(long)]
        agent_id: Option<String>,
    },
}

/// Supervise the host runtime plane (sessiond + per-agent channel bridges and
/// schedule runners) as one coherent, restart-on-failure process group.
#[derive(Debug, Args)]
struct UpCommand {
    /// Agents to run. Defaults to every materialized agent under the home.
    #[arg(long = "agent-id")]
    agent_ids: Vec<String>,
    #[arg(long, default_value = "0.0.0.0:47834")]
    sessiond_bind: String,
    #[arg(long, env = "MATURANA_SESSIOND_TOKEN")]
    sessiond_token: Option<String>,
    #[arg(long, default_value = "telegram-main")]
    session_id: String,
    #[arg(long, default_value = "pipelock:telegram/bot-token")]
    telegram_token_source: String,
    #[arg(long)]
    no_telegram: bool,
    #[arg(long)]
    no_schedules: bool,
    #[arg(long, default_value_t = 5)]
    channel_poll_seconds: u64,
    #[arg(long, default_value_t = 60)]
    schedule_poll_seconds: u64,
    /// Print the resolved process plan and the canonical guest session ids,
    /// then exit without launching anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SpecCommand {
    #[command(subcommand)]
    command: SpecSubcommand,
}

#[derive(Debug, Args)]
struct SkillCommand {
    #[command(subcommand)]
    command: SkillSubcommand,
}

#[derive(Debug, Subcommand)]
enum SkillSubcommand {
    Validate {
        #[arg(default_value = "skills")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, serde::Serialize)]
struct SkillValidationReport {
    root: PathBuf,
    checked: usize,
    failures: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum SpecSubcommand {
    Validate {
        #[arg(default_value = "MATURANA.md")]
        spec: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
struct AgentCommand {
    #[command(subcommand)]
    command: AgentSubcommand,
}

#[derive(Debug, Subcommand)]
enum AgentSubcommand {
    Launch {
        #[arg(default_value = "MATURANA.md")]
        spec: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    Inspect {
        agent_id: String,
        #[arg(long)]
        live: bool,
        #[arg(long)]
        guest: bool,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
    },
    Stop {
        agent_id: String,
        #[arg(long)]
        live: bool,
    },
    Run {
        agent_id: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    Logs {
        agent_id: String,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long, value_enum, default_value_t = LogKind::Agent)]
        kind: LogKind,
        #[arg(long, default_value_t = 80)]
        lines: u16,
    },
    Fetch {
        agent_id: String,
        remote_path: String,
        local_path: PathBuf,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        recursive: bool,
    },
    Push {
        agent_id: String,
        local_path: PathBuf,
        remote_path: String,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        recursive: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogKind {
    Agent,
    Error,
    Stdout,
    Stderr,
    LastMessage,
}

#[derive(Debug, Args)]
struct SnapshotCommand {
    #[command(subcommand)]
    command: SnapshotSubcommand,
}

#[derive(Debug, Subcommand)]
enum SnapshotSubcommand {
    List {
        agent_id: String,
        #[arg(long)]
        live: bool,
    },
    Take {
        agent_id: String,
        name: String,
        #[arg(long)]
        live: bool,
    },
    Restore {
        agent_id: String,
        name: String,
        #[arg(long)]
        live: bool,
    },
}

#[derive(Debug, Args)]
struct AuditCommand {
    #[command(subcommand)]
    command: AuditSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuditSubcommand {
    List {
        agent_id: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
struct HostdCommand {
    #[command(subcommand)]
    command: HostdSubcommand,
}

#[derive(Debug, Subcommand)]
enum HostdSubcommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Serve {
        #[arg(long, default_value = "http://127.0.0.1:47832/")]
        bind_prefix: String,
        #[arg(long, default_value = ".maturana/hostd/token")]
        token_path: PathBuf,
        #[arg(long, default_value = ".maturana/logs/hostd.log")]
        log_path: PathBuf,
    },
}

#[derive(Debug, Args)]
struct PipelockCommand {
    #[command(subcommand)]
    command: PipelockSubcommand,
}

#[derive(Debug, Subcommand)]
enum PipelockSubcommand {
    Init,
    Set {
        name: String,
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        #[arg(long)]
        value_file: Option<PathBuf>,
    },
    Get {
        name: String,
    },
    List,
    Delete {
        name: String,
    },
    CaCert,
    Proxy {
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long)]
        bind: Option<String>,
        #[arg(long = "allow")]
        allowlist: Vec<String>,
        #[arg(long = "inject-header")]
        inject_headers: Vec<HeaderInjectionArg>,
    },
}

#[derive(Debug, Clone)]
struct HeaderInjectionArg(HeaderInjection);

impl std::str::FromStr for HeaderInjectionArg {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(HeaderInjection::parse(value)?))
    }
}

#[derive(Debug, Args)]
struct NotifyCommand {
    #[command(subcommand)]
    command: NotifySubcommand,
}

#[derive(Debug, Args)]
struct DoctorCommand {
    #[arg(long = "agent-id")]
    agent_ids: Vec<String>,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "http://127.0.0.1:47834")]
    sessiond_url: String,
}

#[derive(Debug, Args)]
struct RepairCommand {
    #[command(subcommand)]
    command: RepairSubcommand,
}

#[derive(Debug, Subcommand)]
enum RepairSubcommand {
    UbuntuCloudimg {
        #[arg(long, default_value = "noble")]
        release: String,
        #[arg(long, default_value = "amd64")]
        arch: String,
        #[arg(long = "image-url")]
        image_url: Option<String>,
        #[arg(long = "sha256sums-url")]
        sha256sums_url: Option<String>,
        #[arg(long = "qemu-img")]
        qemu_img: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    SshKey {
        #[arg(
            long = "key-path",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        key_path: PathBuf,
        #[arg(long)]
        force: bool,
    },
    WindowsHarnesses {
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(long = "session-id")]
        session_ids: Vec<String>,
        #[arg(long = "harness")]
        harnesses: Vec<String>,
        #[arg(long = "harness-auth-guest-path")]
        harness_auth_guest_paths: Vec<String>,
        #[arg(long = "telegram-token-source")]
        telegram_token_sources: Vec<String>,
        #[arg(long)]
        register_tasks: bool,
        #[arg(long)]
        skip_guest_worker_refresh: bool,
    },
    GuestWorker {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long)]
        harness: String,
        #[arg(long = "guest-ip")]
        guest_ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long = "harness-auth-guest-path")]
        harness_auth_guest_path: String,
        #[arg(
            long = "sessiond-url",
            default_value = "__MATURANA_DEFAULT_SESSIOND_URL__"
        )]
        sessiond_url: String,
        #[arg(
            long = "sessiond-token-path",
            default_value = ".maturana/sessiond/token"
        )]
        sessiond_token_path: PathBuf,
        #[arg(long = "auth-source")]
        auth_source: Option<PathBuf>,
        #[arg(long)]
        install_harness: bool,
    },
    FirecrackerHarnesses {
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(
            long = "ssh-key",
            default_value = ".maturana/images/firecracker/maturana-firecracker.id_rsa"
        )]
        ssh_key: PathBuf,
        #[arg(long = "sessiond-bind", default_value = "0.0.0.0:47834")]
        sessiond_bind: String,
        #[arg(
            long = "sessiond-token-path",
            default_value = ".maturana/sessiond/token"
        )]
        sessiond_token_path: PathBuf,
        #[arg(long)]
        skip_assets: bool,
        #[arg(long)]
        skip_launch: bool,
        #[arg(long)]
        skip_worker_refresh: bool,
        #[arg(long)]
        no_install_harness: bool,
        #[arg(long, default_value_t = 120)]
        ssh_wait_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum NotifySubcommand {
    Telegram {
        #[arg(
            long,
            env = "MATURANA_TELEGRAM_BOT_TOKEN_SOURCE",
            default_value = "pipelock:telegram/bot-token"
        )]
        token_source: String,
        #[arg(long, env = "MATURANA_TELEGRAM_CHAT_ID_SOURCE")]
        chat_id_source: Option<String>,
        #[arg(long)]
        message: String,
        #[arg(long)]
        dry_run: bool,
    },
    Discord {
        #[arg(long, env = "MATURANA_DISCORD_WEBHOOK_SOURCE")]
        webhook_source: String,
        #[arg(long)]
        message: String,
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let home = cli
        .home
        .map(MaturanaHome::new)
        .unwrap_or_else(|| MaturanaHome::default_for_cwd(&cwd));

    match cli.command {
        Command::Spec(command) => match command.command {
            SpecSubcommand::Validate { spec, json } => {
                let spec = AgentSpec::from_maturana_markdown(&spec)
                    .with_context(|| format!("failed to read {}", spec.display()))?;
                let report = validate_spec(&spec);
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print_report(&report);
                }
                if !report.valid {
                    anyhow::bail!("spec is invalid");
                }
            }
        },
        Command::Agent(command) => match command.command {
            AgentSubcommand::Launch { spec, apply } => {
                let raw = fs::read_to_string(&spec)
                    .with_context(|| format!("failed to read {}", spec.display()))?;
                let parsed = AgentSpec::from_maturana_markdown(&spec)
                    .with_context(|| format!("failed to parse {}", spec.display()))?;
                let mode = if apply {
                    LaunchMode::Apply
                } else {
                    LaunchMode::DryRun
                };
                let materialized = materialize_agent(&parsed, &raw, &home, mode)?;
                println!(
                    "agent {} materialized at {}",
                    materialized.agent_id,
                    materialized.agent_dir.display()
                );
                println!(
                    "launch plan: {}",
                    materialized.agent_dir.join("launch-plan.json").display()
                );
            }
            AgentSubcommand::Inspect {
                agent_id,
                live,
                guest,
                ip,
                ssh_user,
                ssh_key,
            } => {
                let agent_dir = home.agent_dir(&agent_id);
                if !agent_dir.exists() {
                    anyhow::bail!("agent does not exist: {}", agent_id);
                }
                println!("agent: {agent_id}");
                println!("dir: {}", agent_dir.display());
                println!("spec: {}", agent_dir.join("MATURANA.md").display());
                println!("plan: {}", agent_dir.join("launch-plan.json").display());
                if guest && !live {
                    anyhow::bail!("agent inspect --guest requires --live");
                }
                let spec = if guest || ip.is_some() {
                    Some(AgentSpec::from_maturana_markdown(
                        agent_dir.join("MATURANA.md"),
                    )?)
                } else {
                    None
                };
                if live {
                    let status = inspect_agent(&home, &agent_id)?;
                    print_live_agent_status(&status);
                    audit_agent_event(
                        &home,
                        &agent_id,
                        "agent.inspect.live",
                        format!("inspected live {} state", status.provider),
                    )?;

                    if guest || ip.is_some() {
                        let guest_ip = ip.or_else(|| status.ipv4.clone()).ok_or_else(|| {
                            anyhow::anyhow!(
                                "could not discover live IP for {agent_id}; pass --ip explicitly"
                            )
                        })?;
                        let host_key = GuestHostKey::resolve(&home, &agent_id, &guest_ip)?;
                        print_live_guest_state(
                            &guest_ip,
                            &ssh_user,
                            &ssh_key,
                            &host_key,
                            spec.as_ref()
                                .map(|spec| spec.browser.headless_chrome)
                                .unwrap_or(false),
                        )?;
                        audit_agent_event(
                            &home,
                            &agent_id,
                            "agent.inspect.live.guest",
                            format!("inspected live guest at {guest_ip} over ssh"),
                        )?;
                    }
                }
            }
            AgentSubcommand::Stop { agent_id, live } => {
                if !live {
                    anyhow::bail!("agent stop currently requires --live");
                }
                stop_agent(&home, &agent_id)?;
            }
            AgentSubcommand::Run {
                agent_id,
                prompt,
                prompt_file,
                ip: _,
                ssh_user: _,
                ssh_key: _,
                wait,
                timeout_seconds,
            } => {
                let prompt = read_agent_prompt(prompt, prompt_file)?;
                let queued = enqueue_agent_run(&home, &agent_id, &prompt)?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.run.live",
                    format!("queued live prompt in session {}", queued.session_id),
                )?;
                println!(
                    "queued prompt for {agent_id} session {} message {}",
                    queued.session_id, queued.message_id
                );
                if wait {
                    let output = wait_for_agent_run(&home, &agent_id, &queued, timeout_seconds)?;
                    audit_agent_event(
                        &home,
                        &agent_id,
                        "agent.run.live.completed",
                        format!("session prompt completed as {}", output.message_id),
                    )?;
                    println!("{}", output.text);
                }
            }
            AgentSubcommand::Logs {
                agent_id,
                ip,
                ssh_user,
                ssh_key,
                kind,
                lines,
            } => {
                let ip = match ip {
                    Some(ip) => ip,
                    None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not discover live IP for {agent_id}; pass --ip explicitly"
                        )
                    })?,
                };
                let host_key = GuestHostKey::resolve(&home, &agent_id, &ip)?;
                let output = read_live_log(&ip, &ssh_user, &ssh_key, &host_key, kind, lines)?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.logs.live",
                    format!("read live guest log {} at {ip}", kind.as_str()),
                )?;
                print!("{output}");
            }
            AgentSubcommand::Fetch {
                agent_id,
                remote_path,
                local_path,
                ip,
                ssh_user,
                ssh_key,
                recursive,
            } => {
                let ip = match ip {
                    Some(ip) => ip,
                    None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not discover live IP for {agent_id}; pass --ip explicitly"
                        )
                    })?,
                };
                let transfer_roots = agent_transfer_roots(&home, &agent_id, false)?;
                let host_key = GuestHostKey::resolve(&home, &agent_id, &ip)?;
                fetch_live_path(
                    &ip,
                    &ssh_user,
                    &ssh_key,
                    &host_key,
                    &remote_path,
                    &local_path,
                    &transfer_roots,
                    recursive,
                )?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.fetch.live",
                    format!(
                        "fetched {remote_path} from guest at {ip} to {}",
                        local_path.display()
                    ),
                )?;
                println!(
                    "fetched {agent_id}:{remote_path} from {ip} to {}",
                    local_path.display()
                );
            }
            AgentSubcommand::Push {
                agent_id,
                local_path,
                remote_path,
                ip,
                ssh_user,
                ssh_key,
                recursive,
            } => {
                let ip = match ip {
                    Some(ip) => ip,
                    None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not discover live IP for {agent_id}; pass --ip explicitly"
                        )
                    })?,
                };
                let transfer_roots = agent_transfer_roots(&home, &agent_id, true)?;
                let host_key = GuestHostKey::resolve(&home, &agent_id, &ip)?;
                push_live_path(
                    &ip,
                    &ssh_user,
                    &ssh_key,
                    &host_key,
                    &local_path,
                    &remote_path,
                    &transfer_roots,
                    recursive,
                )?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.push.live",
                    format!(
                        "pushed {} to guest at {ip}:{remote_path}",
                        local_path.display()
                    ),
                )?;
                println!(
                    "pushed {} to {agent_id}:{remote_path} at {ip}",
                    local_path.display()
                );
            }
        },
        Command::Snapshot(command) => match command.command {
            SnapshotSubcommand::List { agent_id, live } => {
                let snapshots = match list_snapshots(&home, &agent_id, live) {
                    Ok(snapshots) => snapshots,
                    Err(error) => {
                        audit_agent_event(
                            &home,
                            &agent_id,
                            snapshot_audit_event("list", live, true),
                            format!("failed to list snapshots: {error:#}"),
                        )?;
                        return Err(error);
                    }
                };
                for snapshot in snapshots {
                    print_snapshot_record(&snapshot);
                }
                audit_agent_event(
                    &home,
                    &agent_id,
                    snapshot_audit_event("list", live, false),
                    "listed snapshots through provider-aware Rust snapshot manager",
                )?;
            }
            SnapshotSubcommand::Take {
                agent_id,
                name,
                live,
            } => {
                let snapshot = match take_snapshot(&home, &agent_id, &name, live) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        audit_agent_event(
                            &home,
                            &agent_id,
                            snapshot_audit_event("take", live, true),
                            format!("failed to take snapshot {name}: {error:#}"),
                        )?;
                        return Err(error);
                    }
                };
                print_snapshot_record(&snapshot);
                audit_agent_event(
                    &home,
                    &agent_id,
                    snapshot_audit_event("take", live, false),
                    format!("created {:?} snapshot {name}", snapshot.kind),
                )?;
            }
            SnapshotSubcommand::Restore {
                agent_id,
                name,
                live,
            } => {
                let snapshot = match restore_snapshot(&home, &agent_id, &name, live) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        audit_agent_event(
                            &home,
                            &agent_id,
                            snapshot_audit_event("restore", live, true),
                            format!("failed to restore snapshot {name}: {error:#}"),
                        )?;
                        return Err(error);
                    }
                };
                print_snapshot_record(&snapshot);
                audit_agent_event(
                    &home,
                    &agent_id,
                    snapshot_audit_event("restore", live, false),
                    format!("restored {:?} snapshot {name}", snapshot.kind),
                )?;
            }
        },
        Command::Audit(command) => match command.command {
            AuditSubcommand::List {
                agent_id,
                limit,
                json,
            } => {
                let events = read_agent_audit_events(&home, &agent_id)?;
                let start = events.len().saturating_sub(limit);
                let events = &events[start..];
                if json {
                    println!("{}", serde_json::to_string_pretty(events)?);
                } else if events.is_empty() {
                    println!("no audit events for {agent_id}");
                } else {
                    for event in events {
                        println!(
                            "{} {} {}",
                            event.at.to_rfc3339(),
                            event.action,
                            event.message
                        );
                    }
                }
            }
        },
        Command::Hostd(command) => match command.command {
            HostdSubcommand::Status { json } => {
                let status = hostd_status()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    println!("hostd.url: {}", status.url);
                    println!("hostd.reachable: {}", status.reachable);
                    println!("hostd.token_present: {}", status.token_present);
                    if let Some(error) = status.error {
                        println!("hostd.error: {error}");
                    }
                }
            }
            HostdSubcommand::Serve {
                bind_prefix,
                token_path,
                log_path,
            } => {
                run_hostd_server(&bind_prefix, &token_path, &log_path)?;
            }
        },
        Command::Pipelock(command) => {
            let vault = PipelockVault::new(home.pipelock_dir());
            match command.command {
                PipelockSubcommand::Init => {
                    vault.init()?;
                    println!("pipelock vault: {}", vault.vault_path().display());
                    println!("pipelock key: {}", vault.key_path().display());
                }
                PipelockSubcommand::Set {
                    name,
                    value,
                    value_file,
                } => {
                    let value = read_pipelock_value(value, value_file)?;
                    vault.set(&name, &value)?;
                    println!("pipelock secret stored: {name}");
                }
                PipelockSubcommand::Get { name } => {
                    println!("{}", vault.get(&name)?);
                }
                PipelockSubcommand::List => {
                    for name in vault.list()? {
                        println!("{name}");
                    }
                }
                PipelockSubcommand::Delete { name } => {
                    if vault.delete(&name)? {
                        println!("pipelock secret deleted: {name}");
                    } else {
                        println!("pipelock secret not found: {name}");
                    }
                }
                PipelockSubcommand::CaCert => {
                    let path = ensure_mitm_ca_cert(home.root())?;
                    println!("{}", path.display());
                }
                PipelockSubcommand::Proxy {
                    spec,
                    bind,
                    allowlist,
                    inject_headers,
                } => {
                    let (bind, mut config) = match spec {
                        Some(spec_path) => {
                            let spec = AgentSpec::from_maturana_markdown(&spec_path).with_context(
                                || format!("failed to read {}", spec_path.display()),
                            )?;
                            let report = validate_spec(&spec);
                            if !report.valid {
                                anyhow::bail!(
                                    "spec is invalid; run `maturana spec validate {}`",
                                    spec_path.display()
                                );
                            }
                            let proxy = spec.network.proxy.as_ref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "{} does not declare network.proxy",
                                    spec_path.display()
                                )
                            })?;
                            let bind = bind.unwrap_or_else(|| proxy.bind.clone());
                            let audit_path = home
                                .audit_dir()
                                .join(format!("{}-pipelock-proxy.jsonl", spec.identity.id));
                            (
                                bind,
                                ProxyConfig::from_spec(
                                    home.root().to_path_buf(),
                                    &spec,
                                    audit_path,
                                )?,
                            )
                        }
                        None => {
                            let audit_path = home.audit_dir().join("pipelock-proxy.jsonl");
                            (
                                bind.unwrap_or_else(|| "127.0.0.1:47833".to_string()),
                                ProxyConfig {
                                    home_root: home.root().to_path_buf(),
                                    allowlist: Vec::new(),
                                    injections: Vec::new(),
                                    audit_path,
                                },
                            )
                        }
                    };
                    config.allowlist.extend(allowlist);
                    config
                        .injections
                        .extend(inject_headers.into_iter().map(|injection| injection.0));
                    if config.allowlist.is_empty() {
                        anyhow::bail!(
                            "pipelock proxy requires network.egress_allowlist or at least one --allow host"
                        );
                    }
                    println!("pipelock proxy listening on {bind}");
                    println!("pipelock proxy allowlist: {}", config.allowlist.join(", "));
                    println!("pipelock proxy audit: {}", config.audit_path.display());
                    run_proxy(&bind, config)?;
                }
            }
        }
        Command::Notify(command) => match command.command {
            NotifySubcommand::Telegram {
                token_source,
                chat_id_source,
                message,
                dry_run,
            } => {
                if dry_run {
                    println!("telegram notification dry-run: {message}");
                    return Ok(());
                }

                let token = resolve_secret_source_with_home(&token_source, home.root())?;
                let chat_id_source =
                    chat_id_source.or_else(|| paired_telegram_chat_source(&home)).ok_or_else(
                        || {
                            anyhow::anyhow!(
                                "Telegram chat is not paired; run `maturana channel pair telegram start`, send `/pair CODE` to the bot, then run `maturana channel pair telegram complete`"
                            )
                        },
                    )?;
                let chat_id = resolve_secret_source_with_home(&chat_id_source, home.root())?;
                send_telegram(
                    token.expose_for_runtime(),
                    chat_id.expose_for_runtime(),
                    &message,
                )?;
                println!("telegram notification sent");
            }
            NotifySubcommand::Discord {
                webhook_source,
                message,
                dry_run,
            } => {
                if dry_run {
                    println!("discord notification dry-run: {message}");
                    return Ok(());
                }
                let webhook = resolve_secret_source_with_home(&webhook_source, home.root())?;
                send_discord(webhook.expose_for_runtime(), &message)?;
                println!("discord notification sent");
            }
        },
        Command::Personal(command) => handle_personal(command, &home)?,
        Command::Wiki(command) => handle_wiki(command, &home)?,
        Command::Heartbeat(command) => handle_heartbeat(command, &home)?,
        Command::Schedule(command) => handle_schedule(command, &home)?,
        Command::Deploy(command) => handle_deploy(command, &home)?,
        Command::Develop(command) => handle_develop(command)?,
        Command::Skill(command) => handle_skill(command)?,
        Command::Channel(command) => handle_channel(command, &home)?,
        Command::Session(command) => handle_session(command, &home)?,
        Command::Graph(command) => graph::handle_graph(command, &home)?,
        Command::Up(command) => run_up(&home, command)?,
        Command::Tool(command) => run_tool_command(&home, command)?,
        Command::Improve(command) => run_improve_command(&home, command)?,
        Command::Doctor(command) => run_doctor(&home, command)?,
        Command::Repair(command) => run_repair(&home, command)?,
    }

    Ok(())
}

fn read_pipelock_value(
    value: Option<String>,
    value_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(path) = value_file {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        return Ok(trim_trailing_newlines(raw));
    }

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    if raw.is_empty() {
        anyhow::bail!("pipelock set requires --value, --value-file, or stdin");
    }
    Ok(trim_trailing_newlines(raw))
}

fn trim_trailing_newlines(mut value: String) -> String {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    value
}

fn handle_skill(command: SkillCommand) -> anyhow::Result<()> {
    match command.command {
        SkillSubcommand::Validate { root, json } => {
            let report = validate_skill_pack(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.failures.is_empty() {
                println!(
                    "valid skill pack: {} skills checked under {}",
                    report.checked,
                    report.root.display()
                );
            } else {
                for failure in &report.failures {
                    eprintln!("{failure}");
                }
            }
            if report.failures.is_empty() {
                Ok(())
            } else {
                anyhow::bail!(
                    "skill validation failed: {} issue(s)",
                    report.failures.len()
                )
            }
        }
    }
}

fn validate_skill_pack(root: &Path) -> anyhow::Result<SkillValidationReport> {
    let required_sections = [
        "## Grounding",
        "## Preflight",
        "## Decision Path",
        "## Actions",
        "## Evidence",
        "## Recovery",
        "## Boundaries",
    ];
    let mut failures = Vec::new();
    let mut checked = 0usize;

    if !root.exists() {
        anyhow::bail!("skill root does not exist: {}", root.display());
    }

    for required_skill in required_initial_skills() {
        let skill_path = root.join(required_skill).join("SKILL.md");
        if !skill_path.exists() {
            failures.push(format!(
                "missing AGENTS.md initial skill: {}",
                skill_path.display()
            ));
        }
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.exists() {
            failures.push(format!("missing {}", skill_path.display()));
            continue;
        }
        checked += 1;
        let raw = fs::read_to_string(&skill_path)
            .with_context(|| format!("failed to read {}", skill_path.display()))?;
        if !raw.trim_start().starts_with("# ") {
            failures.push(format!(
                "{} must start with a level-1 title",
                skill_path.display()
            ));
        }
        if !raw.contains("Use this skill when") {
            failures.push(format!(
                "{} must describe when to use the skill",
                skill_path.display()
            ));
        }
        if !raw.contains("Read `AGENTS.md`") {
            failures.push(format!(
                "{} grounding must read AGENTS.md",
                skill_path.display()
            ));
        }
        for section in required_sections {
            if !raw.contains(section) {
                failures.push(format!("{} missing {section}", skill_path.display()));
            }
        }
        if raw.contains("## Procedure") {
            failures.push(format!(
                "{} still uses catch-all Procedure section",
                skill_path.display()
            ));
        }
        if raw.contains("just run") || raw.contains("simply run") {
            failures.push(format!(
                "{} uses thin-wrapper language; add grounding/evidence/recovery instead",
                skill_path.display()
            ));
        }
        let evidence_bullets = section_bullet_count(&raw, "## Evidence", Some("## Recovery"));
        if evidence_bullets < 4 {
            failures.push(format!(
                "{} evidence section must list at least four concrete proof points",
                skill_path.display()
            ));
        }
        let recovery_bullets = section_bullet_count(&raw, "## Recovery", Some("## Boundaries"));
        if recovery_bullets < 4 {
            failures.push(format!(
                "{} recovery section must list at least four failure-handling paths",
                skill_path.display()
            ));
        }
        let boundary_do_nots = section_prefixed_line_count(&raw, "## Boundaries", None, "- Do not");
        if boundary_do_nots < 3 {
            failures.push(format!(
                "{} boundaries section must include at least three explicit 'Do not' limits",
                skill_path.display()
            ));
        }
    }

    Ok(SkillValidationReport {
        root: root.to_path_buf(),
        checked,
        failures,
    })
}

fn required_initial_skills() -> &'static [&'static str] {
    &[
        "maturana-agent-create",
        "maturana-agent-validate",
        "maturana-agent-launch",
        "maturana-agent-inspect",
        "maturana-agent-update",
        "maturana-skill-create",
        "maturana-tool-create",
        "maturana-skill-deploy",
        "maturana-security-review",
        "maturana-snapshot",
    ]
}

fn section_text<'a>(raw: &'a str, start: &str, end: Option<&str>) -> &'a str {
    let Some((_, after_start)) = raw.split_once(start) else {
        return "";
    };
    if let Some(end) = end {
        after_start
            .split_once(end)
            .map(|(section, _)| section)
            .unwrap_or(after_start)
    } else {
        after_start
    }
}

fn section_bullet_count(raw: &str, start: &str, end: Option<&str>) -> usize {
    section_prefixed_line_count(raw, start, end, "- ")
}

fn section_prefixed_line_count(raw: &str, start: &str, end: Option<&str>, prefix: &str) -> usize {
    section_text(raw, start, end)
        .lines()
        .filter(|line| line.trim_start().starts_with(prefix))
        .count()
}

#[derive(Debug, serde::Serialize)]
struct DoctorReport {
    ok: bool,
    home: String,
    hostd: DoctorCheck,
    sessiond: DoctorCheck,
    agents: Vec<DoctorAgentReport>,
}

#[derive(Debug, serde::Serialize)]
struct DoctorAgentReport {
    agent_id: String,
    vm: DoctorCheck,
    telegram: DoctorCheck,
    guest_worker: DoctorCheck,
}

#[derive(Debug, serde::Serialize)]
struct DoctorCheck {
    ok: bool,
    message: String,
}

fn tool_registry(home: &MaturanaHome) -> ToolRegistry {
    ToolRegistry::new(home.root().join("tools"))
}

fn run_tool_command(home: &MaturanaHome, command: ToolCommand) -> anyhow::Result<()> {
    let registry = tool_registry(home);
    match command.command {
        ToolSubcommand::Register {
            name,
            wasm,
            manifest,
            version,
            description,
        } => {
            let wasm_bytes = fs::read(&wasm)
                .with_context(|| format!("failed to read wasm {}", wasm.display()))?;
            let manifest = match manifest {
                Some(path) => {
                    let raw = fs::read_to_string(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?;
                    let mut parsed: ToolManifest = serde_json::from_str(&raw)
                        .with_context(|| format!("failed to parse manifest {}", path.display()))?;
                    parsed.name = name.clone();
                    parsed
                }
                None => ToolManifest {
                    name: name.clone(),
                    version,
                    description,
                    wasm: "module.wasm".to_string(),
                    capabilities: Capabilities::default(),
                    limits: ResourceLimits::default(),
                    input_schema: serde_json::Value::Null,
                    output_schema: serde_json::Value::Null,
                },
            };
            let stored = registry.register(&manifest, &wasm_bytes)?;
            audit_agent_event(
                home,
                &name,
                "tool.register",
                format!("registered wasm tool {} v{}", stored.name, stored.version),
            )
            .ok();
            println!(
                "registered tool {} v{} ({})",
                stored.name,
                stored.version,
                registry.tool_dir(&stored.name).display()
            );
            if !stored.capabilities.is_pure() {
                println!(
                    "capabilities: fs_read={:?} fs_write={:?} env={:?} net={:?}",
                    stored.capabilities.fs_read,
                    stored.capabilities.fs_write,
                    stored.capabilities.env,
                    stored.capabilities.net
                );
            }
        }
        ToolSubcommand::List { json } => {
            let tools = registry.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tools)?);
            } else if tools.is_empty() {
                println!("no tools registered under {}", registry.root().display());
            } else {
                for tool in tools {
                    println!(
                        "{} v{} pure={} :: {}",
                        tool.name,
                        tool.version,
                        tool.capabilities.is_pure(),
                        tool.description
                    );
                }
            }
        }
        ToolSubcommand::Inspect { name } => {
            let manifest = registry.load(&name)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        ToolSubcommand::Run {
            name,
            input,
            input_file,
            agent_id,
        } => {
            let input = match input_file {
                Some(path) => fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
                None => input,
            };
            let result = run_tool(&registry, &name, &input)?;
            if let Some(agent_id) = agent_id {
                audit_agent_event(
                    home,
                    &agent_id,
                    "tool.run",
                    format!(
                        "ran tool {} ok={} duration_ms={}",
                        result.tool, result.ok, result.duration_ms
                    ),
                )
                .ok();
            }
            print!("{}", result.stdout);
            if !result.stdout.ends_with('\n') && !result.stdout.is_empty() {
                println!();
            }
            if !result.ok {
                anyhow::bail!("tool {} failed: {}", result.tool, result.stderr.trim());
            }
        }
    }
    Ok(())
}

fn run_improve_command(home: &MaturanaHome, command: ImproveCommand) -> anyhow::Result<()> {
    let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
    match command.command {
        ImproveSubcommand::Record {
            agent_id,
            session_id,
            kind,
            input,
            output,
            tool_calls,
        } => {
            let id = store.record(&agent_id, &session_id, &kind, &input, &output, &tool_calls)?;
            println!("recorded trajectory {id}");
        }
        ImproveSubcommand::Reward {
            trajectory_id,
            agent_id,
            session_id,
            source,
            value,
            note,
        } => match (trajectory_id, agent_id) {
            (Some(id), _) => {
                store.attach_reward(&id, &source, value, note.as_deref())?;
                println!("rewarded {id} ({source} {value:+})");
            }
            (None, Some(agent_id)) => {
                match store.reward_latest(&agent_id, &session_id, &source, value, note.as_deref())? {
                    Some(id) => println!("rewarded latest trajectory {id} ({source} {value:+})"),
                    None => println!("no trajectory for {agent_id}/{session_id} to reward"),
                }
            }
            (None, None) => anyhow::bail!("pass --trajectory-id or --agent-id"),
        },
        ImproveSubcommand::Curate { min_reward, jsonl } => {
            if jsonl {
                print!("{}", store.export_sft_jsonl(min_reward)?);
            } else {
                let curated = store.curate(min_reward)?;
                if curated.is_empty() {
                    println!("no trajectories at or above reward {min_reward}");
                }
                for example in curated {
                    println!(
                        "{} reward={:+} ({} signals) :: {}",
                        example.trajectory.id,
                        example.reward.total,
                        example.reward.count,
                        truncate_inline(&example.trajectory.input, 60)
                    );
                }
            }
        }
        ImproveSubcommand::Report { json } => {
            let report = store.report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("trajectories: {}", report.trajectories);
                println!("rewarded: {}", report.rewarded);
                println!("net-positive: {}", report.positive);
                println!("net-negative: {}", report.negative);
            }
        }
    }
    Ok(())
}

fn truncate_inline(value: &str, limit: usize) -> String {
    let value = value.replace('\n', " ");
    if value.chars().count() <= limit {
        value
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}

fn build_orchestrator_config(
    home: &MaturanaHome,
    command: &UpCommand,
) -> anyhow::Result<OrchestratorConfig> {
    let agent_ids = if command.agent_ids.is_empty() {
        discover_agent_ids(home)?
    } else {
        command.agent_ids.clone()
    };
    if agent_ids.is_empty() {
        anyhow::bail!(
            "no agents to supervise; launch one with `maturana agent launch` or pass --agent-id"
        );
    }
    let agents = agent_ids
        .into_iter()
        .map(|agent_id| AgentRuntime {
            agent_id,
            session_id: command.session_id.clone(),
            telegram: !command.no_telegram,
            telegram_token_source: command.telegram_token_source.clone(),
            schedules: !command.no_schedules,
        })
        .collect();
    // sessiond now refuses to run unauthenticated, so default the token to the
    // persistent per-home token file when the operator did not pass one. This
    // keeps `up` secure-by-default without forcing a manual --sessiond-token.
    let sessiond_token = match command.sessiond_token.clone() {
        Some(token) => Some(token),
        None => Some(ensure_sessiond_token(&home.root().join("sessiond/token"))?),
    };
    Ok(OrchestratorConfig {
        sessiond_bind: command.sessiond_bind.clone(),
        sessiond_token,
        channel_poll_seconds: command.channel_poll_seconds,
        schedule_poll_seconds: command.schedule_poll_seconds,
        agents,
        graph_bind: "0.0.0.0:47835".to_string(),
        // Supervise the graph service only if it's been set up (token present).
        graph_token: maturana_core::worker::read_graph_token(home.root()),
    })
}

fn run_up(home: &MaturanaHome, command: UpCommand) -> anyhow::Result<()> {
    let config = build_orchestrator_config(home, &command)?;
    let plan = plan_processes(&config);

    if command.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        println!("\nguest workers must claim from these session ids:");
        for agent in &config.agents {
            println!("  {} -> {}", agent.agent_id, agent.session_id);
        }
        return Ok(());
    }

    supervise_plan(home, &plan)
}

struct Supervised {
    process: SupervisedProcess,
    child: std::process::Child,
    started_at: Instant,
    restarts: u32,
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
        for slot in supervised.iter_mut() {
            match slot.child.try_wait() {
                Ok(Some(status)) => {
                    if slot.process.critical {
                        anyhow::bail!(
                            "critical process {} exited with {status}; shutting down the plane",
                            slot.process.name
                        );
                    }
                    // Reset the restart counter after a process has stayed up a
                    // while, so a flaky transient does not exhaust the budget.
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
                    println!("up: restarted {} (pid {})", slot.process.name, slot.child.id());
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

fn run_doctor(home: &MaturanaHome, command: DoctorCommand) -> anyhow::Result<()> {
    let agent_ids = if command.agent_ids.is_empty() {
        discover_agent_ids(home)?
    } else {
        command.agent_ids
    };

    let hostd = doctor_hostd();
    let vms = doctor_vms().unwrap_or_default();
    let sessiond = doctor_http_health(&format!(
        "{}/health",
        command.sessiond_url.trim_end_matches('/')
    ));
    let agents = agent_ids
        .iter()
        .map(|agent_id| doctor_agent(home, agent_id, &vms))
        .collect::<Vec<_>>();
    let ok = hostd.ok
        && sessiond.ok
        && agents
            .iter()
            .all(|agent| agent.vm.ok && agent.telegram.ok && agent.guest_worker.ok);
    let report = DoctorReport {
        ok,
        home: home.root().display().to_string(),
        hostd,
        sessiond,
        agents,
    };

    if command.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("maturana.ok: {}", report.ok);
        println!("home: {}", report.home);
        print_doctor_check("hostd", &report.hostd);
        print_doctor_check("sessiond", &report.sessiond);
        for agent in &report.agents {
            println!("agent: {}", agent.agent_id);
            print_doctor_check("  vm", &agent.vm);
            print_doctor_check("  telegram", &agent.telegram);
            print_doctor_check("  guest_worker", &agent.guest_worker);
        }
    }
    if !report.ok {
        anyhow::bail!("maturana doctor found unhealthy components");
    }
    Ok(())
}

fn run_repair(home: &MaturanaHome, command: RepairCommand) -> anyhow::Result<()> {
    match command.command {
        RepairSubcommand::UbuntuCloudimg {
            release,
            arch,
            image_url,
            sha256sums_url,
            qemu_img,
            force,
        } => repair_ubuntu_cloudimg(UbuntuCloudimgRepair {
            home: home.clone(),
            release,
            arch,
            image_url,
            sha256sums_url,
            qemu_img,
            force,
        }),
        RepairSubcommand::SshKey { key_path, force } => {
            ensure_agent_ssh_key(absolute_or_cwd(key_path)?, force)
        }
        RepairSubcommand::WindowsHarnesses {
            agent_ids,
            session_ids,
            harnesses,
            harness_auth_guest_paths,
            telegram_token_sources,
            register_tasks,
            skip_guest_worker_refresh,
        } => {
            let config = repair_windows_config(
                agent_ids,
                session_ids,
                harnesses,
                harness_auth_guest_paths,
                telegram_token_sources,
            )?;
            repair_windows_harnesses(home, &config, register_tasks, skip_guest_worker_refresh)
        }
        RepairSubcommand::GuestWorker {
            agent_id,
            session_id,
            harness,
            guest_ip,
            ssh_user,
            ssh_key,
            harness_auth_guest_path,
            sessiond_url,
            sessiond_token_path,
            auth_source,
            install_harness,
        } => install_guest_worker(
            home,
            GuestWorkerInstall {
                guest_ip: resolve_guest_ip(home, &agent_id, guest_ip)?,
                agent_id,
                session_id,
                harness: parse_harness_runtime(&harness)?,
                ssh_user,
                ssh_key,
                harness_auth_guest_path,
                sessiond_url,
                sessiond_token_path,
                auth_source,
                install_harness,
            },
        ),
        RepairSubcommand::FirecrackerHarnesses {
            agent_ids,
            ssh_key,
            sessiond_bind,
            sessiond_token_path,
            skip_assets,
            skip_launch,
            skip_worker_refresh,
            no_install_harness,
            ssh_wait_seconds,
        } => repair_firecracker_harnesses(
            home,
            FirecrackerHarnessRepair {
                agent_ids,
                ssh_key,
                sessiond_bind,
                sessiond_token_path,
                skip_assets,
                skip_launch,
                skip_worker_refresh,
                install_harness: !no_install_harness,
                ssh_wait_seconds,
            },
        ),
    }
}

#[derive(Debug, Clone)]
struct UbuntuCloudimgRepair {
    home: MaturanaHome,
    release: String,
    arch: String,
    image_url: Option<String>,
    sha256sums_url: Option<String>,
    qemu_img: Option<PathBuf>,
    force: bool,
}

fn repair_ubuntu_cloudimg(repair: UbuntuCloudimgRepair) -> anyhow::Result<()> {
    let image_url = repair.image_url.unwrap_or_else(|| {
        format!(
            "https://cloud-images.ubuntu.com/{release}/current/{release}-server-cloudimg-{arch}.img",
            release = repair.release,
            arch = repair.arch
        )
    });
    let sha256sums_url = repair.sha256sums_url.unwrap_or_else(|| {
        format!(
            "https://cloud-images.ubuntu.com/{}/current/SHA256SUMS",
            repair.release
        )
    });

    let image_name = image_url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("image URL has no filename: {image_url}"))?
        .to_string();
    let image_dir = repair
        .home
        .root()
        .join("images")
        .join(format!("ubuntu-{}", repair.release));
    fs::create_dir_all(&image_dir)
        .with_context(|| format!("failed to create {}", image_dir.display()))?;

    let img_path = image_dir.join(&image_name);
    let sha_path = image_dir.join("SHA256SUMS");
    let vhdx_path = image_dir.join(format!(
        "{}-server-cloudimg-{}.vhdx",
        repair.release, repair.arch
    ));

    download_if_needed(
        &image_url,
        &img_path,
        repair.force,
        "official Ubuntu cloud image",
    )?;
    download_if_needed(&sha256sums_url, &sha_path, repair.force, "SHA256SUMS")?;

    let expected = expected_sha256_for_image(&sha_path, &image_name)?;
    let actual = sha256_file_hex(&img_path)?;
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {}. Expected {} but got {}.",
            img_path.display(),
            expected,
            actual
        );
    }
    println!("Checksum OK.");

    if repair.force || !vhdx_path.exists() {
        let qemu_img = find_qemu_img(repair.qemu_img)?;
        println!("Converting image to VHDX with {}...", qemu_img.display());
        let status = ProcessCommand::new(&qemu_img)
            .arg("convert")
            .arg("-p")
            .arg("-O")
            .arg("vhdx")
            .arg("-o")
            .arg("subformat=dynamic")
            .arg(&img_path)
            .arg(&vhdx_path)
            .status()
            .with_context(|| format!("failed to run {}", qemu_img.display()))?;
        if !status.success() {
            anyhow::bail!("qemu-img conversion failed with {status}");
        }
    } else {
        println!("Using existing VHDX {}", vhdx_path.display());
    }

    println!("VHDX: {}", vhdx_path.display());
    Ok(())
}

fn download_if_needed(url: &str, path: &Path, force: bool, label: &str) -> anyhow::Result<()> {
    if path.exists() && !force {
        println!("Using existing {} {}", label, path.display());
        return Ok(());
    }
    println!("Downloading {label}...");
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read response from {url}"))?;
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn expected_sha256_for_image(sha_path: &Path, image_name: &str) -> anyhow::Result<String> {
    let sha_text = fs::read_to_string(sha_path)
        .with_context(|| format!("failed to read {}", sha_path.display()))?;
    for line in sha_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        if parts.any(|part| part.trim_start_matches('*') == image_name) {
            return Ok(hash.to_lowercase());
        }
    }
    anyhow::bail!(
        "no checksum entry for {image_name} in {}",
        sha_path.display()
    )
}

fn sha256_file_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn find_qemu_img(requested: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        let resolved = absolute_or_cwd(path)?;
        if resolved.exists() {
            return Ok(resolved);
        }
        anyhow::bail!("qemu-img not found at {}", resolved.display());
    }

    if let Some(path) = find_on_path(qemu_img_binary()) {
        return Ok(path);
    }

    for candidate in qemu_img_candidates() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!(
        "{} is required to convert the official Ubuntu cloud image to VHDX. Install QEMU for Windows or pass --qemu-img.",
        qemu_img_binary()
    )
}

fn qemu_img_binary() -> &'static str {
    if cfg!(windows) {
        "qemu-img.exe"
    } else {
        "qemu-img"
    }
}

fn qemu_img_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if cfg!(windows) {
        candidates.extend([
            PathBuf::from(r"C:\Program Files\qemu\qemu-img.exe"),
            PathBuf::from(r"C:\Program Files (x86)\qemu\qemu-img.exe"),
            PathBuf::from(r"C:\msys64\mingw64\bin\qemu-img.exe"),
            PathBuf::from(r"C:\msys64\ucrt64\bin\qemu-img.exe"),
        ]);
        if let Some(localappdata) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(localappdata).join(
                r"Microsoft\WinGet\Packages\cloudbase.qemu-img_Microsoft.Winget.Source_8wekyb3d8bbwe\qemu-img.exe",
            ));
        }
    }
    candidates
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn ensure_agent_ssh_key(key_path: PathBuf, force: bool) -> anyhow::Result<()> {
    if key_path.exists() && !force {
        println!("Using existing SSH key: {}", key_path.display());
        return Ok(());
    }

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if force {
        let _ = fs::remove_file(&key_path);
        let _ = fs::remove_file(public_key_path(&key_path));
    }

    let status = ProcessCommand::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(&key_path)
        .arg("-C")
        .arg("maturana-agent")
        .status()
        .context("failed to run ssh-keygen")?;
    if !status.success() {
        anyhow::bail!("ssh-keygen failed with {status}");
    }

    tighten_private_key_permissions(&key_path)?;
    println!("SSH key: {}", key_path.display());
    Ok(())
}

fn public_key_path(key_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.pub", key_path.display()))
}

#[cfg(windows)]
fn tighten_private_key_permissions(key_path: &PathBuf) -> anyhow::Result<()> {
    let user = std::env::var("USERNAME").context("USERNAME is not set")?;
    let grant = format!("{user}:R");
    let status = ProcessCommand::new("icacls")
        .arg(key_path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .status()
        .context("failed to run icacls")?;
    if !status.success() {
        anyhow::bail!("icacls failed with {status}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn tighten_private_key_permissions(key_path: &PathBuf) -> anyhow::Result<()> {
    let status = ProcessCommand::new("chmod")
        .arg("600")
        .arg(key_path)
        .status()
        .context("failed to run chmod")?;
    if !status.success() {
        anyhow::bail!("chmod failed with {status}");
    }
    Ok(())
}

fn resolve_guest_ip(
    home: &MaturanaHome,
    agent_id: &str,
    explicit_ip: Option<String>,
) -> anyhow::Result<String> {
    if let Some(ip) = explicit_ip {
        return Ok(ip);
    }
    inspect_agent(home, agent_id)?.ipv4.ok_or_else(|| {
        anyhow::anyhow!("could not discover live IP for {agent_id}; pass --guest-ip explicitly")
    })
}

#[derive(Debug, Clone)]
struct FirecrackerHarnessRepair {
    agent_ids: Vec<String>,
    ssh_key: PathBuf,
    sessiond_bind: String,
    sessiond_token_path: PathBuf,
    skip_assets: bool,
    skip_launch: bool,
    skip_worker_refresh: bool,
    install_harness: bool,
    ssh_wait_seconds: u64,
}

#[derive(Debug, Clone)]
struct FirecrackerHarnessProfile {
    agent_id: &'static str,
    image_name: &'static str,
    harness_arg: &'static str,
    host_ip: &'static str,
    guest_ip: &'static str,
    cidr: &'static str,
    tap_name: &'static str,
    guest_mac: &'static str,
    session_id: &'static str,
    auth_source: &'static str,
    auth_guest_path: &'static str,
    spec_path: &'static str,
}

const FIRECRACKER_HARNESS_PROFILES: &[FirecrackerHarnessProfile] = &[
    FirecrackerHarnessProfile {
        agent_id: "codex-firecracker",
        image_name: "codex",
        harness_arg: "codex",
        host_ip: "172.30.10.1",
        guest_ip: "172.30.10.2",
        cidr: "172.30.10.0/30",
        tap_name: "tap-mat-codex",
        guest_mac: "AA:FC:00:00:10:01",
        session_id: "codex-main",
        auth_source: ".maturana/host-auth/codex",
        auth_guest_path: "/home/ubuntu/.codex",
        spec_path: "examples/MATURANA.codex-firecracker.md",
    },
    FirecrackerHarnessProfile {
        agent_id: "opencode-firecracker",
        image_name: "opencode",
        harness_arg: "opencode",
        host_ip: "172.30.10.5",
        guest_ip: "172.30.10.6",
        cidr: "172.30.10.4/30",
        tap_name: "tap-mat-open",
        guest_mac: "AA:FC:00:00:10:02",
        session_id: "opencode-main",
        auth_source: ".maturana/host-auth/opencode",
        auth_guest_path: "/home/ubuntu",
        spec_path: "examples/MATURANA.opencode-firecracker.md",
    },
    FirecrackerHarnessProfile {
        agent_id: "claude-firecracker",
        image_name: "claude",
        harness_arg: "claude-code",
        host_ip: "172.30.10.9",
        guest_ip: "172.30.10.10",
        cidr: "172.30.10.8/30",
        tap_name: "tap-mat-claude",
        guest_mac: "AA:FC:00:00:10:03",
        session_id: "claude-main",
        auth_source: ".maturana/host-auth/claude-code",
        auth_guest_path: "/home/ubuntu/.claude",
        spec_path: "examples/MATURANA.claude-firecracker.md",
    },
];

fn repair_firecracker_harnesses(
    home: &MaturanaHome,
    repair: FirecrackerHarnessRepair,
) -> anyhow::Result<()> {
    if cfg!(windows) {
        anyhow::bail!("firecracker harness repair requires a Linux host");
    }

    let selected = selected_firecracker_profiles(&repair.agent_ids)?;
    let sessiond_token_path = absolute_or_cwd(repair.sessiond_token_path.clone())?;
    let ssh_key = absolute_or_cwd(repair.ssh_key.clone())?;
    let sessiond_token = ensure_sessiond_token(&sessiond_token_path)?;
    start_linux_sessiond(
        home,
        &repair.sessiond_bind,
        &sessiond_token,
        &sessiond_token_path,
    )?;

    // MaturanaGraph is opt-in: only stand up the host graph service when at
    // least one selected agent enabled `knowledge_graph`. Ensuring the token
    // before rendering guest artifacts is what makes `read_graph_token` inject
    // the graph env into those agents' guests.
    let graph_opt_in = selected.iter().any(|profile| {
        AgentSpec::from_maturana_markdown(&PathBuf::from(profile.spec_path))
            .map(|spec| spec.knowledge_graph.enabled)
            .unwrap_or(false)
    });
    if graph_opt_in {
        let graph_token = ensure_graph_token(home)?;
        start_linux_graph(home, GRAPH_BIND, &graph_token)?;
    }

    for profile in selected {
        println!("=== {} ===", profile.agent_id);
        if !repair.skip_launch {
            let _ = stop_agent(home, profile.agent_id);
        }
        if !repair.skip_assets {
            setup_firecracker_tap(profile)?;
            prepare_firecracker_assets(
                home,
                profile,
                &ssh_key,
                &sessiond_token,
                bind_port(&repair.sessiond_bind)?,
            )?;
        }

        let spec_path = PathBuf::from(profile.spec_path);
        validate_and_materialize_firecracker_spec(home, &spec_path, !repair.skip_launch)?;

        if !repair.skip_worker_refresh {
            // Record the rootfs's baked host public key so SSH verifies the guest
            // (falls back to accept-new if the image predates host-key pinning).
            pin_firecracker_host_key(home, profile)?;
            let host_key = GuestHostKey::resolve(home, profile.agent_id, profile.guest_ip)?;
            wait_for_guest_ssh(
                profile.guest_ip,
                "ubuntu",
                &ssh_key,
                &host_key,
                Duration::from_secs(repair.ssh_wait_seconds),
            )?;
            install_guest_worker(
                home,
                GuestWorkerInstall {
                    agent_id: profile.agent_id.to_string(),
                    session_id: profile.session_id.to_string(),
                    harness: parse_harness_runtime(profile.harness_arg)?,
                    guest_ip: profile.guest_ip.to_string(),
                    ssh_user: "ubuntu".to_string(),
                    ssh_key: ssh_key.clone(),
                    harness_auth_guest_path: profile.auth_guest_path.to_string(),
                    sessiond_url: format!(
                        "http://{}:{}",
                        profile.host_ip,
                        bind_port(&repair.sessiond_bind)?
                    ),
                    sessiond_token_path: sessiond_token_path.clone(),
                    auth_source: Some(PathBuf::from(profile.auth_source)),
                    install_harness: repair.install_harness,
                },
            )?;
        }
    }

    println!("Firecracker harness repair complete.");
    Ok(())
}

fn selected_firecracker_profiles(
    agent_ids: &[String],
) -> anyhow::Result<Vec<&'static FirecrackerHarnessProfile>> {
    if agent_ids.is_empty() {
        return Ok(FIRECRACKER_HARNESS_PROFILES.iter().collect());
    }
    let mut selected = Vec::new();
    for agent_id in agent_ids {
        let profile = FIRECRACKER_HARNESS_PROFILES
            .iter()
            .find(|profile| profile.agent_id == agent_id)
            .ok_or_else(|| anyhow::anyhow!("unknown Firecracker harness agent: {agent_id}"))?;
        selected.push(profile);
    }
    Ok(selected)
}

fn ensure_sessiond_token(path: &PathBuf) -> anyhow::Result<String> {
    if path.exists() {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect();
    fs::write(path, format!("{token}\n"))?;
    Ok(token)
}

fn start_linux_sessiond(
    home: &MaturanaHome,
    bind: &str,
    token: &str,
    token_path: &PathBuf,
) -> anyhow::Result<()> {
    let _ = ProcessCommand::new("pkill")
        .arg("-f")
        .arg("maturana session serve")
        .status();
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = fs::File::create(logs_dir.join("sessiond-linux.out.log"))?;
    let stderr = fs::File::create(logs_dir.join("sessiond-linux.err.log"))?;
    let child = ProcessCommand::new(std::env::current_exe()?)
        .arg("--home")
        .arg(home.root())
        .arg("session")
        .arg("serve")
        .arg("--bind")
        .arg(bind)
        .arg("--token")
        .arg(token)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("failed to start sessiond")?;
    fs::write(
        home.root().join("sessiond/runner.pid"),
        child.id().to_string(),
    )?;
    thread::sleep(Duration::from_secs(1));
    println!("sessiond pid={} bind={bind}", child.id());
    Ok(())
}

/// Address the MaturanaGraph service binds to on the Linux host. Guests resolve
/// the URL sentinel to `http://<host-gateway>:47835`, so the port is fixed.
const GRAPH_BIND: &str = "0.0.0.0:47835";

/// Ensure the host MaturanaGraph token (`<home>/graph/token`) exists, generating
/// one on first use. Mirrors [`ensure_sessiond_token`]; `read_graph_token` reads
/// the same path to decide whether to inject graph env into guests.
fn ensure_graph_token(home: &MaturanaHome) -> anyhow::Result<String> {
    let path = home.root().join("graph").join("token");
    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect();
    fs::write(&path, format!("{token}\n"))?;
    Ok(token)
}

/// Start the MaturanaGraph host service on Linux, mirroring
/// [`start_linux_sessiond`]: kill any prior instance, bind the fixed port, and
/// record the pid. The service fails closed without a token, so we always pass
/// one.
fn start_linux_graph(home: &MaturanaHome, bind: &str, token: &str) -> anyhow::Result<()> {
    let _ = ProcessCommand::new("pkill")
        .arg("-f")
        .arg("maturana graph serve")
        .status();
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let stdout = fs::File::create(logs_dir.join("graph-linux.out.log"))?;
    let stderr = fs::File::create(logs_dir.join("graph-linux.err.log"))?;
    let child = ProcessCommand::new(std::env::current_exe()?)
        .arg("--home")
        .arg(home.root())
        .arg("graph")
        .arg("serve")
        .arg("--bind")
        .arg(bind)
        .arg("--token")
        .arg(token)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("failed to start graph service")?;
    let graph_dir = home.root().join("graph");
    fs::create_dir_all(&graph_dir)?;
    fs::write(graph_dir.join("runner.pid"), child.id().to_string())?;
    thread::sleep(Duration::from_secs(1));
    println!("graph pid={} bind={bind}", child.id());
    Ok(())
}

fn setup_firecracker_tap(profile: &FirecrackerHarnessProfile) -> anyhow::Result<()> {
    run_checked_process(
        ProcessCommand::new("sudo")
            .arg("./scripts/firecracker-setup-tap.sh")
            .arg(profile.tap_name)
            .arg(format!("{}/30", profile.host_ip))
            .arg(profile.cidr),
        "setup Firecracker TAP",
    )
}

fn prepare_firecracker_assets(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
    ssh_key: &PathBuf,
    sessiond_token: &str,
    sessiond_port: &str,
) -> anyhow::Result<()> {
    let image_dir = PathBuf::from(format!(
        ".maturana/images/firecracker/{}",
        profile.image_name
    ));
    let asset_manifest_path = image_dir.join("asset-manifest.json");
    let spec = AgentSpec::from_maturana_markdown(&PathBuf::from(profile.spec_path))
        .with_context(|| format!("failed to parse {}", profile.spec_path))?;
    let artifacts = render_firecracker_guest_artifacts(
        home,
        profile,
        &spec,
        sessiond_token,
        &format!("http://{}:{}", profile.host_ip, sessiond_port),
    )?;
    let mut command = ProcessCommand::new("sudo");
    command
        .arg("env")
        .arg(format!("MATURANA_AGENT_ID={}", profile.agent_id))
        .arg(format!(
            "MATURANA_SESSIOND_ENV_PATH={}",
            artifacts.sessiond_env.display()
        ))
        .arg(format!(
            "MATURANA_RUN_AGENT_PATH={}",
            artifacts.runner.display()
        ))
        .arg(format!(
            "MATURANA_AGENT_SERVICE_PATH={}",
            artifacts.service.display()
        ))
        .arg(format!(
            "MATURANA_HARNESS_INSTALL_PATH={}",
            artifacts.harness_install.display()
        ))
        .arg(format!(
            "MATURANA_HARNESS_INSTALL_SERVICE_PATH={}",
            artifacts.harness_install_service.display()
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_BOOTSTRAP_PATH={}",
            artifacts.firecracker_bootstrap.display()
        ))
        .arg(format!(
            "MATURANA_NETPLAN_PATH={}",
            artifacts.netplan.display()
        ))
        .arg(format!(
            "MATURANA_CLOUD_CFG_PATH={}",
            artifacts.cloud_cfg.display()
        ))
        .arg(format!("MATURANA_FIRECRACKER_HOST_IP={}", profile.host_ip))
        .arg(format!(
            "MATURANA_FIRECRACKER_GUEST_IP={}",
            profile.guest_ip
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_GUEST_MAC={}",
            profile.guest_mac
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_TAP_NAME={}",
            profile.tap_name
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_ASSET_MANIFEST_PATH={}",
            asset_manifest_path.display()
        ));
    if let Some(proxy_env) = artifacts.proxy_env.as_ref() {
        command.arg(format!("MATURANA_PROXY_ENV_PATH={}", proxy_env.display()));
        let ca_cert = ensure_mitm_ca_cert(home.root())?;
        command.arg(format!("MATURANA_PROXY_CA_CERT_PATH={}", ca_cert.display()));
    }
    command
        .arg("./scripts/firecracker-prepare-assets.sh")
        .arg(&image_dir)
        .arg(ssh_key)
        .arg(profile.auth_source);
    run_checked_process(&mut command, "prepare Firecracker assets")?;
    validate_firecracker_asset_manifest(profile, &asset_manifest_path, &image_dir, ssh_key)
}

/// Copy the image's baked SSH host public key (from the asset manifest) into the
/// agent's state dir so SSH connections to the guest can pin it. No-op (leaving
/// accept-new migration in effect) if the image predates host-key pinning.
fn pin_firecracker_host_key(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
) -> anyhow::Result<()> {
    let manifest_path = PathBuf::from(format!(
        ".maturana/images/firecracker/{}/asset-manifest.json",
        profile.image_name
    ));
    if !manifest_path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: FirecrackerAssetManifest =
        serde_json::from_str(&raw).context("failed to parse Firecracker asset manifest")?;
    if let Some(public_line) = manifest.ssh_host_ed25519_pub.as_deref() {
        let state_dir = home.agent_dir(profile.agent_id).join("state");
        fs::create_dir_all(&state_dir)?;
        fs::write(
            state_dir.join(maturana_core::ssh_pin::HOST_PUBLIC_KEY_FILE),
            format!("{}\n", public_line.trim()),
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct FirecrackerGuestArtifacts {
    sessiond_env: PathBuf,
    runner: PathBuf,
    service: PathBuf,
    harness_install: PathBuf,
    harness_install_service: PathBuf,
    firecracker_bootstrap: PathBuf,
    netplan: PathBuf,
    cloud_cfg: PathBuf,
    proxy_env: Option<PathBuf>,
}

#[derive(Debug, serde::Deserialize)]
struct FirecrackerAssetManifest {
    agent_id: String,
    kernel: PathBuf,
    rootfs: PathBuf,
    ssh_key: PathBuf,
    guest_ip: String,
    host_ip: String,
    guest_mac: String,
    tap_name: String,
    kernel_sha256: String,
    rootfs_sha256: String,
    kernel_bytes: u64,
    rootfs_bytes: u64,
    /// The baked guest SSH host public key, pinned per agent. Optional so images
    /// built before host-key pinning still parse (they fall back to accept-new).
    #[serde(default)]
    ssh_host_ed25519_pub: Option<String>,
}

fn validate_firecracker_asset_manifest(
    profile: &FirecrackerHarnessProfile,
    manifest_path: &Path,
    image_dir: &Path,
    expected_ssh_key: &Path,
) -> anyhow::Result<()> {
    let manifest_path = absolute_or_cwd(manifest_path.to_path_buf())?;
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: FirecrackerAssetManifest =
        serde_json::from_str(&raw).context("failed to parse Firecracker asset manifest")?;
    if manifest.agent_id != profile.agent_id {
        anyhow::bail!(
            "Firecracker asset manifest agent mismatch: expected {}, got {}",
            profile.agent_id,
            manifest.agent_id
        );
    }
    if manifest.guest_ip != profile.guest_ip
        || manifest.host_ip != profile.host_ip
        || !manifest.guest_mac.eq_ignore_ascii_case(profile.guest_mac)
        || manifest.tap_name != profile.tap_name
    {
        anyhow::bail!(
            "Firecracker asset manifest network identity does not match profile {}",
            profile.agent_id
        );
    }
    let image_dir = absolute_or_cwd(image_dir.to_path_buf())?;
    let expected_kernel = image_dir.join("vmlinux.bin");
    let expected_rootfs = image_dir.join("ubuntu-rootfs.ext4");
    assert_manifest_path("kernel", &manifest.kernel, &expected_kernel)?;
    assert_manifest_path("rootfs", &manifest.rootfs, &expected_rootfs)?;
    assert_manifest_path("ssh_key", &manifest.ssh_key, expected_ssh_key)?;
    validate_exact_size_file("kernel", &expected_kernel, manifest.kernel_bytes)?;
    validate_exact_size_file("rootfs", &expected_rootfs, manifest.rootfs_bytes)?;
    validate_nonempty_file("ssh_key", expected_ssh_key)?;
    validate_nonempty_file(
        "ssh public key",
        &PathBuf::from(format!("{}.pub", expected_ssh_key.display())),
    )?;
    validate_elf_file(&expected_kernel)?;
    validate_manifest_sha256("kernel", &expected_kernel, &manifest.kernel_sha256)?;
    validate_manifest_sha256("rootfs", &expected_rootfs, &manifest.rootfs_sha256)?;
    println!(
        "Firecracker assets verified: kernel={} rootfs={} manifest={}",
        expected_kernel.display(),
        expected_rootfs.display(),
        manifest_path.display()
    );
    Ok(())
}

fn assert_manifest_path(name: &str, actual: &Path, expected: &Path) -> anyhow::Result<()> {
    let actual = absolute_or_cwd(actual.to_path_buf())?;
    let expected = absolute_or_cwd(expected.to_path_buf())?;
    if actual != expected {
        anyhow::bail!(
            "Firecracker asset manifest {name} path mismatch: expected {}, got {}",
            expected.display(),
            actual.display()
        );
    }
    Ok(())
}

fn validate_exact_size_file(name: &str, path: &Path, manifest_bytes: u64) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing {name}: {}", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("{name} is empty: {}", path.display());
    }
    if metadata.len() != manifest_bytes {
        anyhow::bail!(
            "{name} size mismatch: manifest={} actual={} path={}",
            manifest_bytes,
            metadata.len(),
            path.display()
        );
    }
    Ok(())
}

fn validate_nonempty_file(name: &str, path: &Path) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing {name}: {}", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("{name} is empty: {}", path.display());
    }
    Ok(())
}

fn validate_elf_file(path: &Path) -> anyhow::Result<()> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    if magic != [0x7f, b'E', b'L', b'F'] {
        anyhow::bail!(
            "Firecracker kernel is not an ELF vmlinux: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_manifest_sha256(name: &str, path: &Path, expected: &str) -> anyhow::Result<()> {
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("Firecracker asset manifest {name} sha256 is invalid");
    }
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected.to_ascii_lowercase() {
        anyhow::bail!(
            "Firecracker asset manifest {name} sha256 mismatch: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

fn render_firecracker_guest_artifacts(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
    spec: &AgentSpec,
    sessiond_token: &str,
    sessiond_url: &str,
) -> anyhow::Result<FirecrackerGuestArtifacts> {
    let artifacts_dir = home
        .root()
        .join("agents")
        .join(profile.agent_id)
        .join("state")
        .join("firecracker-image");
    fs::create_dir_all(&artifacts_dir)?;

    let config = GuestWorkerConfig {
        agent_id: profile.agent_id.to_string(),
        session_id: profile.session_id.to_string(),
        sessiond_url: sessiond_url.to_string(),
        sessiond_token: sessiond_token.to_string(),
        harness: parse_harness_runtime(profile.harness_arg)?,
        harness_auth_guest_path: profile.auth_guest_path.to_string(),
        headless_chrome: spec.browser.headless_chrome,
        graph_token: maturana_core::worker::read_graph_token(home.root()),
        graph_name: spec
            .knowledge_graph
            .enabled
            .then(|| spec.knowledge_graph.graph_name(profile.agent_id)),
    };

    let sessiond_env = artifacts_dir.join("sessiond.env");
    let runner = artifacts_dir.join("run-agent.sh");
    let service = artifacts_dir.join("maturana-agent.service");
    let harness_install = artifacts_dir.join("install-harness.sh");
    let harness_install_service = artifacts_dir.join("maturana-harness-install.service");
    let firecracker_bootstrap = artifacts_dir.join("firecracker-bootstrap.sh");
    let netplan = artifacts_dir.join("50-maturana-firecracker.yaml");
    let cloud_cfg = artifacts_dir.join("99-disable-network-config.cfg");
    let proxy_env_path = artifacts_dir.join("proxy.env");

    fs::write(&sessiond_env, render_session_env(&config))?;
    fs::write(&runner, render_run_agent())?;
    fs::write(
        &harness_install,
        render_harness_install(&config.harness, config.headless_chrome),
    )?;
    fs::write(&harness_install_service, render_harness_install_service())?;
    fs::write(&firecracker_bootstrap, render_firecracker_bootstrap())?;
    fs::write(
        &service,
        render_systemd_service(
            &format!(
                "Maturana {} agent {}",
                profile.harness_arg, profile.agent_id
            ),
            "ubuntu",
        ),
    )?;
    fs::write(
        &netplan,
        render_firecracker_netplan(profile.guest_mac, profile.guest_ip, profile.host_ip),
    )?;
    fs::write(&cloud_cfg, render_firecracker_cloud_cfg())?;
    let proxy_env = if let Some(content) = render_firecracker_proxy_env(
        spec.network
            .proxy
            .as_ref()
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
        spec.network.proxy.as_ref().map(|proxy| proxy.bind.as_str()),
        profile.host_ip,
    )? {
        fs::write(&proxy_env_path, content)?;
        Some(absolute_or_cwd(proxy_env_path)?)
    } else {
        None
    };

    Ok(FirecrackerGuestArtifacts {
        sessiond_env: absolute_or_cwd(sessiond_env)?,
        runner: absolute_or_cwd(runner)?,
        service: absolute_or_cwd(service)?,
        harness_install: absolute_or_cwd(harness_install)?,
        harness_install_service: absolute_or_cwd(harness_install_service)?,
        firecracker_bootstrap: absolute_or_cwd(firecracker_bootstrap)?,
        netplan: absolute_or_cwd(netplan)?,
        cloud_cfg: absolute_or_cwd(cloud_cfg)?,
        proxy_env,
    })
}

fn validate_and_materialize_firecracker_spec(
    home: &MaturanaHome,
    spec_path: &PathBuf,
    apply: bool,
) -> anyhow::Result<()> {
    let raw = fs::read_to_string(spec_path)
        .with_context(|| format!("failed to read {}", spec_path.display()))?;
    let spec = AgentSpec::from_maturana_markdown(spec_path)
        .with_context(|| format!("failed to parse {}", spec_path.display()))?;
    let report = validate_spec(&spec);
    if !report.valid {
        anyhow::bail!("spec validation failed: {}", report.errors.join("; "));
    }
    materialize_agent(&spec, &raw, home, LaunchMode::DryRun)?;
    if apply {
        materialize_agent(&spec, &raw, home, LaunchMode::Apply)?;
    }
    Ok(())
}

fn wait_for_guest_ssh(
    guest_ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if run_ssh_with_stdin(guest_ip, ssh_user, ssh_key, host_key, "echo ok", None).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
    anyhow::bail!(
        "guest SSH did not become reachable at {} within {}s",
        guest_ip,
        timeout.as_secs()
    )
}

fn bind_port(bind: &str) -> anyhow::Result<&str> {
    bind.rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty())
        .ok_or_else(|| anyhow::anyhow!("sessiond bind must include a port: {bind}"))
}

fn run_checked_process(command: &mut ProcessCommand, label: &str) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {label}"))?;
    if !status.success() {
        anyhow::bail!("{label} failed with {status}");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RepairWindowsHarnessConfig {
    agent_ids: Vec<String>,
    session_ids: Vec<String>,
    harnesses: Vec<String>,
    harness_auth_guest_paths: Vec<String>,
    telegram_token_sources: Vec<String>,
}

fn repair_windows_config(
    agent_ids: Vec<String>,
    session_ids: Vec<String>,
    harnesses: Vec<String>,
    harness_auth_guest_paths: Vec<String>,
    telegram_token_sources: Vec<String>,
) -> anyhow::Result<RepairWindowsHarnessConfig> {
    let agent_ids = default_if_empty(agent_ids, &["codex-demo", "opencode-demo", "claude-demo"]);
    let session_ids =
        default_if_empty(session_ids, &["codex-main", "opencode-main", "claude-main"]);
    let harnesses = default_if_empty(harnesses, &["codex", "opencode", "claude-code"]);
    let harness_auth_guest_paths = default_if_empty(
        harness_auth_guest_paths,
        &[
            "/home/ubuntu/.codex",
            "/home/ubuntu",
            "/home/ubuntu/.claude",
        ],
    );
    let telegram_token_sources = default_if_empty(
        telegram_token_sources,
        &[
            "pipelock:telegram/bot-token",
            "pipelock:telegram/opencode-bot-token",
            "pipelock:telegram/claude-bot-token",
        ],
    );

    let expected = agent_ids.len();
    for (name, values) in [
        ("session-id", &session_ids),
        ("harness", &harnesses),
        ("harness-auth-guest-path", &harness_auth_guest_paths),
        ("telegram-token-source", &telegram_token_sources),
    ] {
        if values.len() != expected {
            anyhow::bail!(
                "--{name} count ({}) must match --agent-id count ({expected})",
                values.len()
            );
        }
    }
    for harness in &harnesses {
        if !matches!(harness.as_str(), "codex" | "opencode" | "claude-code") {
            anyhow::bail!("unsupported harness for repair: {harness}");
        }
    }

    Ok(RepairWindowsHarnessConfig {
        agent_ids,
        session_ids,
        harnesses,
        harness_auth_guest_paths,
        telegram_token_sources,
    })
}

fn default_if_empty(values: Vec<String>, defaults: &[&str]) -> Vec<String> {
    if values.is_empty() {
        defaults.iter().map(|value| (*value).to_string()).collect()
    } else {
        values
    }
}

fn repair_windows_harnesses(
    home: &MaturanaHome,
    config: &RepairWindowsHarnessConfig,
    register_tasks: bool,
    skip_guest_worker_refresh: bool,
) -> anyhow::Result<()> {
    if !cfg!(windows) {
        anyhow::bail!("windows harness repair requires a Windows host");
    }

    stop_windows_harness_processes()?;

    let repo_root = std::env::current_dir()?;
    start_windows_sessiond(home, &repo_root, register_tasks)?;

    for index in 0..config.agent_ids.len() {
        let agent_id = &config.agent_ids[index];
        let session_id = &config.session_ids[index];
        let harness = &config.harnesses[index];
        let auth_path = &config.harness_auth_guest_paths[index];
        let token_source = &config.telegram_token_sources[index];

        if !skip_guest_worker_refresh {
            refresh_live_guest_worker(home, agent_id, session_id, harness, auth_path)?;
        }

        start_windows_telegram_channel(
            home,
            &repo_root,
            agent_id,
            session_id,
            token_source,
            register_tasks,
        )?;
    }

    run_doctor(
        home,
        DoctorCommand {
            agent_ids: config.agent_ids.clone(),
            json: false,
            sessiond_url: "http://127.0.0.1:47834".to_string(),
        },
    )
}

fn start_windows_sessiond(
    home: &MaturanaHome,
    repo_root: &Path,
    register_task: bool,
) -> anyhow::Result<()> {
    let sessiond_token_path = home.root().join("sessiond/token");
    let token = ensure_sessiond_token(&sessiond_token_path)?;
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let pid_path = home.root().join("sessiond/runner.pid");
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let args = vec![
        "session".to_string(),
        "serve".to_string(),
        "--bind".to_string(),
        "0.0.0.0:47834".to_string(),
        "--token".to_string(),
        token,
    ];
    let exe = windows_maturana_exe(repo_root)?;
    if register_task {
        register_windows_task(
            "MaturanaSessiond",
            &exe,
            &args,
            repo_root,
            &logs_dir.join("sessiond.out.log"),
            &logs_dir.join("sessiond.err.log"),
        )?;
    }
    start_windows_runner(
        &exe,
        &args,
        repo_root,
        &logs_dir.join("sessiond.out.log"),
        &logs_dir.join("sessiond.err.log"),
        &pid_path,
        "sessiond",
    )
}

fn start_windows_telegram_channel(
    home: &MaturanaHome,
    repo_root: &Path,
    agent_id: &str,
    session_id: &str,
    token_source: &str,
    register_task: bool,
) -> anyhow::Result<()> {
    let safe_agent_id = safe_windows_task_suffix(agent_id);
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let pid_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/runner.pid");
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let args = vec![
        "channel".to_string(),
        "serve".to_string(),
        "telegram".to_string(),
        "--agent-id".to_string(),
        agent_id.to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
        "--token-source".to_string(),
        token_source.to_string(),
    ];
    let exe = windows_maturana_exe(repo_root)?;
    if register_task {
        register_windows_task(
            &format!("MaturanaTelegramChannel-{safe_agent_id}"),
            &exe,
            &args,
            repo_root,
            &logs_dir.join(format!("telegram-channel-{safe_agent_id}.out.log")),
            &logs_dir.join(format!("telegram-channel-{safe_agent_id}.err.log")),
        )?;
    }
    start_windows_runner(
        &exe,
        &args,
        repo_root,
        &logs_dir.join(format!("telegram-channel-{safe_agent_id}.out.log")),
        &logs_dir.join(format!("telegram-channel-{safe_agent_id}.err.log")),
        &pid_path,
        &format!("telegram channel {agent_id}"),
    )
}

fn refresh_live_guest_worker(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    harness: &str,
    harness_auth_guest_path: &str,
) -> anyhow::Result<()> {
    let harness = parse_harness_runtime(harness)?;
    let status = inspect_agent(home, agent_id)?;
    let ip = status.ipv4.ok_or_else(|| {
        anyhow::anyhow!("could not discover live IP for {agent_id}; run live inspect first")
    })?;
    let ssh_user = "ubuntu";
    let ssh_key = default_agent_ssh_key()?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            harness,
            guest_ip: ip,
            ssh_user: ssh_user.to_string(),
            ssh_key,
            harness_auth_guest_path: harness_auth_guest_path.to_string(),
            sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
            sessiond_token_path: home.root().join("sessiond/token"),
            auth_source: None,
            install_harness: false,
        },
    )
}

struct GuestWorkerInstall {
    agent_id: String,
    session_id: String,
    harness: HarnessRuntime,
    guest_ip: String,
    ssh_user: String,
    ssh_key: PathBuf,
    harness_auth_guest_path: String,
    sessiond_url: String,
    sessiond_token_path: PathBuf,
    auth_source: Option<PathBuf>,
    install_harness: bool,
}

fn install_guest_worker(home: &MaturanaHome, install: GuestWorkerInstall) -> anyhow::Result<()> {
    let ssh_key = absolute_or_cwd(install.ssh_key)?;
    let sessiond_token = read_optional_trimmed(absolute_or_cwd(install.sessiond_token_path)?)?;

    let state_dir = home.agent_dir(&install.agent_id).join("state");
    fs::create_dir_all(&state_dir)?;
    let env_path = state_dir.join("sessiond.env");
    let runner_path = state_dir.join("run-agent.sh");
    // The post-boot re-render also carries the graph env so it isn't lost when a
    // worker is refreshed. Read the agent's materialized spec for its opt-in.
    let knowledge_graph = AgentSpec::from_maturana_markdown(
        &home.agent_dir(&install.agent_id).join("MATURANA.md"),
    )
    .ok()
    .map(|spec| spec.knowledge_graph)
    .unwrap_or_default();
    fs::write(
        &env_path,
        render_session_env(&GuestWorkerConfig {
            agent_id: install.agent_id.clone(),
            session_id: install.session_id.clone(),
            sessiond_url: install.sessiond_url.clone(),
            sessiond_token,
            harness: install.harness.clone(),
            harness_auth_guest_path: install.harness_auth_guest_path.clone(),
            headless_chrome: false,
            graph_token: maturana_core::worker::read_graph_token(home.root()),
            graph_name: knowledge_graph
                .enabled
                .then(|| knowledge_graph.graph_name(&install.agent_id)),
        }),
    )?;
    fs::write(&runner_path, render_run_agent())?;

    // Verify the guest's host key (strict if pinned, else accept-new) before
    // pushing the sessiond token and harness credentials over these connections.
    let host_key = GuestHostKey::resolve(home, &install.agent_id, &install.guest_ip)?;

    copy_path_to_guest(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &env_path,
        "/tmp/sessiond.env",
        false,
    )?;
    copy_path_to_guest(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &runner_path,
        "/tmp/run-agent.sh",
        false,
    )?;
    if let Some(auth_source) = install.auth_source.as_ref() {
        let auth_source = absolute_or_cwd(auth_source.clone())?;
        if auth_source.exists() {
            copy_path_to_guest(
                &install.guest_ip,
                &install.ssh_user,
                &ssh_key,
                &host_key,
                &auth_source,
                "/tmp/maturana-harness-auth",
                true,
            )?;
        }
    }
    if install.install_harness {
        run_ssh_with_stdin(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            &render_harness_install(&install.harness, false),
            None,
        )?;
    }
    if install.auth_source.is_some() {
        run_ssh_with_stdin(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            &render_auth_install_command(
                &install.harness,
                &install.ssh_user,
                &install.harness_auth_guest_path,
            ),
            None,
        )?;
    }
    run_ssh_with_stdin(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &format!(
            "sudo mkdir -p /agent /opt/maturana/bin /var/log/maturana /workspace /memory /wiki && sudo mv /tmp/sessiond.env /agent/sessiond.env && sudo mv /tmp/run-agent.sh /opt/maturana/bin/run-agent.sh && sudo chown {user}:{user} /agent/sessiond.env /opt/maturana/bin/run-agent.sh && sudo chmod 0600 /agent/sessiond.env && sudo chmod 0755 /opt/maturana/bin/run-agent.sh && sudo systemctl restart maturana-agent.service",
            user = shell_quote(&install.ssh_user)
        ),
        None,
    )?;
    println!(
        "refreshed {} worker at {}",
        install.agent_id, install.guest_ip
    );
    Ok(())
}

fn render_auth_install_command(
    harness: &HarnessRuntime,
    ssh_user: &str,
    harness_auth_guest_path: &str,
) -> String {
    let user = shell_quote(ssh_user);
    let guest_path = shell_quote(harness_auth_guest_path);
    match harness {
        HarnessRuntime::Opencode => format!(
            "sudo mkdir -p {guest_path} && sudo cp -a /tmp/maturana-harness-auth/. {guest_path}/ && sudo rm -rf /tmp/maturana-harness-auth && sudo chown -R {user}:{user} {guest_path} && chmod -R go-rwx {guest_path}/.config {guest_path}/.local {guest_path}/.maturana-env 2>/dev/null || true"
        ),
        _ => {
            let parent = shell_quote(posix_parent(harness_auth_guest_path));
            format!(
                "sudo mkdir -p {parent} && sudo rm -rf {guest_path} && sudo mv /tmp/maturana-harness-auth {guest_path} && sudo chown -R {user}:{user} {guest_path} && chmod -R go-rwx {guest_path}"
            )
        }
    }
}

fn posix_parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((parent, _)) if !parent.is_empty() => parent,
        _ => ".",
    }
}

fn parse_harness_runtime(harness: &str) -> anyhow::Result<HarnessRuntime> {
    match harness {
        "codex" => Ok(HarnessRuntime::Codex),
        "claude-code" => Ok(HarnessRuntime::ClaudeCode),
        "opencode" => Ok(HarnessRuntime::Opencode),
        _ => anyhow::bail!("unsupported harness: {harness}"),
    }
}

fn default_agent_ssh_key() -> anyhow::Result<PathBuf> {
    std::env::var("MATURANA_AGENT_SSH_KEY")
        .map(PathBuf::from)
        .or_else(|_| {
            Ok::<PathBuf, std::env::VarError>(PathBuf::from(
                ".maturana/keys/maturana-agent-ed25519",
            ))
        })
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            }
        })
        .map_err(Into::into)
}

fn read_optional_trimmed(path: PathBuf) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn stop_windows_harness_processes() -> anyhow::Result<()> {
    let home = MaturanaHome::default_for_cwd(&std::env::current_dir()?);
    stop_windows_pid_file(&home.root().join("sessiond/runner.pid"), "sessiond")?;
    let agents_dir = home.agents_dir();
    if agents_dir.exists() {
        for entry in fs::read_dir(agents_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stop_windows_pid_file(
                    &entry.path().join("channels/telegram/runner.pid"),
                    &format!("telegram channel {}", entry.file_name().to_string_lossy()),
                )?;
            }
        }
    }
    Ok(())
}

fn stop_windows_pid_file(path: &Path, label: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let pid = fs::read_to_string(path)?.trim().to_string();
    if pid.is_empty() {
        return Ok(());
    }
    let status = ProcessCommand::new("taskkill")
        .arg("/PID")
        .arg(&pid)
        .arg("/F")
        .status()
        .with_context(|| format!("failed to stop {label} pid {pid}"))?;
    if !status.success() {
        eprintln!("warning: failed to stop {label} pid {pid}: {status}");
    } else {
        println!("stopped {label} pid={pid}");
    }
    let _ = fs::remove_file(path);
    Ok(())
}

fn start_windows_runner(
    exe: &Path,
    args: &[String],
    repo_root: &Path,
    log_path: &Path,
    err_path: &Path,
    pid_path: &Path,
    label: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = err_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = fs::File::create(log_path)?;
    let stderr = fs::File::create(err_path)?;
    let child = ProcessCommand::new(exe)
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("failed to start {label}"))?;
    fs::write(pid_path, child.id().to_string())?;
    println!("started {label} pid={}", child.id());
    Ok(())
}

fn register_windows_task(
    task_name: &str,
    exe: &Path,
    args: &[String],
    repo_root: &Path,
    log_path: &Path,
    err_path: &Path,
) -> anyhow::Result<()> {
    let mut command = format!(
        "cmd.exe /c \"cd /d {} && {} {} >> {} 2>> {}\"",
        quote_cmd_arg(repo_root),
        quote_cmd_arg(exe),
        args.iter()
            .map(|arg| quote_cmd_arg(arg))
            .collect::<Vec<_>>()
            .join(" "),
        quote_cmd_arg(log_path),
        quote_cmd_arg(err_path)
    );
    if command.len() > 260 {
        command = format!(
            "{} {}",
            quote_cmd_arg(exe),
            args.iter()
                .map(|arg| quote_cmd_arg(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let status = ProcessCommand::new("schtasks")
        .arg("/Create")
        .arg("/TN")
        .arg(task_name)
        .arg("/SC")
        .arg("ONLOGON")
        .arg("/TR")
        .arg(command)
        .arg("/F")
        .status()
        .with_context(|| format!("failed to register Windows task {task_name}"))?;
    if !status.success() {
        eprintln!("warning: could not register Windows task {task_name}: {status}");
    }
    Ok(())
}

fn windows_maturana_exe(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    if current.exists() {
        return Ok(current);
    }
    for path in [
        repo_root.join("target/x86_64-pc-windows-msvc/debug/maturana.exe"),
        repo_root.join("target/x86_64-pc-windows-gnu/debug/maturana.exe"),
        repo_root.join("target/debug/maturana.exe"),
    ] {
        if path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("maturana.exe not found; build the Windows binary first")
}

fn safe_windows_task_suffix(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn quote_cmd_arg(value: impl AsRef<Path>) -> String {
    let raw = value.as_ref().display().to_string();
    format!("\"{}\"", raw.replace('"', "\\\""))
}

fn print_doctor_check(label: &str, check: &DoctorCheck) {
    println!("{label}.ok: {}", check.ok);
    if !check.message.is_empty() {
        println!("{label}.message: {}", check.message);
    }
}

fn discover_agent_ids(home: &MaturanaHome) -> anyhow::Result<Vec<String>> {
    let agents_dir = home.agents_dir();
    if !agents_dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(agents_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            ids.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    ids.sort();
    Ok(ids)
}

fn doctor_hostd() -> DoctorCheck {
    match hostd_status() {
        Ok(status) if status.reachable => DoctorCheck {
            ok: true,
            message: status.url,
        },
        Ok(status) => DoctorCheck {
            ok: false,
            message: status.error.unwrap_or(status.url),
        },
        Err(error) => DoctorCheck {
            ok: false,
            message: error.to_string(),
        },
    }
}

fn doctor_http_health(url: &str) -> DoctorCheck {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();
    match agent.get(url).call() {
        Ok(response) => match response.into_json::<serde_json::Value>() {
            Ok(payload)
                if payload
                    .get("ok")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false) =>
            {
                DoctorCheck {
                    ok: true,
                    message: url.to_string(),
                }
            }
            Ok(payload) => DoctorCheck {
                ok: false,
                message: format!("unexpected payload from {url}: {payload}"),
            },
            Err(error) => DoctorCheck {
                ok: false,
                message: format!("invalid JSON from {url}: {error}"),
            },
        },
        Err(error) => DoctorCheck {
            ok: false,
            message: format!("{url}: {error}"),
        },
    }
}

fn doctor_vms() -> anyhow::Result<Vec<serde_json::Value>> {
    let response = hostd_get("/vms")?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd /vms returned an error: {payload}");
    }
    Ok(payload
        .get("vms")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default())
}

fn doctor_agent(
    home: &MaturanaHome,
    agent_id: &str,
    vms: &[serde_json::Value],
) -> DoctorAgentReport {
    DoctorAgentReport {
        agent_id: agent_id.to_string(),
        vm: doctor_agent_vm(agent_id, vms),
        telegram: doctor_agent_telegram(home, agent_id),
        guest_worker: doctor_guest_worker(home, agent_id),
    }
}

fn doctor_agent_vm(agent_id: &str, vms: &[serde_json::Value]) -> DoctorCheck {
    let expected_name = format!("maturana-{agent_id}");
    let Some(vm) = vms.iter().find(|vm| {
        vm.get("name")
            .and_then(|value| value.as_str())
            .map(|name| name == expected_name)
            .unwrap_or(false)
    }) else {
        return DoctorCheck {
            ok: false,
            message: format!("{expected_name} not found"),
        };
    };
    let state = vm
        .get("state")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ip = vm
        .get("ipv4")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ok = state.eq_ignore_ascii_case("Running") && !ip.trim().is_empty();
    DoctorCheck {
        ok,
        message: format!("state={state} ipv4={ip}"),
    }
}

fn doctor_agent_telegram(home: &MaturanaHome, agent_id: &str) -> DoctorCheck {
    let vault = PipelockVault::new(home.pipelock_dir());
    let paired = vault.get(&format!("telegram/{agent_id}/chat-id")).is_ok()
        || vault.get("telegram/chat-id").is_ok();
    let pid_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/runner.pid");
    let pid = read_pid(&pid_path);
    let pid_alive = pid.map(process_alive).unwrap_or(false);
    let state_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/state.json");
    let state_age = file_age_seconds(&state_path);
    let heartbeat_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/heartbeat.json");
    let heartbeat_age = file_age_seconds(&heartbeat_path);
    let heartbeat_ok = heartbeat_age.map(|age| age <= 30).unwrap_or(false);
    let ok = paired && (pid_alive || heartbeat_ok);
    DoctorCheck {
        ok,
        message: format!(
            "paired={paired} pid={} pid_alive={pid_alive} state_age_s={} heartbeat_age_s={}",
            pid.map(|value| value.to_string()).unwrap_or_default(),
            state_age
                .map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string()),
            heartbeat_age
                .map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string())
        ),
    }
}

fn doctor_guest_worker(home: &MaturanaHome, agent_id: &str) -> DoctorCheck {
    let path = home.agent_dir(agent_id).join("worker-status.json");
    let age = file_age_seconds(&path);
    let payload = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let status = payload
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ok = age.map(|value| value <= 60).unwrap_or(false)
        && (status.is_empty() || status == "idle" || status == "completed" || status == "claimed");
    DoctorCheck {
        ok,
        message: format!(
            "status={} age_s={}",
            if status.is_empty() { "unknown" } else { status },
            age.map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string())
        ),
    }
}

fn read_pid(path: &PathBuf) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()
}

fn file_age_seconds(path: &PathBuf) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age.as_secs())
}

fn process_alive(pid: u32) -> bool {
    process_alive_impl(pid).unwrap_or(false)
}

#[cfg(windows)]
fn process_alive_impl(pid: u32) -> anyhow::Result<bool> {
    let status = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "try {{ Get-Process -Id {pid} -ErrorAction Stop | Out-Null; exit 0 }} catch {{ exit 1 }}"
            ),
        ])
        .status()?;
    Ok(status.success())
}

#[cfg(not(windows))]
fn process_alive_impl(pid: u32) -> anyhow::Result<bool> {
    let status = ProcessCommand::new("sh")
        .args(["-c", &format!("kill -0 {pid} >/dev/null 2>&1")])
        .status()?;
    Ok(status.success())
}

fn print_report(report: &maturana_core::ValidationReport) {
    if report.valid {
        println!("valid");
    } else {
        println!("invalid");
    }

    for error in &report.errors {
        println!("error: {error}");
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

fn print_snapshot_record(snapshot: &SnapshotRecord) {
    println!(
        "{} {:?} {:?} {}",
        snapshot.name, snapshot.provider, snapshot.kind, snapshot.created_at
    );
    if let Some(path) = &snapshot.state_path {
        println!("  state: {}", path.display());
    }
    if let Some(path) = &snapshot.memory_path {
        println!("  memory: {}", path.display());
    }
    if let Some(path) = &snapshot.disk_path {
        println!("  disk: {}", path.display());
    }
}

fn print_live_agent_status(status: &LiveAgentStatus) {
    println!("live.provider: {}", status.provider);
    println!("live.state: {}", status.state);
    if let Some(vm_name) = &status.vm_name {
        println!("live.vm: {vm_name}");
    }
    if let Some(pid) = status.pid {
        println!("live.pid: {pid}");
    }
    if let Some(ipv4) = &status.ipv4 {
        println!("live.ipv4: {ipv4}");
    }
    if let Some(uptime) = &status.uptime {
        println!("live.uptime: {uptime}");
    }
    if let Some(path) = &status.socket_path {
        println!("live.socket: {}", path.display());
    }
    if let Some(path) = &status.config_path {
        println!("live.config: {}", path.display());
    }
    if let Some(path) = &status.metadata_path {
        println!("live.metadata: {}", path.display());
    }
    if !status.metrics_tail.is_empty() {
        println!("live.metrics_tail:");
        for line in &status.metrics_tail {
            println!("{line}");
        }
    }
}

fn send_telegram(token: &str, chat_id: &str, message: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
    });

    let request = ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
        .set("content-type", "application/json")
        .send_string(&body.to_string());

    match request {
        Ok(_) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("Telegram notification failed: {error}")),
    }
}

fn send_discord(webhook: &str, message: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "content": message,
    });

    let request = ureq::post(webhook)
        .set("content-type", "application/json")
        .send_string(&body.to_string());

    match request {
        Ok(_) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("Discord notification failed: {error}")),
    }
}

fn audit_agent_event(
    home: &MaturanaHome,
    agent_id: &str,
    action: &str,
    message: impl Into<String>,
) -> anyhow::Result<()> {
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            message: message.into(),
        },
    )
}

fn snapshot_audit_event(operation: &str, live: bool, failed: bool) -> &'static str {
    match (operation, live, failed) {
        ("list", true, false) => "snapshot.list.live",
        ("list", false, false) => "snapshot.list.local",
        ("list", true, true) => "snapshot.list.live.failed",
        ("list", false, true) => "snapshot.list.local.failed",
        ("take", true, false) => "snapshot.take.live",
        ("take", false, false) => "snapshot.take.local",
        ("take", true, true) => "snapshot.take.live.failed",
        ("take", false, true) => "snapshot.take.local.failed",
        ("restore", _, false) => "snapshot.restore.live",
        ("restore", _, true) => "snapshot.restore.live.failed",
        _ => "snapshot.unknown.failed",
    }
}

fn read_agent_audit_events(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<Vec<AuditEvent>> {
    let path = home.audit_dir().join(format!("{agent_id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read audit log {}", path.display()))?;
    let mut events = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: AuditEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse audit log {} at line {}",
                path.display(),
                index + 1
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

#[derive(Debug, serde::Serialize)]
struct HostdStatus {
    url: String,
    reachable: bool,
    token_present: bool,
    error: Option<String>,
}

fn hostd_status() -> anyhow::Result<HostdStatus> {
    let health_url = hostd_url("/health");
    let token_present = hostd_token()?.is_some();
    match ureq::get(&health_url).call() {
        Ok(response) => {
            let payload: serde_json::Value = response.into_json()?;
            let reachable = payload
                .get("ok")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Ok(HostdStatus {
                url: health_url,
                reachable,
                token_present,
                error: if reachable {
                    None
                } else {
                    Some(format!("unexpected health payload: {payload}"))
                },
            })
        }
        Err(error) => Ok(HostdStatus {
            url: health_url,
            reachable: false,
            token_present,
            error: Some(error.to_string()),
        }),
    }
}

fn live_agent_ip(agent_id: &str) -> anyhow::Result<Option<String>> {
    let response = hostd_get("/vms")?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd /vms returned an error: {payload}");
    }

    let expected_name = format!("maturana-{agent_id}");
    let ip = payload
        .get("vms")
        .and_then(|value| value.as_array())
        .and_then(|vms| {
            vms.iter().find_map(|vm| {
                let name = vm.get("name").and_then(|value| value.as_str())?;
                if name != expected_name {
                    return None;
                }
                vm.get("ipv4")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(ToString::to_string)
            })
        });
    Ok(ip)
}

fn print_live_guest_state(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    headless_chrome: bool,
) -> anyhow::Result<()> {
    let remote = render_live_guest_state_script(headless_chrome);
    let output = run_ssh_with_stdin(ip, ssh_user, ssh_key, host_key, &remote, None)?;
    print!("{output}");
    Ok(())
}

fn render_live_guest_state_script(headless_chrome: bool) -> String {
    let browser_probe = if headless_chrome {
        r#"echo "live.browser_expected: true"
echo "live.browser_smoke:"
if [ -f /opt/maturana/bin/browser-smoke.js ]; then
  PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/opt/maturana/browsers}" node /opt/maturana/bin/browser-smoke.js 2>&1 | sed 's/^/live.browser_smoke_output: /' || true
else
  echo "live.browser_smoke_output: missing /opt/maturana/bin/browser-smoke.js"
fi
"#
    } else {
        r#"echo "live.browser_expected: false"
"#
    };
    format!(
        r#"set -eu
echo "live.guest_ip: $(hostname -I 2>/dev/null | awk '{{print $1}}')"
echo "live.guest: $(hostname)"
echo "live.codex: $(command -v codex 2>/dev/null || true)"
codex --version 2>/dev/null | sed 's/^/live.codex_version: /' || true
echo "live.claude: $(command -v claude 2>/dev/null || command -v claude-code 2>/dev/null || true)"
(claude --version 2>/dev/null || claude-code --version 2>/dev/null || true) | sed 's/^/live.claude_version: /' || true
echo "live.opencode: $(command -v opencode 2>/dev/null || true)"
opencode --version 2>/dev/null | sed 's/^/live.opencode_version: /' || true
echo "live.service: $(systemctl is-active maturana-agent.service 2>/dev/null || true)"
echo "live.rootfs: $(df -h / | awk 'NR==2 {{print $2 " total, " $4 " free"}}')"
echo "live.heartbeat: $(cat /var/log/maturana/heartbeat 2>/dev/null || true)"
echo "live.last_message:"
cat /var/log/maturana/last-message.txt 2>/dev/null || true
echo "live.agent_log_tail:"
tail -n 20 /var/log/maturana/agent.log 2>/dev/null || true
{browser_probe}"#
    )
}

fn read_agent_prompt(
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    match (prompt, prompt_file) {
        (Some(prompt), None) if !prompt.trim().is_empty() => Ok(prompt),
        (None, Some(path)) => {
            let prompt = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt file is empty: {}", path.display());
            }
            Ok(prompt)
        }
        (Some(_), Some(_)) => anyhow::bail!("pass either --prompt or --prompt-file, not both"),
        _ => anyhow::bail!("agent run requires --prompt or --prompt-file"),
    }
}

#[derive(Debug, Clone)]
struct QueuedAgentRun {
    session_id: String,
    message_id: String,
}

#[derive(Debug, Clone)]
struct CompletedAgentRun {
    message_id: String,
    text: String,
}

fn enqueue_agent_run(
    home: &MaturanaHome,
    agent_id: &str,
    prompt: &str,
) -> anyhow::Result<QueuedAgentRun> {
    let session_id = infer_agent_session_id(home, agent_id)?;
    let paths = session_paths(&home.agent_dir(agent_id), &session_id);
    ensure_session(&paths)?;
    let content = serde_json::json!({
        "text": prompt,
        "prompt": prompt,
    })
    .to_string();
    let message_id = insert_inbound(&paths, "chat", "cli", "agent-run", None, &content)?;
    Ok(QueuedAgentRun {
        session_id,
        message_id,
    })
}

fn wait_for_agent_run(
    home: &MaturanaHome,
    agent_id: &str,
    queued: &QueuedAgentRun,
    timeout_seconds: u64,
) -> anyhow::Result<CompletedAgentRun> {
    let paths = session_paths(&home.agent_dir(agent_id), &queued.session_id);
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        let outbox = list_undelivered(&paths)?;
        if let Some(message) = outbox
            .into_iter()
            .find(|message| message.in_reply_to.as_deref() == Some(&queued.message_id))
        {
            let text = outbound_message_text(&message.content)?;
            mark_delivered(&paths, &message.id, Some("agent-run"))?;
            return Ok(CompletedAgentRun {
                message_id: message.id,
                text,
            });
        }
        thread::sleep(Duration::from_secs(1));
    }
    anyhow::bail!(
        "timed out waiting for response to {} in session {}",
        queued.message_id,
        queued.session_id
    )
}

fn infer_agent_session_id(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<String> {
    let env_path = home.agent_dir(agent_id).join("state/sessiond.env");
    if env_path.exists() {
        let raw = fs::read_to_string(&env_path)
            .with_context(|| format!("failed to read {}", env_path.display()))?;
        if let Some(session_id) = session_env_value(&raw, "MATURANA_SESSION_ID") {
            return Ok(session_id);
        }
    }

    match agent_id {
        "codex-demo" | "codex-firecracker" => Ok("codex-main".to_string()),
        "opencode-demo" | "opencode-firecracker" => Ok("opencode-main".to_string()),
        "claude-demo" | "claude-firecracker" => Ok("claude-main".to_string()),
        _ => Ok("telegram-main".to_string()),
    }
}

fn session_env_value(raw: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    raw.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(unquote_shell_env_value)
        .filter(|value| !value.trim().is_empty())
}

fn unquote_shell_env_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        trimmed[1..trimmed.len() - 1]
            .replace("'\"'\"'", "'")
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn outbound_message_text(content: &str) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .with_context(|| format!("outbound message content was not JSON: {content}"))?;
    value
        .get("text")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("outbound message did not contain text"))
}

fn read_live_log(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    kind: LogKind,
    lines: u16,
) -> anyhow::Result<String> {
    let path = kind.guest_path();
    let command = if matches!(kind, LogKind::LastMessage) {
        format!("test -f {path} && cat {path} || true")
    } else {
        format!("test -f {path} && tail -n {} {path} || true", lines.max(1))
    };
    run_ssh_with_stdin(ip, ssh_user, ssh_key, host_key, &command, None)
}

impl LogKind {
    fn as_str(self) -> &'static str {
        match self {
            LogKind::Agent => "agent",
            LogKind::Error => "error",
            LogKind::Stdout => "stdout",
            LogKind::Stderr => "stderr",
            LogKind::LastMessage => "last-message",
        }
    }

    fn guest_path(self) -> &'static str {
        match self {
            LogKind::Agent => "/var/log/maturana/agent.log",
            LogKind::Error => "/var/log/maturana/agent.err.log",
            LogKind::Stdout => "/var/log/maturana/harness.out.log",
            LogKind::Stderr => "/var/log/maturana/harness.err.log",
            LogKind::LastMessage => "/var/log/maturana/last-message.txt",
        }
    }
}

fn fetch_live_path(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    remote_path: &str,
    local_path: &PathBuf,
    allowed_roots: &[String],
    recursive: bool,
) -> anyhow::Result<()> {
    validate_guest_transfer_path(remote_path, allowed_roots)?;
    if let Some(parent) = local_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut command = ProcessCommand::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PreferredAuthentications=publickey")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    command
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .arg(local_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command.output().context("failed to start scp")?;
    if !output.status.success() {
        anyhow::bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn push_live_path(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    local_path: &PathBuf,
    remote_path: &str,
    allowed_roots: &[String],
    recursive: bool,
) -> anyhow::Result<()> {
    if !local_path.exists() {
        anyhow::bail!("local path does not exist: {}", local_path.display());
    }
    validate_guest_transfer_path(remote_path, allowed_roots)?;

    if let Some(parent) = remote_parent(remote_path) {
        let mkdir = format!("mkdir -p {}", shell_quote(&parent));
        run_ssh_with_stdin(ip, ssh_user, ssh_key, host_key, &mkdir, None)?;
    }

    let mut command = ProcessCommand::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    command
        .arg(local_path)
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command.output().context("failed to start scp")?;
    if !output.status.success() {
        anyhow::bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn agent_transfer_roots(
    home: &MaturanaHome,
    agent_id: &str,
    writable_only: bool,
) -> anyhow::Result<Vec<String>> {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    let mut roots = default_guest_transfer_roots();
    if spec_path.exists() {
        let spec = AgentSpec::from_maturana_markdown(&spec_path)
            .with_context(|| format!("failed to parse {}", spec_path.display()))?;
        for mount in spec.filesystem.mounts {
            if writable_only && !mount.writable {
                continue;
            }
            if let Some(root) = normalize_guest_transfer_root(&mount.guest_path) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
    }
    Ok(roots)
}

fn default_guest_transfer_roots() -> Vec<String> {
    vec![
        "/workspace".to_string(),
        "/memory".to_string(),
        "/wiki".to_string(),
    ]
}

fn normalize_guest_transfer_root(root: &str) -> Option<String> {
    let root = root.trim().trim_end_matches('/');
    if root.is_empty() || root == "/" || !root.starts_with('/') {
        return None;
    }
    if root.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(root.to_string())
}

fn validate_guest_transfer_path(remote_path: &str, allowed_roots: &[String]) -> anyhow::Result<()> {
    let path = remote_path.trim();
    if path.is_empty() {
        anyhow::bail!("remote path must not be empty");
    }
    if !path.starts_with('/') {
        anyhow::bail!("remote path must be absolute: {path}");
    }
    if path.split('/').any(|segment| segment == "..") {
        anyhow::bail!("remote path must not contain '..': {path}");
    }
    if !is_allowed_guest_transfer_path(path, allowed_roots) {
        anyhow::bail!(
            "remote path is outside allowed guest transfer roots ({}): {path}",
            allowed_roots.join(", ")
        );
    }
    Ok(())
}

fn is_allowed_guest_transfer_path(path: &str, allowed_roots: &[String]) -> bool {
    allowed_roots
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{root}/")))
}

fn remote_parent(remote_path: &str) -> Option<String> {
    let trimmed = remote_path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    if parent.is_empty() {
        Some("/".to_string())
    } else {
        Some(parent.to_string())
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn run_ssh_with_stdin(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    remote_command: &str,
    stdin_text: Option<&str>,
) -> anyhow::Result<String> {
    let mut command = ProcessCommand::new("ssh");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key)
        .arg(format!("{ssh_user}@{ip}"))
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_text.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command.spawn().context("failed to start ssh")?;
    if let Some(stdin_text) = stdin_text {
        let mut stdin = child.stdin.take().context("failed to open ssh stdin")?;
        stdin.write_all(stdin_text.as_bytes())?;
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ssh timed out after 30 seconds");
        }
        thread::sleep(Duration::from_millis(100));
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn copy_path_to_guest(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    local_path: &PathBuf,
    remote_path: &str,
    recursive: bool,
) -> anyhow::Result<()> {
    let mut command = ProcessCommand::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    let output = command
        .arg(local_path)
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start scp for {}", local_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Per-agent SSH host-key verification material: which known_hosts file to use
/// and whether to verify strictly (a pinned key is recorded) or `accept-new`
/// (a guest provisioned before pinning existed — trust-on-first-use migration).
struct GuestHostKey {
    known_hosts: PathBuf,
    strict: bool,
}

impl GuestHostKey {
    fn resolve(home: &MaturanaHome, agent_id: &str, ip: &str) -> anyhow::Result<Self> {
        let state_dir = home.agent_dir(agent_id).join("state");
        let (known_hosts, strict) = maturana_core::ssh_pin::prepare_known_hosts(&state_dir, ip)?;
        Ok(Self {
            known_hosts,
            strict,
        })
    }

    fn options(&self) -> Vec<String> {
        maturana_core::ssh_pin::ssh_host_key_options(&self.known_hosts, self.strict)
    }
}

#[derive(Debug)]
struct HostdHttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, serde::Deserialize)]
struct HostdLaunchBody {
    agent_id: Option<String>,
    harness: Option<String>,
    base_vhdx_path: Option<PathBuf>,
    switch_name: Option<String>,
    ssh_user: Option<String>,
    ssh_key_path: Option<PathBuf>,
    cloud_init_user_data_path: Option<PathBuf>,
    cloud_init_meta_data_path: Option<PathBuf>,
    disk_size_gb: Option<u32>,
    vcpu: Option<u8>,
    memory_mib: Option<u32>,
    provision_existing: Option<bool>,
    force: Option<bool>,
}

#[derive(Debug)]
struct HostdRouteResponse {
    status: u16,
    body: serde_json::Value,
}

fn run_hostd_server(bind_prefix: &str, token_path: &Path, log_path: &Path) -> anyhow::Result<()> {
    if !cfg!(windows) {
        anyhow::bail!("hostd serve is only supported on Windows hosts");
    }
    assert_windows_elevated()?;
    let token_path = absolute_or_cwd(token_path.to_path_buf())?;
    let log_path = absolute_or_cwd(log_path.to_path_buf())?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token = ensure_hostd_server_token(&token_path)?;
    let bind = parse_hostd_bind_prefix(bind_prefix)?;
    hostd_log(
        &log_path,
        &format!("maturana rust hostd listening on {bind_prefix}"),
    )?;
    let listener = TcpListener::bind(bind).with_context(|| format!("failed to bind {bind}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_hostd_stream(&mut stream, &token, &log_path) {
                    let _ = hostd_log(&log_path, &format!("request failed: {error:#}"));
                    let _ = write_hostd_json(
                        &mut stream,
                        500,
                        serde_json::json!({ "ok": false, "error": error.to_string() }),
                    );
                }
            }
            Err(error) => {
                hostd_log(&log_path, &format!("accept failed: {error}"))?;
            }
        }
    }
    Ok(())
}

fn assert_windows_elevated() -> anyhow::Result<()> {
    let script = "$identity=[Security.Principal.WindowsIdentity]::GetCurrent();$principal=[Security.Principal.WindowsPrincipal]::new($identity);if($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){exit 0}else{exit 1}";
    let status = ProcessCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .status()
        .context("failed to check Windows elevation")?;
    if !status.success() {
        anyhow::bail!("Run maturana hostd serve from an elevated shell or scheduled task");
    }
    Ok(())
}

fn ensure_hostd_server_token(path: &Path) -> anyhow::Result<String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        let token: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        fs::write(path, &token)?;
    }
    let token = fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("hostd token file is empty: {}", path.display());
    }
    Ok(token)
}

fn handle_hostd_stream(stream: &mut TcpStream, token: &str, log_path: &Path) -> anyhow::Result<()> {
    let request = read_hostd_request(stream)?;
    if request.path != "/health" && !hostd_request_authorized(&request, token) {
        return write_hostd_json(
            stream,
            401,
            serde_json::json!({ "ok": false, "error": "unauthorized" }),
        );
    }
    let response = route_hostd_request(request, log_path)?;
    write_hostd_json(stream, response.status, response.body)
}

fn route_hostd_request(
    request: HostdHttpRequest,
    log_path: &Path,
) -> anyhow::Result<HostdRouteResponse> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => Ok(hostd_ok(serde_json::json!({}))),
        ("GET", "/vms") => hyperv_vms(),
        ("POST", "/agents/launch/ubuntu") => {
            let body: HostdLaunchBody = serde_json::from_slice(&request.body)
                .context("failed to parse launch request body")?;
            hyperv_launch_ubuntu(body, log_path)
        }
        ("POST", "/agents/stop") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse stop request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            hyperv_stop(&agent_id)
        }
        ("POST", "/agents/snapshot/take") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse snapshot request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            let name = required_json_string(&body, "name")?;
            hyperv_snapshot_take(&agent_id, &name)
        }
        ("POST", "/agents/snapshot/restore") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse snapshot request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            let name = required_json_string(&body, "name")?;
            hyperv_snapshot_restore(&agent_id, &name)
        }
        ("GET", "/agents/snapshot/list") => {
            let agent_id = request
                .query
                .get("agent_id")
                .ok_or_else(|| anyhow::anyhow!("agent_id is required"))?;
            hyperv_snapshot_list(agent_id)
        }
        _ => Ok(HostdRouteResponse {
            status: 404,
            body: serde_json::json!({ "ok": false, "error": "unknown endpoint" }),
        }),
    }
}

fn hostd_ok(mut extra: serde_json::Value) -> HostdRouteResponse {
    if let Some(object) = extra.as_object_mut() {
        object.insert("ok".to_string(), serde_json::json!(true));
    }
    HostdRouteResponse {
        status: 200,
        body: extra,
    }
}

fn required_json_string(body: &serde_json::Value, name: &str) -> anyhow::Result<String> {
    body.get(name)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("{name} is required"))
}

fn hyperv_launch_ubuntu(
    body: HostdLaunchBody,
    log_path: &Path,
) -> anyhow::Result<HostdRouteResponse> {
    let agent_id = body.agent_id.unwrap_or_else(|| "codex-demo".to_string());
    validate_hostd_agent_id(&agent_id)?;
    let harness = body.harness.unwrap_or_else(|| "codex".to_string());
    if !matches!(
        harness.as_str(),
        "codex" | "claude-code" | "opencode" | "none"
    ) {
        return Ok(HostdRouteResponse {
            status: 400,
            body: serde_json::json!({ "ok": false, "error": format!("unsupported harness: {harness}") }),
        });
    }
    let repo_root = repo_root()?;
    let launch_log_path = repo_root
        .join(".maturana")
        .join("logs")
        .join(format!("hyperv-launch-{agent_id}.log"));
    if let Some(parent) = launch_log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    hostd_log(
        log_path,
        &format!("launch requested; agent={agent_id} harness={harness}"),
    )?;
    hostd_log(&launch_log_path, "hostd launch started")?;
    let launcher = repo_root
        .join("scripts")
        .join("launch-ubuntu-cloudimg-hyperv.ps1");
    let mut command = ProcessCommand::new("powershell.exe");
    command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
    command.arg(launcher);
    add_process_arg(&mut command, "-AgentId", Some(agent_id.as_str()));
    add_process_arg_path(
        &mut command,
        "-BaseVhdxPath",
        body.base_vhdx_path.as_deref(),
    );
    add_process_arg(&mut command, "-SwitchName", body.switch_name.as_deref());
    add_process_arg(
        &mut command,
        "-SshUser",
        body.ssh_user.as_deref().or(Some("ubuntu")),
    );
    add_process_arg_path(&mut command, "-SshKeyPath", body.ssh_key_path.as_deref());
    add_process_arg_path(
        &mut command,
        "-CloudInitUserDataPath",
        body.cloud_init_user_data_path.as_deref(),
    );
    add_process_arg_path(
        &mut command,
        "-CloudInitMetaDataPath",
        body.cloud_init_meta_data_path.as_deref(),
    );
    let disk_size = body.disk_size_gb.map(|value| value.to_string());
    add_process_arg(&mut command, "-DiskSizeGB", disk_size.as_deref());
    let vcpu = body.vcpu.map(|value| value.to_string());
    add_process_arg(&mut command, "-Vcpu", vcpu.as_deref());
    let memory = body.memory_mib.map(|value| value.to_string());
    add_process_arg(&mut command, "-MemoryMiB", memory.as_deref());
    if body.provision_existing.unwrap_or(false) {
        command.arg("-ProvisionExisting");
    }
    if body.force.unwrap_or(false) {
        command.arg("-Force");
    }
    let output = command.output().context("failed to run Hyper-V launcher")?;
    let lines = command_output_lines(&output);
    let status_code = if output.status.success() { 200 } else { 500 };
    hostd_log(
        &launch_log_path,
        &format!("hostd launch finished exit_code={:?}", output.status.code()),
    )?;
    Ok(HostdRouteResponse {
        status: status_code,
        body: serde_json::json!({
            "ok": output.status.success(),
            "agent_id": agent_id,
            "status": if output.status.success() { "succeeded" } else { "failed" },
            "exit_code": output.status.code(),
            "log": launch_log_path,
            "output": lines,
        }),
    })
}

fn hyperv_vms() -> anyhow::Result<HostdRouteResponse> {
    let script = r#"
$ErrorActionPreference = 'Stop'
function Get-MaturanaVMIPv4 {
  param([string]$Name)
  $adapter = Get-VMNetworkAdapter -VMName $Name -ErrorAction SilentlyContinue
  if (!$adapter) { return '' }
  $addresses = @($adapter.IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' -and $_ -notlike '0.*' -and $_ -notlike '127.*' })
  if ($addresses.Count -gt 0) { return $addresses[0] }
  $mac = ($adapter.MacAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant()
  if (!$mac) { return '' }
  $neighbor = Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {
    ($_.LinkLayerAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant() -eq $mac -and
    $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and
    $_.IPAddress -notlike '169.254.*' -and $_.IPAddress -notlike '0.*' -and $_.IPAddress -notlike '127.*'
  } | Select-Object -First 1
  if ($neighbor) { return $neighbor.IPAddress }
  return ''
}
$vms = @(Get-VM | Where-Object { $_.Name -like 'maturana-*' } | ForEach-Object {
  [pscustomobject]@{
    name = $_.Name
    state = "$($_.State)"
    status = "$($_.Status)"
    uptime = "$($_.Uptime)"
    generation = $_.Generation
    processor_count = $_.ProcessorCount
    memory_startup = $_.MemoryStartup
    ipv4 = Get-MaturanaVMIPv4 -Name $_.Name
  }
})
@{ ok = $true; vms = $vms } | ConvertTo-Json -Compress -Depth 10
"#;
    hyperv_json_script(script)
}

fn hyperv_stop(agent_id: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; Stop-VM -Name '{vm_name}' -Force -TurnOff; @{{ ok=$true; vm='{vm_name}'; state='stopped' }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_take(agent_id: &str, name: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let snapshot = validate_hostd_snapshot_name(name)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; Checkpoint-VM -Name '{vm_name}' -SnapshotName '{snapshot}' | Out-Null; @{{ ok=$true; vm='{vm_name}'; snapshot='{snapshot}' }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_restore(agent_id: &str, name: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let snapshot = validate_hostd_snapshot_name(name)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; $s=Get-VMSnapshot -VMName '{vm_name}' -Name '{snapshot}' -ErrorAction SilentlyContinue; if(!$s){{ @{{ ok=$false; error='Snapshot not found: {snapshot}' }} | ConvertTo-Json -Compress; exit 4 }}; Restore-VMSnapshot -VMSnapshot $s -Confirm:$false; @{{ ok=$true; vm='{vm_name}'; snapshot='{snapshot}'; restored=$true }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_list(agent_id: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; $snapshots=@(Get-VMSnapshot -VMName '{vm_name}' -ErrorAction SilentlyContinue | Select-Object Name, CreationTime, SnapshotType); @{{ ok=$true; vm='{vm_name}'; snapshots=$snapshots }} | ConvertTo-Json -Compress -Depth 10"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_json_script(script: &str) -> anyhow::Result<HostdRouteResponse> {
    let output = ProcessCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .output()
        .context("failed to run Hyper-V PowerShell adapter")?;
    let status = if output.status.success() { 200 } else { 500 };
    let body = parse_powershell_json_output(&output).unwrap_or_else(|_| {
        serde_json::json!({
            "ok": false,
            "error": String::from_utf8_lossy(&output.stderr).trim().to_string(),
            "output": command_output_lines(&output),
        })
    });
    Ok(HostdRouteResponse { status, body })
}

fn hyperv_json_script_with_not_found(script: &str) -> anyhow::Result<HostdRouteResponse> {
    let mut response = hyperv_json_script(script)?;
    if response.status >= 400
        && response
            .body
            .get("error")
            .and_then(|value| value.as_str())
            .map(|value| value.contains("not found"))
            .unwrap_or(false)
    {
        response.status = 404;
    }
    Ok(response)
}

fn parse_powershell_json_output(
    output: &std::process::Output,
) -> anyhow::Result<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("PowerShell adapter did not return JSON"))?;
    Ok(serde_json::from_str(json_line.trim())?)
}

fn command_output_lines(output: &std::process::Output) -> Vec<String> {
    let mut lines = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        lines.push(line.to_string());
    }
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        lines.push(line.to_string());
    }
    lines
}

fn add_process_arg(command: &mut ProcessCommand, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        command.arg(name).arg(value);
    }
}

fn add_process_arg_path(command: &mut ProcessCommand, name: &str, value: Option<&Path>) {
    if let Some(value) = value {
        command.arg(name).arg(value);
    }
}

fn hostd_vm_name(agent_id: &str) -> anyhow::Result<String> {
    validate_hostd_agent_id(agent_id)?;
    Ok(format!("maturana-{agent_id}"))
}

fn validate_hostd_agent_id(agent_id: &str) -> anyhow::Result<()> {
    if !agent_id.is_empty()
        && agent_id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        Ok(())
    } else {
        anyhow::bail!("invalid agent id: {agent_id}")
    }
}

fn validate_hostd_snapshot_name(name: &str) -> anyhow::Result<&str> {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        Ok(name)
    } else {
        anyhow::bail!("invalid snapshot name: {name}")
    }
}

fn read_hostd_request(stream: &TcpStream) -> anyhow::Result<HostdHttpRequest> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP target"))?;
    let (path, query) = parse_http_target(target);
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    // Cap the pre-allocation so a forged Content-Length can't OOM the elevated
    // daemon before any bytes arrive. hostd payloads are small JSON requests.
    if content_length > 1024 * 1024 {
        anyhow::bail!("hostd request body too large");
    }
    let mut body = vec![0; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(HostdHttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn parse_http_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(name.to_string(), percent_decode_minimal(value));
    }
    (path.to_string(), query)
}

fn percent_decode_minimal(value: &str) -> String {
    let mut output = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(hex);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

fn hostd_request_authorized(request: &HostdHttpRequest, token: &str) -> bool {
    request
        .headers
        .get("x-maturana-hostd-token")
        .map(|actual| actual == token)
        .unwrap_or(false)
}

fn write_hostd_json(
    stream: &mut TcpStream,
    status: u16,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let json = serde_json::to_vec(&body)?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        json.len()
    )?;
    stream.write_all(&json)?;
    stream.flush()?;
    Ok(())
}

fn parse_hostd_bind_prefix(prefix: &str) -> anyhow::Result<SocketAddr> {
    let rest = prefix
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("hostd bind prefix must start with http://"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("hostd bind prefix must include a port"))?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        anyhow::bail!("hostd bind must stay on loopback, got {host}");
    }
    let port = port.parse::<u16>()?;
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn hostd_log(path: &Path, message: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = format!("{} {message}\n", Utc::now().to_rfc3339());
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())?;
    Ok(())
}

fn repo_root() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn hostd_url(path: &str) -> String {
    format!(
        "{}{}",
        std::env::var("MATURANA_HOSTD_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:47832".to_string())
            .trim_end_matches('/'),
        path
    )
}

fn hostd_get(path: &str) -> anyhow::Result<ureq::Response> {
    let mut request = ureq::get(&hostd_url(path));
    if let Some(token) = hostd_token()? {
        request = request.set("X-Maturana-Hostd-Token", &token);
    }
    Ok(request.call()?)
}

fn hostd_token() -> anyhow::Result<Option<String>> {
    if let Ok(token) = std::env::var("MATURANA_HOSTD_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    let path = std::env::var("MATURANA_HOSTD_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/hostd/token"));
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.exists() {
        let token = fs::read_to_string(path)?;
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_transfer_paths_allow_declared_roots() {
        let roots = default_guest_transfer_roots();
        assert!(validate_guest_transfer_path("/workspace/output.txt", &roots).is_ok());
        assert!(validate_guest_transfer_path("/memory/state.json", &roots).is_ok());
        assert!(validate_guest_transfer_path("/wiki/page.md", &roots).is_ok());
        assert!(validate_guest_transfer_path("/workspace", &roots).is_ok());
    }

    #[test]
    fn guest_transfer_paths_allow_extra_declared_roots() {
        let roots = vec!["/workspace".to_string(), "/scratch".to_string()];
        assert!(validate_guest_transfer_path("/scratch/output.txt", &roots).is_ok());
    }

    #[test]
    fn guest_transfer_paths_reject_escape_paths() {
        let roots = default_guest_transfer_roots();
        assert!(validate_guest_transfer_path("", &roots).is_err());
        assert!(validate_guest_transfer_path("workspace/output.txt", &roots).is_err());
        assert!(validate_guest_transfer_path("/workspace/../etc/passwd", &roots).is_err());
        assert!(validate_guest_transfer_path("/etc/passwd", &roots).is_err());
        assert!(validate_guest_transfer_path("/home/ubuntu/.codex/auth.json", &roots).is_err());
    }

    #[test]
    fn guest_transfer_roots_ignore_unsafe_mount_roots() {
        assert_eq!(
            normalize_guest_transfer_root("/scratch/"),
            Some("/scratch".to_string())
        );
        assert_eq!(normalize_guest_transfer_root("/"), None);
        assert_eq!(normalize_guest_transfer_root("relative"), None);
        assert_eq!(normalize_guest_transfer_root("/workspace/../etc"), None);
    }

    #[test]
    fn repair_windows_config_uses_three_harness_defaults() {
        let config =
            repair_windows_config(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .unwrap();

        assert_eq!(
            config.agent_ids,
            vec!["codex-demo", "opencode-demo", "claude-demo"]
        );
        assert_eq!(
            config.session_ids,
            vec!["codex-main", "opencode-main", "claude-main"]
        );
        assert_eq!(config.harnesses, vec!["codex", "opencode", "claude-code"]);
        assert_eq!(
            config.telegram_token_sources,
            vec![
                "pipelock:telegram/bot-token",
                "pipelock:telegram/opencode-bot-token",
                "pipelock:telegram/claude-bot-token",
            ]
        );
    }

    #[test]
    fn rust_hostd_bind_prefix_stays_loopback() {
        assert_eq!(
            parse_hostd_bind_prefix("http://127.0.0.1:47832/").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 47832))
        );
        assert_eq!(
            parse_hostd_bind_prefix("http://localhost:47832").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 47832))
        );
        assert!(parse_hostd_bind_prefix("https://127.0.0.1:47832/").is_err());
        assert!(parse_hostd_bind_prefix("http://0.0.0.0:47832/").is_err());
    }

    #[test]
    fn rust_hostd_validates_agent_and_snapshot_names() {
        assert!(validate_hostd_agent_id("codex-demo-1").is_ok());
        assert!(validate_hostd_agent_id("Codex").is_err());
        assert!(validate_hostd_agent_id("../demo").is_err());
        assert_eq!(hostd_vm_name("codex-demo").unwrap(), "maturana-codex-demo");

        assert!(validate_hostd_snapshot_name("before.update-1").is_ok());
        assert!(validate_hostd_snapshot_name("../escape").is_err());
        assert!(validate_hostd_snapshot_name("bad/name").is_err());
    }

    #[test]
    fn rust_hostd_auth_and_target_parsing_are_fixed() {
        let (path, query) = parse_http_target("/agents/snapshot/list?agent_id=codex-demo%201");
        assert_eq!(path, "/agents/snapshot/list");
        assert_eq!(query.get("agent_id").unwrap(), "codex-demo 1");

        let mut request = HostdHttpRequest {
            method: "GET".to_string(),
            path,
            query,
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert!(!hostd_request_authorized(&request, "secret"));
        request
            .headers
            .insert("x-maturana-hostd-token".to_string(), "secret".to_string());
        assert!(hostd_request_authorized(&request, "secret"));
    }

    #[test]
    fn repair_windows_config_rejects_uneven_lists() {
        let error = repair_windows_config(
            vec!["codex-demo".to_string(), "opencode-demo".to_string()],
            vec!["codex-main".to_string()],
            vec!["codex".to_string(), "opencode".to_string()],
            vec![
                "/home/ubuntu/.codex".to_string(),
                "/home/ubuntu".to_string(),
            ],
            vec![
                "pipelock:telegram/bot-token".to_string(),
                "pipelock:telegram/opencode-bot-token".to_string(),
            ],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("--session-id count"));
    }

    #[test]
    fn ssh_key_repair_keeps_existing_key_without_force() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-ssh-key-repair-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let key_path = temp.join("maturana-agent-ed25519");
        fs::write(&key_path, "existing-key").unwrap();

        ensure_agent_ssh_key(key_path.clone(), false).unwrap();

        assert_eq!(fs::read_to_string(&key_path).unwrap(), "existing-key");
        assert_eq!(
            public_key_path(&key_path),
            PathBuf::from(format!("{}.pub", key_path.display()))
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn ubuntu_cloudimg_checksum_helpers_match_script_behavior() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-cloudimg-repair-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let image_path = temp.join("noble-server-cloudimg-amd64.img");
        fs::write(&image_path, b"maturana").unwrap();
        let sha_path = temp.join("SHA256SUMS");
        fs::write(
            &sha_path,
            format!(
                "{} *noble-server-cloudimg-amd64.img\n{} other.img\n",
                sha256_file_hex(&image_path).unwrap(),
                "0".repeat(64)
            ),
        )
        .unwrap();

        assert_eq!(
            expected_sha256_for_image(&sha_path, "noble-server-cloudimg-amd64.img").unwrap(),
            sha256_file_hex(&image_path).unwrap()
        );
        assert!(expected_sha256_for_image(&sha_path, "missing.img").is_err());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn snapshot_audit_events_cover_success_and_failure() {
        assert_eq!(
            snapshot_audit_event("list", true, false),
            "snapshot.list.live"
        );
        assert_eq!(
            snapshot_audit_event("list", false, true),
            "snapshot.list.local.failed"
        );
        assert_eq!(
            snapshot_audit_event("take", false, false),
            "snapshot.take.local"
        );
        assert_eq!(
            snapshot_audit_event("take", true, true),
            "snapshot.take.live.failed"
        );
        assert_eq!(
            snapshot_audit_event("restore", true, false),
            "snapshot.restore.live"
        );
        assert_eq!(
            snapshot_audit_event("restore", true, true),
            "snapshot.restore.live.failed"
        );
    }

    #[test]
    fn live_guest_state_script_smokes_browser_only_when_requested() {
        let without_browser = render_live_guest_state_script(false);
        assert!(without_browser.contains("live.browser_expected: false"));
        assert!(!without_browser.contains("browser-smoke.js"));
        assert!(!without_browser.contains("PLAYWRIGHT_BROWSERS_PATH"));

        let with_browser = render_live_guest_state_script(true);
        assert!(with_browser.contains("live.browser_expected: true"));
        assert!(with_browser.contains("/opt/maturana/bin/browser-smoke.js"));
        assert!(with_browser.contains("PLAYWRIGHT_BROWSERS_PATH"));
        assert!(with_browser.contains("live.browser_smoke_output"));
        assert!(with_browser.contains("live.agent_log_tail"));
    }

    #[test]
    fn windows_runner_helpers_are_rust_owned() {
        assert_eq!(safe_windows_task_suffix("codex-demo"), "codex-demo");
        assert_eq!(safe_windows_task_suffix("../bad name"), "..-bad-name");
        assert!(quote_cmd_arg("C:/Program Files/maturana/maturana.exe").starts_with('"'));
        assert!(!quote_cmd_arg("maturana.exe").contains("powershell"));
    }

    #[test]
    fn agent_run_uses_rendered_session_id() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-agent-run-session-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);
        let state_dir = home.agent_dir("agent").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='custom-session'\n",
        )
        .unwrap();

        assert_eq!(
            infer_agent_session_id(&home, "agent").unwrap(),
            "custom-session"
        );
        assert_eq!(
            infer_agent_session_id(&home, "opencode-firecracker").unwrap(),
            "opencode-main"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn agent_run_queues_and_waits_through_session_db() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-agent-run-queue-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);
        let state_dir = home.agent_dir("agent").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='cli-main'\n",
        )
        .unwrap();

        let queued = enqueue_agent_run(&home, "agent", "hello from cli").unwrap();
        assert_eq!(queued.session_id, "cli-main");
        let paths = session_paths(&home.agent_dir("agent"), "cli-main");
        let pending = maturana_core::session_db::claim_pending_inbound(&paths, 1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, queued.message_id);
        assert!(pending[0].content.contains("hello from cli"));

        maturana_core::session_db::write_outbound(
            &paths,
            Some(&queued.message_id),
            "chat",
            "cli",
            "agent-run",
            None,
            &serde_json::json!({"text": "hello back"}).to_string(),
        )
        .unwrap();
        let completed = wait_for_agent_run(&home, "agent", &queued, 1).unwrap();
        assert_eq!(completed.text, "hello back");
        assert!(list_undelivered(&paths).unwrap().is_empty());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn firecracker_profile_selection_defaults_to_three_harnesses() {
        let profiles = selected_firecracker_profiles(&[]).unwrap();
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].agent_id, "codex-firecracker");
        assert_eq!(profiles[1].harness_arg, "opencode");
        assert_eq!(profiles[2].auth_guest_path, "/home/ubuntu/.claude");
    }

    #[test]
    fn firecracker_profile_selection_rejects_unknown_agent() {
        let error = selected_firecracker_profiles(&["missing".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown Firecracker harness agent"));
    }

    #[test]
    fn firecracker_asset_manifest_validation_accepts_expected_assets() {
        let root = temp_test_dir("firecracker-assets-ok");
        let image_dir = root.join("images");
        fs::create_dir_all(&image_dir).unwrap();
        let kernel = image_dir.join("vmlinux.bin");
        let rootfs = image_dir.join("ubuntu-rootfs.ext4");
        let ssh_key = root.join("maturana-firecracker.id_rsa");
        fs::write(&kernel, b"\x7fELFkernel").unwrap();
        fs::write(&rootfs, b"rootfs").unwrap();
        fs::write(&ssh_key, b"private").unwrap();
        fs::write(
            PathBuf::from(format!("{}.pub", ssh_key.display())),
            b"public",
        )
        .unwrap();
        let profile = &FIRECRACKER_HARNESS_PROFILES[0];
        let manifest = image_dir.join("asset-manifest.json");
        write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);

        validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key).unwrap();
    }

    #[test]
    fn firecracker_asset_manifest_validation_rejects_identity_mismatch() {
        let root = temp_test_dir("firecracker-assets-mismatch");
        let image_dir = root.join("images");
        fs::create_dir_all(&image_dir).unwrap();
        let kernel = image_dir.join("vmlinux.bin");
        let rootfs = image_dir.join("ubuntu-rootfs.ext4");
        let ssh_key = root.join("maturana-firecracker.id_rsa");
        fs::write(&kernel, b"\x7fELFkernel").unwrap();
        fs::write(&rootfs, b"rootfs").unwrap();
        fs::write(&ssh_key, b"private").unwrap();
        fs::write(
            PathBuf::from(format!("{}.pub", ssh_key.display())),
            b"public",
        )
        .unwrap();
        let profile = &FIRECRACKER_HARNESS_PROFILES[0];
        let manifest = image_dir.join("asset-manifest.json");
        write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
        value["agent_id"] = serde_json::json!("wrong-agent");
        fs::write(&manifest, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let error = validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key)
            .unwrap_err()
            .to_string();
        assert!(error.contains("agent mismatch"));
    }

    #[test]
    fn firecracker_asset_manifest_validation_rejects_non_elf_kernel() {
        let root = temp_test_dir("firecracker-assets-non-elf");
        let image_dir = root.join("images");
        fs::create_dir_all(&image_dir).unwrap();
        let kernel = image_dir.join("vmlinux.bin");
        let rootfs = image_dir.join("ubuntu-rootfs.ext4");
        let ssh_key = root.join("maturana-firecracker.id_rsa");
        fs::write(&kernel, b"not-elf").unwrap();
        fs::write(&rootfs, b"rootfs").unwrap();
        fs::write(&ssh_key, b"private").unwrap();
        fs::write(
            PathBuf::from(format!("{}.pub", ssh_key.display())),
            b"public",
        )
        .unwrap();
        let profile = &FIRECRACKER_HARNESS_PROFILES[0];
        let manifest = image_dir.join("asset-manifest.json");
        write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);

        let error = validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an ELF"));
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_test_firecracker_manifest(
        profile: &FirecrackerHarnessProfile,
        manifest: &Path,
        kernel: &Path,
        rootfs: &Path,
        ssh_key: &Path,
    ) {
        let value = serde_json::json!({
            "agent_id": profile.agent_id,
            "kernel": kernel,
            "rootfs": rootfs,
            "ssh_key": ssh_key,
            "guest_ip": profile.guest_ip,
            "host_ip": profile.host_ip,
            "guest_mac": profile.guest_mac,
            "tap_name": profile.tap_name,
            "kernel_sha256": sha256_file_hex(kernel).unwrap(),
            "rootfs_sha256": sha256_file_hex(rootfs).unwrap(),
            "kernel_bytes": fs::metadata(kernel).unwrap().len(),
            "rootfs_bytes": fs::metadata(rootfs).unwrap().len(),
        });
        fs::write(manifest, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    fn repo_example(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }

    #[test]
    fn skill_pack_uses_workflow_shape() {
        let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("skills");
        let report = validate_skill_pack(&skills_dir).unwrap();

        assert!(
            report.failures.is_empty(),
            "skill workflow shape failures:\n{}",
            report.failures.join("\n")
        );
    }

    #[test]
    fn skill_validator_enforces_agents_initial_skill_contract() {
        let required = required_initial_skills();
        assert!(required.contains(&"maturana-agent-create"));
        assert!(required.contains(&"maturana-agent-update"));
        assert!(required.contains(&"maturana-skill-create"));
        assert!(required.contains(&"maturana-tool-create"));
        assert!(required.contains(&"maturana-skill-deploy"));
        assert!(required.contains(&"maturana-security-review"));
        assert_eq!(required.len(), 10);
    }

    #[test]
    fn skill_validator_rejects_wrapper_shape() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-skill-validator-test-{}",
            std::process::id()
        ));
        let skill_dir = temp.join("thin-wrapper");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# thin-wrapper\n\nUse this skill when testing.\n\n## Procedure\n\nsimply run `maturana --help`.\n",
        )
        .unwrap();

        let report = validate_skill_pack(&temp).unwrap();
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("## Grounding")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("catch-all Procedure")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("thin-wrapper language")));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn skill_validator_rejects_heading_only_workflows() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-skill-validator-content-test-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let skill_dir = temp.join("decorated-wrapper");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# decorated-wrapper

Use this skill when testing.

## Grounding

1. Read `AGENTS.md` first.

## Preflight

- Confirm the input.

## Decision Path

- Run the command.

## Actions

```powershell
.\scripts\maturana.ps1 --help
```

## Evidence

- Command returned.

## Recovery

- Retry.

## Boundaries

- Do not bypass validation.
"#,
        )
        .unwrap();

        let report = validate_skill_pack(&temp).unwrap();
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least four concrete proof points")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least four failure-handling paths")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least three explicit 'Do not' limits")));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn firecracker_guest_artifacts_are_rust_rendered() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-firecracker-artifacts-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);
        let profile = &FIRECRACKER_HARNESS_PROFILES[1];
        let spec = AgentSpec::from_maturana_markdown(&repo_example(profile.spec_path)).unwrap();

        let artifacts = render_firecracker_guest_artifacts(
            &home,
            profile,
            &spec,
            "test-token",
            "http://172.30.10.5:47834",
        )
        .unwrap();

        let env = fs::read_to_string(&artifacts.sessiond_env).unwrap();
        assert!(env.contains("MATURANA_AGENT_ID='opencode-firecracker'"));
        assert!(env.contains("MATURANA_HARNESS='opencode'"));
        assert!(env.contains("MATURANA_SESSIOND_URL='http://172.30.10.5:47834'"));
        assert!(env.contains("MATURANA_SESSIOND_TOKEN='test-token'"));

        let runner = fs::read_to_string(&artifacts.runner).unwrap();
        assert!(runner.contains("/session/claim"));
        assert!(runner.contains("opencode"));

        let service = fs::read_to_string(&artifacts.service).unwrap();
        assert!(service.contains("Description=Maturana opencode agent opencode-firecracker"));
        assert!(service.contains("ExecStart=/opt/maturana/bin/run-agent.sh"));

        let harness_install = fs::read_to_string(&artifacts.harness_install).unwrap();
        assert!(harness_install.contains("npm install -g opencode-ai"));
        assert!(!harness_install.contains("@openai/codex"));

        let firecracker_bootstrap = fs::read_to_string(&artifacts.firecracker_bootstrap).unwrap();
        assert!(firecracker_bootstrap.contains("openssh-server curl ca-certificates nodejs npm"));
        assert!(firecracker_bootstrap.contains("systemctl enable ssh.service"));
        assert!(firecracker_bootstrap.contains("/etc/sudoers.d/90-maturana-ubuntu"));

        let netplan = fs::read_to_string(&artifacts.netplan).unwrap();
        assert!(netplan.contains("macaddress: \"AA:FC:00:00:10:02\""));
        assert!(netplan.contains("- 172.30.10.6/30"));
        assert!(netplan.contains("via: 172.30.10.5"));

        let cloud_cfg = fs::read_to_string(&artifacts.cloud_cfg).unwrap();
        assert_eq!(cloud_cfg, "network: {config: disabled}\n");
        assert!(artifacts.proxy_env.is_none());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn firecracker_proxy_env_is_rust_rendered_from_spec() {
        let spec = AgentSpec::from_maturana_markdown(&repo_example(
            "examples/MATURANA.firecracker-demo.md",
        ))
        .unwrap();
        let proxy = spec.network.proxy.as_ref().unwrap();
        let proxy_env =
            render_firecracker_proxy_env(proxy.enabled, Some(&proxy.bind), "172.30.0.1")
                .unwrap()
                .unwrap();

        assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
        assert!(proxy_env.contains("MATURANA_PROXY_HOST=172.30.0.1"));
        assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));
        assert!(proxy_env.contains("MATURANA_PROXY_HTTPS=1"));
        assert!(proxy_env.contains("NO_PROXY=localhost,127.0.0.1,::1"));
    }

    #[test]
    fn bind_port_extracts_sessiond_port() {
        assert_eq!(bind_port("0.0.0.0:47834").unwrap(), "47834");
        assert_eq!(bind_port("127.0.0.1:1").unwrap(), "1");
        assert!(bind_port("0.0.0.0").is_err());
    }
}
