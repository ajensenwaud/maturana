//! Channels overview: which chat surfaces each agent exposes (from its spec's
//! `channels` block) and whether the supervisor is currently running that
//! channel's bridge (inferred from the `up/state.json` process list). One row
//! per agent so the operator sees "one agent, every surface" at a glance.

use axum::extract::State;
use axum::response::Response;
use maturana_core::spec::AgentSpec;

use super::{blocking, ok};
use crate::state::AppState;

/// Strip to lowercase alphanumerics so "telegram-channel-codex_fc" matches both
/// the channel keyword and the agent id regardless of `-`/`_` styling.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

pub async fn overview(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        // Supervisor process names → liveness signal for the channel bridges.
        let procs: Vec<String> = std::fs::read_to_string(root.join("up").join("state.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("processes").and_then(|p| p.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(norm))
            .collect();
        let plane_up = root.join("up").join("state.json").exists();
        let live = |keyword: &str, agent: &str| -> bool {
            let a = norm(agent);
            procs.iter().any(|p| p.contains(keyword) && p.contains(&a))
        };

        let mut rows = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root.join("agents")) {
            for entry in entries.flatten() {
                let spec_path = entry.path().join("MATURANA.md");
                if !spec_path.exists() {
                    continue;
                }
                let agent = entry.file_name().to_string_lossy().to_string();
                let ch = AgentSpec::from_maturana_markdown(&spec_path)
                    .map(|s| s.channels)
                    .unwrap_or_default();
                let mut channels = Vec::new();
                // Web is always available (the cockpit itself is the bridge).
                channels.push(serde_json::json!({
                    "name": "web", "configured": true, "live": true, "detail": "this cockpit",
                }));
                channels.push(serde_json::json!({
                    "name": "tui", "configured": ch.tui, "live": ch.tui && plane_up,
                    "detail": "terminal chat",
                }));
                let surface = |name: &str, configured: bool, detail: String, rows: &mut Vec<serde_json::Value>| {
                    rows.push(serde_json::json!({
                        "name": name,
                        "configured": configured,
                        "live": configured && live(name, &agent),
                        "detail": detail,
                    }));
                };
                surface("telegram", ch.telegram.is_some(),
                    ch.telegram.as_ref().map(|t| t.token_source.clone()).unwrap_or_default(), &mut channels);
                surface("discord", ch.discord.is_some(),
                    ch.discord.as_ref().map(|d| d.bot_token_source.clone()).unwrap_or_default(), &mut channels);
                surface("slack", ch.slack.is_some(),
                    ch.slack.as_ref().map(|s| s.bot_token_source.clone()).unwrap_or_default(), &mut channels);
                surface("agentmail", ch.agentmail.is_some(),
                    ch.agentmail.as_ref().and_then(|m| m.inbox.clone()).unwrap_or_default(), &mut channels);

                rows.push(serde_json::json!({ "agent_id": agent, "channels": channels }));
            }
        }
        rows.sort_by(|a, b| {
            a.get("agent_id").and_then(|v| v.as_str()).unwrap_or("")
                .cmp(b.get("agent_id").and_then(|v| v.as_str()).unwrap_or(""))
        });
        Ok(serde_json::json!(rows))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
