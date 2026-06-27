//! Dashboard REST API: thin `spawn_blocking` wrappers over the sync
//! maturana-core functions. Auth + the mutating-CSRF header are enforced by
//! the middleware in `auth.rs`; everything here can assume an authenticated
//! operator.

pub mod agents;
pub mod egress;
pub mod graph;
pub mod ops;
pub mod pipelock;
pub mod runtime;
pub mod search;
pub mod sessions;
pub mod skills;
pub mod system;
pub mod tools;
pub mod voice;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post, put};
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/overview", get(system::overview))
        .route("/api/agents", get(agents::list).post(agents::create))
        .route("/api/agents/:id/status", get(agents::status))
        .route("/api/agents/:id/detail", get(agents::detail))
        .route("/api/agents/:id/stop", post(agents::stop))
        .route("/api/agents/:id/restart", post(agents::restart))
        .route("/api/agents/:id/deploy-skill", post(agents::deploy_skill))
        .route("/api/agents/:id/files", get(agents::files))
        .route("/api/agents/:id/files/read", get(agents::file_read))
        .route("/api/agents/:id/files/write", post(agents::file_write))
        .route("/api/agents/:id/spec", get(agents::spec_get).put(agents::spec_put))
        .route("/api/agents/:id/spec/validate", post(agents::spec_validate))
        .route("/api/agents/:id/apply", post(agents::apply))
        .route("/api/agents/:id/egress", get(agents::egress_get).put(agents::egress_put))
        .route("/api/agents/:id/config", get(agents::config_get).put(agents::config_put))
        .route("/api/egress/approve", post(egress::approve))
        .route("/api/runtime/plan", get(runtime::plan))
        .route("/api/runtime/up", get(runtime::up_state))
        .route("/api/doctor", get(runtime::doctor))
        .route("/api/system/stats", get(system::stats))
        .route("/api/system/logs", get(system::logs))
        .route("/api/system/logs/sources", get(system::log_sources))
        .route("/api/system/analytics", get(system::analytics))
        .route("/api/ops/gateway/:action", post(ops::gateway))
        .route("/api/ops/backup", post(ops::backup))
        .route("/api/sessions", get(sessions::list))
        .route("/api/sessions/search", get(sessions::search))
        .route("/api/sessions/prune", post(sessions::prune))
        .route("/api/sessions/:agent/:session/messages", get(sessions::messages))
        .route("/api/sessions/:agent/:session/export", get(sessions::export))
        .route("/api/sessions/:agent/:session/label", put(sessions::set_label))
        .route("/api/graph/stats", post(graph::stats))
        .route("/api/graph/query", post(graph::query))
        .route("/api/graph/ingest", post(graph::ingest))
        .route("/api/pipelock/secrets", get(pipelock::list).post(pipelock::set))
        .route("/api/pipelock/secrets/:name", axum::routing::delete(pipelock::delete))
        .route("/api/search", post(search::search))
        .route("/api/voice/tts", post(voice::tts))
        .route("/api/voice/stt", post(voice::stt))
        .route("/api/tools", get(tools::list))
        .route("/api/skills", get(skills::list).post(skills::create))
        .route("/api/skills/:name", get(skills::detail))
        // PUT routes share the same mutating-CSRF gate as POST/DELETE.
        .route("/api/_csrf_probe", put(|| async { ok(serde_json::json!({})) }))
}

/// Run sync core code off the async runtime, flattening join + app errors.
pub async fn blocking<T, F>(work: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(err(StatusCode::BAD_REQUEST, &format!("{error:#}"))),
        Err(join_error) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("task panicked: {join_error}"),
        )),
    }
}

/// True for an id/name safe to use as a single path segment: non-empty, ≤128
/// chars, only `[A-Za-z0-9._-]`, and never a `..` traversal. Guards every handler
/// that builds a filesystem path from a URL segment (agent/session ids, log
/// filenames) — axum's Path/Query extractors percent-decode, so `%2e%2e`/`%2F`
/// would otherwise let an authed operator escape the home tree.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn ok(data: serde_json::Value) -> Response {
    Json(serde_json::json!({ "ok": true, "data": data })).into_response()
}

pub fn err(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::valid_id;

    #[test]
    fn valid_id_blocks_traversal() {
        // Legit ids/filenames.
        assert!(valid_id("codex-firecracker"));
        assert!(valid_id("humberto-maturana-main"));
        assert!(valid_id("up-maturana.out.log"));
        // Traversal + separators (incl. what percent-decoding yields) are rejected.
        assert!(!valid_id(".."));
        assert!(!valid_id("../.."));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a\\b"));
        assert!(!valid_id("..%2f"));
        assert!(!valid_id("/etc/passwd"));
        assert!(!valid_id(""));
        assert!(!valid_id(&"x".repeat(200)));
    }
}
