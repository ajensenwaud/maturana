use std::{fs, io::Read, path::Path};

use anyhow::Context;
use chrono::Utc;
use maturana_core::state::MaturanaHome;
use serde::Deserialize;

use crate::channels::{
    append_channel_turn, audit_channel_event, run_channel_prompt, send_telegram,
    send_telegram_chat_action, TelegramDocument, TelegramServe,
};

/// Telegram bot API refuses `getFile` beyond 20 MB; stay under it.
const MAX_TELEGRAM_DOCUMENT_BYTES: u64 = 19 * 1024 * 1024;

/// A document uploaded to the paired chat: download it and ingest it into the
/// agent's knowledge graph.
pub(super) fn handle_telegram_document(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    document: &TelegramDocument,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    let file_name = sanitize_document_name(document.file_name.as_deref());
    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph_token = maturana_core::worker::read_graph_token(home.root());
    let (graph_token, graph_name) = match (graph_token, knowledge_graph.enabled) {
        (Some(token), true) => (token, crate::graph::agent_graph_name(&config.agent_id)),
        _ => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I received the document, but my knowledge graph is not enabled, so I cannot store it. Enable `knowledge_graph` in MATURANA.md and set up the graph service.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    if document.file_size.unwrap_or(0) > MAX_TELEGRAM_DOCUMENT_BYTES as i64 {
        send_telegram(
            token,
            &chat_id.to_string(),
            "That document is larger than 19 MB, which is more than I can pull from Telegram. Please send a smaller file.",
            reply_to_message_id,
        )?;
        return Ok(());
    }
    let supported = file_name
        .rsplit_once('.')
        .map(|(_, ext)| crate::graph::SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !supported {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!(
                "I can ingest these document types: {}. `{file_name}` is not one of them.",
                crate::graph::SUPPORTED_EXTS.join(", ")
            ),
            reply_to_message_id,
        )?;
        return Ok(());
    }

    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    fs::create_dir_all(&inbox)?;
    let dest = inbox.join(format!(
        "{}-{file_name}",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let result = download_telegram_document(token, &document.file_id, &dest).and_then(|_| {
        crate::graph::ingest_file_into_service(
            crate::graph::DEFAULT_LOCAL_URL,
            &graph_token,
            &graph_name,
            &dest,
            1800,
        )
    });
    match result {
        Ok(chunks) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.document",
                &format!("ingested {file_name} into graph '{graph_name}' ({chunks} chunks)"),
            )?;
            let transcript_note = match caption {
                Some(caption) if !caption.trim().is_empty() => {
                    format!("[uploaded document: {file_name}] {}", caption.trim())
                }
                _ => format!("[uploaded document: {file_name}]"),
            };
            append_channel_turn(home, &config.agent_id, chat_id, "user", &transcript_note)?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "Added `{file_name}` to my knowledge graph `{graph_name}` ({chunks} chunks). Ask me about it any time."
                ),
                reply_to_message_id,
            )?;
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.document_error",
                &format!("failed to ingest {file_name}: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I could not ingest `{file_name}`: {error:#}"),
                reply_to_message_id,
            )?;
        }
    }
    Ok(())
}

