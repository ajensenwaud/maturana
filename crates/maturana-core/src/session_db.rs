use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub dir: PathBuf,
    pub inbound_db: PathBuf,
    pub outbound_db: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboundMessage {
    pub id: String,
    pub kind: String,
    pub channel: String,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub content: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundMessage {
    pub id: String,
    pub in_reply_to: Option<String>,
    pub kind: String,
    pub channel: String,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

pub fn session_paths(agent_dir: &Path, session_id: &str) -> SessionPaths {
    let dir = agent_dir.join("sessions").join(session_id);
    SessionPaths {
        inbound_db: dir.join("inbound.sqlite"),
        outbound_db: dir.join("outbound.sqlite"),
        dir,
    }
}

pub fn ensure_session(paths: &SessionPaths) -> anyhow::Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("failed to create {}", paths.dir.display()))?;
    let inbound = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&inbound)?;
    let outbound = open_rw(&paths.outbound_db)?;
    ensure_outbound_schema(&outbound)?;
    Ok(())
}

pub fn insert_inbound(
    paths: &SessionPaths,
    kind: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    content: &str,
) -> anyhow::Result<String> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let id = format!("msg-{}", Uuid::new_v4());
    let seq = next_seq(&db, "messages_in")?;
    let now = Utc::now().to_rfc3339();
    db.execute(
        r#"
        INSERT INTO messages_in
          (id, seq, kind, channel, platform_id, thread_id, content, status, tries, created_at, status_changed)
        VALUES
          (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, ?8)
        "#,
        params![id, seq, kind, channel, platform_id, thread_id, content, now],
    )?;
    Ok(id)
}

/// Cancel queued (not-yet-claimed) inbound work for a session, returning how
/// many pending messages were dropped. A message already being processed by a
/// worker is left alone — the in-guest turn runs to completion — so this clears
/// a backlog without corrupting an in-flight turn. Used by the channel `/stop`
/// command to drop queued messages the user no longer wants answered.
pub fn cancel_pending_inbound(paths: &SessionPaths) -> anyhow::Result<usize> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let n = db.execute(
        "UPDATE messages_in SET status = 'failed', status_changed = ?1 WHERE status = 'pending'",
        params![now],
    )?;
    Ok(n)
}

/// Request cancellation of any IN-PROGRESS (`processing`) turn for this session:
/// records its message id so the polling guest worker can abort the live harness
/// run mid-turn. Unlike [`cancel_pending_inbound`] (which only drops queued work),
/// this reaches a turn the worker has already claimed. Returns how many in-flight
/// turns were flagged (0 = nothing is currently running). The worker clears the
/// flag when it finishes (`mark_inbound_completed`).
pub fn request_cancel_in_progress(paths: &SessionPaths) -> anyhow::Result<usize> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let n = db.execute(
        "INSERT OR IGNORE INTO cancels (message_id, requested_at)
         SELECT id, ?1 FROM messages_in WHERE status = 'processing'",
        params![now],
    )?;
    Ok(n)
}

