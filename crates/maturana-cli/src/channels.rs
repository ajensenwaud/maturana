use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use maturana_core::{
    animation::{frame, is_terminal, Phase},
    audit::{append_event, AuditEvent},
    improvement::{signals, TrajectoryStore},
    pipelock::PipelockVault,
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, insert_inbound, list_undelivered, mark_delivered, session_paths},
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
        let updates = telegram_updates(token.expose_for_runtime(), state.offset)?;
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
        let updates = telegram_updates(&token, None)?;
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
    loop {
        write_telegram_heartbeat(home, &config.agent_id, "polling", None)?;
        let updates = match telegram_updates(&token, state.offset) {
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
        if let Some(chat_id) = current_paired_telegram_chat_id(home, &config.agent_id) {
            if let Err(error) =
                deliver_telegram_outbox(home, &token, &config.agent_id, &config.session_id, chat_id)
            {
                let message = error.to_string();
                write_telegram_heartbeat(home, &config.agent_id, "deliver_error", Some(&message))?;
                audit_channel_event(
                    home,
                    &config.agent_id,
                    "channel.telegram.deliver_error",
                    &message,
                )?;
                if config.once {
                    return Err(error);
                }
            }
        }
        write_telegram_heartbeat(home, &config.agent_id, "idle", None)?;
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(config.poll_seconds.max(1)));
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
            send_telegram(
                token,
                &chat_id.to_string(),
                "Maturana paired. You can now message the agent.",
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Help { chat_id } => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "Commands: /status, /new, /spawn, /help. Any other message is sent to the agent.",
                reply_to_message_id,
            )?;
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
                "Maturana channel is alive and paired.",
                reply_to_message_id,
            )?;
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
        (Some(token), true) => (token, knowledge_graph.graph_name(&config.agent_id)),
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
    match command.as_str() {
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
        _ if command.starts_with("/tool ") => match parse_tool_command(&command) {
            Some((name, input)) => InboundAction::Tool {
                chat_id,
                name,
                input,
            },
            None => InboundAction::Help { chat_id },
        },
        _ if command.starts_with("/spawn ") => match parse_spawn_command(&command) {
            Some((mode, name, prompt)) => InboundAction::Spawn {
                chat_id,
                mode,
                name,
                prompt,
            },
            None => InboundAction::Help { chat_id },
        },
        _ if command.starts_with('/') => InboundAction::Help { chat_id },
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

fn telegram_updates(token: &str, offset: Option<i64>) -> anyhow::Result<Vec<TelegramUpdate>> {
    let mut url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout=0");
    if let Some(offset) = offset {
        url.push_str(&format!("&offset={offset}"));
    }
    let response: TelegramUpdatesResponse = ureq::get(&url)
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
    let graph = knowledge_graph.graph_name(agent_id);
    let rendered = match crate::graph::query_rendered_context(
        crate::graph::DEFAULT_LOCAL_URL,
        &token,
        &graph,
        terms,
        2,
    ) {
        Ok(rendered) => rendered,
        Err(error) => format!("(knowledge graph unavailable: {error:#})"),
    };
    Some(GraphChannelContext { graph, rendered })
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
{graph_section}
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

fn maybe_remember_user_message(
    home: &MaturanaHome,
    agent_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let normalized = text.to_ascii_lowercase();
    if !normalized.contains("remember") {
        return Ok(());
    }
    let path = home.agent_dir(agent_id).join("memory/MEMORY.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(&path, "# Memory\n")?;
    }
    let entry = format!("\n- {}: {}\n", Utc::now().date_naive(), text.trim());
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;
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
        assert!(memory.contains("remember that I prefer short replies"));
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
