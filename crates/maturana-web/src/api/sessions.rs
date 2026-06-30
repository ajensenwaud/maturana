//! Session queue views over the per-agent sqlite DBs: list, transcript, plus
//! search / export / prune / label (the "sessions depth" features).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use maturana_core::session_db;

use super::{blocking, err, ok, valid_id};
use crate::state::AppState;

/// Reject agent/session ids that aren't safe path segments (traversal guard).
macro_rules! check_ids {
    ($agent:expr, $session:expr) => {
        if !valid_id(&$agent) || !valid_id(&$session) {
            return err(StatusCode::BAD_REQUEST, "invalid agent or session id");
        }
    };
}

/// Channels/kinds that are host-internal self-triggers, NOT part of the
/// user<->agent conversation: the proactive self-check, heartbeats, schedule
/// fires, and orchestration steps. A genuine proactive *message* to the user is
/// routed to a real channel (web/telegram) and is NOT tagged with these, so it
/// still shows; only the internal trigger prompts are hidden.
const INTERNAL_TURN_TAGS: &[&str] = &["proactive", "heartbeat", "schedule", "orchestrate"];

/// The agent's "say nothing" reply to a proactive/heartbeat check. Mirrors
/// `crate::proactive::SILENCE_SENTINEL` in maturana-cli — kept in sync here as a
/// stable protocol constant (maturana-web can't depend on the cli crate).
const SILENCE_SENTINEL: &str = "[[MATURANA_SILENT]]";

fn is_internal_inbound(m: &session_db::InboundMessage) -> bool {
    INTERNAL_TURN_TAGS.contains(&m.kind.as_str())
        || INTERNAL_TURN_TAGS.contains(&m.channel.as_str())
}

fn is_internal_outbound(m: &session_db::OutboundMessage) -> bool {
    INTERNAL_TURN_TAGS.contains(&m.kind.as_str())
        || INTERNAL_TURN_TAGS.contains(&m.channel.as_str())
        || extract_text(&m.content).trim() == SILENCE_SENTINEL
}

/// Pull the display text out of a stored message's JSON `content` blob.
fn extract_text(content: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
            return t.to_string();
        }
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
    }
    content.to_string()
}

/// A short snippet of `text` centered on the first case-insensitive match of
/// `needle_lower` (already lowercased).
fn snippet(text: &str, needle_lower: &str) -> String {
    let lower = text.to_lowercase();
    let Some(pos) = lower.find(needle_lower) else {
        return text.chars().take(120).collect();
    };
    let start = pos.saturating_sub(50);
    let end = (pos + needle_lower.len() + 70).min(text.len());
    let mut s: String = text.get(start..end).unwrap_or(text).to_string();
    if start > 0 {
        s.insert_str(0, "…");
    }
    if end < text.len() {
        s.push('…');
    }
    s.replace('\n', " ")
}

