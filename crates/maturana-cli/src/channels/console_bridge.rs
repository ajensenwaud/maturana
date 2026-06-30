use std::fs;

use maturana_core::{
    improvement::{signals, TrajectoryStore},
    session_db::{ensure_session, session_paths},
    state::MaturanaHome,
};

use super::{
    apply_channel_selection,
    command_catalog::{command_selector_buttons, help_text},
    command_handler::{handle_channel_command, status_text},
    create_subagent, frame_subtask, mark_onboarded, onboarding_prompt, set_onboarding_active,
    slugify_channel_id, SpawnMode,
};

pub(crate) fn console_chat_key() -> i64 {
    maturana_ops::conversation::console_chat_key()
}

pub(crate) fn record_console_turn(
    home: &MaturanaHome,
    agent_id: &str,
    role: &str,
    text: &str,
) -> anyhow::Result<()> {
    maturana_ops::conversation::append_channel_turn(home, agent_id, console_chat_key(), role, text)
}

pub(crate) fn clear_console_transcript(home: &MaturanaHome, agent_id: &str) -> std::io::Result<()> {
    let path =
        maturana_ops::conversation::channel_transcript_path(home, agent_id, console_chat_key());
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(crate) fn read_console_transcript(
    home: &MaturanaHome,
    agent_id: &str,
) -> Vec<(String, String)> {
    let path =
        maturana_ops::conversation::channel_transcript_path(home, agent_id, console_chat_key());
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut role: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    let flush = |role: &Option<String>, body: &[String], out: &mut Vec<(String, String)>| {
        if let Some(r) = role {
            let t = body.join("\n").trim().to_string();
            if !t.is_empty() {
                out.push((r.clone(), t));
            }
        }
    };
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(&role, &body, &mut out);
            body.clear();
            role = rest.split_whitespace().last().map(|s| s.to_string());
        } else if role.is_some() {
            body.push(line.to_string());
        }
    }
    flush(&role, &body, &mut out);
    out
}

pub(crate) struct SelectOption {
    pub label: String,
    pub apply: Box<dyn FnOnce() -> String + Send>,
}

pub(crate) enum ConsoleCommand {
    Reply(String),
    Prompt(String),
    Clear,
    NewSession,
    Quit,
    Select {
        title: String,
        options: Vec<SelectOption>,
    },
}

pub(crate) fn run_console_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    raw: &str,
) -> ConsoleCommand {
    dispatch_slash_command(
        home,
        agent_id,
        session_id,
        console_chat_key(),
        "console",
        &console_chat_key().to_string(),
        raw,
    )
}

pub(crate) fn dispatch_slash_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    raw: &str,
) -> ConsoleCommand {
    let trimmed = raw.trim();
    let (head, args) = match trimmed.split_once(char::is_whitespace) {
        Some((h, a)) => (h, a.trim()),
        None => (trimmed, ""),
    };
    let name = head
        .trim_start_matches('/')
        .replace('_', "-")
        .to_ascii_lowercase();

    match name.as_str() {
        "help" | "start" => ConsoleCommand::Reply(format!(
            "{}\n\nKeys: Enter send · Alt+Enter newline · PgUp/PgDn scroll · / menu · \
             Esc interrupts a reply · Ctrl+C quits.",
            help_text()
        )),
        "clear" => ConsoleCommand::Clear,
        "quit" | "exit" => ConsoleCommand::Quit,
        "new" | "reset" => {
            let _ = maturana_ops::conversation::reset_channel_context(home, agent_id, chat_id);
            ConsoleCommand::NewSession
        }
        "status" => ConsoleCommand::Reply(status_text(home, agent_id, session_id, channel)),
        "good" | "bad" => {
            let value = if name == "good" {
                signals::THUMBS_UP
            } else {
                signals::THUMBS_DOWN
            };
            let reply = match TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))
                .and_then(|store| store.reward_latest(agent_id, session_id, channel, value, None))
            {
                Ok(Some(_)) if value > 0.0 => "Logged a 👍 on the last reply.".to_string(),
                Ok(Some(_)) => "Logged a 👎 on the last reply.".to_string(),
                Ok(None) => "No recent agent turn to rate yet.".to_string(),
                Err(error) => format!("Could not record feedback: {error:#}"),
            };
            ConsoleCommand::Reply(reply)
        }
        "skill" if !args.is_empty() => {
            let (skill, rest) = match args.split_once(char::is_whitespace) {
                Some((s, r)) => (s, r.trim()),
                None => (args, ""),
            };
            ConsoleCommand::Prompt(
                format!("Use the `{skill}` skill. {rest}")
                    .trim()
                    .to_string(),
            )
        }
        "emerge" if !args.is_empty() => {
            let sub = slugify_channel_id(args);
            let _ = create_subagent(home, agent_id, &sub, SpawnMode::Ephemeral, args);
            ConsoleCommand::Prompt(frame_subtask(&sub, args))
        }
        "onboard" => {
            let _ = mark_onboarded(home, agent_id);
            set_onboarding_active(home, agent_id);
            ConsoleCommand::Prompt(onboarding_prompt())
        }
        "model" | "models" | "reasoning" | "tts-provider" | "session" if args.is_empty() => {
            match command_selector_buttons(home, agent_id, &name) {
                Some((title, buttons, _cols)) => {
                    let options = buttons
                        .into_iter()
                        .map(|(label, data)| {
                            let home = MaturanaHome::new(home.root().to_path_buf());
                            let agent = agent_id.to_string();
                            SelectOption {
                                label,
                                apply: Box::new(move || {
                                    apply_channel_selection(&home, &agent, &data)
                                }),
                            }
                        })
                        .collect();
                    ConsoleCommand::Select { title, options }
                }
                None => match handle_channel_command(
                    home,
                    agent_id,
                    session_id,
                    chat_id,
                    channel,
                    platform_id,
                    &name,
                    args,
                ) {
                    Ok(reply) => ConsoleCommand::Reply(reply),
                    Err(error) => ConsoleCommand::Reply(format!("Command failed: {error:#}")),
                },
            }
        }
        _ => match handle_channel_command(
            home,
            agent_id,
            session_id,
            chat_id,
            channel,
            platform_id,
            &name,
            args,
        ) {
            Ok(reply) => ConsoleCommand::Reply(reply),
            Err(error) => ConsoleCommand::Reply(format!("Command failed: {error:#}")),
        },
    }
}

pub(crate) fn apply_web_console_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    cmd: ConsoleCommand,
) -> anyhow::Result<String> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let reply = |text: String| -> anyhow::Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        maturana_core::session_db::write_outbound(
            &paths, None, "chat", "web", "web", None, &content,
        )
    };
    match cmd {
        ConsoleCommand::Reply(text) => reply(text),
        ConsoleCommand::Prompt(prompt) => maturana_ops::conversation::enqueue_turn(
            home,
            agent_id,
            session_id,
            "web",
            "web",
            chat_id,
            None,
            &prompt,
            serde_json::json!({}),
        ),
        ConsoleCommand::Clear | ConsoleCommand::NewSession => {
            let _ = maturana_ops::conversation::reset_channel_context(home, agent_id, chat_id);
            reply("Conversation reset — starting fresh.".to_string())
        }
        ConsoleCommand::Quit => reply("Close the browser tab to end the session.".to_string()),
        ConsoleCommand::Select { title, options } => {
            let labels: Vec<String> = options.into_iter().map(|o| o.label).collect();
            reply(format!(
                "{title}\nOptions: {}\n(Re-send the command with a value, e.g. `/model <name>`.)",
                labels.join(", ")
            ))
        }
    }
}
