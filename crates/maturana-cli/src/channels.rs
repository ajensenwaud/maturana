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
        cancel_pending_inbound, claim_delivery, clear_progress, ensure_session, find_reply_outbound,
        insert_inbound, list_undelivered, mark_delivered, read_progress,
        request_cancel_in_progress, session_paths, unclaim_delivery, ProgressEvent, SessionPaths,
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
    time::{Duration, SystemTime},
};

use crate::session::{message_files, message_text, run_session_once, RunnerOptions};

const TELEGRAM_PAIR_CODE: &str = "telegram/pair-code";
const TELEGRAM_CHAT_ID: &str = "telegram/chat-id";
const MAX_RESPONSE_CHARS: usize = 3500;
/// The 1s background delivery thread only backstops a telegram reply once it is
/// older than this — comfortably past the inline streaming loop's own deadline
/// (`STREAM_TURN_TIMEOUT`), so the two never edit the live message concurrently.
const STREAM_BACKSTOP_AGE: Duration = Duration::from_secs(360);
/// How long the inline streaming loop animates + waits for a turn's reply before
/// handing the (undelivered) reply off to the background backstop.
const STREAM_TURN_TIMEOUT: Duration = Duration::from_secs(300);
// AGENTS.md is the agent's own contract + operational recipes (capabilities,
// peer delegation, honesty limits). A fully-loaded spec renders ~5KB; a 4KB cap
// truncated it mid-recipe (the A2A delegation tail + the "## Limits" block were
// lost), so the agent never saw how to read a peer's reply or its own caps. Keep
// the whole file — it is authored and bounded, not unbounded context like wiki
// or transcript. 8000 fits a maxed AGENTS.md with headroom.
const IDENTITY_CONTEXT_CHARS: usize = 8000;
const SOUL_CONTEXT_CHARS: usize = 4000;
const CONTRACT_CONTEXT_CHARS: usize = 5000;
const MEMORY_CONTEXT_CHARS: usize = 5000;
const AGENT_CONTEXT_CHARS: usize = 3000;
const TRANSCRIPT_CONTEXT_CHARS: usize = 8000;

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
    /// Show a channel's pairing + runner health for an agent.
    Status {
        /// Platform (telegram). Optional; telegram by default.
        #[arg(default_value = "telegram")]
        platform: String,
        #[arg(long = "agent-id", default_value = "default")]
        agent_id: String,
    },
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
    Discord(DiscordServe),
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
pub struct DiscordServe {
    #[arg(long)]
    pub agent_id: String,
    #[arg(long, default_value = "discord-main")]
    pub session_id: String,
    #[arg(long, default_value = "pipelock:discord/bot-token")]
    pub bot_token_source: String,
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
    wiki_query_terms: Vec<String>,
    wiki_term_sources: Vec<WikiTermSource>,
    /// GraphRAG context from the agent's knowledge graph, when enabled.
    graph_context: Option<GraphChannelContext>,
    /// Few-shot examples from past positively-rewarded turns (self-improvement).
    learned_examples: String,
    /// Whether this agent may build + run WebAssembly capabilities on the fly.
    self_forge: bool,
    /// Mid first-run onboarding interview → inject the "keep interviewing" directive.
    onboarding_active: bool,
    transcript: String,
    transcript_path: PathBuf,
}

#[derive(Debug, Clone)]
struct GraphChannelContext {
    graph: String,
    rendered: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WikiTermSource {
    term: String,
    sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextPolicySummary {
    strategy: String,
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
    /// Set when the user taps an inline-keyboard button (e.g. the model selector).
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

/// A tap on an inline-keyboard button. `data` carries our `action:value` payload
/// (e.g. `model:gpt-5`); `message` is the bot message the keyboard is attached to,
/// which gives us the chat id (for the pairing gate) and message id (to edit).
#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    document: Option<TelegramDocument>,
    #[serde(default)]
    photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    voice: Option<TelegramVoice>,
    #[serde(default)]
    audio: Option<TelegramAudio>,
    chat: TelegramChat,
}

/// A Telegram voice note (an OGG/Opus recording from the mic button). Carries no
/// `text`; we download and transcribe it (STT) host-side. Extra fields
/// (duration, mime_type, file_size) are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TelegramVoice {
    file_id: String,
}

/// A Telegram audio file (a music/audio attachment, as opposed to a voice note).
/// Also transcribed; the original file name, when present, is a better STT format
/// hint than the generic voice default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TelegramAudio {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
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

/// One size of a Telegram photo upload. Telegram sends an ascending array of
/// sizes (thumbnail → original); we OCR the largest. Extra fields (file_size,
/// width, height) are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
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
    /// (Re)run the first-run onboarding interview, routed back to this chat.
    Onboard {
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
    Photo {
        chat_id: i64,
        file_id: String,
        caption: Option<String>,
    },
    /// A voice note or audio file to transcribe (STT), then treat as a prompt.
    Voice {
        chat_id: i64,
        file_id: String,
        filename: String,
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
            ChannelServeSubcommand::Discord(config) => serve_discord(home, config),
            ChannelServeSubcommand::Agentmail(config) => serve_agentmail(home, config),
        },
        ChannelSubcommand::Status { platform: _, agent_id } => channel_status(home, &agent_id),
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

fn channel_status(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    println!("agent: {agent_id}");
    telegram_pair_status(home, agent_id)?;
    println!("telegram.presence: {}", channel_presence(home, agent_id));
    let state = read_telegram_state(home, agent_id).unwrap_or_default();
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
    // Publish the slash-command menu so `/` brings up the interactive picker.
    // Best-effort: a transient API hiccup here must not stop the channel.
    if let Err(error) = set_telegram_commands(&token) {
        eprintln!("telegram: could not set command menu: {error:#}");
    }
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
                    // Backstop only: never deliver a reply whose inline streaming
                    // loop may still be live (it owns the single live message).
                    // STREAM_BACKSTOP_AGE exceeds the streamer's whole deadline.
                    let _ = deliver_telegram_outbox(
                        &home,
                        &deliver_token,
                        &deliver_agent,
                        &deliver_session,
                        chat_id,
                        Some(STREAM_BACKSTOP_AGE),
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
    // Flush replies orphaned by a previous run BEFORE entering the poll loop. A
    // bridge restart kills any in-flight streaming loop, leaving its reply
    // written-but-undelivered with a now-stale "working…" marker — which would make
    // the backstop defer it for the full STREAM_BACKSTOP_AGE (minutes of "stuck").
    // On startup no streamer is live, so deliver everything pending immediately.
    if !config.once {
        if let Some(chat_id) = current_paired_telegram_chat_id(home, &config.agent_id) {
            let flushed =
                deliver_telegram_outbox(home, &token, &config.agent_id, &config.session_id, chat_id, None)
                    .unwrap_or(0);
            if flushed > 0 {
                println!("telegram: flushed {flushed} reply(ies) orphaned by restart");
            }
        }
    }
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
                    None,
                )?;
            }
            break;
        }
        write_telegram_heartbeat(home, &config.agent_id, "idle", None)?;
        // No client-side sleep: the long-poll getUpdates above paces the loop.
    }
    Ok(())
}

pub(crate) fn current_paired_telegram_chat_id(home: &MaturanaHome, agent_id: &str) -> Option<i64> {
    let vault = PipelockVault::new(home.pipelock_dir());
    vault
        .get(&telegram_chat_id_key(agent_id))
        .or_else(|_| vault.get(TELEGRAM_CHAT_ID))
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
}

/// Whether this agent's Telegram bridge is currently alive, so an outbound row
/// written to its outbox will actually be sent (not parked forever). The bridge
/// rewrites `heartbeat.json` on every poll loop; a long-poll can be up to ~50s,
/// so a window of 120s comfortably covers a healthy bridge while still rejecting
/// an agent whose poller died. Used by card delivery to pick the agent that
/// genuinely serves the channel rather than any agent that was once paired.
pub(crate) fn telegram_bridge_live(home: &MaturanaHome, agent_id: &str) -> bool {
    let hb = telegram_heartbeat_path(home, agent_id);
    let Ok(meta) = std::fs::metadata(&hb) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age <= Duration::from_secs(120))
        .unwrap_or(false)
}

/// Where this agent's Discord bridge would push an unsolicited message: the last
/// channel it received a message in. Discord (unlike Telegram) has no static
/// paired destination — the bot only learns a channel once someone talks to it
/// there — so the running bridge persists that channel id here for host-side
/// delivery to reuse. `None` until the bot has seen at least one message.
pub(crate) fn current_discord_delivery_channel(home: &MaturanaHome, agent_id: &str) -> Option<String> {
    let path = discord_last_channel_path(home, agent_id);
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn discord_last_channel_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/discord/last-channel")
}

/// Persist the Discord channel the bot last heard from so host-side delivery
/// (e.g. a finished board card) can reach the same conversation. Best-effort.
fn remember_discord_channel(home: &MaturanaHome, agent_id: &str, channel_id: &str) {
    let path = discord_last_channel_path(home, agent_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, channel_id);
}

fn handle_telegram_update(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
    update: &TelegramUpdate,
) -> anyhow::Result<()> {
    // Inline-keyboard button taps arrive as callback queries, not messages.
    if let Some(callback) = &update.callback_query {
        return handle_telegram_callback(home, token, config, paired_chat_id, callback);
    }
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
                enqueue_onboarding(home, &config.agent_id, &config.session_id, chat_id)?;
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
        InboundAction::Onboard { chat_id } => {
            // Enqueue the onboarding turn tagged with THIS chat so the agent's
            // greeting routes back here, then tell the user it's starting.
            enqueue_onboarding(home, &config.agent_id, &config.session_id, chat_id)?;
            let _ = mark_onboarded(home, &config.agent_id);
            send_telegram(
                token,
                &chat_id.to_string(),
                "Starting onboarding — one moment while I introduce myself.",
                reply_to_message_id,
            )?;
            send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
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
                &status_text(home, &config.agent_id, &config.session_id, "telegram"),
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
            // Commands with a natural set of choices render as a tappable
            // selector (model picker, TTS provider, session state). Bare form
            // only — `/model gpt-5` still sets directly via the text path.
            if let Some((prompt, buttons, columns)) =
                command_selector(home, config, &name, &args)
            {
                send_telegram_keyboard(
                    token,
                    &chat_id.to_string(),
                    &prompt,
                    &buttons,
                    columns,
                    reply_to_message_id,
                )?;
                return Ok(());
            }
            let reply =
                handle_channel_command(home, &config.agent_id, &config.session_id, chat_id, "telegram", &chat_id.to_string(), &name, &args)
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
            // Record the sub-task (so /subagents can list it), then run it as a
            // real turn in the live session: the worker executes it and the reply
            // streams back here. (Previously this queued the task into a
            // subagent-<id> session that nothing polled — a silent no-op.)
            let _ = create_subagent(home, &config.agent_id, &name, mode, &prompt);
            run_channel_prompt(
                home,
                token,
                config,
                chat_id,
                &frame_subtask(&name, &prompt),
                reply_to_message_id,
            )
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
        InboundAction::Photo {
            chat_id,
            file_id,
            caption,
        } => handle_telegram_photo(
            home,
            token,
            config,
            chat_id,
            &file_id,
            caption.as_deref(),
            reply_to_message_id,
        ),
        InboundAction::Voice {
            chat_id,
            file_id,
            filename,
        } => handle_telegram_voice(
            home,
            token,
            config,
            chat_id,
            &file_id,
            &filename,
            reply_to_message_id,
        ),
        InboundAction::Prompt { chat_id, text } => {
            run_channel_prompt(home, token, config, chat_id, &text, reply_to_message_id)
        }
    }
}

/// Run one channel turn end-to-end: record the user message, enqueue it as an
/// inbound for the live (polled) session, stream the agent's reply back to
/// Telegram, and deliver. Shared by ordinary prompts and by /emerge + /spawn, so
/// a queued sub-task actually runs and replies instead of vanishing into a
/// session nothing polls.
fn run_channel_prompt(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    audit_channel_event(
        home,
        &config.agent_id,
        "channel.telegram.inbound",
        "received telegram prompt",
    )?;
    println!("telegram inbound prompt accepted");
    // Enqueue through the shared front door (records the turn, builds context,
    // attaches model/reasoning). Telegram keys its transcript by the raw chat id
    // and carries its reply-to as the channel-specific extra.
    let inbound_id = enqueue_turn(
        home,
        &config.agent_id,
        &config.session_id,
        "telegram",
        &chat_id.to_string(),
        chat_id,
        reply_to_message_id.map(|id| id.to_string()).as_deref(),
        text,
        serde_json::json!({ "telegram_reply_to": reply_to_message_id }),
    )?;
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    if let Some(provider) = &config.run_once_provider {
        let options = RunnerOptions {
            provider: provider.to_string(),
        };
        run_session_once(&paths, &options, 20)?;
    }
    // Live progress: a status message is created lazily as the agent works
    // (tool calls + streamed text), then finalized into the reply — no
    // "working…" placeholder. The loop marks the final outbound delivered
    // so the delivery thread doesn't re-send; on timeout it leaves the late
    // final to that thread.
    // Best-effort: a streamer error must not stop the trailing delivery
    // (which is what actually gets the reply out for this turn).
    if let Err(error) = stream_turn_to_telegram(
        home,
        token,
        config,
        chat_id,
        &inbound_id,
        reply_to_message_id,
        &paths,
        STREAM_TURN_TIMEOUT,
    ) {
        eprintln!("telegram streamer error (delivering anyway): {error:#}");
    }
    // The streaming loop already owned delivery; this trailing call is a
    // same-thread fallback for the timed-out / errored case, so deliver now.
    deliver_telegram_outbox(home, token, &config.agent_id, &config.session_id, chat_id, None)?;
    Ok(())
}

/// Frame a spawned sub-task as a focused instruction for the agent's next turn.
fn frame_subtask(name: &str, task: &str) -> String {
    format!(
        "You've been handed a focused sub-task labelled `{name}`. Work on it now \
         and report back with the result when you're done.\n\nTask: {task}"
    )
}

/// A stable per-channel "chat key" for the console TUI, so commands that key off
/// a chat id (transcript reset) have a consistent target.
pub(crate) fn console_chat_key() -> i64 {
    stable_chat_key("console:tui")
}

/// Persist one console-TUI turn into the same Markdown transcript the Telegram
/// channel writes (keyed by `console_chat_key()`), so the conversation survives
/// an agent switch and feeds the next turn's context exactly like Telegram.
pub(crate) fn record_console_turn(
    home: &MaturanaHome,
    agent_id: &str,
    role: &str,
    text: &str,
) -> anyhow::Result<()> {
    append_channel_turn(home, agent_id, console_chat_key(), role, text)
}

/// Wipe the console TUI's persisted transcript so a `/clear` (or a new session)
/// stays cleared across quit/reopen — otherwise `read_console_transcript` would
/// restore the old dialogue the next time the TUI opens. Clearing the file also
/// drops it from the next turn's context, which is what `/clear` should mean.
/// Missing file is success (already clear).
pub(crate) fn clear_console_transcript(home: &MaturanaHome, agent_id: &str) -> std::io::Result<()> {
    let path = channel_transcript_path(home, agent_id, console_chat_key());
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read the console transcript back as ordered (role, body) pairs to repopulate
/// the TUI view on agent switch. Parses the `## <rfc3339> <role>` section format
/// `append_channel_turn` writes; non-`## ` lines (reset banners, notices) are
/// skipped because they carry no role header.
pub(crate) fn read_console_transcript(home: &MaturanaHome, agent_id: &str) -> Vec<(String, String)> {
    let path = channel_transcript_path(home, agent_id, console_chat_key());
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut role: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    let flush = |role: &Option<String>, body: &[String], out: &mut Vec<(String, String)>| {
        if let Some(r) = role {
            let t = body.join("\n").trim().to_string();
            if !t.is_empty() {
                out.push((r.clone(), t));
            }
        }
    };
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(&role, &body, &mut out);
            body.clear();
            // Header is `<rfc3339-ts> <role>`; the role is the last whitespace token.
            role = rest.split_whitespace().last().map(|s| s.to_string());
        } else if role.is_some() {
            body.push(line.to_string());
        }
    }
    flush(&role, &body, &mut out);
    out
}

/// The full slash-command catalog the console TUI advertises (autocomplete +
/// /help) — the Telegram command menu plus the TUI-local commands.
pub(crate) fn console_command_catalog() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = vec![
        ("/help", "show commands and keybindings"),
        ("/clear", "clear the transcript view"),
        ("/quit", "exit the chat"),
    ];
    for (_, cmds) in COMMAND_GROUPS {
        for (name, desc) in *cmds {
            if !out.iter().any(|(n, _)| n == name) {
                out.push((name, desc));
            }
        }
    }
    out.push(("/good", "rate the last reply"));
    out.push(("/bad", "rate the last reply"));
    out
}

/// What a console slash command resolves to; the TUI renders/acts on this.
/// One pickable option in a console Select overlay. `apply` runs the SAME save
/// path the Telegram inline-keyboard callback uses and returns a confirmation.
pub(crate) struct SelectOption {
    pub label: String,
    pub apply: Box<dyn FnOnce() -> String + Send>,
}

pub(crate) enum ConsoleCommand {
    /// Show this text in the transcript (no agent turn).
    Reply(String),
    /// Send this text to the agent as a normal turn.
    Prompt(String),
    /// Clear the on-screen transcript.
    Clear,
    /// Start a fresh session view (context reset).
    NewSession,
    /// Exit the TUI.
    Quit,
    /// Open a modal picker (parity with Telegram's inline keyboard); on selection
    /// the chosen option's `apply` persists the choice.
    Select {
        title: String,
        options: Vec<SelectOption>,
    },
}

/// Dispatch a slash command typed in the console TUI, reusing the same handlers
/// the Telegram channel uses so the two stay at parity. `raw` includes the
/// leading '/'.
pub(crate) fn run_console_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    raw: &str,
) -> ConsoleCommand {
    dispatch_slash_command(
        home,
        agent_id,
        session_id,
        console_chat_key(),
        "console",
        &console_chat_key().to_string(),
        raw,
    )
}

/// Channel-agnostic slash-command dispatcher. The console TUI, Discord (and any
/// text channel) share this so the command set never drifts: every command not
/// handled locally below routes to `handle_channel_command` — the SAME leaf
/// handler Telegram uses — so adding a command there lights it up everywhere.
/// `chat_id` is the surface's stable per-chat key; `channel` is the source label
/// (used for the feedback-trajectory partition and per-chat settings).
pub(crate) fn dispatch_slash_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    raw: &str,
) -> ConsoleCommand {
    let trimmed = raw.trim();
    let (head, args) = match trimmed.split_once(char::is_whitespace) {
        Some((h, a)) => (h, a.trim()),
        None => (trimmed, ""),
    };
    let name = head
        .trim_start_matches('/')
        .replace('_', "-")
        .to_ascii_lowercase();

    match name.as_str() {
        "help" | "start" => ConsoleCommand::Reply(format!(
            "{}\n\nKeys: Enter send · Alt+Enter newline · PgUp/PgDn scroll · / menu · \
             Esc interrupts a reply · Ctrl+C quits.",
            help_text()
        )),
        "clear" => ConsoleCommand::Clear,
        "quit" | "exit" => ConsoleCommand::Quit,
        // Both reset the conversation view; /new and /reset behave the same here.
        "new" | "reset" => {
            let _ = reset_channel_context(home, agent_id, chat_id);
            ConsoleCommand::NewSession
        }
        "status" => ConsoleCommand::Reply(status_text(home, agent_id, session_id, channel)),
        "good" | "bad" => {
            let value = if name == "good" {
                signals::THUMBS_UP
            } else {
                signals::THUMBS_DOWN
            };
            let reply = match TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))
                .and_then(|store| store.reward_latest(agent_id, session_id, channel, value, None))
            {
                Ok(Some(_)) if value > 0.0 => "Logged a 👍 on the last reply.".to_string(),
                Ok(Some(_)) => "Logged a 👎 on the last reply.".to_string(),
                Ok(None) => "No recent agent turn to rate yet.".to_string(),
                Err(error) => format!("Could not record feedback: {error:#}"),
            };
            ConsoleCommand::Reply(reply)
        }
        // /skill <name> [args] runs the skill via a normal turn (matches Telegram).
        "skill" if !args.is_empty() => {
            let (skill, rest) = match args.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args, ""),
            };
            ConsoleCommand::Prompt(format!("Use the `{skill}` skill. {rest}").trim().to_string())
        }
        // /emerge <task> spawns an ephemeral sub-agent on the task.
        "emerge" if !args.is_empty() => {
            // Record the sub-task, then run it as a real turn.
            let sub = slugify_channel_id(args);
            let _ = create_subagent(home, agent_id, &sub, SpawnMode::Ephemeral, args);
            ConsoleCommand::Prompt(frame_subtask(&sub, args))
        }
        // /onboard runs the onboarding interview as a real turn. Returning a
        // Prompt lets the surface enqueue it through its own correctly-routed path
        // (real channel + chat id), so the agent's greeting comes back here —
        // Telegram has its own InboundAction::Onboard for the same reason.
        "onboard" => {
            let _ = mark_onboarded(home, agent_id);
            set_onboarding_active(home, agent_id);
            ConsoleCommand::Prompt(onboarding_prompt())
        }
        // Bare selector commands open a modal picker (parity with Telegram's
        // inline keyboard). `/model gpt-5` (non-empty args) falls through to the
        // text handler below and sets directly.
        "model" | "models" | "reasoning" | "tts-provider" | "session" if args.is_empty() => {
            match command_selector_buttons(home, agent_id, &name) {
                Some((title, buttons, _cols)) => {
                    let options = buttons
                        .into_iter()
                        .map(|(label, data)| {
                            let home = MaturanaHome::new(home.root().to_path_buf());
                            let agent = agent_id.to_string();
                            SelectOption {
                                label,
                                apply: Box::new(move || {
                                    apply_channel_selection(&home, &agent, &data)
                                }),
                            }
                        })
                        .collect();
                    ConsoleCommand::Select { title, options }
                }
                None => match handle_channel_command(home, agent_id, session_id, chat_id, channel, platform_id, &name, args)
                {
                    Ok(reply) => ConsoleCommand::Reply(reply),
                    Err(error) => ConsoleCommand::Reply(format!("Command failed: {error:#}")),
                },
            }
        }
        // Every other slash command routes to the shared channel handler — the
        // SAME one Telegram uses — so no surface lags the command set. A new
        // command in `handle_channel_command` works here automatically;
        // unrecognized names get its own "Unknown command" reply.
        _ => match handle_channel_command(home, agent_id, session_id, chat_id, channel, platform_id, &name, args) {
            Ok(reply) => ConsoleCommand::Reply(reply),
            Err(error) => ConsoleCommand::Reply(format!("Command failed: {error:#}")),
        },
    }
}

/// Apply a [`dispatch_slash_command`] result for the web cockpit. The cockpit has
/// no interactive TUI loop, so each [`ConsoleCommand`] maps to a concrete effect:
/// a local `Reply`/`Select` becomes a `web` outbound the cockpit's poller streams
/// straight back to the browser; a `Prompt` (skill/emerge/onboard) is enqueued as
/// a real turn so the agent actually answers. Returns the id the cockpit treats as
/// the "enqueued message id" (an outbound id for local replies, the inbound id for
/// a real turn). Without this, `/model …` from the web chat would reach the agent
/// as a literal user message instead of being handled like every other channel.
pub(crate) fn apply_web_console_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    cmd: ConsoleCommand,
) -> anyhow::Result<String> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let reply = |text: String| -> anyhow::Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        maturana_core::session_db::write_outbound(&paths, None, "chat", "web", "web", None, &content)
    };
    match cmd {
        ConsoleCommand::Reply(text) => reply(text),
        ConsoleCommand::Prompt(prompt) => enqueue_turn(
            home,
            agent_id,
            session_id,
            "web",
            "web",
            chat_id,
            None,
            &prompt,
            serde_json::json!({}),
        ),
        ConsoleCommand::Clear | ConsoleCommand::NewSession => {
            let _ = reset_channel_context(home, agent_id, chat_id);
            reply("Conversation reset — starting fresh.".to_string())
        }
        ConsoleCommand::Quit => {
            reply("Close the browser tab to end the session.".to_string())
        }
        // No modal picker over the WS yet — surface the choices as text so the
        // operator can re-issue the command with a value (e.g. `/model <name>`).
        ConsoleCommand::Select { title, options } => {
            let labels: Vec<String> = options.into_iter().map(|o| o.label).collect();
            reply(format!(
                "{title}\nOptions: {}\n(Re-send the command with a value, e.g. `/model <name>`.)",
                labels.join(", ")
            ))
        }
    }
}

