//! Agent fleet API. Spec edits follow the safety flow: validate → dry-run →
//! explicit apply; the egress editor rewrites only the `network` block of the
//! spec frontmatter and re-validates before anything is written.

use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use maturana_core::spec::AgentSpec;
use maturana_core::state::MaturanaHome;
use maturana_core::validation::validate_spec;

use super::{blocking, err, ok};
use crate::state::AppState;

fn home(state: &AppState) -> MaturanaHome {
    MaturanaHome::new(state.home_root.clone())
}

fn agent_spec_path(state: &AppState, agent_id: &str) -> PathBuf {
    home(state).agent_dir(agent_id).join("MATURANA.md")
}

/// Reject ids that could traverse out of the agents directory.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

macro_rules! check_id {
    ($id:expr) => {
        if !valid_agent_id(&$id) {
            return err(StatusCode::BAD_REQUEST, "invalid agent id");
        }
    };
}

pub async fn list(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || snapshot(&root)).await {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// One JSON snapshot of the fleet; shared by the REST list and the agents
/// dashboard topic poller.
pub(crate) fn snapshot(root: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    {
        let agents_dir = root.join("agents");
        let mut agents = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&agents_dir) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let spec_path = dir.join("MATURANA.md");
                if !spec_path.exists() {
                    continue;
                }
                let agent_id = entry.file_name().to_string_lossy().to_string();
                let spec = AgentSpec::from_maturana_markdown(&spec_path).ok();
                let worker_status: Option<serde_json::Value> = std::fs::read_to_string(
                    dir.join("worker-status.json"),
                )
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok());
                agents.push(serde_json::json!({
                    "agent_id": agent_id,
                    "name": spec.as_ref().map(|s| s.identity.name.clone()),
                    "purpose": spec.as_ref().map(|s| s.identity.purpose.clone()),
                    "harness": spec.as_ref().map(|s| maturana_core::worker::harness_name(&s.runtime.harness)),
                    "provider": spec.as_ref().map(|s| format!("{:?}", s.vm.provider)),
                    "knowledge_graph": spec.as_ref().map(|s| s.knowledge_graph.enabled).unwrap_or(false),
                    "graph_name": spec.as_ref().filter(|s| s.knowledge_graph.enabled).map(|s| s.knowledge_graph.graph_name(&agent_id)),
                    "egress_allowlist": spec.as_ref().map(|s| s.network.egress_allowlist.clone()).unwrap_or_default(),
                    "worker_status": worker_status,
                    "spec_parses": spec.is_some(),
                }));
            }
        }
        agents.sort_by(|a, b| a["agent_id"].as_str().cmp(&b["agent_id"].as_str()));
        Ok(serde_json::json!(agents))
    }
}

