use anyhow::Context;
use chrono::{DateTime, Datelike, Timelike, Utc};
use clap::{Args, Subcommand, ValueEnum};
use maturana_core::{
    audit::{append_event, AuditEvent},
    session_db::{ensure_session, insert_inbound, session_paths},
    spec::AgentSpec,
    state::MaturanaHome,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::Duration,
};

#[derive(Debug, Args)]
pub struct PersonalCommand {
    #[command(subcommand)]
    pub command: PersonalSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum PersonalSubcommand {
    Init {
        agent_id: String,
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub struct WikiCommand {
    #[command(subcommand)]
    pub command: WikiSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum WikiSubcommand {
    Init,
    Ingest {
        path: PathBuf,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value_t = 1800)]
        chunk_chars: usize,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

#[derive(Debug, Args)]
pub struct HeartbeatCommand {
    #[command(subcommand)]
    pub command: HeartbeatSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum HeartbeatSubcommand {
    Beat {
        agent_id: String,
        #[arg(long, default_value = "alive")]
        status: String,
        #[arg(long)]
        message: Option<String>,
    },
    Status {
        agent_id: String,
    },
}

#[derive(Debug, Args)]
pub struct ScheduleCommand {
    #[command(subcommand)]
    pub command: ScheduleSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleSubcommand {
    Add {
        agent_id: String,
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        channel: Option<String>,
    },
    List {
        agent_id: String,
    },
    RunDue {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
        #[arg(long)]
        now: Option<String>,
    },
    Serve {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
        #[arg(long, default_value_t = 60)]
        poll_seconds: u64,
    },
}

#[derive(Debug, Args)]
pub struct DeployCommand {
    #[command(subcommand)]
    pub command: DeploySubcommand,
}

#[derive(Debug, Args)]
pub struct DevelopCommand {
    #[command(subcommand)]
    pub command: DevelopSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DevelopSubcommand {
    Skill {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value = "skills")]
        root: PathBuf,
    },
    Tool {
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value = "tools")]
        root: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeploySubcommand {
    Skill(DeployItem),
    Tool(DeployItem),
}

#[derive(Debug, Args)]
pub struct DeployItem {
    pub agent_id: String,
    pub path: PathBuf,
    #[arg(long)]
    pub ip: String,
    #[arg(long, default_value = "ubuntu")]
    pub ssh_user: String,
    #[arg(
        long,
        env = "MATURANA_AGENT_SSH_KEY",
        default_value = ".maturana/keys/maturana-agent-ed25519"
    )]
    pub ssh_key: PathBuf,
    #[arg(long)]
    pub guest_path: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DeployKind {
    Skill,
    Tool,
}

#[derive(Debug, Serialize, Deserialize)]
struct WikiChunkRecord {
    id: String,
    source: String,
    title: String,
    chunk_path: String,
    chars: usize,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HeartbeatRecord {
    agent_id: String,
    status: String,
    message: Option<String>,
    at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ScheduleRecord {
    id: String,
    agent_id: String,
    name: String,
    cron: String,
    prompt: String,
    channel: Option<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
}

pub fn handle_personal(command: PersonalCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        PersonalSubcommand::Init {
            agent_id,
            spec,
            force,
        } => init_personal_agent(home, &agent_id, spec.as_deref(), force),
    }
}

pub fn handle_wiki(command: WikiCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        WikiSubcommand::Init => {
            init_wiki(home)?;
            println!("wiki initialized at {}", wiki_dir(home).display());
            Ok(())
        }
        WikiSubcommand::Ingest {
            path,
            title,
            chunk_chars,
        } => ingest_wiki(home, &path, title, chunk_chars),
        WikiSubcommand::Search { query, limit } => search_wiki(home, &query, limit),
    }
}

pub fn handle_heartbeat(command: HeartbeatCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        HeartbeatSubcommand::Beat {
            agent_id,
            status,
            message,
        } => write_heartbeat(home, &agent_id, &status, message),
        HeartbeatSubcommand::Status { agent_id } => read_heartbeat(home, &agent_id),
    }
}

pub fn handle_schedule(command: ScheduleCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        ScheduleSubcommand::Add {
            agent_id,
            name,
            cron,
            prompt,
            channel,
        } => add_schedule(home, &agent_id, &name, &cron, &prompt, channel),
        ScheduleSubcommand::List { agent_id } => list_schedules(home, &agent_id),
        ScheduleSubcommand::RunDue {
            agent_id,
            session_id,
            now,
        } => run_due_schedules(home, &agent_id, &session_id, now.as_deref()),
        ScheduleSubcommand::Serve {
            agent_id,
            session_id,
            poll_seconds,
        } => serve_schedules(home, &agent_id, &session_id, poll_seconds),
    }
}

pub fn handle_deploy(command: DeployCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        DeploySubcommand::Skill(item) => deploy_item(home, DeployKind::Skill, item),
        DeploySubcommand::Tool(item) => deploy_item(home, DeployKind::Tool, item),
    }
}

pub fn handle_develop(command: DevelopCommand) -> anyhow::Result<()> {
    match command.command {
        DevelopSubcommand::Skill {
            name,
            description,
            root,
        } => scaffold_skill(&root, &name, description.as_deref()),
        DevelopSubcommand::Tool {
            name,
            description,
            root,
        } => scaffold_tool(&root, &name, description.as_deref()),
    }
}

fn init_personal_agent(
    home: &MaturanaHome,
    agent_id: &str,
    spec_path: Option<&Path>,
    force: bool,
) -> anyhow::Result<()> {
    let agent_dir = home.agent_dir(agent_id);
    fs::create_dir_all(agent_dir.join("context"))?;
    fs::create_dir_all(agent_dir.join("memory/daily"))?;
    fs::create_dir_all(agent_dir.join("skills"))?;
    fs::create_dir_all(agent_dir.join("tools"))?;
    fs::create_dir_all(agent_dir.join("schedules"))?;
    init_wiki(home)?;

    let identity = if let Some(path) = spec_path {
        let spec = AgentSpec::from_maturana_markdown(path)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        (spec.identity.name, spec.identity.purpose)
    } else {
        (
            agent_id.to_string(),
            "A personal Maturana agent with bounded VM execution.".to_string(),
        )
    };

    write_if_missing(
        &agent_dir.join("AGENTS.md"),
        &format!(
            "# {}\n\nRead `/agent/IDENTITY.md`, `/agent/SOUL.md`, `/memory/MEMORY.md`, `/wiki/INDEX.md`, and `/agent/MATURANA.md` before acting.\n\nOperate only through declared tools, mounted paths, channels, schedules, and pipelock-governed egress.\n",
            identity.0
        ),
        force,
    )?;
    write_if_missing(
        &agent_dir.join("IDENTITY.md"),
        &format!(
            "# Identity — {}\n\n## Who I am\n{} — {}\n\n## Who you are to me\n<Your owner: name, how to address you, timezone, and what you rely on me for.>\n\n## Scope & boundaries\n- In scope: <…>\n- Out of scope: <…>\n",
            identity.0, identity.0, identity.1
        ),
        force,
    )?;
    write_if_missing(
        &agent_dir.join("SOUL.md"),
        &format!(
            "# Soul — {}\n\nPurpose: {}\n\n## Voice\n<Tone, formality, brevity, humor.>\n\n## Behavior\nDefault posture: useful, calm, secure, and concise. Ask for approval before writing long-term personal memory unless the user explicitly says to remember something.\n",
            identity.0, identity.1
        ),
        force,
    )?;
    write_if_missing(
        &agent_dir.join("memory/MEMORY.md"),
        "# Memory\n\nDurable facts, preferences, and commitments for this agent.\n",
        force,
    )?;
    write_if_missing(
        &agent_dir.join("context/README.md"),
        "# Context\n\nAgent-specific working context. Use the shared wiki for reusable knowledge.\n",
        force,
    )?;

    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: "personal.init".to_string(),
            message: format!(
                "initialized personal agent files in {}",
                agent_dir.display()
            ),
        },
    )?;
    println!("personal agent initialized at {}", agent_dir.display());
    Ok(())
}

