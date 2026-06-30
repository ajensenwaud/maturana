#[cfg(test)]
use std::path::PathBuf;

use maturana_core::{
    session_db::{
        claim_delivery, ensure_session, insert_inbound, list_undelivered, mark_delivered,
        session_paths,
    },
    state::MaturanaHome,
};

use crate::session::message_text;

pub(crate) fn build_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<String> {
    maturana_ops::conversation::build_channel_prompt(home, agent_id, chat_id, user_message)
}

pub(super) fn append_channel_turn(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    role: &str,
    text: &str,
) -> anyhow::Result<()> {
    maturana_ops::conversation::append_channel_turn(home, agent_id, chat_id, role, text)
}

pub(super) fn reset_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
) -> anyhow::Result<()> {
    maturana_ops::conversation::reset_channel_context(home, agent_id, chat_id)
}

#[cfg(test)]
pub(super) fn extract_memory_fact(text: &str) -> Option<String> {
    maturana_ops::conversation::extract_memory_fact(text)
}

#[cfg(test)]
pub(super) fn maybe_remember_user_message(
    home: &MaturanaHome,
    agent_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    maturana_ops::conversation::maybe_remember_user_message(home, agent_id, text)
}

#[cfg(test)]
pub(super) fn channel_transcript_path(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
) -> PathBuf {
    maturana_ops::conversation::channel_transcript_path(home, agent_id, chat_id)
}

#[cfg(test)]
pub(super) fn channel_context_manifest_path(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
) -> PathBuf {
    maturana_ops::conversation::channel_context_manifest_path(home, agent_id, chat_id)
}

pub(super) fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "\n...[truncated]"
}

pub(crate) fn enqueue_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    chat_key: i64,
    thread_id: Option<&str>,
    text: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    maturana_ops::conversation::enqueue_turn(
        home,
        agent_id,
        session_id,
        channel,
        platform_id,
        chat_key,
        thread_id,
        text,
        extra,
    )
}

#[cfg(test)]
pub(crate) fn enqueue_outreach_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    directive: &str,
    kind: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    maturana_ops::conversation::enqueue_outreach_turn(
        home, agent_id, session_id, chat_id, directive, kind, extra,
    )
}

pub(crate) fn enqueue_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    maturana_ops::conversation::enqueue_channel_prompt(
        home,
        agent_id,
        session_id,
        channel,
        platform_id,
        thread_id,
        text,
    )
}

pub(crate) struct DispatchHandle {
    pub session_id: String,
    pub message_id: String,
}

pub(super) fn build_dispatch_prompt(home: &MaturanaHome, agent_id: &str, task: &str) -> String {
    let agent_dir = home.agent_dir(agent_id);
    let head = |rel: &str, cap: usize| -> String {
        std::fs::read_to_string(agent_dir.join(rel))
            .map(|s| s.chars().take(cap).collect::<String>())
            .unwrap_or_default()
    };
    let identity = head("AGENTS.md", 4000);
    let memory = head("memory/MEMORY.md", 4000);
    let mut out = String::new();
    if !identity.trim().is_empty() {
        out.push_str("=== WHO YOU ARE ===\n");
        out.push_str(identity.trim());
        out.push_str("\n\n");
    }
    if !memory.trim().is_empty() {
        out.push_str("=== YOUR MEMORY (operator, contacts, context) ===\n");
        out.push_str(memory.trim());
        out.push_str("\n\n");
    }
    out.push_str("=== TASK ===\n");
    out.push_str(task);
    out
}

pub(crate) fn enqueue_dispatch_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    run_id: &str,
    framed_task: &str,
    model: Option<&str>,
) -> anyhow::Result<DispatchHandle> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let content = serde_json::json!({
        "text": framed_task,
        "prompt": build_dispatch_prompt(home, agent_id, framed_task),
        "model": model,
        "reasoning": serde_json::Value::Null,
    });
    let message_id = insert_inbound(
        &paths,
        "dispatch",
        "orchestrate",
        run_id,
        None,
        &content.to_string(),
    )?;
    Ok(DispatchHandle {
        session_id: session_id.to_string(),
        message_id,
    })
}

pub(crate) fn try_collect_dispatch(
    home: &MaturanaHome,
    agent_id: &str,
    handle: &DispatchHandle,
) -> anyhow::Result<Option<String>> {
    let paths = session_paths(&home.agent_dir(agent_id), &handle.session_id);
    let Some(message) = list_undelivered(&paths)?
        .into_iter()
        .find(|m| m.in_reply_to.as_deref() == Some(&handle.message_id))
    else {
        return Ok(None);
    };
    if !claim_delivery(&paths, &message.id)? {
        return Ok(None);
    }
    let text = match message_text(&message.content) {
        Ok(text) => text,
        Err(error) => format!("[unparseable worker reply: {error}]"),
    };
    mark_delivered(&paths, &message.id, Some("orchestrate"))?;
    Ok(Some(text))
}