pub async fn status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        let status = maturana_core::materialize::inspect_agent(&home, &id)?;
        Ok(serde_json::to_value(status)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn stop(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        maturana_core::materialize::stop_agent(&home, &id)?;
        Ok(serde_json::json!({ "stopped": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn spec_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || Ok(std::fs::read_to_string(&path)?)).await {
        Ok(markdown) => ok(serde_json::json!({ "markdown": markdown })),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct SpecBody {
    markdown: String,
}

/// Validate the submitted spec; write it only when the report is clean. The
/// report comes back either way so the UI can render it as the gate.
pub async fn spec_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SpecBody>,
) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let report = validate_markdown(&body.markdown)?;
        let written = report.valid;
        if written {
            std::fs::write(&path, &body.markdown)?;
        }
        Ok(serde_json::json!({ "report": report, "written": written }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn spec_validate(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SpecBody>,
) -> Response {
    check_id!(id);
    match blocking(move || Ok(serde_json::to_value(validate_markdown(&body.markdown)?)?)).await {
        Ok(report) => ok(serde_json::json!({ "report": report })),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct ApplyBody {
    #[serde(default)]
    dry_run: bool,
}

/// Materialize the agent's current on-disk spec. `dry_run: true` is the
/// default-safe preview; the UI requires it before offering the real apply.
pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApplyBody>,
) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        let path = home.agent_dir(&id).join("MATURANA.md");
        let raw = std::fs::read_to_string(&path)?;
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        let mode = if body.dry_run {
            maturana_core::materialize::LaunchMode::DryRun
        } else {
            maturana_core::materialize::LaunchMode::Apply
        };
        let result = maturana_core::materialize::materialize_agent(&spec, &raw, &home, mode)?;
        Ok(serde_json::json!({ "dry_run": body.dry_run, "result": result }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn egress_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        Ok(serde_json::json!({
            "egress_allowlist": spec.network.egress_allowlist,
            "inject_headers": spec.network.proxy.as_ref().map(|p| p.inject_headers.clone()).unwrap_or_default(),
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct EgressBody {
    egress_allowlist: Vec<String>,
    #[serde(default)]
    inject_headers: Vec<maturana_core::spec::NetworkProxyHeader>,
}

pub async fn egress_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<EgressBody>,
) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let markdown = std::fs::read_to_string(&path)?;
        let updated = update_network_block(&markdown, &body.egress_allowlist, &body.inject_headers)?;
        let report = validate_markdown(&updated)?;
        if !report.valid {
            anyhow::bail!("edited spec failed validation: {}", report.errors.join("; "));
        }
        std::fs::write(&path, &updated)?;
        Ok(serde_json::json!({ "report": report }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Spec sections the cockpit's Config panel may read/write directly. Editing is
/// confined to these declarative blocks — never the identity/vm/runtime that
/// define the agent's isolation.
pub const CONFIG_SECTIONS: &[&str] = &[
    "schedules",
    "mcp_servers",
    "channels",
    "skills",
    "tools",
    "capabilities",
];

#[derive(serde::Deserialize)]
pub struct ConfigQuery {
    section: String,
}

/// Read one config section of an agent's spec as JSON.
pub async fn config_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ConfigQuery>,
) -> Response {
    check_id!(id);
    if !CONFIG_SECTIONS.contains(&q.section.as_str()) {
        return err(StatusCode::BAD_REQUEST, "unknown config section");
    }
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        let full = serde_json::to_value(&spec)?;
        Ok(serde_json::json!({
            "section": q.section,
            "value": full.get(&q.section).cloned().unwrap_or(serde_json::Value::Null),
            "editable": CONFIG_SECTIONS,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct ConfigBody {
    section: String,
    value: serde_json::Value,
}

/// Replace one config section, re-validate the whole spec, and write it. A
/// running agent picks up channel/MCP/schedule changes on its next
/// materialize/restart (the spec is the source of truth, not live RPC).
pub async fn config_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConfigBody>,
) -> Response {
    check_id!(id);
    if !CONFIG_SECTIONS.contains(&body.section.as_str()) {
        return err(StatusCode::BAD_REQUEST, "unknown config section");
    }
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let markdown = std::fs::read_to_string(&path)?;
        let updated = update_spec_section(&markdown, &body.section, &body.value)?;
        let report = validate_markdown(&updated)?;
        if !report.valid {
            anyhow::bail!("edited spec failed validation: {}", report.errors.join("; "));
        }
        std::fs::write(&path, &updated)?;
        Ok(serde_json::json!({ "section": body.section, "report": report }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Replace ONE top-level frontmatter key with the given JSON value (null
/// removes it), preserving the rest of the spec byte-for-byte structurally.
pub fn update_spec_section(
    markdown: &str,
    section: &str,
    value: &serde_json::Value,
) -> anyhow::Result<String> {
    let rest = markdown
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("spec has no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is unterminated"))?;
    let frontmatter = &rest[..end];
    let body = &rest[end + 4..];
    let mut doc: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is not a mapping"))?;
    let key = serde_yaml::Value::String(section.to_string());
    if value.is_null() {
        map.remove(&key);
    } else {
        map.insert(key, serde_yaml::to_value(value)?);
    }
    let new_frontmatter = serde_yaml::to_string(&doc)?;
    Ok(format!("---\n{new_frontmatter}---{body}"))
}

fn validate_markdown(markdown: &str) -> anyhow::Result<maturana_core::validation::ValidationReport> {
    // from_maturana_markdown reads a file; validate in-memory via a temp file
    // so unsaved editor content can be checked without touching the spec.
    let tmp = std::env::temp_dir().join(format!("mweb-spec-{}.md", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, markdown)?;
    let parsed = AgentSpec::from_maturana_markdown(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let spec = parsed?;
    Ok(validate_spec(&spec))
}

/// Rewrite ONLY the `network` block of the spec frontmatter, preserving every
/// other field. The frontmatter is YAML between the leading `---` fences; we
/// round-trip it as a `serde_yaml::Value` mapping (insertion order preserved)
/// rather than through `AgentSpec`, so unknown-to-us formatting like field
/// order survives.
pub fn update_network_block(
    markdown: &str,
    allowlist: &[String],
    headers: &[maturana_core::spec::NetworkProxyHeader],
) -> anyhow::Result<String> {
    let rest = markdown
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("spec has no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is unterminated"))?;
    let frontmatter = &rest[..end];
    let body = &rest[end + 4..];

    let mut doc: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is not a mapping"))?;

    let network_key = serde_yaml::Value::String("network".to_string());
    let mut network = map
        .get(&network_key)
        .cloned()
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    {
        let network_map = network
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("network block is not a mapping"))?;
        network_map.insert(
            serde_yaml::Value::String("egress_allowlist".to_string()),
            serde_yaml::to_value(allowlist)?,
        );
        let proxy_key = serde_yaml::Value::String("proxy".to_string());
        if headers.is_empty() {
            // Leave an existing proxy block alone but clear its injections.
            if let Some(proxy) = network_map.get_mut(&proxy_key).and_then(|p| p.as_mapping_mut()) {
                proxy.insert(
                    serde_yaml::Value::String("inject_headers".to_string()),
                    serde_yaml::Value::Sequence(Vec::new()),
                );
            }
        } else {
            let mut proxy = network_map
                .get(&proxy_key)
                .cloned()
                .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
            if let Some(proxy_map) = proxy.as_mapping_mut() {
                proxy_map.insert(
                    serde_yaml::Value::String("inject_headers".to_string()),
                    serde_yaml::to_value(headers)?,
                );
            }
            network_map.insert(proxy_key, proxy);
        }
    }
    map.insert(network_key, network);

    let new_frontmatter = serde_yaml::to_string(&doc)?;
    Ok(format!("---\n{new_frontmatter}---{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::spec::NetworkProxyHeader;

    const SPEC: &str = r#"---
identity:
  id: demo
  name: Demo
  purpose: Test agent for egress editing.
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 1024
network:
  egress_allowlist:
    - api.openai.com
memory:
  wiki_path: .maturana/wiki
---

# Demo Agent
body text stays put
"#;

    #[test]
    fn egress_rewrite_round_trips_and_preserves_other_fields() {
        let headers = vec![NetworkProxyHeader {
            host: "api.search.brave.com".into(),
            header: "X-Subscription-Token".into(),
            source: "pipelock:brave/api-key".into(),
            prefix: None,
        }];
        let allowlist = vec!["api.openai.com".to_string(), "api.search.brave.com".to_string()];
        let updated = update_network_block(SPEC, &allowlist, &headers).unwrap();

        // Re-parse: the edited model holds, the rest is untouched.
        let tmp = std::env::temp_dir().join(format!("mweb-egress-{}.md", std::process::id()));
        std::fs::write(&tmp, &updated).unwrap();
        let spec = AgentSpec::from_maturana_markdown(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(spec.network.egress_allowlist, allowlist);
        let proxy = spec.network.proxy.unwrap();
        assert_eq!(proxy.inject_headers.len(), 1);
        assert_eq!(proxy.inject_headers[0].host, "api.search.brave.com");
        assert_eq!(spec.identity.id, "demo");
        assert_eq!(spec.identity.purpose, "Test agent for egress editing.");
        assert_eq!(spec.memory.wiki_path.as_deref(), Some(".maturana/wiki"));
        assert!(updated.contains("body text stays put"));

        // Idempotent: applying the same edit again changes nothing.
        let again = update_network_block(&updated, &allowlist, &headers).unwrap();
        assert_eq!(again, updated);
    }

    #[test]
    fn egress_rewrite_rejects_specs_without_frontmatter() {
        assert!(update_network_block("# no frontmatter", &[], &[]).is_err());
    }

    #[test]
    fn agent_id_validation_blocks_traversal() {
        assert!(valid_agent_id("codex-firecracker"));
        for bad in ["", "..", "a/b", "a\\b", "x y"] {
            assert!(!valid_agent_id(bad), "should reject {bad:?}");
        }
    }
}