/// Per-channel send behavior for the shared [`deliver_outbox`] loop. The loop owns
/// claiming, dropping unparseable rows, the silence-sentinel filter, transcript
/// recording, mark-delivered, audit, and release-on-failure (a failed send is
/// retried, never wedged). Each channel's sink supplies only HOW to send plus any
/// extras (Telegram's live-message edit + TTS).
trait OutboundSink {
    /// Send the final reply; return the platform message id (if any). `inbound_id`
    /// is the originating inbound row (Telegram looks up its live "working…"
    /// message by it); `reply_to` is the outbound row's thread id.
    fn send(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>>;
    /// Deliver a reply that carries one or more host-side files. The default, for
    /// channels with no native upload, sends the text plus the file NAMES so the
    /// user at least sees what was produced; channels that can upload (Telegram)
    /// override this to send the bytes. `files` are absolute host paths.
    fn send_files(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                Path::new(f)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| f.clone())
            })
            .collect();
        let combined = if text.trim().is_empty() {
            format!("📎 Files produced: {}", names.join(", "))
        } else {
            format!("{text}\n📎 Files produced: {}", names.join(", "))
        };
        self.send(inbound_id, &combined, reply_to)
    }
    /// The reply was the silence sentinel — clean up any live placeholder. No-op by default.
    fn on_silence(&mut self, _inbound_id: Option<&str>) {}
    /// After a successful send (e.g. speak it via TTS). No-op by default.
    fn after_delivered(&mut self, _text: &str, _reply_to: Option<&str>) {}
    /// Is an inline streamer still potentially animating this turn's live message?
    /// The backstop only defers young replies when true; replies with no streamer
    /// (e.g. onboarding) deliver immediately. Default: false (no streaming).
    fn has_pending_stream(&self, _inbound_id: Option<&str>) -> bool {
        false
    }
}

/// The ONE outbound delivery loop every async channel bridge shares — Telegram and
/// the generic Discord/Slack/AgentMail path. It claims each undelivered row for
/// `channel`+`platform_id`, drops unparseable ones, filters the silence sentinel,
/// sends via `sink`, records the assistant turn under `chat_key`, marks delivered,
/// and audits — and on a send error RELEASES the claim so a later pass retries
/// instead of wedging a claimed-but-undelivered row. `min_age` skips replies
/// younger than it (Telegram's concurrent-backstop gate); `None` delivers now.
fn deliver_outbox(
    home: &MaturanaHome,
    agent_id: &str,
    paths: &SessionPaths,
    channel: &str,
    platform_id: &str,
    chat_key: i64,
    min_age: Option<Duration>,
    sink: &mut dyn OutboundSink,
) -> anyhow::Result<usize> {
    let mut delivered = 0;
    for message in list_undelivered(paths)? {
        if message.channel != channel || message.platform_id != platform_id {
            continue;
        }
        let inbound_id = message.in_reply_to.as_deref();
        // Backstop gate: only DEFER a young reply while its inline streamer might
        // still be animating the live message (a status marker exists for the
        // inbound). A reply with no marker — e.g. the onboarding greeting enqueued
        // at pairing, which never had a streamer — has nothing to race, so deliver
        // it immediately instead of waiting out the full STREAM_BACKSTOP_AGE (the
        // bug where "Paired!" was followed by minutes of silence).
        if let Some(min_age) = min_age {
            let too_young = (Utc::now() - message.created_at)
                .to_std()
                .map(|age| age < min_age)
                .unwrap_or(false);
            if too_young && sink.has_pending_stream(inbound_id) {
                continue;
            }
        }
        // Atomic claim so concurrent delivery paths can't double-send a reply.
        if !claim_delivery(paths, &message.id)? {
            continue;
        }
        // Drop an unparseable row rather than spinning on it forever (deterministic).
        let response = match message_text(&message.content) {
            Ok(text) => truncate_for_telegram(&finalize_onboarding_reply(home, agent_id, &text)),
            Err(error) => {
                eprintln!("{channel}: dropping unparseable outbound {}: {error:#}", message.id);
                let _ = mark_delivered(paths, &message.id, None);
                continue;
            }
        };
        let reply_to = message.thread_id.as_deref();
        // A self-check that decided there's nothing worth saying emits the silence
        // sentinel; never surface it. Let the sink clean up any live placeholder.
        if response.trim() == crate::proactive::SILENCE_SENTINEL {
            sink.on_silence(inbound_id);
            let _ = mark_delivered(paths, &message.id, None);
            continue;
        }
        // An outbound may carry host-side files (e.g. a `/loop` deliverable); the
        // sink uploads them where the channel supports it, else names them.
        let files = message_files(&message.content);
        let send_result = if files.is_empty() {
            sink.send(inbound_id, &response, reply_to)
        } else {
            sink.send_files(inbound_id, &response, &files, reply_to)
        };
        match send_result {
            Ok(platform_message_id) => {
                let _ = mark_delivered(paths, &message.id, platform_message_id.as_deref());
                let _ = append_channel_turn(home, agent_id, chat_key, "assistant", &response);
                sink.after_delivered(&response, reply_to);
                let _ = audit_channel_event(
                    home,
                    agent_id,
                    &format!("channel.{channel}.outbound"),
                    "sent channel response",
                );
                delivered += 1;
            }
            Err(error) => {
                // Release the claim so a later pass retries, not drop the reply.
                eprintln!("{channel} delivery failed, will retry: {error:#}");
                unclaim_delivery(paths, &message.id)?;
            }
        }
    }
    Ok(delivered)
}

/// Telegram delivery sink: turn the live streaming "working…" message into the
/// final reply by editing it in place (one clean message, never a duplicate), then
/// speak it if TTS is on. The silence path deletes the live placeholder.
struct TelegramSink<'a> {
    token: &'a str,
    chat_id: i64,
    paths: &'a SessionPaths,
    home: &'a MaturanaHome,
    agent_id: &'a str,
}

impl OutboundSink for TelegramSink<'_> {
    fn send(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        let reply_to = reply_to.and_then(|value| value.parse::<i64>().ok());
        let platform_message_id =
            finalize_reply(self.token, self.chat_id, live_id, text, reply_to)?;
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
        Ok(platform_message_id.map(|id| id.to_string()))
    }

    fn send_files(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let reply_to_i = reply_to.and_then(|value| value.parse::<i64>().ok());
        // If a live "working…" message exists, turn it into the text now and send
        // the files as their own messages; otherwise the text rides as the first
        // document's caption.
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        let text_already_sent = if let Some(id) = live_id {
            let _ = finalize_reply(self.token, self.chat_id, Some(id), text, reply_to_i)?;
            true
        } else {
            false
        };
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
        let mut last_id: Option<String> = None;
        let mut uploaded = 0usize;
        for (i, path) in files.iter().enumerate() {
            let caption = if i == 0 && !text_already_sent { Some(text) } else { None };
            match send_telegram_document(self.token, self.chat_id, Path::new(path), caption, reply_to_i) {
                Ok(id) => {
                    last_id = id.map(|i| i.to_string());
                    uploaded += 1;
                }
                Err(error) => eprintln!("telegram: sendDocument failed for {path}: {error:#}"),
            }
        }
        // Nothing uploaded and the text never went out → send it as text so the
        // user is never left silent (e.g. all files were missing or oversized).
        if uploaded == 0 && !text_already_sent {
            let names: Vec<String> = files
                .iter()
                .filter_map(|f| Path::new(f).file_name().map(|n| n.to_string_lossy().to_string()))
                .collect();
            let msg = format!("{text}\n(couldn't attach: {})", names.join(", "));
            return Ok(finalize_reply(self.token, self.chat_id, None, &msg, reply_to_i)?
                .map(|id| id.to_string()));
        }
        Ok(last_id)
    }

    fn on_silence(&mut self, inbound_id: Option<&str>) {
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        if let Some(id) = live_id {
            let _ = delete_telegram_message(self.token, self.chat_id, id);
        }
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
    }

    fn after_delivered(&mut self, text: &str, reply_to: Option<&str>) {
        let reply_to = reply_to.and_then(|value| value.parse::<i64>().ok());
        maybe_send_tts(self.home, self.token, self.agent_id, self.chat_id, text, reply_to);
    }

    fn has_pending_stream(&self, inbound_id: Option<&str>) -> bool {
        // An active streamer owns delivery for its turn; the backstop must not race
        // it. The `.tgactive` lock is set at the streamer's ENTRY (before any send),
        // so it covers the whole turn — including the early window before the live
        // "working…" message exists. The message-id marker is a secondary signal for
        // a streamer that set it but whose lock was somehow lost.
        inbound_id
            .map(|inbound| {
                telegram_active_exists(self.paths, inbound)
                    || peek_telegram_status(self.paths, inbound).is_some()
            })
            .unwrap_or(false)
    }
}

/// Deliver pending telegram replies. `min_age` gates how old an outbound must be
/// before this path will deliver it: the inline path (the streaming loop already
/// returned) passes `None` to deliver immediately, while the concurrent 1s
/// background thread passes a value LARGER than the streamer's whole deadline, so
/// it only ever acts as a backstop for a turn whose streamer died — it never edits
/// the live message while a streamer is still animating it (which would race the
/// answer back to a spinner). Thin wrapper over the shared [`deliver_outbox`].
fn deliver_telegram_outbox(
    home: &MaturanaHome,
    token: &str,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    min_age: Option<Duration>,
) -> anyhow::Result<usize> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut sink = TelegramSink {
        token,
        chat_id,
        paths: &paths,
        home,
        agent_id,
    };
    let delivered = deliver_outbox(
        home,
        agent_id,
        &paths,
        "telegram",
        &chat_id.to_string(),
        chat_id,
        min_age,
        &mut sink,
    )?;
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

/// A photo upload: download the largest size, OCR it with tesseract, and ingest
/// the extracted text into the agent's knowledge graph (so the agent can answer
/// from it later, exactly like an uploaded document).
fn handle_telegram_photo(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    file_id: &str,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    fs::create_dir_all(&inbox)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let image_dest = inbox.join(format!("{stamp}-photo.jpg"));
    if let Err(error) = download_telegram_document(token, file_id, &image_dest) {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!("I couldn't download that image: {error:#}"),
            reply_to_message_id,
        )?;
        return Ok(());
    }

    // VISION (primary): push the image into the guest workspace and run it as a
    // normal prompt turn that points the harness at the file. A vision-capable
    // harness (Claude Code / codex / opencode) opens and *sees* the image, then
    // replies through the same memory + reply + TTS pipeline as a typed message.
    match crate::deliver_image_to_guest(home, &config.agent_id, &image_dest) {
        Ok(guest_path) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.image",
                &format!("delivered image to guest ({guest_path}); running vision turn"),
            )?;
            let prompt = crate::vision_prompt_text(caption, &guest_path);
            return run_channel_prompt(home, token, config, chat_id, &prompt, reply_to_message_id);
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.image_fallback",
                &format!("guest image delivery failed ({error:#}); falling back to OCR"),
            )?;
        }
    }

    // FALLBACK: the guest is unreachable — OCR the image into the knowledge graph
    // so it is at least retained, if the graph is enabled.
    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph_token = maturana_core::worker::read_graph_token(home.root());
    let (graph_token, graph_name) = match (graph_token, knowledge_graph.enabled) {
        (Some(value), true) => (value, crate::graph::agent_graph_name(&config.agent_id)),
        _ => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I received your image but couldn't reach my VM to view it, and my knowledge graph is off, so I can't store it either. Try again, or enable `knowledge_graph`.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };

    let text = match ocr_image_text(&image_dest) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I read that image but couldn't find any text in it (OCR returned nothing).",
                reply_to_message_id,
            )?;
            return Ok(());
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo_error",
                &format!("OCR failed: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I couldn't OCR that image: {error:#}"),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };

    // Ingest the OCR'd text as a markdown note the graph can chunk + retrieve.
    let text_dest = inbox.join(format!("{stamp}-photo-ocr.md"));
    let heading = caption
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| format!("# {c}\n\n"))
        .unwrap_or_default();
    fs::write(&text_dest, format!("{heading}{}", text.trim()))?;
    let chars = text.trim().chars().count();
    match crate::graph::ingest_file_into_service(
        crate::graph::DEFAULT_LOCAL_URL,
        &graph_token,
        &graph_name,
        &text_dest,
        1800,
    ) {
        Ok(chunks) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo",
                &format!("OCR'd image ({chars} chars) into graph '{graph_name}' ({chunks} chunks)"),
            )?;
            let note = match caption {
                Some(c) if !c.trim().is_empty() => format!("[uploaded image, OCR'd] {}", c.trim()),
                _ => "[uploaded image, OCR'd]".to_string(),
            };
            append_channel_turn(home, &config.agent_id, chat_id, "user", &note)?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "Read the text from your image ({chars} characters) and added it to my knowledge graph `{graph_name}` ({chunks} chunks). Ask me about it any time."
                ),
                reply_to_message_id,
            )?;
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo_error",
                &format!("failed to ingest OCR text: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I read the image but couldn't store the text: {error:#}"),
                reply_to_message_id,
            )?;
        }
    }
    Ok(())
}

