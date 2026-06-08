use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use maturana_core::{
    audit::{append_event, AuditEvent},
    pipelock::PipelockVault,
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, insert_inbound, list_undelivered, mark_delivered, session_paths},
    state::MaturanaHome,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use crate::session::{message_text, run_session_once, RunnerOptions};

const TELEGRAM_PAIR_CODE: &str = "telegram/pair-code";
const TELEGRAM_CHAT_ID: &str = "telegram/chat-id";
const MAX_RESPONSE_CHARS: usize = 3500;

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
    pub ip: Option<String>,
    #[arg(long, default_value = "ubuntu")]
    pub ssh_user: String,
    #[arg(
        long,
        env = "MATURANA_AGENT_SSH_KEY",
        default_value = ".maturana/keys/maturana-agent-ed25519"
    )]
    pub ssh_key: PathBuf,
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
    chat: TelegramChat,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum InboundAction {
    Ignore,
    Pair { chat_id: i64 },
    Help { chat_id: i64 },
    Status { chat_id: i64 },
    Prompt { chat_id: i64, text: String },
    Deny { chat_id: i64 },
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
                "Commands: /status, /help. Any other message is sent to the agent.",
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
                    ip: config.ip.clone(),
                    ssh_user: config.ssh_user.clone(),
                    ssh_key: config.ssh_key.clone(),
                    guest_workspace: "/workspace".to_string(),
                    timeout_seconds: config.timeout_seconds,
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

fn classify_telegram_update(
    update: &TelegramUpdate,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
) -> InboundAction {
    let Some(message) = telegram_message(update) else {
        return InboundAction::Ignore;
    };
    let chat_id = message.chat.id;
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
        _ if command.starts_with('/') => InboundAction::Help { chat_id },
        _ => InboundAction::Prompt {
            chat_id,
            text: text.to_string(),
        },
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

fn build_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<String> {
    let agent_dir = home.agent_dir(agent_id);
    let identity = read_context_file(&agent_dir.join("AGENTS.md"), 4000)?;
    let soul = read_context_file(&agent_dir.join("SOUL.md"), 4000)?;
    let contract = read_context_file(&agent_dir.join("MATURANA.md"), 5000)?;
    let memory = read_context_file(&agent_dir.join("memory/MEMORY.md"), 5000)?;
    let agent_context = read_context_file(&agent_dir.join("context/README.md"), 3000)?;
    let wiki_index = read_context_file(&home.root().join("wiki/INDEX.md"), 5000)?;
    let transcript = tail_context_file(&channel_transcript_path(home, agent_id, chat_id), 8000)?;

    Ok(format!(
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

## Recent Telegram Transcript
{transcript}

## Current Telegram Message
{user_message}
"#
    ))
}

fn read_context_file(path: &Path, limit: usize) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok("(missing)".to_string());
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(truncate_chars(&contents, limit))
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
            classify_telegram_update(&text_update(7, "hello"), Some(7), None),
            InboundAction::Prompt {
                chat_id: 7,
                text: "hello".to_string()
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
        append_channel_turn(&home, "agent", 42, "user", "my name is Anders").unwrap();

        let prompt = build_channel_prompt(&home, "agent", 42, "what is my name?").unwrap();
        assert!(prompt.contains("likes tea"));
        assert!(prompt.contains("my name is Anders"));
        assert!(prompt.contains("what is my name?"));
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
                chat: TelegramChat { id: chat_id },
            }),
            channel_post: None,
        }
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