fn init_wiki(home: &MaturanaHome) -> anyhow::Result<()> {
    fs::create_dir_all(wiki_dir(home).join("chunks"))?;
    write_if_missing(
        &wiki_dir(home).join("INDEX.md"),
        "# Maturana Wiki\n\nShared markdown context for agents. Ingested chunks live in `chunks/`.\n",
        false,
    )
}

fn ingest_wiki(
    home: &MaturanaHome,
    path: &Path,
    title: Option<String>,
    chunk_chars: usize,
) -> anyhow::Result<()> {
    init_wiki(home)?;
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let title = title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("untitled")
            .to_string()
    });
    let slug = slugify(&title);
    let chunks = chunk_markdown(&raw, chunk_chars.max(400));
    let chunk_dir = wiki_dir(home).join("chunks");
    let mut records = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let id = format!("{slug}-{:03}", index + 1);
        let chunk_path = chunk_dir.join(format!("{id}.md"));
        fs::write(
            &chunk_path,
            format!(
                "---\ntitle: {}\nsource: {}\nchunk: {}\n---\n\n{}",
                title,
                path.display(),
                index + 1,
                chunk.trim()
            ),
        )?;
        records.push(WikiChunkRecord {
            id,
            source: path.display().to_string(),
            title: title.clone(),
            chunk_path: chunk_path.display().to_string(),
            chars: chunk.len(),
            created_at: Utc::now(),
        });
    }
    append_wiki_index(home, &title, path, &records)?;
    println!(
        "ingested {} chunks into {}",
        records.len(),
        chunk_dir.display()
    );
    Ok(())
}

