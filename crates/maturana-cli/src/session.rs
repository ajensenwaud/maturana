use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use maturana_core::{
    session_db::{
        claim_pending_inbound, ensure_session, insert_inbound, list_undelivered, mark_delivered,
        mark_inbound_completed, session_paths, write_outbound, SessionPaths,
    },
    state::MaturanaHome,
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
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
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long, default_value = "/workspace")]
        guest_workspace: String,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
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
            ip,
            ssh_user,
            ssh_key,
            guest_workspace,
            timeout_seconds,
        } => {
            let paths = session_paths(&home.agent_dir(&agent_id), &session_id);
            ensure_session(&paths)?;
            let options = RunnerOptions {
                provider,
                ip,
                ssh_user,
                ssh_key,
                guest_workspace,
                timeout_seconds,
            };
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

fn serve_sessiond(home: &MaturanaHome, bind: &str, token: Option<&str>) -> anyhow::Result<()> {
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
            if actual != expected {
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
            let paths = session_paths(&home.agent_dir(&body.agent_id), &body.session_id);
            ensure_session(&paths)?;
            mark_inbound_completed(&paths, &body.message_ids)?;
            write_json_response(stream, 200, &serde_json::json!({ "ok": true }))
        }
        ("POST", "/session/outbound") => {
            let body: OutboundRequest = serde_json::from_slice(&request.body)?;
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
            write_json_response(stream, 200, &serde_json::json!({ "ok": true, "id": id }))
        }
        ("POST", "/session/heartbeat") => {
            let body: HeartbeatRequest = serde_json::from_slice(&request.body)?;
            write_worker_status(home, &body)?;
            write_json_response(stream, 200, &serde_json::json!({ "ok": true }))
        }
        _ => write_json_response(
            stream,
            404,
            &serde_json::json!({ "ok": false, "error": "not found" }),
        ),
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
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

fn write_json_response(
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
    pub ip: Option<String>,
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    pub guest_workspace: String,
    pub timeout_seconds: u64,
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
            "codex-ssh" => {
                let prompt = message_prompt(&message.content)?;
                run_codex_over_ssh(options, &prompt)?
            }
            other => anyhow::bail!("unsupported session provider for MVP: {other}"),
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

fn message_prompt(content: &str) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .with_context(|| format!("invalid message json: {content}"))?;
    Ok(value
        .get("prompt")
        .or_else(|| value.get("text"))
        .and_then(|text| text.as_str())
        .unwrap_or(content)
        .to_string())
}

fn run_codex_over_ssh(options: &RunnerOptions, prompt: &str) -> anyhow::Result<String> {
    let ip = options
        .ip
        .as_deref()
        .context("codex-ssh provider requires --ip")?;
    let prompt_path = "/tmp/maturana-session-prompt.txt";
    let output_path = "/tmp/maturana-session-response.txt";
    let stderr_path = "/tmp/maturana-session-codex.err";
    let remote = format!(
        "set -eu; cat > {prompt_path}; rm -f {output_path} {stderr_path}; codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C {} -o {output_path} \"$(cat {prompt_path})\" >/tmp/maturana-session-codex.out 2>{stderr_path}; cat {output_path}",
        shell_quote(&options.guest_workspace)
    );
    run_ssh_with_stdin(
        ip,
        &options.ssh_user,
        &options.ssh_key,
        &remote,
        Some(prompt),
        options.timeout_seconds,
    )
}

fn run_ssh_with_stdin(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    remote_command: &str,
    stdin_text: Option<&str>,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    let mut command = ProcessCommand::new("ssh");
    command
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PreferredAuthentications=publickey")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", null_known_hosts()))
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key)
        .arg(format!("{ssh_user}@{ip}"))
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_text.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command.spawn().context("failed to start ssh")?;
    if let Some(stdin_text) = stdin_text {
        let mut stdin = child.stdin.take().context("failed to open ssh stdin")?;
        stdin.write_all(stdin_text.as_bytes())?;
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ssh timed out after {} seconds", timeout_seconds.max(1));
        }
        thread::sleep(Duration::from_millis(100));
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!("ssh failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn null_known_hosts() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_prompt_prefers_prebuilt_prompt() {
        let content = serde_json::json!({
            "text": "hello",
            "prompt": "full context prompt"
        })
        .to_string();
        assert_eq!(message_prompt(&content).unwrap(), "full context prompt");
    }

    #[test]
    fn message_prompt_falls_back_to_text() {
        let content = serde_json::json!({ "text": "hello" }).to_string();
        assert_eq!(message_prompt(&content).unwrap(), "hello");
    }
}
