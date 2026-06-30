#[cfg(test)]
use chrono::DateTime;
use chrono::Utc;
use clap::{Args, Subcommand};
#[cfg(test)]
use maturana_core::improvement::signals;
#[cfg(test)]
use maturana_core::session_db::{
    claim_delivery, find_reply_outbound, insert_inbound, list_undelivered, ProgressEvent,
};
use maturana_core::{
    audit::{append_event, AuditEvent},
    pipelock::PipelockVault,
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, session_paths},
    state::MaturanaHome,
};
#[cfg(test)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::{thread, time::Duration};

mod agentmail;
mod command_catalog;
mod command_handler;
mod console_bridge;
mod conversation;
mod delivery;
mod discord;
mod loops;
mod media;
mod models;
mod onboarding;
mod settings;
mod slack;
mod state;
mod subagents;
mod telegram;
mod telegram_api;
mod telegram_inbound;
mod telegram_live;
mod telegram_pairing;
mod telegram_routing;
mod telegram_state;
mod voice;
use agentmail::*;
use command_catalog::command_selector_buttons;
#[cfg(test)]
use command_catalog::help_text;
pub(crate) use command_catalog::{apply_channel_selection, console_command_catalog};
#[cfg(test)]
use command_catalog::{commands_text, COMMAND_GROUPS};
use command_handler::channel_presence;
#[cfg(test)]
use console_bridge::console_chat_key;
pub(crate) use console_bridge::{
    apply_web_console_command, clear_console_transcript, dispatch_slash_command,
    read_console_transcript, record_console_turn, run_console_command, ConsoleCommand,
    SelectOption,
};
use conversation::{
    append_channel_turn, build_channel_prompt, enqueue_channel_prompt, enqueue_turn,
    reset_channel_context, truncate_chars,
};
#[cfg(test)]
use conversation::{
    build_dispatch_prompt, channel_context_manifest_path, channel_transcript_path,
    enqueue_outreach_turn, extract_memory_fact, maybe_remember_user_message,
};
pub(crate) use conversation::{enqueue_dispatch_turn, try_collect_dispatch};
use delivery::deliver_telegram_outbox;
pub(crate) use delivery::{deliver_channel_outbox, deliver_outbox, OutboundSink};
pub(crate) use discord::current_discord_delivery_channel;
#[cfg(test)]
use discord::discord_extract_message;
use discord::serve_discord;
pub(crate) use media::download_telegram_file_bytes;
pub(crate) use media::{agent_knowledge_graph, sanitize_document_name};
use models::*;
pub(crate) use onboarding::finalize_onboarding_reply;
#[cfg(test)]
use onboarding::is_onboarding_active;
use onboarding::{mark_onboarded, onboarding_prompt, set_onboarding_active};
use settings::*;
use slack::*;
use state::*;
use subagents::{create_subagent, frame_subtask, slugify_channel_id, SpawnMode};
use telegram::*;
use telegram_api::*;
pub(crate) use telegram_api::{send_telegram, send_telegram_chat_action};
use telegram_inbound::handle_telegram_update;
pub(crate) use telegram_inbound::run_channel_prompt;
#[cfg(test)]
use telegram_live::render_progress_html;
#[cfg(test)]
use telegram_live::{finalize_reply, stream_turn_to_telegram, telegram_active_exists};
use telegram_pairing::{complete_telegram_pair, start_telegram_pair, telegram_pair_status};
#[cfg(test)]
use telegram_routing::is_pair_command;
#[cfg(test)]
use telegram_routing::{classify_telegram_update, InboundAction};
pub use telegram_state::paired_telegram_chat_source;
pub(crate) use telegram_state::{current_paired_telegram_chat_id, telegram_bridge_live};
use telegram_state::{
    read_telegram_state, telegram_chat_id_key, telegram_pair_code_key, write_telegram_heartbeat,
    write_telegram_state, TELEGRAM_CHAT_ID, TELEGRAM_PAIR_CODE,
};
#[cfg(test)]
use voice::*;

const MAX_RESPONSE_CHARS: usize = 3500;
/// The 1s background delivery thread only backstops a telegram reply once it is
/// older than this — comfortably past the inline streaming loop's own deadline
/// (`STREAM_TURN_TIMEOUT`), so the two never edit the live message concurrently.
const STREAM_BACKSTOP_AGE: Duration = Duration::from_secs(360);
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

#[cfg(test)]
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

#[cfg(test)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoadedContextFile {
    label: String,
    path: String,
    chars: usize,
    missing: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WikiTermSource {
    term: String,
    sources: Vec<String>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextPolicySummary {
    strategy: String,
    transcript_char_budget: usize,
    excludes_reset_marker: bool,
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
        ChannelSubcommand::Status {
            platform: _,
            agent_id,
        } => channel_status(home, &agent_id),
    }
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
            let flushed = deliver_telegram_outbox(
                home,
                &token,
                &config.agent_id,
                &config.session_id,
                chat_id,
                None,
            )
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

pub(super) fn audit_channel_event(
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

pub(super) fn truncate_for_telegram(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_RESPONSE_CHARS {
        return value.to_string();
    }
    value.chars().take(MAX_RESPONSE_CHARS).collect::<String>() + "\n...[truncated]"
}

#[cfg(test)]
mod tests;
