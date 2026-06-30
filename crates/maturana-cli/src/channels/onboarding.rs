use std::{fs, path::PathBuf};

use chrono::Utc;
use maturana_core::{
    session_db::{ensure_session, insert_inbound, session_paths},
    state::MaturanaHome,
};

use super::{build_channel_prompt, settings::load_channel_settings};

fn onboarded_marker(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("onboarded")
}

pub(super) fn is_onboarded(home: &MaturanaHome, agent_id: &str) -> bool {
    onboarded_marker(home, agent_id).exists()
}

pub(super) fn mark_onboarded(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let path = onboarded_marker(home, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, Utc::now().to_rfc3339())?;
    Ok(())
}

/// The agent's reply ends with this when it has finished the onboarding
/// interview. The host clears the active state and strips it before sending.
const ONBOARDING_COMPLETE_SENTINEL: &str = "[[ONBOARDING_COMPLETE]]";

fn onboarding_active_marker(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("onboarding-active")
}

#[cfg(test)]
pub(super) fn is_onboarding_active(home: &MaturanaHome, agent_id: &str) -> bool {
    onboarding_active_marker(home, agent_id).exists()
}

pub(super) fn set_onboarding_active(home: &MaturanaHome, agent_id: &str) {
    let path = onboarding_active_marker(home, agent_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, Utc::now().to_rfc3339());
}

fn clear_onboarding_active(home: &MaturanaHome, agent_id: &str) {
    let _ = fs::remove_file(onboarding_active_marker(home, agent_id));
}

/// If the agent signalled it finished onboarding, end the interview and strip the
/// sentinel from the user-facing reply. Returns the text to actually send.
pub(crate) fn finalize_onboarding_reply(
    home: &MaturanaHome,
    agent_id: &str,
    reply: &str,
) -> String {
    if reply.contains(ONBOARDING_COMPLETE_SENTINEL) {
        clear_onboarding_active(home, agent_id);
        reply
            .replace(ONBOARDING_COMPLETE_SENTINEL, "")
            .trim()
            .to_string()
    } else {
        reply.to_string()
    }
}

/// First contact: the agent greets the user and runs a short onboarding
/// interview so it learns who they are and records it to memory + IDENTITY.md.
pub(super) fn onboarding_prompt() -> String {
    "[FIRST CONTACT - your owner just paired with you; they have NOT spoken yet.]\n\n\
     Greet them warmly and briefly in your own voice (per SOUL.md), then begin a short \
     onboarding interview. Ask their name and how they'd like to be addressed, their \
     timezone / working hours, and the main things they want your help with. Ask only \
     1-2 questions at a time - keep it natural, not a form. As you learn durable facts, \
     save them to your memory and fill in IDENTITY.md's \"Who you are to me\" section. \
     Send your greeting and first question now."
        .to_string()
}

/// Enqueue the onboarding turn for a telegram chat, tagged with the real
/// `telegram`/`chat_id` so the greeting returns to the paired chat.
pub(super) fn enqueue_onboarding(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
) -> anyhow::Result<()> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let directive = onboarding_prompt();
    let prompt = build_channel_prompt(home, agent_id, chat_id, &directive)?;
    set_onboarding_active(home, agent_id);
    let settings = load_channel_settings(home, agent_id);
    insert_inbound(
        &paths,
        "chat",
        "telegram",
        &chat_id.to_string(),
        None,
        &serde_json::json!({
            "text": directive,
            "prompt": prompt,
            "model": settings.model,
            "reasoning": settings.reasoning,
        })
        .to_string(),
    )?;
    Ok(())
}
