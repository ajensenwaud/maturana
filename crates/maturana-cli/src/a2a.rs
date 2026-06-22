//! A2A (Agent2Agent) over the wire for Maturana.
//!
//! A threaded HTTP server exposes, for every agent, an Agent Card (discovery)
//! and a JSON-RPC `message/send` endpoint. `message/send` delivers the message
//! to the target agent as a turn (the existing dispatch queue), waits for the
//! reply, and returns a completed A2A Task. The same core ([`a2a_dispatch`]) is
//! used in-process by the master orchestrator and over the wire by an agent
//! delegating to a peer — so both speak A2A. Hard limits (delegation depth,
//! self-dispatch) are enforced here, host-side, where an agent can't bypass them.

use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use maturana_core::a2a::{
    self, AgentCapabilities, AgentCard, AgentSkill, JsonRpcRequest, JsonRpcResponse, Message,
    SendMessageParams, Task,
};
use maturana_core::state::MaturanaHome;

/// Default A2A bind. Distinct from sessiond (47834), graph (47835), web (47836).
const DEFAULT_A2A_BIND: &str = "0.0.0.0:47837";
/// How long `message/send` waits for the target agent's reply.
const A2A_REPLY_TIMEOUT_SECONDS: u64 = 300;
/// Most levels of nested delegation: the orchestrator (depth 0) dispatches a
/// worker (depth 1), which may delegate once more (depth 2) but no deeper. The
/// host refuses past this so an agent can't recurse without bound.
const MAX_A2A_DEPTH: u32 = 2;

#[derive(Debug, Args)]
pub struct A2aCommand {
    #[command(subcommand)]
    pub command: A2aSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum A2aSubcommand {
    /// Serve the A2A endpoints (Agent Cards + message/send) for all agents.
    Serve {
        #[arg(long, default_value = DEFAULT_A2A_BIND)]
        bind: String,
        #[arg(long, env = "MATURANA_SESSIOND_TOKEN")]
        token: Option<String>,
    },
}

pub fn handle_a2a(command: A2aCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        A2aSubcommand::Serve { bind, token } => serve_a2a(home, &bind, token.as_deref()),
    }
}

fn serve_a2a(home: &MaturanaHome, bind: &str, token: Option<&str>) -> anyhow::Result<()> {
    let token = match token {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => anyhow::bail!(
            "a2a serve requires a token (it binds a public interface); pass --token or MATURANA_SESSIOND_TOKEN"
        ),
    };
    let listener =
        TcpListener::bind(bind).map_err(|e| anyhow::anyhow!("failed to bind {bind}: {e}"))?;
    println!("maturana a2a listening on {bind}");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                // Thread-per-connection: a message/send blocks for as long as the
                // target agent takes to reply (~minutes), so it must NEVER freeze
                // the server for other callers.
                let home = MaturanaHome::new(home.root().to_path_buf());
                let token = token.clone();
                std::thread::spawn(move || {
                    if let Err(error) = handle_a2a_connection(&home, &token, &mut stream) {
                        let _ = crate::session::write_json_response(
                            &mut stream,
                            500,
                            &serde_json::json!({ "error": error.to_string() }),
                        );
                    }
                });
            }
            Err(error) => eprintln!("a2a accept error: {error}"),
        }
    }
    Ok(())
}

