//! Knowledge-graph panel: a small client to the MaturanaGraph service on
//! :47835. All writes go through the service (single writer per graph) —
//! including document ingest, which parses host-side via maturana-ingest and
//! upserts over HTTP exactly like `maturana graph ingest`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;

use super::{blocking, err, ok};
use crate::state::AppState;

const GRAPH_URL: &str = "http://127.0.0.1:47835";

fn graph_token(home_root: &std::path::Path) -> anyhow::Result<String> {
    maturana_core::worker::read_graph_token(home_root)
        .ok_or_else(|| anyhow::anyhow!("graph service not set up (no graph token)"))
}

fn post_json(
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let response = ureq::post(&format!("{GRAPH_URL}{path}"))
        .set("x-maturana-graph-token", token)
        .timeout(std::time::Duration::from_secs(20))
        .send_json(body)?;
    Ok(response.into_json()?)
}

#[derive(serde::Deserialize)]
pub struct GraphBody {
    graph: String,
    #[serde(default)]
    query_terms: Vec<String>,
    #[serde(default)]
    depth: Option<usize>,
}

pub async fn stats(State(state): State<AppState>, Json(body): Json<GraphBody>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let token = graph_token(&root)?;
        post_json(
            &token,
            "/graph/stats",
            &serde_json::json!({ "graph": body.graph }),
        )
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn query(State(state): State<AppState>, Json(body): Json<GraphBody>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let token = graph_token(&root)?;
        post_json(
            &token,
            "/graph/query",
            &serde_json::json!({
                "graph": body.graph,
                "query_terms": body.query_terms,
                "depth": body.depth.unwrap_or(2),
            }),
        )
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Document upload → parse/chunk host-side → upsert through the service.
/// Body: raw file bytes; filename and graph in headers (keeps the client a
/// plain fetch without multipart).
pub async fn ingest(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(filename) = headers
        .get("x-maturana-filename")
        .and_then(|v| v.to_str().ok())
        .map(sanitize_filename)
    else {
        return err(
            StatusCode::BAD_REQUEST,
            "missing x-maturana-filename header",
        );
    };
    let graph = headers
        .get("x-maturana-graph")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("personal")
        .to_string();
    if body.len() > 32 * 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "document exceeds 32 MB");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let token = graph_token(&root)?;
        let uploads = root.join("web").join("uploads");
        std::fs::create_dir_all(&uploads)?;
        let dest = uploads.join(format!(
            "{}-{filename}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        std::fs::write(&dest, &body)?;
        let ingested = maturana_ingest::ingest(&dest, 1800)?;
        let upsert = serde_json::json!({
            "graph": graph,
            "nodes": ingested.nodes,
            "edges": ingested.edges,
        });
        let response = post_json(&token, "/graph/upsert", &upsert)?;
        Ok(serde_json::json!({
            "file": filename,
            "graph": graph,
            "chunks": ingested.chunks,
            "stats": response.get("stats"),
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        "document".to_string()
    } else {
        trimmed.to_string()
    }
}
