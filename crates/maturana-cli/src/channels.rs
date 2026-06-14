use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use maturana_core::{
    animation::{frame, is_terminal, Phase},
    audit::{append_event, AuditEvent},
    improvement::{signals, TrajectoryStore},
    pipelock::PipelockVault,
    secrets::resolve_secret_source_with_home,
    session_db::{
        ensure_session, insert_inbound, list_undelivered, mark_delivered, session_paths,
        SessionPaths,
    },
    spec::{AgentSpec, HarnessRuntime},
    state::MaturanaHome,
    tools::{run_tool, ToolRegistry},
};
use std::sync::mpsc;
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use crate::session::{message_text, run_session_once, RunnerOptions};

const TELEGRAM_PAIR_CODE: &str = "telegram/pair-code";
const TELEGRAM_CHAT_ID: &str = "telegram/chat-id";
const MAX_RESPONSE_CHARS: usize = 3500;
const IDENTITY_CONTEXT_CHARS: usize = 4000;
const SOUL_CONTEXT_CHARS: usize = 4000;
const CONTRACT_CONTEXT_CHARS: usize = 5000;
const MEMORY_CONTEXT_CHARS: usize = 5000;
const AGENT_CONTEXT_CHARS: usize = 3000;
const WIKI_INDEX_CONTEXT_CHARS: usize = 5000;
const WIKI_CHUNK_CONTEXT_CHARS: usize = 6000;
const TRANSCRIPT_CONTEXT_CHARS: usize = 8000;
const CONTEXT_WIKI_CHUNK_LIMIT: usize = 3;

