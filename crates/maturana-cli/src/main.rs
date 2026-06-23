mod a2a;
mod channels;
mod graph;
mod orchestrate;
mod proactive;
mod service;
mod personal;
mod session;
mod tui;

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
    session_db::{
        list_recent_inbound, list_undelivered, mark_delivered, queue_stats, session_paths,
    },
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
    /// Validate a MATURANA.md agent spec (pre-flight check).
    Spec(SpecCommand),
    /// Manage one agent: launch, inspect, stop, chat, run, logs, fetch/push.
    Agent(AgentCommand),
    /// List, take, or restore agent VM snapshots.
    Snapshot(SnapshotCommand),
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
    /// Route inbound messages (by channel/sender/content) to the right agent.
    Route(RouteCommand),
    /// Serve Agent2Agent (A2A) endpoints for agent-to-agent calls.
    #[command(hide = true)]
    A2a(a2a::A2aCommand),
    /// Push a skill or tool to a live agent over SSH.
    Deploy(DeployCommand),
    /// Scaffold a new skill or tool locally under skills/ or tools/.
    Develop(DevelopCommand),
    /// Validate skills and install them as native Codex skills.
    Skill(SkillCommand),
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

#[derive(Debug, Args)]
struct ListCommand {
    /// Emit JSON instead of the table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StatusCommand {
    /// Emit JSON instead of the dashboard.
    #[arg(long)]
    json: bool,
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

/// Web cockpit server. Binds a LAN-reachable port; access is gated by the
/// token at `<home>/web/token` exchanged for a session cookie at /login.
#[derive(Debug, Args)]
struct WebCommand {
    #[command(subcommand)]
    command: Option<WebSubcommand>,
    #[arg(long, default_value = "0.0.0.0:47836")]
    bind: String,
}

#[derive(Debug, Subcommand)]
enum WebSubcommand {
    /// Print the cockpit login token (creating it if absent).
    Token,
}

/// Keep claude-code OAuth tokens fresh host-side.
#[derive(Debug, Args)]
struct ClaudeRefreshCommand {
    #[command(subcommand)]
    command: ClaudeRefreshSubcommand,
}

#[derive(Debug, Subcommand)]
enum ClaudeRefreshSubcommand {
    /// Do ONE real refresh against the host-auth creds to verify the endpoint,
    /// rotating + writing the result. Prints success/expiry only, never tokens.
    Probe {
        #[arg(long, default_value = ".maturana/host-auth/claude-code/.credentials.json")]
        creds: PathBuf,
    },
    /// Run the refresh daemon: watch host-auth creds and refresh before expiry.
    Serve {
        /// claude-code agent ids to keep refreshed + re-pushed. Empty = all.
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(long, default_value = ".maturana/host-auth/claude-code/.credentials.json")]
        creds: PathBuf,
        #[arg(long, default_value_t = 300)]
        poll_seconds: u64,
    },
}

/// Host-side web search: `maturana search "query" --provider brave|tavily`.
/// Keys live in pipelock (`brave/api-key`, `tavily/api-key`). Guests use the
/// maturana-web-search skill (proxy header injection) instead.
#[derive(Debug, Args)]
struct SearchCommand {
    query: Vec<String>,
    #[arg(long, default_value = "brave")]
    provider: String,
    #[arg(long, default_value_t = 5)]
    count: usize,
    #[arg(long)]
    json: bool,
}

/// Supervise the whole host runtime plane as one restart-on-failure process
/// group: sessiond (:47834), optional MaturanaGraph (:47835) and claude-refresh,
/// plus per-agent channel bridges, schedule + proactivity runners, and egress
/// proxies. Writes <home>/up/state.json (read by `maturana status`). Does NOT
/// boot the agent VMs (that's `agent launch`). Use --dry-run to print the plan.
#[derive(Debug, Args)]
struct UpCommand {
    /// Agents to run. Defaults to every materialized agent under the home.
    #[arg(long = "agent-id")]
    agent_ids: Vec<String>,
    #[arg(long, default_value = "0.0.0.0:47834")]
    sessiond_bind: String,
    #[arg(long, env = "MATURANA_SESSIOND_TOKEN")]
    sessiond_token: Option<String>,
    /// Override every agent's session id. When omitted (the default), each
    /// agent's session id is derived from its materialized spec / Firecracker
    /// profile via [`infer_agent_session_id`], so the supervised channel writes
    /// to the same queue the guest worker claims from.
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long, default_value = "pipelock:telegram/bot-token")]
    telegram_token_source: String,
    #[arg(long)]
    no_telegram: bool,
    #[arg(long)]
    no_schedules: bool,
    #[arg(long)]
    no_proactive: bool,
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
struct RouteCommand {
    #[command(subcommand)]
    command: RouteSubcommand,
}

