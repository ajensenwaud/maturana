use std::{fs, path::Path, time::Duration};

use anyhow::Context;

use super::{
    command_catalog::COMMAND_GROUPS,
    telegram::{
        TelegramGetMeResponse, TelegramOkResponse, TelegramSendResponse, TelegramUpdate,
        TelegramUpdatesResponse,
    },
};

/// Fetch the bot username so pairing instructions can include `/pair@bot`.
pub(super) fn telegram_bot_username(token: &str) -> anyhow::Result<Option<String>> {
    let response: TelegramGetMeResponse =
        ureq::get(&format!("https://api.telegram.org/bot{token}/getMe"))
            .call()
            .context("Telegram getMe failed")?
            .into_json()
            .context("failed to parse Telegram getMe response")?;
    if !response.ok {
        anyhow::bail!("Telegram getMe returned ok=false");
    }
    Ok(response.result.and_then(|user| user.username))
}

/// Fetch updates. `long_poll_secs > 0` uses Telegram long-polling.
pub(super) fn telegram_updates(
    token: &str,
    offset: Option<i64>,
    long_poll_secs: u64,
) -> anyhow::Result<Vec<TelegramUpdate>> {
    let mut url =
        format!("https://api.telegram.org/bot{token}/getUpdates?timeout={long_poll_secs}");
    if let Some(offset) = offset {
        url.push_str(&format!("&offset={offset}"));
    }
    let response: TelegramUpdatesResponse = ureq::get(&url)
        .timeout(Duration::from_secs(long_poll_secs + 15))
        .call()
        .context("Telegram getUpdates failed")?
        .into_json()
        .context("failed to parse Telegram getUpdates response")?;
    if !response.ok {
        anyhow::bail!("Telegram getUpdates returned ok=false");
    }
    Ok(response.result)
}

/// Upload a host-side file to the chat via Telegram `sendDocument`.
pub(super) fn send_telegram_document(
    token: &str,
    chat_id: i64,
    path: &Path,
    caption: Option<&str>,
    reply_to: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() > MAX_DOCUMENT_BYTES {
        anyhow::bail!(
            "{} is {} bytes (over Telegram's 50 MB sendDocument limit)",
            path.display(),
            meta.len()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().replace(['"', '\r', '\n'], "_"))
        .unwrap_or_else(|| "file".to_string());
    let boundary = "maturanadocboundary7e3f";
    let mut body: Vec<u8> = Vec::new();
    let field = |name: &str, value: &str, body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("content-disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };
    field("chat_id", &chat_id.to_string(), &mut body);
    if let Some(caption) = caption {
        let caption: String = caption.chars().take(1000).collect();
        field("caption", &caption, &mut body);
    }
    if let Some(id) = reply_to {
        field("reply_to_message_id", &id.to_string(), &mut body);
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "content-disposition: form-data; name=\"document\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let response = ureq::post(&format!("https://api.telegram.org/bot{token}/sendDocument"))
        .set(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(Duration::from_secs(120))
        .send_bytes(&body)
        .map_err(|error| anyhow::anyhow!("telegram sendDocument failed: {error}"))?;
    let parsed: TelegramSendResponse = response.into_json().unwrap_or(TelegramSendResponse {
        ok: false,
        result: None,
    });
    Ok(parsed.result.map(|message| message.message_id))
}

/// Base URL for the Telegram Bot API. Overridable for local transport tests.
fn tg_api_base() -> String {
    std::env::var("MATURANA_TELEGRAM_API_BASE")
        .unwrap_or_else(|_| "https://api.telegram.org".to_string())
}

/// Hard ceiling on every bounded Telegram HTTP call.
const TG_HTTP_TIMEOUT: Duration = Duration::from_secs(12);

fn tg_live_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(4))
            .timeout_read(Duration::from_secs(6))
            .timeout_write(Duration::from_secs(6))
            .build()
    })
}

/// Outcome of a live-progress edit, so callers can back off intelligently.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LiveEditOutcome {
    Ok,
    Throttled(Option<u64>),
    Failed(String),
}

