//! Session queue views over the per-agent sqlite DBs.

use axum::extract::{Path, Query, State};
use axum::response::Response;
use maturana_core::session_db;

use super::{blocking, ok};
use crate::state::AppState;

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
    let root = state.home_root.clone();
    match blocking(move || {
        let paths = session_db::session_paths(&root.join("agents").join(&agent), &session);
        let inbound = session_db::list_recent_inbound(&paths, query.limit.min(200))?;
        let outbound = session_db::list_recent_outbound(&paths, query.limit.min(200))?;
        Ok(serde_json::json!({ "inbound": inbound, "outbound": outbound }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
