//! Cockpit voice: operator dictation (STT → fills the prompt console) and
//! read-aloud (TTS of a response). Both call OpenAI host-side with the key from
//! `pipelock:openai/api-key`; the key never reaches the browser.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::{blocking, err, ok};
use crate::state::AppState;

const OPENAI_KEY_SOURCE: &str = "pipelock:openai/api-key";

fn openai_key(home: &std::path::Path) -> anyhow::Result<String> {
    maturana_core::secrets::resolve_secret_source_with_home(OPENAI_KEY_SOURCE, home)
        .map(|s| s.expose_for_runtime().to_string())
        .map_err(|_| anyhow::anyhow!("set pipelock:openai/api-key to use cockpit voice"))
}

#[derive(serde::Deserialize)]
pub struct TtsBody {
    text: String,
    #[serde(default = "default_voice")]
    voice: String,
}
fn default_voice() -> String {
    "alloy".to_string()
}

/// Text → speech (mp3 bytes streamed back to the browser for playback).
pub async fn tts(State(state): State<AppState>, Json(body): Json<TtsBody>) -> Response {
    let home = state.home_root.clone();
    let audio = blocking(move || {
        let key = openai_key(&home)?;
        let response = ureq::post("https://api.openai.com/v1/audio/speech")
            .set("authorization", &format!("Bearer {key}"))
            .timeout(std::time::Duration::from_secs(60))
            .send_json(serde_json::json!({
                "model": "tts-1",
                "input": body.text,
                "voice": body.voice,
                "response_format": "mp3",
            }))
            .map_err(|e| anyhow::anyhow!("openai tts failed: {e}"))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)?;
        Ok(bytes)
    })
    .await;
    match audio {
        Ok(bytes) => ([(header::CONTENT_TYPE, "audio/mpeg")], bytes).into_response(),
        Err(response) => response,
    }
}

/// Speech → text. Body is raw audio bytes; the filename header sets the format
/// hint OpenAI needs for the multipart upload.
pub async fn stt(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > 25 * 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "audio exceeds 25 MB");
    }
    let filename = headers
        .get("x-maturana-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio.webm")
        .to_string();
    let home = state.home_root.clone();
    match blocking(move || {
        let key = openai_key(&home)?;
        let (content_type, payload) = multipart_transcription(&filename, &body);
        let response = ureq::post("https://api.openai.com/v1/audio/transcriptions")
            .set("authorization", &format!("Bearer {key}"))
            .set("content-type", &content_type)
            .timeout(std::time::Duration::from_secs(120))
            .send_bytes(&payload)
            .map_err(|e| anyhow::anyhow!("openai stt failed: {e}"))?;
        let json: serde_json::Value = response.into_json()?;
        Ok(serde_json::json!({ "text": json.get("text").and_then(|t| t.as_str()).unwrap_or("") }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Build a minimal multipart/form-data body for the transcription endpoint
/// (model=whisper-1 + the audio file). Returns (content_type, body).
fn multipart_transcription(filename: &str, audio: &[u8]) -> (String, Vec<u8>) {
    let boundary = "maturanavoiceboundary7e3f";
    let mut body = Vec::new();
    let mut part = |headers: &str, data: &[u8], body: &mut Vec<u8>| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(headers.as_bytes());
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    part(
        "content-disposition: form-data; name=\"model\"",
        b"whisper-1",
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
    (
        format!("multipart/form-data; boundary={boundary}"),
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_has_model_and_file_parts() {
        let (ct, body) = multipart_transcription("clip.webm", b"AUDIODATA");
        assert!(ct.contains("multipart/form-data; boundary="));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model\""));
        assert!(text.contains("whisper-1"));
        assert!(text.contains("filename=\"clip.webm\""));
        assert!(text.contains("AUDIODATA"));
        assert!(text.trim_end().ends_with("--"));
    }
}
