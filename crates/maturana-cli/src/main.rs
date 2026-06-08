mod channels;
mod personal;
mod session;

use anyhow::Context;
use channels::{handle_channel, paired_telegram_chat_source, ChannelCommand};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use maturana_core::{
    audit::{append_event, AuditEvent},
    materialize_agent,
    pipelock::PipelockVault,
    pipelock_proxy::{ensure_mitm_ca_cert, run_proxy, HeaderInjection, ProxyConfig},
    secrets::resolve_secret_source_with_home,
    spec::AgentSpec,
    state::MaturanaHome,
    validate_spec, LaunchMode,
};
use personal::{
    handle_deploy, handle_heartbeat, handle_personal, handle_schedule, handle_wiki, DeployCommand,
    HeartbeatCommand, PersonalCommand, ScheduleCommand, WikiCommand,
};
use session::{handle_session, SessionCommand};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
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
    Channel(ChannelCommand),
    Session(SessionCommand),
    Doctor(DoctorCommand),
}

#[derive(Debug, Args)]
struct SpecCommand {
    #[command(subcommand)]
    command: SpecSubcommand,
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
                if live {
                    if let Some(ip) = ip {
                        print_live_guest_state(&ip, &ssh_user, &ssh_key)?;
                        audit_agent_event(
                            &home,
                            &agent_id,
                            "agent.inspect.live.ssh",
                            format!("inspected live guest at {ip} over ssh"),
                        )?;
                    } else {
                        print_live_agent_state(&agent_id)?;
                        audit_agent_event(
                            &home,
                            &agent_id,
                            "agent.inspect.live.hostd",
                            "inspected live VM state through hostd",
                        )?;
                    }
                }
            }
            AgentSubcommand::Stop { agent_id, live } => {
                if !live {
                    anyhow::bail!(
                        "agent stop currently requires --live and a running maturana-hostd"
                    );
                }
                stop_live_agent(&agent_id)?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.stop.live",
                    "stopped live VM through hostd",
                )?;
            }
            AgentSubcommand::Run {
                agent_id,
                prompt,
                prompt_file,
                ip,
                ssh_user,
                ssh_key,
                wait,
                timeout_seconds,
            } => {
                let prompt = read_agent_prompt(prompt, prompt_file)?;
                let ip = match ip {
                    Some(ip) => ip,
                    None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not discover live IP for {agent_id}; pass --ip explicitly"
                        )
                    })?,
                };
                submit_live_prompt(&ip, &ssh_user, &ssh_key, &prompt)?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "agent.run.live",
                    format!("submitted live prompt to guest at {ip}"),
                )?;
                println!("submitted prompt to {agent_id} at {ip}");
                if wait {
                    let output = wait_for_live_run(&ip, &ssh_user, &ssh_key, timeout_seconds)?;
                    audit_agent_event(
                        &home,
                        &agent_id,
                        "agent.run.live.completed",
                        format!("live prompt completed on guest at {ip}"),
                    )?;
                    println!("{output}");
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
                let output = read_live_log(&ip, &ssh_user, &ssh_key, kind, lines)?;
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
                fetch_live_path(
                    &ip,
                    &ssh_user,
                    &ssh_key,
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
                push_live_path(
                    &ip,
                    &ssh_user,
                    &ssh_key,
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
                if live {
                    list_live_snapshots(&agent_id)?;
                    audit_agent_event(
                        &home,
                        &agent_id,
                        "snapshot.list.live",
                        "listed live Hyper-V checkpoints through hostd",
                    )?;
                    return Ok(());
                }
                let snapshots = home.agent_dir(&agent_id).join("snapshots");
                if !snapshots.exists() {
                    anyhow::bail!("agent does not exist or has no snapshots: {agent_id}");
                }
                for entry in fs::read_dir(snapshots)? {
                    println!("{}", entry?.file_name().to_string_lossy());
                }
            }
            SnapshotSubcommand::Take {
                agent_id,
                name,
                live,
            } => {
                let snapshot_dir = home.agent_dir(&agent_id).join("snapshots").join(&name);
                fs::create_dir_all(&snapshot_dir)?;
                fs::write(snapshot_dir.join("README.md"), "MVP snapshot marker.\n")?;
                println!("snapshot marker created at {}", snapshot_dir.display());
                if live {
                    take_live_snapshot(
                        &agent_id,
                        snapshot_dir.file_name().unwrap().to_string_lossy().as_ref(),
                    )?;
                    audit_agent_event(
                        &home,
                        &agent_id,
                        "snapshot.take.live",
                        format!("created live Hyper-V checkpoint {name} through hostd"),
                    )?;
                }
            }
            SnapshotSubcommand::Restore {
                agent_id,
                name,
                live,
            } => {
                if !live {
                    anyhow::bail!(
                        "snapshot restore currently requires --live and a running maturana-hostd"
                    );
                }
                restore_live_snapshot(&agent_id, &name)?;
                audit_agent_event(
                    &home,
                    &agent_id,
                    "snapshot.restore.live",
                    format!("restored live Hyper-V checkpoint {name} through hostd"),
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
        Command::Channel(command) => handle_channel(command, &home)?,
        Command::Session(command) => handle_session(command, &home)?,
        Command::Doctor(command) => run_doctor(&home, command)?,
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

fn print_live_agent_state(agent_id: &str) -> anyhow::Result<()> {
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
    let vm = payload
        .get("vms")
        .and_then(|value| value.as_array())
        .and_then(|vms| {
            vms.iter().find(|vm| {
                vm.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name == expected_name)
                    .unwrap_or(false)
            })
        });

    if let Some(vm) = vm {
        println!(
            "live.vm: {}",
            vm.get("name").unwrap_or(&serde_json::Value::Null)
        );
        println!(
            "live.state: {}",
            vm.get("state").unwrap_or(&serde_json::Value::Null)
        );
        println!(
            "live.ipv4: {}",
            vm.get("ipv4").unwrap_or(&serde_json::Value::Null)
        );
        println!(
            "live.uptime: {}",
            vm.get("uptime").unwrap_or(&serde_json::Value::Null)
        );
    } else {
        println!("live.vm: not found");
    }

    Ok(())
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

fn print_live_guest_state(ip: &str, ssh_user: &str, ssh_key: &PathBuf) -> anyhow::Result<()> {
    let remote = r#"set -eu
echo "live.guest: $(hostname)"
echo "live.codex: $(command -v codex 2>/dev/null || true)"
codex --version 2>/dev/null | sed 's/^/live.codex_version: /' || true
echo "live.service: $(systemctl is-active maturana-agent.service 2>/dev/null || true)"
echo "live.rootfs: $(df -h / | awk 'NR==2 {print $2 " total, " $4 " free"}')"
echo "live.heartbeat: $(cat /var/log/maturana/heartbeat 2>/dev/null || true)"
echo "live.last_message:"
cat /var/log/maturana/last-message.txt 2>/dev/null || true
"#;
    let output = run_ssh_with_stdin(ip, ssh_user, ssh_key, remote, None)?;
    print!("{output}");
    Ok(())
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

fn submit_live_prompt(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    prompt: &str,
) -> anyhow::Result<()> {
    let remote_command = format!(
        "cat > /tmp/maturana-prompt.txt && sudo mv /tmp/maturana-prompt.txt /agent/prompt.txt && sudo chown {ssh_user}:{ssh_user} /agent/prompt.txt && sudo chmod 0644 /agent/prompt.txt && sudo rm -f /agent/run-command /var/log/maturana/run.done && sudo systemctl restart maturana-agent.service"
    );
    run_ssh_with_stdin(ip, ssh_user, ssh_key, &remote_command, Some(prompt)).map(|_| ())
}

fn wait_for_live_run(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    let attempts = timeout_seconds.max(1);
    for _ in 0..attempts {
        let done = run_ssh_with_stdin(
            ip,
            ssh_user,
            ssh_key,
            "test -f /var/log/maturana/run.done && cat /var/log/maturana/last-message.txt",
            None,
        );
        if let Ok(output) = done {
            if !output.trim().is_empty() {
                return Ok(output);
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    anyhow::bail!("timed out waiting for live agent run to finish")
}

fn read_live_log(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    kind: LogKind,
    lines: u16,
) -> anyhow::Result<String> {
    let path = kind.guest_path();
    let command = if matches!(kind, LogKind::LastMessage) {
        format!("test -f {path} && cat {path} || true")
    } else {
        format!("test -f {path} && tail -n {} {path} || true", lines.max(1))
    };
    run_ssh_with_stdin(ip, ssh_user, ssh_key, &command, None)
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
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PreferredAuthentications=publickey")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", null_known_hosts()))
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
        run_ssh_with_stdin(ip, ssh_user, ssh_key, &mkdir, None)?;
    }

    let mut command = ProcessCommand::new("scp");
    command
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", null_known_hosts()))
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
    remote_command: &str,
    stdin_text: Option<&str>,
) -> anyhow::Result<String> {
    let mut command = ProcessCommand::new("ssh");
    command
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", null_known_hosts()))
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

fn null_known_hosts() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn list_live_snapshots(agent_id: &str) -> anyhow::Result<()> {
    let response = hostd_get(&format!("/agents/snapshot/list?agent_id={agent_id}"))?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd snapshot list returned an error: {payload}");
    }
    if let Some(snapshots) = payload.get("snapshots").and_then(|value| value.as_array()) {
        for snapshot in snapshots {
            let name = snapshot
                .get("Name")
                .or_else(|| snapshot.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or("<unnamed>");
            let created = snapshot
                .get("CreationTime")
                .or_else(|| snapshot.get("creation_time"))
                .map(|value| value.to_string())
                .unwrap_or_default();
            println!("{name} {created}");
        }
    }
    Ok(())
}

fn take_live_snapshot(agent_id: &str, name: &str) -> anyhow::Result<()> {
    let response = hostd_post_json(
        "/agents/snapshot/take",
        serde_json::json!({
            "agent_id": agent_id,
            "name": name,
        }),
    )?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd snapshot take returned an error: {payload}");
    }
    println!("live snapshot created: {name}");
    Ok(())
}

fn restore_live_snapshot(agent_id: &str, name: &str) -> anyhow::Result<()> {
    let response = hostd_post_json(
        "/agents/snapshot/restore",
        serde_json::json!({
            "agent_id": agent_id,
            "name": name,
        }),
    )?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd snapshot restore returned an error: {payload}");
    }
    println!("live snapshot restored: {name}");
    Ok(())
}

fn stop_live_agent(agent_id: &str) -> anyhow::Result<()> {
    let response = hostd_post_json(
        "/agents/stop",
        serde_json::json!({
            "agent_id": agent_id,
        }),
    )?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd stop returned an error: {payload}");
    }
    println!("live agent stopped: {agent_id}");
    Ok(())
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

fn hostd_post_json(path: &str, body: serde_json::Value) -> anyhow::Result<ureq::Response> {
    let mut request = ureq::post(&hostd_url(path));
    if let Some(token) = hostd_token()? {
        request = request.set("X-Maturana-Hostd-Token", &token);
    }
    Ok(request.send_json(body)?)
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
}