fn edit_telegram_live(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> LiveEditOutcome {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    let req = tg_live_agent()
        .post(&format!("{}/bot{token}/editMessageText", tg_api_base()))
        .set("content-type", "application/json");
    match req.send_string(&body.to_string()) {
        Ok(resp) => match resp.into_json::<TelegramOkResponse>() {
            Ok(parsed) if parsed.ok => LiveEditOutcome::Ok,
            Ok(_) => LiveEditOutcome::Failed("editMessageText returned ok=false".to_string()),
            Err(error) => LiveEditOutcome::Failed(format!("parse: {error}")),
        },
        Err(ureq::Error::Status(429, resp)) => {
            let retry_after = resp
                .header("retry-after")
                .and_then(|value| value.trim().parse::<u64>().ok());
            LiveEditOutcome::Throttled(retry_after)
        }
        Err(ureq::Error::Status(400, resp)) => {
            let desc = resp.into_string().unwrap_or_default();
            if desc.contains("not modified") {
                LiveEditOutcome::Ok
            } else {
                LiveEditOutcome::Failed(format!("status 400: {}", desc.trim()))
            }
        }
        Err(ureq::Error::Status(code, _)) => LiveEditOutcome::Failed(format!("status {code}")),
        Err(error) => LiveEditOutcome::Failed(error.to_string()),
    }
}

pub(super) fn edit_telegram_live_html(
    token: &str,
    chat_id: i64,
    message_id: i64,
    html: &str,
) -> LiveEditOutcome {
    edit_telegram_live(token, chat_id, message_id, html, Some("HTML"))
}

pub(crate) fn send_telegram(
    token: &str,
    chat_id: &str,
    message: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    send_telegram_with(token, chat_id, message, reply_to_message_id, None)
}

pub(super) fn send_telegram_html(
    token: &str,
    chat_id: &str,
    html: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    send_telegram_with(token, chat_id, html, reply_to_message_id, Some("HTML"))
}

fn send_telegram_with(
    token: &str,
    chat_id: &str,
    message: &str,
    reply_to_message_id: Option<i64>,
    parse_mode: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    if let Some(message_id) = reply_to_message_id {
        body["reply_parameters"] = serde_json::json!({
            "message_id": message_id,
        });
    }
    let response: TelegramSendResponse =
        ureq::post(&format!("{}/bot{token}/sendMessage", tg_api_base()))
            .timeout(TG_HTTP_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendMessage failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram sendMessage response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram sendMessage returned ok=false");
    }
    Ok(response.result.map(|message| message.message_id))
}

/// Register Telegram's slash-command menu from the shared channel catalog.
pub(super) fn set_telegram_commands(token: &str) -> anyhow::Result<()> {
    let mut commands: Vec<serde_json::Value> = Vec::new();
    for (_, cmds) in COMMAND_GROUPS {
        for (name, desc) in *cmds {
            let command = name.trim_start_matches('/').replace('-', "_");
            let description: String = desc.chars().take(256).collect();
            commands.push(serde_json::json!({
                "command": command,
                "description": description,
            }));
        }
    }
    let body = serde_json::json!({ "commands": commands });
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/setMyCommands"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram setMyCommands failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram setMyCommands response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram setMyCommands returned ok=false");
    }
    Ok(())
}

pub(super) fn send_telegram_keyboard(
    token: &str,
    chat_id: &str,
    text: &str,
    buttons: &[(String, String)],
    columns: usize,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let columns = columns.max(1);
    let rows: Vec<Vec<serde_json::Value>> = buttons
        .chunks(columns)
        .map(|chunk| {
            chunk
                .iter()
                .map(|(label, data)| serde_json::json!({ "text": label, "callback_data": data }))
                .collect()
        })
        .collect();
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "reply_markup": { "inline_keyboard": rows },
    });
    if let Some(message_id) = reply_to_message_id {
        body["reply_parameters"] = serde_json::json!({ "message_id": message_id });
    }
    let response: TelegramSendResponse =
        ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendMessage (keyboard) failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram sendMessage response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram sendMessage (keyboard) returned ok=false");
    }
    Ok(response.result.map(|message| message.message_id))
}

pub(super) fn answer_callback_query(
    token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> anyhow::Result<()> {
    let mut body = serde_json::json!({ "callback_query_id": callback_query_id });
    if let Some(text) = text {
        if !text.is_empty() {
            body["text"] = serde_json::json!(text);
        }
    }
    let response: TelegramOkResponse = ureq::post(&format!(
        "https://api.telegram.org/bot{token}/answerCallbackQuery"
    ))
    .set("content-type", "application/json")
    .send_string(&body.to_string())
    .map_err(|error| anyhow::anyhow!("Telegram answerCallbackQuery failed: {error}"))
    .and_then(|response| {
        response.into_json().map_err(|error| {
            anyhow::anyhow!("failed to parse Telegram answerCallbackQuery response: {error}")
        })
    })?;
    if !response.ok {
        anyhow::bail!("Telegram answerCallbackQuery returned ok=false");
    }
    Ok(())
}

pub(crate) fn send_telegram_chat_action(
    token: &str,
    chat_id: &str,
    action: &str,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "action": action,
    });
    let response: TelegramOkResponse =
        ureq::post(&format!("{}/bot{token}/sendChatAction", tg_api_base()))
            .timeout(TG_HTTP_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram sendChatAction failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram sendChatAction response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram sendChatAction returned ok=false");
    }
    Ok(())
}

pub(super) fn edit_telegram_message(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> anyhow::Result<()> {
    edit_telegram_message_with(token, chat_id, message_id, text, None)
}

fn edit_telegram_message_with(
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> anyhow::Result<()> {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::json!(mode);
    }
    let response: TelegramOkResponse =
        ureq::post(&format!("{}/bot{token}/editMessageText", tg_api_base()))
            .timeout(TG_HTTP_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram editMessageText failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram editMessageText response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram editMessageText returned ok=false");
    }
    Ok(())
}

pub(super) fn delete_telegram_message(
    token: &str,
    chat_id: i64,
    message_id: i64,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
    });
    let response: TelegramOkResponse =
        ureq::post(&format!("{}/bot{token}/deleteMessage", tg_api_base()))
            .timeout(TG_HTTP_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|error| anyhow::anyhow!("Telegram deleteMessage failed: {error}"))
            .and_then(|response| {
                response.into_json().map_err(|error| {
                    anyhow::anyhow!("failed to parse Telegram deleteMessage response: {error}")
                })
            })?;
    if !response.ok {
        anyhow::bail!("Telegram deleteMessage returned ok=false");
    }
    Ok(())
}
