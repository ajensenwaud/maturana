use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use chrono::Utc;
use maturana_core::{
    secrets::resolve_secret_source_with_home,
    session_db::{ensure_session, session_paths, SessionPaths},
    state::MaturanaHome,
};

use crate::{
    channels::{
        agent_knowledge_graph, apply_channel_selection, audit_channel_event,
        command_selector_buttons, deliver_outbox, dispatch_slash_command, enqueue_channel_prompt,
        enqueue_turn, sanitize_document_name, stable_chat_key, ConsoleCommand, DiscordServe,
        OutboundSink,
    },
    session::{run_session_once, RunnerOptions},
};

const DISCORD_API: &str = "https://discord.com/api/v10";
// GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT. MESSAGE_CONTENT
// is privileged and must be enabled in the Discord Developer Portal.
const DISCORD_INTENTS: u64 = 1 | (1 << 9) | (1 << 12) | (1 << 15);
/// Discord's upload limit for a standard (non-boosted) server.
const MAX_DISCORD_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// Where this agent's Discord bridge would push an unsolicited message: the last
/// channel it received a message in. Discord has no static paired destination, so
/// the running bridge persists the channel id for host-side delivery to reuse.
pub(crate) fn current_discord_delivery_channel(
    home: &MaturanaHome,
    agent_id: &str,
) -> Option<String> {
    let path = discord_last_channel_path(home, agent_id);
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn discord_last_channel_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/discord/last-channel")
}

/// Persist the Discord channel the bot last heard from so host-side delivery
/// can reach the same conversation. Best-effort.
fn remember_discord_channel(home: &MaturanaHome, agent_id: &str, channel_id: &str) {
    let path = discord_last_channel_path(home, agent_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, channel_id);
}

