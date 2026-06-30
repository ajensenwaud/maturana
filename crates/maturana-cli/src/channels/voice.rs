use std::{io::Read, time::Duration};

use maturana_core::{secrets::resolve_secret_source_with_home, state::MaturanaHome};

use crate::channels::{
    audit_channel_event, command_handler::truncate_inline, download_telegram_file_bytes,
    load_channel_settings, run_channel_prompt, send_telegram, send_telegram_chat_action,
    TelegramServe,
};

const MAX_TELEGRAM_AUDIO_BYTES: u64 = 19 * 1024 * 1024;

/// A voice note or audio file: download it, transcribe it host-side (STT), echo
/// the transcript so the user can confirm it was heard correctly, then run the
/// transcript through the same channel-prompt pipeline a typed message uses.
pub(super) fn handle_telegram_voice(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    file_id: &str,
    filename: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    let audio = match download_telegram_file_bytes(token, file_id, MAX_TELEGRAM_AUDIO_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!("I couldn't download that voice message: {error:#}"),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    let settings = load_channel_settings(home, &config.agent_id);
    let provider = stt_provider(home, settings.tts_provider.as_deref());
    let transcript = match transcribe_speech(home, &provider, &audio, filename) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        Ok(_) => {
            send_telegram(
                token,
                &chat_id.to_string(),
                "I heard a voice message but couldn't make out any words. Try again, or send text.",
                reply_to_message_id,
            )?;
            return Ok(());
        }
        Err(error) => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.voice_error",
                &format!("STT failed via {provider}: {error:#}"),
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &format!(
                    "I couldn't transcribe that voice message (provider: {provider}): {error:#}\n\nSet a transcription key in pipelock (e.g. `pipelock:elevenlabs/api-key`)."
                ),
                reply_to_message_id,
            )?;
            return Ok(());
        }
    };
    audit_channel_event(
        home,
        &config.agent_id,
        "channel.telegram.voice",
        &format!(
            "transcribed {} chars via {provider}",
            transcript.chars().count()
        ),
    )?;
    send_telegram(
        token,
        &chat_id.to_string(),
        &format!("🎙️ \"{}\"", truncate_inline(&transcript, 400)),
        reply_to_message_id,
    )?;
    run_channel_prompt(
        home,
        token,
        config,
        chat_id,
        &transcript,
        reply_to_message_id,
    )
}

/// Read-aloud for channels: when /tts is enabled, synthesize the reply with the
/// selected provider and send it as an audio message after the text. Always
/// best-effort.
pub(super) fn maybe_send_tts(
    home: &MaturanaHome,
    token: &str,
    agent_id: &str,
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
) {
    let settings = load_channel_settings(home, agent_id);
    if !settings.tts_enabled {
        return;
    }
    let spoken = text.trim();
    if spoken.is_empty() || spoken == crate::proactive::SILENCE_SENTINEL {
        return;
    }
    let spoken: String = spoken.chars().take(4000).collect();
    let provider = settings.tts_provider.as_deref().unwrap_or("openai");
    match synthesize_speech(home, provider, &spoken) {
        Ok(audio) => {
            if let Err(error) = send_telegram_audio(token, chat_id, &audio, reply_to) {
                eprintln!("telegram tts send failed (text already delivered): {error:#}");
            }
        }
        Err(error) => {
            eprintln!("tts synthesis failed via {provider} (text already delivered): {error:#}");
        }
    }
}

fn synthesize_speech(home: &MaturanaHome, provider: &str, text: &str) -> anyhow::Result<Vec<u8>> {
    let resolve = |source: &str| -> anyhow::Result<String> {
        Ok(resolve_secret_source_with_home(source, home.root())?
            .expose_for_runtime()
            .to_string())
    };
    let request = match provider.to_ascii_lowercase().as_str() {
        "elevenlabs" => {
            let key = resolve("pipelock:elevenlabs/api-key")?;
            ureq::post("https://api.elevenlabs.io/v1/text-to-speech/21m00Tcm4TlvDq8ikWAM")
                .set("xi-api-key", &key)
                .set("accept", "audio/mpeg")
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({
                    "text": text,
                    "model_id": "eleven_multilingual_v2",
                }))
        }
        "deepgram" => {
            let key = resolve("pipelock:deepgram/api-key")?;
            ureq::post("https://api.deepgram.com/v1/speak?model=aura-asteria-en")
                .set("authorization", &format!("Token {key}"))
                .set("content-type", "application/json")
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({ "text": text }))
        }
        _ => {
            let key = resolve("pipelock:openai/api-key")?;
            ureq::post("https://api.openai.com/v1/audio/speech")
                .set("authorization", &format!("Bearer {key}"))
                .timeout(Duration::from_secs(60))
                .send_json(serde_json::json!({
                    "model": "tts-1",
                    "input": text,
                    "voice": "alloy",
                    "response_format": "mp3",
                }))
        }
    };
    let response = request.map_err(|e| anyhow::anyhow!("{provider} tts request failed: {e}"))?;
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        anyhow::bail!("{provider} tts returned no audio");
    }
    Ok(bytes)
}