fn handle_a2a_connection(
    home: &MaturanaHome,
    token: &str,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    let request = crate::session::read_http_request(stream)?;
    // Token required on every call (binds a public interface).
    let actual = request
        .headers
        .get("x-maturana-session-token")
        .map(String::as_str)
        .unwrap_or("");
    if !crate::session::constant_time_eq(actual.as_bytes(), token.as_bytes()) {
        return crate::session::write_json_response(
            stream,
            401,
            &serde_json::json!({ "error": "unauthorized" }),
        );
    }

    let path = request.path.clone();
    // GET /a2a/<agent>/.well-known/agent-card.json
    if request.method == "GET" {
        let suffix = format!("/{}", a2a::AGENT_CARD_PATH);
        if let Some(agent) = path
            .strip_prefix("/a2a/")
            .and_then(|rest| rest.strip_suffix(&suffix))
        {
            let card = build_agent_card(agent);
            return crate::session::write_json_response(stream, 200, &serde_json::to_value(card)?);
        }
        return crate::session::write_json_response(stream, 404, &serde_json::json!({"error":"not found"}));
    }
    // POST /a2a/<agent>  (JSON-RPC)
    if request.method == "POST" {
        if let Some(agent) = path.strip_prefix("/a2a/") {
            let agent = agent.trim_end_matches('/');
            let rpc: JsonRpcRequest = serde_json::from_slice(&request.body)?;
            let response = dispatch_rpc(home, agent, &rpc);
            return crate::session::write_json_response(stream, 200, &serde_json::to_value(response)?);
        }
    }
    crate::session::write_json_response(stream, 404, &serde_json::json!({"error":"not found"}))
}

fn dispatch_rpc(home: &MaturanaHome, agent: &str, rpc: &JsonRpcRequest) -> JsonRpcResponse {
    if rpc.method == a2a::method::MESSAGE_SEND {
        let params: SendMessageParams = match serde_json::from_value(rpc.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::err(rpc.id.clone(), -32602, &format!("invalid params: {e}"))
            }
        };
        match a2a_dispatch(home, agent, &params.message) {
            Ok(task) => {
                JsonRpcResponse::ok(rpc.id.clone(), serde_json::to_value(task).unwrap_or_default())
            }
            Err(e) => JsonRpcResponse::err(rpc.id.clone(), -32000, &format!("{e:#}")),
        }
    } else {
        JsonRpcResponse::err(rpc.id.clone(), -32601, "method not found")
    }
}

/// The core of A2A `message/send`: deliver `message` to `target_agent` as a turn,
/// wait for the reply, and return a completed A2A Task. Enforces the host-side
/// limits (delegation depth, self-dispatch). Shared by the HTTP server and the
/// in-process orchestrator. The message's `metadata` may carry `maturana_depth`
/// (how many delegation hops deep this is) and `maturana_caller` (the caller's
/// agent id); both are used only to refuse unsafe calls, never trusted for more.
pub(crate) fn a2a_dispatch(
    home: &MaturanaHome,
    target_agent: &str,
    message: &Message,
) -> anyhow::Result<Task> {
    let context_id = message.context_id.clone().unwrap_or_else(a2a::gen_id);
    let task_id = a2a::gen_id();

    let depth = message
        .metadata
        .as_ref()
        .and_then(|m| m.get("maturana_depth"))
        .and_then(|d| d.as_u64())
        .unwrap_or(0) as u32;
    if depth >= MAX_A2A_DEPTH {
        return Ok(Task::failed(
            &task_id,
            &context_id,
            &format!("refused: max delegation depth {MAX_A2A_DEPTH} reached"),
        ));
    }
    let caller = message
        .metadata
        .as_ref()
        .and_then(|m| m.get("maturana_caller"))
        .and_then(|c| c.as_str());
    if caller == Some(target_agent) {
        return Ok(Task::failed(
            &task_id,
            &context_id,
            "refused: an agent cannot dispatch to itself (its one-flight worker would deadlock)",
        ));
    }

    let session_id = crate::infer_agent_session_id(home, target_agent)?;
    // A per-role model override travels in metadata so it survives the A2A hop.
    let model = message
        .metadata
        .as_ref()
        .and_then(|m| m.get("maturana_model"))
        .and_then(|m| m.as_str());
    let handle = crate::channels::enqueue_dispatch_turn(
        home,
        target_agent,
        &session_id,
        &context_id,
        &message.text(),
        model,
    )?;
    let deadline = Instant::now() + Duration::from_secs(A2A_REPLY_TIMEOUT_SECONDS);
    while Instant::now() < deadline {
        if let Some(reply) = crate::channels::try_collect_dispatch(home, target_agent, &handle)? {
            return Ok(Task::completed_text(&task_id, &context_id, &reply));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(Task::failed(
        &task_id,
        &context_id,
        &format!("timed out waiting for {target_agent} after {A2A_REPLY_TIMEOUT_SECONDS}s"),
    ))
}

/// Build an agent's Agent Card. `url` is relative (`/a2a/<agent>`) because the
/// caller already knows the host it fetched the card from.
fn build_agent_card(agent: &str) -> AgentCard {
    AgentCard {
        protocol_version: a2a::PROTOCOL_VERSION.to_string(),
        name: agent.to_string(),
        description: format!("Maturana agent '{agent}', reachable over A2A message/send."),
        url: format!("/a2a/{agent}"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: AgentCapabilities::default(),
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![AgentSkill {
            id: "respond".to_string(),
            name: "respond".to_string(),
            description: format!("Run a task as the {agent} agent and return the result."),
            tags: vec!["maturana".to_string()],
            examples: Vec::new(),
        }],
    }
}

/// Start an A2A server bound to loopback on an ephemeral port, served in a
/// background thread, and return its base URL (e.g. `http://127.0.0.1:54321`).
/// The master orchestrator uses this so its worker dispatches go over the real
/// A2A wire without requiring a separately-running server; it dies with the
/// process.
pub(crate) fn start_local_a2a_server(home: &MaturanaHome, token: &str) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let home = MaturanaHome::new(home.root().to_path_buf());
    let token = token.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let home = MaturanaHome::new(home.root().to_path_buf());
            let token = token.clone();
            std::thread::spawn(move || {
                let _ = handle_a2a_connection(&home, &token, &mut stream);
            });
        }
    });
    Ok(format!("http://{addr}"))
}