fn search_wiki(home: &MaturanaHome, query: &str, limit: usize) -> anyhow::Result<()> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .collect();
    if terms.is_empty() {
        anyhow::bail!("wiki search query must not be empty");
    }
    let chunk_dir = wiki_dir(home).join("chunks");
    if !chunk_dir.exists() {
        println!("no wiki chunks found");
        return Ok(());
    }
    let mut hits = Vec::new();
    for entry in fs::read_dir(chunk_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let lower = raw.to_ascii_lowercase();
        let score = terms
            .iter()
            .filter(|term| lower.contains(term.as_str()))
            .count();
        if score > 0 {
            hits.push((score, path, first_content_line(&raw)));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (score, path, preview) in hits.into_iter().take(limit.max(1)) {
        println!("score={score} {} :: {preview}", path.display());
    }
    Ok(())
}

fn write_heartbeat(
    home: &MaturanaHome,
    agent_id: &str,
    status: &str,
    message: Option<String>,
) -> anyhow::Result<()> {
    let agent_dir = home.agent_dir(agent_id);
    fs::create_dir_all(&agent_dir)?;
    let record = HeartbeatRecord {
        agent_id: agent_id.to_string(),
        status: status.to_string(),
        message,
        at: Utc::now(),
    };
    fs::write(
        agent_dir.join("HEARTBEAT.json"),
        serde_json::to_string_pretty(&record)?,
    )?;
    fs::write(
        agent_dir.join("HEARTBEAT.md"),
        format!(
            "# Heartbeat\n\n- agent: {}\n- status: {}\n- at: {}\n- message: {}\n",
            record.agent_id,
            record.status,
            record.at.to_rfc3339(),
            record.message.as_deref().unwrap_or("")
        ),
    )?;
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: "heartbeat.beat".to_string(),
            message: format!("heartbeat status={status}"),
        },
    )?;
    println!("heartbeat written for {agent_id}: {status}");
    Ok(())
}

fn read_heartbeat(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let path = home.agent_dir(agent_id).join("HEARTBEAT.json");
    if !path.exists() {
        anyhow::bail!("heartbeat not found for {agent_id}");
    }
    println!("{}", fs::read_to_string(path)?);
    Ok(())
}