#[derive(Debug, Args)]
pub struct ChannelCommand {
    #[command(subcommand)]
    pub command: ChannelSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ChannelSubcommand {
    Pair {
        #[command(subcommand)]
        command: ChannelPairSubcommand,
    },
    Serve {
        #[command(subcommand)]
        command: ChannelServeSubcommand,
    },
    Status,
}

#[derive(Debug, Subcommand)]
pub enum ChannelPairSubcommand {
    Telegram {
        #[command(subcommand)]
        command: TelegramPairSubcommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TelegramPairSubcommand {
    Start {
        #[arg(long, default_value = "default")]
        agent_id: String,
        #[arg(long, default_value = "pipelock:telegram/bot-token")]
        token_source: String,
    },
    Complete {
        #[arg(long, default_value = "default")]
        agent_id: String,
        #[arg(long, default_value = "pipelock:telegram/bot-token")]
        token_source: String,
        #[arg(long, default_value_t = 60)]
        timeout_seconds: u64,
    },
    Status {
        #[arg(long, default_value = "default")]
        agent_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelServeSubcommand {
    Telegram(TelegramServe),
    Slack(SlackServe),
    Agentmail(AgentMailServe),
}

#[derive(Debug, Args)]
pub struct SlackServe {
    #[arg(long)]
    pub agent_id: String,
    #[arg(long, default_value = "slack-main")]
    pub session_id: String,
    #[arg(long, default_value = "pipelock:slack/bot-token")]
    pub bot_token_source: String,
    #[arg(long, default_value = "pipelock:slack/app-token")]
    pub app_token_source: String,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub run_once_provider: Option<String>,
}

#[derive(Debug, Args)]
pub struct AgentMailServe {
    #[arg(long)]
    pub agent_id: String,
    #[arg(long, default_value = "agentmail-main")]
    pub session_id: String,
    #[arg(long, default_value = "pipelock:agentmail/api-key")]
    pub api_key_source: String,
    /// Inbox id; omitted uses the account default inbox.
    #[arg(long)]
    pub inbox: Option<String>,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub run_once_provider: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub poll_seconds: u64,
}

#[derive(Debug, Args)]
pub struct TelegramServe {
    #[arg(long)]
    pub agent_id: String,
    #[arg(long, default_value = "telegram-main")]
    pub session_id: String,
    #[arg(long, default_value = "pipelock:telegram/bot-token")]
    pub token_source: String,
    #[arg(long)]
    pub once: bool,
    #[arg(long)]
    pub run_once_provider: Option<String>,
    #[arg(long, default_value_t = 5)]
    pub poll_seconds: u64,
    #[arg(long, default_value_t = 600)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct TelegramChannelState {
    offset: Option<i64>,
    last_seen_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChannelContextManifest {
    at: DateTime<Utc>,
    agent_id: String,
    chat_id: i64,
    source_files: Vec<LoadedContextFile>,
    wiki_chunks: Vec<LoadedWikiChunkSummary>,
    wiki_query_terms: Vec<String>,
    wiki_term_sources: Vec<WikiTermSource>,
    #[serde(default)]
    graph_name: Option<String>,
    #[serde(default)]
    graph_context_chars: usize,
    context_policy: ContextPolicySummary,
    loaded_context_chars: usize,
    transcript_path: String,
    transcript_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoadedContextFile {
    label: String,
    path: String,
    chars: usize,
    missing: bool,
}

#[derive(Debug, Clone)]
struct ContextFile {
    contents: String,
    summary: LoadedContextFile,
}

#[derive(Debug)]
struct ChannelContextBundle {
    identity: ContextFile,
    soul: ContextFile,
    contract: ContextFile,
    memory: ContextFile,
    agent_context: ContextFile,
    wiki_index: ContextFile,
    wiki_chunks: Vec<LoadedWikiChunk>,
    wiki_query_terms: Vec<String>,
    wiki_term_sources: Vec<WikiTermSource>,
    /// GraphRAG context from the agent's knowledge graph, when enabled.
    graph_context: Option<GraphChannelContext>,
    /// Few-shot examples from past positively-rewarded turns (self-improvement).
    learned_examples: String,
    transcript: String,
    transcript_path: PathBuf,
}

#[derive(Debug, Clone)]
struct GraphChannelContext {
    graph: String,
    rendered: String,
}

#[derive(Debug, Clone)]
struct LoadedWikiChunk {
    score: usize,
    matched_terms: Vec<String>,
    path: PathBuf,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoadedWikiChunkSummary {
    score: usize,
    matched_terms: Vec<String>,
    path: String,
    chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WikiTermSource {
    term: String,
    sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextPolicySummary {
    strategy: String,
    wiki_chunk_limit: usize,
    wiki_char_budget: usize,
    transcript_char_budget: usize,
    excludes_reset_marker: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramGetMeResponse {
    ok: bool,
    result: Option<TelegramUser>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdatesResponse {
    ok: bool,
    result: Vec<TelegramUpdate>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    channel_post: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    document: Option<TelegramDocument>,
    chat: TelegramChat,
}

/// A Telegram document attachment (file upload). The bot API caps `getFile`
/// downloads at 20 MB, so anything larger is refused up front.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TelegramDocument {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramSendResponse {
    ok: bool,
    result: Option<TelegramSentMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramOkResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Clone, PartialEq)]
enum InboundAction {
    Ignore,
    Pair {
        chat_id: i64,
    },
    Help {
        chat_id: i64,
    },
    Status {
        chat_id: i64,
    },
    New {
        chat_id: i64,
    },
    Spawn {
        chat_id: i64,
        mode: SpawnMode,
        name: String,
        prompt: String,
    },
    Prompt {
        chat_id: i64,
        text: String,
    },
    Document {
        chat_id: i64,
        document: TelegramDocument,
        caption: Option<String>,
    },
    Tool {
        chat_id: i64,
        name: String,
        input: String,
    },
    Feedback {
        chat_id: i64,
        value: f64,
    },
    Command {
        chat_id: i64,
        name: String,
        args: String,
    },
    Deny {
        chat_id: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnMode {
    Ephemeral,
    Persistent,
}

pub fn handle_channel(command: ChannelCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        ChannelSubcommand::Pair { command } => match command {
            ChannelPairSubcommand::Telegram { command } => match command {
                TelegramPairSubcommand::Start {
                    agent_id,
                    token_source,
                } => start_telegram_pair(home, &agent_id, &token_source),
                TelegramPairSubcommand::Complete {
                    agent_id,
                    token_source,
                    timeout_seconds,
                } => complete_telegram_pair(home, &agent_id, &token_source, timeout_seconds),
                TelegramPairSubcommand::Status { agent_id } => {
                    telegram_pair_status(home, &agent_id)
                }
            },
        },
        ChannelSubcommand::Serve { command } => match command {
            ChannelServeSubcommand::Telegram(config) => serve_telegram(home, config),
            ChannelServeSubcommand::Slack(config) => serve_slack(home, config),
            ChannelServeSubcommand::Agentmail(config) => serve_agentmail(home, config),
        },
        ChannelSubcommand::Status => channel_status(home),
    }
}

pub fn paired_telegram_chat_source(home: &MaturanaHome) -> Option<String> {
    let vault = PipelockVault::new(home.pipelock_dir());
    if vault.get(TELEGRAM_CHAT_ID).is_ok() {
        Some(format!("pipelock:{TELEGRAM_CHAT_ID}"))
    } else {
        None
    }
}

fn start_telegram_pair(
    home: &MaturanaHome,
    agent_id: &str,
    token_source: &str,
) -> anyhow::Result<()> {
    let token = resolve_secret_source_with_home(token_source, home.root())?;
    let bot_name = telegram_bot_username(token.expose_for_runtime())?;
    let code = generate_pair_code();
    let vault = PipelockVault::new(home.pipelock_dir());
    vault.set(&telegram_pair_code_key(agent_id), &code)?;
    println!("telegram pairing code: {code}");
    if let Some(bot_name) = bot_name {
        println!("send this to @{bot_name}: /pair {code}");
    } else {
        println!("send this to the bot: /pair {code}");
    }
    Ok(())
}

fn complete_telegram_pair(
    home: &MaturanaHome,
    agent_id: &str,
    token_source: &str,
    timeout_seconds: u64,
) -> anyhow::Result<()> {
    let token = resolve_secret_source_with_home(token_source, home.root())?;
    let vault = PipelockVault::new(home.pipelock_dir());
    let pair_code_key = telegram_pair_code_key(agent_id);
    let chat_id_key = telegram_chat_id_key(agent_id);
    let code = vault.get(&pair_code_key).with_context(|| {
        "no active Telegram pair code; run `maturana channel pair telegram start` first"
    })?;
    let mut state = read_telegram_state(home, agent_id)?;
    let attempts = timeout_seconds.max(1);
    for _ in 0..attempts {
        let updates = telegram_updates(token.expose_for_runtime(), state.offset, 0)?;
        for update in &updates {
            state.offset = Some(update.update_id + 1);
            if let InboundAction::Pair { chat_id } =
                classify_telegram_update(update, None, Some(&code))
            {
                vault.set(&chat_id_key, &chat_id.to_string())?;
                let _ = vault.delete(&pair_code_key)?;
                write_telegram_state(home, agent_id, &state)?;
                println!("telegram paired chat id stored in pipelock:{chat_id_key}");
                return Ok(());
            }
        }
        write_telegram_state(home, agent_id, &state)?;
        thread::sleep(Duration::from_secs(1));
    }
    anyhow::bail!("timed out waiting for `/pair {code}`")
}

fn telegram_pair_status(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let vault = PipelockVault::new(home.pipelock_dir());
    let paired = vault.get(&telegram_chat_id_key(agent_id)).is_ok()
        || (agent_id == "default" && vault.get(TELEGRAM_CHAT_ID).is_ok());
    let pending = vault.get(&telegram_pair_code_key(agent_id)).is_ok()
        || (agent_id == "default" && vault.get(TELEGRAM_PAIR_CODE).is_ok());
    println!("telegram.paired: {paired}");
    println!("telegram.pending_pair_code: {pending}");
    Ok(())
}

fn channel_status(home: &MaturanaHome) -> anyhow::Result<()> {
    telegram_pair_status(home, "default")?;
    let state = read_telegram_state(home, "default").unwrap_or_default();
    println!(
        "telegram.offset: {}",
        state.offset.map(|v| v.to_string()).unwrap_or_default()
    );
    println!(
        "telegram.last_seen_at: {}",
        state
            .last_seen_at
            .map(|at| at.to_rfc3339())
            .unwrap_or_default()
    );
    Ok(())
}

fn serve_telegram(home: &MaturanaHome, config: TelegramServe) -> anyhow::Result<()> {
    let token = resolve_secret_source_with_home(&config.token_source, home.root())?;
    let token = token.expose_for_runtime().to_string();
    let mut state = read_telegram_state(home, &config.agent_id)?;
    ensure_session(&session_paths(
        &home.agent_dir(&config.agent_id),
        &config.session_id,
    ))?;
    println!("telegram channel serving agent {}", config.agent_id);
    if state.offset.is_none() {
        let updates = telegram_updates(&token, None, 0)?;
        if let Some(max_update_id) = updates.iter().map(|update| update.update_id).max() {
            state.offset = Some(max_update_id + 1);
            state.last_seen_at = Some(Utc::now());
            write_telegram_state(home, &config.agent_id, &state)?;
            println!("telegram channel offset initialized");
        }
        if config.once {
            return Ok(());
        }
    }
    // Deliver agent replies on a dedicated fast cadence so an outbound message
    // never waits for the inbound long-poll. Without this, a reply sits in the
    // outbox until the next getUpdates returns - the main source of "sluggish".
    if !config.once {
        let deliver_root = home.root().to_path_buf();
        let deliver_token = token.clone();
        let deliver_agent = config.agent_id.clone();
        let deliver_session = config.session_id.clone();
        thread::spawn(move || {
            let home = MaturanaHome::new(deliver_root);
            loop {
                if let Some(chat_id) = current_paired_telegram_chat_id(&home, &deliver_agent) {
                    let _ = deliver_telegram_outbox(
                        &home,
                        &deliver_token,
                        &deliver_agent,
                        &deliver_session,
                        chat_id,
                    );
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }
    // Long-poll inbound: getUpdates blocks server-side until a message arrives,
    // so inbound is near-instant instead of waiting for a client sleep.
    let long_poll_secs = if config.once {
        0
    } else {
        config.timeout_seconds.clamp(1, 50)
    };
    loop {
        write_telegram_heartbeat(home, &config.agent_id, "polling", None)?;
        let updates = match telegram_updates(&token, state.offset, long_poll_secs) {
            Ok(updates) => updates,
            Err(error) => {
                let message = error.to_string();
                write_telegram_heartbeat(home, &config.agent_id, "poll_error", Some(&message))?;
                audit_channel_event(
                    home,
                    &config.agent_id,
                    "channel.telegram.poll_error",
                    &message,
                )?;
                if config.once {
                    return Err(error);
                }
                thread::sleep(Duration::from_secs(config.poll_seconds.max(1)));
                continue;
            }
        };
        for update in &updates {
            state.offset = Some(update.update_id + 1);
            state.last_seen_at = Some(Utc::now());
            let vault = PipelockVault::new(home.pipelock_dir());
            let paired_chat_id = vault
                .get(&telegram_chat_id_key(&config.agent_id))
                .or_else(|_| vault.get(TELEGRAM_CHAT_ID))
                .ok()
                .and_then(|value| value.parse::<i64>().ok());
            let pair_code = vault
                .get(&telegram_pair_code_key(&config.agent_id))
                .or_else(|_| vault.get(TELEGRAM_PAIR_CODE))
                .ok();
            if let Err(error) = handle_telegram_update(
                home,
                &token,
                &config,
                paired_chat_id,
                pair_code.as_deref(),
                update,
            ) {
                let message = error.to_string();
                write_telegram_heartbeat(home, &config.agent_id, "update_error", Some(&message))?;
                audit_channel_event(
                    home,
                    &config.agent_id,
                    "channel.telegram.update_error",
                    &message,
                )?;
                if config.once {
                    return Err(error);
                }
                continue;
            }
        }
        write_telegram_state(home, &config.agent_id, &state)?;
        // In serve mode the delivery thread handles the outbox; in `once` mode
        // (tests / one-shot) deliver inline since no thread was spawned.
        if config.once {
            if let Some(chat_id) = current_paired_telegram_chat_id(home, &config.agent_id) {
                deliver_telegram_outbox(
                    home,
                    &token,
                    &config.agent_id,
                    &config.session_id,
                    chat_id,
                )?;
            }
            break;
        }
        write_telegram_heartbeat(home, &config.agent_id, "idle", None)?;
        // No client-side sleep: the long-poll getUpdates above paces the loop.
    }
    Ok(())
}

fn current_paired_telegram_chat_id(home: &MaturanaHome, agent_id: &str) -> Option<i64> {
    let vault = PipelockVault::new(home.pipelock_dir());
    vault
        .get(&telegram_chat_id_key(agent_id))
        .or_else(|_| vault.get(TELEGRAM_CHAT_ID))
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
}

fn handle_telegram_update(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
    update: &TelegramUpdate,
) -> anyhow::Result<()> {
    let reply_to_message_id = telegram_message(update).map(|message| message.message_id);
    match classify_telegram_update(update, paired_chat_id, pair_code) {
        InboundAction::Ignore => Ok(()),
        InboundAction::Deny { chat_id } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.denied",
                "ignored unpaired chat",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                "This Maturana agent is not paired with this chat.",
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Pair { chat_id } => {
            let vault = PipelockVault::new(home.pipelock_dir());
            vault.set(
                &telegram_chat_id_key(&config.agent_id),
                &chat_id.to_string(),
            )?;
            let _ = vault.delete(&telegram_pair_code_key(&config.agent_id))?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.paired",
                "paired telegram chat",
            )?;
            // First contact: greet + run a brief onboarding interview so the
            // agent learns who its owner is. The agent's greeting arrives via the
            // outbox delivery thread once the guest worker runs the turn.
            if !is_onboarded(home, &config.agent_id) {
                enqueue_onboarding(home, &config.agent_id, &config.session_id)?;
                let _ = mark_onboarded(home, &config.agent_id);
                send_telegram(
                    token,
                    &chat_id.to_string(),
                    "Paired! One moment — let me introduce myself.",
                    reply_to_message_id,
                )?;
                send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
            } else {
                send_telegram(
                    token,
                    &chat_id.to_string(),
                    "Paired. Welcome back — message me any time.",
                    reply_to_message_id,
                )?;
            }
            Ok(())
        }
        InboundAction::Help { chat_id } => {
            send_telegram(token, &chat_id.to_string(), &help_text(), reply_to_message_id)?;
            Ok(())
        }
        InboundAction::Status { chat_id } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.status",
                "status requested",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &status_text(home, config),
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Command {
            chat_id,
            name,
            args,
        } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.command",
                &format!("/{name}"),
            )?;
            let reply = handle_channel_command(home, config, chat_id, &name, &args)
                .unwrap_or_else(|error| format!("Command failed: {error:#}"));
            send_telegram(token, &chat_id.to_string(), &reply, reply_to_message_id)?;
            Ok(())
        }
        InboundAction::New { chat_id } => {
            reset_channel_context(home, &config.agent_id, chat_id)?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.new_session",
                "reset telegram context window",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                "New session started.",
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Spawn {
            chat_id,
            mode,
            name,
            prompt,
        } => {
            let subagent_id = create_subagent(home, &config.agent_id, &name, mode, &prompt)?;
            let paths = session_paths(
                &home.agent_dir(&config.agent_id),
                &format!("subagent-{subagent_id}"),
            );
            ensure_session(&paths)?;
            insert_inbound(
                &paths,
                "spawn",
                "subagent",
                &subagent_id,
                None,
                &serde_json::json!({
                    "text": prompt,
                    "prompt": prompt,
                    "subagent_id": subagent_id,
                    "parent_agent_id": config.agent_id,
                })
                .to_string(),
            )?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.spawn",
                "spawned sub-agent session",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("Spawned sub-agent `{subagent_id}`."),
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Feedback { chat_id, value } => {
            let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
            let source = "telegram";
            let rewarded =
                store.reward_latest(&config.agent_id, &config.session_id, source, value, None)?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.feedback",
                &format!("recorded reward {value:+} on {:?}", rewarded),
            )?;
            let reply = match rewarded {
                Some(_) if value > 0.0 => "Thanks — logged 👍 for the last reply.",
                Some(_) => "Noted — logged 👎 for the last reply.",
                None => "No recent agent turn to rate yet.",
            };
            send_telegram(token, &chat_id.to_string(), reply, reply_to_message_id)?;
            Ok(())
        }
        InboundAction::Tool {
            chat_id,
            name,
            input,
        } => {
            run_tool_with_animation(home, token, config, chat_id, &name, &input)?;
            Ok(())
        }
        InboundAction::Document {
            chat_id,
            document,
            caption,
        } => handle_telegram_document(
            home,
            token,
            config,
            chat_id,
            &document,
            caption.as_deref(),
            reply_to_message_id,
        ),
        InboundAction::Prompt { chat_id, text } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.inbound",
                "received telegram prompt",
            )?;
            println!("telegram inbound prompt accepted");
            append_channel_turn(home, &config.agent_id, chat_id, "user", &text)?;
            maybe_remember_user_message(home, &config.agent_id, &text)?;
            let prompt = build_channel_prompt(home, &config.agent_id, chat_id, &text)?;
            let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
            ensure_session(&paths)?;
            insert_inbound(
                &paths,
                "chat",
                "telegram",
                &chat_id.to_string(),
                reply_to_message_id.map(|id| id.to_string()).as_deref(),
                &serde_json::json!({
                    "text": text,
                    "prompt": prompt,
                    "telegram_reply_to": reply_to_message_id,
                })
                .to_string(),
            )?;
            send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
            if let Some(provider) = &config.run_once_provider {
                let options = RunnerOptions {
                    provider: provider.to_string(),
                };
                run_session_once(&paths, &options, 20)?;
            }
            deliver_telegram_outbox(home, token, &config.agent_id, &config.session_id, chat_id)?;
            Ok(())
        }
    }
}

fn deliver_telegram_outbox(
    home: &MaturanaHome,
    token: &str,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
) -> anyhow::Result<usize> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut delivered = 0;
    for message in list_undelivered(&paths)? {
        if message.channel != "telegram" || message.platform_id != chat_id.to_string() {
            continue;
        }
        let response = truncate_for_telegram(&message_text(&message.content)?);
        // A proactive self-check that decided there's nothing worth saying emits
        // the silence sentinel: mark it delivered (so it isn't retried) but don't
        // message the user.
        if response.trim() == crate::proactive::SILENCE_SENTINEL {
            mark_delivered(&paths, &message.id, None)?;
            continue;
        }
        let reply_to_message_id = message
            .thread_id
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok());
        let platform_message_id =
            send_telegram(token, &chat_id.to_string(), &response, reply_to_message_id)?;
        append_channel_turn(home, agent_id, chat_id, "assistant", &response)?;
        mark_delivered(
            &paths,
            &message.id,
            platform_message_id.map(|id| id.to_string()).as_deref(),
        )?;
        audit_channel_event(
            home,
            agent_id,
            "channel.telegram.outbound",
            "sent telegram response",
        )?;
        delivered += 1;
    }
    if delivered > 0 {
        println!("telegram outbound responses sent: {delivered}");
    }
    Ok(delivered)
}

/// Telegram bot API refuses `getFile` beyond 20 MB; stay under it.
const MAX_TELEGRAM_DOCUMENT_BYTES: u64 = 19 * 1024 * 1024;

/// A document uploaded to the paired chat: download it and ingest it into the
/// agent's knowledge graph (via the running MaturanaGraph service, so this
/// process never opens the store directly). The reply tells the user what
/// landed where; follow-up questions hit the graph through the channel prompt's
/// GraphRAG context.
fn handle_telegram_document(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    document: &TelegramDocument,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    let file_name = sanitize_document_name(document.file_name.as_deref());
    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph_token = maturana_core::worker::read_graph_token(home.root());
    let (graph_token, graph_name) = match (graph_token, knowledge_graph.enabled) {
        (Some(token), true) => (token, crate::graph::agent_graph_name(&config.agent_id)),
        _ => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I received the document, but my knowledge graph is not enabled, so I cannot store it. Enable `knowledge_graph` in MATURANA.md and set up the graph service.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    if document.file_size.unwrap_or(0) > MAX_TELEGRAM_DOCUMENT_BYTES as i64 {
        send_telegram(
            token,
            &chat_id.to_string(),
            "That document is larger than 19 MB, which is more than I can pull from Telegram. Please send a smaller file.",
            reply_to_message_id,
        )?;
        return Ok(());
    }
    let supported = file_name
        .rsplit_once('.')
        .map(|(_, ext)| crate::graph::SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !supported {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!(
                "I can ingest these document types: {}. `{file_name}` is not one of them.",
                crate::graph::SUPPORTED_EXTS.join(", ")
            ),
            reply_to_message_id,
        )?;
        return Ok(());
    }

    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    fs::create_dir_all(&inbox)?;
    let dest = inbox.join(format!(
        "{}-{file_name}",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let result = download_telegram_document(token, &document.file_id, &dest)
        .and_then(|_| {
            crate::graph::ingest_file_into_service(
                crate::graph::DEFAULT_LOCAL_URL,
                &graph_token,
                &graph_name,
                &dest,
                1800,
            )
        });
    match result {
        Ok(chunks) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.document",
                &format!("ingested {file_name} into graph '{graph_name}' ({chunks} chunks)"),
            )?;
            // Record the upload in the transcript so follow-up questions carry
            // the document's terms into wiki/graph retrieval.
            let transcript_note = match caption {
                Some(caption) if !caption.trim().is_empty() => {
                    format!("[uploaded document: {file_name}] {}", caption.trim())
                }
                _ => format!("[uploaded document: {file_name}]"),
            };
            append_channel_turn(home, &config.agent_id, chat_id, "user", &transcript_note)?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "Added `{file_name}` to my knowledge graph `{graph_name}` ({chunks} chunks). Ask me about it any time."
                ),
                reply_to_message_id,
            )?;
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.document_error",
                &format!("failed to ingest {file_name}: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I could not ingest `{file_name}`: {error:#}"),
                reply_to_message_id,
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct TelegramGetFileResponse {
    ok: bool,
    result: Option<TelegramFilePath>,
}

#[derive(Debug, Deserialize)]
struct TelegramFilePath {
    file_path: Option<String>,
}

fn download_telegram_document(
    token: &str,
    file_id: &str,
    dest: &Path,
) -> anyhow::Result<u64> {
    let response: TelegramGetFileResponse = ureq::get(&format!(
        "https://api.telegram.org/bot{token}/getFile?file_id={file_id}"
    ))
    .call()
    .context("Telegram getFile failed")?
    .into_json()
    .context("failed to parse Telegram getFile response")?;
    if !response.ok {
        anyhow::bail!("Telegram getFile returned ok=false");
    }
    let file_path = response
        .result
        .and_then(|result| result.file_path)
        .context("Telegram getFile returned no file_path")?;
    let reader = ureq::get(&format!(
        "https://api.telegram.org/file/bot{token}/{file_path}"
    ))
    .call()
    .context("Telegram file download failed")?
    .into_reader();
    let mut bytes = Vec::new();
    std::io::Read::take(reader, MAX_TELEGRAM_DOCUMENT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TELEGRAM_DOCUMENT_BYTES {
        anyhow::bail!("document exceeds {MAX_TELEGRAM_DOCUMENT_BYTES} bytes");
    }
    fs::write(dest, &bytes)
        .with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(bytes.len() as u64)
}

/// Keep only filesystem-safe filename characters; Telegram file names are
/// attacker-controlled input that ends up in a path under the agent inbox.
fn sanitize_document_name(name: Option<&str>) -> String {
    let cleaned = name
        .unwrap_or("document")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(|ch: char| ch == '.' || ch.is_whitespace())
        .to_string();
    if cleaned.is_empty() {
        "document".to_string()
    } else {
        cleaned
    }
}

/// The agent's `knowledge_graph` opt-in, read from its materialized spec.
/// Missing/unparseable spec means disabled (the default).
fn agent_knowledge_graph(home: &MaturanaHome, agent_id: &str) -> maturana_core::spec::KnowledgeGraph {
    maturana_core::spec::AgentSpec::from_maturana_markdown(
        &home.agent_dir(agent_id).join("MATURANA.md"),
    )
    .ok()
    .map(|spec| spec.knowledge_graph)
    .unwrap_or_default()
}

fn classify_telegram_update(
    update: &TelegramUpdate,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
) -> InboundAction {
    let Some(message) = telegram_message(update) else {
        return InboundAction::Ignore;
    };
    let chat_id = message.chat.id;
    // Document uploads have no `text`; route them before the text path. The
    // pairing gate still applies — only the paired chat can feed the agent.
    if let Some(document) = &message.document {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Document {
            chat_id,
            document: document.clone(),
            caption: message.caption.clone(),
        };
    }
    let text = message.text.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return InboundAction::Ignore;
    }
    if let Some(code) = pair_code {
        if is_pair_command(text, code) {
            return InboundAction::Pair { chat_id };
        }
    }
    if paired_chat_id != Some(chat_id) {
        return InboundAction::Deny { chat_id };
    }
    let command = normalize_bot_command(text);
    let (cmd, args) = match command.split_once(char::is_whitespace) {
        Some((c, a)) => (c.to_ascii_lowercase(), a.trim().to_string()),
        None => (command.to_ascii_lowercase(), String::new()),
    };
    match cmd.as_str() {
        "/start" | "/help" => InboundAction::Help { chat_id },
        "/status" => InboundAction::Status { chat_id },
        "/new" => InboundAction::New { chat_id },
        "/good" => InboundAction::Feedback {
            chat_id,
            value: signals::THUMBS_UP,
        },
        "/bad" => InboundAction::Feedback {
            chat_id,
            value: signals::THUMBS_DOWN,
        },
        "/tool" => match parse_tool_command(&command) {
            Some((name, input)) => InboundAction::Tool {
                chat_id,
                name,
                input,
            },
            None => InboundAction::Help { chat_id },
        },
        "/spawn" => match parse_spawn_command(&command) {
            Some((mode, name, prompt)) => InboundAction::Spawn {
                chat_id,
                mode,
                name,
                prompt,
            },
            None => InboundAction::Help { chat_id },
        },
        // /emerge <task> spawns an ephemeral sub-agent on the task.
        "/emerge" if !args.is_empty() => InboundAction::Spawn {
            chat_id,
            mode: SpawnMode::Ephemeral,
            name: slugify_channel_id(&args),
            prompt: args,
        },
        // /skill <name> [args] runs a skill by telling the agent to use it
        // (reuses the full prompt pipeline). Bare /skill lists skills.
        "/skill" if !args.is_empty() => {
            let (skill, rest) = match args.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args.as_str(), ""),
            };
            InboundAction::Prompt {
                chat_id,
                text: format!("Use the `{skill}` skill. {rest}").trim().to_string(),
            }
        }
        "/commands" | "/tools" | "/models" | "/model" | "/reset" | "/stop" | "/compact"
        | "/session" | "/subagents" | "/graph-query" | "/graph-insert" | "/tts"
        | "/tts-provider" | "/emerge" | "/skill" | "/onboard" => InboundAction::Command {
            chat_id,
            name: cmd.trim_start_matches('/').to_string(),
            args,
        },
        _ if cmd.starts_with('/') => InboundAction::Command {
            chat_id,
            name: "unknown".to_string(),
            args: cmd,
        },
        _ => InboundAction::Prompt {
            chat_id,
            text: text.to_string(),
        },
    }
}

/// `/tool <name> [json-input]` — name plus an optional JSON argument. The
/// input defaults to `{}` when omitted.
fn parse_tool_command(command: &str) -> Option<(String, String)> {
    let rest = command.strip_prefix("/tool")?.trim();
    let (name, input) = match rest.split_once(char::is_whitespace) {
        Some((name, input)) => (name.trim(), input.trim()),
        None => (rest, ""),
    };
    if !maturana_core::tools::is_valid_tool_name(name) {
        return None;
    }
    let input = if input.is_empty() { "{}" } else { input };
    Some((name.to_string(), input.to_string()))
}

fn parse_spawn_command(command: &str) -> Option<(SpawnMode, String, String)> {
    let rest = command.strip_prefix("/spawn")?.trim();
    let (head, prompt) = rest.split_once("--")?;
    let mut parts = head.split_whitespace();
    let first = parts.next()?;
    let (mode, name) = match first {
        "ephemeral" => (SpawnMode::Ephemeral, parts.next()?),
        "persistent" => (SpawnMode::Persistent, parts.next()?),
        name => (SpawnMode::Ephemeral, name),
    };
    let prompt = prompt.trim();
    if name.trim().is_empty() || prompt.is_empty() {
        return None;
    }
    Some((mode, slugify_channel_id(name), prompt.to_string()))
}

fn create_subagent(
    home: &MaturanaHome,
    parent_agent_id: &str,
    name: &str,
    mode: SpawnMode,
    prompt: &str,
) -> anyhow::Result<String> {
    let subagent_id = slugify_channel_id(name);
    let path = home
        .agent_dir(parent_agent_id)
        .join("subagents")
        .join(format!("{subagent_id}.json"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "id": subagent_id,
            "parent_agent_id": parent_agent_id,
            "mode": match mode {
                SpawnMode::Ephemeral => "ephemeral",
                SpawnMode::Persistent => "persistent",
            },
            "prompt": prompt,
            "created_at": Utc::now(),
        }))?,
    )?;
    Ok(subagent_id)
}

fn slugify_channel_id(value: &str) -> String {
    let slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "subagent".to_string()
    } else {
        slug
    }
}

// ---------------------------------------------------------------------------
// In-channel slash commands: the control surface a user drives the agent with.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChannelSettings {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tts_enabled: bool,
    #[serde(default)]
    tts_provider: Option<String>,
    #[serde(default)]
    idle: bool,
}

fn truncate_inline(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}

fn channel_settings_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("channel-settings.json")
}

fn load_channel_settings(home: &MaturanaHome, agent_id: &str) -> ChannelSettings {
    fs::read_to_string(channel_settings_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_channel_settings(
    home: &MaturanaHome,
    agent_id: &str,
    settings: &ChannelSettings,
) -> anyhow::Result<()> {
    let path = channel_settings_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

/// Grouped command catalog — single source of truth for /help and /commands.
const COMMAND_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Session",
        &[
            ("/new", "start a new session"),
            ("/reset", "reset the current session"),
            ("/stop", "stop the current run"),
            ("/compact", "compact the session context"),
            ("/session", "session settings (e.g. /session idle)"),
            ("/onboard", "(re)run the first-run interview"),
        ],
    ),
    (
        "Options",
        &[
            ("/model", "show or set the model (/model <id>)"),
            ("/models", "list available models"),
        ],
    ),
    (
        "Status",
        &[
            ("/help", "show available commands"),
            ("/commands", "list all slash commands"),
            ("/tools", "list available runtime tools"),
            ("/status", "model, channel, harness, time, OS"),
        ],
    ),
    (
        "Management",
        &[
            ("/subagents", "inspect subagent runs for this session"),
            ("/skill", "run a skill by name (/skill <name> [args])"),
            ("/emerge", "run a sub-agent on a task (/emerge <task>)"),
        ],
    ),
    (
        "MaturanaGraph",
        &[
            ("/graph-query", "GraphRAG query (/graph-query <terms>)"),
            ("/graph-insert", "add content to MaturanaGraph"),
        ],
    ),
    (
        "Voice",
        &[
            ("/tts", "enable/disable text-to-speech"),
            ("/tts-provider", "set TTS provider (e.g. elevenlabs)"),
        ],
    ),
];

fn help_text() -> String {
    let mut out = String::from("Maturana commands:\n");
    for (group, cmds) in COMMAND_GROUPS {
        out.push_str(&format!("\n{group}\n"));
        for (name, desc) in *cmds {
            out.push_str(&format!("  {name} — {desc}\n"));
        }
    }
    out.push_str("\nAny other message is sent to the agent.");
    out
}

fn commands_text() -> String {
    let mut names: Vec<&str> = Vec::new();
    for (_, cmds) in COMMAND_GROUPS {
        for (name, _) in *cmds {
            names.push(name);
        }
    }
    format!("{}\n/good /bad — rate the last reply", names.join("  "))
}

fn harness_label(home: &MaturanaHome, agent_id: &str) -> String {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    match AgentSpec::from_maturana_markdown(&spec_path) {
        Ok(spec) => match spec.runtime.harness {
            HarnessRuntime::Codex => "codex",
            HarnessRuntime::ClaudeCode => "claude-code",
            HarnessRuntime::Opencode => "opencode",
        }
        .to_string(),
        Err(_) => "unknown".to_string(),
    }
}

fn status_text(home: &MaturanaHome, config: &TelegramServe) -> String {
    let settings = load_channel_settings(home, &config.agent_id);
    let harness = harness_label(home, &config.agent_id);
    let model = settings.model.unwrap_or_else(|| "(harness default)".to_string());
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "Status\n  agent: {}\n  channel: telegram (session {})\n  presence: {}\n  harness: {}\n  model: {}\n  OS: {}\n  time: {}\n  idle: {}",
        config.agent_id,
        config.session_id,
        channel_presence(home, &config.agent_id),
        harness,
        model,
        std::env::consts::OS,
        now,
        if settings.idle { "on" } else { "off" },
    )
}

fn tools_text(home: &MaturanaHome) -> String {
    match ToolRegistry::new(home.root().join("tools")).list() {
        Ok(tools) if !tools.is_empty() => {
            let mut out = String::from("Runtime tools:\n");
            for t in tools {
                let desc = t.description.lines().next().unwrap_or("").trim();
                out.push_str(&format!("  {} — {}\n", t.name, truncate_inline(desc, 80)));
            }
            out
        }
        Ok(_) => "No runtime tools installed yet. Build one with the maturana-tool-create or maturana-wasm-tool skill.".to_string(),
        Err(error) => format!("Could not list tools: {error:#}"),
    }
}

fn subagents_text(home: &MaturanaHome, agent_id: &str) -> String {
    let dir = home.agent_dir(agent_id).join("subagents");
    let mut entries: Vec<String> = Vec::new();
    if let Ok(read) = fs::read_dir(&dir) {
        for e in read.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                    let mode = fs::read_to_string(e.path())
                        .ok()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(String::from))
                        .unwrap_or_else(|| "ephemeral".to_string());
                    entries.push(format!("  {stem} ({mode})"));
                }
            }
        }
    }
    if entries.is_empty() {
        "No subagents yet. Spawn one with /emerge <task>.".to_string()
    } else {
        entries.sort();
        format!("Subagents:\n{}", entries.join("\n"))
    }
}