/// A voice note or audio file: download it, transcribe it host-side (STT), echo
/// the transcript so the user can confirm it was heard correctly, then run the
/// transcript through the SAME channel-prompt pipeline a typed message uses (so
/// memory, graph, reply, and read-aloud all apply). The audio and the API key
/// stay host-side; neither reaches the guest.
fn handle_telegram_voice(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    file_id: &str,
    filename: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let audio = match download_telegram_file_bytes(token, file_id, MAX_TELEGRAM_DOCUMENT_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I couldn't download that voice message: {error:#}"),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    let settings = load_channel_settings(home, &config.agent_id);
    let provider = stt_provider(home, settings.tts_provider.as_deref());
    let transcript = match transcribe_speech(home, &provider, &audio, filename) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        Ok(_) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I heard a voice message but couldn't make out any words. Try again, or send text.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.voice_error",
                &format!("STT failed via {provider}: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "I couldn't transcribe that voice message (provider: {provider}): {error:#}\n\nSet a transcription key in pipelock (e.g. `pipelock:elevenlabs/api-key`)."
                ),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    audit_channel_event(
        home,
        &config.agent_id,
        "channel.telegram.voice",
        &format!("transcribed {} chars via {provider}", transcript.chars().count()),
    )?;
    // Echo the transcript so the user can see what was understood before the reply.
    send_telegram(
        token,
        &chat_id.to_string(),
        &format!("🎙️ \"{}\"", truncate_inline(&transcript, 400)),
        reply_to_message_id,
    )?;
    // Run it exactly like a typed prompt: same memory/graph/reply/TTS pipeline.
    run_channel_prompt(home, token, config, chat_id, &transcript, reply_to_message_id)
}

/// OCR an image to text by shelling out to the `tesseract` CLI — no API key, runs
/// offline host-side. Returns the extracted text; errors clearly if tesseract is
/// not installed.
fn ocr_image_text(image_path: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("tesseract")
        .arg(image_path)
        .arg("stdout")
        .output()
        .map_err(|error| {
            anyhow::anyhow!(
                "OCR needs the `tesseract` binary on the host (install it with \
                 `sudo apt install -y tesseract-ocr`): {error}"
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "tesseract failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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

/// Download a Telegram file (by `file_id`) into memory rather than to disk —
/// used for voice/audio, which we hand straight to the STT provider. Caps the
/// read at `max_bytes` so a hostile `getFile` can't exhaust memory.
fn download_telegram_file_bytes(
    token: &str,
    file_id: &str,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
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
    std::io::Read::take(reader, max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("file exceeds {max_bytes} bytes");
    }
    Ok(bytes)
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
pub(crate) fn agent_knowledge_graph(
    home: &MaturanaHome,
    agent_id: &str,
) -> maturana_core::spec::KnowledgeGraph {
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
    // Photo uploads (compressed images) arrive as an ascending size array, not a
    // document, and carry no `text`. OCR the largest size into the graph. The
    // pairing gate still applies — only the paired chat can feed the agent.
    if let Some(largest) = message.photo.as_ref().and_then(|sizes| sizes.last()) {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Photo {
            chat_id,
            file_id: largest.file_id.clone(),
            caption: message.caption.clone(),
        };
    }
    // Voice notes and audio files carry no `text`; transcribe them (STT) before
    // the text path. The pairing gate still applies — only the paired chat can
    // feed the agent.
    if let Some((file_id, filename)) = message
        .voice
        .as_ref()
        .map(|v| (v.file_id.clone(), "voice.ogg".to_string()))
        .or_else(|| {
            message.audio.as_ref().map(|a| {
                (
                    a.file_id.clone(),
                    a.file_name.clone().unwrap_or_else(|| "audio.ogg".to_string()),
                )
            })
        })
    {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Voice {
            chat_id,
            file_id,
            filename,
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
    // The Telegram command menu sends hyphenated commands as underscores
    // (`/graph_query`), since Telegram command names can't contain hyphens. Map
    // them back to our canonical hyphenated form before matching.
    let cmd = if cmd.starts_with('/') {
        cmd.replace('_', "-")
    } else {
        cmd
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
        // /onboard runs the onboarding interview as a real turn, so it needs the
        // chat's own routing (channel + platform_id) for the agent's greeting to
        // come back. The generic Command path returns a text reply only.
        "/onboard" => InboundAction::Onboard { chat_id },
        "/commands" | "/tools" | "/models" | "/model" | "/reasoning" | "/reset" | "/stop" | "/compact"
        | "/session" | "/subagents" | "/graph-query" | "/graph-insert" | "/tts"
        | "/tts-provider" | "/emerge" | "/skill" | "/loop" => InboundAction::Command {
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
    /// Reasoning effort for reasoning-capable harnesses (codex/gpt-5):
    /// low|medium|high. None => the worker's fast default (low).
    #[serde(default)]
    reasoning: Option<String>,
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

/// Reasoning levels offered by `/reasoning` (codex/gpt-5). `low` is snappy;
/// `high` reasons deepest. Validated against this list before storing.
/// `minimal` is intentionally excluded: the codex agent enables the
/// `web_search`/`image_gen` tools, and the API rejects `reasoning.effort
/// minimal` (HTTP 400) whenever those tools are present.
const REASONING_LEVELS: &[&str] = &["low", "medium", "high"];

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
            ("/reasoning", "codex reasoning effort (low|medium|high)"),
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
            ("/loop", "run a multi-agent loop on a goal (/loop <goal>); posts progress here"),
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

fn status_text(home: &MaturanaHome, agent_id: &str, session_id: &str, channel: &str) -> String {
    let settings = load_channel_settings(home, agent_id);
    let harness = harness_label(home, agent_id);
    let model = settings.model.unwrap_or_else(|| "(harness default)".to_string());
    let reasoning = settings.reasoning.unwrap_or_else(|| "low (default)".to_string());
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "Status\n  agent: {}\n  channel: {} (session {})\n  presence: {}\n  harness: {}\n  model: {}\n  reasoning: {}\n  OS: {}\n  time: {}\n  idle: {}",
        agent_id,
        channel,
        session_id,
        channel_presence(home, agent_id),
        harness,
        model,
        reasoning,
        std::env::consts::OS,
        now,
        if settings.idle { "on" } else { "off" },
    )
}

fn tools_text(home: &MaturanaHome, agent_id: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    // The agent's real tools live in its spec — MCP servers + opt-in capabilities
    // (the same set render_guest_agents writes into AGENTS.md). The WASM registry
    // below is separate (forged/installed tools) and is usually empty.
    if let Ok(spec) =
        AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md"))
    {
        if !spec.mcp_servers.is_empty() {
            let names = spec
                .mcp_servers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            sections.push(format!("MCP servers: {names}"));
        }
        let egress = &spec.network.egress_allowlist;
        let allows = |needle: &str| egress.iter().any(|h| h.contains(needle));
        let mut caps: Vec<&str> = Vec::new();
        if spec.network.egress_allow_all {
            caps.push("open web (allow-all egress)");
        }
        if allows("brave") || allows("tavily") {
            caps.push("web search");
        }
        if spec.browser.headless_chrome {
            caps.push("browse (headless Chrome)");
        }
        if spec.knowledge_graph.enabled {
            caps.push("knowledge graph (GraphRAG)");
        }
        if spec.capabilities.image_gen {
            caps.push("image generation");
        }
        if spec.capabilities.self_forge {
            caps.push("self-forge (build WASM tools)");
        }
        if !caps.is_empty() {
            sections.push(format!("Capabilities: {}", caps.join(", ")));
        }
    }

    match ToolRegistry::new(home.root().join("tools")).list() {
        Ok(tools) if !tools.is_empty() => {
            let mut out = String::from("Runtime (WASM) tools:\n");
            for t in tools {
                let desc = t.description.lines().next().unwrap_or("").trim();
                out.push_str(&format!("  {} — {}\n", t.name, truncate_inline(desc, 80)));
            }
            sections.push(out.trim_end().to_string());
        }
        Ok(_) => {}
        Err(error) => sections.push(format!("Could not list runtime tools: {error:#}")),
    }

    if sections.is_empty() {
        "No tools or capabilities configured for this agent yet.".to_string()
    } else {
        sections.join("\n")
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
        "No sub-tasks dispatched yet. Run one with /emerge <task>.".to_string()
    } else {
        entries.sort();
        format!(
            "Sub-tasks dispatched via /emerge (each runs as a turn and replies here):\n{}",
            entries.join("\n")
        )
    }
}

/// Curated codex model ids, split by auth mode because the OpenAI backend gates
/// the catalog on it. A ChatGPT (OAuth) login — what the firecracker codex agent
/// uses — only accepts `gpt-5.5`; every other id (gpt-5, gpt-5-codex, gpt-5-mini,
/// gpt-5.5-codex, gpt-5.1, o3, o4-mini, …) returns HTTP 400 "not supported when
/// using Codex with a ChatGPT account" (live-verified 2026-06-17, codex-cli
/// 0.140). On a ChatGPT account the model is fixed and you vary effort via
/// `/reasoning`. An API-key login can use the wider catalog. `codex_models()`
/// also unions in the operator's seeded default (config.toml `model`) so the
/// picker stays correct if the supported id changes without a code bump.
const CODEX_MODELS_CHATGPT: &[&str] = &["gpt-5.5"];
const CODEX_MODELS_APIKEY: &[&str] =
    &["gpt-5.5", "gpt-5.5-codex", "gpt-5", "gpt-5-codex", "gpt-5-mini"];
// Claude Code resolves these aliases to the current model versions (opus -> Opus
// 4.8, sonnet -> Sonnet 4.6, haiku -> Haiku 4.5). Use the aliases, NOT invented
// dotted ids like "claude-sonnet-4.6" — `claude --model` rejects those.
const CLAUDE_MODELS: &[&str] = &["opus", "sonnet", "haiku"];
const TTS_PROVIDERS: &[&str] = &["elevenlabs", "openai", "deepgram"];

/// How the agent's codex login authenticates, read from the host-side `auth.json`
/// its `harness_auth` entry points at. The OpenAI backend accepts a different
/// model set per mode (see [`CODEX_MODELS_CHATGPT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexAuthMode {
    ChatGpt,
    ApiKey,
    Unknown,
}

/// Resolve the host-side codex auth/config directory for `agent_id` from its
/// spec's `harness_auth` (the same dir maturana pushes into the guest's
/// `~/.codex`).
fn codex_host_auth_dir(home: &MaturanaHome, agent_id: &str) -> Option<PathBuf> {
    let spec =
        AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md")).ok()?;
    let auth = spec
        .harness_auth
        .iter()
        .find(|a| a.runtime == HarnessRuntime::Codex)?;
    let source = PathBuf::from(&auth.source_path);
    if source.is_absolute() {
        return Some(source);
    }
    // `source_path` is conventionally relative to the maturana project root (the
    // parent of the `.maturana` home dir), e.g. ".maturana/host-auth/codex". The
    // long-running channel daemon's cwd is the user's $HOME (systemd default),
    // not the project root, so resolve against the home root's parent first and
    // only fall back to cwd (for launch-time callers run from the project root).
    let project_root = home.root().parent().map(|p| p.join(&source));
    let cwd_relative = std::env::current_dir().ok().map(|c| c.join(&source));
    [project_root.clone(), cwd_relative]
        .into_iter()
        .flatten()
        .find(|p| p.join("auth.json").exists())
        .or(project_root)
}

/// Detect the codex auth mode from `<dir>/auth.json`. Prefers codex's explicit
/// `auth_mode` field; falls back to which credential is populated (a ChatGPT
/// login carries `tokens`, an API-key login carries `OPENAI_API_KEY`). Never
/// reads or returns any secret material — only the mode discriminator.
fn codex_auth_mode_from_dir(dir: &Path) -> CodexAuthMode {
    let Ok(raw) = fs::read_to_string(dir.join("auth.json")) else {
        return CodexAuthMode::Unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return CodexAuthMode::Unknown;
    };
    if let Some(mode) = value.get("auth_mode").and_then(|m| m.as_str()) {
        match mode.to_ascii_lowercase().as_str() {
            "chatgpt" => return CodexAuthMode::ChatGpt,
            "apikey" => return CodexAuthMode::ApiKey,
            _ => {}
        }
    }
    let has_tokens = value.get("tokens").map(|t| !t.is_null()).unwrap_or(false);
    let has_api_key = value
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    if has_tokens {
        CodexAuthMode::ChatGpt
    } else if has_api_key {
        CodexAuthMode::ApiKey
    } else {
        CodexAuthMode::Unknown
    }
}

/// The operator's seeded default model from `<dir>/config.toml` (top-level
/// `model = "..."`). Unioning it into the picker keeps the offered set correct
/// even if the curated lists drift from what the account actually accepts.
fn codex_default_model(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join("config.toml")).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break; // top-level keys precede the first table header
        }
        if let Some((key, val)) = line.split_once('=') {
            if key.trim() == "model" {
                let val = val.trim().trim_matches(|c| c == '"' || c == '\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

/// Codex model ids to offer for `agent_id`, gated by its auth mode.
fn codex_models(home: &MaturanaHome, agent_id: &str) -> Vec<String> {
    codex_models_for_auth(codex_host_auth_dir(home, agent_id).as_deref())
}

/// Curated codex model set for a host-auth `dir`, unioned with the seeded
/// default. Unknown/unreadable auth falls back to the ChatGPT-safe set because
/// `gpt-5.5` is the one id that works under both auth modes.
fn codex_models_for_auth(dir: Option<&Path>) -> Vec<String> {
    let mode = dir.map(codex_auth_mode_from_dir).unwrap_or(CodexAuthMode::Unknown);
    let base: &[&str] = match mode {
        CodexAuthMode::ApiKey => CODEX_MODELS_APIKEY,
        CodexAuthMode::ChatGpt | CodexAuthMode::Unknown => CODEX_MODELS_CHATGPT,
    };
    let mut out: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    if let Some(def) = dir.and_then(codex_default_model) {
        if !out.iter().any(|m| m == &def) {
            out.insert(0, def);
        }
    }
    out
}

/// Models offered as tappable buttons in the interactive selector. For OpenCode
/// we surface the top of the live OpenRouter catalog; the full list stays in the
/// /models text. Bounded so the inline keyboard stays usable.
fn model_button_choices(home: &MaturanaHome, agent_id: &str) -> Vec<String> {
    match harness_label(home, agent_id).as_str() {
        "opencode" => fetch_openrouter_catalog()
            .map(|models| recent_openrouter_models(&models, 20))
            .unwrap_or_default(),
        "claude-code" => CLAUDE_MODELS.iter().map(|s| s.to_string()).collect(),
        _ => codex_models(home, agent_id),
    }
}

/// The `n` most recently-added chat models from the LIVE OpenRouter catalog,
/// newest first. Filters out non-text models (image, embedding) and classifier/
/// guard models so the picker only shows things you'd actually chat with — but
/// any catalog id still works via `/model <id>`. Replaces the old hardcoded
/// "mainstream" allowlist, which went stale as new model families shipped.
fn recent_openrouter_models(models: &[OpenRouterModel], n: usize) -> Vec<String> {
    // Backstop name filters: image is also caught by `text_output`, but classifier/
    // safety/embedding models report text output yet aren't chat models.
    const DENY_SUBSTR: &[&str] = &[
        "embed", "moderation", "content-safety", "guard", "image", "rerank",
    ];
    let mut picked: Vec<&OpenRouterModel> = models
        .iter()
        .filter(|m| m.text_output)
        // opencode always sends tools; a model without tool support APIErrors.
        .filter(|m| m.supports_tools)
        .filter(|m| {
            let id = m.id.to_ascii_lowercase();
            !DENY_SUBSTR.iter().any(|deny| id.contains(deny))
        })
        .collect();
    // Newest first; ties broken by id for a stable order.
    picked.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    picked.into_iter().take(n).map(|m| m.id.clone()).collect()
}

/// Live OpenRouter catalog for OpenCode/OpenRouter; a short curated set otherwise.
fn models_text(home: &MaturanaHome, agent_id: &str) -> String {
    let settings = load_channel_settings(home, agent_id);
    let current = settings.model.clone().unwrap_or_else(|| "(harness default)".to_string());
    let harness = harness_label(home, agent_id);
    let body = if harness == "opencode" {
        match fetch_openrouter_catalog() {
            Ok(models) if !models.is_empty() => {
                let shown = recent_openrouter_models(&models, 30);
                format!(
                    "OpenRouter — {} most recent chat models (newest first):\n{}",
                    shown.len(),
                    shown.join("\n")
                )
            }
            Ok(_) => "OpenRouter returned no models.".to_string(),
            Err(error) => format!("Could not fetch OpenRouter catalog: {error:#}"),
        }
    } else if harness == "codex" {
        let dir = codex_host_auth_dir(home, agent_id);
        let mode = dir
            .as_deref()
            .map(codex_auth_mode_from_dir)
            .unwrap_or(CodexAuthMode::Unknown);
        let models = codex_models_for_auth(dir.as_deref()).join(", ");
        let note = match mode {
            CodexAuthMode::ChatGpt => {
                "ChatGPT login: only gpt-5.5 is accepted — vary effort with /reasoning"
            }
            CodexAuthMode::ApiKey => "API-key login: any catalog id also works via /model <id>",
            CodexAuthMode::Unknown => {
                "could not read codex auth; showing the ChatGPT-safe default"
            }
        };
        format!("Codex models: {models}\n({note})")
    } else {
        format!("claude-code models: {}", CLAUDE_MODELS.join(", "))
    };
    format!("Current: {current}\nSet with /model <id>\n\n{body}")
}

/// One entry from the live OpenRouter catalog, with the fields the picker needs
/// to rank by recency and keep only models you'd actually chat with.
#[derive(Debug, Clone, PartialEq)]
struct OpenRouterModel {
    id: String,
    /// Unix epoch seconds the model was added to OpenRouter (newest = largest).
    created: i64,
    /// Whether the model emits text (a chat model) vs. image/embedding-only.
    text_output: bool,
    /// Whether the model accepts a `tools` array. opencode always sends one, so a
    /// model without tool support APIErrors every turn — exclude it from the picker.
    supports_tools: bool,
}

fn fetch_openrouter_catalog() -> anyhow::Result<Vec<OpenRouterModel>> {
    let resp: serde_json::Value = ureq::get("https://openrouter.ai/api/v1/models")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .context("OpenRouter request failed")?
        .into_json()
        .context("failed to parse OpenRouter response")?;
    let models = resp
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|i| i.as_str())?.to_string();
                    let created = m.get("created").and_then(|c| c.as_i64()).unwrap_or(0);
                    // A chat model lists "text" among its output modalities. Image
                    // and embedding models do not. Absent field => assume text.
                    let text_output = m
                        .get("architecture")
                        .and_then(|a| a.get("output_modalities"))
                        .and_then(|o| o.as_array())
                        .map(|arr| arr.iter().any(|v| v.as_str() == Some("text")))
                        .unwrap_or(true);
                    // Require tool support when the field is present; absent => don't
                    // penalize on missing metadata.
                    let supports_tools = match m
                        .get("supported_parameters")
                        .and_then(|p| p.as_array())
                    {
                        Some(arr) => arr.iter().any(|v| v.as_str() == Some("tools")),
                        None => true,
                    };
                    Some(OpenRouterModel { id, created, text_output, supports_tools })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(models)
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

/// The agent's reply ends with this (on its own line) when it has finished the
/// onboarding interview. The host clears the active state and strips it before the
/// message goes out.
const ONBOARDING_COMPLETE_SENTINEL: &str = "[[ONBOARDING_COMPLETE]]";

/// While this marker exists the agent is MID-onboarding interview, so every channel
/// turn re-injects a "keep interviewing" directive — otherwise the directive only
/// reaches turn 1 and the agent answers once then goes quiet instead of asking the
/// next question. Set when onboarding starts; cleared on the completion sentinel.
fn onboarding_active_marker(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("onboarding-active")
}

fn is_onboarding_active(home: &MaturanaHome, agent_id: &str) -> bool {
    onboarding_active_marker(home, agent_id).exists()
}

fn set_onboarding_active(home: &MaturanaHome, agent_id: &str) {
    let path = onboarding_active_marker(home, agent_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, Utc::now().to_rfc3339());
}

fn clear_onboarding_active(home: &MaturanaHome, agent_id: &str) {
    let _ = fs::remove_file(onboarding_active_marker(home, agent_id));
}

/// If the agent signalled it finished onboarding, end the interview and strip the
/// sentinel from the user-facing reply. Returns the text to actually send.
pub(crate) fn finalize_onboarding_reply(home: &MaturanaHome, agent_id: &str, reply: &str) -> String {
    if reply.contains(ONBOARDING_COMPLETE_SENTINEL) {
        clear_onboarding_active(home, agent_id);
        reply.replace(ONBOARDING_COMPLETE_SENTINEL, "").trim().to_string()
    } else {
        reply.to_string()
    }
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

/// Enqueue the onboarding turn for a telegram chat. Tagged with the REAL
/// `telegram`/`chat_id` (NOT the old bogus `onboard`/`onboard`) so the agent's
/// greeting — whose outbound row inherits the inbound's channel + platform_id —
/// matches the telegram delivery loop and actually reaches the user. Wrapped with
/// `build_channel_prompt` so the greeting carries the agent's context, mirroring a
/// normal turn (`run_channel_prompt`), minus recording the system directive as
/// user history.
fn enqueue_onboarding(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
) -> anyhow::Result<()> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let directive = onboarding_prompt();
    let prompt = build_channel_prompt(home, agent_id, chat_id, &directive)?;
    // Turn 1 carries the directive as its message; mark the interview active so
    // EVERY following turn re-injects the "keep interviewing" directive until the
    // agent signals completion.
    set_onboarding_active(home, agent_id);
    let settings = load_channel_settings(home, agent_id);
    insert_inbound(
        &paths,
        "chat",
        "telegram",
        &chat_id.to_string(),
        None,
        &serde_json::json!({
            "text": directive,
            "prompt": prompt,
            "model": settings.model,
            "reasoning": settings.reasoning,
        })
        .to_string(),
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
/// Spawn `maturana orchestrator loop` as a detached host process that reports its
/// progress + final result back to THIS chat (via the chat-target flags). The
/// command handler returns immediately; a reaper thread waits on the child so it
/// never becomes a zombie. The loop is a normal process, so it keeps running after
/// the handler returns (only a full plane restart would stop it mid-run).
fn spawn_loop_process(
    home: &MaturanaHome,
    run_id: &str,
    goal: &str,
    channel: &str,
    platform_id: &str,
    agent_id: &str,
    session_id: &str,
    no_verify: bool,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate the maturana binary")?;
    let run_dir = home.root().join("orchestration").join(run_id);
    fs::create_dir_all(&run_dir)?;
    let log = fs::File::create(run_dir.join("loop.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home")
        .arg(home.root())
        .arg("orchestrator")
        .arg("loop")
        .arg(goal)
        .arg("--run-id")
        .arg(run_id)
        .arg("--chat-channel")
        .arg(channel)
        .arg("--chat-platform-id")
        .arg(platform_id)
        .arg("--chat-agent")
        .arg(agent_id)
        .arg("--chat-session")
        .arg(session_id)
        .stdin(std::process::Stdio::null())
        .stderr(log.try_clone()?)
        .stdout(log);
    if no_verify {
        cmd.arg("--no-verify");
    }
    let mut child = cmd.spawn().context("spawn orchestrator loop")?;
    // Reap the child off-thread so it doesn't zombie; never blocks the channel.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A run id is safe to use as a single path segment under `orchestration/`:
/// non-empty, no traversal, only `[A-Za-z0-9._-]`. Guards `/loop abort|status`,
/// which join the operator-supplied id into a filesystem path.
fn valid_run_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `/loop` — start a multi-agent orchestration loop on a goal (it posts progress
/// + the result back here), or manage one (`status` / `abort`). Available on every
/// text channel through the shared command handler.
fn handle_loop_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    args: &str,
) -> String {
    let args = args.trim();
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((a, b)) => (a.to_ascii_lowercase(), b.trim()),
        None => (args.to_ascii_lowercase(), ""),
    };
    match sub.as_str() {
        "" => loop_usage_text(),
        "abort" => {
            if rest.is_empty() {
                return "Usage: /loop abort <run_id>".to_string();
            }
            // run_id becomes a path segment under orchestration/ — reject anything
            // that could traverse out (a `/loop abort ../../x` would otherwise drop
            // an `abort` file at an attacker-chosen path).
            if !valid_run_id(rest) {
                return "Invalid run id.".to_string();
            }
            let dir = home.root().join("orchestration").join(rest);
            if !dir.exists() {
                return format!("No loop `{rest}` found.");
            }
            match fs::write(dir.join("abort"), "aborted") {
                Ok(()) => format!(
                    "🛑 Abort requested for `{rest}` — the in-flight step finishes its lease, then it stops."
                ),
                Err(error) => format!("Couldn't abort `{rest}`: {error}"),
            }
        }
        "status" => {
            if rest.is_empty() {
                loop_list_text(home)
            } else if !valid_run_id(rest) {
                "Invalid run id.".to_string()
            } else {
                loop_status_text(home, rest)
            }
        }
        // `/loop fast <goal>` skips the run-it-and-verify pass for ~half the time.
        "fast" => start_loop(home, agent_id, session_id, chat_id, channel, platform_id, rest, true),
        // Anything else is the goal.
        _ => start_loop(home, agent_id, session_id, chat_id, channel, platform_id, args, false),
    }
}

/// Spawn a `/loop` run and return the user-facing acknowledgement. `no_verify`
/// skips the deliverable's run-it-and-check pass (the `fast` form).
#[allow(clippy::too_many_arguments)]
fn start_loop(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    goal: &str,
    no_verify: bool,
) -> String {
    let goal = goal.trim();
    if goal.is_empty() {
        return loop_usage_text();
    }
    let run_id = format!("loop-{}-{}", chat_id.unsigned_abs(), Utc::now().timestamp());
    match spawn_loop_process(
        home,
        &run_id,
        goal,
        channel,
        platform_id,
        agent_id,
        session_id,
        no_verify,
    ) {
        Ok(()) => {
            let _ = audit_channel_event(
                home,
                agent_id,
                "channel.loop.start",
                &format!("{run_id}{}: {goal}", if no_verify { " (fast)" } else { "" }),
            );
            let mode = if no_verify {
                " (fast — skips the run-it-and-verify pass)"
            } else {
                ""
            };
            format!(
                "🔄 Loop `{run_id}` started{mode} on:\n{goal}\n\nSeveral agents will plan it, do the parts, check the result, and combine them — I'll post the plan and each step here, then the result (files attached when produced).\nManage: /loop status {run_id} · /loop abort {run_id}"
            )
        }
        Err(error) => format!("Couldn't start the loop: {error:#}"),
    }
}

fn loop_usage_text() -> String {
    "🔄 /loop runs a multi-agent loop on a goal: several agents plan it, do the parts, \
     check the result, and combine them — progress posts here.\n\n\
     • /loop <goal> — start (e.g. /loop build a tic-tac-toe game playable in the browser)\n\
     • /loop fast <goal> — start without the run-it-and-verify pass (~half the time)\n\
     • /loop status [run_id] — list loops, or show one run's steps\n\
     • /loop abort <run_id> — stop a run"
        .to_string()
}

/// A one-line state for a loop run, read from its durable plan.json.
fn loop_state_label(run_dir: &Path) -> String {
    let raw = match fs::read_to_string(run_dir.join("plan.json")) {
        Ok(raw) => raw,
        Err(_) => return "planning…".to_string(),
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return "planning…".to_string(),
    };
    let Some(steps) = value.get("steps").and_then(|s| s.as_array()) else {
        return "planning…".to_string();
    };
    let status_is = |step: &serde_json::Value, want: &str| {
        step.get("status")
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case(want))
            .unwrap_or(false)
    };
    let total = steps.len();
    let done = steps.iter().filter(|s| status_is(s, "done")).count();
    let failed = steps.iter().any(|s| status_is(s, "failed"));
    let state = if run_dir.join("abort").exists() {
        "aborted"
    } else if failed {
        "failed"
    } else if total > 0 && done == total {
        "complete"
    } else {
        "running"
    };
    format!("{state} — {done}/{total} steps")
}

fn loop_status_text(home: &MaturanaHome, run_id: &str) -> String {
    let run_dir = home.root().join("orchestration").join(run_id);
    if !run_dir.exists() {
        return format!("No loop `{run_id}` found.");
    }
    let goal = fs::read_to_string(run_dir.join("plan.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("goal").and_then(|g| g.as_str()).map(str::to_string))
        .unwrap_or_default();
    if goal.is_empty() {
        format!("Loop `{run_id}`: {}", loop_state_label(&run_dir))
    } else {
        format!("Loop `{run_id}`: {}\nGoal: {goal}", loop_state_label(&run_dir))
    }
}

fn loop_list_text(home: &MaturanaHome) -> String {
    let dir = home.root().join("orchestration");
    let mut runs: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("loop-") {
                continue;
            }
            runs.push(format!("• `{name}` — {}", loop_state_label(&entry.path())));
        }
    }
    if runs.is_empty() {
        "No loops yet. Start one with /loop <goal>.".to_string()
    } else {
        runs.sort();
        format!("Loops:\n{}", runs.join("\n"))
    }
}

fn handle_channel_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    name: &str,
    args: &str,
) -> anyhow::Result<String> {
    // Channel-agnostic: the console TUI and Telegram share this handler. A tiny
    // owned shim keeps the body's `config.agent_id`/`config.session_id` reads intact.
    struct Target {
        agent_id: String,
        session_id: String,
    }
    let config = Target {
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
    };
    let reply = match name {
        "commands" => commands_text(),
        "tools" => tools_text(home, &config.agent_id),
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
        "reasoning" => {
            let mut settings = load_channel_settings(home, &config.agent_id);
            let arg = args.trim().to_lowercase();
            if arg.is_empty() {
                format!(
                    "Reasoning effort: {} (codex/gpt-5). Set with /reasoning <{}>",
                    settings.reasoning.clone().unwrap_or_else(|| "low (default)".to_string()),
                    REASONING_LEVELS.join("|"),
                )
            } else if REASONING_LEVELS.contains(&arg.as_str()) {
                settings.reasoning = Some(arg.clone());
                save_channel_settings(home, &config.agent_id, &settings)?;
                format!("Reasoning effort set to `{arg}` (applies to new turns; codex/gpt-5).")
            } else {
                format!("Unknown level `{arg}`. Choose one of: {}", REASONING_LEVELS.join(", "))
            }
        }
        "reset" => {
            reset_channel_context(home, &config.agent_id, chat_id)?;
            "Session reset — durable memory and wiki are preserved.".to_string()
        }
        "stop" => {
            // Two halves: drop queued-but-unclaimed turns, AND flag any IN-PROGRESS
            // turn so the guest worker (which polls sessiond) kills the running
            // harness mid-turn and replies "Stopped." The in-progress kill is async
            // (the worker notices within a couple of seconds), so we acknowledge it.
            let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
            let queued = cancel_pending_inbound(&paths).unwrap_or(0);
            let in_progress = request_cancel_in_progress(&paths).unwrap_or(0);
            match (queued, in_progress) {
                (0, 0) => "Nothing to stop — nothing is queued or in progress.".to_string(),
                (q, 0) => format!(
                    "Stopped {q} queued message{}.",
                    if q == 1 { "" } else { "s" }
                ),
                (0, _) => "Stopping the reply in progress…".to_string(),
                (q, _) => format!(
                    "Stopping the reply in progress and dropped {q} queued message{}.",
                    if q == 1 { "" } else { "s" }
                ),
            }
        }
        "compact" => {
            // The per-turn context is always the recent *tail* of the transcript
            // (auto-bounded), so "compaction" here is housekeeping: truncate the
            // on-disk transcript to that tail so it can't grow without bound.
            // Durable facts already live in memory + the wiki, and recent context
            // is preserved.
            let path = channel_transcript_path(home, &config.agent_id, chat_id);
            match compact_transcript_file(&path, TRANSCRIPT_CONTEXT_CHARS) {
                Ok(0) => "Nothing to compact — the transcript is already within the live context window.".to_string(),
                Ok(freed) => format!(
                    "Compacted the stored transcript (freed ~{} KB). Recent context is preserved; durable facts live in memory + the wiki.",
                    (freed + 1023) / 1024
                ),
                Err(error) => format!("Couldn't compact the transcript: {error:#}"),
            }
        }
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
        "loop" => handle_loop_command(
            home,
            &config.agent_id,
            &config.session_id,
            chat_id,
            channel,
            platform_id,
            args,
        ),
        _ => format!("Unknown command `/{name}`. Try /help."),
    };
    Ok(reply)
}

/// For commands with a small, well-known choice set, return an interactive
/// selector (prompt text, `(label, callback_data)` buttons, column count) instead
/// of a plain text reply. Returns `None` for everything else (handled as text) and
/// for the explicit-argument form (`/model gpt-5` sets directly, no menu).
fn command_selector(
    home: &MaturanaHome,
    config: &TelegramServe,
    name: &str,
    args: &str,
) -> Option<(String, Vec<(String, String)>, usize)> {
    if !args.trim().is_empty() {
        return None;
    }
    command_selector_buttons(home, &config.agent_id, name)
}

/// The button source shared by the Telegram inline keyboard and the console TUI
/// picker. Returns (prompt, [(label, callback_data)], columns); `None` for
/// non-selectable commands. callback_data is always `<action>:<value>`.
fn command_selector_buttons(
    home: &MaturanaHome,
    agent_id: &str,
    name: &str,
) -> Option<(String, Vec<(String, String)>, usize)> {
    let settings = load_channel_settings(home, agent_id);
    match name {
        "models" | "model" => {
            let current = settings
                .model
                .unwrap_or_else(|| "(harness default)".to_string());
            // callback_data is capped at 64 bytes; drop any id that wouldn't fit.
            let buttons: Vec<(String, String)> = model_button_choices(home, agent_id)
                .into_iter()
                .map(|id| {
                    let data = format!("model:{id}");
                    (id, data)
                })
                .filter(|(_, data)| data.len() <= 64)
                .collect();
            if buttons.is_empty() {
                return None;
            }
            Some((
                format!("Current model: {current}\nTap a recent model, or send /model <id> for any model:"),
                buttons,
                2,
            ))
        }
        "reasoning" => {
            let current = settings
                .reasoning
                .unwrap_or_else(|| "low (default)".to_string());
            let buttons: Vec<(String, String)> = REASONING_LEVELS
                .iter()
                .map(|lvl| (lvl.to_string(), format!("reasoning:{lvl}")))
                .collect();
            Some((
                format!("Reasoning effort: {current} (codex/gpt-5)\nTap a level:"),
                buttons,
                2,
            ))
        }
        "tts-provider" => {
            let current = settings.tts_provider.unwrap_or_else(|| "(none)".to_string());
            let buttons = TTS_PROVIDERS
                .iter()
                .map(|p| (p.to_string(), format!("ttsprov:{p}")))
                .collect();
            Some((format!("TTS provider: {current}\nPick one:"), buttons, 1))
        }
        "tts" => {
            let buttons = vec![
                ("Enable".to_string(), "tts:on".to_string()),
                ("Disable".to_string(), "tts:off".to_string()),
            ];
            Some((
                format!(
                    "Text-to-speech is {}.",
                    if settings.tts_enabled { "ON" } else { "off" }
                ),
                buttons,
                2,
            ))
        }
        "session" => {
            let buttons = vec![
                ("Active".to_string(), "session:active".to_string()),
                ("Idle".to_string(), "session:idle".to_string()),
            ];
            Some((
                format!(
                    "Session is {}.",
                    if settings.idle { "idle" } else { "active" }
                ),
                buttons,
                2,
            ))
        }
        _ => None,
    }
}

/// Apply an inline-keyboard button tap: persist the chosen setting, clear the
/// client spinner with a toast, and replace the menu message with a confirmation.
/// Apply one `<action>:<value>` selection: set exactly one ChannelSettings field
/// and persist it. Shared by the Telegram inline-keyboard callback and the
/// console TUI picker so the two can't drift. Returns a confirmation string.
pub(crate) fn apply_channel_selection(home: &MaturanaHome, agent_id: &str, data: &str) -> String {
    let (action, value) = data.split_once(':').unwrap_or((data, ""));
    let mut settings = load_channel_settings(home, agent_id);
    let save = |settings: &ChannelSettings| save_channel_settings(home, agent_id, settings);
    match action {
        "model" => {
            settings.model = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("Model set to `{value}` (applies to new turns)."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "reasoning" => {
            settings.reasoning = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("Reasoning effort set to `{value}` (applies to new turns)."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "ttsprov" => {
            settings.tts_provider = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("TTS provider set to `{value}`."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "tts" => {
            settings.tts_enabled = value == "on";
            let state = if settings.tts_enabled { "ENABLED" } else { "disabled" };
            match save(&settings) {
                Ok(_) => format!("Text-to-speech {state}."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "session" => {
            settings.idle = value == "idle";
            let state = if settings.idle { "idle" } else { "active" };
            match save(&settings) {
                Ok(_) => format!("Session set to {state}."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        _ => "Unknown selection.".to_string(),
    }
}

fn handle_telegram_callback(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    paired_chat_id: Option<i64>,
    callback: &TelegramCallbackQuery,
) -> anyhow::Result<()> {
    let Some(message) = &callback.message else {
        // The original message is no longer accessible; just stop the spinner.
        return answer_callback_query(token, &callback.id, None);
    };
    let chat_id = message.chat.id;
    if paired_chat_id != Some(chat_id) {
        answer_callback_query(token, &callback.id, Some("Not paired with this chat."))?;
        return Ok(());
    }
    let data = callback.data.clone().unwrap_or_default();
    let (action, value) = data.split_once(':').unwrap_or((data.as_str(), ""));
    // Telegram-only toast; the actual persist is shared with the console picker.
    let toast = match action {
        "model" => format!("Model: {value}"),
        "reasoning" => format!("Reasoning: {value}"),
        "ttsprov" => format!("Provider: {value}"),
        "tts" => format!("TTS {}", if value == "on" { "ENABLED" } else { "disabled" }),
        "session" => format!("Session {value}"),
        _ => String::new(),
    };
    let updated = apply_channel_selection(home, &config.agent_id, &data);
    answer_callback_query(token, &callback.id, Some(&toast))?;
    // Replacing the text also strips the keyboard, so the menu can't be re-tapped.
    let _ = edit_telegram_message(token, chat_id, message.message_id, &updated);
    audit_channel_event(home, &config.agent_id, "channel.telegram.callback", &data)?;
    Ok(())
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

/// Read-aloud for channels: when /tts is enabled, synthesize the reply with the
/// selected provider and send it as an audio message after the text. Always
/// best-effort — a TTS failure (missing key, provider error) must never affect
/// the text reply that was already delivered, so we log and move on.
fn maybe_send_tts(
    home: &MaturanaHome,
    token: &str,
    agent_id: &str,
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
) {
    let settings = load_channel_settings(home, agent_id);
    if !settings.tts_enabled {
        return;
    }
    let spoken = text.trim();
    if spoken.is_empty() || spoken == crate::proactive::SILENCE_SENTINEL {
        return;
    }
    // Bound the spoken text — providers and Telegram both cap upload size.
    let spoken: String = spoken.chars().take(4000).collect();
    let provider = settings.tts_provider.as_deref().unwrap_or("openai");
    match synthesize_speech(home, provider, &spoken) {
        Ok(audio) => {
            if let Err(error) = send_telegram_audio(token, chat_id, &audio, reply_to) {
                eprintln!("telegram tts send failed (text already delivered): {error:#}");
            }
        }
        Err(error) => {
            eprintln!("tts synthesis failed via {provider} (text already delivered): {error:#}");
        }
    }
}

/// Synthesize speech (mp3 bytes) host-side via the chosen provider. Keys come
/// from pipelock and never reach the guest. Supported: openai (default),
/// elevenlabs, deepgram — matching the /tts-provider picker.
fn synthesize_speech(home: &MaturanaHome, provider: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    let resolve = |source: &str| -> anyhow::Result<String> {
        Ok(resolve_secret_source_with_home(source, home.root())?
            .expose_for_runtime()
            .to_string())
    };
    let request = match provider.to_ascii_lowercase().as_str() {
        "elevenlabs" => {
            let key = resolve("pipelock:elevenlabs/api-key")?;
            // Default multilingual voice ("Rachel").
            ureq::post("https://api.elevenlabs.io/v1/text-to-speech/21m00Tcm4TlvDq8ikWAM")
                .set("xi-api-key", &key)
                .set("accept", "audio/mpeg")
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({
                    "text": text,
                    "model_id": "eleven_multilingual_v2",
                }))
        }
        "deepgram" => {
            let key = resolve("pipelock:deepgram/api-key")?;
            ureq::post("https://api.deepgram.com/v1/speak?model=aura-asteria-en")
                .set("authorization", &format!("Token {key}"))
                .set("content-type", "application/json")
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({ "text": text }))
        }
        _ => {
            // OpenAI (default), matching the cockpit voice path.
            let key = resolve("pipelock:openai/api-key")?;
            ureq::post("https://api.openai.com/v1/audio/speech")
                .set("authorization", &format!("Bearer {key}"))
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({
                    "model": "tts-1",
                    "input": text,
                    "voice": "alloy",
                    "response_format": "mp3",
                }))
        }
    };
    let response = request.map_err(|e| anyhow::anyhow!("{provider} tts request failed: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)?;
    if bytes.is_empty() {
        anyhow::bail!("{provider} tts returned no audio");
    }
    Ok(bytes)
}

/// Choose an STT provider: honor the chat's `/tts-provider` choice if its key is
/// configured (so a user who set elevenlabs gets elevenlabs both ways), otherwise
/// fall back to the first provider whose key is present (elevenlabs → openai →
/// deepgram). When nothing is configured, default to elevenlabs so the failure
/// message points the user at the most likely key to set.
fn stt_provider(home: &MaturanaHome, tts_provider: Option<&str>) -> String {
    let configured = |provider: &str| -> bool {
        let source = match provider {
            "elevenlabs" => "pipelock:elevenlabs/api-key",
            "openai" => "pipelock:openai/api-key",
            "deepgram" => "pipelock:deepgram/api-key",
            _ => return false,
        };
        resolve_secret_source_with_home(source, home.root()).is_ok()
    };
    if let Some(preferred) = tts_provider {
        let preferred = preferred.to_ascii_lowercase();
        if configured(&preferred) {
            return preferred;
        }
    }
    for provider in ["elevenlabs", "openai", "deepgram"] {
        if configured(provider) {
            return provider.to_string();
        }
    }
    "elevenlabs".to_string()
}

/// Transcribe audio host-side (STT) via the chosen provider. Keys come from
/// pipelock and never reach the guest. Supported: elevenlabs (scribe_v1, default),
/// openai (whisper-1), deepgram (nova-2) — the same provider set as `/tts-provider`.
fn transcribe_speech(
    home: &MaturanaHome,
    provider: &str,
    audio: &[u8],
    filename: &str,
) -> anyhow::Result<String> {
    let resolve = |source: &str| -> anyhow::Result<String> {
        Ok(resolve_secret_source_with_home(source, home.root())?
            .expose_for_runtime()
            .to_string())
    };
    match provider.to_ascii_lowercase().as_str() {
        "openai" => {
            let key = resolve("pipelock:openai/api-key")?;
            let (content_type, payload) = multipart_audio("model", "whisper-1", filename, audio);
            let response = ureq::post("https://api.openai.com/v1/audio/transcriptions")
                .set("authorization", &format!("Bearer {key}"))
                .set("content-type", &content_type)
                .timeout(Duration::from_secs(120))
                .send_bytes(&payload)
                .map_err(|e| anyhow::anyhow!("openai stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
        "deepgram" => {
            let key = resolve("pipelock:deepgram/api-key")?;
            let response =
                ureq::post("https://api.deepgram.com/v1/listen?model=nova-2&smart_format=true")
                    .set("authorization", &format!("Token {key}"))
                    .set("content-type", "audio/ogg")
                    .timeout(Duration::from_secs(120))
                    .send_bytes(audio)
                    .map_err(|e| anyhow::anyhow!("deepgram stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .pointer("/results/channels/0/alternatives/0/transcript")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
        _ => {
            // ElevenLabs scribe (default).
            let key = resolve("pipelock:elevenlabs/api-key")?;
            let (content_type, payload) = multipart_audio("model_id", "scribe_v1", filename, audio);
            let response = ureq::post("https://api.elevenlabs.io/v1/speech-to-text")
                .set("xi-api-key", &key)
                .set("content-type", &content_type)
                .timeout(Duration::from_secs(120))
                .send_bytes(&payload)
                .map_err(|e| anyhow::anyhow!("elevenlabs stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
    }
}

/// Build a minimal multipart/form-data body for an STT endpoint: a model field
/// plus the audio file. `model_field` is `model` (OpenAI) or `model_id`
/// (ElevenLabs). Returns (content_type, body).
fn multipart_audio(
    model_field: &str,
    model_value: &str,
    filename: &str,
    audio: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "maturanasttboundary7e3f";
    let mut body = Vec::new();
    let part = |headers: &str, data: &[u8], body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(headers.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    part(
        &format!("content-disposition: form-data; name=\"{model_field}\""),
        model_value.as_bytes(),
        &mut body,
    );
    part(
        &format!(
            "content-disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream"
        ),
        audio,
        &mut body,
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Upload mp3 audio to the chat via Telegram `sendAudio` (multipart/form-data).
fn send_telegram_audio(
    token: &str,
    chat_id: i64,
    audio: &[u8],
    reply_to: Option<i64>,
) -> anyhow::Result<()> {
    let boundary = "maturanattsboundary7e3f";
    let mut body: Vec<u8> = Vec::new();
    let field = |name: &str, value: &str, body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("content-disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field("chat_id", &chat_id.to_string(), &mut body);
    if let Some(id) = reply_to {
        field("reply_to_message_id", &id.to_string(), &mut body);
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"content-disposition: form-data; name=\"audio\"; filename=\"reply.mp3\"\r\ncontent-type: audio/mpeg\r\n\r\n",
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let response = ureq::post(&format!("https://api.telegram.org/bot{token}/sendAudio"))
        .set(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(Duration::from_secs(60))
        .send_bytes(&body)
        .map_err(|e| anyhow::anyhow!("telegram sendAudio failed: {e}"))?;
    let _ = response.into_string();
    Ok(())
}

/// Upload a host-side file to the chat via Telegram `sendDocument`
/// (multipart/form-data), with an optional caption on the first one. Returns the
/// sent message id. The Telegram bot API caps `sendDocument` at 50 MB.
fn send_telegram_document(
    token: &str,
    chat_id: i64,
    path: &Path,
    caption: Option<&str>,
    reply_to: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;
    let meta =
        fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > MAX_DOCUMENT_BYTES {
        anyhow::bail!(
            "{} is {} bytes (over Telegram's 50 MB sendDocument limit)",
            path.display(),
            meta.len()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().replace(['"', '\r', '\n'], "_"))
        .unwrap_or_else(|| "file".to_string());
    let boundary = "maturanadocboundary7e3f";
    let mut body: Vec<u8> = Vec::new();
    let field = |name: &str, value: &str, body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("content-disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field("chat_id", &chat_id.to_string(), &mut body);
    if let Some(caption) = caption {
        // Telegram caps a document caption at ~1024 chars.
        let caption: String = caption.chars().take(1000).collect();
        field("caption", &caption, &mut body);
    }
    if let Some(id) = reply_to {
        field("reply_to_message_id", &id.to_string(), &mut body);
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "content-disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let response = ureq::post(&format!("https://api.telegram.org/bot{token}/sendDocument"))
        .set(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(Duration::from_secs(120))
        .send_bytes(&body)
        .map_err(|e| anyhow::anyhow!("telegram sendDocument failed: {e}"))?;
    let parsed: TelegramSendResponse = response
        .into_json()
        .unwrap_or(TelegramSendResponse { ok: false, result: None });
    Ok(parsed.result.map(|r| r.message_id))
}

/// Base URL for the Telegram Bot API. Overridable via `MATURANA_TELEGRAM_API_BASE`
/// so a test can point the live-progress + dissolve path at a local mock server and
/// capture the exact HTTP sequence it emits — exercising the real send/edit code
/// end-to-end without touching real Telegram or a bot token.
fn tg_api_base() -> String {
    std::env::var("MATURANA_TELEGRAM_API_BASE")
        .unwrap_or_else(|_| "https://api.telegram.org".to_string())
}

/// Hard ceiling on every Telegram HTTP call. ureq has NO default timeout, so
/// without this a single hung request (a stalled egress proxy, a slow Telegram
/// edit) blocks the synchronous progress loop indefinitely — which is exactly how
/// the live "Thinking…" clock froze mid-turn with no error logged. With a ceiling,
/// a slow call fails fast, the loop logs + retries, and the clock keeps ticking.
const TG_HTTP_TIMEOUT: Duration = Duration::from_secs(12);

/// Target cadence for the live "Thinking…" counter, and the ceiling we back off
/// to when Telegram pushes back. ~1s gives a smooth, stopwatch-like count instead
/// of multi-second jumps. Edits go through the persistent keep-alive agent (no
/// per-edit TLS handshake) and the session DBs are WAL (reads don't stall on the
/// worker's writes), so the synchronous loop keeps up; if Telegram ever 429s or an
/// edit stalls, the interval doubles up to `LIVE_EDIT_MAX` and snaps back to base
/// the instant an edit lands again.
const LIVE_EDIT_BASE: Duration = Duration::from_millis(1000);
const LIVE_EDIT_MAX: Duration = Duration::from_secs(20);

/// A persistent, tightly-bounded HTTP agent for the high-frequency live-progress
/// edits. Two reasons it is separate from the one-shot `ureq::post` calls:
///   1. **Connection reuse.** A fresh `ureq::post` opens a new TLS connection
///      every tick; reusing one keep-alive connection removes a per-edit
///      handshake that, under Telegram throttling, was part of the stall.
///   2. **Hard per-phase bounds.** `ureq`'s request-level `.timeout()` behaves as
///      a *read* timeout (≈3s connect + 12s read ≈ the ~15s edits we observed in
///      the journal). Setting connect/read/write explicitly caps the WHOLE call at
///      a few seconds, so a single stalled edit can never freeze the clock for 15s.
fn tg_live_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(4))
            .timeout_read(Duration::from_secs(6))
            .timeout_write(Duration::from_secs(6))
            .build()
    })
}

/// Outcome of a live-progress edit, so the loop can back off intelligently instead
/// of hammering a Telegram that is already throttling us.
enum LiveEditOutcome {
    /// The edit landed (or there was nothing to change).
    Ok,
    /// 429 flood control. `retry_after` is Telegram's requested cooldown in seconds
    /// (from the `retry-after` header), when it supplied one.
    Throttled(Option<u64>),
    /// Timeout / network error / non-2xx — back off and retry next tick.
    Failed(String),
}

/// Edit the live message through the bounded, connection-reusing agent. Classifies
/// 429 (so the caller honors `retry_after`) and treats a benign 400 "message is not
/// modified" as a no-op success so it never triggers backoff.
fn edit_telegram_live(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> LiveEditOutcome {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    let req = tg_live_agent()
        .post(&format!("{}/bot{token}/editMessageText", tg_api_base()))
        .set("content-type", "application/json");
    match req.send_string(&body.to_string()) {
        Ok(resp) => match resp.into_json::<TelegramOkResponse>() {
            Ok(parsed) if parsed.ok => LiveEditOutcome::Ok,
            Ok(_) => LiveEditOutcome::Failed("editMessageText returned ok=false".to_string()),
            Err(error) => LiveEditOutcome::Failed(format!("parse: {error}")),
        },
        Err(ureq::Error::Status(429, resp)) => {
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            LiveEditOutcome::Throttled(retry_after)
        }
        Err(ureq::Error::Status(400, resp)) => {
            // "Bad Request: message is not modified" is harmless (we already dedup on
            // the rendered text); anything else 400 is a real problem worth logging.
            let desc = resp.into_string().unwrap_or_default();
            if desc.contains("not modified") {
                LiveEditOutcome::Ok
            } else {
                LiveEditOutcome::Failed(format!("status 400: {}", desc.trim()))
            }
        }
        Err(ureq::Error::Status(code, _)) => LiveEditOutcome::Failed(format!("status {code}")),
        Err(error) => LiveEditOutcome::Failed(error.to_string()),
    }
}

/// HTML variant of [`edit_telegram_live`] for the rich `<pre>`/`<tg-spoiler>` draft.
fn edit_telegram_live_html(token: &str, chat_id: i64, message_id: i64, html: &str) -> LiveEditOutcome {
    edit_telegram_live(token, chat_id, message_id, html, Some("HTML"))
}

fn send_telegram(
    token: &str,
    chat_id: &str,
    message: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    send_telegram_with(token, chat_id, message, reply_to_message_id, None)
}

/// Send an HTML-formatted message (Telegram `parse_mode: "HTML"`), used for the
/// rich live progress draft (bold tool titles + monospace detail). All dynamic
/// content MUST be passed through `html_escape` by the caller.
fn send_telegram_html(
    token: &str,
    chat_id: &str,
    html: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    send_telegram_with(token, chat_id, html, reply_to_message_id, Some("HTML"))
}

fn send_telegram_with(
    token: &str,
    chat_id: &str,
    message: &str,
    reply_to_message_id: Option<i64>,
    parse_mode: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    if let Some(message_id) = reply_to_message_id {
        body["reply_parameters"] = serde_json::json!({
            "message_id": message_id,
        });
    }
    let response: TelegramSendResponse =
        ureq::post(&format!("{}/bot{token}/sendMessage", tg_api_base()))
            .timeout(TG_HTTP_TIMEOUT)
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

/// Register the slash-command menu so Telegram clients show the command list
/// when the user types `/` (the interactive command picker). Telegram command
/// names allow only `[a-z0-9_]{1,32}`, so hyphens in our catalog (`graph-query`)
/// are sent as underscores; the classifier maps `_` back to `-` on the way in.
fn set_telegram_commands(token: &str) -> anyhow::Result<()> {
    let mut commands: Vec<serde_json::Value> = Vec::new();
    for (_, cmds) in COMMAND_GROUPS {
        for (name, desc) in *cmds {
            let command = name.trim_start_matches('/').replace('-', "_");
            let description: String = desc.chars().take(256).collect();
            commands.push(serde_json::json!({
                "command": command,
                "description": description,
            }));
        }
    }
    let body = serde_json::json!({ "commands": commands });
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/setMyCommands"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram setMyCommands failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram setMyCommands response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram setMyCommands returned ok=false");
    }
    Ok(())
}

/// Send a message with an inline keyboard (tappable buttons). `buttons` is a flat
/// list of `(label, callback_data)`; `columns` lays them out into rows. The data
/// payloads come back as a `callback_query` update when tapped (max 64 bytes each).
fn send_telegram_keyboard(
    token: &str,
    chat_id: &str,
    text: &str,
    buttons: &[(String, String)],
    columns: usize,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let columns = columns.max(1);
    let rows: Vec<Vec<serde_json::Value>> = buttons
        .chunks(columns)
        .map(|chunk| {
            chunk
                .iter()
                .map(|(label, data)| {
                    serde_json::json!({ "text": label, "callback_data": data })
                })
                .collect()
        })
        .collect();
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "reply_markup": { "inline_keyboard": rows },
    });
    if let Some(message_id) = reply_to_message_id {
        body["reply_parameters"] = serde_json::json!({ "message_id": message_id });
    }
    let response: TelegramSendResponse =
        ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendMessage (keyboard) failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram sendMessage response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram sendMessage (keyboard) returned ok=false");
    }
    Ok(response.result.map(|message| message.message_id))
}

/// Acknowledge a button tap so Telegram stops the client-side spinner. An
/// optional short `text` is shown as a transient toast.
fn answer_callback_query(token: &str, callback_query_id: &str, text: Option<&str>) -> anyhow::Result<()> {
    let mut body = serde_json::json!({ "callback_query_id": callback_query_id });
    if let Some(text) = text {
        if !text.is_empty() {
            body["text"] = serde_json::json!(text);
        }
    }
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/answerCallbackQuery"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram answerCallbackQuery failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram answerCallbackQuery response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram answerCallbackQuery returned ok=false");
    }
    Ok(())
}

fn send_telegram_chat_action(token: &str, chat_id: &str, action: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "action": action,
    });
    let response: TelegramOkResponse = ureq::post(&format!(
        "{}/bot{token}/sendChatAction",
        tg_api_base()
    ))
    .timeout(TG_HTTP_TIMEOUT)
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
/// Escape text for Telegram `parse_mode: "HTML"` (only `&`, `<`, `>` are special).
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Collapse internal whitespace/newlines so a multi-line command renders as a
/// single compact progress line.
fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Title-case an unknown tool key (`record_voice` → `Record Voice`) for the
/// fallback display, mirroring OpenClaw's `defaultTitle`.
fn titleize(key: &str) -> String {
    key.split(|c| c == '_' || c == '-' || c == ' ')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map a tool key to its (emoji, title) — ported from OpenClaw's
/// `tool-display.json` so the live progress reads the same way: a glanceable icon
/// plus a human title. Unknown keys fall back to 🧩 + a title-cased key.
fn tool_display(key: &str) -> (&'static str, String) {
    let known: Option<(&str, &str)> = match key.trim().to_ascii_lowercase().as_str() {
        "bash" | "exec" | "shell" | "command" | "command_execution" => Some(("🛠️", "Bash")),
        "process" => Some(("🧰", "Process")),
        "read" => Some(("📖", "Read")),
        "write" => Some(("✍️", "Write")),
        "edit" | "file_change" | "apply_patch" | "patch" => Some(("📝", "Edit")),
        "attach" => Some(("📎", "Attach")),
        "browser" | "browse" => Some(("🌐", "Browser")),
        "web_search" | "search" | "websearch" => Some(("🔎", "Web Search")),
        "web_fetch" | "fetch" => Some(("📄", "Web Fetch")),
        "code_execution" => Some(("🧮", "Code Execution")),
        "update_plan" | "plan" | "todo_list" | "todo" => Some(("🗺️", "Update Plan")),
        "memory_search" => Some(("🗄️", "Memory Search")),
        "memory_get" => Some(("📓", "Memory Get")),
        "image" | "image_generate" => Some(("🎨", "Image")),
        "mcp_tool_call" | "tool_call" | "tool_call_update" => Some(("🧰", "Tool Call")),
        "message" => Some(("✉️", "Message")),
        _ => None,
    };
    match known {
        Some((emoji, title)) => (emoji, title.to_string()),
        None => ("🧩", titleize(key)),
    }
}

/// A worker tool event carries `"<toolkey>\u{1f}<detail>"` (new) or a legacy
/// `"running: <cmd>" / "done: <cmd>"` string (old guest worker). Parse either into
/// `(toolkey, detail)`.
fn parse_tool_event(text: &str) -> (String, String) {
    if let Some((key, detail)) = text.split_once('\u{1f}') {
        return (key.trim().to_string(), detail.to_string());
    }
    let detail = text
        .strip_prefix("running: ")
        .or_else(|| text.strip_prefix("done: "))
        .or_else(|| text.find("): ").map(|i| &text[i + 3..]))
        .unwrap_or(text);
    ("bash".to_string(), detail.to_string())
}

/// Render the live progress side-lane as ONE OpenClaw-style draft: a single
/// monospace block (Telegram `<pre>` — the "different font") of the tools the
/// agent is running, e.g.
/// ```text
/// 🛠️ rg foo
/// 🔎 Web Search: L:Ron:Harald top songs
/// ```
/// Empty when there's nothing to show yet. Sent with `parse_mode: "HTML"`; the
/// block content is HTML-escaped. No "thinking"/brain chatter — just the tools.
fn render_progress_html(events: &[ProgressEvent]) -> String {
    let mut lines: Vec<(String, String)> = Vec::new();
    let mut text = "";
    let mut errored = false;
    for event in events {
        match event.kind.as_str() {
            "tool" => lines.push(parse_tool_event(&event.text)),
            "text" => text = event.text.as_str(),
            "status" if event.text == "error" => errored = true,
            // "thinking" is intentionally ignored — show actions, not a brain-dump.
            _ => {}
        }
    }
    let mut block = String::new();
    for (key, detail) in lines.iter().rev().take(8).rev() {
        let (emoji, title) = tool_display(key);
        let detail = collapse_ws(detail.trim());
        let line = if key == "bash" || key == "exec" {
            // Shell commands: just the icon + command (OpenClaw drops the label).
            if detail.is_empty() {
                format!("{emoji} {title}")
            } else {
                format!("{emoji} {detail}")
            }
        } else if detail.is_empty() {
            format!("{emoji} {title}")
        } else {
            format!("{emoji} {title}: {detail}")
        };
        block.push_str(&truncate_chars(&line, 160));
        block.push('\n');
    }
    let block = block.trim_end();
    if !block.is_empty() {
        // Whole block in monospace — the "different font" the live progress reads in.
        truncate_for_telegram(&format!("<pre>{}</pre>", html_escape(block)))
    } else if !text.trim().is_empty() {
        html_escape(&truncate_chars(text.trim(), 3000))
    } else if errored {
        "<pre>error</pre>".to_string()
    } else {
        String::new()
    }
}

/// The live progress message id for a turn is recorded in a marker file so that
/// whichever path delivers the reply (the streaming loop or the 1s delivery
/// thread) can edit that exact message into the final answer — one clean message,
/// no orphan, no duplicate.
fn telegram_status_path(paths: &SessionPaths, inbound_id: &str) -> PathBuf {
    let safe: String = inbound_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    paths.dir.join("progress").join(format!("{safe}.tgstatus"))
}

fn set_telegram_status(paths: &SessionPaths, inbound_id: &str, message_id: i64) {
    let path = telegram_status_path(paths, inbound_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, message_id.to_string());
}

/// Read the live message id for a turn WITHOUT removing the marker, so a delivery
/// attempt that fails can be retried against the same live message.
fn peek_telegram_status(paths: &SessionPaths, inbound_id: &str) -> Option<i64> {
    let path = telegram_status_path(paths, inbound_id);
    std::fs::read_to_string(&path).ok()?.trim().parse::<i64>().ok()
}

/// Drop the live-message marker once the turn is finalized (delivered or
/// intentionally silenced), so no later pass touches that message again.
fn clear_telegram_status(paths: &SessionPaths, inbound_id: &str) {
    let _ = std::fs::remove_file(telegram_status_path(paths, inbound_id));
}

/// An "active streamer" lock for a turn, distinct from the message-id marker.
/// The marker only appears once the live bubble has been SENT (≥2s in, and later
/// still if the first send is retried/backed-off), which left an early window
/// where the backstop saw no marker and delivered the reply as a fresh duplicate
/// while a late bubble lingered as a never-ending counter. This lock is written
/// at loop ENTRY (before any send) and cleared on EVERY exit (RAII), so the
/// backstop reliably defers to the streamer for the whole turn — the streamer is
/// the sole deliverer. A crashed streamer leaves the lock behind, but the
/// backstop's `STREAM_BACKSTOP_AGE` gate still takes over once the reply ages out.
fn telegram_active_path(paths: &SessionPaths, inbound_id: &str) -> PathBuf {
    let safe: String = inbound_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    paths.dir.join("progress").join(format!("{safe}.tgactive"))
}

fn set_telegram_active(paths: &SessionPaths, inbound_id: &str) {
    let path = telegram_active_path(paths, inbound_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"1");
}

fn clear_telegram_active(paths: &SessionPaths, inbound_id: &str) {
    let _ = std::fs::remove_file(telegram_active_path(paths, inbound_id));
}

fn telegram_active_exists(paths: &SessionPaths, inbound_id: &str) -> bool {
    telegram_active_path(paths, inbound_id).exists()
}

/// Turn the live progress message into the final reply as exactly ONE message:
/// edit it in place; if the edit fails, delete the stale live message and only
/// then send a fresh copy — but if that delete cannot be confirmed, return `Err`
/// WITHOUT sending, so a failed delete can never leave two messages in the chat.
/// On `Err` the caller releases the claim and a later pass retries.
/// Finalize a turn into its reply as ONE message: edit the live "Thinking…" bubble
/// in place into the answer (exactly-once, can never duplicate or orphan). If the
/// edit fails, delete the stale bubble first and only then send a fresh message —
/// so a failed edit still can't leave two messages. With no live bubble, just send.
///
/// (The native delete-dissolve was attempted by sending the answer as a separate
/// message and deleting the bubble, but that destabilized delivery — duplicate
/// bubble + a counter that kept ticking — so it was reverted to this reliable
/// in-place finish. See the channel notes / [[maturana-telegram-swoosh]].)
fn finalize_reply(
    token: &str,
    chat_id: i64,
    live_id: Option<i64>,
    reply: &str,
    reply_to: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    match live_id {
        Some(id) => match edit_telegram_message(token, chat_id, id, reply) {
            Ok(()) => Ok(Some(id)),
            Err(edit_err) => {
                delete_telegram_message(token, chat_id, id).map_err(|del_err| {
                    anyhow::anyhow!(
                        "edit failed ({edit_err}); refusing to send a duplicate because the stale live message could not be removed ({del_err})"
                    )
                })?;
                send_telegram(token, &chat_id.to_string(), reply, reply_to)
            }
        },
        None => send_telegram(token, &chat_id.to_string(), reply, reply_to),
    }
}

/// OpenClaw-style live progress for a turn: post ONE message immediately, tick a
/// spinner on it while the agent works (showing the tools it runs + streamed
/// text), then — the instant the reply lands — edit that SAME message in place
/// into the final answer. The progress animation literally becomes the reply, so
/// there is exactly one message per turn: it cannot orphan a status message and
/// cannot duplicate the reply.
///
/// Delivery is still gated by `claim_delivery` (the atomic single-send gate), and
/// the message id is recorded in a marker so that if the 1s delivery thread wins
/// the claim instead of this loop, it edits the very same message. Whoever wins
/// the claim turns the live message into the answer; the loser just exits.
fn stream_turn_to_telegram(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    inbound_id: &str,
    reply_to: Option<i64>,
    paths: &SessionPaths,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    // The single live progress draft for this turn, created lazily once there's
    // something to show. No spinner — liveness is the native Telegram "typing…"
    // action plus the rich tool lines appearing as work happens (OpenClaw style).
    let mut message_id: Option<i64> = None;
    // Track exactly what the draft currently shows so a real change is never skipped
    // as a no-op; `last_edit_ok` re-sends after a failed update so a transient outage
    // resolves the instant Telegram recovers.
    let mut last_render = String::new();
    let mut last_edit_ok = true;
    let started = std::time::Instant::now();
    // Live-edit cadence: edit at most every `edit_interval` (starts at base), backing
    // off on stalls/429 so a throttled Telegram doesn't freeze this synchronous loop.
    // `next_edit_at == started` lets the very first draft go out immediately.
    let mut edit_interval = LIVE_EDIT_BASE;
    let mut next_edit_at = started;
    let mut last_typing = started
        .checked_sub(Duration::from_secs(10))
        .unwrap_or(started);
    // Mark this turn as actively streamed BEFORE anything is sent. The backstop
    // delivery thread defers to an active streamer (see `has_pending_stream`), so
    // the streamer is the SOLE deliverer for the whole turn. This closes the
    // window where the backstop saw no live-message marker yet (the first bubble
    // send is ≥2s in, and later still if it is retried/backed-off) and delivered
    // the reply as a fresh duplicate while the late bubble lingered as a counter.
    set_telegram_active(paths, inbound_id);
    // Reply detection runs on a BACKGROUND thread, NOT in this loop, so the only
    // thing pacing the counter is the ~1s editMessageText. The watcher detects the
    // reply by EXISTENCE (`find_reply_outbound`, a cheap `WHERE in_reply_to=?`),
    // NOT by undelivered status — so even if some pass delivered it first, the loop
    // still finds it and cleans up instead of ticking forever. The loop still
    // claims + finalizes, so there is no double-delivery.
    let (reply_tx, reply_rx) = mpsc::channel();
    let watcher_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let paths = paths.clone();
        let inbound_id = inbound_id.to_string();
        let stop = watcher_stop.clone();
        thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match find_reply_outbound(&paths, &inbound_id) {
                    Ok(Some(m)) if m.channel == "telegram" => {
                        let _ = reply_tx.send(m);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("telegram: reply watcher query failed (retrying): {error:#}")
                    }
                }
                thread::sleep(Duration::from_millis(1000));
            }
        });
    }
    // Stop the watcher AND release the active-streamer lock on EVERY return path
    // (including `?` early-returns) so a finished turn isn't still being polled and
    // the backstop can take over immediately if we exited without delivering.
    struct TurnGuard<'a> {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        paths: &'a SessionPaths,
        inbound_id: &'a str,
    }
    impl Drop for TurnGuard<'_> {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            clear_telegram_active(self.paths, self.inbound_id);
        }
    }
    let _turn_guard = TurnGuard {
        stop: watcher_stop.clone(),
        paths,
        inbound_id,
    };
    loop {
        // Reply ready? A cheap, non-blocking check of the background watcher's
        // channel — never the multi-second DB read, so the counter is never stalled.
        let final_msg = reply_rx.try_recv().ok();
        if let Some(final_msg) = final_msg {
            // Lost the claim → someone else already delivered this reply (a backstop
            // pass that ran before our live bubble existed, or a prior streamer).
            // Our live "Thinking…" bubble is now an orphan that would linger as a
            // duplicate message + a counter that never stops — delete it so the chat
            // shows exactly the one delivered answer, then exit.
            if !claim_delivery(paths, &final_msg.id)? {
                if let Some(id) = message_id {
                    let _ = delete_telegram_message(token, chat_id, id);
                }
                clear_telegram_status(paths, inbound_id);
                let _ = clear_progress(paths, inbound_id);
                return Ok(());
            }
            // Won the claim: this turn's live message (our local id == the marker)
            // is ours to finalize. Drop an unparseable outbound rather than spinning
            // on it forever (the parse error is deterministic).
            let reply = match message_text(&final_msg.content) {
                Ok(text) => {
                    truncate_for_telegram(&finalize_onboarding_reply(home, &config.agent_id, &text))
                }
                Err(error) => {
                    eprintln!(
                        "telegram: dropping unparseable outbound {}: {error:#}",
                        final_msg.id
                    );
                    clear_telegram_status(paths, inbound_id);
                    let _ = mark_delivered(paths, &final_msg.id, None);
                    let _ = clear_progress(paths, inbound_id);
                    return Ok(());
                }
            };
            // A turn that decided there's nothing worth saying emits the silence
            // sentinel: remove the live message (best-effort) and finalize regardless
            // — a failed delete must never wedge the turn into an endless retry.
            if reply.trim() == crate::proactive::SILENCE_SENTINEL {
                if let Some(id) = message_id {
                    // Nothing worth saying — delete the thinking bubble, which
                    // plays Telegram's native particle-dissolve on it.
                    let _ = delete_telegram_message(token, chat_id, id);
                }
                clear_telegram_status(paths, inbound_id);
                let _ = mark_delivered(paths, &final_msg.id, None);
                let _ = clear_progress(paths, inbound_id);
                return Ok(());
            }
            // Deliver the answer as its own message and dissolve the thinking
            // bubble (finalize_reply sends, then deletes the live message → the
            // native Telegram particle-dissolve the user asked for).
            match finalize_reply(token, chat_id, message_id, &reply, reply_to) {
                Ok(platform_id) => {
                    // Finalize the claim (the row already exists from claim_delivery,
                    // so a mark_delivered hiccup can't re-open it for a duplicate).
                    let _ = mark_delivered(
                        paths,
                        &final_msg.id,
                        platform_id.map(|id| id.to_string()).as_deref(),
                    );
                    clear_telegram_status(paths, inbound_id);
                    let _ =
                        append_channel_turn(home, &config.agent_id, chat_id, "assistant", &reply);
                    maybe_send_tts(home, token, &config.agent_id, chat_id, &reply, reply_to);
                    let _ = clear_progress(paths, inbound_id);
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.telegram.outbound",
                        "sent telegram response",
                    );
                }
                Err(error) => {
                    // Could not deliver without risking a duplicate: release the
                    // claim (marker left intact) so the backstop retries the same
                    // live message instead of dropping the reply forever.
                    eprintln!("telegram delivery failed, will retry: {error:#}");
                    unclaim_delivery(paths, &final_msg.id)?;
                }
            }
            return Ok(());
        }
        // No reply yet. Keep the native "typing…" indicator alive (it expires after
        // ~5s) so the chat shows liveness even before the first tool line — no spinner.
        if last_typing.elapsed() >= Duration::from_secs(4) {
            let _ = send_telegram_chat_action(token, &chat_id.to_string(), "typing");
            last_typing = std::time::Instant::now();
        }
        // Render the rich tool/thinking draft, plus an always-advancing elapsed clock
        // so the user is NEVER staring at a frozen message. The clock value is computed
        // every iteration but only PUSHED on the flood-safe cadence below (~2.5s), so
        // it advances in small steps instead of jumping in 10s+ blocks (the old
        // bucketed bug) or hammering Telegram into throttling us (the freeze).
        let t_prog = std::time::Instant::now();
        let progress = render_progress_html(&read_progress(paths, inbound_id).unwrap_or_default());
        let prog_ms = t_prog.elapsed().as_millis();
        let secs = started.elapsed().as_secs();
        let clock = format!("{}:{:02}", secs / 60, secs % 60);
        let rendered = if progress.is_empty() {
            if started.elapsed() >= Duration::from_secs(2) {
                format!("<pre>💭 Thinking… {clock}</pre>")
            } else {
                String::new()
            }
        } else {
            format!("{progress}\n<pre>⏳ {clock}</pre>")
        };
        // Push updates on a flood-safe cadence — NOT every loop iteration. We edit
        // only when due (>= next_edit_at) and the content actually changed (or the
        // last update failed). On success we reset to the base interval; on a stall
        // or 429 we back off (honoring `retry_after`) so we stop hammering a Telegram
        // that is already throttling. The ready-reply check above still runs every
        // ~900ms, so the answer is never delayed by this cadence. Advance last_render
        // only on a landed update so a failure can't desync us.
        let due = std::time::Instant::now() >= next_edit_at;
        let t_edit = std::time::Instant::now();
        if !rendered.is_empty() && due && (rendered != last_render || !last_edit_ok) {
            match message_id {
                Some(id) => match edit_telegram_live_html(token, chat_id, id, &rendered) {
                    LiveEditOutcome::Ok => {
                        last_render = rendered;
                        last_edit_ok = true;
                        edit_interval = LIVE_EDIT_BASE;
                    }
                    LiveEditOutcome::Throttled(retry_after) => {
                        if last_edit_ok {
                            eprintln!(
                                "telegram: live progress throttled (429, retry_after={retry_after:?}); backing off"
                            );
                        }
                        last_edit_ok = false;
                        edit_interval = retry_after
                            .map(Duration::from_secs)
                            .unwrap_or(edit_interval * 2)
                            .clamp(LIVE_EDIT_BASE, LIVE_EDIT_MAX);
                    }
                    LiveEditOutcome::Failed(error) => {
                        if last_edit_ok {
                            eprintln!(
                                "telegram: live progress update failing, will keep retrying: {error}"
                            );
                        }
                        last_edit_ok = false;
                        edit_interval = (edit_interval * 2).min(LIVE_EDIT_MAX);
                    }
                },
                None => {
                    message_id =
                        send_telegram_html(token, &chat_id.to_string(), &rendered, reply_to)
                            .unwrap_or(None);
                    if let Some(id) = message_id {
                        set_telegram_status(paths, inbound_id, id);
                        last_render = rendered;
                        last_edit_ok = true;
                        edit_interval = LIVE_EDIT_BASE;
                    } else {
                        last_edit_ok = false;
                        edit_interval = (edit_interval * 2).min(LIVE_EDIT_MAX);
                    }
                }
            }
            // Schedule the next edit from when THIS one STARTED (t_edit), not from
            // now (after it). editMessageText takes ~1s, so `now() + interval`
            // double-counted the edit duration → effective 2s ticks. Start-to-start
            // means the cadence is max(interval, edit_duration) ≈ 1s, so the clock
            // advances ~1s/step like a stopwatch.
            next_edit_at = t_edit + edit_interval;
        }
        let edit_ms = t_edit.elapsed().as_millis();
        // Surface any per-step stall (>800ms) so a jumpy/frozen clock is explained
        // in the journal instead of being invisible. The DB read is off this loop
        // now (background watcher), so the only blocking step left is the edit.
        if prog_ms > 800 || edit_ms > 800 {
            eprintln!("telegram loop slow @ {clock}: progress={prog_ms}ms edit={edit_ms}ms");
        }
        if std::time::Instant::now() >= deadline {
            // Gave up waiting; leave the (undelivered) reply to the delivery thread,
            // which will edit this same live message via the marker.
            return Ok(());
        }
        // Sleep only until the next edit is due — not a fixed poll. When an edit
        // just landed the next is due ~immediately (the ~1s edit IS the cadence),
        // so the counter ticks ~1s; clamped to [100ms, 500ms] so we never busy-spin
        // and the background reply is still picked up promptly.
        let nap = next_edit_at
            .saturating_duration_since(std::time::Instant::now())
            .clamp(Duration::from_millis(100), Duration::from_millis(500));
        thread::sleep(nap);
    }
}

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
    edit_telegram_message_with(token, chat_id, message_id, text, None)
}

fn edit_telegram_message_with(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> anyhow::Result<()> {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    let response: TelegramOkResponse = ureq::post(&format!(
        "{}/bot{token}/editMessageText",
        tg_api_base()
    ))
    .timeout(TG_HTTP_TIMEOUT)
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

/// Delete an ephemeral message (the live progress status) once the real reply is
/// ready, so the channel ends up showing only the clean answer — OpenClaw-style.
fn delete_telegram_message(token: &str, chat_id: i64, message_id: i64) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
    });
    let response: TelegramOkResponse = ureq::post(&format!(
        "{}/bot{token}/deleteMessage",
        tg_api_base()
    ))
    .timeout(TG_HTTP_TIMEOUT)
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram deleteMessage failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram deleteMessage response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram deleteMessage returned ok=false");
    }
    Ok(())
}

pub(crate) fn build_channel_prompt(
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
    // The wiki was removed as a per-turn knowledge source — the graph is the
    // single store. These query terms now drive only the GraphRAG lookup below.
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
        wiki_query_terms,
        wiki_term_sources: wiki_query.term_sources,
        graph_context,
        learned_examples,
        self_forge: AgentSpec::from_maturana_markdown(agent_dir.join("MATURANA.md"))
            .map(|spec| spec.capabilities.self_forge)
            .unwrap_or(false),
        onboarding_active: is_onboarding_active(home, agent_id),
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

/// Awareness block injected when the agent is granted `self_forge`: tells it the
/// WebAssembly runtime exists, that it is allowed to build, and exactly how.
fn forge_prompt_section() -> &'static str {
    r#"
## Self-Forge — build and run a capability on the fly
You are allowed to extend yourself at runtime. When a task needs computation or
transformation you don't already have, author a small WebAssembly capability and
run it immediately, the same turn, in a sandbox — no host rebuild. Use the
`maturana-forge` shell helper:

```
maturana-forge <name> --input '{"n": 7}' <<'WAT'
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; ... compute, then write the result to stdout (fd 1) via fd_write ...
  (func (export "_start") ...))
WAT
```

It assembles your WAT, runs the module under a fuel/memory/timeout sandbox (no
ambient filesystem or network unless you declare it), and returns the module's
stdout. Submit a precompiled module with `--wasm <base64>` instead of heredoc
WAT. The channel shows a 🔨 Building / ⚙️ Running animation while it happens.
Forge sparingly and only when it helps; then describe in your reply what you
built and what it returned.
"#
}

fn render_channel_prompt(context: &ChannelContextBundle, user_message: &str) -> String {
    let forge_section = if context.self_forge {
        forge_prompt_section()
    } else {
        ""
    };
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
    // While onboarding, drive the interview forward EVERY turn — without this the
    // agent answers once and goes quiet instead of asking the next question.
    let onboarding_section = if context.onboarding_active {
        "\n## Onboarding in progress — KEEP THE INTERVIEW GOING\n\
         You are still in your first-run onboarding interview with your owner. This \
         is a short, warm conversation, not a one-off Q&A. After briefly acknowledging \
         what they just told you, ASK THE NEXT THING you don't yet know — one question \
         at a time — until you have learned ALL of: their name and how they'd like to \
         be addressed; their timezone / working hours; and the main things they want \
         your help with. Save durable facts to memory and fill IDENTITY.md's \"Who you \
         are to me\" section as you learn them. Until you have all of that, your reply \
         MUST end with the next question. Only when you genuinely have everything, give \
         a short warm wrap-up and put [[ONBOARDING_COMPLETE]] on its own final line (it \
         is removed before the message is sent).\n"
    } else {
        ""
    };
    format!(
        r#"You are a Maturana personal agent running inside an isolated VM.

Answer the current Telegram message directly and conversationally.
Use the durable memory and recent channel transcript for continuity.
Do not say you cannot remember earlier messages if the transcript contains them.
If the user asks you to remember something, acknowledge it briefly; the host has already stored the raw user memory note.
Return only the message that should be sent back to Telegram.
{onboarding_section}
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
{graph_section}{learned_section}{forge_section}
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
    ];
    let graph_context_chars = context
        .graph_context
        .as_ref()
        .map(|graph| graph.rendered.chars().count())
        .unwrap_or(0);
    let loaded_context_chars = source_files.iter().map(|file| file.chars).sum::<usize>()
        + graph_context_chars
        + context.transcript.chars().count();
    let manifest = ChannelContextManifest {
        at: Utc::now(),
        agent_id: agent_id.to_string(),
        chat_id,
        source_files,
        wiki_query_terms: context.wiki_query_terms.clone(),
        wiki_term_sources: context.wiki_term_sources.clone(),
        graph_name: context
            .graph_context
            .as_ref()
            .map(|graph| graph.graph.clone()),
        graph_context_chars,
        context_policy: ContextPolicySummary {
            strategy: "durable-files-plus-current-message-and-recent-transcript-graph-terms"
                .to_string(),
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

/// Truncate an on-disk channel transcript to its most recent `keep_chars` (the
/// same tail the per-turn context uses), returning the number of bytes freed.
/// The live context is always the recent tail, so this bounds disk growth
/// without dropping anything the agent would have seen this turn.
fn compact_transcript_file(path: &Path, keep_chars: usize) -> anyhow::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let contents = fs::read_to_string(path)?;
    let char_count = contents.chars().count();
    if char_count <= keep_chars {
        return Ok(0);
    }
    let before = contents.len();
    let tail: String = contents
        .chars()
        .skip(char_count.saturating_sub(keep_chars))
        .collect();
    // Align to the next line boundary so we never slice a transcript line in half.
    let trimmed = match tail.find('\n') {
        Some(idx) => &tail[idx + 1..],
        None => tail.as_str(),
    };
    let new_contents = format!("[older transcript compacted]\n{trimmed}");
    fs::write(path, &new_contents)?;
    Ok(before.saturating_sub(new_contents.len()))
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

// ---- Discord: full two-way channel via the Gateway (WS) + REST API ----

const DISCORD_API: &str = "https://discord.com/api/v10";
// GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT. MESSAGE_CONTENT
// is privileged and must be enabled in the Discord Developer Portal.
const DISCORD_INTENTS: u64 = 1 | (1 << 9) | (1 << 12) | (1 << 15);

fn serve_discord(home: &MaturanaHome, config: DiscordServe) -> anyhow::Result<()> {
    let token = resolve_secret_source_with_home(&config.bot_token_source, home.root())?;
    let token = token.expose_for_runtime().to_string();
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!("discord channel serving agent {}", config.agent_id);
    loop {
        if let Err(error) = discord_gateway_session(home, &config, &token, &paths) {
            eprintln!("discord gateway error: {error}");
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

/// Connect the Discord Gateway, IDENTIFY, heartbeat on schedule, and turn
/// MESSAGE_CREATE events into agent prompts (replying via REST) until the socket
/// drops; the outer loop reconnects.
fn discord_gateway_session(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    paths: &SessionPaths,
) -> anyhow::Result<()> {
    let (mut socket, _) = tungstenite::connect("wss://gateway.discord.gg/?v=10&encoding=json")
        .map_err(|e| anyhow::anyhow!("discord gateway connect failed: {e}"))?;
    // Short read timeout so the loop wakes to send heartbeats even when idle.
    discord_set_read_timeout(&mut socket, Duration::from_millis(1000));

    let mut heartbeat_interval = Duration::from_secs(41);
    let mut last_heartbeat = std::time::Instant::now();
    let mut last_seq: Option<i64> = None;
    let mut identified = false;
    let mut self_id: Option<String> = None;
    // The guest worker answers asynchronously, so a reply lands in the outbox
    // seconds AFTER this loop enqueued the prompt. Without a periodic flush the
    // reply sits undelivered until the NEXT inbound message arrives — the
    // "Discord is painfully laggy" symptom (Telegram avoids it with a 1s delivery
    // thread; Discord had none). Track the active channel and flush every ~1s.
    let mut last_channel: Option<String> = None;
    let mut last_flush = std::time::Instant::now();

    loop {
        // Deliver replies the guest finished since the last tick, so a turn's
        // answer reaches Discord within ~1s instead of waiting for another message.
        if last_flush.elapsed() >= Duration::from_millis(1000) {
            if let Some(chan) = last_channel.clone() {
                let _ = deliver_discord_outbox(
                    home,
                    &config.agent_id,
                    &config.session_id,
                    bot_token,
                    &chan,
                );
            }
            last_flush = std::time::Instant::now();
        }
        if last_heartbeat.elapsed() >= heartbeat_interval {
            let hb = serde_json::json!({ "op": 1, "d": last_seq }).to_string();
            socket
                .send(tungstenite::Message::Text(hb))
                .map_err(|e| anyhow::anyhow!("discord heartbeat send: {e}"))?;
            last_heartbeat = std::time::Instant::now();
        }

        let msg = match socket.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("discord read: {e}")),
        };
        let text = match msg {
            tungstenite::Message::Text(t) => t,
            tungstenite::Message::Close(_) => {
                return Err(anyhow::anyhow!("discord gateway closed"))
            }
            _ => continue,
        };
        let event: serde_json::Value = serde_json::from_str(&text)?;
        let op = event.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);
        if let Some(s) = event.get("s").and_then(|v| v.as_i64()) {
            last_seq = Some(s);
        }
        match op {
            10 => {
                if let Some(ms) = event
                    .pointer("/d/heartbeat_interval")
                    .and_then(|v| v.as_u64())
                {
                    heartbeat_interval = Duration::from_millis(ms);
                }
                last_heartbeat = std::time::Instant::now();
                if !identified {
                    let identify = serde_json::json!({
                        "op": 2,
                        "d": {
                            "token": bot_token,
                            "intents": DISCORD_INTENTS,
                            "properties": { "os": "linux", "browser": "maturana", "device": "maturana" }
                        }
                    })
                    .to_string();
                    socket
                        .send(tungstenite::Message::Text(identify))
                        .map_err(|e| anyhow::anyhow!("discord identify send: {e}"))?;
                    identified = true;
                }
            }
            1 => {
                let hb = serde_json::json!({ "op": 1, "d": last_seq }).to_string();
                let _ = socket.send(tungstenite::Message::Text(hb));
                last_heartbeat = std::time::Instant::now();
            }
            11 => { /* heartbeat ACK */ }
            7 | 9 => {
                return Err(anyhow::anyhow!(
                    "discord gateway requested reconnect (op {op})"
                ))
            }
            0 => {
                let t = event.get("t").and_then(|v| v.as_str()).unwrap_or("");
                match t {
                    "READY" => {
                        self_id = event
                            .pointer("/d/user/id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                    }
                    "MESSAGE_CREATE" => {
                        if let Some((channel_id, content, attachments)) =
                            discord_extract_message(&event, self_id.as_deref())
                        {
                            // Remember where to flush async replies (the 1s loop above)
                            // AND persist it so host-side delivery (a finished board
                            // card) can reach the same Discord channel.
                            last_channel = Some(channel_id.clone());
                            remember_discord_channel(home, &config.agent_id, &channel_id);
                            // Inbound files: download + ingest each attachment so a
                            // file-only message isn't dropped (parity with Telegram).
                            if !attachments.is_empty() {
                                handle_discord_attachments(
                                    home,
                                    config,
                                    bot_token,
                                    &channel_id,
                                    &attachments,
                                    &content,
                                );
                            }
                            if !content.is_empty() {
                            let token = bot_token.to_string();
                            let chan = channel_id.clone();
                            // Slash commands share the TUI/Telegram dispatcher so
                            // Discord exposes the same command set. A command is
                            // either a text reply (posted here) or a turn
                            // (enqueued like a normal message below).
                            let turn_text: Option<String> = if content
                                .trim_start()
                                .starts_with('/')
                            {
                                // Bare selector commands (/model etc.) render native
                                // Discord buttons; the click comes back via
                                // INTERACTION_CREATE and applies the same way Telegram does.
                                let trimmed = content.trim();
                                let (head, sel_args) = trimmed
                                    .split_once(char::is_whitespace)
                                    .unwrap_or((trimmed, ""));
                                let cmd = head
                                    .trim_start_matches('/')
                                    .replace('_', "-")
                                    .to_ascii_lowercase();
                                let is_selector = matches!(
                                    cmd.as_str(),
                                    "model" | "models" | "reasoning" | "tts-provider" | "session"
                                );
                                if is_selector && sel_args.trim().is_empty() {
                                    match command_selector_buttons(home, &config.agent_id, &cmd) {
                                        Some((prompt, buttons, cols)) => {
                                            let _ = discord_post_message_with_buttons(
                                                &token, &chan, &prompt, &buttons, cols,
                                            );
                                        }
                                        None => {
                                            let _ = discord_post_message(
                                                &token,
                                                &chan,
                                                "No options available for that command.",
                                            );
                                        }
                                    }
                                    None
                                } else {
                                    match dispatch_slash_command(
                                        home,
                                        &config.agent_id,
                                        &config.session_id,
                                        stable_chat_key(&channel_id),
                                        "discord",
                                        &channel_id,
                                        &content,
                                    ) {
                                        ConsoleCommand::Reply(text) => {
                                            let _ = discord_post_message(&token, &chan, &text);
                                            None
                                        }
                                        ConsoleCommand::Prompt(text) => Some(text),
                                        ConsoleCommand::NewSession => {
                                            let _ = discord_post_message(
                                                &token,
                                                &chan,
                                                "New session started.",
                                            );
                                            None
                                        }
                                        ConsoleCommand::Clear => {
                                            let _ = discord_post_message(&token, &chan, "Cleared.");
                                            None
                                        }
                                        ConsoleCommand::Quit => {
                                            let _ = discord_post_message(
                                                &token,
                                                &chan,
                                                "`/quit` is console-only.",
                                            );
                                            None
                                        }
                                        // Selectors are handled above; defensive text fallback.
                                        ConsoleCommand::Select { title, options } => {
                                            let mut msg = title
                                                .lines()
                                                .next()
                                                .unwrap_or("Options")
                                                .to_string();
                                            for opt in &options {
                                                msg.push_str(&format!("\n• {}", opt.label));
                                            }
                                            let _ = discord_post_message(&token, &chan, &msg);
                                            None
                                        }
                                    }
                                }
                            } else {
                                Some(content)
                            };
                            if let Some(text) = turn_text {
                                enqueue_channel_prompt(
                                    home,
                                    &config.agent_id,
                                    &config.session_id,
                                    "discord",
                                    &channel_id,
                                    None,
                                    &text,
                                )?;
                                if let Some(provider) = &config.run_once_provider {
                                    let options = RunnerOptions {
                                        provider: provider.to_string(),
                                    };
                                    run_session_once(paths, &options, 20)?;
                                }
                                deliver_discord_outbox(
                                    home,
                                    &config.agent_id,
                                    &config.session_id,
                                    bot_token,
                                    &channel_id,
                                )?;
                            }
                            } // end: content non-empty (file-only messages skip the turn)
                        }
                    }
                    "INTERACTION_CREATE" => {
                        // A button tap on a /model-style picker (type 3 =
                        // MESSAGE_COMPONENT). The custom_id carries our
                        // `<action>:<value>`; apply it via the SAME path Telegram
                        // uses, then update the message to the confirmation.
                        let d = event.pointer("/d");
                        let itype = d
                            .and_then(|d| d.get("type"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        if itype == 3 {
                            let iid = d
                                .and_then(|d| d.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let itok = d
                                .and_then(|d| d.get("token"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let custom_id = d
                                .and_then(|d| d.pointer("/data/custom_id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !iid.is_empty() && !itok.is_empty() && !custom_id.is_empty() {
                                let confirm =
                                    apply_channel_selection(home, &config.agent_id, custom_id);
                                let _ = discord_interaction_callback(iid, itok, &confirm);
                                let _ = audit_channel_event(
                                    home,
                                    &config.agent_id,
                                    "channel.discord.callback",
                                    custom_id,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Set a read timeout on the gateway socket so the heartbeat loop can run even
/// when no events arrive (works for both plaintext and rustls streams).
fn discord_set_read_timeout(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    dur: Duration,
) {
    match socket.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(dur));
        }
        tungstenite::stream::MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(dur));
        }
        _ => {}
    }
}

/// Pull (channel_id, content, attachments) from a MESSAGE_CREATE event; skip
/// bot/own messages. Content may be empty (a file-only message) as long as there
/// are attachments — those are still returned so the bridge ingests them instead
/// of dropping the message. Each attachment is (filename, cdn_url).
fn discord_extract_message(
    event: &serde_json::Value,
    self_id: Option<&str>,
) -> Option<(String, String, Vec<(String, String)>)> {
    let d = event.get("d")?;
    if d.pointer("/author/bot").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    if let (Some(self_id), Some(author_id)) =
        (self_id, d.pointer("/author/id").and_then(|v| v.as_str()))
    {
        if self_id == author_id {
            return None;
        }
    }
    let channel_id = d.get("channel_id").and_then(|v| v.as_str())?.to_string();
    let content = d
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let attachments: Vec<(String, String)> = d
        .get("attachments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let url = a.get("url").and_then(|v| v.as_str())?;
                    let name = a
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("attachment");
                    Some((name.to_string(), url.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if content.is_empty() && attachments.is_empty() {
        return None;
    }
    Some((channel_id, strip_discord_mention(&content), attachments))
}

/// Remove a leading `<@id>` / `<@!id>` bot mention so the prompt is clean.
fn strip_discord_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<@") {
        if let Some(close) = rest.find('>') {
            return rest[close + 1..].trim().to_string();
        }
    }
    text.to_string()
}

fn discord_post_message(
    bot_token: &str,
    channel_id: &str,
    text: &str,
) -> anyhow::Result<Option<String>> {
    // Discord caps message content at 2000 characters.
    let content: String = text.chars().take(2000).collect();
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .send_json(serde_json::json!({ "content": content }))
            .map_err(|e| anyhow::anyhow!("discord send message failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

/// Send a Discord message with an inline button keyboard (parity with Telegram's
/// `/model` etc. picker). `buttons` are (label, custom_id) where custom_id is the
/// same `<action>:<value>` callback data; Discord delivers it back via
/// INTERACTION_CREATE on click. Packed into action rows (max 5 buttons/row, 5
/// rows = 25 total) so everything fits.
fn discord_post_message_with_buttons(
    bot_token: &str,
    channel_id: &str,
    content: &str,
    buttons: &[(String, String)],
    columns: usize,
) -> anyhow::Result<Option<String>> {
    let buttons: Vec<&(String, String)> = buttons.iter().take(25).collect();
    let per_row = columns.max(buttons.len().div_ceil(5)).clamp(1, 5);
    let rows: Vec<serde_json::Value> = buttons
        .chunks(per_row)
        .map(|chunk| {
            let comps: Vec<serde_json::Value> = chunk
                .iter()
                .map(|(label, data)| {
                    serde_json::json!({
                        "type": 2,   // button
                        "style": 1,  // primary
                        "label": label.chars().take(80).collect::<String>(),
                        "custom_id": data,
                    })
                })
                .collect();
            serde_json::json!({ "type": 1, "components": comps }) // action row
        })
        .collect();
    let content: String = content.chars().take(2000).collect();
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .send_json(serde_json::json!({ "content": content, "components": rows }))
            .map_err(|e| anyhow::anyhow!("discord send buttons failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

/// Discord's upload limit for a standard (non-boosted) server.
const MAX_DISCORD_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// Post a message with one or more file attachments via multipart/form-data
/// (`payload_json` + `files[n]`) — the real upload, not just the file names.
/// Oversized/unreadable files are skipped; errors if none could be attached so
/// the caller can fall back to a text reply.
fn discord_post_message_with_files(
    bot_token: &str,
    channel_id: &str,
    text: &str,
    files: &[String],
) -> anyhow::Result<Option<String>> {
    let boundary = "maturanadiscordfileboundary7e3f";
    let mut body: Vec<u8> = Vec::new();
    let content: String = text.chars().take(2000).collect();
    let payload = serde_json::json!({ "content": content }).to_string();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"content-disposition: form-data; name=\"payload_json\"\r\ncontent-type: application/json\r\n\r\n",
    );
    body.extend_from_slice(payload.as_bytes());
    body.extend_from_slice(b"\r\n");
    let mut attached = 0usize;
    for (i, path) in files.iter().enumerate() {
        let p = Path::new(path);
        match fs::metadata(p) {
            Ok(meta) if meta.len() > MAX_DISCORD_UPLOAD_BYTES => {
                eprintln!("discord: {path} exceeds the 25 MB upload limit, skipping");
                continue;
            }
            Ok(_) => {}
            Err(_) => continue,
        }
        let bytes = match fs::read(p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let filename = p
            .file_name()
            .map(|n| n.to_string_lossy().replace(['"', '\r', '\n'], "_"))
            .unwrap_or_else(|| format!("file{i}"));
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("content-disposition: form-data; name=\"files[{i}]\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(&bytes);
        body.extend_from_slice(b"\r\n");
        attached += 1;
    }
    if attached == 0 {
        anyhow::bail!("no attachable files (all missing or over 25 MB)");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .set("content-type", &format!("multipart/form-data; boundary={boundary}"))
            .send_bytes(&body)
            .map_err(|e| anyhow::anyhow!("discord file upload failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

/// Download a Discord attachment (its CDN url is public — no auth) to `dest`,
/// capping the size so a huge upload can't exhaust memory/disk.
fn discord_download_attachment(url: &str, dest: &Path, max_bytes: u64) -> anyhow::Result<u64> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("discord attachment download failed: {e}"))?;
    let mut reader = resp.into_reader().take(max_bytes + 1);
    let mut bytes: Vec<u8> = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("attachment exceeds {max_bytes} bytes");
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, &bytes)?;
    Ok(bytes.len() as u64)
}

/// Inbound Discord files: download each attachment, ingest supported document
/// types into the agent's knowledge graph (so a VM-isolated guest can query
/// them), save the rest to the inbox, and post one summary reply. Mirrors the
/// Telegram document path so files aren't silently dropped.
fn handle_discord_attachments(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    channel_id: &str,
    attachments: &[(String, String)],
    caption: &str,
) {
    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph = match (
        maturana_core::worker::read_graph_token(home.root()),
        knowledge_graph.enabled,
    ) {
        (Some(token), true) => Some((token, crate::graph::agent_graph_name(&config.agent_id))),
        _ => None,
    };
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    let _ = fs::create_dir_all(&inbox);
    let mut lines: Vec<String> = Vec::new();
    for (name, url) in attachments {
        let file_name = sanitize_document_name(Some(name));
        let dest = inbox.join(format!("{}-{file_name}", Utc::now().format("%Y%m%dT%H%M%SZ")));
        if let Err(error) = discord_download_attachment(url, &dest, MAX_DISCORD_UPLOAD_BYTES) {
            lines.push(format!("• `{file_name}` — download failed: {error}"));
            continue;
        }
        // VISION: an image attachment is delivered into the guest workspace and
        // run as a turn so the vision-capable harness opens and *sees* it (parity
        // with Telegram). Falls through to graph/inbox if the guest is unreachable.
        let ext = file_name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let is_image = matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "heif" | "tif" | "tiff"
        );
        if is_image {
            match crate::deliver_image_to_guest(home, &config.agent_id, &dest) {
                Ok(guest_path) => {
                    let cap = (!caption.trim().is_empty()).then_some(caption);
                    let prompt = crate::vision_prompt_text(cap, &guest_path);
                    let _ = enqueue_turn(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "discord",
                        channel_id,
                        stable_chat_key(channel_id),
                        None,
                        &prompt,
                        serde_json::json!({ "image": guest_path }),
                    );
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.image",
                        &format!("delivered image to guest ({guest_path}); running vision turn"),
                    );
                    lines.push(format!("• `{file_name}` — 👁️ viewing it now"));
                    continue;
                }
                Err(error) => {
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.image_fallback",
                        &format!("guest image delivery failed ({error:#}); falling back"),
                    );
                }
            }
        }
        let supported = file_name
            .rsplit_once('.')
            .map(|(_, ext)| {
                crate::graph::SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str())
            })
            .unwrap_or(false);
        match (&graph, supported) {
            (Some((token, graph_name)), true) => match crate::graph::ingest_file_into_service(
                crate::graph::DEFAULT_LOCAL_URL,
                token,
                graph_name,
                &dest,
                1800,
            ) {
                Ok(chunks) => {
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.document",
                        &format!("ingested {file_name} ({chunks} chunks)"),
                    );
                    lines.push(format!(
                        "• `{file_name}` → added to my knowledge graph ({chunks} chunks)"
                    ));
                }
                Err(error) => lines.push(format!("• `{file_name}` — could not ingest: {error}")),
            },
            _ => lines.push(format!("• `{file_name}` — saved to my inbox")),
        }
    }
    if lines.is_empty() {
        return;
    }
    let trailer = if caption.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n_re: {}_", caption.trim().chars().take(180).collect::<String>())
    };
    let _ = discord_post_message(
        bot_token,
        channel_id,
        &format!("📎 Received:\n{}{trailer}", lines.join("\n")),
    );
}

/// Respond to a Discord component interaction (button click) by editing the
/// picker message into the confirmation and removing the buttons (type 7 =
/// UPDATE_MESSAGE). The interaction token authorizes this — no bot auth header.
fn discord_interaction_callback(
    interaction_id: &str,
    interaction_token: &str,
    content: &str,
) -> anyhow::Result<()> {
    let content: String = content.chars().take(2000).collect();
    ureq::post(&format!(
        "{DISCORD_API}/interactions/{interaction_id}/{interaction_token}/callback"
    ))
    .send_json(serde_json::json!({
        "type": 7, // UPDATE_MESSAGE
        "data": { "content": content, "components": [] },
    }))
    .map_err(|e| anyhow::anyhow!("discord interaction callback failed: {e}"))?;
    Ok(())
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

/// THE single front door every chat surface enqueues through — Telegram, the
/// console TUI, Discord, Slack, AgentMail, and the web cockpit. It does the three
/// things that MUST happen identically on every channel, so none can drift:
///   1. record the user turn in the channel transcript (`chat_key`),
///   2. build the full channel context (transcript + memory + wiki + graph +
///      learned examples) — this is what gives the agent turn-to-turn memory,
///   3. enqueue the turn tagged with the real `channel`/`platform_id` so the
///      reply routes back, carrying the agent's current model + reasoning.
///
/// `chat_key` is the transcript/context key: Telegram passes its raw chat id;
/// other channels pass `stable_chat_key(platform_id)`. `extra` is merged into the
/// content JSON for channel-specific fields (e.g. Telegram's reply-to). Returns
/// the enqueued message id. Adding a new surface? Call THIS — never insert_inbound
/// a chat turn yourself, or it silently loses memory/model/routing.
pub(crate) fn enqueue_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    chat_key: i64,
    thread_id: Option<&str>,
    text: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    append_channel_turn(home, agent_id, chat_key, "user", text)?;
    maybe_remember_user_message(home, agent_id, text)?;
    let prompt = build_channel_prompt(home, agent_id, chat_key, text)?;
    let settings = load_channel_settings(home, agent_id);
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut content = serde_json::json!({
        "text": text,
        "prompt": prompt,
        // Per-turn model + reasoning overrides for EVERY channel (the guest worker
        // passes them to the harness; null => harness default).
        "model": settings.model,
        "reasoning": settings.reasoning,
    });
    if let (Some(obj), serde_json::Value::Object(extra_map)) = (content.as_object_mut(), extra) {
        for (key, value) in extra_map {
            obj.insert(key, value);
        }
    }
    let id = insert_inbound(&paths, "chat", channel, platform_id, thread_id, &content.to_string())?;
    fire_agent_hooks(
        home,
        maturana_core::hooks::HookContext::new(
            maturana_core::hooks::HookEvent::MessageIn,
            agent_id,
        )
        .channel(channel)
        .text(text),
    );
    Ok(id)
}

/// Fire an agent's lifecycle hooks for `ctx`, off the hot path. Loads the
/// agent's spec; if it declares no hooks this is a cheap no-op. Command/webhook
/// actions run on the host (never the guest). The `enqueue-turn` action is routed
/// through [`enqueue_outreach_turn`] (system-initiated) so it can NEVER recurse
/// back into the `message-in` hook fired above.
pub(crate) fn fire_agent_hooks(home: &MaturanaHome, ctx: maturana_core::hooks::HookContext) {
    let spec_path = home.agent_dir(&ctx.agent_id).join("MATURANA.md");
    let spec = match maturana_core::AgentSpec::from_maturana_markdown(&spec_path) {
        Ok(spec) => spec,
        Err(_) => return,
    };
    if spec.hooks.on.is_empty() {
        return;
    }
    let home = home.clone();
    std::thread::spawn(move || {
        let enqueue = |target: &str, prompt: &str| -> anyhow::Result<()> {
            let session = format!("{target}-main");
            match current_paired_telegram_chat_id(&home, target) {
                Some(chat_id) => {
                    enqueue_outreach_turn(
                        &home,
                        target,
                        &session,
                        chat_id,
                        prompt,
                        "hook",
                        serde_json::json!({}),
                    )?;
                    Ok(())
                }
                None => anyhow::bail!(
                    "agent '{target}' has no paired channel to receive a hook-enqueued turn"
                ),
            }
        };
        maturana_core::hooks::fire(&spec, &ctx, Some(&enqueue));
    });
}

/// Enqueue a SYSTEM-initiated turn (proactivity, scheduler) tagged for the
/// agent's real outreach channel (Telegram), so the agent's reply is delivered by
/// the normal outbox loop — the whole point being that a reply tagged `proactive`
/// is filtered out by every channel's delivery loop (`deliver_outbox` matches on
/// channel+platform_id) and can NEVER reach the user.
///
/// Unlike [`enqueue_turn`], it does NOT record the directive as a user turn: a
/// `[PROACTIVE CHECK]` the user never sent must not pollute the visible transcript
/// or seed every future prompt's recent-history context with a fake user message.
/// The agent still gets full context (memory + transcript + graph) so it can
/// decide whether there is anything worth saying; its reply is recorded and
/// delivered like any other channel turn.
pub(crate) fn enqueue_outreach_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    directive: &str,
    kind: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    let prompt = build_channel_prompt(home, agent_id, chat_id, directive)?;
    let settings = load_channel_settings(home, agent_id);
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut content = serde_json::json!({
        "text": directive,
        "prompt": prompt,
        // Same per-turn model/reasoning overrides as a normal channel turn.
        "model": settings.model,
        "reasoning": settings.reasoning,
    });
    // Caller metadata (e.g. a scheduler's schedule_id/name) merged into the payload.
    if let (Some(obj), serde_json::Value::Object(extra_map)) = (content.as_object_mut(), extra) {
        for (key, value) in extra_map {
            obj.insert(key, value);
        }
    }
    insert_inbound(
        &paths,
        kind,
        "telegram",
        &chat_id.to_string(),
        None,
        &content.to_string(),
    )
}

/// Enqueue a user message as a chat prompt for a text channel keyed by its string
/// `platform_id` (Discord, Slack, AgentMail). Thin wrapper over [`enqueue_turn`].
pub(crate) fn enqueue_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    enqueue_turn(
        home,
        agent_id,
        session_id,
        channel,
        platform_id,
        stable_chat_key(platform_id),
        thread_id,
        text,
        serde_json::json!({}),
    )?;
    Ok(())
}

/// A handle to one dispatched orchestration step: which session it was queued in
/// and the inbound message id to match its reply by.
pub(crate) struct DispatchHandle {
    pub session_id: String,
    pub message_id: String,
}

/// Enqueue ONE orchestration step to a worker agent and return a handle to poll
/// for its reply. Unlike a chat turn ([`enqueue_turn`]) this:
///   * records NOTHING in the agent's visible transcript (a step is not a user
///     message), so orchestration work never pollutes the agent's chat memory;
///   * sends the already-framed task text verbatim (the role's instruction
///     prefix + the task + any upstream results the loop chose) with NO
///     host-injected transcript — so concurrent runs and live chats can never
///     bleed context into a step;
///   * tags the turn `channel="orchestrate"`, `platform_id=<run_id>` — a channel
///     no live delivery loop serves — so the worker's reply is collected by the
///     orchestrator loop on the host and can NEVER leak into a user's chat.
/// The orchestrator loop charges the turn budget before calling this, so every
/// turn is paid for before it is sent.
/// Prepend the agent's identity + memory to a dispatched task. Without this a
/// board card (or any A2A task) is a context-free turn: it doesn't know who it
/// is, what it can do, or who its operator is — which is why a card asked to
/// "send this to Anders" couldn't tell who Anders was. Bounded, best-effort
/// reads of the same files the channel prompt uses.
fn build_dispatch_prompt(home: &MaturanaHome, agent_id: &str, task: &str) -> String {
    let agent_dir = home.agent_dir(agent_id);
    let head = |rel: &str, cap: usize| -> String {
        std::fs::read_to_string(agent_dir.join(rel))
            .map(|s| s.chars().take(cap).collect::<String>())
            .unwrap_or_default()
    };
    let identity = head("AGENTS.md", 4000);
    let memory = head("memory/MEMORY.md", 4000);
    let mut out = String::new();
    if !identity.trim().is_empty() {
        out.push_str("--- WHO YOU ARE ---\n");
        out.push_str(identity.trim());
        out.push_str("\n\n");
    }
    if !memory.trim().is_empty() {
        out.push_str("--- YOUR MEMORY (operator, contacts, context) ---\n");
        out.push_str(memory.trim());
        out.push_str("\n\n");
    }
    out.push_str("--- TASK ---\n");
    out.push_str(task);
    out
}

pub(crate) fn enqueue_dispatch_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    run_id: &str,
    framed_task: &str,
    model: Option<&str>,
) -> anyhow::Result<DispatchHandle> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let content = serde_json::json!({
        "text": framed_task,
        "prompt": build_dispatch_prompt(home, agent_id, framed_task),
        "model": model,
        "reasoning": serde_json::Value::Null,
    });
    let message_id = insert_inbound(
        &paths,
        "dispatch",
        "orchestrate",
        run_id,
        None,
        &content.to_string(),
    )?;
    Ok(DispatchHandle {
        session_id: session_id.to_string(),
        message_id,
    })
}

/// One NON-blocking poll for a dispatched step's reply. The orchestrator loop
/// calls this for every in-flight step each tick, so one slow step never blocks
/// the others. Returns `Ok(Some(text))` once the worker has replied (the reply
/// row is atomically claimed via `claim_delivery` so no other path can take it,
/// then consumed so it never reaches a chat), `Ok(None)` if not ready yet, or an
/// error only on a real storage failure.
pub(crate) fn try_collect_dispatch(
    home: &MaturanaHome,
    agent_id: &str,
    handle: &DispatchHandle,
) -> anyhow::Result<Option<String>> {
    let paths = session_paths(&home.agent_dir(agent_id), &handle.session_id);
    let Some(message) = list_undelivered(&paths)?
        .into_iter()
        .find(|m| m.in_reply_to.as_deref() == Some(&handle.message_id))
    else {
        return Ok(None);
    };
    // Atomically claim so a stray delivery path can never double-take the row.
    if !claim_delivery(&paths, &message.id)? {
        return Ok(None);
    }
    let text = match message_text(&message.content) {
        Ok(text) => text,
        // An unparseable reply is consumed (not spun on forever) and surfaced as
        // an error string the loop treats as a failed step.
        Err(error) => format!("[unparseable worker reply: {error}]"),
    };
    mark_delivered(&paths, &message.id, Some("orchestrate"))?;
    Ok(Some(text))
}

/// A sink that delegates to a plain send closure — for text channels (Discord,
/// Slack, AgentMail) with no live-message edit or TTS.
struct ClosureSink<'a, F> {
    send: &'a mut F,
}

impl<F> OutboundSink for ClosureSink<'_, F>
where
    F: FnMut(&str, Option<&str>) -> anyhow::Result<Option<String>>,
{
    fn send(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        (self.send)(text, reply_to)
    }
}

/// Deliver undelivered outbound rows for `channel`+`platform_id` using a
/// channel-specific `send` closure (returns the platform message id). A thin
/// wrapper over the shared [`deliver_outbox`] loop — same claiming, silence
/// filter, transcript recording, mark-delivered, audit, and retry-on-failure as
/// Telegram, just a plain send instead of a live-message edit.
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
    let mut sink = ClosureSink { send: &mut send };
    deliver_outbox(home, agent_id, &paths, channel, platform_id, key, None, &mut sink)
}

/// Outbound sink for Discord: posts text, and uploads any host-side files as real
/// attachments (parity with Telegram) instead of just naming them.
struct DiscordSink<'a> {
    bot_token: &'a str,
    channel_id: &'a str,
}

impl OutboundSink for DiscordSink<'_> {
    fn send(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        _reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        discord_post_message(self.bot_token, self.channel_id, text)
    }

    fn send_files(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        _reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        match discord_post_message_with_files(self.bot_token, self.channel_id, text, files) {
            Ok(id) => Ok(id),
            Err(error) => {
                eprintln!("discord: file upload failed ({error:#}); sending text only");
                let names: Vec<String> = files
                    .iter()
                    .filter_map(|f| {
                        Path::new(f).file_name().map(|n| n.to_string_lossy().to_string())
                    })
                    .collect();
                let msg = if text.trim().is_empty() {
                    format!("(couldn't attach: {})", names.join(", "))
                } else {
                    format!("{text}\n(couldn't attach: {})", names.join(", "))
                };
                discord_post_message(self.bot_token, self.channel_id, &msg)
            }
        }
    }
}

/// Deliver pending Discord replies for one channel, uploading any files as real
/// attachments. Used by both the gateway loop's 1s flush and the inline path.
fn deliver_discord_outbox(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    bot_token: &str,
    channel_id: &str,
) -> anyhow::Result<usize> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let key = stable_chat_key(channel_id);
    let mut sink = DiscordSink {
        bot_token,
        channel_id,
    };
    deliver_outbox(home, agent_id, &paths, "discord", channel_id, key, None, &mut sink)
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
    fn discord_extract_message_keeps_file_only_messages() {
        // A message with an attachment but NO text must still be surfaced (with
        // its attachments) instead of dropped — the bug behind "file upload
        // doesn't work via Discord".
        let ev = serde_json::json!({
            "d": {
                "channel_id": "123",
                "content": "",
                "author": { "id": "user1" },
                "attachments": [
                    { "filename": "report.pdf", "url": "https://cdn.discordapp.com/x/report.pdf" }
                ]
            }
        });
        let (chan, content, atts) = discord_extract_message(&ev, Some("bot9")).unwrap();
        assert_eq!(chan, "123");
        assert!(content.is_empty());
        assert_eq!(
            atts,
            vec![(
                "report.pdf".to_string(),
                "https://cdn.discordapp.com/x/report.pdf".to_string()
            )]
        );
        // The bot's own messages are still ignored (no echo loop).
        let own = serde_json::json!({
            "d": { "channel_id": "1", "content": "hi", "author": { "id": "bot9" } }
        });
        assert!(discord_extract_message(&own, Some("bot9")).is_none());
        // A plain text message with no attachments still parses.
        let txt = serde_json::json!({
            "d": { "channel_id": "9", "content": "  hello  ", "author": { "id": "u" } }
        });
        let (_, c, a) = discord_extract_message(&txt, Some("bot9")).unwrap();
        assert_eq!(c, "hello");
        assert!(a.is_empty());
    }

    #[test]
    fn recent_models_are_newest_first_and_filter_non_chat() {
        let m = |id: &str, created: i64, text_output: bool, supports_tools: bool| {
            OpenRouterModel {
                id: id.to_string(),
                created,
                text_output,
                supports_tools,
            }
        };
        // Mixed catalog: chat models of varying age, an image model, a safety
        // classifier, and a newest-but-toolless model (like openrouter/fusion) —
        // none of the last three may appear in a chat picker opencode can use.
        let catalog = vec![
            m("deepseek/deepseek-chat-v3.1", 100, true, true),
            m("google/gemini-3.5-flash", 300, true, true),
            m("anthropic/claude-opus-4.8", 250, true, true),
            m("z-ai/glm-5.2", 280, true, true),
            m("google/gemini-3-pro-image", 999, false, true), // newest, image-only
            m("nvidia/nemotron-3.5-content-safety", 998, true, true), // classifier
            m("openrouter/fusion", 997, true, false), // newest, but no tool support
        ];
        let picked = recent_openrouter_models(&catalog, 3);
        // Newest tool-capable chat models first; the image, safety, and toolless
        // models are excluded even though they have the largest `created` stamps.
        assert_eq!(
            picked,
            vec![
                "google/gemini-3.5-flash".to_string(),
                "z-ai/glm-5.2".to_string(),
                "anthropic/claude-opus-4.8".to_string(),
            ]
        );
        assert!(!picked.iter().any(|id| {
            id.contains("image") || id.contains("content-safety") || id.contains("fusion")
        }));
    }

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
    fn classifies_onboard_as_its_own_action() {
        // /onboard must route to its own action (not the generic Command path,
        // which only returns a text reply) so the onboarding turn is enqueued with
        // THIS chat's routing and the agent's greeting actually comes back.
        // Regression: it used to enqueue with channel/platform_id "onboard" and the
        // greeting was silently dropped.
        let update = text_update(7, "/onboard");
        assert_eq!(
            classify_telegram_update(&update, Some(7), None),
            InboundAction::Onboard { chat_id: 7 }
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
    fn command_menu_underscores_map_to_hyphenated_commands() {
        // Telegram's setMyCommands can't carry hyphens, so the interactive `/`
        // menu sends `/graph_query` and `/tts_provider`; these must classify the
        // same as their canonical hyphenated forms.
        assert_eq!(
            classify_telegram_update(&text_update(7, "/graph_query roadmap q3"), Some(7), None),
            InboundAction::Command {
                chat_id: 7,
                name: "graph-query".to_string(),
                args: "roadmap q3".to_string()
            }
        );
        assert_eq!(
            classify_telegram_update(&text_update(7, "/tts_provider"), Some(7), None),
            InboundAction::Command {
                chat_id: 7,
                name: "tts-provider".to_string(),
                args: String::new()
            }
        );
    }

    #[test]
    fn discord_extracts_prompt_and_skips_bot_and_self() {
        // A real user message: returns (channel_id, content) with the leading
        // bot mention stripped.
        let ev = serde_json::json!({
            "op": 0, "t": "MESSAGE_CREATE",
            "d": { "channel_id": "123", "content": "<@999> hello there",
                   "author": { "id": "42", "bot": false } }
        });
        assert_eq!(
            discord_extract_message(&ev, Some("999")),
            Some(("123".to_string(), "hello there".to_string(), vec![]))
        );
        // Bot-authored message is ignored.
        let bot = serde_json::json!({
            "d": { "channel_id": "1", "content": "hi", "author": { "id": "7", "bot": true } }
        });
        assert_eq!(discord_extract_message(&bot, Some("999")), None);
        // Our own message (author id == self) is ignored (no echo loop).
        let own = serde_json::json!({
            "d": { "channel_id": "1", "content": "hi", "author": { "id": "999" } }
        });
        assert_eq!(discord_extract_message(&own, Some("999")), None);
        // Empty content AND no attachments is ignored.
        let empty = serde_json::json!({
            "d": { "channel_id": "1", "content": "   ", "author": { "id": "42" } }
        });
        assert_eq!(discord_extract_message(&empty, Some("999")), None);
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
    fn codex_models_track_auth_mode() {
        let temp = temp_dir("codex-auth");
        let dir = temp.path();

        // ChatGPT (OAuth) login → only gpt-5.5 is offered (live-verified: every
        // other id 400s on a ChatGPT account).
        fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":"x"}}"#,
        )
        .unwrap();
        assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ChatGpt);
        assert_eq!(codex_models_for_auth(Some(dir)), vec!["gpt-5.5".to_string()]);

        // API-key login → the wider catalog.
        fs::write(
            dir.join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test","tokens":null}"#,
        )
        .unwrap();
        assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ApiKey);
        assert!(codex_models_for_auth(Some(dir)).contains(&"gpt-5".to_string()));

        // No explicit auth_mode → infer from which credential is populated.
        fs::write(
            dir.join("auth.json"),
            r#"{"OPENAI_API_KEY":null,"tokens":{"access_token":"x"}}"#,
        )
        .unwrap();
        assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ChatGpt);

        // The operator's seeded default is unioned in and de-duplicated (and not
        // confused with `model_reasoning_effort`).
        fs::write(
            dir.join("config.toml"),
            "model = \"gpt-6-preview\"\nmodel_reasoning_effort = \"low\"\n[tui]\n",
        )
        .unwrap();
        let models = codex_models_for_auth(Some(dir));
        assert_eq!(models.first().map(String::as_str), Some("gpt-6-preview"));
        assert!(models.contains(&"gpt-5.5".to_string()));

        // Unreadable / unknown auth → the ChatGPT-safe default set.
        assert_eq!(codex_auth_mode_from_dir(temp.path().join("missing").as_path()), CodexAuthMode::Unknown);
        assert_eq!(codex_models_for_auth(None), vec!["gpt-5.5".to_string()]);
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
        append_channel_turn(&home, "agent", 42, "user", "my name is Anders").unwrap();

        let prompt =
            build_channel_prompt(&home, "agent", 42, "what is my name and tea preference?")
                .unwrap();
        assert!(prompt.contains("likes tea"));
        assert!(prompt.contains("my name is Anders"));
        assert!(prompt.contains("what is my name and tea preference?"));
        let manifest_path = channel_context_manifest_path(&home, "agent", 42);
        let manifest: ChannelContextManifest =
            serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.agent_id, "agent");
        assert_eq!(manifest.chat_id, 42);
        assert!(manifest.loaded_context_chars > 0);
        assert!(manifest.wiki_query_terms.contains(&"name".to_string()));
        assert_eq!(
            manifest.context_policy.strategy,
            "durable-files-plus-current-message-and-recent-transcript-graph-terms"
        );
        assert!(manifest.context_policy.excludes_reset_marker);
        // Query-term extraction (now feeding only the graph) still picks up message terms.
        assert!(manifest.wiki_term_sources.iter().any(
            |term| term.term == "tea" && term.sources.contains(&"current_message".to_string())
        ));
        assert!(manifest
            .source_files
            .iter()
            .any(|file| file.label == "memory/MEMORY.md" && !file.missing));
    }

    #[test]
    fn console_turns_feed_the_next_prompt_so_the_tui_has_memory() {
        // Regression: the TUI (agent_chat_turn) sent the bare prompt, so the agent
        // "started fresh" every turn. It now injects the console transcript via
        // build_channel_prompt(console_chat_key()). Record a couple of console
        // turns and assert the next prompt carries them — the exact path the TUI
        // uses, keyed the same way the TUI records (console_chat_key).
        let temp = temp_dir("tui-memory");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
        record_console_turn(&home, "agent", "user", "My name is Anders.").unwrap();
        record_console_turn(&home, "agent", "assistant", "Hi Anders! Nice to meet you.").unwrap();

        let prompt =
            build_channel_prompt(&home, "agent", console_chat_key(), "what's my name?").unwrap();
        assert!(
            prompt.contains("My name is Anders."),
            "prompt is missing the prior user turn → no memory: {prompt}"
        );
        assert!(prompt.contains("Hi Anders!"), "prompt is missing the prior assistant turn");
        assert!(prompt.contains("what's my name?"));
    }

    #[test]
    fn enqueue_turn_is_the_single_front_door() {
        // Every chat surface goes through enqueue_turn. It must, for ANY channel:
        // record the user turn, inject the recent transcript (memory), attach
        // model+reasoning, and enqueue tagged with the real channel/platform_id.
        let temp = temp_dir("front-door");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

        // First turn establishes history; second must see it in its enriched prompt.
        enqueue_turn(&home, "agent", "s", "telegram", "555", 555, None, "remember the blue door", serde_json::json!({"telegram_reply_to": 9})).unwrap();
        let id = enqueue_turn(&home, "agent", "s", "telegram", "555", 555, None, "what did I say?", serde_json::json!({})).unwrap();
        assert!(!id.is_empty());

        let paths = session_paths(&home.agent_dir("agent"), "s");
        let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
        let msg = pending.iter().find(|m| m.id == id).expect("enqueued message present");
        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        let prompt = content["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("remember the blue door"),
            "front door must inject the prior turn (memory): {prompt}"
        );
        assert!(content.get("model").is_some(), "front door must attach model");
        assert!(content.get("reasoning").is_some(), "front door must attach reasoning");
        // The transcript is recorded under the channel chat key (555), not a sentinel.
        let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 555)).unwrap();
        assert!(transcript.contains("remember the blue door"));
        assert!(transcript.contains("what did I say?"));
    }

    #[test]
    fn outreach_turn_is_tagged_for_the_real_chat_and_keeps_the_directive_invisible() {
        // The proactive-outreach bug: a turn tagged "proactive" had its reply
        // filtered out by deliver_outbox (channel mismatch) and never reached the
        // user. enqueue_outreach_turn must tag the turn for the REAL telegram chat
        // so the reply delivers — while NOT recording the system directive as a
        // user turn (which would pollute the transcript + every future prompt).
        let temp = temp_dir("outreach-turn");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

        let id = enqueue_outreach_turn(
            &home,
            "agent",
            "s",
            8566198884,
            "[PROACTIVE CHECK] anything worth saying?",
            "proactive",
            serde_json::json!({}),
        )
        .unwrap();

        let paths = session_paths(&home.agent_dir("agent"), "s");
        let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
        let msg = pending.iter().find(|m| m.id == id).expect("enqueued");
        // Tagged for the real telegram chat => the telegram delivery loop (which
        // matches channel=="telegram" && platform_id==chat_id) WILL pick the reply up.
        assert_eq!(msg.channel, "telegram", "must route via the telegram channel");
        assert_eq!(msg.platform_id, "8566198884", "must target the paired chat id");
        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert!(content.get("prompt").is_some(), "context prompt attached");
        assert!(content.get("model").is_some(), "model override attached");
        assert!(content.get("reasoning").is_some(), "reasoning override attached");

        // The directive must NOT appear as a user turn in the visible transcript.
        let recorded =
            fs::read_to_string(channel_transcript_path(&home, "agent", 8566198884)).unwrap_or_default();
        assert!(
            !recorded.contains("PROACTIVE CHECK"),
            "the system directive must not pollute the transcript as a fake user turn"
        );
    }

    #[test]
    fn deliver_outbox_is_the_single_delivery_loop() {
        // Telegram and the generic Discord/Slack/AgentMail path share this loop.
        // It must: filter by channel/platform, drop the silence sentinel to
        // on_silence (never send it), record the assistant turn under chat_key,
        // mark delivered, and on a send FAILURE release the claim so the row is
        // retried (not wedged claimed-and-undelivered).
        struct MockSink {
            sent: Vec<String>,
            silenced: usize,
            fail: bool,
        }
        impl OutboundSink for MockSink {
            fn send(
                &mut self,
                _inbound: Option<&str>,
                text: &str,
                _reply: Option<&str>,
            ) -> anyhow::Result<Option<String>> {
                if self.fail {
                    anyhow::bail!("send boom");
                }
                self.sent.push(text.to_string());
                Ok(Some("pmid".to_string()))
            }
            fn on_silence(&mut self, _inbound: Option<&str>) {
                self.silenced += 1;
            }
        }
        use maturana_core::session_db::write_outbound;
        let temp = temp_dir("deliver-outbox");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let paths = session_paths(&home.agent_dir("agent"), "s");
        ensure_session(&paths).unwrap();
        let body = |t: &str| serde_json::json!({ "text": t }).to_string();
        write_outbound(&paths, None, "chat", "tg", "111", None, &body("hello there")).unwrap();
        write_outbound(&paths, None, "chat", "tg", "111", None, &body(crate::proactive::SILENCE_SENTINEL)).unwrap();
        write_outbound(&paths, None, "chat", "other", "111", None, &body("wrong channel")).unwrap();

        let mut sink = MockSink { sent: vec![], silenced: 0, fail: false };
        let delivered =
            deliver_outbox(&home, "agent", &paths, "tg", "111", 111, None, &mut sink).unwrap();
        assert_eq!(delivered, 1, "only the matching non-silence row delivers");
        assert_eq!(sink.sent, vec!["hello there".to_string()]);
        assert_eq!(sink.silenced, 1, "silence sentinel routes to on_silence, never sent");
        let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 111)).unwrap();
        assert!(transcript.contains("hello there"), "assistant turn recorded under chat_key");

        // A failing send must RELEASE the claim so the row is retried next pass.
        write_outbound(&paths, None, "chat", "tg", "111", None, &body("retry me")).unwrap();
        let mut failer = MockSink { sent: vec![], silenced: 0, fail: true };
        let n = deliver_outbox(&home, "agent", &paths, "tg", "111", 111, None, &mut failer).unwrap();
        assert_eq!(n, 0);
        let undelivered = list_undelivered(&paths).unwrap();
        assert!(
            undelivered.iter().any(|m| m.content.contains("retry me")),
            "failed send must release the claim for retry, not wedge it"
        );
    }

    #[test]
    fn deliver_outbox_does_not_make_unstreamed_replies_wait_the_backstop() {
        // The "Paired! …silence" bug: the onboarding greeting (enqueued by pairing,
        // no streamer) was deliverable only by the 6-min backstop. The age gate must
        // ONLY defer a young reply whose streamer might still be live.
        use maturana_core::session_db::write_outbound;
        let backstop = std::time::Duration::from_secs(3600); // huge, like the real one

        struct NoStream(usize);
        impl OutboundSink for NoStream {
            fn send(&mut self, _i: Option<&str>, _t: &str, _r: Option<&str>) -> anyhow::Result<Option<String>> {
                self.0 += 1;
                Ok(None)
            }
            // has_pending_stream defaults false (no streamer).
        }
        struct Streaming(usize);
        impl OutboundSink for Streaming {
            fn send(&mut self, _i: Option<&str>, _t: &str, _r: Option<&str>) -> anyhow::Result<Option<String>> {
                self.0 += 1;
                Ok(None)
            }
            fn has_pending_stream(&self, _i: Option<&str>) -> bool {
                true
            }
        }
        let body = serde_json::json!({ "text": "greeting" }).to_string();

        // No streamer → a brand-new reply delivers immediately despite the backstop.
        let temp = temp_dir("deliver-no-stream");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let paths = session_paths(&home.agent_dir("agent"), "s");
        ensure_session(&paths).unwrap();
        write_outbound(&paths, Some("inb-1"), "chat", "tg", "111", None, &body).unwrap();
        let mut sink = NoStream(0);
        let n = deliver_outbox(&home, "agent", &paths, "tg", "111", 111, Some(backstop), &mut sink).unwrap();
        assert_eq!(n, 1, "a no-streamer reply must deliver now, not wait the backstop");

        // A live streamer → the same young reply is deferred (streamer owns it).
        let temp2 = temp_dir("deliver-streaming");
        let home2 = MaturanaHome::new(temp2.path().join(".maturana"));
        let paths2 = session_paths(&home2.agent_dir("agent"), "s");
        ensure_session(&paths2).unwrap();
        write_outbound(&paths2, Some("inb-1"), "chat", "tg", "111", None, &body).unwrap();
        let mut sink2 = Streaming(0);
        let n2 = deliver_outbox(&home2, "agent", &paths2, "tg", "111", 111, Some(backstop), &mut sink2).unwrap();
        assert_eq!(n2, 0, "a young reply with a live streamer must be deferred");
        assert_eq!(sink2.0, 0);
    }

    #[test]
    fn onboarding_interview_persists_every_turn_until_the_completion_sentinel() {
        // The bug: the onboarding directive only reached turn 1, so the agent
        // answered once and stopped asking. Now an active marker re-injects the
        // "keep interviewing" directive on EVERY turn until the agent signals done.
        let temp = temp_dir("onboarding-interview");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let agent_dir = home.agent_dir("agent");
        fs::create_dir_all(agent_dir.join("memory")).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
        fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
        fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
        fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

        // Not onboarding → no directive.
        let p = build_channel_prompt(&home, "agent", 7, "hi").unwrap();
        assert!(!p.contains("KEEP THE INTERVIEW GOING"));

        // Active → a LATER turn (not turn 1) still carries the directive.
        set_onboarding_active(&home, "agent");
        assert!(is_onboarding_active(&home, "agent"));
        let p2 = build_channel_prompt(&home, "agent", 7, "My name is Anders").unwrap();
        assert!(
            p2.contains("KEEP THE INTERVIEW GOING"),
            "onboarding directive must persist into follow-up turns"
        );

        // The completion sentinel ends the interview and is stripped from the reply.
        let shown =
            finalize_onboarding_reply(&home, "agent", "Great, all set!\n[[ONBOARDING_COMPLETE]]");
        assert_eq!(shown, "Great, all set!", "sentinel stripped from the user-facing reply");
        assert!(!is_onboarding_active(&home, "agent"), "sentinel clears the active state");

        // Cleared → directive no longer injected.
        let p3 = build_channel_prompt(&home, "agent", 7, "thanks").unwrap();
        assert!(!p3.contains("KEEP THE INTERVIEW GOING"));
    }

    #[test]
    fn channel_context_selects_query_terms_from_recent_transcript_for_followups() {
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
        append_channel_turn(
            &home,
            "agent",
            42,
            "user",
            "Please remember the calendar planning context.",
        )
        .unwrap();

        let _prompt = build_channel_prompt(&home, "agent", 42, "what about that?").unwrap();
        let manifest: ChannelContextManifest = serde_json::from_str(
            &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
        )
        .unwrap();
        // A term from the RECENT TRANSCRIPT (not the bare follow-up) is selected for
        // the graph query, so follow-ups like "what about that?" still retrieve context.
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
        append_channel_turn(
            &home,
            "agent",
            42,
            "user",
            "Please use oldcontext next time.",
        )
        .unwrap();

        reset_channel_context(&home, "agent", 42).unwrap();
        let _prompt = build_channel_prompt(&home, "agent", 42, "freshnote please").unwrap();

        let manifest: ChannelContextManifest = serde_json::from_str(
            &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
        )
        .unwrap();
        // After a reset, graph query terms come from the fresh message only — the
        // archived transcript ("oldcontext") and the reset-marker text ("reloaded")
        // must not drive retrieval.
        assert!(manifest.wiki_query_terms.contains(&"freshnote".to_string()));
        assert!(!manifest
            .wiki_query_terms
            .contains(&"oldcontext".to_string()));
        assert!(!manifest.wiki_query_terms.contains(&"reloaded".to_string()));
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

    #[test]
    fn dispatch_turn_round_trips_and_is_never_chat_deliverable() {
        use maturana_core::session_db::write_outbound;
        let temp = temp_dir("dispatch");
        let home = MaturanaHome::new(temp.path().join(".maturana"));

        // Enqueue one orchestration step to a worker; nothing to collect yet.
        let handle =
            enqueue_dispatch_turn(&home, "worker", "s", "run-7", "do the thing", None).unwrap();
        assert!(try_collect_dispatch(&home, "worker", &handle).unwrap().is_none());

        // The step inbound is tagged on the non-deliverable orchestrate channel,
        // not a user channel — no live delivery loop serves it.
        let paths = session_paths(&home.agent_dir("worker"), "s");
        let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
        let step = pending.iter().find(|m| m.id == handle.message_id).unwrap();
        assert_eq!(step.channel, "orchestrate");
        assert_eq!(step.platform_id, "run-7");
        assert_eq!(step.kind, "dispatch");

        // The worker replies; the loop collects it exactly once, then it's consumed.
        write_outbound(
            &paths,
            Some(&handle.message_id),
            "dispatch",
            "orchestrate",
            "run-7",
            None,
            &serde_json::json!({ "text": "did the thing" }).to_string(),
        )
        .unwrap();
        assert_eq!(
            try_collect_dispatch(&home, "worker", &handle).unwrap().as_deref(),
            Some("did the thing")
        );
        assert!(
            try_collect_dispatch(&home, "worker", &handle).unwrap().is_none(),
            "a collected reply is consumed and never seen again"
        );
    }

    #[test]
    fn console_transcript_round_trips_and_is_per_agent() {
        // BUG1: TUI conversation must persist across an agent switch — record +
        // read back the same Markdown transcript Telegram uses, keyed per agent.
        let temp = temp_dir("console-transcript");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        record_console_turn(&home, "alpha", "user", "hello\nworld").unwrap();
        record_console_turn(&home, "alpha", "assistant", "hi there").unwrap();
        assert_eq!(
            read_console_transcript(&home, "alpha"),
            vec![
                ("user".to_string(), "hello\nworld".to_string()),
                ("assistant".to_string(), "hi there".to_string()),
            ]
        );
        // A different agent has its own (empty) transcript — switching can't bleed.
        assert!(read_console_transcript(&home, "beta").is_empty());
    }

    #[test]
    fn clear_console_transcript_persists_across_reopen() {
        // /clear must wipe the stored transcript so it does NOT come back the next
        // time the TUI opens (read_console_transcript returns empty after a clear).
        let temp = temp_dir("console-clear");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        record_console_turn(&home, "alpha", "user", "old conversation").unwrap();
        assert!(!read_console_transcript(&home, "alpha").is_empty());
        clear_console_transcript(&home, "alpha").unwrap();
        assert!(read_console_transcript(&home, "alpha").is_empty());
        // Clearing an already-empty transcript is fine (no file → ok).
        clear_console_transcript(&home, "alpha").unwrap();
    }

    #[test]
    fn dispatch_model_with_args_sets_via_text_not_picker() {
        // BUG2: bare `/model` opens a picker (Select), but `/model gpt-5` must set
        // directly via the text handler (never a picker).
        let temp = temp_dir("dispatch-model-args");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let out = dispatch_slash_command(
            &home,
            "alpha",
            "s",
            console_chat_key(),
            "console",
            &console_chat_key().to_string(),
            "/model gpt-5",
        );
        assert!(matches!(out, ConsoleCommand::Reply(_)));
    }

    #[test]
    fn every_catalog_command_dispatches_on_all_surfaces() {
        // Anti-drift guard for slash-command parity: every command advertised in
        // COMMAND_GROUPS must be RECOGNIZED by the shared dispatcher on every text
        // surface (console TUI + Discord share `dispatch_slash_command`; Telegram
        // routes the same names to the same `handle_channel_command`). A command
        // that fell through to "Unknown command" would mean a channel lags the set.
        let temp = temp_dir("channel-command-parity");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let names: Vec<&str> = COMMAND_GROUPS
            .iter()
            .flat_map(|(_, cmds)| cmds.iter().map(|(name, _)| *name))
            .collect();
        assert!(!names.is_empty(), "command catalog is empty");
        for name in names {
            // arg-guarded commands (/skill, /emerge, …) need a dummy arg to reach
            // the handler rather than the usage fallthrough. /loop's bare-goal form
            // would actually spawn a run, so exercise its non-spawning `status` path.
            let raw = if name == "/loop" {
                format!("{name} status")
            } else {
                format!("{name} x")
            };
            for (chat_id, channel) in
                [(console_chat_key(), "console"), (stable_chat_key("c1"), "discord")]
            {
                let outcome =
                    dispatch_slash_command(&home, "a", "s", chat_id, channel, &chat_id.to_string(), &raw);
                if let ConsoleCommand::Reply(text) = &outcome {
                    assert!(
                        !text.starts_with("Unknown command"),
                        "catalog command {name} fell through to Unknown on {channel}: {text}"
                    );
                }
            }
        }
    }

    fn text_update(chat_id: i64, text: &str) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                text: Some(text.to_string()),
                caption: None,
                document: None,
                photo: None,
                voice: None,
                audio: None,
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
            callback_query: None,
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
                photo: None,
                voice: None,
                audio: None,
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
            callback_query: None,
        }
    }

    fn photo_update(chat_id: i64, caption: Option<&str>) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                text: None,
                caption: caption.map(str::to_string),
                document: None,
                photo: Some(vec![
                    TelegramPhotoSize {
                        file_id: "photo-small".to_string(),
                    },
                    TelegramPhotoSize {
                        file_id: "photo-large".to_string(),
                    },
                ]),
                voice: None,
                audio: None,
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
            callback_query: None,
        }
    }

    fn voice_update(chat_id: i64) -> TelegramUpdate {
        TelegramUpdate {
            update_id: 1,
            message: Some(TelegramMessage {
                message_id: 1,
                text: None,
                caption: None,
                document: None,
                photo: None,
                voice: Some(TelegramVoice {
                    file_id: "voice-123".to_string(),
                }),
                audio: None,
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
            callback_query: None,
        }
    }

    #[test]
    fn routes_voice_notes_to_transcription_from_paired_chat_only() {
        // A voice note carries no text/document/photo. Before the fix it fell
        // through to the empty-text path and was Ignored (the "doesn't even
        // register it" bug); it must now classify as Voice so it gets transcribed.
        assert_eq!(
            classify_telegram_update(&voice_update(7), Some(7), None),
            InboundAction::Voice {
                chat_id: 7,
                file_id: "voice-123".to_string(),
                filename: "voice.ogg".to_string(),
            }
        );
        // The pairing gate applies to voice exactly like documents/photos.
        assert_eq!(
            classify_telegram_update(&voice_update(9), Some(7), None),
            InboundAction::Deny { chat_id: 9 }
        );
    }

    #[test]
    fn stt_multipart_carries_model_and_audio() {
        let (content_type, body) = multipart_audio("model_id", "scribe_v1", "voice.ogg", b"OGGDATA");
        assert!(content_type.contains("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model_id\""));
        assert!(text.contains("scribe_v1"));
        assert!(text.contains("filename=\"voice.ogg\""));
        assert!(text.contains("OGGDATA"));
        assert!(text.trim_end().ends_with("--"));
    }

    #[test]
    fn routes_photo_uploads_to_ocr_from_paired_chat_only() {
        // The largest size is OCR'd; pairing gates the upload.
        assert_eq!(
            classify_telegram_update(&photo_update(7, Some("store this")), Some(7), None),
            InboundAction::Photo {
                chat_id: 7,
                file_id: "photo-large".to_string(),
                caption: Some("store this".to_string()),
            }
        );
        assert_eq!(
            classify_telegram_update(&photo_update(9, None), Some(7), None),
            InboundAction::Deny { chat_id: 9 }
        );
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

    #[test]
    fn console_command_dispatch_matches_telegram_catalog() {
        let temp = temp_dir("console-commands");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        fs::create_dir_all(home.agent_dir("agent")).unwrap();

        assert!(matches!(
            run_console_command(&home, "agent", "telegram-main", "/clear"),
            ConsoleCommand::Clear
        ));
        assert!(matches!(
            run_console_command(&home, "agent", "telegram-main", "/quit"),
            ConsoleCommand::Quit
        ));
        assert!(matches!(
            run_console_command(&home, "agent", "telegram-main", "/new"),
            ConsoleCommand::NewSession
        ));
        match run_console_command(&home, "agent", "telegram-main", "/status") {
            ConsoleCommand::Reply(t) => {
                assert!(t.contains("agent: agent"));
                assert!(t.contains("console"));
            }
            _ => panic!("/status should produce a reply"),
        }
        // /skill <name> [args] runs the skill via a normal agent turn.
        match run_console_command(&home, "agent", "telegram-main", "/skill summarize the notes") {
            ConsoleCommand::Prompt(p) => assert_eq!(p, "Use the `summarize` skill. the notes"),
            _ => panic!("/skill with args should be a prompt"),
        }
        // /model persists a setting and confirms it (shared with Telegram).
        match run_console_command(&home, "agent", "telegram-main", "/model gpt-5") {
            ConsoleCommand::Reply(t) => assert!(t.contains("gpt-5")),
            _ => panic!("/model should reply"),
        }
        match run_console_command(&home, "agent", "telegram-main", "/bogus") {
            ConsoleCommand::Reply(t) => assert!(t.contains("Unknown command")),
            _ => panic!("unknown command should reply"),
        }
        // The catalog the TUI advertises includes the Telegram menu commands.
        let names: Vec<&str> = console_command_catalog().into_iter().map(|(n, _)| n).collect();
        for cmd in [
            "/model",
            "/models",
            "/session",
            "/tools",
            "/subagents",
            "/graph-query",
            "/tts",
            "/onboard",
            "/new",
            "/good",
        ] {
            assert!(names.contains(&cmd), "catalog missing {cmd}");
        }
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A throwaway HTTP server that records each request (first line + body) and
    /// replies with a generic Telegram-OK so the real send/edit code succeeds.
    fn spawn_mock_telegram() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let cap = captured.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    let n = match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let cl = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let body_start = pos + 4;
                        while buf.len() < body_start + cl {
                            match s.read(&mut tmp) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                        let first = headers.lines().next().unwrap_or("").to_string();
                        let body = String::from_utf8_lossy(
                            &buf[body_start..(body_start + cl).min(buf.len())],
                        )
                        .to_string();
                        cap.lock().unwrap().push(format!("{first}\n{body}"));
                        break;
                    }
                }
                let body = r#"{"ok":true,"result":{"message_id":4242}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), captured)
    }

    /// `MATURANA_TELEGRAM_API_BASE` is process-global, so tests that point the real
    /// Telegram code at a local mock must not run concurrently — otherwise one test's
    /// base URL bleeds into another's HTTP call. Serialize them all through this lock.
    static TG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_tg_base<T>(base: &str, f: impl FnOnce() -> T) -> T {
        let _guard = TG_ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        std::env::set_var("MATURANA_TELEGRAM_API_BASE", base);
        let out = f();
        std::env::remove_var("MATURANA_TELEGRAM_API_BASE");
        out
    }

    /// A throwaway HTTP server that replies with a FIXED status line + (optional)
    /// extra headers + body, so the live-edit classifier can be exercised against
    /// real 429/400 responses. `extra_headers`, if non-empty, must include its own
    /// trailing CRLF (e.g. "retry-after: 3\r\n"). Returns the base URL.
    fn spawn_mock_telegram_status(status_line: &str, extra_headers: &str, body: &str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status_line = status_line.to_string();
        let extra_headers = extra_headers.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                // Read the FULL request (headers + Content-Length body) before replying.
                // Closing the socket while the client is still writing its body makes
                // ureq surface a transport error instead of our intended status code.
                loop {
                    let n = match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let cl = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        let body_start = pos + 4;
                        while buf.len() < body_start + cl {
                            match s.read(&mut tmp) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                        break;
                    }
                }
                let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn live_edit_classifies_429_with_retry_after() {
        // A 429 with a retry-after header must surface as Throttled(retry_after) so
        // the loop honors Telegram's cooldown instead of hammering it (the behaviour
        // that turned per-second edits into a 15s freeze).
        let base = spawn_mock_telegram_status(
            "HTTP/1.1 429 Too Many Requests",
            "retry-after: 3\r\n",
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#,
        );
        let outcome = with_tg_base(&base, || edit_telegram_live_html("TESTTOKEN", 1, 2, "<pre>x</pre>"));
        assert!(
            matches!(outcome, LiveEditOutcome::Throttled(Some(3))),
            "429 with retry-after must classify as Throttled(Some(3))"
        );
    }

    #[test]
    fn live_edit_treats_not_modified_400_as_ok() {
        // Editing with identical content yields 400 "message is not modified" — that
        // is benign and must NOT trigger backoff (we already dedup on rendered text).
        let base = spawn_mock_telegram_status(
            "HTTP/1.1 400 Bad Request",
            "",
            r#"{"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}"#,
        );
        let outcome = with_tg_base(&base, || edit_telegram_live_html("TESTTOKEN", 1, 2, "<pre>x</pre>"));
        assert!(
            matches!(outcome, LiveEditOutcome::Ok),
            "benign 'message is not modified' 400 must classify as Ok, got a failure"
        );
    }

    #[test]
    fn finalize_edits_thinking_bubble_into_answer() {
        // Reliable single-message finish: the live "Thinking…" bubble is EDITED in
        // place into the answer — exactly one message, no new send, no delete (so it
        // can never duplicate or orphan a bubble). finalize returns the bubble id.
        let (base, captured) = spawn_mock_telegram();
        let answer = "Three biggest stories: one, two, and three with a short note on each.";
        let returned = with_tg_base(&base, || {
            let id = send_telegram_html("TESTTOKEN", "123", "<pre>💭 Thinking… 0:08</pre>", None)
                .unwrap()
                .expect("draft message id");
            finalize_reply("TESTTOKEN", 123, Some(id), answer, None).unwrap()
        });

        let reqs = captured.lock().unwrap().clone();
        // sendMessage(draft) + editMessageText(answer). No second send, no delete.
        assert_eq!(reqs.len(), 2, "unexpected sequence:\n{}", reqs.join("\n--\n"));
        assert!(reqs[0].contains("/sendMessage") && reqs[0].contains("Thinking"), "{}", reqs[0]);
        assert!(
            reqs[1].contains("/editMessageText") && reqs[1].contains("short note on each"),
            "answer must edit the bubble in place: {}",
            reqs[1]
        );
        assert!(
            reqs.iter().all(|r| !r.contains("/deleteMessage")),
            "no delete — the one bubble becomes the answer"
        );
        // finalize returns the (reused) bubble id, not a new message id.
        assert_eq!(returned, Some(4242));
    }

    #[test]
    fn live_loop_ticks_counter_then_edits_into_answer() {
        // Drive the REAL stream_turn_to_telegram loop against the mock, with a worker
        // reply that lands after ~8s. Asserts the captured HTTP shows the counter
        // ADVANCING (≥2 distinct "💭 Thinking… 0:0X" frames — the old 10s-frozen bug
        // would emit only one) but NOT hammering Telegram (≤6 frames over the ~6s
        // pre-reply window), then a reliable finish: the live bubble is EDITED in
        // place into the answer (one message, no duplicate, no leftover bubble).
        let (base, captured) = spawn_mock_telegram();

        let tmp = std::env::temp_dir().join(format!("mat-streamloop-{}", std::process::id()));
        let home = MaturanaHome::new(tmp.clone());
        let agent = "claude";
        let session = "telegram-main";
        let chat_id = 777i64;
        std::fs::create_dir_all(home.agent_dir(agent)).unwrap();
        let paths = session_paths(&home.agent_dir(agent), session);
        ensure_session(&paths).unwrap();
        let inbound_id = insert_inbound(
            &paths,
            "chat",
            "telegram",
            &chat_id.to_string(),
            None,
            &serde_json::json!({ "text": "hi" }).to_string(),
        )
        .unwrap();

        // The worker's reply lands after ~8s so the counter advances across a few
        // cadence ticks (base 2.5s) before the answer arrives.
        let answer = "Here is a reasonably long answer that the live bubble is edited into when the turn completes.";
        let paths_w = paths.clone();
        let reply_to = inbound_id.clone();
        let config = TelegramServe {
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            token_source: "x".to_string(),
            once: false,
            run_once_provider: None,
            poll_seconds: 5,
            timeout_seconds: 600,
        };
        with_tg_base(&base, || {
            // Spawn the worker INSIDE the serialized section so its reply-delay timer
            // starts at the loop's start, not before this test acquired the shared
            // TG_ENV_LOCK — otherwise a slow earlier test makes the reply land early
            // and the counter shows too few frames (flaky under test contention).
            let worker = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(8000));
                let body = serde_json::json!({ "text": answer }).to_string();
                let _ = maturana_core::session_db::write_outbound(
                    &paths_w,
                    Some(&reply_to),
                    "chat",
                    "telegram",
                    &chat_id.to_string(),
                    None,
                    &body,
                );
            });
            stream_turn_to_telegram(
                &home,
                "TESTTOKEN",
                &config,
                chat_id,
                &inbound_id,
                None,
                &paths,
                std::time::Duration::from_secs(25),
            )
            .unwrap();
            worker.join().unwrap();
        });
        let _ = std::fs::remove_dir_all(&tmp);

        let reqs = captured.lock().unwrap().clone();
        // Counter ADVANCED smoothly: several DISTINCT "💭 Thinking… 0:0X" payloads
        // over the ~6s pre-reply window (~1/s cadence). ≥3 proves the clock isn't
        // frozen/jumpy (the old bucketed bug emitted one, the 2.5s cadence ~2–3);
        // the upper bound just guards against an unbounded edit storm.
        let thinking: std::collections::BTreeSet<String> =
            reqs.iter().filter(|r| r.contains("Thinking")).cloned().collect();
        assert!(
            (3..=14).contains(&thinking.len()),
            "counter should advance ~1/s (expect several distinct frames over the ~6s window), got {}:\n{}",
            thinking.len(),
            thinking.into_iter().collect::<Vec<_>>().join("\n--\n")
        );
        // No dust anywhere — the simulated dots/crumble are gone for good.
        assert!(reqs.iter().all(|r| !r.contains('·')), "no dust frames");
        // The answer was delivered by EDITING the live bubble in place (one message),
        // not a second send and not a delete.
        assert!(
            reqs.iter().any(|r| r.contains("/editMessageText") && r.contains("edited into when the turn completes")),
            "answer not delivered by editing the bubble:\n{}",
            reqs.join("\n--\n")
        );
        assert!(
            reqs.iter().all(|r| !r.contains("/deleteMessage")),
            "no delete on a normal answer — the bubble becomes the answer:\n{}",
            reqs.join("\n--\n")
        );
    }

    #[test]
    fn live_loop_deletes_orphan_bubble_when_reply_already_delivered() {
        // Regression for the duplicate-message + never-ending-counter class
        // (the "Snak dansk" incident): if a backstop pass delivers the reply while
        // the streamer is mid-turn (its live bubble already on screen), the streamer
        // must DELETE its now-orphan bubble and send NO duplicate — the chat shows
        // exactly the one delivered answer, and the counter stops. The streamer
        // detects the reply by EXISTENCE (not undelivered status), so it can't tick
        // forever against a reply someone else already delivered.
        let (base, captured) = spawn_mock_telegram();

        let tmp = std::env::temp_dir().join(format!("mat-orphanbubble-{}", std::process::id()));
        let home = MaturanaHome::new(tmp.clone());
        let agent = "claude";
        let session = "telegram-main";
        let chat_id = 778i64;
        std::fs::create_dir_all(home.agent_dir(agent)).unwrap();
        let paths = session_paths(&home.agent_dir(agent), session);
        ensure_session(&paths).unwrap();
        let inbound_id = insert_inbound(
            &paths,
            "chat",
            "telegram",
            &chat_id.to_string(),
            None,
            &serde_json::json!({ "text": "hi" }).to_string(),
        )
        .unwrap();

        // After ~3.5s the streamer's live bubble exists; THEN a backstop delivers the
        // reply (write outbound + atomically claim it) out from under the streamer.
        let answer = "Already delivered by the backstop.";
        let paths_w = paths.clone();
        let reply_to = inbound_id.clone();
        let config = TelegramServe {
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            token_source: "x".to_string(),
            once: false,
            run_once_provider: None,
            poll_seconds: 5,
            timeout_seconds: 600,
        };
        with_tg_base(&base, || {
            // Spawn the worker INSIDE the serialized section so its timer starts at
            // the loop's start (see the sibling live-loop test for why).
            let worker = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(3500));
                let body = serde_json::json!({ "text": answer }).to_string();
                let _ = maturana_core::session_db::write_outbound(
                    &paths_w,
                    Some(&reply_to),
                    "chat",
                    "telegram",
                    &chat_id.to_string(),
                    None,
                    &body,
                );
                // Simulate the backstop winning the atomic claim before the streamer.
                if let Ok(Some(msg)) = find_reply_outbound(&paths_w, &reply_to) {
                    let _ = claim_delivery(&paths_w, &msg.id);
                }
            });
            stream_turn_to_telegram(
                &home,
                "TESTTOKEN",
                &config,
                chat_id,
                &inbound_id,
                None,
                &paths,
                std::time::Duration::from_secs(25),
            )
            .unwrap();
            worker.join().unwrap();
        });

        // The active-streamer lock is released on every exit path.
        assert!(
            !telegram_active_exists(&paths, &inbound_id),
            "the .tgactive lock must be cleared when the streamer exits"
        );
        let _ = std::fs::remove_dir_all(&tmp);

        let reqs = captured.lock().unwrap().clone();
        // The orphan bubble was DELETED (cleanup) ...
        assert!(
            reqs.iter().any(|r| r.contains("/deleteMessage")),
            "orphan bubble should be deleted on a lost claim:\n{}",
            reqs.join("\n--\n")
        );
        // ... and the streamer did NOT re-send the answer (no duplicate).
        assert!(
            reqs.iter().all(|r| !r.contains("Already delivered by the backstop")),
            "streamer must NOT send the answer the backstop already delivered:\n{}",
            reqs.join("\n--\n")
        );
    }

    #[test]
    fn progress_html_renders_monospace_tool_block() {
        // Nothing to show yet → empty (no placeholder; caller posts no draft).
        assert_eq!(render_progress_html(&[]), "");

        // Structured tool events render as ONE monospace <pre> block: web_search
        // labelled, bash shown as just icon + command. No brain/thinking chrome.
        let events = vec![
            ProgressEvent { seq: 0, kind: "tool".into(), text: "web_search\u{1f}L:Ron:Harald top songs".into() },
            ProgressEvent { seq: 1, kind: "tool".into(), text: "bash\u{1f}rg foo".into() },
        ];
        let rendered = render_progress_html(&events);
        assert!(rendered.starts_with("<pre>") && rendered.ends_with("</pre>"), "{rendered}");
        assert!(rendered.contains("🔎 Web Search: L:Ron:Harald top songs"), "{rendered}");
        assert!(rendered.contains("🛠️ rg foo"), "{rendered}");
        assert!(!rendered.contains('🧠'), "no brain emoji: {rendered}");

        // Legacy "running: <cmd>" events still map to a bash line for back-compat.
        let legacy = vec![ProgressEvent { seq: 0, kind: "tool".into(), text: "running: ls -la".into() }];
        assert!(render_progress_html(&legacy).contains("🛠️ ls -la"));

        // HTML-special characters in the detail are escaped inside the <pre>.
        let unsafe_detail = vec![ProgressEvent { seq: 0, kind: "tool".into(), text: "bash\u{1f}grep <foo> & bar".into() }];
        let escaped = render_progress_html(&unsafe_detail);
        assert!(escaped.contains("grep &lt;foo&gt; &amp; bar"), "{escaped}");

        // "thinking" events are ignored (no brain-dump); final text shows plain.
        let mixed = vec![
            ProgressEvent { seq: 0, kind: "thinking".into(), text: "Looking it up".into() },
            ProgressEvent { seq: 1, kind: "text".into(), text: "Here is the answer.".into() },
        ];
        let r = render_progress_html(&mixed);
        assert!(r.contains("Here is the answer."));
        assert!(!r.contains("Looking it up"), "thinking suppressed: {r}");

        // Unknown tool key falls back to 🧩 + title-cased key.
        let unknown = vec![ProgressEvent { seq: 0, kind: "tool".into(), text: "record_voice\u{1f}cue".into() }];
        assert!(render_progress_html(&unknown).contains("🧩 Record Voice: cue"));
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