fn add_schedule(
    home: &MaturanaHome,
    agent_id: &str,
    name: &str,
    cron: &str,
    prompt: &str,
    channel: Option<String>,
) -> anyhow::Result<()> {
    let path = schedules_path(home, agent_id);
    let mut schedules = read_schedules(&path)?;
    let id = slugify(name);
    schedules.retain(|schedule| schedule.id != id);
    schedules.push(ScheduleRecord {
        id: id.clone(),
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        cron: cron.to_string(),
        prompt: prompt.to_string(),
        channel,
        enabled: true,
        created_at: Utc::now(),
    });
    write_schedules(&path, &schedules)?;
    println!("schedule added: {id}");
    Ok(())
}

fn list_schedules(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let schedules = read_schedules(&schedules_path(home, agent_id))?;
    if schedules.is_empty() {
        println!("no schedules for {agent_id}");
        return Ok(());
    }
    for schedule in schedules {
        println!(
            "{} enabled={} cron={} channel={} prompt={}",
            schedule.id,
            schedule.enabled,
            schedule.cron,
            schedule.channel.as_deref().unwrap_or(""),
            schedule.prompt
        );
    }
    Ok(())
}

fn run_due_schedules(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    now: Option<&str>,
) -> anyhow::Result<()> {
    let now = match now {
        Some(value) => DateTime::parse_from_rfc3339(value)
            .with_context(|| format!("invalid --now timestamp: {value}"))?
            .with_timezone(&Utc),
        None => Utc::now(),
    };
    let schedules = read_schedules(&schedules_path(home, agent_id))?;
    let mut fired = 0;
    for schedule in schedules.iter().filter(|schedule| schedule.enabled) {
        if !cron_matches(&schedule.cron, now)? {
            continue;
        }
        if !mark_schedule_run(home, agent_id, &schedule.id, now)? {
            continue;
        }
        enqueue_schedule(home, agent_id, session_id, schedule, now)?;
        fired += 1;
    }
    println!("schedules fired: {fired}");
    Ok(())
}

fn serve_schedules(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    poll_seconds: u64,
) -> anyhow::Result<()> {
    println!("schedule runner serving agent {agent_id}");
    loop {
        if let Err(error) = run_due_schedules(home, agent_id, session_id, None) {
            eprintln!("schedule runner error: {error}");
        }
        thread::sleep(Duration::from_secs(poll_seconds.max(1)));
    }
}

fn enqueue_schedule(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    schedule: &ScheduleRecord,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let channel = schedule.channel.as_deref().unwrap_or("schedule");
    // A telegram schedule is a reminder TO the user. Route it through the shared
    // outreach front door so the reply is tagged for the REAL chat (telegram +
    // chat_id) and actually gets delivered — tagging platform_id = schedule.id
    // meant the telegram delivery loop (which matches platform_id == chat_id) never
    // delivered the reply, the same dropped-reply bug fixed for proactivity. The
    // front door also injects context so the reminder turn has memory.
    let chat_id = (channel == "telegram")
        .then(|| crate::channels::current_paired_telegram_chat_id(home, agent_id))
        .flatten();
    let id = if let Some(chat_id) = chat_id {
        crate::channels::enqueue_outreach_turn(
            home,
            agent_id,
            session_id,
            chat_id,
            &schedule.prompt,
            "schedule",
            serde_json::json!({
                "schedule_id": schedule.id,
                "schedule_name": schedule.name,
                "scheduled_at": now,
            }),
        )?
    } else {
        // Legacy tagging for non-telegram channels (or telegram with no paired chat).
        let paths = session_paths(&home.agent_dir(agent_id), session_id);
        ensure_session(&paths)?;
        let content = serde_json::json!({
            "text": schedule.prompt,
            "prompt": schedule.prompt,
            "schedule_id": schedule.id,
            "schedule_name": schedule.name,
            "scheduled_at": now,
        })
        .to_string();
        insert_inbound(&paths, "schedule", channel, &schedule.id, None, &content)?
    };
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: now,
            agent_id: agent_id.to_string(),
            action: "schedule.fired".to_string(),
            message: format!("{} enqueued as {id}", schedule.id),
        },
    )?;
    Ok(())
}

