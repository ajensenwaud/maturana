use std::fs;

use chrono::Utc;
use maturana_core::state::MaturanaHome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SpawnMode {
    Ephemeral,
    Persistent,
}

/// Frame a spawned sub-task as a focused instruction for the agent's next turn.
pub(super) fn frame_subtask(name: &str, task: &str) -> String {
    format!(
        "You've been handed a focused sub-task labelled `{name}`. Work on it now \
         and report back with the result when you're done.\n\nTask: {task}"
    )
}

pub(super) fn create_subagent(
    home: &MaturanaHome,
    parent_agent_id: &str,
    name: &str,
    mode: SpawnMode,
    prompt: &str,
) -> anyhow::Result<String> {
    let subagent_id = slugify_channel_id(name);
    let path = home
        .agent_dir(parent_agent_id)
        .join("subagents")
        .join(format!("{subagent_id}.json"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "id": subagent_id,
            "parent_agent_id": parent_agent_id,
            "mode": match mode {
                SpawnMode::Ephemeral => "ephemeral",
                SpawnMode::Persistent => "persistent",
            },
            "prompt": prompt,
            "created_at": Utc::now(),
        }))?,
    )?;
    Ok(subagent_id)
}

pub(super) fn slugify_channel_id(value: &str) -> String {
    let slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "subagent".to_string()
    } else {
        slug
    }
}
