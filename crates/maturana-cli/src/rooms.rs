//! `maturana room` — host-side runner for agent-to-agent rooms.
//!
//! The runner is a single poll loop per room that:
//! 1. ingests human messages from the bridged Telegram group / Discord
//!    channel into the room log,
//! 2. collects member replies from their session outbound queues and posts
//!    them to the room (dropping `PASS` turns),
//! 3. fans new room messages out to each member's session inbound as one
//!    digest prompt (respecting hop budget, mention policy, cooldown, and the
//!    one-digest-in-flight rule),
//! 4. mirrors the room to the bridges so humans can watch and steer.
//!
//! Agents never talk to Discord/Telegram themselves; the room store is the
//! A2A transport and the platforms are views onto it.

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use maturana_core::{
    audit::{append_event, AuditEvent},
    room::{
        build_digest, is_pass, parse_mentions, relevant_to, render_room_prompt, DiscordBridge,
        NewRoomMessage, RoomBridges, RoomConfig, RoomMember, RoomMessage, RoomMode, RoomPolicy,
        RoomStore, SenderKind, TelegramBridge, ROOM_CHANNEL,
    },
    secrets::resolve_secret_source_with_home,
    session_db::{
        count_open_inbound, ensure_session, insert_inbound, list_undelivered, mark_delivered,
        session_paths, SessionPaths,
    },
    state::MaturanaHome,
};
use serde::Deserialize;
use std::{fs, io::Write, thread, time::Duration};

use crate::session::message_text;

const TELEGRAM_MAX_CHARS: usize = 3500;
const DISCORD_MAX_CHARS: usize = 1900;
/// Cursor sentinel: bridge not yet initialized (history must be skipped).
const CURSOR_UNINITIALIZED: i64 = 0;
/// Cursor sentinel: initialized while the platform backlog was empty.
const CURSOR_LIVE_FROM_START: i64 = -1;

#[derive(Debug, Args)]
pub struct RoomCommand {
    #[command(subcommand)]
    pub command: RoomSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum RoomSubcommand {
    /// Create a room: members, policy, and optional chat bridges.
    Init(RoomInit),
    /// Run the room loop: bridge ingest, reply collection, fan-out, mirroring.
    Serve(RoomServe),
    /// Post a message into the room from the host (for testing/steering).
    Post(RoomPost),
    /// Show room configuration and queue positions.
    Status { room_id: String },
    /// Print the most recent room messages.
    Transcript {
        room_id: String,
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
}

#[derive(Debug, Args)]
pub struct RoomInit {
    pub room_id: String,
    /// What the room is trying to achieve; included in every agent prompt.
    #[arg(long, default_value = "")]
    pub goal: String,
    /// Member spec `agent-id[:session-id[:role]]`; repeat for each member.
    #[arg(long = "member", required = true)]
    pub members: Vec<String>,
    /// `open` (everyone sees everything) or `mention-only`.
    #[arg(long, default_value = "open")]
    pub mode: String,
    #[arg(long, default_value_t = 8)]
    pub hop_limit: i64,
    #[arg(long, default_value_t = 0)]
    pub agent_cooldown_seconds: u64,
    /// Telegram group chat id to mirror the room to (pairs with token source).
    #[arg(long)]
    pub telegram_chat_id: Option<i64>,
    #[arg(long, default_value = "pipelock:telegram/bot-token")]
    pub telegram_token_source: String,
    /// Discord channel id (snowflake) to mirror the room to.
    #[arg(long)]
    pub discord_channel_id: Option<String>,
    #[arg(long, default_value = "pipelock:discord/bot-token")]
    pub discord_token_source: String,
    /// Overwrite an existing room config.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct RoomServe {
    pub room_id: String,
    #[arg(long, default_value_t = 5)]
    pub poll_seconds: u64,
    /// Run one tick and exit (for tests and cron-style driving).
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Args)]
pub struct RoomPost {
    pub room_id: String,
    #[arg(long)]
    pub text: String,
    #[arg(long, default_value = "user:cli")]
    pub from: String,
}

pub fn handle_room(command: RoomCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        RoomSubcommand::Init(args) => init_room(home, args),
        RoomSubcommand::Serve(args) => serve_room(home, args),
        RoomSubcommand::Post(args) => {
            let config = load_room(home, &args.room_id)?;
            let store = open_store(home, &args.room_id)?;
            let message = post_to_room(
                home,
                &config,
                &store,
                &args.from,
                SenderKind::User,
                0,
                &args.text,
                "cli",
            )?;
            println!("posted {} (seq {})", message.id, message.seq);
            Ok(())
        }
        RoomSubcommand::Status { room_id } => room_status(home, &room_id),
        RoomSubcommand::Transcript { room_id, tail } => {
            let store = open_store(home, &room_id)?;
            for message in store.recent_messages(tail)? {
                println!(
                    "[{}] {} (hop {}): {}",
                    message.created_at.to_rfc3339(),
                    message.sender,
                    message.hop,
                    message.content
                );
            }
            Ok(())
        }
    }
}