fn mark_schedule_run(
    home: &MaturanaHome,
    agent_id: &str,
    schedule_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let path = schedule_last_run_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut runs = read_schedule_runs(&path)?;
    let minute_key = now.format("%Y-%m-%dT%H:%MZ").to_string();
    if runs.get(schedule_id).map(String::as_str) == Some(minute_key.as_str()) {
        return Ok(false);
    }
    runs.insert(schedule_id.to_string(), minute_key);
    fs::write(path, serde_json::to_string_pretty(&runs)?)?;
    Ok(true)
}

fn cron_matches(cron: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let fields = cron.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        anyhow::bail!("cron must have 5 fields: {cron}");
    }
    Ok(matches_cron_field(fields[0], now.minute(), 0, 59)?
        && matches_cron_field(fields[1], now.hour(), 0, 23)?
        && matches_cron_field(fields[2], now.day(), 1, 31)?
        && matches_cron_field(fields[3], now.month(), 1, 12)?
        && matches_cron_field(fields[4], now.weekday().num_days_from_sunday(), 0, 6)?)
}

fn matches_cron_field(field: &str, value: u32, min: u32, max: u32) -> anyhow::Result<bool> {
    for part in field.split(',') {
        let part = part.trim();
        if part == "*" {
            return Ok(true);
        }
        if let Some(step) = part.strip_prefix("*/") {
            let step = step.parse::<u32>()?;
            if step == 0 {
                anyhow::bail!("cron step cannot be 0");
            }
            if value % step == 0 {
                return Ok(true);
            }
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<u32>()?;
            let end = end.parse::<u32>()?;
            if start < min || end > max || start > end {
                anyhow::bail!("cron range out of bounds: {part}");
            }
            if (start..=end).contains(&value) {
                return Ok(true);
            }
            continue;
        }
        let exact = part.parse::<u32>()?;
        if exact < min || exact > max {
            anyhow::bail!("cron value out of bounds: {part}");
        }
        if exact == value {
            return Ok(true);
        }
    }
    Ok(false)
}

fn deploy_item(home: &MaturanaHome, kind: DeployKind, item: DeployItem) -> anyhow::Result<()> {
    if !item.path.exists() {
        anyhow::bail!("deploy path does not exist: {}", item.path.display());
    }
    let name = item
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("deploy path has no file name"))?;
    let base = match kind {
        DeployKind::Skill => "/agent/skills",
        DeployKind::Tool => "/agent/tools",
    };
    let guest_path = item.guest_path.unwrap_or_else(|| format!("{base}/{name}"));
    let parent = guest_path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .unwrap_or(base);
    // Verify the guest host key (strict if pinned for this agent, else
    // accept-new) so a deploy can't push skills/tools to an impostor.
    let state_dir = home.agent_dir(&item.agent_id).join("state");
    let (known_hosts, strict) =
        maturana_core::ssh_pin::prepare_known_hosts(&state_dir, &item.ip)?;
    let host_key_opts = maturana_core::ssh_pin::ssh_host_key_options(&known_hosts, strict);
    run_ssh(
        &item.ip,
        &item.ssh_user,
        &item.ssh_key,
        &host_key_opts,
        &format!("mkdir -p {}", shell_quote(parent)),
    )?;
    run_scp(
        &item.ip,
        &item.ssh_user,
        &item.ssh_key,
        &host_key_opts,
        &item.path,
        &guest_path,
    )?;
    append_event(
        home.audit_dir().join(format!("{}.jsonl", item.agent_id)),
        &AuditEvent {
            at: Utc::now(),
            agent_id: item.agent_id.clone(),
            action: format!(
                "deploy.{}",
                match kind {
                    DeployKind::Skill => "skill",
                    DeployKind::Tool => "tool",
                }
            ),
            message: format!("deployed {} to {}", item.path.display(), guest_path),
        },
    )?;
    println!("deployed {} to {}", item.path.display(), guest_path);
    Ok(())
}