/// A photo upload: try a vision turn first, then OCR into the graph when the
/// guest is unreachable.
pub(super) fn handle_telegram_photo(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    file_id: &str,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let inbox = home.agent_dir(&config.agent_id).join("inbox");
    fs::create_dir_all(&inbox)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let image_dest = inbox.join(format!("{stamp}-photo.jpg"));
    if let Err(error) = download_telegram_document(token, file_id, &image_dest) {
        send_telegram(
            token,
            &chat_id.to_string(),
            &format!("I couldn't download that image: {error:#}"),
            reply_to_message_id,
        )?;
        return Ok(());
    }

    match crate::deliver_image_to_guest(home, &config.agent_id, &image_dest) {
        Ok(guest_path) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.image",
                &format!("delivered image to guest ({guest_path}); running vision turn"),
            )?;
            let prompt = crate::vision_prompt_text(caption, &guest_path);
            return run_channel_prompt(home, token, config, chat_id, &prompt, reply_to_message_id);
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.image_fallback",
                &format!("guest image delivery failed ({error:#}); falling back to OCR"),
            )?;
        }
    }

    let knowledge_graph = agent_knowledge_graph(home, &config.agent_id);
    let graph_token = maturana_core::worker::read_graph_token(home.root());
    let (graph_token, graph_name) = match (graph_token, knowledge_graph.enabled) {
        (Some(value), true) => (value, crate::graph::agent_graph_name(&config.agent_id)),
        _ => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I received your image but couldn't reach my VM to view it, and my knowledge graph is off, so I can't store it either. Try again, or enable `knowledge_graph`.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };

    let text = match ocr_image_text(&image_dest) {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I read that image but couldn't find any text in it (OCR returned nothing).",
                reply_to_message_id,
            )?;
            return Ok(());
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo_error",
                &format!("OCR failed: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I couldn't OCR that image: {error:#}"),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };

    let text_dest = inbox.join(format!("{stamp}-photo-ocr.md"));
    let heading = caption
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| format!("# {c}\n\n"))
        .unwrap_or_default();
    fs::write(&text_dest, format!("{heading}{}", text.trim()))?;
    let chars = text.trim().chars().count();
    match crate::graph::ingest_file_into_service(
        crate::graph::DEFAULT_LOCAL_URL,
        &graph_token,
        &graph_name,
        &text_dest,
        1800,
    ) {
        Ok(chunks) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo",
                &format!("OCR'd image ({chars} chars) into graph '{graph_name}' ({chunks} chunks)"),
            )?;
            let note = match caption {
                Some(c) if !c.trim().is_empty() => format!("[uploaded image, OCR'd] {}", c.trim()),
                _ => "[uploaded image, OCR'd]".to_string(),
            };
            append_channel_turn(home, &config.agent_id, chat_id, "user", &note)?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "Read the text from your image ({chars} characters) and added it to my knowledge graph `{graph_name}` ({chunks} chunks). Ask me about it any time."
                ),
                reply_to_message_id,
            )?;
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.photo_error",
                &format!("failed to ingest OCR text: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I read the image but couldn't store the text: {error:#}"),
                reply_to_message_id,
            )?;
        }
    }
    Ok(())
}

fn ocr_image_text(image_path: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("tesseract")
        .arg(image_path)
        .arg("stdout")
        .output()
        .map_err(|error| {
            anyhow::anyhow!(
                "OCR needs the `tesseract` binary on the host (install it with \
                 `sudo apt install -y tesseract-ocr`): {error}"
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "tesseract failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[derive(Debug, Deserialize)]
struct TelegramGetFileResponse {
    ok: bool,
    result: Option<TelegramFilePath>,
}

#[derive(Debug, Deserialize)]
struct TelegramFilePath {
    file_path: Option<String>,
}

fn download_telegram_document(token: &str, file_id: &str, dest: &Path) -> anyhow::Result<u64> {
    let bytes = download_telegram_file_bytes_with_label(
        token,
        file_id,
        MAX_TELEGRAM_DOCUMENT_BYTES,
        "document",
    )?;
    fs::write(dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;
    Ok(bytes.len() as u64)
}

pub(crate) fn download_telegram_file_bytes(
    token: &str,
    file_id: &str,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    download_telegram_file_bytes_with_label(token, file_id, max_bytes, "file")
}

fn download_telegram_file_bytes_with_label(
    token: &str,
    file_id: &str,
    max_bytes: u64,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    let response: TelegramGetFileResponse = ureq::get(&format!(
        "https://api.telegram.org/bot{token}/getFile?file_id={file_id}"
    ))
    .call()
    .context("Telegram getFile failed")?
    .into_json()
    .context("failed to parse Telegram getFile response")?;
    if !response.ok {
        anyhow::bail!("Telegram getFile returned ok=false");
    }
    let file_path = response
        .result
        .and_then(|result| result.file_path)
        .context("Telegram getFile returned no file_path")?;
    let reader = ureq::get(&format!(
        "https://api.telegram.org/file/bot{token}/{file_path}"
    ))
    .call()
    .context("Telegram file download failed")?
    .into_reader();
    let mut bytes = Vec::new();
    reader.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("{label} exceeds {max_bytes} bytes");
    }
    Ok(bytes)
}

/// Keep only filesystem-safe filename characters; Telegram/Discord file names
/// are attacker-controlled input that can end up in a path under the agent inbox.
pub(crate) fn sanitize_document_name(name: Option<&str>) -> String {
    let cleaned = name
        .unwrap_or("document")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(|ch: char| ch == '.' || ch.is_whitespace())
        .to_string();
    if cleaned.is_empty() {
        "document".to_string()
    } else {
        cleaned
    }
}

/// The agent's `knowledge_graph` opt-in, read from its materialized spec.
/// Missing/unparseable spec means disabled (the default).
pub(crate) fn agent_knowledge_graph(
    home: &MaturanaHome,
    agent_id: &str,
) -> maturana_core::spec::KnowledgeGraph {
    maturana_core::spec::AgentSpec::from_maturana_markdown(
        &home.agent_dir(agent_id).join("MATURANA.md"),
    )
    .ok()
    .map(|spec| spec.knowledge_graph)
    .unwrap_or_default()
}
