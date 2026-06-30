use std::{thread, time::Duration};

use maturana_core::{
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, session_paths, SessionPaths},
    state::MaturanaHome,
};

use crate::{
    channels::{deliver_channel_outbox, enqueue_channel_prompt, SlackServe},
    session::{run_session_once, RunnerOptions},
};

pub(super) fn serve_slack(home: &MaturanaHome, config: SlackServe) -> anyhow::Result<()> {
    let bot = resolve_secret_source_with_home(&config.bot_token_source, home.root())?;
    let bot = bot.expose_for_runtime().to_string();
    let app = resolve_secret_source_with_home(&config.app_token_source, home.root())?;
    let app = app.expose_for_runtime().to_string();
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!("slack channel serving agent {}", config.agent_id);
    loop {
        if let Err(error) = slack_socket_session(home, &config, &bot, &app, &paths) {
            eprintln!("slack socket error: {error}");
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

/// Open a Socket Mode WebSocket and process events until it drops.
fn slack_socket_session(
    home: &MaturanaHome,
    config: &SlackServe,
    bot_token: &str,
    app_token: &str,
    paths: &SessionPaths,
) -> anyhow::Result<()> {
    let ws_url = slack_open_connection(app_token)?;
    let (mut socket, _) = tungstenite::connect(&ws_url)
        .map_err(|e| anyhow::anyhow!("slack socket connect failed: {e}"))?;
    loop {
        let msg = socket
            .read()
            .map_err(|e| anyhow::anyhow!("slack read: {e}"))?;
        let tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let envelope: serde_json::Value = serde_json::from_str(&text)?;
        let envelope_type = envelope.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(envelope_id) = envelope.get("envelope_id").and_then(|v| v.as_str()) {
            let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
            let _ = socket.send(tungstenite::Message::Text(ack));
        }
        if envelope_type != "events_api" {
            continue;
        }
        if let Some((channel, text, thread)) = slack_extract_prompt(&envelope) {
            enqueue_channel_prompt(
                home,
                &config.agent_id,
                &config.session_id,
                "slack",
                &channel,
                thread.as_deref(),
                &text,
            )?;
            if let Some(provider) = &config.run_once_provider {
                let options = RunnerOptions {
                    provider: provider.to_string(),
                };
                run_session_once(paths, &options, 20)?;
            }
            let bot = bot_token.to_string();
            let thread_for_send = thread.clone();
            deliver_channel_outbox(
                home,
                &config.agent_id,
                &config.session_id,
                "slack",
                &channel,
                |reply, _| slack_post_message(&bot, &channel, thread_for_send.as_deref(), reply),
            )?;
        }
    }
}

fn slack_open_connection(app_token: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = ureq::post("https://slack.com/api/apps.connections.open")
        .set("authorization", &format!("Bearer {app_token}"))
        .call()
        .map_err(|e| anyhow::anyhow!("slack apps.connections.open failed: {e}"))?
        .into_json()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!("slack apps.connections.open returned not-ok: {resp}");
    }
    resp.get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("slack response missing url"))
}

/// Pull (channel, text, thread_ts) from an events_api envelope for a user
/// message / app_mention; returns None for bot messages and non-message events.
pub(super) fn slack_extract_prompt(
    envelope: &serde_json::Value,
) -> Option<(String, String, Option<String>)> {
    let event = envelope.pointer("/payload/event")?;
    let kind = event.get("type").and_then(|t| t.as_str())?;
    if kind != "message" && kind != "app_mention" {
        return None;
    }
    if event.get("bot_id").is_some() || event.get("subtype").is_some() {
        return None;
    }
    let text = event
        .get("text")
        .and_then(|t| t.as_str())?
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let channel = event.get("channel").and_then(|c| c.as_str())?.to_string();
    let thread = event
        .get("thread_ts")
        .or_else(|| event.get("ts"))
        .and_then(|t| t.as_str())
        .map(str::to_string);
    Some((channel, strip_slack_mention(&text), thread))
}

/// Remove a leading `<@U...>` bot mention so the prompt is clean.
pub(super) fn strip_slack_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(close) = rest.find('>') {
            if rest.starts_with('@') {
                return rest[close + 1..].trim().to_string();
            }
        }
    }
    text.to_string()
}

fn slack_post_message(
    bot_token: &str,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let mut body = serde_json::json!({ "channel": channel, "text": text });
    if let Some(thread) = thread_ts {
        body["thread_ts"] = serde_json::json!(thread);
    }
    let resp: serde_json::Value = ureq::post("https://slack.com/api/chat.postMessage")
        .set("authorization", &format!("Bearer {bot_token}"))
        .send_json(body)
        .map_err(|e| anyhow::anyhow!("slack chat.postMessage failed: {e}"))?
        .into_json()?;
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        anyhow::bail!("slack chat.postMessage not-ok: {resp}");
    }
    Ok(resp.get("ts").and_then(|v| v.as_str()).map(str::to_string))
}