/// The user-set label for a session, if any (sidecar file; rename is
/// non-destructive — the session_id stays the key).
fn read_label(session_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(session_dir.join("label.txt"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Newest activity timestamp across a session's inbound+outbound.
fn last_activity(paths: &session_db::SessionPaths) -> Option<chrono::DateTime<chrono::Utc>> {
    let i = session_db::list_recent_inbound(paths, 1)
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|m| m.created_at);
    let o = session_db::list_recent_outbound(paths, 1)
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|m| m.created_at);
    match (i, o) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Every (agent, session) pair with queue stats.
pub async fn list(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let mut sessions = Vec::new();
        let agents_dir = root.join("agents");
        if let Ok(agents) = std::fs::read_dir(&agents_dir) {
            for agent in agents.flatten() {
                let agent_id = agent.file_name().to_string_lossy().to_string();
                let sessions_dir = agent.path().join("sessions");
                let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let session_id = entry.file_name().to_string_lossy().to_string();
                    let paths = session_db::session_paths(&agent.path(), &session_id);
                    let stats = session_db::queue_stats(&paths).ok();
                    sessions.push(serde_json::json!({
                        "agent_id": agent_id,
                        "session_id": session_id,
                        "label": read_label(&entry.path()),
                        "last_active": last_activity(&paths).map(|t| t.to_rfc3339()),
                        "stats": stats,
                    }));
                }
            }
        }
        Ok(serde_json::json!(sessions))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct MessagesQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    30
}

pub async fn messages(
    State(state): State<AppState>,
    Path((agent, session)): Path<(String, String)>,
    Query(query): Query<MessagesQuery>,
) -> Response {
    check_ids!(agent, session);
    let root = state.home_root.clone();
    match blocking(move || {
        let paths = session_db::session_paths(&root.join("agents").join(&agent), &session);
        // Hide host-internal self-triggers (proactive checks, silence replies,
        // heartbeats, orchestration) so the transcript reads as the real
        // user<->agent conversation. We over-fetch a little because the filter
        // can drop rows.
        let fetch = (query.limit.min(200)).saturating_mul(2).clamp(1, 400);
        let inbound: Vec<_> = session_db::list_recent_inbound(&paths, fetch)?
            .into_iter()
            .filter(|m| !is_internal_inbound(m))
            .collect();
        let outbound: Vec<_> = session_db::list_recent_outbound(&paths, fetch)?
            .into_iter()
            .filter(|m| !is_internal_outbound(m))
            .collect();
        Ok(serde_json::json!({ "inbound": inbound, "outbound": outbound }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct SearchQuery {
    q: String,
    #[serde(default = "search_limit")]
    limit: usize,
}
fn search_limit() -> usize {
    50
}

/// Full-text-ish search across every session's recent messages (substring,
/// case-insensitive). Returns hits with a snippet, capped at `limit`.
pub async fn search(State(state): State<AppState>, Query(query): Query<SearchQuery>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let needle = query.q.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(serde_json::json!({ "query": query.q, "hits": [] }));
        }
        let limit = query.limit.clamp(1, 500);
        let mut hits: Vec<serde_json::Value> = Vec::new();
        if let Ok(agents) = std::fs::read_dir(root.join("agents")) {
            'outer: for agent in agents.flatten() {
                let agent_id = agent.file_name().to_string_lossy().to_string();
                let Ok(sessions) = std::fs::read_dir(agent.path().join("sessions")) else {
                    continue;
                };
                for session in sessions.flatten() {
                    if !session.path().is_dir() {
                        continue;
                    }
                    let session_id = session.file_name().to_string_lossy().to_string();
                    let paths = session_db::session_paths(&agent.path(), &session_id);
                    let mut scan = |dir: &str, text: String, at: String| {
                        if text.to_lowercase().contains(&needle) {
                            hits.push(serde_json::json!({
                                "agent_id": agent_id,
                                "session_id": session_id,
                                "direction": dir,
                                "snippet": snippet(&text, &needle),
                                "created_at": at,
                            }));
                        }
                    };
                    if let Ok(rows) = session_db::list_recent_inbound(&paths, 400) {
                        for m in rows {
                            if is_internal_inbound(&m) {
                                continue;
                            }
                            scan("in", extract_text(&m.content), m.created_at.to_rfc3339());
                        }
                    }
                    if let Ok(rows) = session_db::list_recent_outbound(&paths, 400) {
                        for m in rows {
                            if is_internal_outbound(&m) {
                                continue;
                            }
                            scan("out", extract_text(&m.content), m.created_at.to_rfc3339());
                        }
                    }
                    if hits.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }
        hits.truncate(limit);
        hits.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
        Ok(serde_json::json!({ "query": query.q, "hits": hits }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Full transcript export (merged inbound+outbound, oldest first) as JSON.
pub async fn export(
    State(state): State<AppState>,
    Path((agent, session)): Path<(String, String)>,
) -> Response {
    check_ids!(agent, session);
    let root = state.home_root.clone();
    match blocking(move || {
        let paths = session_db::session_paths(&root.join("agents").join(&agent), &session);
        let mut msgs: Vec<serde_json::Value> = Vec::new();
        for m in session_db::list_recent_inbound(&paths, 100_000)? {
            msgs.push(serde_json::json!({
                "direction": "in", "kind": m.kind, "channel": m.channel,
                "text": extract_text(&m.content), "created_at": m.created_at.to_rfc3339(),
            }));
        }
        for m in session_db::list_recent_outbound(&paths, 100_000)? {
            msgs.push(serde_json::json!({
                "direction": "out", "kind": m.kind, "channel": m.channel,
                "text": extract_text(&m.content), "created_at": m.created_at.to_rfc3339(),
            }));
        }
        msgs.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
        Ok(serde_json::json!({ "agent_id": agent, "session_id": session, "messages": msgs }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct PruneBody {
    days: i64,
}

/// Delete session directories whose newest message is older than `days`.
/// Sessions with no messages at all are left alone (may be freshly created).
pub async fn prune(State(state): State<AppState>, Json(body): Json<PruneBody>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let days = body.days.clamp(1, 3650);
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let mut deleted: Vec<String> = Vec::new();
        if let Ok(agents) = std::fs::read_dir(root.join("agents")) {
            for agent in agents.flatten() {
                let agent_path = agent.path();
                let agent_id = agent.file_name().to_string_lossy().to_string();
                let Ok(sessions) = std::fs::read_dir(agent_path.join("sessions")) else {
                    continue;
                };
                for session in sessions.flatten() {
                    if !session.path().is_dir() {
                        continue;
                    }
                    let session_id = session.file_name().to_string_lossy().to_string();
                    let paths = session_db::session_paths(&agent_path, &session_id);
                    // Never prune a session that still has un-run work queued.
                    let busy = session_db::queue_stats(&paths)
                        .map(|s| s.pending > 0 || s.processing > 0)
                        .unwrap_or(false);
                    if let Some(last) = last_activity(&paths) {
                        if !busy
                            && last < cutoff
                            && std::fs::remove_dir_all(session.path()).is_ok()
                        {
                            deleted.push(format!("{agent_id}/{session_id}"));
                        }
                    }
                }
            }
        }
        Ok(serde_json::json!({ "older_than_days": days, "count": deleted.len(), "deleted": deleted }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::session_db::{InboundMessage, OutboundMessage};

    fn inbound(kind: &str, channel: &str, text: &str) -> InboundMessage {
        InboundMessage {
            id: "i".into(),
            kind: kind.into(),
            channel: channel.into(),
            platform_id: "p".into(),
            thread_id: None,
            content: serde_json::json!({ "text": text }).to_string(),
            status: "done".into(),
            created_at: chrono::Utc::now(),
        }
    }
    fn outbound(kind: &str, channel: &str, text: &str) -> OutboundMessage {
        OutboundMessage {
            id: "o".into(),
            in_reply_to: None,
            kind: kind.into(),
            channel: channel.into(),
            platform_id: "p".into(),
            thread_id: None,
            content: serde_json::json!({ "text": text }).to_string(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn hides_proactive_and_silence_but_keeps_real_conversation() {
        // The internal proactive self-check prompt is hidden.
        assert!(is_internal_inbound(&inbound(
            "proactive",
            "proactive",
            "[PROACTIVE CHECK - you initiated this...]"
        )));
        // A real user message is kept.
        assert!(!is_internal_inbound(&inbound("chat", "web", "hello there")));
        // The agent's silence reply is hidden, whatever channel it lands on.
        assert!(is_internal_outbound(&outbound(
            "chat",
            "proactive",
            "[[MATURANA_SILENT]]"
        )));
        assert!(is_internal_outbound(&outbound(
            "chat",
            "web",
            "[[MATURANA_SILENT]]"
        )));
        // A genuine agent reply is kept.
        assert!(!is_internal_outbound(&outbound(
            "chat",
            "web",
            "Here's the summary you asked for."
        )));
        // A real proactive *message* (routed to a real channel) is kept.
        assert!(!is_internal_outbound(&outbound(
            "chat",
            "web",
            "Reminder: your build finished."
        )));
    }
}

#[derive(serde::Deserialize)]
pub struct LabelBody {
    label: String,
}

/// Set (or clear, with an empty string) a session's display label. The
/// session_id stays the canonical key — this is a non-destructive sidecar.
pub async fn set_label(
    State(state): State<AppState>,
    Path((agent, session)): Path<(String, String)>,
    Json(body): Json<LabelBody>,
) -> Response {
    check_ids!(agent, session);
    let root = state.home_root.clone();
    match blocking(move || {
        let dir = root
            .join("agents")
            .join(&agent)
            .join("sessions")
            .join(&session);
        if !dir.is_dir() {
            anyhow::bail!("no such session {agent}/{session}");
        }
        let label = body.label.trim();
        let path = dir.join("label.txt");
        if label.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            std::fs::write(&path, label)?;
        }
        Ok(serde_json::json!({ "agent_id": agent, "session_id": session, "label": label }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
