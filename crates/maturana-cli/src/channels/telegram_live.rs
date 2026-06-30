use std::{fs, path::PathBuf, sync::mpsc, thread, time::Duration};

use maturana_core::{
    animation::{frame, is_terminal, Phase},
    improvement::TrajectoryStore,
    session_db::{
        claim_delivery, clear_progress, find_reply_outbound, mark_delivered, read_progress,
        unclaim_delivery, ProgressEvent, SessionPaths,
    },
    state::MaturanaHome,
    tools::{run_tool, ToolRegistry},
};

use crate::session::message_text;

use super::telegram_api::{
    delete_telegram_message, edit_telegram_live_html, edit_telegram_message, send_telegram_html,
    LiveEditOutcome,
};
use super::{
    append_channel_turn, audit_channel_event, finalize_onboarding_reply, send_telegram,
    send_telegram_chat_action, truncate_chars, truncate_for_telegram, voice::maybe_send_tts,
    TelegramServe,
};

const LIVE_EDIT_BASE: Duration = Duration::from_millis(1000);
const LIVE_EDIT_MAX: Duration = Duration::from_secs(20);

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn titleize(key: &str) -> String {
    key.split(|c| c == '_' || c == '-' || c == ' ')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn tool_display(key: &str) -> (&'static str, String) {
    let known: Option<(&str, &str)> = match key.trim().to_ascii_lowercase().as_str() {
        "bash" | "exec" | "shell" | "command" | "command_execution" => Some(("🛠️", "Bash")),
        "process" => Some(("🧰", "Process")),
        "read" => Some(("📖", "Read")),
        "write" => Some(("✍️", "Write")),
        "edit" | "file_change" | "apply_patch" | "patch" => Some(("📝", "Edit")),
        "attach" => Some(("📎", "Attach")),
        "browser" | "browse" => Some(("🌐", "Browser")),
        "web_search" | "search" | "websearch" => Some(("🔎", "Web Search")),
        "web_fetch" | "fetch" => Some(("📄", "Web Fetch")),
        "code_execution" => Some(("🧮", "Code Execution")),
        "update_plan" | "plan" | "todo_list" | "todo" => Some(("🗺️", "Update Plan")),
        "memory_search" => Some(("🗄️", "Memory Search")),
        "memory_get" => Some(("📓", "Memory Get")),
        "image" | "image_generate" => Some(("🎨", "Image")),
        "mcp_tool_call" | "tool_call" | "tool_call_update" => Some(("🧰", "Tool Call")),
        "message" => Some(("✉️", "Message")),
        _ => None,
    };
    match known {
        Some((emoji, title)) => (emoji, title.to_string()),
        None => ("🧩", titleize(key)),
    }
}

fn parse_tool_event(text: &str) -> (String, String) {
    if let Some((key, detail)) = text.split_once('\u{1f}') {
        return (key.trim().to_string(), detail.to_string());
    }
    let detail = text
        .strip_prefix("running: ")
        .or_else(|| text.strip_prefix("done: "))
        .or_else(|| text.find("): ").map(|index| &text[index + 3..]))
        .unwrap_or(text);
    ("bash".to_string(), detail.to_string())
}

pub(super) fn render_progress_html(events: &[ProgressEvent]) -> String {
    let mut lines: Vec<(String, String)> = Vec::new();
    let mut text = "";
    let mut errored = false;
    for event in events {
        match event.kind.as_str() {
            "tool" => lines.push(parse_tool_event(&event.text)),
            "text" => text = event.text.as_str(),
            "status" if event.text == "error" => errored = true,
            _ => {}
        }
    }
    let mut block = String::new();
    for (key, detail) in lines.iter().rev().take(8).rev() {
        let (emoji, title) = tool_display(key);
        let detail = collapse_ws(detail.trim());
        let line = if key == "bash" || key == "exec" {
            if detail.is_empty() {
                format!("{emoji} {title}")
            } else {
                format!("{emoji} {detail}")
            }
        } else if detail.is_empty() {
            format!("{emoji} {title}")
        } else {
            format!("{emoji} {title}: {detail}")
        };
        block.push_str(&truncate_chars(&line, 160));
        block.push('\n');
    }
    let block = block.trim_end();
    if !block.is_empty() {
        truncate_for_telegram(&format!("<pre>{}</pre>", html_escape(block)))
    } else if !text.trim().is_empty() {
        html_escape(&truncate_chars(text.trim(), 3000))
    } else if errored {
        "<pre>error</pre>".to_string()
    } else {
        String::new()
    }
}

fn telegram_status_path(paths: &SessionPaths, inbound_id: &str) -> PathBuf {
    let safe: String = inbound_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    paths.dir.join("progress").join(format!("{safe}.tgstatus"))
}

fn set_telegram_status(paths: &SessionPaths, inbound_id: &str, message_id: i64) {
    let path = telegram_status_path(paths, inbound_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, message_id.to_string());
}

pub(super) fn peek_telegram_status(paths: &SessionPaths, inbound_id: &str) -> Option<i64> {
    let path = telegram_status_path(paths, inbound_id);
    fs::read_to_string(&path).ok()?.trim().parse::<i64>().ok()
}

pub(super) fn clear_telegram_status(paths: &SessionPaths, inbound_id: &str) {
    let _ = fs::remove_file(telegram_status_path(paths, inbound_id));
}

fn telegram_active_path(paths: &SessionPaths, inbound_id: &str) -> PathBuf {
    let safe: String = inbound_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    paths.dir.join("progress").join(format!("{safe}.tgactive"))
}

fn set_telegram_active(paths: &SessionPaths, inbound_id: &str) {
    let path = telegram_active_path(paths, inbound_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"1");
}

fn clear_telegram_active(paths: &SessionPaths, inbound_id: &str) {
    let _ = fs::remove_file(telegram_active_path(paths, inbound_id));
}

pub(super) fn telegram_active_exists(paths: &SessionPaths, inbound_id: &str) -> bool {
    telegram_active_path(paths, inbound_id).exists()
}

pub(super) fn finalize_reply(
    token: &str,
    chat_id: i64,
    live_id: Option<i64>,
    reply: &str,
    reply_to: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    match live_id {
        Some(id) => match edit_telegram_message(token, chat_id, id, reply) {
            Ok(()) => Ok(Some(id)),
            Err(edit_err) => {
                delete_telegram_message(token, chat_id, id).map_err(|del_err| {
                    anyhow::anyhow!(
                        "edit failed ({edit_err}); refusing to send a duplicate because the stale live message could not be removed ({del_err})"
                    )
                })?;
                send_telegram(token, &chat_id.to_string(), reply, reply_to)
            }
        },
        None => send_telegram(token, &chat_id.to_string(), reply, reply_to),
    }
}

pub(super) fn stream_turn_to_telegram(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    inbound_id: &str,
    reply_to: Option<i64>,
    paths: &SessionPaths,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let mut message_id: Option<i64> = None;
    let mut last_render = String::new();
    let mut last_edit_ok = true;
    let started = std::time::Instant::now();
    let mut edit_interval = LIVE_EDIT_BASE;
    let mut next_edit_at = started;
    let mut last_typing = started
        .checked_sub(Duration::from_secs(10))
        .unwrap_or(started);
    set_telegram_active(paths, inbound_id);

    let (reply_tx, reply_rx) = mpsc::channel();
    let watcher_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let paths = paths.clone();
        let inbound_id = inbound_id.to_string();
        let stop = watcher_stop.clone();
        thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match find_reply_outbound(&paths, &inbound_id) {
                    Ok(Some(message)) if message.channel == "telegram" => {
                        let _ = reply_tx.send(message);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!("telegram: reply watcher query failed (retrying): {error:#}")
                    }
                }
                thread::sleep(Duration::from_millis(1000));
            }
        });
    }

    struct TurnGuard<'a> {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        paths: &'a SessionPaths,
        inbound_id: &'a str,
    }
    impl Drop for TurnGuard<'_> {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            clear_telegram_active(self.paths, self.inbound_id);
        }
    }
    let _turn_guard = TurnGuard {
        stop: watcher_stop.clone(),
        paths,
        inbound_id,
    };

    loop {
        if let Some(final_msg) = reply_rx.try_recv().ok() {
            if !claim_delivery(paths, &final_msg.id)? {
                if let Some(id) = message_id {
                    let _ = delete_telegram_message(token, chat_id, id);
                }
                clear_telegram_status(paths, inbound_id);
                let _ = clear_progress(paths, inbound_id);
                return Ok(());
            }

            let reply = match message_text(&final_msg.content) {
                Ok(text) => {
                    truncate_for_telegram(&finalize_onboarding_reply(home, &config.agent_id, &text))
                }
                Err(error) => {
                    eprintln!(
                        "telegram: dropping unparseable outbound {}: {error:#}",
                        final_msg.id
                    );
                    clear_telegram_status(paths, inbound_id);
                    let _ = mark_delivered(paths, &final_msg.id, None);
                    let _ = clear_progress(paths, inbound_id);
                    return Ok(());
                }
            };

            if reply.trim() == crate::proactive::SILENCE_SENTINEL {
                if let Some(id) = message_id {
                    let _ = delete_telegram_message(token, chat_id, id);
                }
                clear_telegram_status(paths, inbound_id);
                let _ = mark_delivered(paths, &final_msg.id, None);
                let _ = clear_progress(paths, inbound_id);
                return Ok(());
            }

            match finalize_reply(token, chat_id, message_id, &reply, reply_to) {
                Ok(platform_id) => {
                    let _ = mark_delivered(
                        paths,
                        &final_msg.id,
                        platform_id.map(|id| id.to_string()).as_deref(),
                    );
                    clear_telegram_status(paths, inbound_id);
                    let _ =
                        append_channel_turn(home, &config.agent_id, chat_id, "assistant", &reply);
                    maybe_send_tts(home, token, &config.agent_id, chat_id, &reply, reply_to);
                    let _ = clear_progress(paths, inbound_id);
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.telegram.outbound",
                        "sent telegram response",
                    );
                }
                Err(error) => {
                    eprintln!("telegram delivery failed, will retry: {error:#}");
                    unclaim_delivery(paths, &final_msg.id)?;
                }
            }
            return Ok(());
        }

        if last_typing.elapsed() >= Duration::from_secs(4) {
            let _ = send_telegram_chat_action(token, &chat_id.to_string(), "typing");
            last_typing = std::time::Instant::now();
        }

        let t_prog = std::time::Instant::now();
        let progress = render_progress_html(&read_progress(paths, inbound_id).unwrap_or_default());
        let prog_ms = t_prog.elapsed().as_millis();
        let secs = started.elapsed().as_secs();
        let clock = format!("{}:{:02}", secs / 60, secs % 60);
        let rendered = if progress.is_empty() {
            if started.elapsed() >= Duration::from_secs(2) {
                format!("<pre>💭 Thinking… {clock}</pre>")
            } else {
                String::new()
            }
        } else {
            format!("{progress}\n<pre>⏳ {clock}</pre>")
        };

        let due = std::time::Instant::now() >= next_edit_at;
        let t_edit = std::time::Instant::now();
        if !rendered.is_empty() && due && (rendered != last_render || !last_edit_ok) {
            match message_id {
                Some(id) => match edit_telegram_live_html(token, chat_id, id, &rendered) {
                    LiveEditOutcome::Ok => {
                        last_render = rendered;
                        last_edit_ok = true;
                        edit_interval = LIVE_EDIT_BASE;
                    }
                    LiveEditOutcome::Throttled(retry_after) => {
                        if last_edit_ok {
                            eprintln!(
                                "telegram: live progress throttled (429, retry_after={retry_after:?}); backing off"
                            );
                        }
                        last_edit_ok = false;
                        edit_interval = retry_after
                            .map(Duration::from_secs)
                            .unwrap_or(edit_interval * 2)
                            .clamp(LIVE_EDIT_BASE, LIVE_EDIT_MAX);
                    }
                    LiveEditOutcome::Failed(error) => {
                        if last_edit_ok {
                            eprintln!(
                                "telegram: live progress update failing, will keep retrying: {error}"
                            );
                        }
                        last_edit_ok = false;
                        edit_interval = (edit_interval * 2).min(LIVE_EDIT_MAX);
                    }
                },
                None => {
                    message_id =
                        send_telegram_html(token, &chat_id.to_string(), &rendered, reply_to)
                            .unwrap_or(None);
                    if let Some(id) = message_id {
                        set_telegram_status(paths, inbound_id, id);
                        last_render = rendered;
                        last_edit_ok = true;
                        edit_interval = LIVE_EDIT_BASE;
                    } else {
                        last_edit_ok = false;
                        edit_interval = (edit_interval * 2).min(LIVE_EDIT_MAX);
                    }
                }
            }
            next_edit_at = t_edit + edit_interval;
        }

        let edit_ms = t_edit.elapsed().as_millis();
        if prog_ms > 800 || edit_ms > 800 {
            eprintln!("telegram loop slow @ {clock}: progress={prog_ms}ms edit={edit_ms}ms");
        }
        if std::time::Instant::now() >= deadline {
            return Ok(());
        }
        let nap = next_edit_at
            .saturating_duration_since(std::time::Instant::now())
            .clamp(Duration::from_millis(100), Duration::from_millis(500));
        thread::sleep(nap);
    }
}

