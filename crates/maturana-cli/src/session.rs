use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use maturana_core::{
    improvement::TrajectoryStore,
    session_db::{
        append_progress, claim_pending_inbound, ensure_session, insert_inbound,
        list_recent_inbound, list_undelivered, mark_delivered, mark_inbound_completed,
        session_paths, write_outbound, ProgressEvent, SessionPaths,
    },
    state::MaturanaHome,
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

#[derive(Debug, Args)]
pub struct SessionCommand {
    #[command(subcommand)]
    pub command: SessionSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionSubcommand {
    Init {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
    },
    Enqueue {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
        #[arg(long, default_value = "telegram")]
        channel: String,
        #[arg(long)]
        platform_id: String,
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        text: String,
    },
    RunOnce {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
        #[arg(long, default_value = "echo")]
        provider: String,
    },
    Outbox {
        agent_id: String,
        #[arg(long, default_value = "default")]
        session_id: String,
        #[arg(long)]
        mark_delivered: bool,
    },
    Serve {
        #[arg(long, default_value = "0.0.0.0:47834")]
        bind: String,
        #[arg(long, env = "MATURANA_SESSIOND_TOKEN")]
        token: Option<String>,
    },
}

pub fn handle_session(command: SessionCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        SessionSubcommand::Init {
            agent_id,
            session_id,
        } => {
            let paths = session_paths(&home.agent_dir(&agent_id), &session_id);
            ensure_session(&paths)?;
            println!("session initialized: {}", paths.dir.display());
        }
        SessionSubcommand::Enqueue {
            agent_id,
            session_id,
            channel,
            platform_id,
            thread_id,
            text,
        } => {
            let paths = session_paths(&home.agent_dir(&agent_id), &session_id);
            ensure_session(&paths)?;
            let content = serde_json::json!({ "text": text }).to_string();
            let id = insert_inbound(
                &paths,
                "chat",
                &channel,
                &platform_id,
                thread_id.as_deref(),
                &content,
            )?;
            println!("enqueued: {id}");
        }
        SessionSubcommand::RunOnce {
            agent_id,
            session_id,
            provider,
        } => {
            let paths = session_paths(&home.agent_dir(&agent_id), &session_id);
            ensure_session(&paths)?;
            let options = RunnerOptions { provider };
            let processed = run_session_once(&paths, &options, 20)?;
            if processed == 0 {
                println!("no pending messages");
            } else {
                println!("processed: {processed}");
            }
        }
        SessionSubcommand::Outbox {
            agent_id,
            session_id,
            mark_delivered: should_mark_delivered,
        } => {
            let paths = session_paths(&home.agent_dir(&agent_id), &session_id);
            ensure_session(&paths)?;
            let messages = list_undelivered(&paths)?;
            for message in &messages {
                println!(
                    "{} {} {}",
                    message.id,
                    message.channel,
                    message_text(&message.content)?
                );
                if should_mark_delivered {
                    mark_delivered(&paths, &message.id, None)?;
                }
            }
            if messages.is_empty() {
                println!("outbox empty");
            }
        }
        SessionSubcommand::Serve { bind, token } => serve_sessiond(home, &bind, token.as_deref())?,
    }
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct ClaimRequest {
    agent_id: String,
    session_id: String,
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct CompleteRequest {
    agent_id: String,
    session_id: String,
    message_ids: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct OutboundRequest {
    agent_id: String,
    session_id: String,
    in_reply_to: Option<String>,
    kind: String,
    channel: String,
    platform_id: String,
    thread_id: Option<String>,
    content: String,
}

#[derive(Debug, serde::Deserialize)]
struct HeartbeatRequest {
    agent_id: String,
    session_id: String,
    status: String,
    message_id: Option<String>,
    error: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ProgressRequest {
    agent_id: String,
    session_id: String,
    message_id: String,
    seq: u64,
    kind: String,
    text: String,
}

/// A guest agent asking the host to build and run a capability it just authored
/// (self-mutation). Gated on the agent's `self_forge` capability.
#[derive(Debug, serde::Deserialize)]
struct ForgeRequest {
    agent_id: String,
    session_id: String,
    /// The in-flight turn's message id; when present, forge progress is streamed
    /// onto that turn's lane so the channel animates it.
    #[serde(default)]
    message_id: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    /// "wat" (default) or "wasm" (base64-encoded module).
    #[serde(default)]
    format: Option<String>,
    source: String,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    capabilities: Option<maturana_core::tools::Capabilities>,
    #[serde(default)]
    limits: Option<maturana_core::tools::ResourceLimits>,
}

fn serve_sessiond(home: &MaturanaHome, bind: &str, token: Option<&str>) -> anyhow::Result<()> {
    // sessiond binds 0.0.0.0 by design (guest VMs reach it), so the token is the
    // only thing standing between any guest/LAN host and every agent's queue.
    // Refuse to start without one rather than silently serving unauthenticated.
    let token = match token {
        Some(token) if !token.is_empty() => Some(token),
        _ => anyhow::bail!(
            "sessiond requires a token; pass --token or set MATURANA_SESSIOND_TOKEN (it binds a public interface)"
        ),
    };
    let listener = TcpListener::bind(bind).with_context(|| format!("failed to bind {bind}"))?;
    println!("maturana sessiond listening on {bind}");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_sessiond_request(home, token, &mut stream) {
                    let _ = write_json_response(
                        &mut stream,
                        500,
                        &serde_json::json!({ "ok": false, "error": error.to_string() }),
                    );
                }
            }
            Err(error) => eprintln!("sessiond accept error: {error}"),
        }
    }
    Ok(())
}

fn handle_sessiond_request(
    home: &MaturanaHome,
    token: Option<&str>,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    let request = read_http_request(stream)?;
    if request.path != "/health" {
        if let Some(expected) = token {
            let actual = request
                .headers
                .get("x-maturana-session-token")
                .map(String::as_str)
                .unwrap_or("");
            if !constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
                return write_json_response(
                    stream,
                    401,
                    &serde_json::json!({ "ok": false, "error": "unauthorized" }),
                );
            }
        }
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => write_json_response(stream, 200, &serde_json::json!({ "ok": true })),
        ("POST", "/session/claim") => {
            let body: ClaimRequest = serde_json::from_slice(&request.body)?;
            if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
                return write_json_response(stream, 400, &error);
            }
            let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
            ensure_session(&paths)?;
            let messages = claim_pending_inbound(&paths, body.limit.unwrap_or(1).clamp(1, 20))?;
            write_json_response(
                stream,
                200,
                &serde_json::json!({ "ok": true, "messages": messages }),
            )
        }
        ("POST", "/session/complete") => {
            let body: CompleteRequest = serde_json::from_slice(&request.body)?;
            if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
                return write_json_response(stream, 400, &error);
            }
            let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
            ensure_session(&paths)?;
            mark_inbound_completed(&paths, &body.message_ids)?;
            write_json_response(stream, 200, &serde_json::json!({ "ok": true }))
        }
        ("POST", "/session/outbound") => {
            let body: OutboundRequest = serde_json::from_slice(&request.body)?;
            if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
                return write_json_response(stream, 400, &error);
            }
            let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
            ensure_session(&paths)?;
            let id = write_outbound(
                &paths,
                body.in_reply_to.as_deref(),
                &body.kind,
                &body.channel,
                &body.platform_id,
                body.thread_id.as_deref(),
                &body.content,
            )?;
            // Record every chat turn as a self-improvement trajectory so /good
            // /bad can reward it and high-reward turns can feed back into
            // context. Harness-agnostic: this is the one seam every guest
            // worker posts its reply through. Best-effort — never fail the
            // reply because the trajectory store hiccupped.
            if body.kind == "chat" {
                if let Err(error) = record_chat_trajectory(home, &paths, &body) {
                    eprintln!("trajectory record failed: {error:#}");
                }
            }
            write_json_response(stream, 200, &serde_json::json!({ "ok": true, "id": id }))
        }
        ("POST", "/session/heartbeat") => {
            let body: HeartbeatRequest = serde_json::from_slice(&request.body)?;
            if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
                return write_json_response(stream, 400, &error);
            }
            write_worker_status(home, &body)?;
            write_json_response(stream, 200, &serde_json::json!({ "ok": true }))
        }
        // Live turn progress: the guest worker streams distilled events here as
        // the harness works. Written to a per-message side-lane (NOT the outbound
        // queue), so delivery and `agent run --wait` are unaffected; channels
        // tail it to show tool calls / streamed text before the final reply.
        ("POST", "/session/progress") => {
            let body: ProgressRequest = serde_json::from_slice(&request.body)?;
            if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
                return write_json_response(stream, 400, &error);
            }
            let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
            ensure_session(&paths)?;
            append_progress(
                &paths,
                &body.message_id,
                &ProgressEvent {
                    seq: body.seq,
                    kind: body.kind,
                    text: body.text,
                },
            )?;
            write_json_response(stream, 200, &serde_json::json!({ "ok": true }))
        }
        // Self-mutation: a guest agent forges a capability (WAT/wasm) and runs it
        // live. Capability-gated; progress is streamed so the channel animates.
        ("POST", "/session/forge") => handle_forge(home, &request, stream),
        _ => write_json_response(
            stream,
            404,
            &serde_json::json!({ "ok": false, "error": "not found" }),
        ),
    }
}