/// Live OpenRouter catalog for OpenCode/OpenRouter; a short curated set otherwise.
fn models_text(home: &MaturanaHome, agent_id: &str) -> String {
    let settings = load_channel_settings(home, agent_id);
    let current = settings.model.clone().unwrap_or_else(|| "(harness default)".to_string());
    let harness = harness_label(home, agent_id);
    let body = if harness == "opencode" {
        match fetch_openrouter_models() {
            Ok(ids) if !ids.is_empty() => {
                let shown: Vec<String> = ids.into_iter().take(60).collect();
                format!("OpenRouter models (first {}):\n{}", shown.len(), shown.join("\n"))
            }
            Ok(_) => "OpenRouter returned no models.".to_string(),
            Err(error) => format!("Could not fetch OpenRouter catalog: {error:#}"),
        }
    } else if harness == "codex" {
        "Codex models: gpt-5-codex, gpt-5, o4-mini\n(your Codex subscription decides availability)".to_string()
    } else {
        "claude-code models: claude-sonnet-4.5, claude-opus-4.1, claude-haiku-4.5".to_string()
    };
    format!("Current: {current}\nSet with /model <id>\n\n{body}")
}

fn fetch_openrouter_models() -> anyhow::Result<Vec<String>> {
    let resp: serde_json::Value = ureq::get("https://openrouter.ai/api/v1/models")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .context("OpenRouter request failed")?
        .into_json()
        .context("failed to parse OpenRouter response")?;
    let ids = resp
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(ids)
}