fn load_room(home: &MaturanaHome, room_id: &str) -> anyhow::Result<RoomConfig> {
    RoomConfig::load(&home.room_dir(room_id))
        .with_context(|| format!("room {room_id} not found; run `maturana room init` first"))
}

fn open_store(home: &MaturanaHome, room_id: &str) -> anyhow::Result<RoomStore> {
    RoomStore::open(&RoomStore::store_path(&home.room_dir(room_id)))
}

fn init_room(home: &MaturanaHome, args: RoomInit) -> anyhow::Result<()> {
    let room_dir = home.room_dir(&args.room_id);
    if RoomConfig::path(&room_dir).exists() && !args.force {
        anyhow::bail!(
            "room {} already exists at {}; use --force to overwrite",
            args.room_id,
            room_dir.display()
        );
    }
    let mode = match args.mode.as_str() {
        "open" => RoomMode::Open,
        "mention-only" | "mention_only" => RoomMode::MentionOnly,
        other => anyhow::bail!("unknown room mode {other}; use open or mention-only"),
    };
    let members = args
        .members
        .iter()
        .map(|spec| parse_member_spec(spec))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let config = RoomConfig {
        room_id: args.room_id.clone(),
        goal: args.goal,
        members,
        policy: RoomPolicy {
            mode,
            hop_limit: args.hop_limit,
            agent_cooldown_seconds: args.agent_cooldown_seconds,
        },
        bridges: RoomBridges {
            telegram: args.telegram_chat_id.map(|chat_id| TelegramBridge {
                token_source: args.telegram_token_source.clone(),
                chat_id,
            }),
            discord: args.discord_channel_id.as_ref().map(|channel_id| {
                DiscordBridge {
                    token_source: args.discord_token_source.clone(),
                    channel_id: channel_id.clone(),
                }
            }),
        },
    };
    config.save(&room_dir)?;
    open_store(home, &args.room_id)?;
    for member in &config.members {
        let paths = member_session_paths(home, member);
        ensure_session(&paths)?;
        if !home.agent_dir(&member.agent_id).exists() {
            println!(
                "warning: agent {} is not materialized yet (no {} directory)",
                member.agent_id,
                home.agent_dir(&member.agent_id).display()
            );
        }
    }
    audit_room(home, &args.room_id, "room.init", "room created")?;
    println!(
        "room {} created with {} members ({} mode, hop limit {})",
        args.room_id,
        config.members.len(),
        match config.policy.mode {
            RoomMode::Open => "open",
            RoomMode::MentionOnly => "mention-only",
        },
        config.policy.hop_limit
    );
    if config.bridges.telegram.is_some() {
        println!("telegram bridge: enabled");
    }
    if config.bridges.discord.is_some() {
        println!("discord bridge: enabled");
    }
    println!("start it with: maturana room serve {}", args.room_id);
    Ok(())
}

fn parse_member_spec(spec: &str) -> anyhow::Result<RoomMember> {
    let mut parts = spec.splitn(3, ':');
    let agent_id = parts
        .next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("invalid member spec {spec}"))?
        .trim()
        .to_string();
    let session_id = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("room-main")
        .to_string();
    let role = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(RoomMember {
        agent_id,
        session_id,
        role,
    })
}

