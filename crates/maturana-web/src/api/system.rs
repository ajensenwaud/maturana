//! Host observability: system stats, log tail, and fleet activity analytics.
//! Stats read Linux `/proc` (degrading gracefully elsewhere); logs tail
//! `journalctl --user` units or files; analytics summarize per-agent session
//! activity. We do NOT report token cost — agents meter tokens inside their own
//! VMs against the operator's own subscription/keys, so the host can't see it;
//! we report activity (turns/messages) instead of inventing dollar figures.

use std::path::Path;
use std::process::Command;

use axum::extract::{Query, State};
use axum::response::Response;
use maturana_core::session_db;
use serde::Deserialize;

use super::{blocking, ok};
use crate::state::AppState;

// ---- host stats ------------------------------------------------------------

pub async fn stats(State(_state): State<AppState>) -> Response {
    match blocking(|| Ok(host_stats())).await {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn host_stats() -> serde_json::Value {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()));
    let loadavg: Vec<f64> = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .map(|s| {
            s.split_whitespace()
                .take(3)
                .filter_map(|x| x.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let (mem_total, mem_avail) = meminfo();
    let (disk_total, disk_avail) = disk_usage(".");
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "hostname": hostname(),
        "cores": cores,
        "uptime_seconds": uptime,
        "loadavg": loadavg,
        "mem_total_bytes": mem_total,
        "mem_available_bytes": mem_avail,
        "disk_total_bytes": disk_total,
        "disk_available_bytes": disk_avail,
    })
}

fn meminfo() -> (Option<u64>, Option<u64>) {
    let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let kb = |key: &str| {
        s.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    (kb("MemTotal:"), kb("MemAvailable:"))
}

fn disk_usage(path: &str) -> (Option<u64>, Option<u64>) {
    let Ok(out) = Command::new("df")
        .args(["-B1", "--output=size,avail", path])
        .output()
    else {
        return (None, None);
    };
    if !out.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = text.lines().nth(1) {
        let nums: Vec<u64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if nums.len() == 2 {
            return (Some(nums[0]), Some(nums[1]));
        }
    }
    (None, None)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---- logs ------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LogQuery {
    source: Option<String>,
    lines: Option<usize>,
}

/// Tail a log source: `plane`/`web`/`fleet` (systemd user units via journalctl),
/// `agent:<id>` (that agent's egress audit JSONL), or a bare filename under
/// `<home>/logs/`.
pub async fn logs(State(state): State<AppState>, Query(q): Query<LogQuery>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let lines = q.lines.unwrap_or(200).clamp(1, 2000);
        let source = q.source.unwrap_or_else(|| "plane".to_string());
        let text = match source.as_str() {
            "plane" => journalctl("maturana-up.service", lines),
            "web" => journalctl("maturana-web.service", lines),
            "fleet" => journalctl("maturana-fleet.service", lines),
            other => {
                if let Some(agent) = other.strip_prefix("agent:") {
                    // Validate the agent id — it goes into a filename; a `..`/`/`
                    // would let `source=agent:../../..` traverse out of audit/.
                    if !super::valid_id(agent) {
                        anyhow::bail!("invalid agent id in log source");
                    }
                    tail_file(
                        &root
                            .join("audit")
                            .join(format!("{agent}-pipelock-proxy.jsonl")),
                        lines,
                    )
                } else {
                    // Bare filename under <home>/logs/ — must be a single safe
                    // segment, never a traversal to an arbitrary host file.
                    if !super::valid_id(other) {
                        anyhow::bail!("invalid log source");
                    }
                    tail_file(&root.join("logs").join(other), lines)
                }
            }
        };
        Ok(serde_json::json!({ "source": source, "lines": lines, "text": text }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// The log sources available on this host (units + per-agent audit logs).
pub async fn log_sources(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let mut sources = vec![
            "plane".to_string(),
            "web".to_string(),
            "fleet".to_string(),
        ];
        if let Ok(rd) = std::fs::read_dir(root.join("audit")) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(agent) = name.strip_suffix("-pipelock-proxy.jsonl") {
                    if agent != "pipelock" {
                        sources.push(format!("agent:{agent}"));
                    }
                }
            }
        }
        sources.sort();
        sources.dedup();
        Ok(serde_json::json!({ "sources": sources }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn journalctl(unit: &str, lines: usize) -> String {
    match Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "-o",
            "short-iso",
        ])
        .output()
    {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if text.trim().is_empty() {
                format!("(no recent journal entries for {unit})")
            } else {
                text
            }
        }
        _ => format!("(journalctl unavailable for {unit} — not on systemd, or unit absent)"),
    }
}

fn tail_file(path: &Path, lines: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return format!("(no log at {})", path.display());
    };
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

// ---- overview (cockpit landing) -------------------------------------------

/// One call powering the Overview home page: the fleet, the plane, host load.
pub async fn overview(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let agents = crate::api::agents::snapshot(&root).unwrap_or_else(|_| serde_json::json!([]));
        let arr = agents.as_array().cloned().unwrap_or_default();
        let count = arr.len();
        // "up" = a fresh heartbeat (computed in agents::snapshot as `live`), not
        // the literal status "running" the worker never writes. "busy" = a turn
        // actually in flight (status "claimed"). This is what fixes the Overview
        // showing 0 agents while a healthy idle fleet is live.
        let up = arr.iter().filter(|a| a["live"] == true).count();
        let busy = arr
            .iter()
            .filter(|a| a["status"].as_str() == Some("claimed"))
            .count();
        let graphs = arr.iter().filter(|a| a["knowledge_graph"] == true).count();
        let plane = std::fs::read_to_string(root.join("up").join("state.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        Ok(serde_json::json!({
            "agents": agents,
            "counts": { "agents": count, "up": up, "busy": busy, "graphs": graphs },
            "plane": { "up": plane.is_some(), "state": plane },
            "host": host_stats(),
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

// ---- activity analytics ----------------------------------------------------

#[derive(Deserialize)]
pub struct AnalyticsQuery {
    days: Option<i64>,
}

/// Per-agent activity summary over the fleet's session DBs: sessions, queued vs
/// delivered counts, and inbound/outbound message volume in the window. Honest
/// activity metrics — not token cost (see module note).
pub async fn analytics(State(state): State<AppState>, Query(q): Query<AnalyticsQuery>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let days = q.days.unwrap_or(30).clamp(1, 365);
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let mut per_agent: Vec<serde_json::Value> = Vec::new();
        let mut total_in = 0u64;
        let mut total_done = 0u64;
        let mut total_sessions = 0u64;
        if let Ok(agents) = std::fs::read_dir(root.join("agents")) {
            for agent in agents.flatten() {
                let agent_id = agent.file_name().to_string_lossy().to_string();
                let Ok(sessions) = std::fs::read_dir(agent.path().join("sessions")) else {
                    continue;
                };
                let mut sessions_n = 0u64;
                let mut inbound = 0u64;
                let mut completed = 0u64;
                let mut last_active: Option<chrono::DateTime<chrono::Utc>> = None;
                for session in sessions.flatten() {
                    if !session.path().is_dir() {
                        continue;
                    }
                    sessions_n += 1;
                    let session_id = session.file_name().to_string_lossy().to_string();
                    let paths = session_db::session_paths(&agent.path(), &session_id);
                    // Inbound turns within the window (cap the scan per session).
                    if let Ok(rows) = session_db::list_recent_inbound(&paths, 1000) {
                        for m in &rows {
                            if m.created_at >= cutoff {
                                inbound += 1;
                            }
                            last_active = Some(last_active.map_or(m.created_at, |p| p.max(m.created_at)));
                        }
                    }
                    if let Ok(stats) = session_db::queue_stats(&paths) {
                        completed += stats.completed.max(0) as u64;
                    }
                }
                total_sessions += sessions_n;
                total_in += inbound;
                total_done += completed;
                per_agent.push(serde_json::json!({
                    "agent_id": agent_id,
                    "sessions": sessions_n,
                    "inbound": inbound,
                    "completed_turns": completed,
                    "last_active": last_active.map(|t| t.to_rfc3339()),
                }));
            }
        }
        per_agent.sort_by(|a, b| {
            b["inbound"].as_u64().unwrap_or(0).cmp(&a["inbound"].as_u64().unwrap_or(0))
        });
        Ok(serde_json::json!({
            "days": days,
            "totals": {
                "sessions": total_sessions,
                "inbound": total_in,
                "completed_turns": total_done,
            },
            "per_agent": per_agent,
            "note": "Activity only — token/cost metering happens inside each agent's VM and is not visible to the host.",
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