fn graph_query_text(home: &MaturanaHome, agent_id: &str, terms: &str) -> String {
    if terms.trim().is_empty() {
        return "Usage: /graph-query <terms>".to_string();
    }
    let kg = agent_knowledge_graph(home, agent_id);
    if !kg.enabled {
        return "Knowledge graph is not enabled for this agent.".to_string();
    }
    let Some(token) = maturana_core::worker::read_graph_token(home.root()) else {
        return "Knowledge graph service is not available (no graph token).".to_string();
    };
    let agent_graph = crate::graph::agent_graph_name(agent_id);
    let graphs = vec![agent_graph.clone(), kg.graph_name(agent_id)];
    let term_list: Vec<String> = terms.split_whitespace().map(String::from).collect();
    let rendered =
        crate::graph::query_blended_context(crate::graph::DEFAULT_LOCAL_URL, &token, &graphs, &term_list, 2);
    format!("GraphRAG (private + shared):\n{}", truncate_inline(&rendered, 3500))
}

// --- First-run interview + presence -----------------------------------------

fn onboarded_marker(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("onboarded")
}

fn is_onboarded(home: &MaturanaHome, agent_id: &str) -> bool {
    onboarded_marker(home, agent_id).exists()
}

fn mark_onboarded(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let path = onboarded_marker(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, Utc::now().to_rfc3339())?;
    Ok(())
}

/// First contact: the agent greets the user and runs a short onboarding
/// interview so it learns who they are (name, timezone, what they want help
/// with) and records it to memory + IDENTITY.md.
fn onboarding_prompt() -> String {
    "[FIRST CONTACT - your owner just paired with you; they have NOT spoken yet.]\n\n\
     Greet them warmly and briefly in your own voice (per SOUL.md), then begin a short \
     onboarding interview. Ask their name and how they'd like to be addressed, their \
     timezone / working hours, and the main things they want your help with. Ask only \
     1-2 questions at a time - keep it natural, not a form. As you learn durable facts, \
     save them to your memory and fill in IDENTITY.md's \"Who you are to me\" section. \
     Send your greeting and first question now."
        .to_string()
}

fn enqueue_onboarding(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let prompt = onboarding_prompt();
    insert_inbound(
        &paths,
        "onboard",
        "onboard",
        &format!("onboard-{}", Utc::now().timestamp_millis()),
        None,
        &serde_json::json!({ "text": prompt, "prompt": prompt }).to_string(),
    )?;
    Ok(())
}

/// A short presence line for /status: the channel's last heartbeat.
fn channel_presence(home: &MaturanaHome, agent_id: &str) -> String {
    match fs::read_to_string(telegram_heartbeat_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    {
        Some(hb) => {
            let status = hb.get("status").and_then(|s| s.as_str()).unwrap_or("unknown");
            let at = hb.get("at").and_then(|a| a.as_str()).unwrap_or("?");
            format!("{status} (last beat {at})")
        }
        None => "not started".to_string(),
    }
}

/// Handle a slash command that returns a text reply (and may persist settings or
/// reset context). Side-effecting spawns/prompts are routed earlier in classify.
fn handle_channel_command(
    home: &MaturanaHome,
    config: &TelegramServe,
    chat_id: i64,
    name: &str,
    args: &str,
) -> anyhow::Result<String> {
    let reply = match name {
        "commands" => commands_text(),
        "tools" => tools_text(home),
        "subagents" => subagents_text(home, &config.agent_id),
        "models" => models_text(home, &config.agent_id),
        "model" => {
            let mut settings = load_channel_settings(home, &config.agent_id);
            if args.trim().is_empty() {
                format!(
                    "Model: {}",
                    settings.model.clone().unwrap_or_else(|| "(harness default)".to_string())
                )
            } else {
                settings.model = Some(args.trim().to_string());
                save_channel_settings(home, &config.agent_id, &settings)?;
                format!("Model set to `{}` (applies to new turns).", args.trim())
            }
        }
        "reset" => {
            reset_channel_context(home, &config.agent_id, chat_id)?;
            "Session reset — durable memory and wiki are preserved.".to_string()
        }
        "stop" => "Nothing to stop — channel turns run one at a time and finish on reply.".to_string(),
        "compact" => "The conversation context is compacted automatically each turn (durable memory + wiki + recent transcript). Use /new to start fresh.".to_string(),
        "session" => {
            let mut settings = load_channel_settings(home, &config.agent_id);
            let sub = args.split_whitespace().next().unwrap_or("");
            match sub {
                "idle" => {
                    settings.idle = true;
                    save_channel_settings(home, &config.agent_id, &settings)?;
                    "Session set to idle.".to_string()
                }
                "active" | "wake" => {
                    settings.idle = false;
                    save_channel_settings(home, &config.agent_id, &settings)?;
                    "Session active.".to_string()
                }
                _ => format!(
                    "Session {}\n  idle: {}\n  model: {}\nSet with: /session idle | /session active",
                    config.session_id,
                    if settings.idle { "on" } else { "off" },
                    settings.model.clone().unwrap_or_else(|| "(default)".to_string()),
                ),
            }
        }
        "tts" => {
            let mut settings = load_channel_settings(home, &config.agent_id);
            settings.tts_enabled = !settings.tts_enabled;
            save_channel_settings(home, &config.agent_id, &settings)?;
            let prov = settings.tts_provider.clone().unwrap_or_else(|| "none set".to_string());
            format!(
                "Text-to-speech {} (provider: {}). Set a provider with /tts-provider <name>.",
                if settings.tts_enabled { "ENABLED" } else { "disabled" },
                prov
            )
        }
        "tts-provider" => {
            if args.trim().is_empty() {
                let s = load_channel_settings(home, &config.agent_id);
                format!("TTS provider: {}", s.tts_provider.unwrap_or_else(|| "(none)".to_string()))
            } else {
                let mut settings = load_channel_settings(home, &config.agent_id);
                settings.tts_provider = Some(args.trim().to_string());
                save_channel_settings(home, &config.agent_id, &settings)?;
                format!("TTS provider set to `{}`.", args.trim())
            }
        }
        "graph-query" => graph_query_text(home, &config.agent_id, args),
        "graph-insert" => {
            if args.trim().is_empty() {
                "Usage: /graph-insert <text> — adds a note to your private memory graph. (Or attach a document to ingest it.)".to_string()
            } else {
                match maturana_core::worker::read_graph_token(home.root()) {
                    Some(token) => {
                        let agent_graph = crate::graph::agent_graph_name(&config.agent_id);
                        let dir = home.agent_dir(&config.agent_id).join("inbox");
                        let _ = fs::create_dir_all(&dir);
                        let path = dir.join(format!("note-{}.md", Utc::now().timestamp_millis()));
                        match fs::write(&path, args) {
                            Ok(()) => match crate::graph::ingest_file_into_service(
                                crate::graph::DEFAULT_LOCAL_URL,
                                &token,
                                &agent_graph,
                                &path,
                                1200,
                            ) {
                                Ok(chunks) => format!("Added to your memory graph `{agent_graph}` ({chunks} chunk(s))."),
                                Err(error) => format!("Graph insert failed: {error:#}"),
                            },
                            Err(error) => format!("Could not stage note: {error:#}"),
                        }
                    }
                    None => "Knowledge graph service is not available.".to_string(),
                }
            }
        }
        "emerge" => "Usage: /emerge <task> — runs a sub-agent on the task.".to_string(),
        "onboard" => {
            enqueue_onboarding(home, &config.agent_id, &config.session_id)?;
            let _ = mark_onboarded(home, &config.agent_id);
            "Starting onboarding — I'll introduce myself and ask a few questions.".to_string()
        }
        "skill" => {
            // No args: list skills so the user can pick one. With args, classify
            // routes it to a prompt instead, so this only handles the bare form.
            let skills_dir = std::path::Path::new("skills");
            let mut names: Vec<String> = Vec::new();
            if let Ok(read) = fs::read_dir(skills_dir) {
                for e in read.flatten() {
                    if e.path().join("SKILL.md").exists() {
                        if let Some(n) = e.path().file_name().and_then(|s| s.to_str()) {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            names.sort();
            if names.is_empty() {
                "Usage: /skill <name> [args]".to_string()
            } else {
                format!("Usage: /skill <name> [args]\nSkills:\n{}", names.join(", "))
            }
        }
        _ => format!("Unknown command `/{name}`. Try /help."),
    };
    Ok(reply)
}

fn telegram_message(update: &TelegramUpdate) -> Option<&TelegramMessage> {
    update.message.as_ref().or(update.channel_post.as_ref())
}

fn is_pair_command(text: &str, code: &str) -> bool {
    let normalized = normalize_bot_command(text.trim());
    let Some(rest) = normalized
        .strip_prefix("/pair")
        .or_else(|| normalized.strip_prefix("pair"))
    else {
        return false;
    };
    rest.trim() == code
}

fn normalize_bot_command(text: &str) -> String {
    let Some((command, rest)) = text.split_once(' ') else {
        return text
            .split_once('@')
            .map(|(command, _)| command)
            .unwrap_or(text)
            .to_string();
    };
    if let Some((base, _)) = command.split_once('@') {
        if base.starts_with('/') {
            return format!("{base} {rest}");
        }
    }
    text.to_string()
}

fn telegram_bot_username(token: &str) -> anyhow::Result<Option<String>> {
    let response: TelegramGetMeResponse =
        ureq::get(&format!("https://api.telegram.org/bot{token}/getMe"))
            .call()
            .context("Telegram getMe failed")?
            .into_json()
            .context("failed to parse Telegram getMe response")?;
    if !response.ok {
        anyhow::bail!("Telegram getMe returned ok=false");
    }
    Ok(response.result.and_then(|user| user.username))
}

/// Fetch updates. `long_poll_secs > 0` uses Telegram long-polling: the call
/// blocks server-side until a message arrives (near-instant inbound) or the
/// timeout elapses, instead of returning immediately and forcing a client-side
/// sleep. The HTTP read timeout is set above the long-poll window.
fn telegram_updates(
    token: &str,
    offset: Option<i64>,
    long_poll_secs: u64,
) -> anyhow::Result<Vec<TelegramUpdate>> {
    let mut url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout={long_poll_secs}");
    if let Some(offset) = offset {
        url.push_str(&format!("&offset={offset}"));
    }
    let response: TelegramUpdatesResponse = ureq::get(&url)
        .timeout(Duration::from_secs(long_poll_secs + 15))
        .call()
        .context("Telegram getUpdates failed")?
        .into_json()
        .context("failed to parse Telegram getUpdates response")?;
    if !response.ok {
        anyhow::bail!("Telegram getUpdates returned ok=false");
    }
    Ok(response.result)
}

fn send_telegram(
    token: &str,
    chat_id: &str,
    message: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
    });
    if let Some(message_id) = reply_to_message_id {
        body["reply_parameters"] = serde_json::json!({
            "message_id": message_id,
        });
    }
    let response: TelegramSendResponse =
        ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendMessage failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram sendMessage response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram sendMessage returned ok=false");
    }
    Ok(response.result.map(|message| message.message_id))
}

fn send_telegram_chat_action(token: &str, chat_id: &str, action: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "action": action,
    });
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/sendChatAction"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram sendChatAction failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram sendChatAction response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram sendChatAction returned ok=false");
    }
    Ok(())
}