/// Build + run a capability an agent authored on the fly. Validates the agent is
/// allowed to self-forge, then delegates to the engine (feature-gated).
fn handle_forge(
    home: &MaturanaHome,
    request: &HttpRequest,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    let body: ForgeRequest = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(error) => {
            return write_json_response(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("invalid forge request: {error}") }),
            )
        }
    };
    if let Err(error) = check_identifiers(&body.agent_id, &body.session_id) {
        return write_json_response(stream, 400, &error);
    }
    // Capability gate: only an agent explicitly granted self_forge may build and
    // run new code. Default-deny — an unknown/unparseable spec is treated as no.
    let granted = maturana_core::spec::AgentSpec::from_maturana_markdown(
        home.agent_dir(&body.agent_id).join("MATURANA.md"),
    )
    .map(|spec| spec.capabilities.self_forge)
    .unwrap_or(false);
    if !granted {
        return write_json_response(
            stream,
            403,
            &serde_json::json!({
                "ok": false,
                "error": "self_forge capability not granted (set capabilities.self_forge: true in MATURANA.md)"
            }),
        );
    }
    forge_impl(home, &body, stream)
}

#[cfg(feature = "wasm-runtime")]
fn forge_impl(
    home: &MaturanaHome,
    body: &ForgeRequest,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    use maturana_core::tools::{forge, ToolRegistry};

    let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
    ensure_session(&paths)?;
    let name = body.name.trim().to_string();
    let base_seq = Utc::now().timestamp_millis().max(0) as u64;
    // Stream forge progress onto the active turn's lane so the channel's live
    // progress animation surfaces the self-mutation as it happens.
    let emit = |offset: u64, kind: &str, text: String| {
        if let Some(message_id) = body.message_id.as_deref() {
            let _ = append_progress(
                &paths,
                message_id,
                &ProgressEvent {
                    seq: base_seq + offset,
                    kind: kind.to_string(),
                    text,
                },
            );
        }
    };

    let format = match forge::ForgeFormat::parse(body.format.as_deref().unwrap_or("wat")) {
        Ok(format) => format,
        Err(error) => {
            return write_json_response(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": error.to_string() }),
            )
        }
    };

    emit(0, "forge.building", format!("🔨 Building `{name}` — assembling WebAssembly…"));
    let registry = ToolRegistry::new(home.agent_dir(&body.agent_id).join("forge"));
    let spec = forge::ForgeSpec {
        name: &name,
        description: body.description.as_deref().unwrap_or("forged on the fly"),
        format,
        source: &body.source,
        input: body.input.as_deref().unwrap_or("{}"),
        capabilities: body.capabilities.clone().unwrap_or_default(),
        limits: body.limits.clone().unwrap_or_default(),
    };
    emit(1, "forge.running", format!("⚙️ Running forged `{name}`…"));

    match forge::forge_and_run(&registry, spec) {
        Ok(outcome) => {
            emit(
                2,
                "forge.done",
                format!(
                    "✅ Forged `{}` ({} bytes, {} ms)",
                    name, outcome.bytes, outcome.run.duration_ms
                ),
            );
            write_json_response(
                stream,
                200,
                &serde_json::json!({
                    "ok": outcome.run.ok,
                    "name": outcome.name,
                    "bytes": outcome.bytes,
                    "stdout": outcome.run.stdout,
                    "stderr": outcome.run.stderr,
                    "fuel_used": outcome.run.fuel_used,
                    "duration_ms": outcome.run.duration_ms,
                }),
            )
        }
        Err(error) => {
            emit(2, "forge.failed", format!("❌ Forge `{name}` failed"));
            write_json_response(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("{error:#}") }),
            )
        }
    }
}

