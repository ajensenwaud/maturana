use std::{thread, time::Duration};

use maturana_core::{
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, session_paths},
    state::MaturanaHome,
};
use serde::{Deserialize, Serialize};

use crate::{
    channels::{
        deliver_channel_outbox, enqueue_channel_prompt, read_channel_state, write_channel_state,
        AgentMailServe,
    },
    session::{run_session_once, RunnerOptions},
};

const AGENTMAIL_BASE: &str = "https://api.agentmail.to/v0";

#[derive(Debug, Serialize, Deserialize, Default)]
struct AgentMailState {
    /// Highest message timestamp seen, so we only enqueue newer mail.
    last_seen: Option<String>,
}

pub(super) fn serve_agentmail(home: &MaturanaHome, config: AgentMailServe) -> anyhow::Result<()> {
    let key = resolve_secret_source_with_home(&config.api_key_source, home.root())?;
    let key = key.expose_for_runtime().to_string();
    let inbox = config
        .inbox
        .clone()
        .map(Ok)
        .unwrap_or_else(|| agentmail_default_inbox(&key))?;
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!(
        "agentmail channel serving agent {} inbox {inbox}",
        config.agent_id
    );
    let mut state: AgentMailState = read_channel_state(home, &config.agent_id, "agentmail")?;
    loop {
        match agentmail_poll(&key, &inbox, state.last_seen.as_deref()) {
            Ok(messages) => {
                for msg in &messages {
                    enqueue_channel_prompt(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "agentmail",
                        &msg.thread_id,
                        Some(&msg.message_id),
                        &msg.text,
                    )?;
                    state.last_seen = Some(msg.timestamp.clone());
                }
                write_channel_state(home, &config.agent_id, "agentmail", &state)?;
                if let Some(provider) = &config.run_once_provider {
                    let options = RunnerOptions {
                        provider: provider.to_string(),
                    };
                    run_session_once(&paths, &options, 20)?;
                }
                for msg in &messages {
                    let key2 = key.clone();
                    let inbox2 = inbox.clone();
                    let thread = msg.thread_id.clone();
                    deliver_channel_outbox(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "agentmail",
                        &msg.thread_id,
                        |text, reply_to| agentmail_send(&key2, &inbox2, &thread, reply_to, text),
                    )?;
                }
            }
            Err(error) => eprintln!("agentmail poll error: {error}"),
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(config.poll_seconds.max(2)));
    }
    Ok(())
}

struct AgentMailMessage {
    message_id: String,
    thread_id: String,
    timestamp: String,
    text: String,
}

fn agentmail_default_inbox(key: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = ureq::get(&format!("{AGENTMAIL_BASE}/inboxes"))
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| anyhow::anyhow!("agentmail list inboxes failed: {e}"))?
        .into_json()?;
    resp.get("inboxes")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("inbox_id").or_else(|| i.get("id")))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("agentmail account has no inbox; pass --inbox"))
}

fn agentmail_poll(
    key: &str,
    inbox: &str,
    since: Option<&str>,
) -> anyhow::Result<Vec<AgentMailMessage>> {
    let mut url = format!("{AGENTMAIL_BASE}/inboxes/{inbox}/messages?limit=20");
    if let Some(since) = since {
        url.push_str(&format!("&after={since}"));
    }
    let resp: serde_json::Value = ureq::get(&url)
        .set("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| anyhow::anyhow!("agentmail list messages failed: {e}"))?
        .into_json()?;
    let mut out = Vec::new();
    let items = resp
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    for m in items {
        if m.get("type").and_then(|t| t.as_str()) == Some("sent") {
            continue;
        }
        let text = m
            .get("text")
            .or_else(|| m.get("preview"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        out.push(AgentMailMessage {
            message_id: m
                .get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            thread_id: m
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| m.get("message_id").and_then(|v| v.as_str()).unwrap_or(""))
                .to_string(),
            timestamp: m
                .get("timestamp")
                .or_else(|| m.get("created_at"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            text,
        });
    }
    Ok(out)
}

fn agentmail_send(
    key: &str,
    inbox: &str,
    thread_id: &str,
    reply_to: Option<&str>,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let body = serde_json::json!({
        "text": text,
        "thread_id": thread_id,
        "in_reply_to": reply_to,
    });
    let resp: serde_json::Value =
        ureq::post(&format!("{AGENTMAIL_BASE}/inboxes/{inbox}/messages/send"))
            .set("authorization", &format!("Bearer {key}"))
            .send_json(body)
            .map_err(|e| anyhow::anyhow!("agentmail send failed: {e}"))?
            .into_json()?;
    Ok(resp
        .get("message_id")
        .and_then(|v| v.as_str())
        .map(str::to_string))
}