/// Run a registered wasm tool from Telegram with an OpenClaw-style animated
/// status message, post its output, and capture the run as a self-improvement
/// trajectory (so `/good` / `/bad` can reward it afterwards).
fn run_tool_with_animation(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    name: &str,
    input: &str,
) -> anyhow::Result<()> {
    let registry = ToolRegistry::new(home.root().join("tools"));
    if registry.load(name).is_err() {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!("Tool `{name}` is not registered. Use `maturana tool register` first."),
            None,
        )?;
        return Ok(());
    }

    let running = Phase::Running {
        tool: name.to_string(),
    };
    let status_id = send_telegram(token, &chat_id.to_string(), &frame(&running, 0), None)?;

    // Run the (bounded) tool off-thread so the main thread can keep editing the
    // animation frame in place while it executes.
    let (tx, rx) = mpsc::channel();
    {
        let registry = registry.clone();
        let name = name.to_string();
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_tool(&registry, &name, &input));
        });
    }

    let mut tick = 1usize;
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(700)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(message_id) = status_id {
                    let _ = edit_telegram_message(
                        token,
                        chat_id,
                        message_id,
                        &frame(&running, tick),
                    );
                }
                tick += 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("tool worker thread disconnected"))
            }
        }
    };

    let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
    let trajectory_input = format!("/tool {name} {input}");
    match result {
        Ok(run) => {
            let final_phase = if run.ok {
                Phase::Done {
                    detail: Some(format!("`{name}` in {}ms", run.duration_ms)),
                }
            } else {
                Phase::Failed {
                    detail: Some(truncate_chars(run.stderr.trim(), 80)),
                }
            };
            if let Some(message_id) = status_id {
                let _ = edit_telegram_message(token, chat_id, message_id, &frame(&final_phase, tick));
            }
            let body = if run.ok {
                let out = truncate_for_telegram(&run.stdout);
                if out.trim().is_empty() {
                    "(tool produced no output)".to_string()
                } else {
                    out
                }
            } else {
                format!("Tool failed: {}", truncate_for_telegram(run.stderr.trim()))
            };
            send_telegram(token, &chat_id.to_string(), &body, None)?;
            store.record(
                &config.agent_id,
                &config.session_id,
                "tool",
                &trajectory_input,
                &run.stdout,
                "[]",
            )?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.tool",
                &format!("ran tool {name} ok={}", run.ok),
            )?;
        }
        Err(error) => {
            let message = format!("{error:#}");
            if let Some(message_id) = status_id {
                let _ = edit_telegram_message(
                    token,
                    chat_id,
                    message_id,
                    &frame(
                        &Phase::Failed {
                            detail: Some(truncate_chars(&message, 80)),
                        },
                        tick,
                    ),
                );
            }
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("Tool error: {message}"),
                None,
            )?;
            store.record(
                &config.agent_id,
                &config.session_id,
                "tool",
                &trajectory_input,
                &message,
                "[]",
            )?;
        }
    }
    debug_assert!(is_terminal(&Phase::Done { detail: None }));
    Ok(())
}

fn edit_telegram_message(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/editMessageText"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram editMessageText failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram editMessageText response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram editMessageText returned ok=false");
    }
    Ok(())
}

fn build_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<String> {
    let context = load_channel_context(home, agent_id, chat_id, user_message)?;
    write_channel_context_manifest(home, agent_id, chat_id, &context)?;
    Ok(render_channel_prompt(&context, user_message))
}

fn load_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<ChannelContextBundle> {
    let agent_dir = home.agent_dir(agent_id);
    let transcript_path = channel_transcript_path(home, agent_id, chat_id);
    let transcript = tail_context_file(&transcript_path, TRANSCRIPT_CONTEXT_CHARS)?;
    let wiki_query = build_wiki_query_policy(user_message, &transcript);
    let wiki_query_terms = wiki_query
        .term_sources
        .iter()
        .map(|term| term.term.clone())
        .collect::<Vec<_>>();
    let wiki_chunks = load_relevant_wiki_chunks_for_terms(
        home,
        &wiki_query_terms,
        CONTEXT_WIKI_CHUNK_LIMIT,
        WIKI_CHUNK_CONTEXT_CHARS,
    )?;
    let graph_context = load_graph_channel_context(home, agent_id, &wiki_query_terms);
    // High-reward past turns shape this prompt (self-improvement, in-context).
    let learned_examples = maturana_core::improvement::TrajectoryStore::open(
        &maturana_core::improvement::TrajectoryStore::store_path(home.root()),
    )
    .and_then(|store| store.learned_examples_markdown(agent_id, 3, 0.5))
    .unwrap_or_default();

    Ok(ChannelContextBundle {
        identity: read_context_file(
            "AGENTS.md",
            &agent_dir.join("AGENTS.md"),
            IDENTITY_CONTEXT_CHARS,
        )?,
        soul: read_context_file("SOUL.md", &agent_dir.join("SOUL.md"), SOUL_CONTEXT_CHARS)?,
        contract: read_context_file(
            "MATURANA.md",
            &agent_dir.join("MATURANA.md"),
            CONTRACT_CONTEXT_CHARS,
        )?,
        memory: read_context_file(
            "memory/MEMORY.md",
            &agent_dir.join("memory/MEMORY.md"),
            MEMORY_CONTEXT_CHARS,
        )?,
        agent_context: read_context_file(
            "context/README.md",
            &agent_dir.join("context/README.md"),
            AGENT_CONTEXT_CHARS,
        )?,
        wiki_index: read_context_file(
            "wiki/INDEX.md",
            &home.root().join("wiki/INDEX.md"),
            WIKI_INDEX_CONTEXT_CHARS,
        )?,
        wiki_chunks,
        wiki_query_terms,
        wiki_term_sources: wiki_query.term_sources,
        graph_context,
        learned_examples,
        transcript,
        transcript_path,
    })
}

/// Retrieve GraphRAG context from the agent's knowledge graph for this turn.
/// Host-side keyword retrieval only (the host never embeds); returns `None`
/// when the graph is not enabled, and an explanatory placeholder when the
/// service is unreachable so a graph outage never breaks the turn.
fn load_graph_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    terms: &[String],
) -> Option<GraphChannelContext> {
    let knowledge_graph = agent_knowledge_graph(home, agent_id);
    if !knowledge_graph.enabled {
        return None;
    }
    let token = maturana_core::worker::read_graph_token(home.root())?;
    let shared = knowledge_graph.graph_name(agent_id);
    let agent_graph = crate::graph::agent_graph_name(agent_id);
    // Blended read: the agent's private memory section + the shared graph.
    let graphs = vec![agent_graph.clone(), shared.clone()];
    let rendered =
        crate::graph::query_blended_context(crate::graph::DEFAULT_LOCAL_URL, &token, &graphs, terms, 2);
    Some(GraphChannelContext {
        graph: format!("{agent_graph} + {shared}"),
        rendered,
    })
}

fn render_channel_prompt(context: &ChannelContextBundle, user_message: &str) -> String {
    let wiki_chunks = render_wiki_chunks(&context.wiki_chunks);
    let graph_section = match &context.graph_context {
        Some(graph) => format!(
            "\n## Knowledge Graph Context (GraphRAG, graph `{}`)\n\nEntities and relationships retrieved from your knowledge graph for this message. Treat them as ground truth about ingested documents and recorded facts.\n\n{}\n",
            graph.graph, graph.rendered
        ),
        None => String::new(),
    };
    let learned_section = if context.learned_examples.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n## Learned Examples (positively rated)\n\n{}\n",
            context.learned_examples
        )
    };
    format!(
        r#"You are a Maturana personal agent running inside an isolated VM.

Answer the current Telegram message directly and conversationally.
Use the durable memory and recent channel transcript for continuity.
Do not say you cannot remember earlier messages if the transcript contains them.
If the user asks you to remember something, acknowledge it briefly; the host has already stored the raw user memory note.
Return only the message that should be sent back to Telegram.

## AGENTS.md
{identity}

## SOUL.md
{soul}

## MATURANA.md
{contract}

## Durable Memory
{memory}

## Agent Context
{agent_context}

## Shared Wiki Index
{wiki_index}

## Relevant Wiki Chunks
{wiki_chunks}
{graph_section}{learned_section}
## Recent Telegram Transcript
{transcript}

## Current Telegram Message
{user_message}
"#,
        identity = context.identity.contents,
        soul = context.soul.contents,
        contract = context.contract.contents,
        memory = context.memory.contents,
        agent_context = context.agent_context.contents,
        wiki_index = context.wiki_index.contents,
        transcript = context.transcript,
    )
}

fn write_channel_context_manifest(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    context: &ChannelContextBundle,
) -> anyhow::Result<()> {
    let path = channel_context_manifest_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let source_files = vec![
        context.identity.summary.clone(),
        context.soul.summary.clone(),
        context.contract.summary.clone(),
        context.memory.summary.clone(),
        context.agent_context.summary.clone(),
        context.wiki_index.summary.clone(),
    ];
    let wiki_chunks = context
        .wiki_chunks
        .iter()
        .map(|chunk| LoadedWikiChunkSummary {
            score: chunk.score,
            matched_terms: chunk.matched_terms.clone(),
            path: chunk.path.display().to_string(),
            chars: chunk.text.chars().count(),
        })
        .collect();
    let graph_context_chars = context
        .graph_context
        .as_ref()
        .map(|graph| graph.rendered.chars().count())
        .unwrap_or(0);
    let loaded_context_chars = source_files.iter().map(|file| file.chars).sum::<usize>()
        + context
            .wiki_chunks
            .iter()
            .map(|chunk| chunk.text.chars().count())
            .sum::<usize>()
        + graph_context_chars
        + context.transcript.chars().count();
    let manifest = ChannelContextManifest {
        at: Utc::now(),
        agent_id: agent_id.to_string(),
        chat_id,
        source_files,
        wiki_chunks,
        wiki_query_terms: context.wiki_query_terms.clone(),
        wiki_term_sources: context.wiki_term_sources.clone(),
        graph_name: context
            .graph_context
            .as_ref()
            .map(|graph| graph.graph.clone()),
        graph_context_chars,
        context_policy: ContextPolicySummary {
            strategy: "durable-files-plus-current-message-and-recent-transcript-wiki-terms"
                .to_string(),
            wiki_chunk_limit: CONTEXT_WIKI_CHUNK_LIMIT,
            wiki_char_budget: WIKI_CHUNK_CONTEXT_CHARS,
            transcript_char_budget: TRANSCRIPT_CONTEXT_CHARS,
            excludes_reset_marker: true,
        },
        loaded_context_chars,
        transcript_path: context.transcript_path.display().to_string(),
        transcript_chars: context.transcript.chars().count(),
    };
    fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(())
}

