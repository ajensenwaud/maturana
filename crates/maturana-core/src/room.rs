//! Agent-to-agent "rooms": a host-side group-conversation bus.
//!
//! A room is a shared, ordered message log that a set of agents (room
//! *members*) read from and post to. The host-side room runner (`maturana
//! room serve`) fans new room messages out into each member's existing
//! session queue as a single digest prompt, collects each member's replies
//! from its session outbound, and posts them back into the room. Discord
//! and Telegram group chats are *bridges*: bidirectional mirrors of the room
//! that let a human watch and steer the agents. The room store — not the
//! chat platform — is the source of truth, because platform limitations
//! (Telegram bots cannot see other bots' messages) make the group chat
//! itself unusable as the A2A transport.
//!
//! Self-organisation is conversational: agents address each other with
//! `@agent-id`, claim work in plain language, and abstain by replying
//! `PASS`. Runaway agent-to-agent loops are prevented structurally:
//!
//! * **Hop budget** — every message carries a relay depth (`hop`). User
//!   messages are hop 0; an agent reply is one more than the digest it
//!   answered. Agent messages at or past `hop_limit` are still mirrored to
//!   the bridges but never fanned out to other agents, so a pure
//!   agent-to-agent cascade always terminates.
//! * **PASS** — an agent with nothing to add replies `PASS`, which is
//!   consumed silently and generates no further fan-out.
//! * **One digest in flight** — the runner never enqueues a new digest for a
//!   member that still has an unfinished room message in its session queue.
//! * **Cooldown** — optional per-agent minimum delay between turns.

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Channel name used for room traffic in the per-agent session queues.
pub const ROOM_CHANNEL: &str = "room";