/// Send an A2A `message/send` to `agent` at `base_url` and return the Task.
/// Blocking (ureq). Used by the orchestrator (over loopback) and any wire caller.
pub(crate) fn a2a_client_send(
    base_url: &str,
    agent: &str,
    token: &str,
    message: Message,
) -> anyhow::Result<Task> {
    let url = format!("{}/a2a/{}", base_url.trim_end_matches('/'), agent);
    let req = JsonRpcRequest::new(
        serde_json::json!(1),
        a2a::method::MESSAGE_SEND,
        serde_json::to_value(SendMessageParams { message, metadata: None })?,
    );
    let resp: JsonRpcResponse = ureq::post(&url)
        .set("x-maturana-session-token", token)
        .timeout(Duration::from_secs(A2A_REPLY_TIMEOUT_SECONDS + 30))
        .send_json(serde_json::to_value(&req)?)?
        .into_json()?;
    if let Some(err) = resp.error {
        anyhow::bail!("a2a error {}: {}", err.code, err.message);
    }
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("a2a response had no result"))?;
    Ok(serde_json::from_value(result)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::state::MaturanaHome;

    fn home() -> MaturanaHome {
        MaturanaHome::new(std::env::temp_dir().join(format!("a2a-test-{}", std::process::id())))
    }

    #[test]
    fn dispatch_refuses_excessive_depth() {
        let mut msg = Message::user_text("m", "do it");
        msg.metadata = Some(serde_json::json!({ "maturana_depth": MAX_A2A_DEPTH }));
        let task = a2a_dispatch(&home(), "some-agent", &msg).unwrap();
        assert_eq!(task.status.state, maturana_core::a2a::TaskState::Failed);
        assert!(task.status.message.unwrap().text().contains("depth"));
    }

    #[test]
    fn dispatch_refuses_self_dispatch() {
        let mut msg = Message::user_text("m", "do it");
        msg.metadata = Some(serde_json::json!({ "maturana_caller": "codex-firecracker" }));
        let task = a2a_dispatch(&home(), "codex-firecracker", &msg).unwrap();
        assert_eq!(task.status.state, maturana_core::a2a::TaskState::Failed);
        assert!(task.status.message.unwrap().text().contains("itself"));
    }

    #[test]
    fn agent_card_has_expected_shape() {
        let card = build_agent_card("codex-firecracker");
        assert_eq!(card.name, "codex-firecracker");
        assert_eq!(card.url, "/a2a/codex-firecracker");
        assert_eq!(card.protocol_version, a2a::PROTOCOL_VERSION);
        assert_eq!(card.skills[0].id, "respond");
    }
}
