use std::{fs, path::PathBuf};

use maturana_core::state::MaturanaHome;
use serde::Serialize;

pub(super) fn read_channel_state<T: serde::de::DeserializeOwned + Default>(
    home: &MaturanaHome,
    agent_id: &str,
    channel: &str,
) -> anyhow::Result<T> {
    let path = channel_state_path(home, agent_id, channel);
    if !path.exists() {
        return Ok(T::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

pub(super) fn write_channel_state<T: Serialize>(
    home: &MaturanaHome,
    agent_id: &str,
    channel: &str,
    state: &T,
) -> anyhow::Result<()> {
    let path = channel_state_path(home, agent_id, channel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn channel_state_path(home: &MaturanaHome, agent_id: &str, channel: &str) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels")
        .join(channel)
        .join("state.json")
}

/// Stable per-conversation key for channels whose platform id is a string
/// (Slack channel, AgentMail thread). Reuses all the i64-keyed transcript /
/// context machinery without changing the Telegram signatures.
pub(crate) fn stable_chat_key(platform_id: &str) -> i64 {
    maturana_ops::conversation::stable_chat_key(platform_id)
}