fn member_session_paths(home: &MaturanaHome, member: &RoomMember) -> SessionPaths {
    session_paths(&home.agent_dir(&member.agent_id), &member.session_id)
}

fn room_status(home: &MaturanaHome, room_id: &str) -> anyhow::Result<()> {
    let config = load_room(home, room_id)?;
    let store = open_store(home, room_id)?;
    println!("room: {room_id}");
    println!("goal: {}", config.goal);
    println!("messages: {}", store.message_count()?);
    for member in &config.members {
        let cursor = store.cursor(&member_cursor(member))?;
        let open = count_open_inbound(
            &member_session_paths(home, member),
            ROOM_CHANNEL,
            room_id,
        )?;
        println!(
            "member @{} session={} cursor={} open_inbound={}{}",
            member.agent_id,
            member.session_id,
            cursor,
            open,
            member
                .role
                .as_deref()
                .map(|role| format!(" role={role}"))
                .unwrap_or_default()
        );
    }
    println!(
        "bridges: telegram={} discord={}",
        config.bridges.telegram.is_some(),
        config.bridges.discord.is_some()
    );
    Ok(())
}

/// Central entry point for anything that enters the room: parses mentions,
/// appends to the store and the human-readable transcript, and audits.
#[allow(clippy::too_many_arguments)]
fn post_to_room(
    home: &MaturanaHome,
    config: &RoomConfig,
    store: &RoomStore,
    sender: &str,
    sender_kind: SenderKind,
    hop: i64,
    content: &str,
    origin: &str,
) -> anyhow::Result<RoomMessage> {
    let mentions = parse_mentions(content, config);
    let message = store.post(NewRoomMessage {
        sender,
        sender_kind,
        hop,
        content,
        mentions,
        origin,
    })?;
    append_room_transcript(home, &config.room_id, sender, content)?;
    audit_room(
        home,
        &config.room_id,
        "room.post",
        &format!("{sender} posted seq {} (hop {hop}, via {origin})", message.seq),
    )?;
    Ok(message)
}

fn serve_room(home: &MaturanaHome, args: RoomServe) -> anyhow::Result<()> {
    let config = load_room(home, &args.room_id)?;
    let store = open_store(home, &args.room_id)?;
    let telegram_token = config
        .bridges
        .telegram
        .as_ref()
        .map(|bridge| {
            resolve_secret_source_with_home(&bridge.token_source, home.root())
                .map(|secret| secret.expose_for_runtime().to_string())
        })
        .transpose()
        .context("failed to resolve telegram bridge token")?;
    let discord_token = config
        .bridges
        .discord
        .as_ref()
        .map(|bridge| {
            resolve_secret_source_with_home(&bridge.token_source, home.root())
                .map(|secret| secret.expose_for_runtime().to_string())
        })
        .transpose()
        .context("failed to resolve discord bridge token")?;

    println!(
        "room {} serving {} members (telegram={}, discord={})",
        config.room_id,
        config.members.len(),
        telegram_token.is_some(),
        discord_token.is_some()
    );

    loop {
        write_room_heartbeat(home, &config.room_id, "polling", None)?;
        let result = room_tick(
            home,
            &config,
            &store,
            telegram_token.as_deref(),
            discord_token.as_deref(),
        );
        match result {
            Ok(()) => write_room_heartbeat(home, &config.room_id, "idle", None)?,
            Err(error) => {
                let message = format!("{error:#}");
                write_room_heartbeat(home, &config.room_id, "error", Some(&message))?;
                audit_room(home, &config.room_id, "room.tick_error", &message)?;
                if args.once {
                    return Err(error);
                }
            }
        }
        if args.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(args.poll_seconds.max(1)));
    }
}

fn room_tick(
    home: &MaturanaHome,
    config: &RoomConfig,
    store: &RoomStore,
    telegram_token: Option<&str>,
    discord_token: Option<&str>,
) -> anyhow::Result<()> {
    if let (Some(bridge), Some(token)) = (&config.bridges.telegram, telegram_token) {
        ingest_telegram(home, config, store, bridge, token)?;
    }
    if let (Some(bridge), Some(token)) = (&config.bridges.discord, discord_token) {
        ingest_discord(home, config, store, bridge, token)?;
    }
    collect_member_outbound(home, config, store)?;
    fan_out(home, config, store)?;
    if let (Some(bridge), Some(token)) = (&config.bridges.telegram, telegram_token) {
        mirror_to_telegram(store, bridge, token)?;
    }
    if let (Some(bridge), Some(token)) = (&config.bridges.discord, discord_token) {
        mirror_to_discord(store, bridge, token)?;
    }
    Ok(())
}

