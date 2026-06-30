use maturana_core::improvement::signals;

use super::{
    slugify_channel_id,
    telegram::{TelegramDocument, TelegramMessage, TelegramUpdate},
    SpawnMode,
};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum InboundAction {
    Ignore,
    Pair {
        chat_id: i64,
    },
    Help {
        chat_id: i64,
    },
    /// (Re)run the first-run onboarding interview, routed back to this chat.
    Onboard {
        chat_id: i64,
    },
    Status {
        chat_id: i64,
    },
    New {
        chat_id: i64,
    },
    Spawn {
        chat_id: i64,
        mode: SpawnMode,
        name: String,
        prompt: String,
    },
    Prompt {
        chat_id: i64,
        text: String,
    },
    Document {
        chat_id: i64,
        document: TelegramDocument,
        caption: Option<String>,
    },
    Photo {
        chat_id: i64,
        file_id: String,
        caption: Option<String>,
    },
    /// A voice note or audio file to transcribe (STT), then treat as a prompt.
    Voice {
        chat_id: i64,
        file_id: String,
        filename: String,
    },
    Tool {
        chat_id: i64,
        name: String,
        input: String,
    },
    Feedback {
        chat_id: i64,
        value: f64,
    },
    Command {
        chat_id: i64,
        name: String,
        args: String,
    },
    Deny {
        chat_id: i64,
    },
}

pub(super) fn classify_telegram_update(
    update: &TelegramUpdate,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
) -> InboundAction {
    let Some(message) = telegram_message(update) else {
        return InboundAction::Ignore;
    };
    let chat_id = message.chat.id;
    if let Some(document) = &message.document {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Document {
            chat_id,
            document: document.clone(),
            caption: message.caption.clone(),
        };
    }
    if let Some(largest) = message.photo.as_ref().and_then(|sizes| sizes.last()) {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Photo {
            chat_id,
            file_id: largest.file_id.clone(),
            caption: message.caption.clone(),
        };
    }
    if let Some((file_id, filename)) = message
        .voice
        .as_ref()
        .map(|v| (v.file_id.clone(), "voice.ogg".to_string()))
        .or_else(|| {
            message.audio.as_ref().map(|a| {
                (
                    a.file_id.clone(),
                    a.file_name
                        .clone()
                        .unwrap_or_else(|| "audio.ogg".to_string()),
                )
            })
        })
    {
        if paired_chat_id != Some(chat_id) {
            return InboundAction::Deny { chat_id };
        }
        return InboundAction::Voice {
            chat_id,
            file_id,
            filename,
        };
    }
    let text = message.text.as_deref().unwrap_or("").trim();
    if text.is_empty() {
        return InboundAction::Ignore;
    }
    if let Some(code) = pair_code {
        if is_pair_command(text, code) {
            return InboundAction::Pair { chat_id };
        }
    }
    if paired_chat_id != Some(chat_id) {
        return InboundAction::Deny { chat_id };
    }
    let command = normalize_bot_command(text);
    let (cmd, args) = match command.split_once(char::is_whitespace) {
        Some((c, a)) => (c.to_ascii_lowercase(), a.trim().to_string()),
        None => (command.to_ascii_lowercase(), String::new()),
    };
    let cmd = if cmd.starts_with('/') {
        cmd.replace('_', "-")
    } else {
        cmd
    };
    match cmd.as_str() {
        "/start" | "/help" => InboundAction::Help { chat_id },
        "/status" => InboundAction::Status { chat_id },
        "/new" => InboundAction::New { chat_id },
        "/good" => InboundAction::Feedback {
            chat_id,
            value: signals::THUMBS_UP,
        },
        "/bad" => InboundAction::Feedback {
            chat_id,
            value: signals::THUMBS_DOWN,
        },
        "/tool" => match parse_tool_command(&command) {
            Some((name, input)) => InboundAction::Tool {
                chat_id,
                name,
                input,
            },
            None => InboundAction::Help { chat_id },
        },
        "/spawn" => match parse_spawn_command(&command) {
            Some((mode, name, prompt)) => InboundAction::Spawn {
                chat_id,
                mode,
                name,
                prompt,
            },
            None => InboundAction::Help { chat_id },
        },
        "/emerge" if !args.is_empty() => InboundAction::Spawn {
            chat_id,
            mode: SpawnMode::Ephemeral,
            name: slugify_channel_id(&args),
            prompt: args,
        },
        "/skill" if !args.is_empty() => {
            let (skill, rest) = match args.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args.as_str(), ""),
            };
            InboundAction::Prompt {
                chat_id,
                text: format!("Use the `{skill}` skill. {rest}")
                    .trim()
                    .to_string(),
            }
        }
        "/onboard" => InboundAction::Onboard { chat_id },
        "/commands" | "/tools" | "/models" | "/model" | "/reasoning" | "/reset" | "/stop"
        | "/compact" | "/session" | "/subagents" | "/graph-query" | "/graph-insert" | "/tts"
        | "/tts-provider" | "/emerge" | "/skill" | "/loop" => InboundAction::Command {
            chat_id,
            name: cmd.trim_start_matches('/').to_string(),
            args,
        },
        _ if cmd.starts_with('/') => InboundAction::Command {
            chat_id,
            name: "unknown".to_string(),
            args: cmd,
        },
        _ => InboundAction::Prompt {
            chat_id,
            text: text.to_string(),
        },
    }
}

fn parse_tool_command(command: &str) -> Option<(String, String)> {
    let rest = command.strip_prefix("/tool")?.trim();
    let (name, input) = match rest.split_once(char::is_whitespace) {
        Some((name, input)) => (name.trim(), input.trim()),
        None => (rest, ""),
    };
    if !maturana_core::tools::is_valid_tool_name(name) {
        return None;
    }
    let input = if input.is_empty() { "{}" } else { input };
    Some((name.to_string(), input.to_string()))
}

fn parse_spawn_command(command: &str) -> Option<(SpawnMode, String, String)> {
    let rest = command.strip_prefix("/spawn")?.trim();
    let (head, prompt) = rest.split_once("--")?;
    let mut parts = head.split_whitespace();
    let first = parts.next()?;
    let (mode, name) = match first {
        "ephemeral" => (SpawnMode::Ephemeral, parts.next()?),
        "persistent" => (SpawnMode::Persistent, parts.next()?),
        name => (SpawnMode::Ephemeral, name),
    };
    let prompt = prompt.trim();
    if name.trim().is_empty() || prompt.is_empty() {
        return None;
    }
    Some((mode, slugify_channel_id(name), prompt.to_string()))
}

pub(super) fn telegram_message(update: &TelegramUpdate) -> Option<&TelegramMessage> {
    update.message.as_ref().or(update.channel_post.as_ref())
}

pub(super) fn is_pair_command(text: &str, code: &str) -> bool {
    let normalized = normalize_bot_command(text.trim());
    let Some(rest) = normalized
        .strip_prefix("/pair")
        .or_else(|| normalized.strip_prefix("pair"))
    else {
        return false;
    };
    rest.trim() == code
}

fn normalize_bot_command(text: &str) -> String {
    let Some((command, rest)) = text.split_once(' ') else {
        return text
            .split_once('@')
            .map(|(command, _)| command)
            .unwrap_or(text)
            .to_string();
    };
    if let Some((base, _)) = command.split_once('@') {
        if base.starts_with('/') {
            return format!("{base} {rest}");
        }
    }
    text.to_string()
}