fn scaffold_skill(root: &Path, name: &str, description: Option<&str>) -> anyhow::Result<()> {
    let slug = slugify(name);
    let dir = root.join(&slug);
    fs::create_dir_all(&dir)?;
    let description = description.unwrap_or("Use this skill for a Maturana capability.");
    write_if_missing(
        &dir.join("SKILL.md"),
        &format!(
            "# {slug}\n\n{description}\n\nThis is a state-aware Maturana workflow. Keep the implementation small, but do not collapse it into a thin CLI wrapper.\n\n## Grounding\n\n1. Read `/agent/MATURANA.md`, `/agent/AGENTS.md`, and `/agent/SOUL.md`.\n2. Identify the target agent, harness, allowed capabilities, and relevant memory/wiki context.\n3. Inspect existing skills and tools before creating duplicate behavior.\n\n## Preflight\n\n- Confirm the requested action is inside the agent contract.\n- Confirm required credentials are available through the approved path.\n- Confirm no raw secrets will be written to specs, source, logs, or audit output.\n- Confirm the smallest useful verification command before changing state.\n\n## Decision Path\n\n- If the task is policy or procedure, keep it in this skill.\n- If the task needs side effects, call an approved tool or Maturana command.\n- If the task needs a new executable capability, develop a tool first and deploy it separately.\n- If the task is outside the contract, stop and ask for an updated spec.\n\n## Actions\n\n1. Gather the minimum evidence needed for the task.\n2. Run the smallest approved command or tool invocation.\n3. Record durable facts in `/memory/MEMORY.md` only when they should survive future sessions.\n4. Prefer wiki updates for reusable shared context.\n\n## Evidence\n\nBefore claiming success, collect:\n\n- The relevant contract or policy file path.\n- The command/tool output or file change that proves the action completed.\n- Any audit entry, heartbeat, snapshot, or deploy evidence created by the action.\n- Any memory or wiki update made intentionally.\n\n## Recovery\n\n- Missing contract: inspect or initialize the agent before continuing.\n- Missing credential: request or configure the approved credential source; do not paste secrets into files.\n- Failed command: inspect logs/audit before retrying.\n- Out-of-scope request: update `MATURANA.md` and validate before acting.\n\n## Boundaries\n\n- Do not bypass spec validation.\n- Do not add a command queue or generic host command endpoint.\n- Do not store raw secrets in source, docs, memory, wiki, or audit logs.\n- Do not use shell scripts for provider state machines.\n- Do not deploy untested tools into a guest VM.\n"
        ),
        false,
    )?;
    println!("skill scaffolded: {}", dir.display());
    Ok(())
}

fn scaffold_tool(root: &Path, name: &str, description: Option<&str>) -> anyhow::Result<()> {
    let slug = slugify(name);
    let dir = root.join(&slug);
    fs::create_dir_all(&dir)?;
    write_if_missing(
        &dir.join("README.md"),
        &format!(
            "# {slug}\n\n{}\n\nBuild and test this tool in Codex, then deploy it with `maturana deploy tool`.\n",
            description.unwrap_or("A Maturana guest tool.")
        ),
        false,
    )?;
    write_if_missing(
        &dir.join("run.sh"),
        "#!/usr/bin/env bash\nset -euo pipefail\nprintf '%s\\n' \"tool scaffold: replace me\"\n",
        false,
    )?;
    println!("tool scaffolded: {}", dir.display());
    Ok(())
}

fn write_if_missing(path: &Path, contents: &str, force: bool) -> anyhow::Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn wiki_dir(home: &MaturanaHome) -> PathBuf {
    home.root().join("wiki")
}

fn schedules_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("schedules/schedules.json")
}

fn schedule_last_run_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("schedules/last-run.json")
}