/// Pull each member's pending room replies out of its session outbound and
/// post them to the room. `PASS` replies are consumed silently. The reply's
/// hop is one more than the budget carried by the digest it answers.
fn collect_member_outbound(
    home: &MaturanaHome,
    config: &RoomConfig,
    store: &RoomStore,
) -> anyhow::Result<()> {
    for member in &config.members {
        let paths = member_session_paths(home, member);
        ensure_session(&paths)?;
        for outbound in list_undelivered(&paths)? {
            if outbound.channel != ROOM_CHANNEL || outbound.platform_id != config.room_id {
                continue;
            }
            let text = message_text(&outbound.content)?;
            if is_pass(&text) {
                audit_room(
                    home,
                    &config.room_id,
                    "room.pass",
                    &format!("@{} passed its turn", member.agent_id),
                )?;
            } else {
                let parent_hop = match &outbound.in_reply_to {
                    Some(inbound_id) => store.fanout_hop(inbound_id)?.unwrap_or(0),
                    None => 0,
                };
                post_to_room(
                    home,
                    config,
                    store,
                    &member.agent_id,
                    SenderKind::Agent,
                    parent_hop + 1,
                    text.trim(),
                    "agent",
                )?;
            }
            mark_delivered(&paths, &outbound.id, None)?;
        }
    }
    Ok(())
}

/// Deliver new room messages to each member as a single digest prompt.
fn fan_out(home: &MaturanaHome, config: &RoomConfig, store: &RoomStore) -> anyhow::Result<()> {
    for member in &config.members {
        let cursor_key = member_cursor(member);
        let cursor = store.cursor(&cursor_key)?;
        let pending = store.messages_after(cursor)?;
        if pending.is_empty() {
            continue;
        }
        let paths = member_session_paths(home, member);
        ensure_session(&paths)?;
        // One digest in flight: wait until the previous one is finished so a
        // busy room batches up instead of swamping the agent's queue.
        if count_open_inbound(&paths, ROOM_CHANNEL, &config.room_id)? > 0 {
            continue;
        }
        // Cooldown: let an agent that just spoke sit out until it elapses.
        if config.policy.agent_cooldown_seconds > 0 {
            if let Some(last_post) = store.last_post_at(&member.agent_id)? {
                let elapsed = Utc::now().signed_duration_since(last_post);
                if elapsed.num_seconds() < config.policy.agent_cooldown_seconds as i64 {
                    continue;
                }
            }
        }
        let relevant = pending
            .iter()
            .filter(|message| relevant_to(member, &config.policy, message))
            .collect::<Vec<_>>();
        let last_seq = pending.last().map(|message| message.seq).unwrap_or(cursor);
        let Some(digest) = build_digest(&relevant) else {
            // Nothing addressed to this member; just advance past the batch.
            store.set_cursor(&cursor_key, last_seq)?;
            continue;
        };
        let prompt = render_room_prompt(config, member, &digest, store)?;
        let inbound_id = insert_inbound(
            &paths,
            "room",
            ROOM_CHANNEL,
            &config.room_id,
            None,
            &serde_json::json!({
                "text": digest.text,
                "prompt": prompt,
                "room": config.room_id,
                "hop": digest.hop,
                "room_msg_ids": digest.message_ids,
            })
            .to_string(),
        )?;
        store.record_fanout(&inbound_id, &member.agent_id, digest.hop, &digest.message_ids)?;
        store.set_cursor(&cursor_key, last_seq)?;
        audit_room(
            home,
            &config.room_id,
            "room.fanout",
            &format!(
                "queued {} message(s) for @{} (hop {})",
                digest.message_ids.len(),
                member.agent_id,
                digest.hop
            ),
        )?;
    }
    Ok(())
}

fn member_cursor(member: &RoomMember) -> String {
    format!("member:{}", member.agent_id)
}