fn load_relevant_wiki_chunks_for_terms(
    home: &MaturanaHome,
    terms: &[String],
    limit: usize,
    char_budget: usize,
) -> anyhow::Result<Vec<LoadedWikiChunk>> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let chunk_dir = home.root().join("wiki/chunks");
    if !chunk_dir.exists() {
        return Ok(Vec::new());
    }
    let mut hits = Vec::new();
    for entry in fs::read_dir(chunk_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let lower = raw.to_ascii_lowercase();
        let matched_terms = terms
            .iter()
            .filter(|term| lower.contains(term.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let score = matched_terms.len();
        if score > 0 {
            hits.push((score, matched_terms, path, raw));
        }
    }
    hits.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut chunks = Vec::new();
    let mut used_chars = 0usize;
    for (score, matched_terms, path, raw) in hits.into_iter().take(limit.max(1)) {
        let remaining = char_budget.saturating_sub(used_chars);
        if remaining == 0 {
            break;
        }
        let text = truncate_chars(&raw, remaining.min(2000));
        used_chars += text.chars().count();
        chunks.push(LoadedWikiChunk {
            score,
            matched_terms,
            path,
            text,
        });
    }
    Ok(chunks)
}

#[derive(Debug)]
struct WikiQueryPolicy {
    term_sources: Vec<WikiTermSource>,
}

fn build_wiki_query_policy(user_message: &str, transcript: &str) -> WikiQueryPolicy {
    let mut terms = BTreeMap::<String, Vec<String>>::new();
    collect_wiki_query_terms("current_message", user_message, &mut terms);
    collect_wiki_query_terms(
        "recent_transcript",
        &transcript_for_wiki_query(transcript),
        &mut terms,
    );
    WikiQueryPolicy {
        term_sources: terms
            .into_iter()
            .map(|(term, sources)| WikiTermSource { term, sources })
            .collect(),
    }
}

fn collect_wiki_query_terms(source: &str, text: &str, terms: &mut BTreeMap<String, Vec<String>>) {
    for term in extract_wiki_query_terms(text) {
        let sources = terms.entry(term).or_default();
        if !sources.iter().any(|existing| existing == source) {
            sources.push(source.to_string());
        }
    }
}

fn extract_wiki_query_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split_whitespace()
        .map(normalize_wiki_query_term)
        .filter(|term| term.len() >= 3 && !is_wiki_query_stopword(term))
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn normalize_wiki_query_term(term: &str) -> String {
    term.trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

fn is_wiki_query_stopword(term: &str) -> bool {
    matches!(
        term,
        "about"
            | "again"
            | "agent"
            | "context"
            | "current"
            | "durable"
            | "hello"
            | "memory"
            | "maturana"
            | "message"
            | "please"
            | "reload"
            | "reloaded"
            | "session"
            | "should"
            | "telegram"
            | "transcript"
            | "turn"
            | "what"
            | "wiki"
            | "with"
    )
}

fn transcript_for_wiki_query(transcript: &str) -> String {
    let lines = transcript
        .lines()
        .filter(|line| !line.starts_with("# Telegram Session"))
        .filter(|line| !line.starts_with("Started:"))
        .filter(|line| !line.contains("Memory and wiki context will be reloaded"))
        .collect::<Vec<_>>();
    lines.join("\n")
}

fn render_wiki_chunks(chunks: &[LoadedWikiChunk]) -> String {
    if chunks.is_empty() {
        return "(no relevant wiki chunks found)".to_string();
    }
    let mut output = String::new();
    for chunk in chunks {
        output.push_str(&format!(
            "\n### {} score={} matched_terms={}\n\n{}\n",
            chunk.path.display(),
            chunk.score,
            chunk.matched_terms.join(","),
            chunk.text.trim()
        ));
    }
    output
}

fn read_context_file(label: &str, path: &Path, limit: usize) -> anyhow::Result<ContextFile> {
    if !path.exists() {
        return Ok(ContextFile {
            contents: "(missing)".to_string(),
            summary: LoadedContextFile {
                label: label.to_string(),
                path: path.display().to_string(),
                chars: 0,
                missing: true,
            },
        });
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let contents = truncate_chars(&contents, limit);
    Ok(ContextFile {
        summary: LoadedContextFile {
            label: label.to_string(),
            path: path.display().to_string(),
            chars: contents.chars().count(),
            missing: false,
        },
        contents,
    })
}

fn tail_context_file(path: &Path, limit: usize) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok("(no transcript yet)".to_string());
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let char_count = contents.chars().count();
    if char_count <= limit {
        return Ok(contents);
    }
    Ok(format!(
        "[older transcript omitted]\n{}",
        contents
            .chars()
            .skip(char_count.saturating_sub(limit))
            .collect::<String>()
    ))
}

fn append_channel_turn(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    role: &str,
    text: &str,
) -> anyhow::Result<()> {
    let path = channel_transcript_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = format!(
        "\n## {} {}\n\n{}\n",
        Utc::now().to_rfc3339(),
        role,
        text.trim()
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;
    Ok(())
}

fn reset_channel_context(home: &MaturanaHome, agent_id: &str, chat_id: i64) -> anyhow::Result<()> {
    let path = channel_transcript_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let reset_id = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    if path.exists() {
        let archive_dir = path
            .parent()
            .expect("transcript path always has a parent")
            .join("archive");
        fs::create_dir_all(&archive_dir)?;
        let archive = archive_dir.join(format!("{chat_id}-{reset_id}.md"));
        fs::rename(&path, archive)?;
    }
    let manifest_path = channel_context_manifest_path(home, agent_id, chat_id);
    if manifest_path.exists() {
        let archive_dir = manifest_path
            .parent()
            .expect("context manifest path always has a parent")
            .join("archive");
        fs::create_dir_all(&archive_dir)?;
        let archive = archive_dir.join(format!("{chat_id}-{reset_id}.context.json"));
        fs::rename(&manifest_path, archive)?;
    }
    fs::write(
        &path,
        format!(
            "# Telegram Session\n\nStarted: {}\n\nMemory and wiki context will be reloaded on the next turn.\n",
            Utc::now().to_rfc3339()
        ),
    )?;
    Ok(())
}

/// Decide whether a user message carries a durable fact worth remembering, and
/// return the fact text. Explicit cues ("remember …", "/remember …") win and are
/// stripped to the bare fact; otherwise a tight set of high-signal heuristics
/// (identity, contact, location, preferences, commitments) captures the message.
/// Deliberately conservative to avoid surprising the user with noisy memories.
fn extract_memory_fact(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    for cue in [
        "/remember ",
        "remember that ",
        "remember this: ",
        "remember this:",
        "remember: ",
        "remember:",
        "please remember ",
        "remember ",
    ] {
        if let Some(rest) = lower.strip_prefix(cue) {
            let fact = t[t.len() - rest.len()..].trim();
            if !fact.is_empty() {
                return Some(fact.to_string());
            }
        }
    }
    const HEURISTICS: &[&str] = &[
        "my name is",
        "call me ",
        "i prefer",
        "i live in",
        "i work at",
        "my email",
        "my phone",
        "my timezone",
        "my birthday",
        "remind me",
        "deadline",
        "due by",
        "due on",
    ];
    if HEURISTICS.iter().any(|h| lower.contains(h)) {
        return Some(t.to_string());
    }
    None
}

fn maybe_remember_user_message(
    home: &MaturanaHome,
    agent_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let Some(fact) = extract_memory_fact(text) else {
        return Ok(());
    };

    // 1. Durable MEMORY.md (loaded into the channel context every turn).
    let path = home.agent_dir(agent_id).join("memory/MEMORY.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(&path, "# Memory\n")?;
    }
    let entry = format!("\n- {}: {}\n", Utc::now().date_naive(), fact);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;

    // 2. Private memory section in MaturanaGraph (best-effort; powers GraphRAG
    //    recall via the blended read). A graph hiccup must never break the turn.
    if let Some(token) = maturana_core::worker::read_graph_token(home.root()) {
        let agent_graph = crate::graph::agent_graph_name(agent_id);
        let dir = home.agent_dir(agent_id).join("inbox");
        if fs::create_dir_all(&dir).is_ok() {
            let note = dir.join(format!("memory-{}.md", Utc::now().timestamp_millis()));
            if fs::write(&note, &fact).is_ok() {
                let _ = crate::graph::ingest_file_into_service(
                    crate::graph::DEFAULT_LOCAL_URL,
                    &token,
                    &agent_graph,
                    &note,
                    1200,
                );
            }
        }
    }
    Ok(())
}

fn channel_transcript_path(home: &MaturanaHome, agent_id: &str, chat_id: i64) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/telegram")
        .join(format!("{chat_id}.md"))
}

fn channel_context_manifest_path(home: &MaturanaHome, agent_id: &str, chat_id: i64) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/telegram")
        .join(format!("{chat_id}.context.json"))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "\n...[truncated]"
}

fn read_telegram_state(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<TelegramChannelState> {
    let path = telegram_state_path(home, agent_id);
    if !path.exists() {
        return Ok(TelegramChannelState::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn write_telegram_state(
    home: &MaturanaHome,
    agent_id: &str,
    state: &TelegramChannelState,
) -> anyhow::Result<()> {
    let path = telegram_state_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)
        .context("failed to write telegram channel state")
}

fn write_telegram_heartbeat(
    home: &MaturanaHome,
    agent_id: &str,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let path = telegram_heartbeat_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::json!({
            "agent_id": agent_id,
            "status": status,
            "error": error,
            "at": Utc::now(),
        }))?,
    )
    .context("failed to write telegram channel heartbeat")
}

fn telegram_state_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    if agent_id == "default" {
        home.root().join("channels/telegram/state.json")
    } else {
        home.agent_dir(agent_id)
            .join("channels/telegram/state.json")
    }
}

fn telegram_heartbeat_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    if agent_id == "default" {
        home.root().join("channels/telegram/heartbeat.json")
    } else {
        home.agent_dir(agent_id)
            .join("channels/telegram/heartbeat.json")
    }
}

fn telegram_pair_code_key(agent_id: &str) -> String {
    if agent_id == "default" {
        TELEGRAM_PAIR_CODE.to_string()
    } else {
        format!("telegram/{agent_id}/pair-code")
    }
}

fn telegram_chat_id_key(agent_id: &str) -> String {
    if agent_id == "default" {
        TELEGRAM_CHAT_ID.to_string()
    } else {
        format!("telegram/{agent_id}/chat-id")
    }
}

fn audit_channel_event(
    home: &MaturanaHome,
    agent_id: &str,
    action: &str,
    message: &str,
) -> anyhow::Result<()> {
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            message: message.to_string(),
        },
    )
}

fn truncate_for_telegram(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_RESPONSE_CHARS {
        return value.to_string();
    }
    value.chars().take(MAX_RESPONSE_CHARS).collect::<String>() + "\n...[truncated]"
}

// ===================== AgentMail (HTTP poll) =====================

const AGENTMAIL_BASE: &str = "https://api.agentmail.to/v0";

#[derive(Debug, Serialize, Deserialize, Default)]
struct AgentMailState {
    /// Highest message timestamp seen, so we only enqueue newer mail.
    last_seen: Option<String>,
}

fn serve_agentmail(home: &MaturanaHome, config: AgentMailServe) -> anyhow::Result<()> {
    let key = resolve_secret_source_with_home(&config.api_key_source, home.root())?;
    let key = key.expose_for_runtime().to_string();
    let inbox = config
        .inbox
        .clone()
        .map(Ok)
        .unwrap_or_else(|| agentmail_default_inbox(&key))?;
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!("agentmail channel serving agent {} inbox {inbox}", config.agent_id);
    let mut state: AgentMailState = read_channel_state(home, &config.agent_id, "agentmail")?;
    loop {
        match agentmail_poll(&key, &inbox, state.last_seen.as_deref()) {
            Ok(messages) => {
                for msg in &messages {
                    enqueue_channel_prompt(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "agentmail",
                        &msg.thread_id,
                        Some(&msg.message_id),
                        &msg.text,
                    )?;
                    state.last_seen = Some(msg.timestamp.clone());
                }
                write_channel_state(home, &config.agent_id, "agentmail", &state)?;
                if let Some(provider) = &config.run_once_provider {
                    let options = RunnerOptions { provider: provider.to_string() };
                    run_session_once(&paths, &options, 20)?;
                }
                // Deliver replies for each thread we know about.
                for msg in &messages {
                    let key2 = key.clone();
                    let inbox2 = inbox.clone();
                    let thread = msg.thread_id.clone();
                    deliver_channel_outbox(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "agentmail",
                        &msg.thread_id,
                        |text, reply_to| {
                            agentmail_send(&key2, &inbox2, &thread, reply_to, text)
                        },
                    )?;
                }
            }
            Err(error) => eprintln!("agentmail poll error: {error}"),
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(config.poll_seconds.max(2)));
    }
    Ok(())
}

struct AgentMailMessage {
    message_id: String,
    thread_id: String,
    timestamp: String,
    text: String,
}

fn agentmail_default_inbox(key: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = ureq::get(&format!("{AGENTMAIL_BASE}/inboxes"))
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| anyhow::anyhow!("agentmail list inboxes failed: {e}"))?
        .into_json()?;
    resp.get("inboxes")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("inbox_id").or_else(|| i.get("id")))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("agentmail account has no inbox; pass --inbox"))
}