#[cfg(not(feature = "wasm-runtime"))]
fn forge_impl(
    _home: &MaturanaHome,
    _body: &ForgeRequest,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    write_json_response(
        stream,
        501,
        &serde_json::json!({
            "ok": false,
            "error": "wasm forge engine not built into this binary (build with --features wasm-runtime)"
        }),
    )
}

pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            anyhow::bail!("connection closed while reading request");
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_header_end(&data) {
            header_end = index;
            break;
        }
        if data.len() > 1024 * 1024 {
            anyhow::bail!("request headers too large");
        }
    }

    let headers_raw = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers_raw.split("\r\n");
    let request_line = lines.next().context("missing request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let path = request_parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        anyhow::bail!("request body too large");
    }
    let body_start = header_end + 4;
    while data.len() < body_start + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    let body = data
        .get(body_start..body_start + content_length)
        .unwrap_or_default()
        .to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Upper bound on a request body. sessiond payloads are small JSON control
/// messages; this caps a malicious `Content-Length` from exhausting host memory
/// on the public listener.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// `agent_id` and `session_id` arrive in attacker-controllable request bodies
/// and are joined into host filesystem paths (`agents/<id>/sessions/<id>/...`,
/// `worker-status.json`). Without this they allow path traversal / arbitrary
/// file writes (e.g. `../../..`, an absolute path, or a Windows drive prefix).
fn check_identifiers(agent_id: &str, session_id: &str) -> Result<(), serde_json::Value> {
    for (label, value) in [("agent_id", agent_id), ("session_id", session_id)] {
        if !valid_identifier(value) {
            return Err(serde_json::json!({
                "ok": false,
                "error": format!("invalid {label}"),
            }));
        }
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !value.contains("..")
}

/// Length-independent byte comparison to keep the session-token check from
/// leaking the token through response timing on the public listener.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let body = serde_json::to_vec(value)?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

/// Record a completed chat turn (user input → agent reply) as a trajectory.
/// The input is recovered from the inbound the reply answers; the output is the
/// reply text. Missing input is tolerated (recorded empty) rather than dropped.
fn record_chat_trajectory(
    home: &MaturanaHome,
    paths: &SessionPaths,
    body: &OutboundRequest,
) -> anyhow::Result<()> {
    let input = match body.in_reply_to.as_deref() {
        Some(reply_to) => list_recent_inbound(paths, 50)?
            .into_iter()
            .find(|m| m.id == reply_to)
            .and_then(|m| message_text(&m.content).ok())
            .unwrap_or_default(),
        None => String::new(),
    };
    let output = message_text(&body.content).unwrap_or_default();
    let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
    store.record(&body.agent_id, &body.session_id, "chat", &input, &output, "[]")?;
    Ok(())
}

fn write_worker_status(home: &MaturanaHome, body: &HeartbeatRequest) -> anyhow::Result<()> {
    let path = home.agent_dir(&body.agent_id).join("worker-status.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({
            "agent_id": body.agent_id,
            "session_id": body.session_id,
            "status": body.status,
            "message_id": body.message_id,
            "error": body.error,
            "at": Utc::now(),
        }))?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

