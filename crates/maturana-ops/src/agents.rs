use anyhow::Context;
use maturana_core::{
    materialize::{inspect_agent, stop_agent},
    providers::LiveAgentStatus,
    session_db::{list_recent_inbound, queue_stats, session_paths},
    spec::AgentSpec,
    state::MaturanaHome,
};
use serde::Serialize;
use std::{fs, process::Command, time::SystemTime};

#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub agent_id: String,
    pub name: Option<String>,
    pub purpose: Option<String>,
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub knowledge_graph: bool,
    pub graph_name: Option<String>,
    pub egress_allowlist: Vec<String>,
    pub egress_allow_all: bool,
    pub worker_status: Option<serde_json::Value>,
    pub status: String,
    pub live: bool,
    pub worker_age_s: Option<u64>,
    pub spec_parses: bool,
}

/// One row of the host-side `list` / `status` agent table.
///
/// This is intentionally a filesystem/control-plane snapshot only: no SSH and
/// no guest commands. Keeping this in ops lets CLI and web share the same view
/// of materialized agents without duplicating queue/session logic.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub agent: String,
    pub harness: String,
    pub vm: String,
    pub queue: String,
    pub last_turn: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRestart {
    pub restarted: String,
    pub output: Vec<String>,
}

pub fn list_agent_ids(home: &MaturanaHome) -> anyhow::Result<Vec<String>> {
    let agents_dir = home.agents_dir();
    if !agents_dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in fs::read_dir(agents_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let id = entry.file_name().to_string_lossy().to_string();
            if home.agent_dir(&id).join("MATURANA.md").exists() {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn list_agent_summaries(home: &MaturanaHome) -> anyhow::Result<Vec<AgentSummary>> {
    let mut out = Vec::new();
    for agent_id in list_agent_ids(home)? {
        let agent_dir = home.agent_dir(&agent_id);
        let spec = AgentSpec::from_maturana_markdown(agent_dir.join("MATURANA.md")).ok();
        let status_path = agent_dir.join("worker-status.json");
        let worker_status: Option<serde_json::Value> = fs::read_to_string(&status_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let worker_age_s = fs::metadata(&status_path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map(|age| age.as_secs());
        let status = worker_status
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        // The guest worker rewrites worker-status.json while idle. Liveness is
        // freshness + non-error, not a literal "running" status.
        let live = worker_age_s.map(|age| age <= 90).unwrap_or(false) && status != "error";
        out.push(AgentSummary {
            agent_id: agent_id.clone(),
            name: spec.as_ref().map(|s| s.identity.name.clone()),
            purpose: spec.as_ref().map(|s| s.identity.purpose.clone()),
            harness: spec
                .as_ref()
                .map(|s| maturana_core::worker::harness_name(&s.runtime.harness).to_string()),
            provider: spec.as_ref().map(|s| format!("{:?}", s.vm.provider)),
            knowledge_graph: spec
                .as_ref()
                .map(|s| s.knowledge_graph.enabled)
                .unwrap_or(false),
            graph_name: spec
                .as_ref()
                .filter(|s| s.knowledge_graph.enabled)
                .map(|s| s.knowledge_graph.graph_name(&agent_id)),
            egress_allowlist: spec
                .as_ref()
                .map(|s| s.network.egress_allowlist.clone())
                .unwrap_or_default(),
            egress_allow_all: spec
                .as_ref()
                .map(|s| s.network.egress_allow_all)
                .unwrap_or(false),
            worker_status,
            status,
            live,
            worker_age_s,
            spec_parses: spec.is_some(),
        });
    }
    Ok(out)
}

pub fn collect_agent_rows(home: &MaturanaHome) -> Vec<AgentRow> {
    list_agent_ids(home)
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let harness =
                AgentSpec::from_maturana_markdown(home.agent_dir(&id).join("MATURANA.md"))
                    .ok()
                    .map(|s| maturana_core::worker::harness_name(&s.runtime.harness).to_string())
                    .unwrap_or_else(|| "?".to_string());
            let vm = inspect_agent(home, &id)
                .map(|s| s.state)
                .unwrap_or_else(|_| "unknown".to_string());
            let (queue, last_turn) = match infer_agent_session_id(home, &id) {
                Ok(sid) => {
                    let paths = session_paths(&home.agent_dir(&id), &sid);
                    let queue = queue_stats(&paths)
                        .map(|q| {
                            if q.pending == 0 && q.processing == 0 {
                                "idle".to_string()
                            } else if q.processing == 0 {
                                format!("{} pend", q.pending)
                            } else {
                                format!("{} pend/{} proc", q.pending, q.processing)
                            }
                        })
                        .unwrap_or_else(|_| "idle".to_string());
                    let last_turn = list_recent_inbound(&paths, 1)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map(|m| {
                            humanize_age(
                                (chrono::Utc::now() - m.created_at).num_seconds().max(0) as u64
                            )
                        })
                        .unwrap_or_else(|| "-".to_string());
                    (queue, last_turn)
                }
                Err(_) => ("idle".to_string(), "-".to_string()),
            };
            AgentRow {
                agent: id,
                harness,
                vm,
                queue,
                last_turn,
            }
        })
        .collect()
}

pub fn infer_agent_session_id(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<String> {
    let env_path = home.agent_dir(agent_id).join("state/sessiond.env");
    if env_path.exists() {
        let raw = fs::read_to_string(&env_path)
            .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", env_path.display()))?;
        if let Some(session_id) = session_env_value(&raw, "MATURANA_SESSION_ID") {
            return Ok(session_id);
        }
    }

    Ok(default_session_id(agent_id))
}

pub fn inspect_live_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<LiveAgentStatus> {
    inspect_agent(home, agent_id)
}

pub fn stop_live_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    stop_agent(home, agent_id)
}

/// Relaunch a Firecracker agent through the existing repair path.
///
/// This is a deliberately narrow lifecycle bridge: callers can ask for one
/// agent restart, but cannot pass arbitrary CLI arguments through the web/API
/// surface. The deeper repair implementation still lives in the CLI for now and
/// should move behind Rust-owned ops in a later slice.
pub fn restart_firecracker_agent(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<AgentRestart> {
    ensure_safe_agent_id(agent_id)?;
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let output = Command::new(exe)
        .arg("--home")
        .arg(home.root())
        .args(["repair", "firecracker-harnesses", "--agent-id"])
        .arg(agent_id)
        .args(["--skip-services", "--skip-assets"])
        .output()
        .context("failed to launch Firecracker restart repair")?;
    let tail = output_tail(&output.stdout, 6);
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("restart failed: {}", err.trim());
    }
    Ok(AgentRestart {
        restarted: agent_id.to_string(),
        output: tail,
    })
}

fn ensure_safe_agent_id(agent_id: &str) -> anyhow::Result<()> {
    if agent_id.is_empty()
        || agent_id.len() > 128
        || agent_id
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')))
    {
        anyhow::bail!("invalid agent id");
    }
    Ok(())
}

fn output_tail(bytes: &[u8], line_count: usize) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .take(line_count)
        .map(|line| line.to_string())
        .collect()
}

fn default_session_id(agent_id: &str) -> String {
    match agent_id {
        "codex-demo" | "codex-firecracker" => "codex-main".to_string(),
        "opencode-demo" | "opencode-firecracker" => "opencode-main".to_string(),
        "claude-demo" | "claude-firecracker" => "claude-main".to_string(),
        _ => format!("{agent_id}-main"),
    }
}

fn session_env_value(raw: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    raw.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(unquote_shell_env_value)
        .filter(|value| !value.trim().is_empty())
}

fn unquote_shell_env_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        trimmed[1..trimmed.len() - 1]
            .replace("'\"'\"'", "'")
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn humanize_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> MaturanaHome {
        let dir = std::env::temp_dir().join(format!(
            "maturana-ops-agent-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("agents/demo")).unwrap();
        MaturanaHome::new(dir)
    }

    #[test]
    fn list_agent_ids_requires_materialized_spec() {
        let home = temp_home("ids");
        assert!(list_agent_ids(&home).unwrap().is_empty());
        fs::write(home.agent_dir("demo").join("MATURANA.md"), "not yaml").unwrap();
        assert_eq!(list_agent_ids(&home).unwrap(), vec!["demo"]);
        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn summary_reports_worker_liveness_without_parsed_spec() {
        let home = temp_home("summary");
        fs::write(home.agent_dir("demo").join("MATURANA.md"), "not yaml").unwrap();
        fs::write(
            home.agent_dir("demo").join("worker-status.json"),
            r#"{"status":"idle"}"#,
        )
        .unwrap();
        let summaries = list_agent_summaries(&home).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].agent_id, "demo");
        assert_eq!(summaries[0].status, "idle");
        assert!(summaries[0].live);
        assert!(!summaries[0].spec_parses);
        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn infer_session_prefers_materialized_env_then_known_defaults() {
        let home = temp_home("session");
        let state_dir = home.agent_dir("demo").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='custom-session'\n",
        )
        .unwrap();

        assert_eq!(
            infer_agent_session_id(&home, "demo").unwrap(),
            "custom-session"
        );
        assert_eq!(
            infer_agent_session_id(&home, "opencode-firecracker").unwrap(),
            "opencode-main"
        );
        assert_eq!(
            infer_agent_session_id(&home, "new-agent").unwrap(),
            "new-agent-main"
        );
        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn restart_agent_id_validation_blocks_traversal() {
        assert!(ensure_safe_agent_id("codex-firecracker_1").is_ok());
        assert!(ensure_safe_agent_id("../escape").is_err());
        assert!(ensure_safe_agent_id("has/slash").is_err());
        assert!(ensure_safe_agent_id("").is_err());
    }

    #[test]
    fn restart_output_tail_matches_legacy_order() {
        let tail = output_tail(b"one\ntwo\nthree\nfour\n", 3);
        assert_eq!(tail, vec!["four", "three", "two"]);
    }
}
