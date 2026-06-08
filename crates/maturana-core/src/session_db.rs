use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
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

pub fn claim_pending_inbound(
    paths: &SessionPaths,
    limit: usize,
) -> anyhow::Result<Vec<InboundMessage>> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
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

pub fn mark_inbound_completed(paths: &SessionPaths, message_ids: &[String]) -> anyhow::Result<()> {
    let db = open_rw(&paths.inbound_db)?;
    ensure_inbound_schema(&db)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = db.prepare(
        "UPDATE messages_in SET status = 'completed', status_changed = ?1 WHERE id = ?2",
    )?;
    for id in message_ids {
        stmt.execute(params![now, id])?;
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
    db.pragma_update(None, "journal_mode", "DELETE")?;
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

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("maturana-session-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