// ---------------------------------------------------------------------------
// Telegram bridge
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TgUpdatesResponse {
    ok: bool,
    result: Vec<TgUpdate>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    text: Option<String>,
    chat: TgChat,
    from: Option<TgUser>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    username: Option<String>,
    first_name: Option<String>,
    #[serde(default)]
    is_bot: bool,
}

#[derive(Debug, Deserialize)]
struct TgOkResponse {
    ok: bool,
}

const TELEGRAM_OFFSET_CURSOR: &str = "bridge:telegram:offset";
const TELEGRAM_OUT_CURSOR: &str = "bridge:telegram:out";
const DISCORD_AFTER_CURSOR: &str = "bridge:discord:after";
const DISCORD_OUT_CURSOR: &str = "bridge:discord:out";

fn ingest_telegram(
    home: &MaturanaHome,
    config: &RoomConfig,
    store: &RoomStore,
    bridge: &TelegramBridge,
    token: &str,
) -> anyhow::Result<()> {
    let cursor = store.cursor(TELEGRAM_OFFSET_CURSOR)?;
    let offset = if cursor > 0 { Some(cursor) } else { None };
    let updates = telegram_updates(token, offset)?;
    if cursor == CURSOR_UNINITIALIZED {
        // First contact: skip whatever backlog predates the room.
        let next = updates
            .iter()
            .map(|update| update.update_id + 1)
            .max()
            .unwrap_or(CURSOR_LIVE_FROM_START);
        store.set_cursor(TELEGRAM_OFFSET_CURSOR, next)?;
        return Ok(());
    }
    let mut max_seen = cursor;
    for update in &updates {
        max_seen = max_seen.max(update.update_id + 1);
        let Some(message) = &update.message else {
            continue;
        };
        if message.chat.id != bridge.chat_id {
            continue;
        }
        let Some(text) = message.text.as_deref().map(str::trim).filter(|t| !t.is_empty())
        else {
            continue;
        };
        if message.from.as_ref().is_some_and(|user| user.is_bot) {
            continue;
        }
        let name = message
            .from
            .as_ref()
            .and_then(|user| user.username.clone().or_else(|| user.first_name.clone()))
            .unwrap_or_else(|| "telegram".to_string());
        post_to_room(
            home,
            config,
            store,
            &format!("user:{name}"),
            SenderKind::User,
            0,
            text,
            "telegram",
        )?;
    }
    if max_seen != cursor {
        store.set_cursor(TELEGRAM_OFFSET_CURSOR, max_seen)?;
    }
    Ok(())
}

fn mirror_to_telegram(
    store: &RoomStore,
    bridge: &TelegramBridge,
    token: &str,
) -> anyhow::Result<()> {
    let cursor = store.cursor(TELEGRAM_OUT_CURSOR)?;
    for message in store.messages_after(cursor)? {
        if message.origin != "telegram" {
            let text = truncate_chars(
                &format!("{}: {}", message.sender, message.content),
                TELEGRAM_MAX_CHARS,
            );
            telegram_send(token, bridge.chat_id, &text)?;
        }
        store.set_cursor(TELEGRAM_OUT_CURSOR, message.seq)?;
    }
    Ok(())
}

fn telegram_updates(token: &str, offset: Option<i64>) -> anyhow::Result<Vec<TgUpdate>> {
    let mut url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout=0");
    if let Some(offset) = offset {
        url.push_str(&format!("&offset={offset}"));
    }
    let response: TgUpdatesResponse = ureq::get(&url)
        .call()
        .context("Telegram getUpdates failed")?
        .into_json()
        .context("failed to parse Telegram getUpdates response")?;
    if !response.ok {
        anyhow::bail!("Telegram getUpdates returned ok=false");
    }
    Ok(response.result)
}

