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
            match ureq::get(url).timeout(std::time::Duration::from_secs(2)).call() {
                Ok(_) => serde_json::json!({ "ok": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error.to_string() }),
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

/// Full doctor report via the CLI's own `--json` flag (zero refactor; the
/// subprocess inherits this server's home).
pub async fn doctor(State(state): State<AppState>) -> Response {
    let home_root = state.home_root.clone();
    match blocking(move || {
        let exe = std::env::current_exe()?;
        let output = std::process::Command::new(exe)
            .arg("--home")
            .arg(&home_root)
            .args(["doctor", "--json"])
            .output()?;
        // doctor exits non-zero when unhealthy but still prints the report.
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| anyhow::anyhow!(String::from_utf8_lossy(&output.stderr).to_string()))?;
        Ok(report)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