#[derive(Debug, Clone)]
pub struct RunnerOptions {
    pub provider: String,
}

pub fn run_session_once(
    paths: &SessionPaths,
    options: &RunnerOptions,
    limit: usize,
) -> anyhow::Result<usize> {
    ensure_session(paths)?;
    let messages = claim_pending_inbound(paths, limit)?;
    let ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    for message in &messages {
        let text = message_text(&message.content)?;
        let response = match options.provider.as_str() {
            "echo" => format!("echo: {text}"),
            other => anyhow::bail!(
                "unsupported local session provider: {other}; use echo for smoke tests or run the guest worker for real harness turns"
            ),
        };
        write_outbound(
            paths,
            Some(&message.id),
            "chat",
            &message.channel,
            &message.platform_id,
            message.thread_id.as_deref(),
            &serde_json::json!({ "text": response }).to_string(),
        )?;
    }
    mark_inbound_completed(paths, &ids)?;
    Ok(ids.len())
}

pub fn message_text(content: &str) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .with_context(|| format!("invalid message json: {content}"))?;
    Ok(value
        .get("text")
        .and_then(|text| text.as_str())
        .unwrap_or(content)
        .to_string())
}

/// Host-side file paths to attach to an outbound message (`{"files":[...]}`). The
/// channel's delivery sink uploads them where the channel supports it (Telegram
/// sendDocument) and otherwise names them in the text. Empty when there's no
/// `files` array, so the ordinary text path is unaffected.
pub fn message_files(content: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| {
            value.get("files").and_then(|f| f.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_once_rejects_host_side_codex_ssh_provider() {
        let root = std::env::temp_dir().join(format!(
            "maturana-session-provider-{}",
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let paths = session_paths(&root, "telegram-main");
        ensure_session(&paths).unwrap();
        insert_inbound(
            &paths,
            "chat",
            "telegram",
            "chat-1",
            None,
            &serde_json::json!({ "text": "hello" }).to_string(),
        )
        .unwrap();

        let error = run_session_once(
            &paths,
            &RunnerOptions {
                provider: ["codex", "ssh"].join("-"),
            },
            1,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported local session provider"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn identifiers_reject_path_traversal() {
        assert!(valid_identifier("opencode-demo"));
        assert!(valid_identifier("telegram-main"));
        assert!(valid_identifier("codex-main"));
        for bad in [
            "..",
            "../../etc",
            "a/../b",
            "a/b",
            "a\\b",
            "/abs",
            "C:\\x",
            "",
        ] {
            assert!(!valid_identifier(bad), "should reject {bad:?}");
        }
        assert!(check_identifiers("../../x", "ok").is_err());
        assert!(check_identifiers("ok", "../../x").is_err());
        assert!(check_identifiers("opencode-demo", "opencode-main").is_ok());
    }

    #[test]
    fn constant_time_eq_matches_only_equal_slices() {
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"tok"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