fn append_wiki_index(
    home: &MaturanaHome,
    title: &str,
    source: &Path,
    records: &[WikiChunkRecord],
) -> anyhow::Result<()> {
    let index_path = wiki_dir(home).join("INDEX.md");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path)?;
    writeln!(
        file,
        "\n## {}\n\n- source: {}\n- chunks: {}\n- ingested: {}\n",
        title,
        source.display(),
        records.len(),
        Utc::now().to_rfc3339()
    )?;
    for record in records {
        writeln!(file, "- `{}` {}", record.id, record.chunk_path)?;
    }
    Ok(())
}

fn chunk_markdown(raw: &str, chunk_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in raw.lines() {
        let starts_section = line.starts_with('#') && !current.trim().is_empty();
        if (starts_section || current.len() + line.len() + 1 > chunk_chars)
            && !current.trim().is_empty()
        {
            chunks.push(current.trim().to_string());
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

fn slugify(value: &str) -> String {
    let slug: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        format!("item-{:x}", hasher.finish())
    } else {
        slug
    }
}

fn first_content_line(raw: &str) -> String {
    raw.lines()
        .find(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with("---") && !line.contains(':')
        })
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

fn read_schedules(path: &Path) -> anyhow::Result<Vec<ScheduleRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn write_schedules(path: &Path, schedules: &[ScheduleRecord]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(schedules)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn read_schedule_runs(path: &Path) -> anyhow::Result<std::collections::HashMap<String, String>> {
    if !path.exists() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn run_scp(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key_opts: &[String],
    local_path: &Path,
    remote_path: &str,
) -> anyhow::Result<()> {
    let mut command = ProcessCommand::new("scp");
    command
        .args(host_key_opts)
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if local_path.is_dir() {
        command.arg("-r");
    }
    command
        .arg(local_path)
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command.output().context("failed to start scp")?;
    if !output.status.success() {
        anyhow::bail!("scp failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

fn run_ssh(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key_opts: &[String],
    remote_command: &str,
) -> anyhow::Result<()> {
    let output = ProcessCommand::new("ssh")
        .args(host_key_opts)
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key)
        .arg(format!("{ssh_user}@{ip}"))
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to start ssh")?;
    if !output.status.success() {
        anyhow::bail!("ssh failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn init_personal_agent_creates_memory_and_preserves_it_by_default() {
        let home = test_home("personal-init");
        init_personal_agent(&home, "demo", None, false).unwrap();

        let agent_dir = home.agent_dir("demo");
        assert!(agent_dir.join("AGENTS.md").exists());
        assert!(agent_dir.join("SOUL.md").exists());
        assert!(agent_dir.join("context/README.md").exists());
        assert!(agent_dir.join("memory/MEMORY.md").exists());
        assert!(home.root().join("wiki/INDEX.md").exists());

        let memory_path = agent_dir.join("memory/MEMORY.md");
        fs::write(&memory_path, "remember this\n").unwrap();
        init_personal_agent(&home, "demo", None, false).unwrap();
        assert_eq!(fs::read_to_string(&memory_path).unwrap(), "remember this\n");

        init_personal_agent(&home, "demo", None, true).unwrap();
        assert!(fs::read_to_string(&memory_path)
            .unwrap()
            .contains("Durable facts"));
    }

    #[test]
    fn wiki_ingest_writes_chunks_and_index() {
        let home = test_home("wiki-ingest");
        let source = home.root().join("source.md");
        fs::create_dir_all(home.root()).unwrap();
        fs::write(
            &source,
            "# Alpha\n\nSecurity context.\n\n## Beta\n\nNetwork policy and memory.\n",
        )
        .unwrap();

        ingest_wiki(&home, &source, Some("Agent Notes".to_string()), 400).unwrap();

        let chunk_dir = home.root().join("wiki/chunks");
        let chunks = fs::read_dir(&chunk_dir).unwrap().count();
        assert_eq!(chunks, 2);
        let index = fs::read_to_string(home.root().join("wiki/INDEX.md")).unwrap();
        assert!(index.contains("Agent Notes"));
        assert!(index.contains("agent-notes-001"));
    }

    #[test]
    fn heartbeat_writes_markdown_json_and_audit() {
        let home = test_home("heartbeat");

        write_heartbeat(&home, "demo", "alive", Some("ready".to_string())).unwrap();

        let raw = fs::read_to_string(home.agent_dir("demo").join("HEARTBEAT.json")).unwrap();
        let heartbeat: HeartbeatRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(heartbeat.agent_id, "demo");
        assert_eq!(heartbeat.status, "alive");
        assert_eq!(heartbeat.message.as_deref(), Some("ready"));
        assert!(
            fs::read_to_string(home.agent_dir("demo").join("HEARTBEAT.md"))
                .unwrap()
                .contains("ready")
        );
        assert!(fs::read_to_string(home.root().join("audit/demo.jsonl"))
            .unwrap()
            .contains("heartbeat.beat"));
    }

    #[test]
    fn schedules_are_stored_and_replace_by_slug() {
        let home = test_home("schedule");

        add_schedule(
            &home,
            "demo",
            "Morning Brief",
            "0 9 * * *",
            "Brief me",
            Some("telegram".to_string()),
        )
        .unwrap();
        add_schedule(
            &home,
            "demo",
            "Morning Brief",
            "30 9 * * *",
            "Brief me later",
            Some("discord".to_string()),
        )
        .unwrap();

        let schedules = read_schedules(&schedules_path(&home, "demo")).unwrap();
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].id, "morning-brief");
        assert_eq!(schedules[0].cron, "30 9 * * *");
        assert_eq!(schedules[0].channel.as_deref(), Some("discord"));
    }

    #[test]
    fn schedule_run_due_enqueues_once_per_minute() {
        let home = test_home("schedule-run-due");
        add_schedule(&home, "demo", "Every Minute", "* * * * *", "ping", None).unwrap();
        run_due_schedules(&home, "demo", "main", Some("2026-06-08T12:34:00Z")).unwrap();
        run_due_schedules(&home, "demo", "main", Some("2026-06-08T12:34:30Z")).unwrap();
        let paths = session_paths(&home.agent_dir("demo"), "main");
        let messages = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].kind, "schedule");
        assert!(messages[0].content.contains("ping"));
    }

    #[test]
    fn cron_matching_supports_steps_ranges_and_lists() {
        let now = DateTime::parse_from_rfc3339("2026-06-08T12:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(cron_matches("*/5 12 * * *", now).unwrap());
        assert!(cron_matches("15,30,45 10-13 * * *", now).unwrap());
        assert!(!cron_matches("31 12 * * *", now).unwrap());
    }

    #[test]
    fn scaffold_skill_and_tool_create_expected_files() {
        let home = test_home("develop");
        scaffold_skill(&home.root().join("skills"), "Example Skill", Some("Demo")).unwrap();
        scaffold_tool(&home.root().join("tools"), "Example Tool", Some("Demo")).unwrap();
        let skill = fs::read_to_string(home.root().join("skills/example-skill/SKILL.md")).unwrap();
        for section in [
            "## Grounding",
            "## Preflight",
            "## Decision Path",
            "## Actions",
            "## Evidence",
            "## Recovery",
            "## Boundaries",
        ] {
            assert!(skill.contains(section), "missing {section}");
        }
        assert!(!skill.contains("## Procedure"));
        assert!(skill.contains("do not collapse it into a thin CLI wrapper"));
        assert!(home.root().join("tools/example-tool/run.sh").exists());
    }

    #[test]
    fn chunking_and_slugging_are_stable() {
        let chunks = chunk_markdown("# One\nbody\n## Two\nbody\n", 400);
        assert_eq!(chunks.len(), 2);
        assert_eq!(slugify("Morning Brief!"), "morning-brief");
        assert!(slugify("!!!").starts_with("item-"));
    }

    fn test_home(name: &str) -> MaturanaHome {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("maturana-{name}-{now}"));
        MaturanaHome::new(path)
    }
}