fn stt_provider(home: &MaturanaHome, tts_provider: Option<&str>) -> String {
    let configured = |provider: &str| -> bool {
        let source = match provider {
            "elevenlabs" => "pipelock:elevenlabs/api-key",
            "openai" => "pipelock:openai/api-key",
            "deepgram" => "pipelock:deepgram/api-key",
            _ => return false,
        };
        resolve_secret_source_with_home(source, home.root()).is_ok()
    };
    if let Some(preferred) = tts_provider {
        let preferred = preferred.to_ascii_lowercase();
        if configured(&preferred) {
            return preferred;
        }
    }
    for provider in ["elevenlabs", "openai", "deepgram"] {
        if configured(provider) {
            return provider.to_string();
        }
    }
    "elevenlabs".to_string()
}

fn transcribe_speech(
    home: &MaturanaHome,
    provider: &str,
    audio: &[u8],
    filename: &str,
) -> anyhow::Result<String> {
    let resolve = |source: &str| -> anyhow::Result<String> {
        Ok(resolve_secret_source_with_home(source, home.root())?
            .expose_for_runtime()
            .to_string())
    };
    match provider.to_ascii_lowercase().as_str() {
        "openai" => {
            let key = resolve("pipelock:openai/api-key")?;
            let (content_type, payload) = multipart_audio("model", "whisper-1", filename, audio);
            let response = ureq::post("https://api.openai.com/v1/audio/transcriptions")
                .set("authorization", &format!("Bearer {key}"))
                .set("content-type", &content_type)
                .timeout(Duration::from_secs(120))
                .send_bytes(&payload)
                .map_err(|e| anyhow::anyhow!("openai stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
        "deepgram" => {
            let key = resolve("pipelock:deepgram/api-key")?;
            let response =
                ureq::post("https://api.deepgram.com/v1/listen?model=nova-2&smart_format=true")
                    .set("authorization", &format!("Token {key}"))
                    .set("content-type", "audio/ogg")
                    .timeout(Duration::from_secs(120))
                    .send_bytes(audio)
                    .map_err(|e| anyhow::anyhow!("deepgram stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .pointer("/results/channels/0/alternatives/0/transcript")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
        _ => {
            let key = resolve("pipelock:elevenlabs/api-key")?;
            let (content_type, payload) = multipart_audio("model_id", "scribe_v1", filename, audio);
            let response = ureq::post("https://api.elevenlabs.io/v1/speech-to-text")
                .set("xi-api-key", &key)
                .set("content-type", &content_type)
                .timeout(Duration::from_secs(120))
                .send_bytes(&payload)
                .map_err(|e| anyhow::anyhow!("elevenlabs stt request failed: {e}"))?;
            let json: serde_json::Value = response.into_json()?;
            Ok(json
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string())
        }
    }
}

pub(super) fn multipart_audio(
    model_field: &str,
    model_value: &str,
    filename: &str,
    audio: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "maturanasttboundary7e3f";
    let mut body = Vec::new();
    let part = |headers: &str, data: &[u8], body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(headers.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    part(
        &format!("content-disposition: form-data; name=\"{model_field}\""),
        model_value.as_bytes(),
        &mut body,
    );
    part(
        &format!(
            "content-disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\ncontent-type: application/octet-stream"
        ),
        audio,
        &mut body,
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn send_telegram_audio(
    token: &str,
    chat_id: i64,
    audio: &[u8],
    reply_to: Option<i64>,
) -> anyhow::Result<()> {
    let boundary = "maturanattsboundary7e3f";
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
    if let Some(id) = reply_to {
        field("reply_to_message_id", &id.to_string(), &mut body);
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"content-disposition: form-data; name=\"audio\"; filename=\"reply.mp3\"\r\ncontent-type: audio/mpeg\r\n\r\n",
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let response = ureq::post(&format!("https://api.telegram.org/bot{token}/sendAudio"))
        .set(
            "content-type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(Duration::from_secs(60))
        .send_bytes(&body)
        .map_err(|e| anyhow::anyhow!("telegram sendAudio failed: {e}"))?;
    let _ = response.into_string();
    Ok(())
}