pub(super) fn serve_discord(home: &MaturanaHome, config: DiscordServe) -> anyhow::Result<()> {
    let token = resolve_secret_source_with_home(&config.bot_token_source, home.root())?;
    let token = token.expose_for_runtime().to_string();
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    ensure_session(&paths)?;
    println!("discord channel serving agent {}", config.agent_id);
    loop {
        if let Err(error) = discord_gateway_session(home, &config, &token, &paths) {
            eprintln!("discord gateway error: {error}");
        }
        if config.once {
            break;
        }
        thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

/// Connect the Discord Gateway, IDENTIFY, heartbeat on schedule, and turn
/// MESSAGE_CREATE events into agent prompts until the socket drops.
fn discord_gateway_session(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    paths: &SessionPaths,
) -> anyhow::Result<()> {
    let (mut socket, _) = tungstenite::connect("wss://gateway.discord.gg/?v=10&encoding=json")
        .map_err(|e| anyhow::anyhow!("discord gateway connect failed: {e}"))?;
    discord_set_read_timeout(&mut socket, Duration::from_millis(1000));

    let mut heartbeat_interval = Duration::from_secs(41);
    let mut last_heartbeat = std::time::Instant::now();
    let mut last_seq: Option<i64> = None;
    let mut identified = false;
    let mut self_id: Option<String> = None;
    let mut last_channel: Option<String> = None;
    let mut last_flush = std::time::Instant::now();

    loop {
        if last_flush.elapsed() >= Duration::from_millis(1000) {
            if let Some(chan) = last_channel.clone() {
                let _ = deliver_discord_outbox(
                    home,
                    &config.agent_id,
                    &config.session_id,
                    bot_token,
                    &chan,
                );
            }
            last_flush = std::time::Instant::now();
        }
        if last_heartbeat.elapsed() >= heartbeat_interval {
            let hb = serde_json::json!({ "op": 1, "d": last_seq }).to_string();
            socket
                .send(tungstenite::Message::Text(hb))
                .map_err(|e| anyhow::anyhow!("discord heartbeat send: {e}"))?;
            last_heartbeat = std::time::Instant::now();
        }

        let msg = match socket.read() {
            Ok(msg) => msg,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("discord read: {e}")),
        };
        let text = match msg {
            tungstenite::Message::Text(t) => t,
            tungstenite::Message::Close(_) => {
                return Err(anyhow::anyhow!("discord gateway closed"));
            }
            _ => continue,
        };
        let event: serde_json::Value = serde_json::from_str(&text)?;
        let op = event.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);
        if let Some(s) = event.get("s").and_then(|v| v.as_i64()) {
            last_seq = Some(s);
        }
        match op {
            10 => {
                if let Some(ms) = event
                    .pointer("/d/heartbeat_interval")
                    .and_then(|v| v.as_u64())
                {
                    heartbeat_interval = Duration::from_millis(ms);
                }
                last_heartbeat = std::time::Instant::now();
                if !identified {
                    let identify = serde_json::json!({
                        "op": 2,
                        "d": {
                            "token": bot_token,
                            "intents": DISCORD_INTENTS,
                            "properties": { "os": "linux", "browser": "maturana", "device": "maturana" }
                        }
                    })
                    .to_string();
                    socket
                        .send(tungstenite::Message::Text(identify))
                        .map_err(|e| anyhow::anyhow!("discord identify send: {e}"))?;
                    identified = true;
                }
            }
            1 => {
                let hb = serde_json::json!({ "op": 1, "d": last_seq }).to_string();
                let _ = socket.send(tungstenite::Message::Text(hb));
                last_heartbeat = std::time::Instant::now();
            }
            11 => {}
            7 | 9 => {
                return Err(anyhow::anyhow!(
                    "discord gateway requested reconnect (op {op})"
                ));
            }
            0 => {
                let t = event.get("t").and_then(|v| v.as_str()).unwrap_or("");
                match t {
                    "READY" => {
                        self_id = event
                            .pointer("/d/user/id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                    }
                    "MESSAGE_CREATE" => {
                        if let Some((channel_id, content, attachments)) =
                            discord_extract_message(&event, self_id.as_deref())
                        {
                            last_channel = Some(channel_id.clone());
                            remember_discord_channel(home, &config.agent_id, &channel_id);
                            if !attachments.is_empty() {
                                handle_discord_attachments(
                                    home,
                                    config,
                                    bot_token,
                                    &channel_id,
                                    &attachments,
                                    &content,
                                );
                            }
                            if !content.is_empty() {
                                handle_discord_content(
                                    home,
                                    config,
                                    bot_token,
                                    paths,
                                    &channel_id,
                                    content,
                                )?;
                            }
                        }
                    }
                    "INTERACTION_CREATE" => handle_discord_interaction(home, config, &event),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn handle_discord_content(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    paths: &SessionPaths,
    channel_id: &str,
    content: String,
) -> anyhow::Result<()> {
    let turn_text = if content.trim_start().starts_with('/') {
        handle_discord_slash(home, config, bot_token, channel_id, &content)
    } else {
        Some(content)
    };
    if let Some(text) = turn_text {
        enqueue_channel_prompt(
            home,
            &config.agent_id,
            &config.session_id,
            "discord",
            channel_id,
            None,
            &text,
        )?;
        if let Some(provider) = &config.run_once_provider {
            let options = RunnerOptions {
                provider: provider.to_string(),
            };
            run_session_once(paths, &options, 20)?;
        }
        deliver_discord_outbox(
            home,
            &config.agent_id,
            &config.session_id,
            bot_token,
            channel_id,
        )?;
    }
    Ok(())
}

fn handle_discord_slash(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    channel_id: &str,
    content: &str,
) -> Option<String> {
    let trimmed = content.trim();
    let (head, sel_args) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    let cmd = head
        .trim_start_matches('/')
        .replace('_', "-")
        .to_ascii_lowercase();
    let is_selector = matches!(
        cmd.as_str(),
        "model" | "models" | "reasoning" | "tts-provider" | "session"
    );
    if is_selector && sel_args.trim().is_empty() {
        match command_selector_buttons(home, &config.agent_id, &cmd) {
            Some((prompt, buttons, cols)) => {
                let _ = discord_post_message_with_buttons(
                    bot_token, channel_id, &prompt, &buttons, cols,
                );
            }
            None => {
                let _ = discord_post_message(
                    bot_token,
                    channel_id,
                    "No options available for that command.",
                );
            }
        }
        return None;
    }

    match dispatch_slash_command(
        home,
        &config.agent_id,
        &config.session_id,
        stable_chat_key(channel_id),
        "discord",
        channel_id,
        content,
    ) {
        ConsoleCommand::Reply(text) => {
            let _ = discord_post_message(bot_token, channel_id, &text);
            None
        }
        ConsoleCommand::Prompt(text) => Some(text),
        ConsoleCommand::NewSession => {
            let _ = discord_post_message(bot_token, channel_id, "New session started.");
            None
        }
        ConsoleCommand::Clear => {
            let _ = discord_post_message(bot_token, channel_id, "Cleared.");
            None
        }
        ConsoleCommand::Quit => {
            let _ = discord_post_message(bot_token, channel_id, "`/quit` is console-only.");
            None
        }
        ConsoleCommand::Select { title, options } => {
            let mut msg = title.lines().next().unwrap_or("Options").to_string();
            for opt in &options {
                msg.push_str(&format!("\n• {}", opt.label));
            }
            let _ = discord_post_message(bot_token, channel_id, &msg);
            None
        }
    }
}

fn handle_discord_interaction(
    home: &MaturanaHome,
    config: &DiscordServe,
    event: &serde_json::Value,
) {
    let d = event.pointer("/d");
    let itype = d
        .and_then(|d| d.get("type"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if itype != 3 {
        return;
    }
    let iid = d
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let itok = d
        .and_then(|d| d.get("token"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let custom_id = d
        .and_then(|d| d.pointer("/data/custom_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if iid.is_empty() || itok.is_empty() || custom_id.is_empty() {
        return;
    }
    let confirm = apply_channel_selection(home, &config.agent_id, custom_id);
    let _ = discord_interaction_callback(iid, itok, &confirm);
    let _ = audit_channel_event(
        home,
        &config.agent_id,
        "channel.discord.callback",
        custom_id,
    );
}

/// Set a read timeout on the gateway socket so the heartbeat loop can run even
/// when no events arrive.
fn discord_set_read_timeout(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    dur: Duration,
) {
    match socket.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(dur));
        }
        tungstenite::stream::MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(dur));
        }
        _ => {}
    }
}

/// Pull (channel_id, content, attachments) from a MESSAGE_CREATE event; skip
/// bot/own messages. Content may be empty as long as there are attachments.
pub(super) fn discord_extract_message(
    event: &serde_json::Value,
    self_id: Option<&str>,
) -> Option<(String, String, Vec<(String, String)>)> {
    let d = event.get("d")?;
    if d.pointer("/author/bot").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    if let (Some(self_id), Some(author_id)) =
        (self_id, d.pointer("/author/id").and_then(|v| v.as_str()))
    {
        if self_id == author_id {
            return None;
        }
    }
    let channel_id = d.get("channel_id").and_then(|v| v.as_str())?.to_string();
    let content = d
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let attachments: Vec<(String, String)> = d
        .get("attachments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let url = a.get("url").and_then(|v| v.as_str())?;
                    let name = a
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("attachment");
                    Some((name.to_string(), url.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    if content.is_empty() && attachments.is_empty() {
        return None;
    }
    Some((channel_id, strip_discord_mention(&content), attachments))
}

fn strip_discord_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<@") {
        if let Some(close) = rest.find('>') {
            return rest[close + 1..].trim().to_string();
        }
    }
    text.to_string()
}

fn discord_post_message(
    bot_token: &str,
    channel_id: &str,
    text: &str,
) -> anyhow::Result<Option<String>> {
    let content: String = text.chars().take(2000).collect();
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .send_json(serde_json::json!({ "content": content }))
            .map_err(|e| anyhow::anyhow!("discord send message failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

fn discord_post_message_with_buttons(
    bot_token: &str,
    channel_id: &str,
    content: &str,
    buttons: &[(String, String)],
    columns: usize,
) -> anyhow::Result<Option<String>> {
    let buttons: Vec<&(String, String)> = buttons.iter().take(25).collect();
    let per_row = columns.max(buttons.len().div_ceil(5)).clamp(1, 5);
    let rows: Vec<serde_json::Value> = buttons
        .chunks(per_row)
        .map(|chunk| {
            let comps: Vec<serde_json::Value> = chunk
                .iter()
                .map(|(label, data)| {
                    serde_json::json!({
                        "type": 2,
                        "style": 1,
                        "label": label.chars().take(80).collect::<String>(),
                        "custom_id": data,
                    })
                })
                .collect();
            serde_json::json!({ "type": 1, "components": comps })
        })
        .collect();
    let content: String = content.chars().take(2000).collect();
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .send_json(serde_json::json!({ "content": content, "components": rows }))
            .map_err(|e| anyhow::anyhow!("discord send buttons failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

fn discord_post_message_with_files(
    bot_token: &str,
    channel_id: &str,
    text: &str,
    files: &[String],
) -> anyhow::Result<Option<String>> {
    let boundary = "maturanadiscordfileboundary7e3f";
    let mut body: Vec<u8> = Vec::new();
    let content: String = text.chars().take(2000).collect();
    let payload = serde_json::json!({ "content": content }).to_string();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"content-disposition: form-data; name=\"payload_json\"\r\ncontent-type: application/json\r\n\r\n",
    );
    body.extend_from_slice(payload.as_bytes());
    body.extend_from_slice(b"\r\n");
    let mut attached = 0usize;
    for (i, path) in files.iter().enumerate() {
        let p = Path::new(path);
        match fs::metadata(p) {
            Ok(meta) if meta.len() > MAX_DISCORD_UPLOAD_BYTES => {
                eprintln!("discord: {path} exceeds the 25 MB upload limit, skipping");
                continue;
            }
            Ok(_) => {}
            Err(_) => continue,
        }
        let bytes = match fs::read(p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let filename = p
            .file_name()
            .map(|n| n.to_string_lossy().replace(['"', '\r', '\n'], "_"))
            .unwrap_or_else(|| format!("file{i}"));
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("content-disposition: form-data; name=\"files[{i}]\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(&bytes);
        body.extend_from_slice(b"\r\n");
        attached += 1;
    }
    if attached == 0 {
        anyhow::bail!("no attachable files (all missing or over 25 MB)");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let resp: serde_json::Value =
        ureq::post(&format!("{DISCORD_API}/channels/{channel_id}/messages"))
            .set("authorization", &format!("Bot {bot_token}"))
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
            .map_err(|e| anyhow::anyhow!("discord file upload failed: {e}"))?
            .into_json()?;
    Ok(resp.get("id").and_then(|v| v.as_str()).map(str::to_string))
}

fn discord_download_attachment(url: &str, dest: &Path, max_bytes: u64) -> anyhow::Result<u64> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("discord attachment download failed: {e}"))?;
    let mut reader = resp.into_reader().take(max_bytes + 1);
    let mut bytes: Vec<u8> = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("attachment exceeds {max_bytes} bytes");
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, &bytes)?;
    Ok(bytes.len() as u64)
}

fn handle_discord_attachments(
    home: &MaturanaHome,
    config: &DiscordServe,
    bot_token: &str,
    channel_id: &str,
    attachments: &[(String, String)],
    caption: &str,
) {
    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph = match (
        maturana_core::worker::read_graph_token(home.root()),
        knowledge_graph.enabled,
    ) {
        (Some(token), true) => Some((token, crate::graph::agent_graph_name(&config.agent_id))),
        _ => None,
    };
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    let _ = fs::create_dir_all(&inbox);
    let mut lines: Vec<String> = Vec::new();
    for (name, url) in attachments {
        let file_name = sanitize_document_name(Some(name));
        let dest = inbox.join(format!(
            "{}-{file_name}",
            Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        if let Err(error) = discord_download_attachment(url, &dest, MAX_DISCORD_UPLOAD_BYTES) {
            lines.push(format!("• `{file_name}` — download failed: {error}"));
            continue;
        }
        let ext = file_name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        let is_image = matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "heif" | "tif" | "tiff"
        );
        if is_image {
            match crate::deliver_image_to_guest(home, &config.agent_id, &dest) {
                Ok(guest_path) => {
                    let cap = (!caption.trim().is_empty()).then_some(caption);
                    let prompt = crate::vision_prompt_text(cap, &guest_path);
                    let _ = enqueue_turn(
                        home,
                        &config.agent_id,
                        &config.session_id,
                        "discord",
                        channel_id,
                        stable_chat_key(channel_id),
                        None,
                        &prompt,
                        serde_json::json!({ "image": guest_path }),
                    );
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.image",
                        &format!("delivered image to guest ({guest_path}); running vision turn"),
                    );
                    lines.push(format!("• `{file_name}` — 👁️ viewing it now"));
                    continue;
                }
                Err(error) => {
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.image_fallback",
                        &format!("guest image delivery failed ({error:#}); falling back"),
                    );
                }
            }
        }
        let supported = file_name
            .rsplit_once('.')
            .map(|(_, ext)| {
                crate::graph::SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str())
            })
            .unwrap_or(false);
        match (&graph, supported) {
            (Some((token, graph_name)), true) => match crate::graph::ingest_file_into_service(
                crate::graph::DEFAULT_LOCAL_URL,
                token,
                graph_name,
                &dest,
                1800,
            ) {
                Ok(chunks) => {
                    let _ = audit_channel_event(
                        home,
                        &config.agent_id,
                        "channel.discord.document",
                        &format!("ingested {file_name} ({chunks} chunks)"),
                    );
                    lines.push(format!(
                        "• `{file_name}` → added to my knowledge graph ({chunks} chunks)"
                    ));
                }
                Err(error) => lines.push(format!("• `{file_name}` — could not ingest: {error}")),
            },
            _ => lines.push(format!("• `{file_name}` — saved to my inbox")),
        }
    }
    if lines.is_empty() {
        return;
    }
    let trailer = if caption.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\n_re: {}_",
            caption.trim().chars().take(180).collect::<String>()
        )
    };
    let _ = discord_post_message(
        bot_token,
        channel_id,
        &format!("📎 Received:\n{}{trailer}", lines.join("\n")),
    );
}

fn discord_interaction_callback(
    interaction_id: &str,
    interaction_token: &str,
    content: &str,
) -> anyhow::Result<()> {
    let content: String = content.chars().take(2000).collect();
    ureq::post(&format!(
        "{DISCORD_API}/interactions/{interaction_id}/{interaction_token}/callback"
    ))
    .send_json(serde_json::json!({
        "type": 7,
        "data": { "content": content, "components": [] },
    }))
    .map_err(|e| anyhow::anyhow!("discord interaction callback failed: {e}"))?;
    Ok(())
}

struct DiscordSink<'a> {
    bot_token: &'a str,
    channel_id: &'a str,
}

impl OutboundSink for DiscordSink<'_> {
    fn send(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        _reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        discord_post_message(self.bot_token, self.channel_id, text)
    }

    fn send_files(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        _reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        match discord_post_message_with_files(self.bot_token, self.channel_id, text, files) {
            Ok(id) => Ok(id),
            Err(error) => {
                eprintln!("discord: file upload failed ({error:#}); sending text only");
                let names: Vec<String> = files
                    .iter()
                    .filter_map(|f| {
                        Path::new(f)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                    })
                    .collect();
                let msg = if text.trim().is_empty() {
                    format!("(couldn't attach: {})", names.join(", "))
                } else {
                    format!("{text}\n(couldn't attach: {})", names.join(", "))
                };
                discord_post_message(self.bot_token, self.channel_id, &msg)
            }
        }
    }
}

fn deliver_discord_outbox(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    bot_token: &str,
    channel_id: &str,
) -> anyhow::Result<usize> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let key = stable_chat_key(channel_id);
    let mut sink = DiscordSink {
        bot_token,
        channel_id,
    };
    deliver_outbox(
        home, agent_id, &paths, "discord", channel_id, key, None, &mut sink,
    )
}