/// Whether a specific in-flight message has a pending cancel request. The guest
/// worker polls this (via sessiond) while the harness runs and aborts if true.
pub fn is_cancel_requested(paths: &SessionPaths, message_id: &str) -> anyhow::Result<bool> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let found: Option<i64> = db
        .query_row(
            "SELECT 1 FROM cancels WHERE message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Visibility / retry policy for the inbound work queue.
///
/// A claimed message is leased for `lease_seconds`. If the worker that claimed
/// it does not mark it completed within the lease, the message is recovered:
/// requeued (with backoff) when it still has retries left, or dead-lettered
/// once it has been attempted `max_tries` times. This is what keeps a crashed
/// guest turn from wedging a message in `processing` forever.
#[derive(Debug, Clone, Copy)]
pub struct ClaimPolicy {
    pub lease_seconds: i64,
    pub max_tries: i64,
    pub backoff_seconds: i64,
}

impl Default for ClaimPolicy {
    fn default() -> Self {
        Self {
            // Lease must comfortably exceed the in-guest harness timeout (240s) plus
            // the per-turn node/runtime boot + curl/MCP retries, or a slow-but-alive
            // turn gets reclaimed mid-flight and runs as a duplicate (and the user
            // sees it "never finish"). 420s leaves ~3min of headroom over 240s.
            lease_seconds: 420,
            max_tries: 5,
            backoff_seconds: 15,
        }
    }
}

/// A message that was just dead-lettered (set `failed` after exhausting retries).
/// Returned by [`recover_stuck_inbound`] so the caller can notify the user instead
/// of letting the request vanish silently.
#[derive(Debug, Clone)]
pub struct DeadLetteredInbound {
    pub id: String,
    pub channel: String,
    pub platform_id: String,
    pub thread_id: Option<String>,
}

pub fn claim_pending_inbound(
    paths: &SessionPaths,
    limit: usize,
) -> anyhow::Result<Vec<InboundMessage>> {
    claim_pending_inbound_with_policy(paths, limit, ClaimPolicy::default())
}

pub fn claim_pending_inbound_with_policy(
    paths: &SessionPaths,
    limit: usize,
    policy: ClaimPolicy,
) -> anyhow::Result<Vec<InboundMessage>> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    // Reclaim stuck turns; anything that exhausted its retry budget is dead-lettered
    // AND gets a synthetic reply so the request is never silently dropped — the user
    // always hears back, even when a turn died without producing an answer.
    for dead in recover_stuck_inbound(&db, policy)? {
        let body = serde_json::json!({
            "text": "⚠️ I couldn't finish your previous request — it failed after several attempts (it likely ran too long or the agent crashed mid-task). Please try again, ideally broken into a smaller step."
        })
        .to_string();
        let _ = write_outbound(
            paths,
            Some(&dead.id),
            "chat",
            &dead.channel,
            &dead.platform_id,
            dead.thread_id.as_deref(),
            &body,
        );
    }
    let messages = {
        let mut stmt = db.prepare(
            r#"
            SELECT id, kind, channel, platform_id, thread_id, content, status, created_at
            FROM messages_in
            WHERE status = 'pending'
              AND (process_after IS NULL OR datetime(process_after) <= datetime('now'))
            ORDER BY seq ASC
            LIMIT ?1
            "#,
        )?;
        let rows = stmt
            .query_map([limit as i64], inbound_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let now = Utc::now().to_rfc3339();
    let mut stmt = db.prepare(
        "UPDATE messages_in SET status = 'processing', tries = tries + 1, status_changed = ?1 WHERE id = ?2",
    )?;
    for message in &messages {
        stmt.execute(params![now, message.id])?;
    }
    Ok(messages)
}

/// Reclaim messages whose lease has expired. Messages still under their retry
/// budget go back to `pending` with a backoff; messages that have exhausted
/// `max_tries` are dead-lettered to `failed` so they stop blocking the queue
/// and become visible to operators via [`queue_stats`] / [`list_dead_letters`].
fn recover_stuck_inbound(
    db: &Connection,
    policy: ClaimPolicy,
) -> anyhow::Result<Vec<DeadLetteredInbound>> {
    let lease = policy.lease_seconds.max(0);
    let backoff = policy.backoff_seconds.max(0);
    let max_tries = policy.max_tries.max(1);
    let now = Utc::now().to_rfc3339();
    db.execute(
        &format!(
            r#"
            UPDATE messages_in
            SET status = 'pending',
                status_changed = ?1,
                process_after = datetime('now', '+{backoff} seconds')
            WHERE status = 'processing'
              AND tries < ?2
              AND datetime(status_changed, '+{lease} seconds') <= datetime('now')
            "#
        ),
        params![now, max_tries],
    )?;
    // Capture the rows about to be dead-lettered (same predicate as the failing
    // UPDATE) so the caller can notify the user — otherwise an exhausted request
    // just vanishes to `failed` with no reply ever delivered.
    let dead: Vec<DeadLetteredInbound> = {
        let mut stmt = db.prepare(&format!(
            r#"
            SELECT id, channel, platform_id, thread_id
            FROM messages_in
            WHERE status = 'processing'
              AND tries >= ?1
              AND datetime(status_changed, '+{lease} seconds') <= datetime('now')
            "#
        ))?;
        let rows = stmt
            .query_map(params![max_tries], |row| {
                Ok(DeadLetteredInbound {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    platform_id: row.get(2)?,
                    thread_id: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    db.execute(
        &format!(
            r#"
            UPDATE messages_in
            SET status = 'failed', status_changed = ?1
            WHERE status = 'processing'
              AND tries >= ?2
              AND datetime(status_changed, '+{lease} seconds') <= datetime('now')
            "#
        ),
        params![now, max_tries],
    )?;
    Ok(dead)
}

/// Aggregate counts per status, for `maturana doctor` / orchestrator health.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueStats {
    pub pending: i64,
    pub processing: i64,
    pub completed: i64,
    pub failed: i64,
}

pub fn queue_stats(paths: &SessionPaths) -> anyhow::Result<QueueStats> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let mut stats = QueueStats::default();
    let mut stmt = db.prepare("SELECT status, COUNT(*) FROM messages_in GROUP BY status")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (status, count) = row?;
        match status.as_str() {
            "pending" => stats.pending = count,
            "processing" => stats.processing = count,
            "completed" => stats.completed = count,
            "failed" => stats.failed = count,
            _ => {}
        }
    }
    Ok(stats)
}

/// Dead-lettered messages awaiting operator attention.
pub fn list_dead_letters(paths: &SessionPaths) -> anyhow::Result<Vec<InboundMessage>> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let mut stmt = db.prepare(
        r#"
        SELECT id, kind, channel, platform_id, thread_id, content, status, created_at
        FROM messages_in
        WHERE status = 'failed'
        ORDER BY seq ASC
        "#,
    )?;
    let rows = stmt
        .query_map([], inbound_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Move a dead-lettered message back to `pending` with a fresh retry budget.
pub fn requeue_inbound(paths: &SessionPaths, message_id: &str) -> anyhow::Result<bool> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let changed = db.execute(
        "UPDATE messages_in SET status = 'pending', tries = 0, process_after = NULL, status_changed = ?1 WHERE id = ?2",
        params![now, message_id],
    )?;
    Ok(changed > 0)
}

pub fn mark_inbound_completed(paths: &SessionPaths, message_ids: &[String]) -> anyhow::Result<()> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = db.prepare(
        "UPDATE messages_in SET status = 'completed', status_changed = ?1 WHERE id = ?2",
    )?;
    for id in message_ids {
        stmt.execute(params![now, id])?;
        // Drop any cancel flag now that the turn is done, so it can't bleed into a
        // future turn that happens to reuse polling on this id.
        db.execute("DELETE FROM cancels WHERE message_id = ?1", params![id])?;
    }
    Ok(())
}

pub fn write_outbound(
    paths: &SessionPaths,
    in_reply_to: Option<&str>,
    kind: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    content: &str,
) -> anyhow::Result<String> {
    let db = open_rw(&paths.outbound_db)?;
    ensure_outbound_schema(&db)?;
    let id = format!("out-{}", Uuid::new_v4());
    let seq = next_seq(&db, "messages_out")?;
    let now = Utc::now().to_rfc3339();
    db.execute(
        r#"
        INSERT INTO messages_out
          (id, seq, in_reply_to, kind, channel, platform_id, thread_id, content, created_at)
        VALUES
          (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
        params![
            id,
            seq,
            in_reply_to,
            kind,
            channel,
            platform_id,
            thread_id,
            content,
            now
        ],
    )?;
    Ok(id)
}

/// Most recent inbound messages (any status), newest first. Read-only view
/// for observers like the web cockpit's sessions panel.
pub fn list_recent_inbound(
    paths: &SessionPaths,
    limit: usize,
) -> anyhow::Result<Vec<InboundMessage>> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let mut stmt = db.prepare(
        r#"
        SELECT id, kind, channel, platform_id, thread_id, content, status, created_at
        FROM messages_in
        ORDER BY seq DESC
        LIMIT ?1
        "#,
    )?;
    let messages = stmt
        .query_map(params![limit as i64], inbound_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(messages)
}

/// Most recent outbound messages, newest first. Read-only observer view.
pub fn list_recent_outbound(
    paths: &SessionPaths,
    limit: usize,
) -> anyhow::Result<Vec<OutboundMessage>> {
    let db = open_rw(&paths.outbound_db)?;
    ensure_outbound_schema(&db)?;
    let mut stmt = db.prepare(
        r#"
        SELECT id, in_reply_to, kind, channel, platform_id, thread_id, content, created_at
        FROM messages_out
        ORDER BY seq DESC
        LIMIT ?1
        "#,
    )?;
    let messages = stmt
        .query_map(params![limit as i64], outbound_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(messages)
}

pub fn list_undelivered(paths: &SessionPaths) -> anyhow::Result<Vec<OutboundMessage>> {
    let inbound = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&inbound)?;
    let outbound = open_ro(&paths.outbound_db)?;
    let delivered = delivered_ids(&inbound)?;
    let mut stmt = outbound.prepare(
        r#"
        SELECT id, in_reply_to, kind, channel, platform_id, thread_id, content, created_at
        FROM messages_out
        ORDER BY seq ASC
        "#,
    )?;
    let messages = stmt
        .query_map([], outbound_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(messages
        .into_iter()
        .filter(|message| !delivered.contains(&message.id))
        .collect())
}

/// Find the outbound reply for a given inbound, REGARDLESS of delivered status.
/// Unlike [`list_undelivered`], this still returns the reply after it has been
/// delivered. An active streaming loop uses it to detect "my reply exists" even
/// when a concurrent backstop already delivered it — so the loop can claim-or-
/// clean-up instead of ticking its live bubble forever (the lingering-counter /
/// duplicate-message class). Targeted query (`WHERE in_reply_to = ?`), so it is
/// cheap to poll every tick.
pub fn find_reply_outbound(
    paths: &SessionPaths,
    inbound_id: &str,
) -> anyhow::Result<Option<OutboundMessage>> {
    let outbound = open_ro(&paths.outbound_db)?;
    let mut stmt = outbound.prepare(
        r#"
        SELECT id, in_reply_to, kind, channel, platform_id, thread_id, content, created_at
        FROM messages_out
        WHERE in_reply_to = ?1
        ORDER BY seq ASC
        LIMIT 1
        "#,
    )?;
    let mut rows = stmt.query_map(params![inbound_id], outbound_from_row)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

pub fn mark_delivered(
    paths: &SessionPaths,
    message_id: &str,
    platform_message_id: Option<&str>,
) -> anyhow::Result<()> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    db.execute(
        r#"
        INSERT OR REPLACE INTO delivered (message_id, platform_message_id, delivered_at)
        VALUES (?1, ?2, ?3)
        "#,
        params![message_id, platform_message_id, now],
    )?;
    Ok(())
}

/// Atomically claim an outbound for delivery: returns `true` only for the first
/// caller. Channels have several delivery paths (the per-channel delivery thread,
/// the streaming render loop, the inline fallback) that each read
/// `list_undelivered` then send — without an atomic claim the same reply goes out
/// multiple times. The winning claimer sends, then records the platform id via
/// `mark_delivered`.
pub fn claim_delivery(paths: &SessionPaths, message_id: &str) -> anyhow::Result<bool> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let changed = db.execute(
        "INSERT OR IGNORE INTO delivered (message_id, platform_message_id, delivered_at) VALUES (?1, NULL, ?2)",
        params![message_id, now],
    )?;
    Ok(changed > 0)
}

/// Release a claim that did NOT result in a delivered message, so a later pass
/// can retry it. Only removes an un-finalized claim (NULL platform id), so a
/// reply that was actually sent (and `mark_delivered` recorded a platform id) is
/// never accidentally un-claimed and re-sent. Use this when a `claim_delivery`
/// winner fails to reach Telegram, instead of leaving the reply claimed-but-unsent
/// (which would drop it from `list_undelivered` forever).
pub fn unclaim_delivery(paths: &SessionPaths, message_id: &str) -> anyhow::Result<()> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    db.execute(
        "DELETE FROM delivered WHERE message_id = ?1 AND platform_message_id IS NULL",
        params![message_id],
    )?;
    Ok(())
}

/// One distilled progress event for an in-flight turn, written by the guest
/// worker as the harness streams its work. Stored in a per-message JSONL
/// side-lane SEPARATE from the inbound/outbound queues, so turn delivery and
/// `agent run --wait` never see it — it exists purely so channels can show what
/// the agent is doing before the final reply lands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgressEvent {
    pub seq: u64,
    /// "tool" | "thinking" | "text" | "status" | "done"
    pub kind: String,
    pub text: String,
}

/// Path to the progress side-lane for one in-flight message.
pub fn progress_path(paths: &SessionPaths, message_id: &str) -> PathBuf {
    let safe: String = message_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    paths.dir.join("progress").join(format!("{safe}.jsonl"))
}

/// Append a progress event for `message_id`. Append-only JSONL; cheap enough for
/// the high-frequency (per-harness-event) writes the worker makes.
pub fn append_progress(
    paths: &SessionPaths,
    message_id: &str,
    event: &ProgressEvent,
) -> anyhow::Result<()> {
    use std::io::Write;
    let path = progress_path(paths, message_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open progress log {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

/// Read all progress events recorded for `message_id`, in order. Missing file →
/// empty; malformed lines are skipped (a half-written line during a concurrent
/// append is simply ignored until the next poll).
pub fn read_progress(paths: &SessionPaths, message_id: &str) -> anyhow::Result<Vec<ProgressEvent>> {
    let path = progress_path(paths, message_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    Ok(raw
        .lines()
        .filter_map(|line| serde_json::from_str::<ProgressEvent>(line.trim()).ok())
        .collect())
}

/// Drop the progress side-lane for `message_id` once its final reply is
/// delivered, so it doesn't accumulate. Missing file is fine.
pub fn clear_progress(paths: &SessionPaths, message_id: &str) -> anyhow::Result<()> {
    let path = progress_path(paths, message_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn open_rw(path: &Path) -> anyhow::Result<Connection> {
    let db =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    tune(&db)?;
    Ok(db)
}

fn open_ro(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open {}", path.display()))?;
    db.pragma_update(None, "busy_timeout", 5000)?;
    Ok(db)
}

fn tune(db: &Connection) -> anyhow::Result<()> {
    // WAL so readers never block on the writer. The host runs several processes
    // against these per-session DBs at once (the channel stream loop reading
    // `list_undelivered` every tick, sessiond writing claims/outbounds, the
    // delivery threads, proactive/scheduler). In rollback mode a write blocked the
    // stream loop's read, which stalled the synchronous loop and made the live
    // "Thinking…" counter jump in multi-second steps. WAL lets the read proceed
    // concurrently, so the counter ticks smoothly. (Guests never open these files
    // directly — they go through sessiond over HTTP — so this is host-only.)
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    db.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

fn ensure_inbound_schema(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS messages_in (
            id TEXT PRIMARY KEY,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL,
            channel TEXT NOT NULL,
            platform_id TEXT NOT NULL,
            thread_id TEXT,
            content TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            tries INTEGER NOT NULL DEFAULT 0,
            process_after TEXT,
            created_at TEXT NOT NULL,
            status_changed TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS delivered (
            message_id TEXT PRIMARY KEY,
            platform_message_id TEXT,
            delivered_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS cancels (
            message_id TEXT PRIMARY KEY,
            requested_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn ensure_outbound_schema(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS messages_out (
            id TEXT PRIMARY KEY,
            seq INTEGER NOT NULL,
            in_reply_to TEXT,
            kind TEXT NOT NULL,
            channel TEXT NOT NULL,
            platform_id TEXT NOT NULL,
            thread_id TEXT,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS processing_ack (
            message_id TEXT PRIMARY KEY,
            status TEXT NOT NULL,
            status_changed TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS runner_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            provider TEXT,
            continuation TEXT,
            heartbeat_at TEXT
        );
        "#,
    )?;
    Ok(())
}

fn delivered_ids(db: &Connection) -> anyhow::Result<HashSet<String>> {
    let mut stmt = db.prepare("SELECT message_id FROM delivered")?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(ids)
}

fn inbound_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboundMessage> {
    Ok(InboundMessage {
        id: row.get(0)?,
        kind: row.get(1)?,
        channel: row.get(2)?,
        platform_id: row.get(3)?,
        thread_id: row.get(4)?,
        content: row.get(5)?,
        status: row.get(6)?,
        created_at: parse_utc(row.get(7)?)?,
    })
}

fn outbound_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboundMessage> {
    Ok(OutboundMessage {
        id: row.get(0)?,
        in_reply_to: row.get(1)?,
        kind: row.get(2)?,
        channel: row.get(3)?,
        platform_id: row.get(4)?,
        thread_id: row.get(5)?,
        content: row.get(6)?,
        created_at: parse_utc(row.get(7)?)?,
    })
}

fn parse_utc(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn next_seq(db: &Connection, table: &str) -> anyhow::Result<i64> {
    let sql = format!("SELECT COALESCE(MAX(seq), 0) + 1 FROM {table}");
    Ok(db.query_row(&sql, [], |row| row.get::<_, i64>(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trip_preserves_inbound_outbound_delivery_state() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();

        let inbound_id = insert_inbound(
            &paths,
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"hello"}"#,
        )
        .unwrap();

        let claimed = claim_pending_inbound(&paths, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, inbound_id);
        write_outbound(
            &paths,
            Some(&inbound_id),
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"hi"}"#,
        )
        .unwrap();
        mark_inbound_completed(&paths, &[inbound_id]).unwrap();

        let undelivered = list_undelivered(&paths).unwrap();
        assert_eq!(undelivered.len(), 1);
        mark_delivered(&paths, &undelivered[0].id, Some("telegram-1")).unwrap();
        assert!(list_undelivered(&paths).unwrap().is_empty());
    }

    #[test]
    fn cancel_pending_inbound_drops_queued_but_not_in_flight() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();

        for text in ["a", "b", "c"] {
            insert_inbound(&paths, "chat", "telegram", "chat-1", None, text).unwrap();
        }
        // One turn is already being processed by a worker…
        let claimed = claim_pending_inbound(&paths, 1).unwrap();
        assert_eq!(claimed.len(), 1);

        // …/stop clears the two still-queued messages, leaving the in-flight one.
        let cancelled = cancel_pending_inbound(&paths).unwrap();
        assert_eq!(cancelled, 2);

        // Nothing left to claim (the in-flight one stays in 'processing').
        assert!(claim_pending_inbound(&paths, 10).unwrap().is_empty());
    }

    #[test]
    fn request_cancel_in_progress_flags_claimed_turn_and_clears_on_complete() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();

        let id = insert_inbound(
            &paths,
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"hi"}"#,
        )
        .unwrap();
        // Not claimed yet → nothing in progress to cancel, and the flag is unset.
        assert_eq!(request_cancel_in_progress(&paths).unwrap(), 0);
        assert!(!is_cancel_requested(&paths, &id).unwrap());

        // The worker claims it (status → processing).
        assert_eq!(claim_pending_inbound(&paths, 1).unwrap().len(), 1);

        // /stop flags the in-flight turn; the worker's poll would see it.
        assert_eq!(request_cancel_in_progress(&paths).unwrap(), 1);
        assert!(is_cancel_requested(&paths, &id).unwrap());
        // Idempotent: a second /stop doesn't double-flag.
        assert_eq!(request_cancel_in_progress(&paths).unwrap(), 0);

        // Completing the turn clears the flag so it can't bleed into a later turn.
        mark_inbound_completed(&paths, &[id.clone()]).unwrap();
        assert!(!is_cancel_requested(&paths, &id).unwrap());
    }

    #[test]
    fn progress_lane_is_independent_of_the_outbound_queue() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();

        let msg = "msg-abc-123";
        assert!(read_progress(&paths, msg).unwrap().is_empty());

        for (seq, kind, text) in [
            (0u64, "status", "thinking"),
            (1, "tool", "web_search"),
            (2, "text", "Here is the"),
        ] {
            append_progress(
                &paths,
                msg,
                &ProgressEvent {
                    seq,
                    kind: kind.into(),
                    text: text.into(),
                },
            )
            .unwrap();
        }
        let events = read_progress(&paths, msg).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[1],
            ProgressEvent {
                seq: 1,
                kind: "tool".into(),
                text: "web_search".into()
            }
        );

        // Writing progress must not enqueue anything deliverable, and the final
        // outbound is unaffected by progress (the safety property: delivery and
        // `agent run --wait` never see the side-lane).
        write_outbound(
            &paths,
            Some(msg),
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"done"}"#,
        )
        .unwrap();
        assert_eq!(list_undelivered(&paths).unwrap().len(), 1);
        assert_eq!(read_progress(&paths, msg).unwrap().len(), 3);

        clear_progress(&paths, msg).unwrap();
        assert!(read_progress(&paths, msg).unwrap().is_empty());
        assert_eq!(list_undelivered(&paths).unwrap().len(), 1);
    }

    #[test]
    fn claim_delivery_is_atomically_once_only() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();
        write_outbound(
            &paths,
            Some("in-1"),
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"hi"}"#,
        )
        .unwrap();
        let id = list_undelivered(&paths).unwrap()[0].id.clone();
        // First claimer wins, all subsequent claimers lose — the dedup guarantee
        // that stops several delivery paths sending the same reply.
        assert!(claim_delivery(&paths, &id).unwrap());
        assert!(!claim_delivery(&paths, &id).unwrap());
        assert!(!claim_delivery(&paths, &id).unwrap());
        // A claimed outbound is no longer undelivered.
        assert!(list_undelivered(&paths).unwrap().is_empty());
    }

    #[test]
    fn unclaim_reopens_an_undelivered_claim_but_not_a_delivered_one() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();
        write_outbound(
            &paths,
            Some("in-1"),
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"hi"}"#,
        )
        .unwrap();
        let id = list_undelivered(&paths).unwrap()[0].id.clone();

        // A claim that failed to actually send is released → the reply is
        // retryable again, never silently dropped.
        assert!(claim_delivery(&paths, &id).unwrap());
        assert!(list_undelivered(&paths).unwrap().is_empty());
        unclaim_delivery(&paths, &id).unwrap();
        assert_eq!(list_undelivered(&paths).unwrap().len(), 1);

        // Once it is actually delivered (a real platform id recorded), unclaim is a
        // no-op: a sent reply is never re-opened for a duplicate send.
        assert!(claim_delivery(&paths, &id).unwrap());
        mark_delivered(&paths, &id, Some("telegram-99")).unwrap();
        unclaim_delivery(&paths, &id).unwrap();
        assert!(list_undelivered(&paths).unwrap().is_empty());
    }

    #[test]
    fn expired_lease_requeues_then_dead_letters_a_crashed_turn() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();
        insert_inbound(
            &paths,
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"x"}"#,
        )
        .unwrap();

        // Lease 0 + max_tries 2 means: every claim that is not completed is
        // immediately recoverable on the next claim, and the message is dead-
        // lettered once it has been attempted twice.
        let policy = ClaimPolicy {
            lease_seconds: 0,
            max_tries: 2,
            backoff_seconds: 0,
        };

        // First claim leases the message (tries -> 1) but the "worker" crashes
        // without completing it.
        let first = claim_pending_inbound_with_policy(&paths, 1, policy).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(queue_stats(&paths).unwrap().processing, 1);

        // Second claim recovers the expired lease and re-leases it (tries -> 2).
        let second = claim_pending_inbound_with_policy(&paths, 1, policy).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, first[0].id);

        // Third claim: retry budget is exhausted, so recovery dead-letters it
        // instead of re-leasing. Nothing is handed out.
        let third = claim_pending_inbound_with_policy(&paths, 1, policy).unwrap();
        assert!(third.is_empty());
        let stats = queue_stats(&paths).unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.processing, 0);

        let dead = list_dead_letters(&paths).unwrap();
        assert_eq!(dead.len(), 1);
        assert!(requeue_inbound(&paths, &dead[0].id).unwrap());
        assert_eq!(queue_stats(&paths).unwrap().pending, 1);
    }

    #[test]
    fn healthy_lease_is_not_reclaimed_while_in_flight() {
        let temp = temp_dir();
        let paths = session_paths(&temp, "telegram-main");
        ensure_session(&paths).unwrap();
        insert_inbound(
            &paths,
            "chat",
            "telegram",
            "chat-1",
            None,
            r#"{"text":"x"}"#,
        )
        .unwrap();

        // A long lease means a still-running turn is never double-claimed.
        let policy = ClaimPolicy {
            lease_seconds: 300,
            max_tries: 5,
            backoff_seconds: 15,
        };
        let first = claim_pending_inbound_with_policy(&paths, 1, policy).unwrap();
        assert_eq!(first.len(), 1);
        let second = claim_pending_inbound_with_policy(&paths, 1, policy).unwrap();
        assert!(
            second.is_empty(),
            "in-flight lease must not be re-handed-out"
        );
        assert_eq!(queue_stats(&paths).unwrap().processing, 1);
    }

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("maturana-session-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