#[derive(Debug, Subcommand)]
enum RouteSubcommand {
    /// Add a rule: an inbound matching the given conditions routes to `--agent`.
    /// Conditions are optional and ANDed; unset = wildcard. Most specific wins.
    Add {
        #[arg(long)]
        agent: String,
        /// Match only this channel (telegram/discord/slack/agentmail/webhook/…).
        #[arg(long)]
        channel: Option<String>,
        /// Match only this sender / peer / chat id.
        #[arg(long)]
        from: Option<String>,
        /// Match only messages containing this text (case-insensitive).
        #[arg(long)]
        contains: Option<String>,
    },
    /// Set the default agent for inbound that matches no rule.
    Default { agent: String },
    /// Show the routing table.
    List,
    /// Remove rule number N (as shown by `list`, 1-based).
    Remove { index: usize },
    /// Test where an inbound would route (prints the resolved agent).
    Test {
        #[arg(long)]
        channel: String,
        #[arg(long, default_value = "")]
        from: String,
        #[arg(long, default_value = "")]
        text: String,
    },
    /// Remove every rule and the default.
    Clear,
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
    /// Install Maturana's skills as native Codex skills (discovered via the
    /// `/skills` menu, `$name` mention, or implicitly). Writes
    /// `<dest>/<name>/SKILL.md` with the required frontmatter; default dest is
    /// the user-level Codex skill root `~/.agents/skills`.
    #[command(alias = "codex")]
    CodexPrompts {
        #[arg(default_value = "skills")]
        root: PathBuf,
        /// Override the Codex skills directory (default ~/.agents/skills).
        #[arg(long, alias = "prompts-dir")]
        dest: Option<PathBuf>,
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
    /// Open an interactive console TUI to chat with a running agent.
    Chat {
        agent_id: String,
        /// Seconds to wait for each reply before showing a timeout.
        #[arg(long, default_value_t = 180)]
        timeout_seconds: u64,
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
        /// Resolve the agent's spec from `--home` (the agent's MATURANA.md).
        /// Preferred over `--spec` for supervised runs: it is independent of the
        /// process working directory, which `maturana up` does not set.
        #[arg(long)]
        agent_id: Option<String>,
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
        /// Re-seed the guest's harness auth from --auth-source even if the guest
        /// already has a live `.credentials.json`. Only for recovering a dead
        /// guest: claude-code self-refreshes its single-use OAuth token in-guest,
        /// so re-seeding a live guest clobbers it and logs the agent out.
        #[arg(long)]
        force_reseed_auth: bool,
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
        /// Skip recreating the per-agent TAP device + NAT rule. The TAP is
        /// ephemeral (gone after a host reboot) and cheap to recreate, so boot
        /// recovery wants it ON even with --skip-assets. Only set this when the
        /// networking is known-good and you want a pure no-op relaunch.
        #[arg(long)]
        skip_net: bool,
        #[arg(long)]
        skip_launch: bool,
        #[arg(long)]
        skip_worker_refresh: bool,
        /// Skip starting sessiond + the MaturanaGraph service. Tokens are still
        /// ensured (so guest artifacts and `maturana up`'s graph supervision can
        /// find them), but the plane processes are left for `maturana up` to
        /// own, avoiding a port collision on 47834/47835 with the systemd
        /// `maturana-up` service.
        #[arg(long)]
        skip_services: bool,
        #[arg(long)]
        no_install_harness: bool,
        #[arg(long, default_value_t = 120)]
        ssh_wait_seconds: u64,
        /// Re-seed each guest's harness auth from the profile's host-auth even if
        /// the guest already has a live `.credentials.json`. Default off: a
        /// firecracker claude guest self-refreshes its own single-use OAuth token,
        /// so a routine repair must NOT clobber it. Only set this to recover a
        /// dead guest.
        #[arg(long)]
        force_reseed_auth: bool,
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
                // Graph is on by default; guest provisioning embeds the graph
                // token (read_graph_token). Generate it now so an --apply launch
                // gives the guest working graph access without a later re-provision.
                if apply && parsed.knowledge_graph.enabled {
                    ensure_graph_token(&home)?;
                }
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
            AgentSubcommand::Chat {
                agent_id,
                timeout_seconds,
            } => {
                tui::run_chat(&home, &agent_id, timeout_seconds)?;
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
                // A rollback is a negative training signal: the turn(s) since the
                // snapshot were bad enough to undo. Penalize the latest turn.
                if let Ok(store) = maturana_core::improvement::TrajectoryStore::open(
                    &maturana_core::improvement::TrajectoryStore::store_path(home.root()),
                ) {
                    // Session-agnostic: the rollback knows the agent, not the
                    // session id the bad turn was recorded under, so penalize the
                    // agent's most recent turn across sessions. Surface (don't
                    // swallow) the no-match / error cases.
                    match store.reward_latest_for_agent(
                        &agent_id,
                        "snapshot",
                        maturana_core::improvement::signals::SNAPSHOT_ROLLBACK,
                        Some(&format!("rollback to {name}")),
                    ) {
                        Ok(Some(_)) => {}
                        Ok(None) => eprintln!(
                            "[maturana] note: no recorded turn for agent {agent_id}; rollback penalty not applied"
                        ),
                        Err(error) => eprintln!(
                            "[maturana] warning: could not record rollback penalty: {error:#}"
                        ),
                    }
                }
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
                    agent_id,
                    spec,
                    bind,
                    allowlist,
                    inject_headers,
                } => {
                    // `--agent-id` resolves the spec from `--home` so supervised
                    // runs (where the working directory is unset) work; an
                    // explicit `--spec` still takes precedence when both are given.
                    let spec = spec.or_else(|| {
                        agent_id.map(|id| home.agent_dir(&id).join("MATURANA.md"))
                    });
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
                                    runtime_allow: Default::default(),
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
        Command::Proactive(command) => proactive::handle_proactive(command, &home)?,
        Command::Orchestrator(command) => orchestrate::handle_orchestrator(command, &home)?,
        Command::Route(command) => handle_route(command, &home)?,
        Command::A2a(command) => a2a::handle_a2a(command, &home)?,
        Command::Deploy(command) => handle_deploy(command, &home)?,
        Command::Develop(command) => handle_develop(command)?,
        Command::Skill(command) => handle_skill(command)?,
        Command::Channel(command) => handle_channel(command, &home)?,
        Command::Session(command) => handle_session(command, &home)?,
        Command::Graph(command) => graph::handle_graph(command, &home)?,
        Command::Up(command) => run_up(&home, command)?,
        Command::List(command) => run_list(&home, command)?,
        Command::Status(command) => run_status(&home, command)?,
        Command::Tui(command) => {
            tui::run_tui(&home, command.agent_id.as_deref(), command.timeout_seconds)?
        }
        Command::Web(command) => match command.command {
            Some(WebSubcommand::Token) => {
                println!("{}", maturana_web::login_token(home.root())?);
            }
            None => {
                eprintln!(
                    "note: the web cockpit is experimental and not yet stabilized; \
                     it is not installed or started by default."
                );
                // Inject the shared channel front door so cockpit turns get the
                // same transcript memory + model/reasoning + routing as every other
                // channel (the cli owns the context builder; the web crate can't).
                let enqueue: maturana_web::EnqueueTurnFn = std::sync::Arc::new(
                    |home_root: &std::path::Path,
                     agent_id: &str,
                     session_id: &str,
                     text: &str| {
                        let home = MaturanaHome::new(home_root.to_path_buf());
                        crate::channels::enqueue_turn(
                            &home,
                            agent_id,
                            session_id,
                            "web",
                            "web",
                            crate::channels::stable_chat_key(&format!("web:{session_id}")),
                            None,
                            text,
                            serde_json::json!({}),
                        )
                    },
                );
                maturana_web::run_web(home.root().to_path_buf(), &command.bind, enqueue)?
            }
        },
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

/// Manage the multi-agent routing table: which agent an inbound message goes to,
/// by channel/sender/content. A dispatch table only — agents stay VM-isolated.
fn handle_route(command: RouteCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    use maturana_core::routing::{Route, RoutingTable};
    match command.command {
        RouteSubcommand::Add {
            agent,
            channel,
            from,
            contains,
        } => {
            let mut table = RoutingTable::load(home)?;
            let route = Route {
                channel,
                sender: from,
                contains,
                agent: agent.clone(),
            };
            println!("added route: {} -> {agent}", route.describe());
            table.routes.push(route);
            table.save(home)?;
            Ok(())
        }
        RouteSubcommand::Default { agent } => {
            let mut table = RoutingTable::load(home)?;
            table.default = Some(agent.clone());
            table.save(home)?;
            println!("default route -> {agent}");
            Ok(())
        }
        RouteSubcommand::List => {
            let table = RoutingTable::load(home)?;
            if table.routes.is_empty() && table.default.is_none() {
                println!("no routes yet (add with `maturana route add --agent <id> [--channel …] [--from …] [--contains …]`)");
                return Ok(());
            }
            for (i, route) in table.routes.iter().enumerate() {
                println!("  {}. {} -> {}", i + 1, route.describe(), route.agent);
            }
            match &table.default {
                Some(agent) => println!("  default -> {agent}"),
                None => println!("  default -> (none; an unmatched message is dropped)"),
            }
            Ok(())
        }
        RouteSubcommand::Remove { index } => {
            let mut table = RoutingTable::load(home)?;
            if index == 0 || index > table.routes.len() {
                anyhow::bail!("no rule #{index} (see `maturana route list`)");
            }
            let removed = table.routes.remove(index - 1);
            table.save(home)?;
            println!("removed rule: {} -> {}", removed.describe(), removed.agent);
            Ok(())
        }
        RouteSubcommand::Test {
            channel,
            from,
            text,
        } => {
            let table = RoutingTable::load(home)?;
            match table.resolve(&channel, &from, &text) {
                Some(agent) => println!("{channel}/{from}: \"{text}\" -> {agent}"),
                None => println!("{channel}/{from}: \"{text}\" -> (no route)"),
            }
            Ok(())
        }
        RouteSubcommand::Clear => {
            RoutingTable::default().save(home)?;
            println!("cleared the routing table");
            Ok(())
        }
    }
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
        SkillSubcommand::CodexPrompts { root, dest } => {
            let count = sync_codex_prompts(&root, dest.as_deref())?;
            println!("installed {count} Codex skill(s); use /skills or $<name> in Codex");
            Ok(())
        }
    }
}

/// Install Maturana's skills as native **Codex skills** so Codex discovers them
/// (`/skills` menu, `$name` mention, or implicit selection). Current Codex
/// (0.117+) reads skills from `<dest>/<name>/SKILL.md` under one of its skill
/// roots (default user-level `~/.agents/skills`); the deprecated
/// `~/.codex/prompts` slash-command path no longer applies. Each emitted
/// `SKILL.md` gets the required `name`/`description` YAML frontmatter (Codex caps
/// the description in the initial list) followed by the canonical skill body.
/// Idempotent.
fn sync_codex_prompts(root: &Path, dest_dir: Option<&Path>) -> anyhow::Result<usize> {
    let skills_root = absolute_or_cwd(root.to_path_buf())?;
    if !skills_root.is_dir() {
        anyhow::bail!("skills directory not found: {}", skills_root.display());
    }
    let dest = match dest_dir {
        Some(p) => p.to_path_buf(),
        None => dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?
            .join(".agents")
            .join("skills"),
    };
    fs::create_dir_all(&dest)?;

    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(&skills_root)? {
        let dir = entry?.path();
        if dir.is_dir() && dir.join("SKILL.md").exists() {
            if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();

    for name in &names {
        let src = skills_root.join(name).join("SKILL.md");
        let body = fs::read_to_string(&src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        // If the canonical file already has frontmatter, copy as-is; else derive
        // a one-line description and prepend the required frontmatter.
        let contents = if body.trim_start().starts_with("---") {
            body
        } else {
            let description = derive_skill_description(&body);
            // Quote both values: a derived description routinely contains a colon
            // (e.g. "when running the flywheel: capture ..."), which unquoted YAML
            // parses as a nested mapping and Codex then rejects the whole skill.
            format!(
                "---\nname: {}\ndescription: {}\n---\n\n{body}",
                yaml_quote_scalar(name),
                yaml_quote_scalar(&description)
            )
        };
        let out_dir = dest.join(name);
        fs::create_dir_all(&out_dir)?;
        fs::write(out_dir.join("SKILL.md"), contents)?;
    }
    Ok(names.len())
}

/// Render a string as a YAML double-quoted scalar so values containing `:`, `#`,
/// quotes, etc. survive parsing. Only `\` and `"` need escaping inside a
/// double-quoted scalar (derived text is already newline-free).
fn yaml_quote_scalar(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// First meaningful line of a skill body as its Codex `description` (single
/// line, trimmed, capped). Prefers the "Use this skill when …" sentence.
fn derive_skill_description(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("A Maturana skill.");
    let line = line.trim_start_matches("Use this skill ").trim();
    let one_line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = one_line.chars().take(300).collect();
    capped.replace(['\n', '\r'], " ")
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
        // Fresh install with no agents yet: keep the plane healthy (supervise
        // sessiond, idle-waiting) instead of exiting with an error. Maturana is
        // Codex-native - the user builds their first agent from Codex, then
        // restarts `maturana up` to wire its channels/schedules. A failed
        // service on a brand-new box reads as "broken"; an idle one reads as
        // "ready".
        eprintln!(
            "up: no agents configured yet - supervising sessiond only (idle). \
             Build an agent from Codex (`cd <repo> && codex`), then restart `maturana up`."
        );
    }
    // claude-code agents get the host-owned OAuth refresh daemon — EXCEPT
    // firecracker guests, which keep their own token alive.
    //
    // Host-side claude token refresh is for agents that CANNOT refresh their own
    // OAuth token. A firecracker claude guest CAN: its resident run-agent.sh loop
    // refreshes the token in-guest when it nears expiry (worker.rs keep-alive),
    // and the guest owns the (single-use) refresh-token lineage end to end. A
    // host-side refresh of the same seed would only race the guest and consume the
    // token out from under it (-> guest 401). So exclude firecracker profiles —
    // the guest is the SOLE refresher, and reboot recovery never re-pushes auth so
    // a stale host seed is harmless. (Earlier this exclusion was unsafe because the
    // guest only refreshed lazily *during a turn*, so an idle agent's token died at
    // ~8h; the in-guest keep-alive loop is what now makes the guest self-sufficient
    // while idle.)
    let claude_refresh_agents = agent_ids
        .iter()
        .filter(|id| {
            AgentSpec::from_maturana_markdown(&home.agent_dir(id).join("MATURANA.md"))
                .map(|spec| spec.runtime.harness == HarnessRuntime::ClaudeCode)
                .unwrap_or(false)
                && firecracker_profile_for(id.as_str()).is_none()
        })
        .cloned()
        .collect::<Vec<_>>();
    // Compute graph opt-in before `agent_ids` is consumed below. MaturanaGraph is
    // on by default, so this is true unless a spec sets enabled: false.
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
            let slack = spec.as_ref().and_then(|s| s.channels.slack.clone()).map(|s| {
                maturana_core::orchestrator::SlackRuntime {
                    bot_token_source: s.bot_token_source,
                    app_token_source: s.app_token_source,
                }
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
            // A single global `--session-id` would write every channel to one
            // queue, but per-agent guest workers claim from their own profile
            // session id (codex-main, claude-main, …). Derive each agent's
            // session id so the supervised channel matches its guest worker;
            // an explicit `--session-id` still overrides all of them.
            let session_id = match &command.session_id {
                Some(session_id) => session_id.clone(),
                None => infer_agent_session_id(home, &agent_id)?,
            };
            // Each fleet agent has its own Telegram bot; a single shared token
            // would point every channel at the same bot. Prefer the agent's
            // profile token, falling back to the command default for others.
            let telegram_token_source = firecracker_profile_for(&agent_id)
                .map(|profile| profile.telegram_token_source.to_string())
                .unwrap_or_else(|| command.telegram_token_source.clone());
            // Supervise the agent's egress proxy whenever its spec turns the
            // proxy on. Otherwise a proxy.enabled agent has no running proxy and
            // its outbound (harness backend, tools) is refused at a dead port.
            let proxy = spec
                .as_ref()
                .and_then(|s| s.network.proxy.as_ref())
                .map(|p| p.enabled)
                .unwrap_or(false);
            Ok(AgentRuntime {
                agent_id,
                session_id,
                telegram: !command.no_telegram,
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
    // sessiond now refuses to run unauthenticated, so default the token to the
    // persistent per-home token file when the operator did not pass one. This
    // keeps `up` secure-by-default without forcing a manual --sessiond-token.
    let sessiond_token = match command.sessiond_token.clone() {
        Some(token) => Some(token),
        None => Some(ensure_sessiond_token(&home.root().join("sessiond/token"))?),
    };
    // MaturanaGraph is on by default, but the graph service is only supervised
    // when a token exists. On the `up`/Hyper-V path nothing generated it, so a
    // graph-enabled agent would silently get no graph. Ensure the token whenever
    // any selected agent opts in (mirrors the firecracker repair path); fall back
    // to read-only for hosts where the graph is managed manually.
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

fn run_up(home: &MaturanaHome, command: UpCommand) -> anyhow::Result<()> {
    let config = build_orchestrator_config(home, &command)?;
    let plan = plan_processes(&config);

    if command.dry_run {
        // Don't echo secrets: the plan carries `--token <value>` args (sessiond,
        // graph). Redact those before printing the dry-run plan.
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
        // Versioned heartbeat for out-of-process observers (the web cockpit's
        // runtime panel reads this file; there is deliberately no IPC).
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

/// Resolve the Claude credentials path. The default (`.maturana/host-auth/...`)
/// is repo-root-relative, so a bare relative path must resolve against the repo
/// root (the parent of `--home`) — NOT the cwd. Under the boot scheduled task
/// cwd is `System32`, which is exactly where `absolute_or_cwd` went wrong and
/// left the claude token un-refreshed at boot.
fn resolve_claude_creds(home: &MaturanaHome, creds: PathBuf) -> PathBuf {
    if creds.is_absolute() {
        return creds;
    }
    let repo_root = home.root().parent().unwrap_or_else(|| home.root());
    repo_root.join(creds)
}

fn run_claude_refresh(home: &MaturanaHome, command: ClaudeRefreshCommand) -> anyhow::Result<()> {
    use maturana_core::claude_refresh as cr;
    match command.command {
        ClaudeRefreshSubcommand::Probe { creds } => {
            let creds_path = resolve_claude_creds(home, creds);
            let current = cr::read_claude_creds(&creds_path)?;
            let now = chrono::Utc::now().timestamp_millis();
            let mins = (current.expires_at_ms - now) / 60000;
            println!("current token expires in {mins} min; attempting one refresh…");
            let rotated = cr::refresh_claude_token(&current)?;
            cr::write_claude_creds(&creds_path, &rotated)?;
            let new_mins = (rotated.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 60000;
            println!("refresh OK — endpoint verified; new token expires in {new_mins} min");
            println!("(rotated creds written to {})", creds_path.display());
            Ok(())
        }
        ClaudeRefreshSubcommand::Serve {
            agent_ids,
            creds,
            poll_seconds,
        } => {
            let creds_path = resolve_claude_creds(home, creds);
            println!("claude-refresh daemon: watching {}", creds_path.display());
            loop {
                if let Err(error) = claude_refresh_tick(home, &creds_path, &agent_ids) {
                    eprintln!("claude-refresh: {error:#}");
                }
                thread::sleep(Duration::from_secs(poll_seconds.max(30)));
            }
        }
    }
}

/// One daemon cycle: refresh the host token if near expiry, then re-push to any
/// idle named claude agents (skips busy ones; the wide pre-expiry window means
/// the next cycle catches them).
fn claude_refresh_tick(
    home: &MaturanaHome,
    creds_path: &Path,
    agent_ids: &[String],
) -> anyhow::Result<()> {
    use maturana_core::claude_refresh as cr;
    let creds = cr::read_claude_creds(creds_path)?;
    let now = chrono::Utc::now().timestamp_millis();
    if !cr::needs_refresh(&creds, now, cr::REFRESH_SKEW) {
        return Ok(());
    }
    let rotated = cr::refresh_claude_token(&creds)?;
    cr::write_claude_creds(creds_path, &rotated)?;
    let mins = (rotated.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 60000;
    println!("claude-refresh: rotated host token (expires in {mins} min)");
    for agent_id in agent_ids {
        match worker_is_idle(home, agent_id) {
            true => {
                if let Err(error) = repush_claude_auth(home, agent_id) {
                    eprintln!("claude-refresh: re-push {agent_id} failed: {error:#}");
                } else {
                    println!("claude-refresh: re-pushed fresh creds to {agent_id}");
                }
            }
            false => println!("claude-refresh: {agent_id} busy; will re-push next cycle"),
        }
    }
    Ok(())
}

fn worker_is_idle(home: &MaturanaHome, agent_id: &str) -> bool {
    let path = home.agent_dir(agent_id).join("worker-status.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
        .map(|s| s == "idle" || s == "polling")
        .unwrap_or(true) // unknown status → assume safe to push
}

/// Re-push fresh claude creds to a running guest via the existing auth install
/// path (resolves the guest IP from the live provider status).
fn repush_claude_auth(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let status = maturana_core::inspect_agent(home, agent_id)?;
    let ip = status
        .ipv4
        .ok_or_else(|| anyhow::anyhow!("{agent_id} has no IPv4 yet"))?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: agent_id.to_string(),
            // Use the SAME canonical session id as the launch/repair path
            // (`profile.session_id`, e.g. `claude-main`), not `{agent_id}-main`.
            // The claude-refresh daemon re-renders the guest worker + the
            // materialized `sessiond.env` every idle cycle; if it used a
            // different session id than the one the guest worker actually
            // claims from, it silently repoints the host plane (and Telegram)
            // at a dead session while the guest answers on the original one —
            // i.e. the agent stops responding even though everything is "up".
            session_id: firecracker_profile_for(agent_id)
                .map(|profile| profile.session_id.to_string())
                .unwrap_or_else(|| format!("{agent_id}-main")),
            harness: HarnessRuntime::ClaudeCode,
            guest_ip: ip,
            ssh_user: "ubuntu".to_string(),
            ssh_key: PathBuf::from(".maturana/keys/maturana-firecracker.id_rsa"),
            harness_auth_guest_path: "/home/ubuntu/.claude".to_string(),
            sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
            sessiond_token_path: home.root().join("sessiond/token"),
            // Do NOT re-push host-auth over a live guest. Claude Code rotates its
            // OAuth refresh token on every self-refresh (against the allowlisted
            // platform.claude.com), so the guest's `.credentials.json` is newer
            // than the host staging copy. Overwriting it hands the guest an
            // already-consumed, single-use refresh token → hard `401 Invalid
            // authentication credentials`, killing a working agent every refresh
            // cycle. The guest owns its refresh lineage; the daemon only keeps the
            // host seed fresh + re-renders env/runner. To intentionally re-seed a
            // *dead* guest, run `repair guest-worker --auth-source …` by hand once.
            auth_source: None,
            install_harness: false,
            // Moot (auth_source is None), but explicit: the daemon never re-seeds.
            force_reseed_auth: false,
        },
    )
}

fn run_search(home: &MaturanaHome, command: SearchCommand) -> anyhow::Result<()> {
    let query = command.query.join(" ");
    if query.trim().is_empty() {
        anyhow::bail!("search query is empty");
    }
    let provider: maturana_core::search::SearchProviderKind = command.provider.parse()?;
    let results = maturana_core::search::search(
        home.root(),
        provider,
        &maturana_core::search::SearchRequest {
            query,
            count: command.count,
        },
    )?;
    if command.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!("(no results)");
    } else {
        for result in &results {
            println!("{}\n  {}\n  {}\n", result.title, result.url, result.snippet);
        }
    }
    Ok(())
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
            force_reseed_auth,
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
                force_reseed_auth,
            },
        ),
        RepairSubcommand::FirecrackerHarnesses {
            agent_ids,
            ssh_key,
            sessiond_bind,
            sessiond_token_path,
            skip_assets,
            skip_net,
            skip_launch,
            skip_worker_refresh,
            skip_services,
            no_install_harness,
            ssh_wait_seconds,
            force_reseed_auth,
        } => repair_firecracker_harnesses(
            home,
            FirecrackerHarnessRepair {
                agent_ids,
                ssh_key,
                sessiond_bind,
                sessiond_token_path,
                skip_assets,
                skip_net,
                skip_launch,
                skip_worker_refresh,
                skip_services,
                install_harness: !no_install_harness,
                ssh_wait_seconds,
                force_reseed_auth,
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
    skip_net: bool,
    skip_launch: bool,
    skip_worker_refresh: bool,
    skip_services: bool,
    install_harness: bool,
    ssh_wait_seconds: u64,
    force_reseed_auth: bool,
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
    /// Pipelock source for this agent's own Telegram bot token, so `maturana up`
    /// supervises each fleet channel with the right bot (not one shared token).
    telegram_token_source: &'static str,
    auth_source: &'static str,
    auth_guest_path: &'static str,
    spec_path: &'static str,
}

/// Look up a Firecracker fleet profile by agent id, so `maturana up` can wire
/// each agent's channel to the same session id its guest worker claims from.
fn firecracker_profile_for(agent_id: &str) -> Option<&'static FirecrackerHarnessProfile> {
    FIRECRACKER_HARNESS_PROFILES
        .iter()
        .find(|profile| profile.agent_id == agent_id)
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
        telegram_token_source: "pipelock:telegram/bot-token",
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
        telegram_token_source: "pipelock:telegram/opencode-bot-token",
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
        telegram_token_source: "pipelock:telegram/claude-bot-token",
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
    // With --skip-services the plane (sessiond + graph) is owned by the systemd
    // `maturana up` service, so we must NOT start our own copies — they would
    // collide on ports 47834/47835. We still ensure the tokens below, since
    // guest artifacts embed them and `maturana up` reads the graph token to
    // decide whether to supervise the graph service.
    if !repair.skip_services {
        start_linux_sessiond(
            home,
            &repair.sessiond_bind,
            &sessiond_token,
            &sessiond_token_path,
        )?;
    }

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
        if !repair.skip_services {
            start_linux_graph(home, GRAPH_BIND, &graph_token)?;
        }
    }

    // One agent's failure must not block the rest — this is the boot-recovery
    // path, so a single slow/dead guest can't be allowed to strand the others.
    // Collect per-agent errors and surface them all at the end.
    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for profile in selected {
        println!("=== {} ===", profile.agent_id);

        // Un-baked guard: boot recovery (`service install fleet` →
        // `--skip-assets`) must no-op cleanly on a host that has never built
        // this agent's rootfs, instead of failing the launch on a missing
        // image. With --skip-assets we reuse the baked rootfs, so if it's not
        // there yet, skip the whole agent.
        if repair.skip_assets {
            let expected_rootfs = PathBuf::from(format!(
                ".maturana/images/firecracker/{}/ubuntu-rootfs.ext4",
                profile.image_name
            ));
            if !expected_rootfs.exists() {
                println!(
                    "  no baked rootfs at {} — skipping (run without --skip-assets to build it)",
                    expected_rootfs.display()
                );
                continue;
            }
        }

        let result = (|| -> anyhow::Result<()> {
            if !repair.skip_launch {
                let _ = stop_agent(home, profile.agent_id);
            }
            // The TAP device + NAT rule are ephemeral (gone after a host reboot)
            // and cheap to recreate, so they're decoupled from the slow
            // libguestfs asset build: recreated unless --skip-net, even with
            // --skip-assets. The setup script is idempotent (`ip link show`).
            if !repair.skip_net {
                setup_firecracker_tap(profile)?;
            }
            if !repair.skip_assets {
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
                // Record the rootfs's baked host public key so SSH verifies the
                // guest (falls back to accept-new if the image predates pinning).
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
                        force_reseed_auth: repair.force_reseed_auth,
                    },
                )?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            eprintln!("  {} failed: {err:#}", profile.agent_id);
            failures.push((profile.agent_id.to_string(), err));
        }
    }

    if !failures.is_empty() {
        let names = failures
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("Firecracker harness repair failed for: {names}");
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
    // Run the script AS THE USER (through `bash`, since a Windows checkout tracks
    // no +x bit) — NOT wrapped in `sudo`. The script elevates per command via
    // `sudo -n ip/iptables/sysctl`, which the scoped /etc/sudoers.d/90-maturana-net
    // rule (from install-firecracker-host.sh) allows passwordless. Wrapping the
    // whole script in `sudo bash` would instead require NOPASSWD on bash, defeating
    // the scoping; running as root (boot recovery) still works — priv() runs the
    // commands directly.
    run_checked_process(
        ProcessCommand::new("bash")
            .arg("./scripts/firecracker-setup-tap.sh")
            .arg(profile.tap_name)
            .arg(format!("{}/30", profile.host_ip))
            .arg(profile.cidr),
        "setup Firecracker TAP",
    )
}

/// Spawn a dedicated worker VM for an orchestration role by cloning a base
/// Firecracker agent: create a fresh TAP on an allocated address, copy the base's
/// baked rootfs (the drive is mounted read-write, so each VM needs its own),
/// materialize + launch the VM, and install the guest worker reusing the base
/// agent's harness auth. Blocks until the worker is reachable and provisioned.
/// Tear it down with [`orchestrator_teardown_worker`].
pub(crate) fn orchestrator_spawn_worker(
    home: &MaturanaHome,
    base_agent_id: &str,
    new_id: &str,
    session_id: &str,
    net: &maturana_core::orchestrator_spawn::FirecrackerNet,
) -> anyhow::Result<()> {
    let base_profile = firecracker_profile_for(base_agent_id).ok_or_else(|| {
        anyhow::anyhow!(
            "--base-spec '{base_agent_id}' is not a known Firecracker agent to clone; \
             pass an existing materialized firecracker agent id (e.g. codex-firecracker)"
        )
    })?;
    let base_spec =
        AgentSpec::from_maturana_markdown(home.agent_dir(base_agent_id).join("MATURANA.md"))?;
    let base_rootfs = {
        let fc = base_spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("base agent {base_agent_id} is not Firecracker"))?;
        absolute_or_cwd(PathBuf::from(&fc.rootfs_image))?
    };

    // 1. Create the host TAP for the new VM (scoped sudo via the setup script).
    run_checked_process(
        ProcessCommand::new("bash")
            .arg("./scripts/firecracker-setup-tap.sh")
            .arg(&net.tap_name)
            .arg(format!("{}/30", net.host_ip))
            .arg(&net.cidr),
        "setup orchestration worker TAP",
    )?;

    // 2. Give the VM its own rootfs (copy a baked one — no CoW required of users).
    //    Keep it under the spawned agent's own dir: the shared images/ dir is
    //    root-owned (from libguestfs asset prep), but agent dirs are user-owned.
    let rootfs_dir = home.agent_dir(new_id);
    fs::create_dir_all(&rootfs_dir)?;
    let new_rootfs = rootfs_dir.join("ubuntu-rootfs.ext4");
    println!("  spawn {new_id}: copying rootfs (this is a few GB)…");
    fs::copy(&base_rootfs, &new_rootfs)
        .with_context(|| format!("failed to copy rootfs for {new_id}"))?;

    // 2b. The guest's static IP is baked into the image's netplan (matched by the
    //     interface MAC), so the copy still carries the base agent's IP. Regenerate
    //     the netplan for THIS VM's MAC + allocated IP/gateway and write it into the
    //     copy with virt-copy-in, the same tool the image build uses.
    let netplan = format!(
        "network:\n  version: 2\n  ethernets:\n    eth0:\n      match:\n        macaddress: \"{mac}\"\n      set-name: eth0\n      dhcp4: false\n      addresses:\n        - {guest_ip}/30\n      routes:\n        - to: default\n          via: {host_ip}\n      nameservers:\n        addresses:\n          - 1.1.1.1\n          - 8.8.8.8\n",
        mac = net.guest_mac,
        guest_ip = net.guest_ip,
        host_ip = net.host_ip,
    );
    let netplan_file = rootfs_dir.join("50-maturana-firecracker.yaml");
    fs::write(&netplan_file, &netplan)?;
    println!("  spawn {new_id}: rewriting guest netplan to {}", net.guest_ip);
    run_checked_process(
        ProcessCommand::new("virt-copy-in")
            .arg("-a")
            .arg(&new_rootfs)
            .arg(&netplan_file)
            .arg("/etc/netplan"),
        "rewrite spawned guest netplan",
    )?;

    // 3. Derive the per-role spec (unique id + allocated net + this rootfs) and launch.
    let mut spec = maturana_core::orchestrator_spawn::derive_role_spec(&base_spec, new_id, net);
    if let Some(fc) = spec.vm.firecracker.as_mut() {
        fc.rootfs_image = new_rootfs.display().to_string();
    }
    let markdown = spec.to_maturana_markdown()?;
    maturana_core::materialize_agent(&spec, &markdown, home, maturana_core::LaunchMode::Apply)?;

    // 4. Wait for SSH, then install the guest worker (reusing the base's auth).
    let ssh_key = absolute_or_cwd(PathBuf::from(
        ".maturana/images/firecracker/maturana-firecracker.id_rsa",
    ))?;
    let host_key = GuestHostKey::resolve(home, new_id, &net.guest_ip)?;
    println!("  spawn {new_id}: waiting for guest SSH at {}…", net.guest_ip);
    wait_for_guest_ssh(&net.guest_ip, "ubuntu", &ssh_key, &host_key, Duration::from_secs(180))?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: new_id.to_string(),
            session_id: session_id.to_string(),
            harness: base_spec.runtime.harness.clone(),
            guest_ip: net.guest_ip.clone(),
            ssh_user: "ubuntu".to_string(),
            ssh_key,
            harness_auth_guest_path: base_profile.auth_guest_path.to_string(),
            sessiond_url: format!("http://{}:47834", net.host_ip),
            sessiond_token_path: home.root().join("sessiond/token"),
            auth_source: Some(PathBuf::from(base_profile.auth_source)),
            // The cloned rootfs already has the harness baked in (and its auth);
            // just re-point the worker at this VM's session. The re-seed guard
            // skips re-pushing auth the copy already carries.
            install_harness: false,
            force_reseed_auth: false,
        },
    )?;
    println!("  spawn {new_id}: worker provisioned on {}", net.guest_ip);
    Ok(())
}

/// Tear down a spawned worker VM: stop it, remove its TAP, and delete its rootfs
/// copy. Best-effort — each step is independent so a partial spawn still cleans up.
pub(crate) fn orchestrator_teardown_worker(
    home: &MaturanaHome,
    agent_id: &str,
    tap_name: &str,
) -> anyhow::Result<()> {
    let _ = maturana_core::stop_agent(home, agent_id);
    let _ = ProcessCommand::new("sudo")
        .args(["-n", "ip", "link", "del", tap_name])
        .status();
    // Reclaim the spawned VM's whole agent dir (its rootfs copy lives there).
    let _ = fs::remove_dir_all(home.agent_dir(agent_id));
    Ok(())
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
        // `bash <script>` so a Windows-checked-out repo (no +x bit) still runs.
        .arg("bash")
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
    // Keep the last attempt's error so the timeout failure names the real cause
    // (host-key mismatch / auth / connection reset) instead of a black box.
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match run_ssh_with_stdin(guest_ip, ssh_user, ssh_key, host_key, "echo ok", None) {
            Ok(_) => return Ok(()),
            Err(error) => last_err = Some(format!("{error:#}")),
        }
        thread::sleep(Duration::from_secs(2));
    }
    anyhow::bail!(
        "guest SSH did not become reachable at {} within {}s (last SSH error: {})",
        guest_ip,
        timeout.as_secs(),
        last_err.as_deref().unwrap_or("none captured")
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
            // Moot (auth_source is None): the live refresh never re-seeds auth.
            force_reseed_auth: false,
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
    /// Override the re-seed guard: push host auth even if the guest already has a
    /// live `.credentials.json`. Only for recovering a genuinely dead guest — see
    /// `install_guest_worker`'s guard and `guest_has_live_harness_creds`.
    force_reseed_auth: bool,
}

/// Existence-only probe (never reads the token back to the host) of whether the
/// guest already holds its own `.credentials.json`. Used to refuse re-seeding a
/// claude guest that self-refreshes its single-use OAuth lineage.
fn guest_has_live_harness_creds(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    harness_auth_guest_path: &str,
) -> bool {
    let path = format!(
        "{}/.credentials.json",
        harness_auth_guest_path.trim_end_matches('/')
    );
    // `test -s` (present + non-empty) → "LIVE"; otherwise "ABSENT". The `|| echo`
    // keeps the remote exit status 0 so SSH itself never errors on a missing file.
    let cmd = format!("test -s {} && echo LIVE || echo ABSENT", shell_quote(&path));
    match run_ssh_with_stdin(ip, ssh_user, ssh_key, host_key, &cmd, None) {
        Ok(out) => out.trim() == "LIVE",
        // Unreachable/edge: env + runner copies already proved SSH works by this
        // point, so treat a probe failure as "can't confirm live" → allow the
        // push (a truly unreachable guest can't be clobbered anyway).
        Err(_) => false,
    }
}

fn install_guest_worker(home: &MaturanaHome, install: GuestWorkerInstall) -> anyhow::Result<()> {
    let ssh_key = absolute_or_cwd(install.ssh_key)?;
    let sessiond_token = read_optional_trimmed(absolute_or_cwd(install.sessiond_token_path)?)?;

    let state_dir = home.agent_dir(&install.agent_id).join("state");
    fs::create_dir_all(&state_dir)?;
    let env_path = state_dir.join("sessiond.env");
    let runner_path = state_dir.join("run-agent.sh");
    // The post-boot re-render also carries the graph env so it isn't lost when a
    // worker is refreshed. Read the agent's materialized spec for its opt-in
    // (graph) and its MCP servers.
    let agent_spec =
        AgentSpec::from_maturana_markdown(&home.agent_dir(&install.agent_id).join("MATURANA.md"))
            .ok();
    let knowledge_graph = agent_spec
        .as_ref()
        .map(|spec| spec.knowledge_graph.clone())
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
    // Resolve + GUARD the auth re-seed exactly once. Pushing host creds over a
    // guest that already self-refreshes its own (single-use, rotating) token hands
    // it an already-consumed refresh token → hard 401, "logged out again". So for
    // claude-code, refuse to clobber a guest that already has a live
    // `.credentials.json` unless the operator explicitly forces a re-seed (only
    // valid for a genuinely dead guest). Initial provisioning is unaffected: a
    // fresh guest has no creds yet → the probe says ABSENT → the push proceeds.
    let auth_push_path: Option<PathBuf> = match install.auth_source.as_ref() {
        None => None,
        Some(src) => {
            let resolved = absolute_or_cwd(src.clone())?;
            if !resolved.exists() {
                None
            } else if install.harness == HarnessRuntime::ClaudeCode
                && !install.force_reseed_auth
                && guest_has_live_harness_creds(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &install.harness_auth_guest_path,
                )
            {
                eprintln!(
                    "guest-worker: NOT re-seeding claude auth for {} — guest {} already \
                     has a live {}/.credentials.json (it self-refreshes its own OAuth \
                     lineage). Overwriting it would consume a single-use refresh token \
                     and log the agent out. Pass --force-reseed-auth to override (only \
                     to recover a dead guest).",
                    install.agent_id, install.guest_ip, install.harness_auth_guest_path,
                );
                None
            } else {
                Some(resolved)
            }
        }
    };
    if let Some(auth_source) = auth_push_path.as_ref() {
        copy_path_to_guest(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            auth_source,
            "/tmp/maturana-harness-auth",
            true,
        )?;
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
    if auth_push_path.is_some() {
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
    // MCP config: render the harness-native file (secrets resolved host-side)
    // and place it where the in-guest harness reads it.
    if let Some(spec) = agent_spec.as_ref() {
        if !spec.mcp_servers.is_empty() {
            let host_auth_dir = install
                .auth_source
                .as_ref()
                .map(|p| absolute_or_cwd(p.clone()))
                .transpose()?
                .unwrap_or_else(|| {
                    home.root()
                        .join("host-auth")
                        .join(maturana_core::worker::harness_name(&install.harness))
                });
            if let Some(rendered) = maturana_core::mcp::render_mcp_config(
                &install.harness,
                &spec.mcp_servers,
                home.root(),
                &host_auth_dir,
            )? {
                let mcp_path = state_dir.join("mcp-config");
                fs::write(&mcp_path, &rendered.contents)?;
                copy_path_to_guest(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &mcp_path,
                    "/tmp/maturana-mcp-config",
                    false,
                )?;
                let guest_path = &rendered.guest_path;
                let parent = posix_parent(guest_path);
                run_ssh_with_stdin(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &format!(
                        "sudo mkdir -p {parent} && sudo mv /tmp/maturana-mcp-config {path} && sudo chown -R {user}:{user} {parent} && sudo chmod 0600 {path}",
                        parent = shell_quote(parent),
                        path = shell_quote(guest_path),
                        user = shell_quote(&install.ssh_user),
                    ),
                    None,
                )?;
                println!("installed MCP config ({} servers) at {guest_path}", spec.mcp_servers.len());
            }
            // Pre-install npx-launched MCP servers globally so the harness runs
            // the resident binary the config now points at (see
            // mcp::launch_invocation) instead of re-resolving via `npx` on every
            // model turn (~4.5s/turn saved). Idempotent; tolerate transient npm
            // failures (a re-provision repairs it) rather than abort the agent.
            let npm_pkgs: Vec<String> = spec
                .mcp_servers
                .iter()
                .filter_map(|s| maturana_core::mcp::npx_package(s.command.as_deref(), &s.args))
                .collect();
            if !npm_pkgs.is_empty() {
                let quoted = npm_pkgs.iter().map(|p| shell_quote(p)).collect::<Vec<_>>().join(" ");
                match run_ssh_with_stdin(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &format!("sudo npm install -g {quoted}"),
                    None,
                ) {
                    Ok(_) => println!(
                        "pre-installed {} resident MCP server(s): {}",
                        npm_pkgs.len(),
                        npm_pkgs.join(", ")
                    ),
                    Err(e) => eprintln!(
                        "warning: failed to pre-install resident MCP server(s) [{}]: {e} — \
                         the harness may be slow or the server unavailable until re-provisioned",
                        npm_pkgs.join(", ")
                    ),
                }
            }
        }
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

/// One row of the `list` / `status` agent table — a host-side snapshot, no SSH.
#[derive(Debug, Clone, serde::Serialize)]
struct AgentRow {
    agent: String,
    harness: String,
    vm: String,
    queue: String,
    last_turn: String,
}

fn humanize_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn humanize_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Build the per-agent snapshot rows shared by `list` and `status`. Every lookup
/// is guarded so one broken agent can't blank the whole table.
fn collect_agent_rows(home: &MaturanaHome) -> Vec<AgentRow> {
    discover_agent_ids(home)
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let harness = AgentSpec::from_maturana_markdown(home.agent_dir(&id).join("MATURANA.md"))
                .ok()
                .map(|s| maturana_core::worker::harness_name(&s.runtime.harness).to_string())
                .unwrap_or_else(|| "?".to_string());
            let vm = maturana_core::materialize::inspect_agent(home, &id)
                .map(|s| s.state)
                .unwrap_or_else(|_| "unknown".to_string());
            let (queue, last_turn) = match infer_agent_session_id(home, &id) {
                Ok(sid) => {
                    let paths = session_paths(&home.agent_dir(&id), &sid);
                    let queue = queue_stats(&paths)
                        .map(|q| {
                            if q.pending == 0 && q.processing == 0 {
                                "idle".to_string()
                            } else if q.processing == 0 {
                                format!("{} pend", q.pending)
                            } else {
                                format!("{} pend/{} proc", q.pending, q.processing)
                            }
                        })
                        .unwrap_or_else(|_| "idle".to_string());
                    let last_turn = list_recent_inbound(&paths, 1)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map(|m| {
                            humanize_age((Utc::now() - m.created_at).num_seconds().max(0) as u64)
                        })
                        .unwrap_or_else(|| "—".to_string());
                    (queue, last_turn)
                }
                Err(_) => ("idle".to_string(), "—".to_string()),
            };
            AgentRow {
                agent: id,
                harness,
                vm,
                queue,
                last_turn,
            }
        })
        .collect()
}

fn print_agent_table(rows: &[AgentRow], indent: &str) {
    let headers = ["AGENT", "HARNESS", "VM", "QUEUE", "LAST TURN"];
    let mut w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows {
        for (i, cell) in [&r.agent, &r.harness, &r.vm, &r.queue]
            .iter()
            .enumerate()
        {
            w[i] = w[i].max(cell.chars().count());
        }
    }
    let line = |c: [&str; 5]| {
        format!(
            "{indent}{:<aw$}  {:<hw$}  {:<vw$}  {:<qw$}  {}",
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            aw = w[0],
            hw = w[1],
            vw = w[2],
            qw = w[3],
        )
    };
    println!("{}", line(headers));
    for r in rows {
        println!(
            "{}",
            line([&r.agent, &r.harness, &r.vm, &r.queue, &r.last_turn])
        );
    }
}

fn run_list(home: &MaturanaHome, command: ListCommand) -> anyhow::Result<()> {
    let rows = collect_agent_rows(home);
    if command.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "No agents found under {}.\nIf your agents live elsewhere, pass --home (or set \
             MATURANA_HOME); otherwise create one with `maturana agent launch`.",
            home.agents_dir().display()
        );
        return Ok(());
    }
    print_agent_table(&rows, "");
    Ok(())
}

fn run_status(home: &MaturanaHome, command: StatusCommand) -> anyhow::Result<()> {
    let rows = collect_agent_rows(home);
    // The plane writes <home>/up/state.json every tick; its presence is the
    // authoritative "is `maturana up` running" signal (same file the cockpit uses).
    let up_state = fs::read_to_string(home.root().join("up").join("state.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let sessiond = doctor_http_health("http://127.0.0.1:47834/health");
    let graph = doctor_http_health("http://127.0.0.1:47835/health");

    if command.json {
        let out = serde_json::json!({
            "plane": up_state,
            "sessiond_ok": sessiond.ok,
            "graph_ok": graph.ok,
            "agents": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("PLANE");
    match &up_state {
        Some(state) => {
            let pid = state.get("pid").and_then(|v| v.as_u64());
            println!(
                "  supervisor                          running{}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
            println!(
                "  sessiond :47834                     {}",
                if sessiond.ok { "ok" } else { "DOWN" }
            );
            println!(
                "  graph    :47835                     {}",
                if graph.ok { "ok" } else { "not running" }
            );
            if let Some(procs) = state.get("processes").and_then(|v| v.as_array()) {
                for p in procs {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    if name == "sessiond" || name == "graph" {
                        continue;
                    }
                    let restarts = p.get("restarts").and_then(|v| v.as_u64()).unwrap_or(0);
                    let up = p.get("uptime_seconds").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!(
                        "  {name:<34}  running  restarts={restarts}  up={}",
                        humanize_uptime(up)
                    );
                }
            }
        }
        None => {
            println!(
                "  plane NOT running — start it with `maturana up` (or `maturana service install up`)."
            );
        }
    }
    println!();
    println!("AGENTS");
    if rows.is_empty() {
        println!("  (none under {})", home.agents_dir().display());
    } else {
        print_agent_table(&rows, "  ");
    }
    Ok(())
}

pub(crate) fn discover_agent_ids(home: &MaturanaHome) -> anyhow::Result<Vec<String>> {
    let agents_dir = home.agents_dir();
    if !agents_dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(agents_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let id = entry.file_name().to_string_lossy().to_string();
            // Only supervise materialized agents (a real agent has a MATURANA.md).
            // Skips phantom dirs — e.g. a session created for a bogus agent id —
            // that would otherwise spawn duplicate channel runners on the same bot.
            if home.agent_dir(&id).join("MATURANA.md").exists() {
                ids.push(id);
            }
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
    text: &str,
) -> anyhow::Result<QueuedAgentRun> {
    // `agent run` is a chat surface too — route it through the SAME front door as
    // every channel (records the turn, injects the recent transcript for memory,
    // attaches model/reasoning), keyed `cli`/`agent-run`. So a multi-turn
    // `agent run` conversation remembers, exactly like Telegram/TUI/web.
    let session_id = infer_agent_session_id(home, agent_id)?;
    let message_id = crate::channels::enqueue_turn(
        home,
        agent_id,
        &session_id,
        "cli",
        "agent-run",
        crate::channels::stable_chat_key("agent-run"),
        None,
        text,
        serde_json::json!({}),
    )?;
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

/// One synchronous chat turn for the console TUI (`maturana agent chat`):
/// enqueue the prompt into sessiond and block until the agent's reply (or
/// timeout). Called from a background thread so the TUI stays responsive.
pub(crate) fn agent_chat_turn(
    home: &MaturanaHome,
    agent_id: &str,
    prompt: &str,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    // The console TUI goes through the SAME shared front door as every other
    // channel (Telegram, Discord, web), so it gets identical behaviour: the user
    // turn recorded under `console_chat_key`, the recent transcript injected
    // (turn-to-turn memory), and the current model/reasoning attached. This is the
    // single place channel turns are enqueued — see channels::enqueue_turn.
    let session_id = infer_agent_session_id(home, agent_id)?;
    let message_id = crate::channels::enqueue_turn(
        home,
        agent_id,
        &session_id,
        "console",
        "console:tui",
        crate::channels::console_chat_key(),
        None,
        prompt,
        serde_json::json!({}),
    )?;
    let queued = QueuedAgentRun {
        session_id,
        message_id,
    };
    let completed = wait_for_agent_run(home, agent_id, &queued, timeout_seconds)?;
    // Strip the onboarding-complete sentinel + end the interview if the agent
    // signalled it (same as the telegram delivery paths).
    Ok(crate::channels::finalize_onboarding_reply(home, agent_id, &completed.text))
}

pub(crate) fn infer_agent_session_id(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<String> {
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

    fn up_command_for_test(agent_ids: Vec<&str>, session_id: Option<&str>) -> UpCommand {
        UpCommand {
            agent_ids: agent_ids.into_iter().map(String::from).collect(),
            sessiond_bind: "0.0.0.0:47834".to_string(),
            sessiond_token: Some("tok".to_string()),
            session_id: session_id.map(String::from),
            telegram_token_source: "pipelock:telegram/bot-token".to_string(),
            no_telegram: false,
            no_schedules: false,
            no_proactive: false,
            channel_poll_seconds: 5,
            schedule_poll_seconds: 60,
            dry_run: false,
        }
    }

    #[test]
    fn build_orchestrator_config_derives_per_agent_session_ids() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-up-session-derive-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);

        // codex-firecracker has a materialized sessiond.env (highest priority);
        // claude-firecracker has none, so it falls back to its profile default.
        let state_dir = home.agent_dir("codex-firecracker").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='codex-rendered'\n",
        )
        .unwrap();

        let command = up_command_for_test(vec!["codex-firecracker", "claude-firecracker"], None);
        let config = build_orchestrator_config(&home, &command).unwrap();

        let codex = config
            .agents
            .iter()
            .find(|a| a.agent_id == "codex-firecracker")
            .unwrap();
        let claude = config
            .agents
            .iter()
            .find(|a| a.agent_id == "claude-firecracker")
            .unwrap();
        // Materialized spec wins for codex; profile default for claude. They are
        // NOT collapsed onto a single global session id.
        assert_eq!(codex.session_id, "codex-rendered");
        assert_eq!(claude.session_id, "claude-main");

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn build_orchestrator_config_session_id_flag_overrides_all_agents() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-up-session-override-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);

        let command = up_command_for_test(
            vec!["codex-firecracker", "claude-firecracker"],
            Some("global-session"),
        );
        let config = build_orchestrator_config(&home, &command).unwrap();
        assert!(config
            .agents
            .iter()
            .all(|a| a.session_id == "global-session"));

        let _ = fs::remove_dir_all(&temp);
    }

    fn parse_firecracker_repair(args: &[&str]) -> FirecrackerHarnessRepair {
        let cli = Cli::try_parse_from(args).expect("parse repair firecracker-harnesses");
        match cli.command {
            Command::Repair(RepairCommand {
                command:
                    RepairSubcommand::FirecrackerHarnesses {
                        agent_ids,
                        ssh_key,
                        sessiond_bind,
                        sessiond_token_path,
                        skip_assets,
                        skip_net,
                        skip_launch,
                        skip_worker_refresh,
                        skip_services,
                        no_install_harness,
                        ssh_wait_seconds,
                        force_reseed_auth,
                    },
            }) => FirecrackerHarnessRepair {
                agent_ids,
                ssh_key,
                sessiond_bind,
                sessiond_token_path,
                skip_assets,
                skip_net,
                skip_launch,
                skip_worker_refresh,
                skip_services,
                install_harness: !no_install_harness,
                ssh_wait_seconds,
                force_reseed_auth,
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn skip_services_flag_parses_and_defaults_false() {
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.skip_services,
            "--skip-services must default to false so a bare repair still owns the plane"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--skip-services",
        ]);
        assert!(with.skip_services);
    }

    #[test]
    fn force_reseed_auth_defaults_false_and_parses() {
        // Default off: a routine firecracker repair must NOT re-seed (clobber) a
        // live claude guest's self-refreshing OAuth token. See install_guest_worker.
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.force_reseed_auth,
            "--force-reseed-auth must default to false so repair never clobbers a live guest token"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--force-reseed-auth",
        ]);
        assert!(with.force_reseed_auth);
    }

    #[test]
    fn guest_worker_force_reseed_auth_flag_parses() {
        // The manual `setup/repair guest-worker --auth-source …` path also gates
        // the destructive re-seed behind --force-reseed-auth.
        fn parse(args: &[&str]) -> bool {
            match Cli::try_parse_from(args)
                .expect("parse guest-worker")
                .command
            {
                Command::Repair(RepairCommand {
                    command: RepairSubcommand::GuestWorker {
                        force_reseed_auth, ..
                    },
                }) => force_reseed_auth,
                other => panic!("unexpected command: {other:?}"),
            }
        }
        let base = [
            "maturana",
            "repair",
            "guest-worker",
            "--agent-id",
            "claude-firecracker",
            "--session-id",
            "claude-main",
            "--harness",
            "claude-code",
            "--harness-auth-guest-path",
            "/home/ubuntu/.claude",
        ];
        assert!(!parse(&base), "guest-worker --force-reseed-auth defaults false");
        let mut forced = base.to_vec();
        forced.push("--force-reseed-auth");
        assert!(parse(&forced));
    }

    #[test]
    fn skip_net_flag_defaults_false_and_parses() {
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.skip_net,
            "--skip-net must default to false so boot recovery recreates the ephemeral TAP"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--skip-net",
        ]);
        assert!(with.skip_net);
    }

    #[test]
    fn firecracker_profiles_carry_per_agent_telegram_tokens() {
        // `maturana up` reads these so each fleet channel uses its own bot,
        // matching the per-agent session id its guest worker claims from.
        assert_eq!(
            firecracker_profile_for("codex-firecracker").unwrap().telegram_token_source,
            "pipelock:telegram/bot-token"
        );
        assert_eq!(
            firecracker_profile_for("claude-firecracker").unwrap().telegram_token_source,
            "pipelock:telegram/claude-bot-token"
        );
        assert_eq!(
            firecracker_profile_for("opencode-firecracker").unwrap().telegram_token_source,
            "pipelock:telegram/opencode-bot-token"
        );
        // Session id and token agree per agent (no cross-wiring).
        assert_eq!(
            firecracker_profile_for("claude-firecracker").unwrap().session_id,
            "claude-main"
        );
        assert!(firecracker_profile_for("not-a-fleet-agent").is_none());
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
    fn codex_prompts_quote_descriptions_with_colons() {
        // A derived description whose first line contains a colon ("speech:")
        // must be emitted as a quoted YAML scalar, else Codex rejects the skill
        // ("mapping values are not allowed in this context").
        let temp = std::env::temp_dir().join(format!(
            "maturana-codex-prompts-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&temp);
        let skills_root = temp.join("skills");
        let skill_dir = skills_root.join("voicey");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# voicey\n\nUse this skill when an agent needs speech: transcribe audio to text.\n",
        )
        .unwrap();
        let dest = temp.join("dest");
        let count = sync_codex_prompts(&skills_root, Some(&dest)).unwrap();
        assert_eq!(count, 1);

        let generated = fs::read_to_string(dest.join("voicey").join("SKILL.md")).unwrap();
        let desc_line = generated
            .lines()
            .find(|l| l.starts_with("description:"))
            .expect("description line present");
        // The value must be wrapped in double quotes so the inner colon is safe.
        let value = desc_line.trim_start_matches("description:").trim();
        assert!(value.starts_with('"') && value.ends_with('"'), "description not quoted: {desc_line}");
        assert!(value.contains("speech:"), "colon-bearing text preserved: {desc_line}");
        // name is quoted too.
        assert!(generated.contains("name: \"voicey\""));

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
        // The opencode example spec declares network.proxy (Brave injection), so
        // the guest proxy.env IS rendered, pointing at the host-TAP proxy bind.
        let proxy_env_path = artifacts
            .proxy_env
            .expect("proxy.env should be rendered for a spec with network.proxy");
        let proxy_env = fs::read_to_string(&proxy_env_path).unwrap();
        assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
        assert!(proxy_env.contains("MATURANA_PROXY_HOST=172.30.10.5"));
        assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));

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
