//! Skill catalog: the repo's `skills/*/SKILL.md` (the repo root is the home
//! dir's parent by the same convention the prompt console uses).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{blocking, err, ok};
use crate::state::AppState;

fn skills_dir(state: &AppState) -> std::path::PathBuf {
    state
        .home_root
        .parent()
        .map(|p| p.join("skills"))
        .unwrap_or_else(|| state.home_root.join("skills"))
}

pub async fn list(State(state): State<AppState>) -> Response {
    let dir = skills_dir(&state);
    match blocking(move || {
        let mut skills = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if !path.exists() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                // First non-heading paragraph = the "use this when" summary.
                let summary = raw
                    .lines()
                    .skip_while(|line| line.starts_with('#') || line.trim().is_empty())
                    .take_while(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                skills.push(serde_json::json!({ "name": name, "summary": summary }));
            }
        }
        skills.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(serde_json::json!(skills))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn detail(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return err(StatusCode::BAD_REQUEST, "invalid skill name");
    }
    let path = skills_dir(&state).join(&name).join("SKILL.md");
    match blocking(move || Ok(std::fs::read_to_string(&path)?)).await {
        Ok(markdown) => ok(serde_json::json!({ "name": name, "markdown": markdown })),
        Err(response) => response,
    }
}