/// Mention token that addresses every member.
pub const MENTION_ALL: &str = "all";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomConfig {
    pub room_id: String,
    #[serde(default)]
    pub goal: String,
    pub members: Vec<RoomMember>,
    #[serde(default)]
    pub policy: RoomPolicy,
    #[serde(default)]
    pub bridges: RoomBridges,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomMember {
    pub agent_id: String,
    /// Session queue the digests are enqueued to and replies collected from.
    #[serde(default = "default_member_session")]
    pub session_id: String,
    #[serde(default)]
    pub role: Option<String>,
}

fn default_member_session() -> String {
    "room-main".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomPolicy {
    #[serde(default)]
    pub mode: RoomMode,
    /// Agent messages at or beyond this relay depth are not fanned out.
    #[serde(default = "default_hop_limit")]
    pub hop_limit: i64,
    /// Minimum seconds between an agent's posts before it is handed new work.
    #[serde(default)]
    pub agent_cooldown_seconds: u64,
}

impl Default for RoomPolicy {
    fn default() -> Self {
        Self {
            mode: RoomMode::Open,
            hop_limit: default_hop_limit(),
            agent_cooldown_seconds: 0,
        }
    }
}

fn default_hop_limit() -> i64 {
    8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoomMode {
    /// Every member sees every message (and decides via PASS).
    #[default]
    Open,
    /// Members only see messages that mention them, mention @all, or are
    /// unaddressed user broadcasts.
    MentionOnly,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomBridges {
    #[serde(default)]
    pub telegram: Option<TelegramBridge>,
    #[serde(default)]
    pub discord: Option<DiscordBridge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramBridge {
    pub token_source: String,
    /// Group chat id the room mirrors to (negative for Telegram groups).
    pub chat_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordBridge {
    pub token_source: String,
    /// Channel id (snowflake) the room mirrors to.
    pub channel_id: String,
}

impl RoomConfig {
    pub fn path(room_dir: &Path) -> PathBuf {
        room_dir.join("room.json")
    }

    pub fn load(room_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(room_dir);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Self = serde_json::from_str(&raw)
            .with_context(|| format!("invalid room config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, room_dir: &Path) -> anyhow::Result<()> {
        self.validate()?;
        fs::create_dir_all(room_dir)
            .with_context(|| format!("failed to create {}", room_dir.display()))?;
        let path = Self::path(room_dir);
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.room_id.trim().is_empty() {
            anyhow::bail!("room_id must not be empty");
        }
        if self.members.len() < 2 {
            anyhow::bail!("a room needs at least two members to be useful");
        }
        let mut seen = std::collections::HashSet::new();
        for member in &self.members {
            if member.agent_id.trim().is_empty() {
                anyhow::bail!("member agent_id must not be empty");
            }
            if !seen.insert(member.agent_id.as_str()) {
                anyhow::bail!("duplicate room member: {}", member.agent_id);
            }
        }
        if self.policy.hop_limit < 1 {
            anyhow::bail!("policy.hop_limit must be at least 1");
        }
        Ok(())
    }

    pub fn member(&self, agent_id: &str) -> Option<&RoomMember> {
        self.members
            .iter()
            .find(|member| member.agent_id == agent_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    User,
    Agent,
    System,
}

impl SenderKind {
    fn as_str(self) -> &'static str {
        match self {
            SenderKind::User => "user",
            SenderKind::Agent => "agent",
            SenderKind::System => "system",
        }
    }

    fn parse(value: &str) -> SenderKind {
        match value {
            "agent" => SenderKind::Agent,
            "system" => SenderKind::System,
            _ => SenderKind::User,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomMessage {
    pub id: String,
    pub seq: i64,
    pub sender: String,
    pub sender_kind: SenderKind,
    pub hop: i64,
    pub content: String,
    pub mentions: Vec<String>,
    /// Where the message entered the room: `telegram`, `discord`, `cli`, `agent`.
    pub origin: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewRoomMessage<'a> {
    pub sender: &'a str,
    pub sender_kind: SenderKind,
    pub hop: i64,
    pub content: &'a str,
    pub mentions: Vec<String>,
    pub origin: &'a str,
}

/// SQLite-backed ordered room log plus per-consumer cursors and the fan-out
/// ledger that maps session inbound ids back to the hop budget they carried.
pub struct RoomStore {
    db: Connection,
}

impl RoomStore {
    pub fn store_path(room_dir: &Path) -> PathBuf {
        room_dir.join("room.sqlite")
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let db = Connection::open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "DELETE")?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                seq INTEGER NOT NULL,
                sender TEXT NOT NULL,
                sender_kind TEXT NOT NULL,
                hop INTEGER NOT NULL,
                content TEXT NOT NULL,
                mentions TEXT NOT NULL,
                origin TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS cursors (
                consumer TEXT PRIMARY KEY,
                last_seq INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS fanout (
                inbound_msg_id TEXT PRIMARY KEY,
                member_agent_id TEXT NOT NULL,
                hop INTEGER NOT NULL,
                room_msg_ids TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(Self { db })
    }

    pub fn post(&self, message: NewRoomMessage<'_>) -> anyhow::Result<RoomMessage> {
        let id = format!("room-{}", Uuid::new_v4());
        let seq: i64 =
            self.db
                .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM messages", [], |row| {
                    row.get(0)
                })?;
        let created_at = Utc::now();
        self.db.execute(
            r#"
            INSERT INTO messages (id, seq, sender, sender_kind, hop, content, mentions, origin, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                id,
                seq,
                message.sender,
                message.sender_kind.as_str(),
                message.hop,
                message.content,
                serde_json::to_string(&message.mentions)?,
                message.origin,
                created_at.to_rfc3339(),
            ],
        )?;
        Ok(RoomMessage {
            id,
            seq,
            sender: message.sender.to_string(),
            sender_kind: message.sender_kind,
            hop: message.hop,
            content: message.content.to_string(),
            mentions: message.mentions,
            origin: message.origin.to_string(),
            created_at,
        })
    }

    pub fn messages_after(&self, seq: i64) -> anyhow::Result<Vec<RoomMessage>> {
        let mut stmt = self.db.prepare(
            r#"
            SELECT id, seq, sender, sender_kind, hop, content, mentions, origin, created_at
            FROM messages
            WHERE seq > ?1
            ORDER BY seq ASC
            "#,
        )?;
        let rows = stmt
            .query_map([seq], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn recent_messages(&self, limit: usize) -> anyhow::Result<Vec<RoomMessage>> {
        let mut stmt = self.db.prepare(
            r#"
            SELECT id, seq, sender, sender_kind, hop, content, mentions, origin, created_at
            FROM messages
            ORDER BY seq DESC
            LIMIT ?1
            "#,
        )?;
        let mut rows = stmt
            .query_map([limit as i64], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn message_count(&self) -> anyhow::Result<i64> {
        Ok(self
            .db
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?)
    }

    pub fn cursor(&self, consumer: &str) -> anyhow::Result<i64> {
        Ok(self
            .db
            .query_row(
                "SELECT last_seq FROM cursors WHERE consumer = ?1",
                [consumer],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_cursor(&self, consumer: &str, last_seq: i64) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO cursors (consumer, last_seq) VALUES (?1, ?2)",
            params![consumer, last_seq],
        )?;
        Ok(())
    }

    pub fn record_fanout(
        &self,
        inbound_msg_id: &str,
        member_agent_id: &str,
        hop: i64,
        room_msg_ids: &[String],
    ) -> anyhow::Result<()> {
        self.db.execute(
            r#"
            INSERT OR REPLACE INTO fanout (inbound_msg_id, member_agent_id, hop, room_msg_ids, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                inbound_msg_id,
                member_agent_id,
                hop,
                serde_json::to_string(room_msg_ids)?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Hop budget the digest behind `inbound_msg_id` carried, if known.
    pub fn fanout_hop(&self, inbound_msg_id: &str) -> anyhow::Result<Option<i64>> {
        Ok(self
            .db
            .query_row(
                "SELECT hop FROM fanout WHERE inbound_msg_id = ?1",
                [inbound_msg_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn last_post_at(&self, sender: &str) -> anyhow::Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT created_at FROM messages WHERE sender = ?1 ORDER BY seq DESC LIMIT 1",
                [sender],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }))
    }
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RoomMessage> {
    let kind: String = row.get(3)?;
    let mentions: String = row.get(6)?;
    Ok(RoomMessage {
        id: row.get(0)?,
        seq: row.get(1)?,
        sender: row.get(2)?,
        sender_kind: SenderKind::parse(&kind),
        hop: row.get(4)?,
        content: row.get(5)?,
        mentions: serde_json::from_str(&mentions).unwrap_or_default(),
        origin: row.get(7)?,
        created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(8)?)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
    })
}

/// Extract `@agent-id` mentions of room members (plus `@all` / `@everyone`).
pub fn parse_mentions(text: &str, config: &RoomConfig) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut mentions = Vec::new();
    if contains_mention(&lower, MENTION_ALL) || contains_mention(&lower, "everyone") {
        mentions.push(MENTION_ALL.to_string());
    }
    for member in &config.members {
        if contains_mention(&lower, &member.agent_id.to_ascii_lowercase()) {
            mentions.push(member.agent_id.clone());
        }
    }
    mentions
}

fn contains_mention(lower_text: &str, lower_token: &str) -> bool {
    let needle = format!("@{lower_token}");
    let mut start = 0;
    while let Some(found) = lower_text[start..].find(&needle) {
        let end = start + found + needle.len();
        // Require a word boundary so `@plan` does not match `@planner`.
        let boundary = lower_text[end..]
            .chars()
            .next()
            .map(|ch| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
            .unwrap_or(true);
        if boundary {
            return true;
        }
        start = end;
    }
    false
}

fn mentions_member(message: &RoomMessage, member: &RoomMember) -> bool {
    message
        .mentions
        .iter()
        .any(|mention| mention == &member.agent_id || mention == MENTION_ALL)
}

/// Should `message` be delivered to `member`?
pub fn relevant_to(member: &RoomMember, policy: &RoomPolicy, message: &RoomMessage) -> bool {
    if message.sender == member.agent_id {
        return false;
    }
    if message.sender_kind == SenderKind::Agent && message.hop >= policy.hop_limit {
        return false;
    }
    match policy.mode {
        RoomMode::Open => true,
        RoomMode::MentionOnly => {
            mentions_member(message, member)
                || (message.sender_kind == SenderKind::User && message.mentions.is_empty())
        }
    }
}

/// Reply token an agent uses to abstain from a turn.
pub fn is_pass(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_matches(|ch| matches!(ch, '[' | ']' | '*' | '`' | '"'))
        .trim()
        .trim_end_matches(['.', '!']);
    trimmed.eq_ignore_ascii_case("pass")
}

/// A batch of new room messages prepared for one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub text: String,
    /// Relay budget carried forward: max hop of the included messages.
    pub hop: i64,
    pub message_ids: Vec<String>,
    pub first_seq: i64,
    pub last_seq: i64,
}

pub fn build_digest(messages: &[&RoomMessage]) -> Option<Digest> {
    if messages.is_empty() {
        return None;
    }
    let text = messages
        .iter()
        .map(|message| format!("{}: {}", message.sender, message.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(Digest {
        text,
        hop: messages.iter().map(|message| message.hop).max().unwrap_or(0),
        message_ids: messages.iter().map(|message| message.id.clone()).collect(),
        first_seq: messages.iter().map(|message| message.seq).min().unwrap_or(0),
        last_seq: messages.iter().map(|message| message.seq).max().unwrap_or(0),
    })
}

const TRANSCRIPT_TAIL_MESSAGES: usize = 30;
const TRANSCRIPT_TAIL_CHARS: usize = 6000;

/// Render the full turn prompt for one member: identity, roster, goal, the
/// A2A protocol rules, recent room context, and the new digest.
pub fn render_room_prompt(
    config: &RoomConfig,
    member: &RoomMember,
    digest: &Digest,
    store: &RoomStore,
) -> anyhow::Result<String> {
    let roster = config
        .members
        .iter()
        .map(|m| {
            let role = m.role.as_deref().unwrap_or("member");
            let you = if m.agent_id == member.agent_id {
                " (you)"
            } else {
                ""
            };
            format!("- @{} — {role}{you}", m.agent_id)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let transcript = store
        .recent_messages(TRANSCRIPT_TAIL_MESSAGES + digest.message_ids.len())?
        .into_iter()
        .filter(|message| message.seq < digest.first_seq)
        .map(|message| format!("{}: {}", message.sender, message.content.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let transcript = truncate_tail(&transcript, TRANSCRIPT_TAIL_CHARS);
    let transcript = if transcript.is_empty() {
        "(no earlier messages)".to_string()
    } else {
        transcript
    };

    let role = member
        .role
        .as_deref()
        .map(|role| format!(" Your role: {role}."))
        .unwrap_or_default();
    let goal = if config.goal.trim().is_empty() {
        "(no goal set yet — the user will provide one)".to_string()
    } else {
        config.goal.trim().to_string()
    };

    Ok(format!(
        r#"You are agent "@{agent_id}", one member of the multi-agent room "{room_id}".{role}
The room is a group conversation between autonomous agents and human operators, mirrored to a group chat the humans watch. You work with the other members to achieve the room goal by talking, dividing work, and reporting results.

## Room goal
{goal}

## Members
{roster}
Messages from senders prefixed "user:" come from human operators. Treat their instructions as authoritative.

## Protocol
- Reply with exactly the one message you want to post to the room (no preamble, no markdown fences around the whole reply).
- Address a member with @agent-id; use @all for everyone. In mention-only rooms, members only see messages that mention them.
- If you have nothing useful to add right now, reply with exactly: PASS
- Claim work explicitly before doing it ("I'll take the landing page copy") so members don't duplicate effort, and report results back to the room when done.
- Be concise. The room is shared bandwidth; long essays slow everyone down.
- Agent-to-agent threads have a relay budget (hop limit {hop_limit}). Deep back-and-forth gets cut off, so converge fast: propose, decide, act.

## Recent room transcript
{transcript}

## New messages for you
{digest}
"#,
        agent_id = member.agent_id,
        room_id = config.room_id,
        hop_limit = config.policy.hop_limit,
        digest = digest.text,
    ))
}

fn truncate_tail(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit {
        return value.to_string();
    }
    format!(
        "[older messages omitted]\n{}",
        value
            .chars()
            .skip(count.saturating_sub(limit))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> RoomConfig {
        RoomConfig {
            room_id: "launch".to_string(),
            goal: "Ship a website".to_string(),
            members: vec![
                RoomMember {
                    agent_id: "planner".to_string(),
                    session_id: "room-main".to_string(),
                    role: Some("project lead".to_string()),
                },
                RoomMember {
                    agent_id: "builder".to_string(),
                    session_id: "room-main".to_string(),
                    role: None,
                },
            ],
            policy: RoomPolicy::default(),
            bridges: RoomBridges::default(),
        }
    }

    fn message(sender: &str, kind: SenderKind, hop: i64, mentions: Vec<&str>) -> RoomMessage {
        RoomMessage {
            id: format!("room-{sender}-{hop}"),
            seq: 1,
            sender: sender.to_string(),
            sender_kind: kind,
            hop,
            content: "hello".to_string(),
            mentions: mentions.into_iter().map(str::to_string).collect(),
            origin: "cli".to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn mentions_match_whole_member_ids_only() {
        let config = test_config();
        assert_eq!(
            parse_mentions("@planner please draft, cc @ALL", &config),
            vec!["all".to_string(), "planner".to_string()]
        );
        // `@plan` must not match `@planner`, and unknown ids are ignored.
        assert!(parse_mentions("@plan @stranger", &config).is_empty());
        assert_eq!(
            parse_mentions("@builder: status?", &config),
            vec!["builder".to_string()]
        );
    }

    #[test]
    fn open_mode_delivers_everything_except_own_and_over_budget() {
        let config = test_config();
        let planner = &config.members[0];
        let policy = &config.policy;
        assert!(relevant_to(
            planner,
            policy,
            &message("user:anders", SenderKind::User, 0, vec![])
        ));
        assert!(relevant_to(
            planner,
            policy,
            &message("builder", SenderKind::Agent, 3, vec![])
        ));
        // Own messages never come back.
        assert!(!relevant_to(
            planner,
            policy,
            &message("planner", SenderKind::Agent, 1, vec![])
        ));
        // Hop budget exhausted: not fanned out.
        assert!(!relevant_to(
            planner,
            policy,
            &message("builder", SenderKind::Agent, policy.hop_limit, vec![])
        ));
    }

    #[test]
    fn mention_only_mode_requires_address_or_user_broadcast() {
        let config = test_config();
        let planner = &config.members[0];
        let policy = RoomPolicy {
            mode: RoomMode::MentionOnly,
            ..RoomPolicy::default()
        };
        assert!(relevant_to(
            planner,
            &policy,
            &message("builder", SenderKind::Agent, 1, vec!["planner"])
        ));
        assert!(relevant_to(
            planner,
            &policy,
            &message("builder", SenderKind::Agent, 1, vec!["all"])
        ));
        assert!(!relevant_to(
            planner,
            &policy,
            &message("builder", SenderKind::Agent, 1, vec!["builder"])
        ));
        // User broadcast with no mentions reaches everyone…
        assert!(relevant_to(
            planner,
            &policy,
            &message("user:anders", SenderKind::User, 0, vec![])
        ));
        // …but an addressed user message only reaches its targets.
        assert!(!relevant_to(
            planner,
            &policy,
            &message("user:anders", SenderKind::User, 0, vec!["builder"])
        ));
    }

    #[test]
    fn pass_detection_is_strict() {
        assert!(is_pass("PASS"));
        assert!(is_pass("pass."));
        assert!(is_pass("[PASS]"));
        assert!(is_pass("`pass`"));
        assert!(!is_pass("Pass the landing page to @builder"));
        assert!(!is_pass("I'll pass on this"));
    }

    #[test]
    fn digest_carries_max_hop_and_last_seq() {
        let mut first = message("user:anders", SenderKind::User, 0, vec![]);
        first.seq = 4;
        first.content = "build me a website".to_string();
        let mut second = message("builder", SenderKind::Agent, 2, vec![]);
        second.seq = 7;
        second.content = "started on it".to_string();
        let digest = build_digest(&[&first, &second]).unwrap();
        assert_eq!(digest.hop, 2);
        assert_eq!(digest.first_seq, 4);
        assert_eq!(digest.last_seq, 7);
        assert_eq!(
            digest.text,
            "user:anders: build me a website\n\nbuilder: started on it"
        );
        assert!(build_digest(&[]).is_none());
    }

    #[test]
    fn store_round_trip_with_cursors_and_fanout() {
        let dir = std::env::temp_dir().join(format!("maturana-room-{}", Uuid::new_v4()));
        let store = RoomStore::open(&RoomStore::store_path(&dir)).unwrap();
        assert_eq!(store.cursor("member:planner").unwrap(), 0);

        let posted = store
            .post(NewRoomMessage {
                sender: "user:anders",
                sender_kind: SenderKind::User,
                hop: 0,
                content: "kick off",
                mentions: vec!["all".to_string()],
                origin: "telegram",
            })
            .unwrap();
        assert_eq!(posted.seq, 1);

        let after = store.messages_after(0).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, posted.id);
        assert_eq!(after[0].sender_kind, SenderKind::User);
        assert_eq!(after[0].mentions, vec!["all".to_string()]);

        store.set_cursor("member:planner", posted.seq).unwrap();
        assert_eq!(store.cursor("member:planner").unwrap(), 1);
        assert!(store.messages_after(1).unwrap().is_empty());

        store
            .record_fanout("msg-abc", "planner", 3, &[posted.id.clone()])
            .unwrap();
        assert_eq!(store.fanout_hop("msg-abc").unwrap(), Some(3));
        assert_eq!(store.fanout_hop("msg-missing").unwrap(), None);
        assert!(store.last_post_at("user:anders").unwrap().is_some());
        assert!(store.last_post_at("planner").unwrap().is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prompt_contains_protocol_roster_goal_and_digest() {
        let dir = std::env::temp_dir().join(format!("maturana-room-prompt-{}", Uuid::new_v4()));
        let store = RoomStore::open(&RoomStore::store_path(&dir)).unwrap();
        let config = test_config();
        store
            .post(NewRoomMessage {
                sender: "user:anders",
                sender_kind: SenderKind::User,
                hop: 0,
                content: "earlier context",
                mentions: vec![],
                origin: "cli",
            })
            .unwrap();
        let new_message = store
            .post(NewRoomMessage {
                sender: "builder",
                sender_kind: SenderKind::Agent,
                hop: 1,
                content: "@planner what first?",
                mentions: vec!["planner".to_string()],
                origin: "agent",
            })
            .unwrap();
        let digest = build_digest(&[&new_message]).unwrap();
        let prompt =
            render_room_prompt(&config, &config.members[0], &digest, &store).unwrap();
        assert!(prompt.contains("@planner\", one member of the multi-agent room \"launch\""));
        assert!(prompt.contains("Ship a website"));
        assert!(prompt.contains("- @builder — member"));
        assert!(prompt.contains("- @planner — project lead (you)"));
        assert!(prompt.contains("reply with exactly: PASS"));
        assert!(prompt.contains("earlier context"));
        assert!(prompt.contains("builder: @planner what first?"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_validation_rejects_duplicates_and_singletons() {
        let mut config = test_config();
        assert!(config.validate().is_ok());
        config.members[1].agent_id = "planner".to_string();
        assert!(config.validate().is_err());
        config.members.truncate(1);
        assert!(config.validate().is_err());
    }
}