fn telegram_send(token: &str, chat_id: i64, text: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({ "chat_id": chat_id, "text": text });
    let response: TgOkResponse =
        ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendMessage failed: {error}"))?
            .into_json()
            .context("failed to parse Telegram sendMessage response")?;
    if !response.ok {
        anyhow::bail!("Telegram sendMessage returned ok=false");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Discord bridge (REST polling; no gateway websocket needed)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DiscordMessage {
    id: String,
    content: String,
    author: DiscordAuthor,
}

#[derive(Debug, Deserialize)]
struct DiscordAuthor {
    username: String,
    #[serde(default)]
    bot: bool,
}

fn ingest_discord(
    home: &MaturanaHome,
    config: &RoomConfig,
    store: &RoomStore,
    bridge: &DiscordBridge,
    token: &str,
) -> anyhow::Result<()> {
    let cursor = store.cursor(DISCORD_AFTER_CURSOR)?;
    if cursor == CURSOR_UNINITIALIZED {
        // First contact: record the newest message id and ignore history.
        let latest = discord_messages(token, &bridge.channel_id, None, 1)?;
        let next = latest
            .first()
            .and_then(|message| message.id.parse::<i64>().ok())
            .unwrap_or(CURSOR_LIVE_FROM_START);
        store.set_cursor(DISCORD_AFTER_CURSOR, next)?;
        return Ok(());
    }
    let after = if cursor > 0 { Some(cursor) } else { None };
    let mut messages = discord_messages(token, &bridge.channel_id, after, 100)?;
    // Discord returns newest first; replay oldest first.
    messages.sort_by_key(|message| message.id.parse::<i64>().unwrap_or(i64::MAX));
    let mut max_seen = cursor;
    for message in &messages {
        let id = message.id.parse::<i64>().unwrap_or(i64::MAX);
        if id <= cursor {
            continue;
        }
        max_seen = max_seen.max(id);
        // Skip our own mirror posts (and any other bot) to avoid echo loops.
        if message.author.bot {
            continue;
        }
        let text = message.content.trim();
        if text.is_empty() {
            continue;
        }
        post_to_room(
            home,
            config,
            store,
            &format!("user:{}", message.author.username),
            SenderKind::User,
            0,
            text,
            "discord",
        )?;
    }
    if max_seen != cursor {
        store.set_cursor(DISCORD_AFTER_CURSOR, max_seen)?;
    }
    Ok(())
}

fn mirror_to_discord(
    store: &RoomStore,
    bridge: &DiscordBridge,
    token: &str,
) -> anyhow::Result<()> {
    let cursor = store.cursor(DISCORD_OUT_CURSOR)?;
    for message in store.messages_after(cursor)? {
        if message.origin != "discord" {
            let text = truncate_chars(
                &format!("**{}**: {}", message.sender, message.content),
                DISCORD_MAX_CHARS,
            );
            discord_send(token, &bridge.channel_id, &text)?;
        }
        store.set_cursor(DISCORD_OUT_CURSOR, message.seq)?;
    }
    Ok(())
}

fn discord_messages(
    token: &str,
    channel_id: &str,
    after: Option<i64>,
    limit: usize,
) -> anyhow::Result<Vec<DiscordMessage>> {
    let mut url =
        format!("https://discord.com/api/v10/channels/{channel_id}/messages?limit={limit}");
    if let Some(after) = after {
        url.push_str(&format!("&after={after}"));
    }
    let messages: Vec<DiscordMessage> = ureq::get(&url)
        .set("authorization", &format!("Bot {token}"))
        .call()
        .context("Discord get messages failed (check token and channel permissions)")?
        .into_json()
        .context("failed to parse Discord messages response")?;
    Ok(messages)
}

fn discord_send(token: &str, channel_id: &str, content: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({ "content": content });
    ureq::post(&format!(
        "https://discord.com/api/v10/channels/{channel_id}/messages"
    ))
    .set("authorization", &format!("Bot {token}"))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Discord send failed: {error}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

fn append_room_transcript(
    home: &MaturanaHome,
    room_id: &str,
    sender: &str,
    content: &str,
) -> anyhow::Result<()> {
    let path = home.room_dir(room_id).join("transcript.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = format!(
        "\n## {} {}\n\n{}\n",
        Utc::now().to_rfc3339(),
        sender,
        content.trim()
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;
    Ok(())
}

fn write_room_heartbeat(
    home: &MaturanaHome,
    room_id: &str,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let path = home.room_dir(room_id).join("heartbeat.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::json!({
            "room_id": room_id,
            "status": status,
            "error": error,
            "at": Utc::now(),
        }))?,
    )
    .context("failed to write room heartbeat")
}

fn audit_room(
    home: &MaturanaHome,
    room_id: &str,
    action: &str,
    message: &str,
) -> anyhow::Result<()> {
    append_event(
        home.audit_dir().join(format!("room-{room_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: format!("room:{room_id}"),
            action: action.to_string(),
            message: message.to_string(),
        },
    )
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "\n...[truncated]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::session_db::{
        claim_pending_inbound, mark_inbound_completed, write_outbound,
    };
    use std::path::{Path, PathBuf};

    fn test_room(home: &MaturanaHome, hop_limit: i64) -> (RoomConfig, RoomStore) {
        let config = RoomConfig {
            room_id: "launch".to_string(),
            goal: "Ship a website".to_string(),
            members: vec![
                RoomMember {
                    agent_id: "planner".to_string(),
                    session_id: "room-main".to_string(),
                    role: Some("lead".to_string()),
                },
                RoomMember {
                    agent_id: "builder".to_string(),
                    session_id: "room-main".to_string(),
                    role: None,
                },
            ],
            policy: RoomPolicy {
                mode: RoomMode::Open,
                hop_limit,
                agent_cooldown_seconds: 0,
            },
            bridges: RoomBridges::default(),
        };
        config.save(&home.room_dir("launch")).unwrap();
        let store = open_store(home, "launch").unwrap();
        for member in &config.members {
            ensure_session(&member_session_paths(home, member)).unwrap();
        }
        (config, store)
    }

    #[test]
    fn user_message_fans_out_to_all_members_as_protocol_prompt() {
        let temp = temp_dir("room-fanout");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let (config, store) = test_room(&home, 8);

        post_to_room(
            &home,
            &config,
            &store,
            "user:anders",
            SenderKind::User,
            0,
            "Team, build and market a new website. @planner break it down.",
            "cli",
        )
        .unwrap();
        fan_out(&home, &config, &store).unwrap();

        for member in &config.members {
            let paths = member_session_paths(&home, member);
            let claimed = claim_pending_inbound(&paths, 10).unwrap();
            assert_eq!(claimed.len(), 1, "member {} should get one digest", member.agent_id);
            assert_eq!(claimed[0].channel, ROOM_CHANNEL);
            assert_eq!(claimed[0].platform_id, "launch");
            let content: serde_json::Value =
                serde_json::from_str(&claimed[0].content).unwrap();
            assert_eq!(content["hop"], 0);
            let prompt = content["prompt"].as_str().unwrap();
            assert!(prompt.contains("Ship a website"));
            assert!(prompt.contains("user:anders: Team, build and market"));
            assert!(prompt.contains("PASS"));
            // Hop is recorded so the reply can inherit the budget.
            assert_eq!(store.fanout_hop(&claimed[0].id).unwrap(), Some(0));
            mark_inbound_completed(&paths, &[claimed[0].id.clone()]).unwrap();
        }
        // Cursors advanced: a second fan-out delivers nothing new.
        fan_out(&home, &config, &store).unwrap();
        for member in &config.members {
            let paths = member_session_paths(&home, member);
            assert!(claim_pending_inbound(&paths, 10).unwrap().is_empty());
        }
    }

    #[test]
    fn agent_reply_reaches_other_members_but_not_itself_and_pass_is_silent() {
        let temp = temp_dir("room-a2a");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let (config, store) = test_room(&home, 8);
        let planner = &config.members[0];
        let builder = &config.members[1];

        post_to_room(
            &home, &config, &store, "user:anders", SenderKind::User, 0, "kick off", "cli",
        )
        .unwrap();
        fan_out(&home, &config, &store).unwrap();

        // Planner answers its digest; builder passes.
        let planner_paths = member_session_paths(&home, planner);
        let planner_inbound = claim_pending_inbound(&planner_paths, 1).unwrap();
        write_outbound(
            &planner_paths,
            Some(&planner_inbound[0].id),
            "room",
            ROOM_CHANNEL,
            "launch",
            None,
            r#"{"text":"@builder please scaffold the repo"}"#,
        )
        .unwrap();
        mark_inbound_completed(&planner_paths, &[planner_inbound[0].id.clone()]).unwrap();

        let builder_paths = member_session_paths(&home, builder);
        let builder_inbound = claim_pending_inbound(&builder_paths, 1).unwrap();
        write_outbound(
            &builder_paths,
            Some(&builder_inbound[0].id),
            "room",
            ROOM_CHANNEL,
            "launch",
            None,
            r#"{"text":"PASS"}"#,
        )
        .unwrap();
        mark_inbound_completed(&builder_paths, &[builder_inbound[0].id.clone()]).unwrap();

        collect_member_outbound(&home, &config, &store).unwrap();
        // PASS was consumed: only user message + planner reply in the room.
        let messages = store.messages_after(0).unwrap();
        assert_eq!(messages.len(), 2);
        let reply = &messages[1];
        assert_eq!(reply.sender, "planner");
        assert_eq!(reply.hop, 1);
        assert_eq!(reply.mentions, vec!["builder".to_string()]);

        fan_out(&home, &config, &store).unwrap();
        // Builder receives the planner's message…
        let builder_digest = claim_pending_inbound(&builder_paths, 10).unwrap();
        assert_eq!(builder_digest.len(), 1);
        let content: serde_json::Value =
            serde_json::from_str(&builder_digest[0].content).unwrap();
        assert_eq!(content["hop"], 1);
        assert!(content["text"]
            .as_str()
            .unwrap()
            .contains("planner: @builder please scaffold the repo"));
        // …and the planner does not get its own message back.
        assert!(claim_pending_inbound(&planner_paths, 10).unwrap().is_empty());
    }

    #[test]
    fn hop_limit_stops_agent_to_agent_cascade() {
        let temp = temp_dir("room-hop");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let (config, store) = test_room(&home, 2);

        // A message that already exhausted the relay budget.
        post_to_room(
            &home, &config, &store, "planner", SenderKind::Agent, 2, "ping", "agent",
        )
        .unwrap();
        fan_out(&home, &config, &store).unwrap();
        let builder_paths = member_session_paths(&home, &config.members[1]);
        assert!(
            claim_pending_inbound(&builder_paths, 10).unwrap().is_empty(),
            "messages at the hop limit must not be fanned out"
        );
        // The cursor still advanced; the room is not wedged.
        assert_eq!(store.cursor("member:builder").unwrap(), 1);
    }

    #[test]
    fn second_digest_waits_until_first_is_finished() {
        let temp = temp_dir("room-inflight");
        let home = MaturanaHome::new(temp.path().join(".maturana"));
        let (config, store) = test_room(&home, 8);
        let builder_paths = member_session_paths(&home, &config.members[1]);

        post_to_room(&home, &config, &store, "user:anders", SenderKind::User, 0, "one", "cli")
            .unwrap();
        fan_out(&home, &config, &store).unwrap();
        post_to_room(&home, &config, &store, "user:anders", SenderKind::User, 0, "two", "cli")
            .unwrap();
        fan_out(&home, &config, &store).unwrap();

        // Only the first digest is enqueued while it is unfinished.
        let first = claim_pending_inbound(&builder_paths, 10).unwrap();
        assert_eq!(first.len(), 1);
        mark_inbound_completed(&builder_paths, &[first[0].id.clone()]).unwrap();

        // Once finished, the backlog arrives as one batched digest.
        fan_out(&home, &config, &store).unwrap();
        let second = claim_pending_inbound(&builder_paths, 10).unwrap();
        assert_eq!(second.len(), 1);
        let content: serde_json::Value = serde_json::from_str(&second[0].content).unwrap();
        assert!(content["text"].as_str().unwrap().contains("two"));
    }

    #[test]
    fn member_spec_parses_session_and_role() {
        let member = parse_member_spec("planner:room-main:project lead").unwrap();
        assert_eq!(member.agent_id, "planner");
        assert_eq!(member.session_id, "room-main");
        assert_eq!(member.role.as_deref(), Some("project lead"));
        let bare = parse_member_spec("builder").unwrap();
        assert_eq!(bare.session_id, "room-main");
        assert!(bare.role.is_none());
        assert!(parse_member_spec(":x").is_err());
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
