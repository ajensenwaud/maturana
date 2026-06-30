use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use maturana_core::{pipelock::PipelockVault, state::MaturanaHome};
use serde::{Deserialize, Serialize};

pub(super) const TELEGRAM_PAIR_CODE: &str = "telegram/pair-code";
pub(super) const TELEGRAM_CHAT_ID: &str = "telegram/chat-id";

#[derive(Debug, Serialize, Deserialize, Default)]
pub(super) struct TelegramChannelState {
    pub(super) offset: Option<i64>,
    pub(super) last_seen_at: Option<DateTime<Utc>>,
}

pub fn paired_telegram_chat_source(home: &MaturanaHome) -> Option<String> {
    let vault = PipelockVault::new(home.pipelock_dir());
    if vault.get(TELEGRAM_CHAT_ID).is_ok() {
        Some(format!("pipelock:{TELEGRAM_CHAT_ID}"))
    } else {
        None
    }
}

pub(crate) fn current_paired_telegram_chat_id(home: &MaturanaHome, agent_id: &str) -> Option<i64> {
    maturana_ops::conversation::current_paired_telegram_chat_id(home, agent_id)
}

/// Whether this agent's Telegram bridge is currently alive, so an outbound row
/// written to its outbox will actually be sent (not parked forever).
pub(crate) fn telegram_bridge_live(home: &MaturanaHome, agent_id: &str) -> bool {
    let hb = telegram_heartbeat_path(home, agent_id);
    let Ok(meta) = fs::metadata(&hb) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age <= Duration::from_secs(120))
        .unwrap_or(false)
}

pub(super) fn read_telegram_state(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<TelegramChannelState> {
    let path = telegram_state_path(home, agent_id);
    if !path.exists() {
        return Ok(TelegramChannelState::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

pub(super) fn write_telegram_state(
    home: &MaturanaHome,
    agent_id: &str,
    state: &TelegramChannelState,
) -> anyhow::Result<()> {
    let path = telegram_state_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)
        .context("failed to write telegram channel state")
}

pub(super) fn write_telegram_heartbeat(
    home: &MaturanaHome,
    agent_id: &str,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let path = telegram_heartbeat_path(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::json!({
            "agent_id": agent_id,
            "status": status,
            "error": error,
            "at": Utc::now(),
        }))?,
    )
    .context("failed to write telegram channel heartbeat")
}

fn telegram_state_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    if agent_id == "default" {
        home.root().join("channels/telegram/state.json")
    } else {
        home.agent_dir(agent_id)
            .join("channels/telegram/state.json")
    }
}

fn telegram_heartbeat_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    if agent_id == "default" {
        home.root().join("channels/telegram/heartbeat.json")
    } else {
        home.agent_dir(agent_id)
            .join("channels/telegram/heartbeat.json")
    }
}

pub(super) fn telegram_pair_code_key(agent_id: &str) -> String {
    if agent_id == "default" {
        TELEGRAM_PAIR_CODE.to_string()
    } else {
        format!("telegram/{agent_id}/pair-code")
    }
}

pub(super) fn telegram_chat_id_key(agent_id: &str) -> String {
    if agent_id == "default" {
        TELEGRAM_CHAT_ID.to_string()
    } else {
        format!("telegram/{agent_id}/chat-id")
    }
}
