use std::{fs, path::PathBuf};

use maturana_core::state::MaturanaHome;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct ChannelSettings {
    #[serde(default)]
    pub(super) model: Option<String>,
    /// Reasoning effort for reasoning-capable harnesses (codex/gpt-5):
    /// low|medium|high. None => the worker's fast default (low).
    #[serde(default)]
    pub(super) reasoning: Option<String>,
    #[serde(default)]
    pub(super) tts_enabled: bool,
    #[serde(default)]
    pub(super) tts_provider: Option<String>,
    #[serde(default)]
    pub(super) idle: bool,
}

/// Reasoning levels offered by `/reasoning` (codex/gpt-5). `low` is snappy;
/// `high` reasons deepest. Validated against this list before storing.
/// `minimal` is intentionally excluded: the codex agent enables the
/// `web_search`/`image_gen` tools, and the API rejects `reasoning.effort
/// minimal` (HTTP 400) whenever those tools are present.
pub(super) const REASONING_LEVELS: &[&str] = &["low", "medium", "high"];

pub(super) fn load_channel_settings(home: &MaturanaHome, agent_id: &str) -> ChannelSettings {
    fs::read_to_string(channel_settings_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(super) fn save_channel_settings(
    home: &MaturanaHome,
    agent_id: &str,
    settings: &ChannelSettings,
) -> anyhow::Result<()> {
    let path = channel_settings_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

fn channel_settings_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("channel-settings.json")
}
