//! Operational actions: gateway (plane) lifecycle + a config backup. The
//! surface is deliberately narrow. Hermes' dashboard also creates shell hooks,
//! self-updates a running host, and ships an `--insecure` no-auth mode; we
//! DECLINE those on zero-trust grounds (see docs/web-ui-comparison.md) — the
//! cockpit governs the fleet, it does not become a remote root shell.

use std::process::Command;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{blocking, err, ok};
use crate::state::AppState;

const GATEWAY_ACTIONS: &[&str] = &["restart", "stop", "start"];

/// Drive the supervised plane (`maturana-up.service`) — restart/stop/start. Only
/// the named unit, only these three verbs. systemd is the only privileged thing
/// touched.
pub async fn gateway(State(_state): State<AppState>, Path(action): Path<String>) -> Response {
    if !GATEWAY_ACTIONS.contains(&action.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            "action must be restart, stop, or start",
        );
    }
    match blocking(move || {
        let out = Command::new("systemctl")
            .args(["--user", &action, "maturana-up.service"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl {action} maturana-up failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(serde_json::json!({ "action": action, "ok": true }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Back up the declarative fleet config (every agent's MATURANA.md + skills/
/// memory/AGENTS.md) to a timestamped tar.gz under `<home>/backups/`. Excludes
/// sessions, state (which holds tokens), snapshots, workspace, and inbox — so a
/// backup carries NO secrets and no bulky runtime data. Restore is intentionally
/// left as a manual, reviewed `tar -x` (we don't auto-overwrite a live fleet).
pub async fn backup(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let backups = root.join("backups");
        std::fs::create_dir_all(&backups)?;
        let dest = backups.join(format!("config-{ts}.tar.gz"));
        let out = Command::new("tar")
            .arg("-czf")
            .arg(&dest)
            .arg("-C")
            .arg(&root)
            .arg("--exclude=*/sessions")
            .arg("--exclude=*/state")
            .arg("--exclude=*/snapshots")
            .arg("--exclude=*/workspace")
            .arg("--exclude=*/inbox")
            .arg("agents")
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "tar failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        Ok(serde_json::json!({
            "path": dest.display().to_string(),
            "bytes": size,
            "note": "Declarative config only — no secrets, sessions, or images. Restore manually with tar -x after review.",
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