pub(super) fn run_tool_with_animation(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    name: &str,
    input: &str,
) -> anyhow::Result<()> {
    let registry = ToolRegistry::new(home.root().join("tools"));
    if registry.load(name).is_err() {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!("Tool `{name}` is not registered. Use `maturana tool register` first."),
            None,
        )?;
        return Ok(());
    }

    let running = Phase::Running {
        tool: name.to_string(),
    };
    let status_id = send_telegram(token, &chat_id.to_string(), &frame(&running, 0), None)?;

    let (tx, rx) = mpsc::channel();
    {
        let registry = registry.clone();
        let name = name.to_string();
        let input = input.to_string();
        thread::spawn(move || {
            let _ = tx.send(run_tool(&registry, &name, &input));
        });
    }

    let mut tick = 1usize;
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(700)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(message_id) = status_id {
                    let _ =
                        edit_telegram_message(token, chat_id, message_id, &frame(&running, tick));
                }
                tick += 1;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("tool worker thread disconnected"))
            }
        }
    };

    let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
    let trajectory_input = format!("/tool {name} {input}");
    match result {
        Ok(run) => {
            let final_phase = if run.ok {
                Phase::Done {
                    detail: Some(format!("`{name}` in {}ms", run.duration_ms)),
                }
            } else {
                Phase::Failed {
                    detail: Some(truncate_chars(run.stderr.trim(), 80)),
                }
            };
            if let Some(message_id) = status_id {
                let _ =
                    edit_telegram_message(token, chat_id, message_id, &frame(&final_phase, tick));
            }
            let body = if run.ok {
                let out = truncate_for_telegram(&run.stdout);
                if out.trim().is_empty() {
                    "(tool produced no output)".to_string()
                } else {
                    out
                }
            } else {
                format!("Tool failed: {}", truncate_for_telegram(run.stderr.trim()))
            };
            send_telegram(token, &chat_id.to_string(), &body, None)?;
            store.record(
                &config.agent_id,
                &config.session_id,
                "tool",
                &trajectory_input,
                &run.stdout,
                "[]",
            )?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.tool",
                &format!("ran tool {name} ok={}", run.ok),
            )?;
        }
        Err(error) => {
            let message = format!("{error:#}");
            if let Some(message_id) = status_id {
                let _ = edit_telegram_message(
                    token,
                    chat_id,
                    message_id,
                    &frame(
                        &Phase::Failed {
                            detail: Some(truncate_chars(&message, 80)),
                        },
                        tick,
                    ),
                );
            }
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("Tool error: {message}"),
                None,
            )?;
            store.record(
                &config.agent_id,
                &config.session_id,
                "tool",
                &trajectory_input,
                &message,
                "[]",
            )?;
        }
    }
    debug_assert!(is_terminal(&Phase::Done { detail: None }));
    Ok(())
}