fn agentmail_poll(
    key: &str,
    inbox: &str,
    since: Option<&str>,
) -> anyhow::Result<Vec<AgentMailMessage>> {
    let mut url = format!("{AGENTMAIL_BASE}/inboxes/{inbox}/messages?limit=20");
    if let Some(since) = since {
        url.push_str(&format!("&after={since}"));
    }
    let resp: serde_json::Value = ureq::get(&url)
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| anyhow::anyhow!("agentmail list messages failed: {e}"))?
        .into_json()?;
    let mut out = Vec::new();
    let items = resp
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    for m in items {
        // Skip our own sent mail.
        if m.get("type").and_then(|t| t.as_str()) == Some("sent") {
            continue;
        }
        let text = m
            .get("text")
            .or_else(|| m.get("preview"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        out.push(AgentMailMessage {
            message_id: m.get("message_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            thread_id: m
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| m.get("message_id").and_then(|v| v.as_str()).unwrap_or(""))
                .to_string(),
            timestamp: m
                .get("timestamp")
                .or_else(|| m.get("created_at"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            text,
        });
    }
    Ok(out)
}

fn agentmail_send(
    key: &str,
    inbox: &str,
    thread_id: &str,
    reply_to: Option<&str>,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let body = serde_json::json!({
        "text": text,
        "thread_id": thread_id,
        "in_reply_to": reply_to,
    });
    let resp: serde_json::Value = ureq::post(&format!("{AGENTMAIL_BASE}/inboxes/{inbox}/messages/send"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(body)
        .map_err(|e| anyhow::anyhow!("agentmail send failed: {e}"))?
        .into_json()?;
    Ok(resp.get("message_id").and_then(|v| v.as_str()).map(str::to_string))
}

// ===================== Slack (Socket Mode) =====================

fn serve_slack(home: &MaturanaHome, config: SlackServe) -> anyhow::Result<()> {
    let bot = resolve_secret_source_with_home(&config.bot_token_source, home.root())?;
    let bot = bot.expose_for_runtime().to_string();
    let app = resolve_secret_source_with_home(&config.app_token_source, home.root())?;
    let app = app.expose_for_runtime().to_string();
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!("slack channel serving agent {}", config.agent_id);
    loop {
        if let Err(error) = slack_socket_session(home, &config, &bot, &app, &paths) {
            eprintln!("slack socket error: {error}");
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

/// Open a Socket Mode WebSocket and process events until it drops.
fn slack_socket_session(
    home: &MaturanaHome,
    config: &SlackServe,
    bot_token: &str,
    app_token: &str,
    paths: &SessionPaths,
) -> anyhow::Result<()> {
    let ws_url = slack_open_connection(app_token)?;
    let (mut socket, _) = tungstenite::connect(&ws_url)
        .map_err(|e| anyhow::anyhow!("slack socket connect failed: {e}"))?;
    loop {
        let msg = socket.read().map_err(|e| anyhow::anyhow!("slack read: {e}"))?;
        let tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(&text)?;
        let envelope_type = envelope.get("type").and_then(|t| t.as_str()).unwrap_or("");
        // Ack every envelope that carries one (Slack requires it within 3s).
        if let Some(envelope_id) = envelope.get("envelope_id").and_then(|v| v.as_str()) {
            let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
            let _ = socket.send(tungstenite::Message::Text(ack));
        }
        if envelope_type != "events_api" {
            continue; // hello / disconnect handled by the outer reconnect loop
        }
        if let Some((channel, text, thread)) = slack_extract_prompt(&envelope) {
            enqueue_channel_prompt(
                home,
                &config.agent_id,
                &config.session_id,
                "slack",
                &channel,
                thread.as_deref(),
                &text,
            )?;
            if let Some(provider) = &config.run_once_provider {
                let options = RunnerOptions { provider: provider.to_string() };
                run_session_once(paths, &options, 20)?;
            }
            let bot = bot_token.to_string();
            let thread_for_send = thread.clone();
            deliver_channel_outbox(
                home,
                &config.agent_id,
                &config.session_id,
                "slack",
                &channel,
                |reply, _| slack_post_message(&bot, &channel, thread_for_send.as_deref(), reply),
            )?;
        }
    }
}

fn slack_open_connection(app_token: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = ureq::post("https://slack.com/api/apps.connections.open")
        .set("authorization", &format!("Bearer {app_token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("slack apps.connections.open failed: {e}"))?
        .into_json()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!("slack apps.connections.open returned not-ok: {resp}");
    }
    resp.get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("slack response missing url"))
}

/// Pull (channel, text, thread_ts) from an events_api envelope for a user
/// message / app_mention; returns None for bot messages and non-message events.
fn slack_extract_prompt(envelope: &serde_json::Value) -> Option<(String, String, Option<String>)> {
    let event = envelope.pointer("/payload/event")?;
    let kind = event.get("type").and_then(|t| t.as_str())?;
    if kind != "message" && kind != "app_mention" {
        return None;
    }
    // Ignore bot/our-own messages and edits.
    if event.get("bot_id").is_some() || event.get("subtype").is_some() {
        return None;
    }
    let text = event.get("text").and_then(|t| t.as_str())?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let channel = event.get("channel").and_then(|c| c.as_str())?.to_string();
    let thread = event
        .get("thread_ts")
        .or_else(|| event.get("ts"))
        .and_then(|t| t.as_str())
        .map(str::to_string);
    Some((channel, strip_slack_mention(&text), thread))
}

/// Remove a leading `<@U…>` bot mention so the prompt is clean.
fn strip_slack_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(close) = rest.find('>') {
            if rest.starts_with('@') {
                return rest[close + 1..].trim().to_string();
            }
        }
    }
    text.to_string()
}

fn slack_post_message(
    bot_token: &str,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let mut body = serde_json::json!({ "channel": channel, "text": text });
    if let Some(thread) = thread_ts {
        body["thread_ts"] = serde_json::json!(thread);
    }
    let resp: serde_json::Value = ureq::post("https://slack.com/api/chat.postMessage")
        .set("authorization", &format!("Bearer {bot_token}"))
        .send_json(body)
        .map_err(|e| anyhow::anyhow!("slack chat.postMessage failed: {e}"))?
        .into_json()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!("slack chat.postMessage not-ok: {resp}");
    }
    Ok(resp.get("ts").and_then(|v| v.as_str()).map(str::to_string))
}

// ---- shared channel-state persistence (generic over channel name) ----

fn read_channel_state<T: serde::de::DeserializeOwned + Default>(
    home: &MaturanaHome,
    agent_id: &str,
    channel: &str,
) -> anyhow::Result<T> {
    let path = home
        .agent_dir(agent_id)
        .join("channels")
        .join(channel)
        .join("state.json");
    if !path.exists() {
        return Ok(T::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn write_channel_state<T: Serialize>(
    home: &MaturanaHome,
    agent_id: &str,
    channel: &str,
    state: &T,
) -> anyhow::Result<()> {
    let path = home
        .agent_dir(agent_id)
        .join("channels")
        .join(channel)
        .join("state.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// Stable per-conversation key for channels whose platform id is a string
/// (Slack channel, AgentMail thread). Reuses all the i64-keyed transcript /
/// context machinery without changing the Telegram signatures.
pub(crate) fn stable_chat_key(platform_id: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    platform_id.hash(&mut hasher);
    (hasher.finish() >> 1) as i64 // positive
}

/// Enqueue a user message as a chat prompt for the guest worker, building the
/// full channel context exactly like the Telegram path. Shared by Slack and
/// AgentMail.
pub(crate) fn enqueue_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    let key = stable_chat_key(platform_id);
    append_channel_turn(home, agent_id, key, "user", text)?;
    maybe_remember_user_message(home, agent_id, text)?;
    let prompt = build_channel_prompt(home, agent_id, key, text)?;
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    insert_inbound(
        &paths,
        "chat",
        channel,
        platform_id,
        thread_id,
        &serde_json::json!({ "text": text, "prompt": prompt }).to_string(),
    )?;
    Ok(())
}

/// Deliver undelivered outbound rows for `channel`+`platform_id` using a
/// channel-specific `send` closure (returns the platform message id). Mirrors
/// `deliver_telegram_outbox` generically.
pub(crate) fn deliver_channel_outbox<F>(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    mut send: F,
) -> anyhow::Result<usize>
where
    F: FnMut(&str, Option<&str>) -> anyhow::Result<Option<String>>,
{
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let key = stable_chat_key(platform_id);
    let mut delivered = 0;
    for message in list_undelivered(&paths)? {
        if message.channel != channel || message.platform_id != platform_id {
            continue;
        }
        let response = truncate_for_telegram(&message_text(&message.content)?);
        let platform_message_id = send(&response, message.thread_id.as_deref())?;
        append_channel_turn(home, agent_id, key, "assistant", &response)?;
        mark_delivered(&paths, &message.id, platform_message_id.as_deref())?;
        audit_channel_event(
            home,
            agent_id,
            &format!("channel.{channel}.outbound"),
            "sent channel response",
        )?;
        delivered += 1;
    }
    Ok(delivered)
}

fn generate_pair_code() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .map(|ch| ch.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pair_before_authorization() {
        let update = text_update(7, "/pair ABC123");
        assert_eq!(
            classify_telegram_update(&update, None, Some("ABC123")),
            InboundAction::Pair { chat_id: 7 }
        );
    }

    #[test]
    fn denies_unpaired_chat() {
        let update = text_update(9, "hello");
        assert_eq!(
            classify_telegram_update(&update, Some(7), None),
            InboundAction::Deny { chat_id: 9 }
        );
    }

    #[test]
    fn routes_paired_prompt_and_status() {
        assert_eq!(
            classify_telegram_update(&text_update(7, "/status"), Some(7), None),
            InboundAction::Status { chat_id: 7 }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/new"), Some(7), None),
            InboundAction::New { chat_id: 7 }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "hello"), Some(7), None),
            InboundAction::Prompt {
                chat_id: 7,
                text: "hello".to_string()
            }
        );
    }

    #[test]
    fn routes_tool_and_feedback_commands() {
        assert_eq!(
            classify_telegram_update(&text_update(7, "/tool weather {\"city\":\"oslo\"}"), Some(7), None),
            InboundAction::Tool {
                chat_id: 7,
                name: "weather".to_string(),
                input: "{\"city\":\"oslo\"}".to_string(),
            }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/tool weather"), Some(7), None),
            InboundAction::Tool {
                chat_id: 7,
                name: "weather".to_string(),
                input: "{}".to_string(),
            }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/good"), Some(7), None),
            InboundAction::Feedback {
                chat_id: 7,
                value: signals::THUMBS_UP,
            }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/bad"), Some(7), None),
            InboundAction::Feedback {
                chat_id: 7,
                value: signals::THUMBS_DOWN,
            }
        );
        // An invalid tool name falls back to help rather than crashing.
        assert_eq!(
            classify_telegram_update(&text_update(7, "/tool Bad_Name"), Some(7), None),
            InboundAction::Help { chat_id: 7 }
        );
    }

    #[test]
    fn routes_spawn_command() {
        assert_eq!(
            classify_telegram_update(
                &text_update(7, "/spawn persistent Researcher -- find context"),
                Some(7),
                None
            ),
            InboundAction::Spawn {
                chat_id: 7,
                mode: SpawnMode::Persistent,
                name: "researcher".to_string(),
                prompt: "find context".to_string(),
            }
        );
    }

    #[test]
    fn routes_new_slash_commands() {
        assert_eq!(
            classify_telegram_update(&text_update(7, "/models"), Some(7), None),
            InboundAction::Command { chat_id: 7, name: "models".to_string(), args: String::new() }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/model openai/gpt-5"), Some(7), None),
            InboundAction::Command { chat_id: 7, name: "model".to_string(), args: "openai/gpt-5".to_string() }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/graph-query roadmap q3"), Some(7), None),
            InboundAction::Command { chat_id: 7, name: "graph-query".to_string(), args: "roadmap q3".to_string() }
        );
        // /emerge spawns a sub-agent; /skill <name> becomes a prompt.
        assert_eq!(
            classify_telegram_update(&text_update(7, "/emerge summarize my inbox"), Some(7), None),
            InboundAction::Spawn {
                chat_id: 7,
                mode: SpawnMode::Ephemeral,
                name: "summarize-my-inbox".to_string(),
                prompt: "summarize my inbox".to_string(),
            }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/skill maturana-pipelock list"), Some(7), None),
            InboundAction::Prompt {
                chat_id: 7,
                text: "Use the `maturana-pipelock` skill. list".to_string(),
            }
        );
        // Unknown slash command routes to the command handler (replies via /help).
        assert_eq!(
            classify_telegram_update(&text_update(7, "/wat"), Some(7), None),
            InboundAction::Command { chat_id: 7, name: "unknown".to_string(), args: "/wat".to_string() }
        );
    }

    #[test]
    fn memory_extraction_explicit_and_heuristic() {
        // Explicit cue is stripped to the bare fact.
        assert_eq!(
            extract_memory_fact("remember that my standup is at 9am"),
            Some("my standup is at 9am".to_string())
        );
        assert_eq!(
            extract_memory_fact("/remember the API key rotates monthly"),
            Some("the API key rotates monthly".to_string())
        );
        // Heuristic captures the whole message.
        assert_eq!(
            extract_memory_fact("My name is Anders"),
            Some("My name is Anders".to_string())
        );
        assert_eq!(
            extract_memory_fact("remind me to call the bank tomorrow"),
            Some("remind me to call the bank tomorrow".to_string())
        );
        // Ordinary chatter is not remembered.
        assert_eq!(extract_memory_fact("what's the weather like?"), None);
        assert_eq!(extract_memory_fact("   "), None);
    }

    #[test]
    fn help_and_commands_cover_the_catalog() {
        let help = help_text();
        for group in ["Session", "Options", "Status", "Management", "MaturanaGraph", "Voice"] {
            assert!(help.contains(group), "help missing group {group}");
        }
        for cmd in ["/model", "/models", "/tools", "/status", "/subagents", "/graph-query", "/tts"] {
            assert!(help.contains(cmd), "help missing {cmd}");
            assert!(commands_text().contains(cmd), "commands missing {cmd}");
        }
    }

    #[test]
    fn pair_command_accepts_bot_suffix() {
        assert!(is_pair_command("/pair@LuhmannSystemsBot ABC123", "ABC123"));
        assert!(!is_pair_command("/pair@LuhmannSystemsBot WRONG", "ABC123"));
    }

    #[test]
    fn channel_prompt_includes_memory_and_transcript() {
        let temp = temp_dir("channel-prompt");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::create_dir_all(agent_dir.join("context")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "likes tea\n").unwrap();
        fs::write(agent_dir.join("context/README.md"), "local context\n").unwrap();
        fs::create_dir_all(home.root().join("wiki/chunks")).unwrap();
        fs::write(
            home.root().join("wiki/chunks/tea-001.md"),
            "Tea ceremonies are relevant shared context.\n",
        )
        .unwrap();
        append_channel_turn(&home, "agent", 42, "user", "my name is Anders").unwrap();

        let prompt =
            build_channel_prompt(&home, "agent", 42, "what is my name and tea preference?")
                .unwrap();
        assert!(prompt.contains("likes tea"));
        assert!(prompt.contains("Tea ceremonies"));
        assert!(prompt.contains("my name is Anders"));
        assert!(prompt.contains("what is my name and tea preference?"));
        let manifest_path = channel_context_manifest_path(&home, "agent", 42);
        let manifest: ChannelContextManifest =
            serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.agent_id, "agent");
        assert_eq!(manifest.chat_id, 42);
        assert_eq!(manifest.wiki_chunks.len(), 1);
        assert!(manifest.loaded_context_chars > 0);
        assert!(manifest.wiki_query_terms.contains(&"name".to_string()));
        assert_eq!(
            manifest.context_policy.strategy,
            "durable-files-plus-current-message-and-recent-transcript-wiki-terms"
        );
        assert!(manifest.context_policy.excludes_reset_marker);
        assert!(manifest.wiki_chunks[0]
            .matched_terms
            .contains(&"tea".to_string()));
        assert!(manifest.wiki_term_sources.iter().any(
            |term| term.term == "tea" && term.sources.contains(&"current_message".to_string())
        ));
        assert!(manifest
            .source_files
            .iter()
            .any(|file| file.label == "memory/MEMORY.md" && !file.missing));
    }

    #[test]
    fn channel_context_selects_wiki_from_recent_transcript_for_followups() {
        let temp = temp_dir("channel-followup-context");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::create_dir_all(agent_dir.join("context")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
        fs::write(agent_dir.join("context/README.md"), "# Context\n").unwrap();
        fs::create_dir_all(home.root().join("wiki/chunks")).unwrap();
        fs::write(
            home.root().join("wiki/chunks/calendars-001.md"),
            "Calendar planning context should be loaded for schedule follow-ups.\n",
        )
        .unwrap();
        append_channel_turn(
            &home,
            "agent",
            42,
            "user",
            "Please remember the calendar planning context.",
        )
        .unwrap();

        let prompt = build_channel_prompt(&home, "agent", 42, "what about that?").unwrap();
        assert!(prompt.contains("Calendar planning context"));
        let manifest: ChannelContextManifest = serde_json::from_str(
            &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.wiki_chunks.len(), 1);
        assert!(manifest.wiki_chunks[0].path.contains("calendars-001.md"));
        assert!(manifest.wiki_chunks[0]
            .matched_terms
            .contains(&"calendar".to_string()));
        assert!(manifest.wiki_query_terms.contains(&"calendar".to_string()));
        assert!(manifest
            .wiki_term_sources
            .iter()
            .any(|term| term.term == "calendar"
                && term.sources.contains(&"recent_transcript".to_string())));
    }

    #[test]
    fn new_session_rotates_transcript_and_reloads_context_next_turn() {
        let temp = temp_dir("channel-new-session");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::create_dir_all(agent_dir.join("context")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "prefers fresh starts\n").unwrap();
        fs::write(agent_dir.join("context/README.md"), "local context\n").unwrap();
        append_channel_turn(&home, "agent", 42, "user", "old context").unwrap();
        fs::write(
            channel_context_manifest_path(&home, "agent", 42),
            r#"{"stale":true}"#,
        )
        .unwrap();

        reset_channel_context(&home, "agent", 42).unwrap();

        let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 42)).unwrap();
        assert!(transcript.contains("Memory and wiki context will be reloaded"));
        assert!(!transcript.contains("old context"));
        let archive_dir = home.agent_dir("agent").join("channels/telegram/archive");
        let archive_files = fs::read_dir(archive_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(archive_files.len(), 2);
        assert!(archive_files.iter().any(|name| name.ends_with(".md")));
        assert!(archive_files
            .iter()
            .any(|name| name.ends_with(".context.json")));
        assert!(!channel_context_manifest_path(&home, "agent", 42).exists());
        let prompt = build_channel_prompt(&home, "agent", 42, "hello again").unwrap();
        assert!(prompt.contains("prefers fresh starts"));
        assert!(!prompt.contains("old context"));
        assert!(channel_context_manifest_path(&home, "agent", 42).exists());
    }

    #[test]
    fn new_session_does_not_use_reset_marker_or_archived_transcript_for_wiki() {
        let temp = temp_dir("channel-new-session-wiki-query");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::create_dir_all(agent_dir.join("context")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
        fs::write(agent_dir.join("context/README.md"), "# Context\n").unwrap();
        fs::create_dir_all(home.root().join("wiki/chunks")).unwrap();
        fs::write(
            home.root().join("wiki/chunks/archived-001.md"),
            "Archived topic oldcontext should not leak into a fresh session.\n",
        )
        .unwrap();
        fs::write(
            home.root().join("wiki/chunks/reset-marker-001.md"),
            "Memory wiki context reloaded turn marker should never drive retrieval.\n",
        )
        .unwrap();
        fs::write(
            home.root().join("wiki/chunks/fresh-001.md"),
            "Freshnote is the only relevant shared context for the new question.\n",
        )
        .unwrap();
        append_channel_turn(
            &home,
            "agent",
            42,
            "user",
            "Please use oldcontext next time.",
        )
        .unwrap();

        reset_channel_context(&home, "agent", 42).unwrap();
        let prompt = build_channel_prompt(&home, "agent", 42, "freshnote please").unwrap();

        assert!(prompt.contains("Freshnote"));
        assert!(!prompt.contains("oldcontext"));
        assert!(!prompt.contains("reloaded turn marker"));
        let manifest: ChannelContextManifest = serde_json::from_str(
            &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
        )
        .unwrap();
        assert!(manifest.wiki_query_terms.contains(&"freshnote".to_string()));
        assert!(!manifest
            .wiki_query_terms
            .contains(&"oldcontext".to_string()));
        assert!(!manifest.wiki_query_terms.contains(&"reloaded".to_string()));
        assert_eq!(manifest.wiki_chunks.len(), 1);
        assert!(manifest.wiki_chunks[0].path.contains("fresh-001.md"));
        assert_eq!(
            manifest.wiki_chunks[0].matched_terms,
            vec!["freshnote".to_string()]
        );
        assert!(manifest
            .wiki_term_sources
            .iter()
            .any(|term| term.term == "freshnote"
                && term.sources.contains(&"current_message".to_string())));
    }

    #[test]
    fn remember_message_appends_to_memory() {
        let temp = temp_dir("channel-memory");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        maybe_remember_user_message(&home, "agent", "remember that I prefer short replies")
            .unwrap();

        let memory = fs::read_to_string(home.agent_dir("agent").join("memory/MEMORY.md")).unwrap();
        // The explicit "remember that" cue is stripped to the bare fact.
        assert!(memory.contains("I prefer short replies"));
        assert!(!memory.contains("remember that"));
    }

    #[test]
    fn slack_extracts_user_message_and_strips_mention() {
        let envelope = serde_json::json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": { "event": {
                "type": "app_mention",
                "channel": "C123",
                "ts": "1700.1",
                "text": "<@U0BOT> what is the roadmap?"
            }}
        });
        let (channel, text, thread) = slack_extract_prompt(&envelope).unwrap();
        assert_eq!(channel, "C123");
        assert_eq!(text, "what is the roadmap?");
        assert_eq!(thread.as_deref(), Some("1700.1"));
    }

    #[test]
    fn slack_ignores_bot_and_non_message_events() {
        let bot = serde_json::json!({
            "type": "events_api",
            "payload": { "event": { "type": "message", "channel": "C1", "text": "hi", "bot_id": "B1" }}
        });
        assert!(slack_extract_prompt(&bot).is_none());
        let edit = serde_json::json!({
            "type": "events_api",
            "payload": { "event": { "type": "message", "channel": "C1", "text": "hi", "subtype": "message_changed" }}
        });
        assert!(slack_extract_prompt(&edit).is_none());
        let reaction = serde_json::json!({
            "type": "events_api",
            "payload": { "event": { "type": "reaction_added" }}
        });
        assert!(slack_extract_prompt(&reaction).is_none());
    }

    #[test]
    fn stable_chat_key_is_deterministic_and_positive() {
        let a = stable_chat_key("C123");
        assert_eq!(a, stable_chat_key("C123"));
        assert!(a >= 0);
        assert_ne!(a, stable_chat_key("C124"));
    }

    fn text_update(chat_id: i64, text: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                text: Some(text.to_string()),
                caption: None,
                document: None,
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
        }
    }

    fn document_update(chat_id: i64, file_name: &str, caption: Option<&str>) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                text: None,
                caption: caption.map(str::to_string),
                document: Some(TelegramDocument {
                    file_id: "file-123".to_string(),
                    file_name: Some(file_name.to_string()),
                    file_size: Some(1024),
                }),
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
        }
    }

    #[test]
    fn routes_document_uploads_from_paired_chat_only() {
        let update = document_update(7, "notes.pdf", Some("for the graph"));
        assert_eq!(
            classify_telegram_update(&update, Some(7), None),
            InboundAction::Document {
                chat_id: 7,
                document: TelegramDocument {
                    file_id: "file-123".to_string(),
                    file_name: Some("notes.pdf".to_string()),
                    file_size: Some(1024),
                },
                caption: Some("for the graph".to_string()),
            }
        );
        // The pairing gate applies to documents exactly like text.
        assert_eq!(
            classify_telegram_update(&document_update(9, "notes.pdf", None), Some(7), None),
            InboundAction::Deny { chat_id: 9 }
        );
        assert_eq!(
            classify_telegram_update(&document_update(9, "notes.pdf", None), None, None),
            InboundAction::Deny { chat_id: 9 }
        );
    }

    #[test]
    fn sanitizes_telegram_document_names() {
        assert_eq!(sanitize_document_name(Some("Q3 Roadmap.pdf")), "Q3 Roadmap.pdf");
        assert_eq!(
            sanitize_document_name(Some("../../etc/passwd")),
            "-..-etc-passwd"
        );
        assert_eq!(sanitize_document_name(Some("..")), "document");
        assert_eq!(sanitize_document_name(None), "document");
        assert_eq!(sanitize_document_name(Some("a/b\\c.md")), "a-b-c.md");
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "maturana-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
