//! Runtime plane view: the supervisor's heartbeat file (written by
//! `maturana up` each tick — no IPC) plus health probes and doctor.

use axum::extract::State;
use axum::response::Response;

use super::{blocking, ok};
use crate::state::AppState;

pub async fn up_state(State(state): State<AppState>) -> Response {
    let path = state.home_root.join("up").join("state.json");
    match blocking(move || {
        if !path.exists() {
            return Ok(serde_json::json!({ "running": false }));
        }
        let raw = std::fs::read_to_string(&path)?;
        let mut value: serde_json::Value = serde_json::from_str(&raw)?;
        value["running"] = serde_json::json!(true);
        Ok(value)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Service health probes the cockpit can do directly (fast, local).
pub async fn plan(State(state): State<AppState>) -> Response {
    let home_root = state.home_root.clone();
    match blocking(move || {
        let probe = |url: &str| -> serde_json::Value {
            let check = maturana_ops::health::http_health(url);
            if check.ok {
                serde_json::json!({ "ok": true })
            } else {
                serde_json::json!({ "ok": false, "error": check.message })
            }
        };
        Ok(serde_json::json!({
            "sessiond": probe("http://127.0.0.1:47834/health"),
            "graph": probe("http://127.0.0.1:47835/health"),
            "graph_enabled": home_root.join("graph").join("token").exists(),
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Full doctor report through the shared ops layer; this keeps the cockpit a
/// front end instead of shelling back into the CLI as an internal API.
pub async fn doctor(State(state): State<AppState>) -> Response {
    let home_root = state.home_root.clone();
    match blocking(move || {
        let home = maturana_core::state::MaturanaHome::new(home_root);
        Ok(serde_json::to_value(maturana_ops::doctor::build_report(
            &home,
            &[],
            "http://127.0.0.1:47834",
        ))?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
