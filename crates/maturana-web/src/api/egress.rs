//! Egress governance: approve a host into the live pipelock proxy (and
//! optionally promote it to the durable spec allowlist). The live feed itself
//! is pushed over the WebSocket by `server::egress_poller`.

use std::path::Path;

use axum::extract::State;
use axum::response::Response;
use axum::Json;
use maturana_core::spec::AgentSpec;

use super::{agents, blocking, ok};
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct ApproveBody {
    host: String,
    /// When true, also write the host into the agent's spec allowlist so it
    /// survives a proxy restart (routed through the validated spec rewrite).
    #[serde(default)]
    permanent: bool,
    /// Required only for `permanent` — which agent's spec to promote into.
    #[serde(default)]
    agent_id: Option<String>,
}

/// Append `host` to `<home>/pipelock/runtime-allow.json` (the running proxy's
/// 1s watcher picks it up) and optionally promote it into the spec.
pub async fn approve(State(state): State<AppState>, Json(body): Json<ApproveBody>) -> Response {
    let home = state.home_root.clone();
    match blocking(move || {
        let host = normalize_host(&body.host)?;
        let granted = append_runtime_allow(&home, &host)?;
        let mut promoted = false;
        if body.permanent {
            let agent_id = body
                .agent_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("permanent approval requires agent_id"))?;
            promote_to_spec(&home, agent_id, &host)?;
            promoted = true;
        }
        Ok(serde_json::json!({ "host": host, "granted": granted, "permanent": promoted }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn normalize_host(raw: &str) -> anyhow::Result<String> {
    let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    // Reject `*` (and any host containing it): the per-host hot-approve path must
    // never be able to write an allow-all wildcard. A live `*` in runtime-allow.json
    // opens egress for EVERY agent (the file is home-level/shared), and `permanent`
    // would bake it into a spec — both silently, bypassing the loud validation
    // warning. Allow-all is only reachable through the deliberate, warned paths
    // (network.egress_allow_all, `pipelock proxy --allow-all`, the cockpit toggle).
    if host.is_empty()
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '/' | '\\' | ':' | '*'))
    {
        anyhow::bail!("invalid host");
    }
    Ok(host)
}

/// Read-modify-write the runtime-allow array. Returns true if the host was
/// newly added (false if it was already granted).
fn append_runtime_allow(home: &Path, host: &str) -> anyhow::Result<bool> {
    let dir = home.join("pipelock");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("runtime-allow.json");
    let mut hosts: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return Ok(false);
    }
    hosts.push(host.to_string());
    std::fs::write(&path, serde_json::to_vec_pretty(&hosts)?)?;
    Ok(true)
}

/// Add the host to the agent's spec egress allowlist via the same validated
/// rewrite the egress editor uses, so the proxy never edits the spec directly.
fn promote_to_spec(home: &Path, agent_id: &str, host: &str) -> anyhow::Result<()> {
    if !agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        || agent_id.is_empty()
    {
        anyhow::bail!("invalid agent id");
    }
    let spec_path = home.join("agents").join(agent_id).join("MATURANA.md");
    let markdown = std::fs::read_to_string(&spec_path)?;
    let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
    let mut allowlist = spec.network.egress_allowlist.clone();
    if !allowlist.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        allowlist.push(host.to_string());
    }
    let headers = spec
        .network
        .proxy
        .as_ref()
        .map(|p| p.inject_headers.clone())
        .unwrap_or_default();
    let updated = agents::update_network_block(&markdown, &allowlist, &headers, None)?;
    // Re-validate before writing (same gate as the egress editor).
    let report = {
        let tmp = std::env::temp_dir().join(format!("mweb-egress-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, &updated)?;
        let parsed = AgentSpec::from_maturana_markdown(&tmp);
        let _ = std::fs::remove_file(&tmp);
        maturana_core::validation::validate_spec(&parsed?)
    };
    if !report.valid {
        anyhow::bail!("promoted spec failed validation: {}", report.errors.join("; "));
    }
    std::fs::write(&spec_path, &updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_runtime_allow_is_idempotent() {
        let home = std::env::temp_dir().join(format!("egress-{}", uuid::Uuid::new_v4()));
        assert!(append_runtime_allow(&home, "api.notion.com").unwrap());
        assert!(!append_runtime_allow(&home, "api.notion.com").unwrap());
        let raw = std::fs::read_to_string(home.join("pipelock/runtime-allow.json")).unwrap();
        let hosts: Vec<String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(hosts, vec!["api.notion.com"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn normalize_rejects_junk() {
        assert_eq!(normalize_host("API.Notion.com.").unwrap(), "api.notion.com");
        assert!(normalize_host("bad host").is_err());
        assert!(normalize_host("host:443").is_err());
        assert!(normalize_host("").is_err());
        // The per-host hot-approve path must never be able to grant a wildcard:
        // allow-all is reachable only via the deliberate, warned spec/flag/toggle.
        assert!(normalize_host("*").is_err());
        assert!(normalize_host("*.example.com").is_err());
        assert!(normalize_host("ex*ample.com").is_err());
    }
}
