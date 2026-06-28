//! Schedules view: read + manage the per-agent cron schedules the scheduler
//! runner fires. The store is `<home>/agents/<id>/schedules/schedules.json`
//! (the same file `maturana schedule …` writes), so the cockpit and CLI stay in
//! lockstep. Records are kept as raw JSON so this never drifts from the CLI's
//! `ScheduleRecord` shape.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;

use super::{blocking, err, ok, valid_id};
use crate::state::AppState;

fn schedules_file(root: &std::path::Path, agent: &str) -> std::path::PathBuf {
    root.join("agents").join(agent).join("schedules").join("schedules.json")
}

fn last_run_file(root: &std::path::Path, agent: &str) -> std::path::PathBuf {
    root.join("agents").join(agent).join("schedules").join("last-run.json")
}

fn read_array(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
        .unwrap_or_default()
}

fn write_array(path: &std::path::Path, arr: &[serde_json::Value]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(arr)?)?;
    Ok(())
}

/// A name → id slug, matching the CLI's `slugify` closely enough to address the
/// same record (lowercase alnum, runs of other chars collapse to one '-').
fn slugify(value: &str) -> String {
    let mapped: String = value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = mapped.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if slug.is_empty() { "item".to_string() } else { slug }
}

/// Every schedule across the fleet, tagged with its agent id and last-run time.
pub async fn list(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root.join("agents")) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let agent = entry.file_name().to_string_lossy().to_string();
                let last_run: serde_json::Value = std::fs::read_to_string(last_run_file(&root, &agent))
                    .ok()
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                for mut sched in read_array(&schedules_file(&root, &agent)) {
                    if let Some(obj) = sched.as_object_mut() {
                        let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        obj.insert("agent_id".into(), serde_json::json!(agent));
                        obj.insert(
                            "last_run".into(),
                            last_run.get(&id).cloned().unwrap_or(serde_json::Value::Null),
                        );
                    }
                    out.push(sched);
                }
            }
        }
        Ok(serde_json::json!(out))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct AddBody {
    name: String,
    cron: String,
    prompt: String,
    #[serde(default)]
    channel: Option<String>,
}

/// Add (or replace by name-slug) a schedule for an agent — mirrors `schedule add`.
pub async fn add(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Json(body): Json<AddBody>,
) -> Response {
    if !valid_id(&agent) {
        return err(StatusCode::BAD_REQUEST, "invalid agent id");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let name = body.name.trim();
        let cron = body.cron.trim();
        let prompt = body.prompt.trim();
        if name.is_empty() || cron.is_empty() || prompt.is_empty() {
            anyhow::bail!("name, cron and prompt are all required");
        }
        if cron.split_whitespace().count() != 5 {
            anyhow::bail!("cron must have 5 fields (min hour dom month dow)");
        }
        let id = slugify(name);
        let path = schedules_file(&root, &agent);
        let mut arr = read_array(&path);
        arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
        arr.push(serde_json::json!({
            "id": id,
            "agent_id": agent,
            "name": name,
            "cron": cron,
            "prompt": prompt,
            "channel": body.channel,
            "enabled": true,
            "created_at": chrono::Utc::now().to_rfc3339(),
        }));
        write_array(&path, &arr)?;
        Ok(serde_json::json!({ "added": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Flip a schedule's `enabled` flag.
pub async fn toggle(
    State(state): State<AppState>,
    Path((agent, id)): Path<(String, String)>,
) -> Response {
    if !valid_id(&agent) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let path = schedules_file(&root, &agent);
        let mut arr = read_array(&path);
        let mut found = false;
        let mut now_enabled = false;
        for s in arr.iter_mut() {
            if s.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) {
                let cur = s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                now_enabled = !cur;
                if let Some(obj) = s.as_object_mut() {
                    obj.insert("enabled".into(), serde_json::json!(now_enabled));
                }
                found = true;
            }
        }
        if !found {
            anyhow::bail!("no such schedule");
        }
        write_array(&path, &arr)?;
        Ok(serde_json::json!({ "id": id, "enabled": now_enabled }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Delete a schedule.
pub async fn delete(
    State(state): State<AppState>,
    Path((agent, id)): Path<(String, String)>,
) -> Response {
    if !valid_id(&agent) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let path = schedules_file(&root, &agent);
        let mut arr = read_array(&path);
        let before = arr.len();
        arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
        if arr.len() == before {
            anyhow::bail!("no such schedule");
        }
        write_array(&path, &arr)?;
        Ok(serde_json::json!({ "deleted": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
