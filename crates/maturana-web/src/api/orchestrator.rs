//! Orchestrator / board view: surface durable multi-agent runs
//! (`<home>/orchestration/<run_id>/plan.json`). Each run's steps ARE the board
//! cards (id, role, task, deps, status, result), so the cockpit renders the plan
//! as a status board. Read-only except `abort`, which shells out to the CLI so
//! it reuses the real cancellation logic.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{blocking, err, ok, valid_id};
use crate::state::AppState;

fn orchestration_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join("orchestration")
}

fn read_plan(dir: &std::path::Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(dir.join("plan.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Count steps by status for a plan, plus the total.
fn tally(plan: &serde_json::Value) -> serde_json::Value {
    let mut done = 0;
    let mut running = 0;
    let mut failed = 0;
    let mut waiting = 0;
    let steps = plan.get("steps").and_then(|s| s.as_array());
    let total = steps.map(|s| s.len()).unwrap_or(0);
    if let Some(steps) = steps {
        for step in steps {
            match step.get("status").and_then(|v| v.as_str()).unwrap_or("waiting") {
                "done" => done += 1,
                "running" => running += 1,
                "failed" => failed += 1,
                _ => waiting += 1,
            }
        }
    }
    let state = if failed > 0 {
        "failed"
    } else if total > 0 && done == total {
        "done"
    } else if running > 0 {
        "running"
    } else {
        "waiting"
    };
    serde_json::json!({
        "total": total, "done": done, "running": running,
        "failed": failed, "waiting": waiting, "state": state,
    })
}

/// All orchestration runs, newest first, each with its goal + step tally.
pub async fn list_runs(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let mut runs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(orchestration_dir(&root)) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let run_id = entry.file_name().to_string_lossy().to_string();
                let Some(plan) = read_plan(&dir) else { continue };
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                let has_output = dir.join("output").is_dir() || dir.join("answer.md").exists();
                runs.push(serde_json::json!({
                    "run_id": run_id,
                    "goal": plan.get("goal").cloned().unwrap_or(serde_json::Value::Null),
                    "tally": tally(&plan),
                    "modified": modified,
                    "has_output": has_output,
                }));
            }
        }
        // Newest first by mtime.
        runs.sort_by(|a, b| {
            b.get("modified").and_then(|v| v.as_u64()).unwrap_or(0)
                .cmp(&a.get("modified").and_then(|v| v.as_u64()).unwrap_or(0))
        });
        Ok(serde_json::json!(runs))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// The full plan for one run (goal + every step), so the board can render cards.
pub async fn run_detail(State(state): State<AppState>, Path(run_id): Path<String>) -> Response {
    if !valid_id(&run_id) {
        return err(StatusCode::BAD_REQUEST, "invalid run id");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let dir = orchestration_dir(&root).join(&run_id);
        let plan = read_plan(&dir).ok_or_else(|| anyhow::anyhow!("no such run"))?;
        // List any deliverable files the run produced (host-side).
        let mut files = Vec::new();
        for sub in ["output", "staging"] {
            if let Ok(entries) = std::fs::read_dir(dir.join(sub)) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        files.push(format!("{sub}/{}", entry.file_name().to_string_lossy()));
                    }
                }
            }
        }
        Ok(serde_json::json!({
            "run_id": run_id,
            "tally": tally(&plan),
            "goal": plan.get("goal").cloned().unwrap_or(serde_json::Value::Null),
            "steps": plan.get("steps").cloned().unwrap_or(serde_json::json!([])),
            "files": files,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Abort a run by shelling out to the CLI (reuses the real cancellation path).
pub async fn abort_run(State(state): State<AppState>, Path(run_id): Path<String>) -> Response {
    if !valid_id(&run_id) {
        return err(StatusCode::BAD_REQUEST, "invalid run id");
    }
    let home_root = state.home_root.clone();
    match blocking(move || {
        let exe = std::env::current_exe()?;
        let output = std::process::Command::new(exe)
            .arg("--home")
            .arg(&home_root)
            .args(["orchestrator", "abort", &run_id])
            .output()?;
        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(serde_json::json!({ "aborted": run_id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
