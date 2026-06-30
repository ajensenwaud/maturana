//! Agent2Agent (A2A) protocol types — the open standard wire format for
//! agent-to-agent communication (JSON-RPC 2.0 over HTTP, with an Agent Card for
//! discovery).
//!
//! Maturana speaks A2A for ALL agent-to-agent traffic:
//!   * the master orchestrator sends a step to a worker agent as an A2A
//!     `message/send`, and
//!   * one agent delegates to a peer agent in-band the same way.
//!
//! This module is just the wire types + small helpers (pure, serde, fully
//! testable). The host A2A server and the client live in the cli crate, and both
//! reuse these. Field names follow the A2A spec: camelCase JSON, a `kind`
//! discriminator on parts/messages/tasks, kebab-case task states.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The A2A protocol version this implementation targets.
pub const PROTOCOL_VERSION: &str = "0.2.5";

/// Path (relative to an agent's A2A base URL) that serves its Agent Card.
pub const AGENT_CARD_PATH: &str = ".well-known/agent-card.json";

/// A2A JSON-RPC method names.
pub mod method {
    /// Send a message to an agent and get back a Task (or a Message).
    pub const MESSAGE_SEND: &str = "message/send";
    /// Poll a previously-created task by id.
    pub const TASKS_GET: &str = "tasks/get";
    /// Request cancellation of a running task.
    pub const TASKS_CANCEL: &str = "tasks/cancel";
}

/// A piece of a message or artifact. `kind` is the discriminator, per spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Part {
    /// Plain text.
    Text { text: String },
    /// Structured JSON (used for machine-readable results, e.g. a plan).
    Data { data: Value },
}

/// One message in an A2A exchange.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// "user" (the sender) or "agent" (the responder).
    pub role: String,
    pub parts: Vec<Part>,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    /// Always the literal "message".
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Message {
    /// A user-role text message with the given id (caller supplies the id so
    /// this stays pure; use [`gen_id`] for a fresh one).
    pub fn user_text(message_id: &str, text: &str) -> Self {
        Self {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: text.to_string(),
            }],
            message_id: message_id.to_string(),
            task_id: None,
            context_id: None,
            kind: "message".to_string(),
            metadata: None,
        }
    }

    /// An agent-role text message (a reply).
    pub fn agent_text(message_id: &str, text: &str) -> Self {
        let mut m = Self::user_text(message_id, text);
        m.role = "agent".to_string();
        m
    }

    /// The concatenation of all text parts (the plain-text content).
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                Part::Data { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Lifecycle state of an A2A task (kebab-case on the wire).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Canceled,
    Failed,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

/// An output produced by an agent for a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub parts: Vec<Part>,
}

/// The unit of work in A2A: a task with a status and (when done) artifacts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// Always the literal "task".
    pub kind: String,
}

impl Task {
    /// A completed task carrying one text artifact (the agent's reply) — the
    /// common case for a synchronous Maturana dispatch.
    pub fn completed_text(id: &str, context_id: &str, text: &str) -> Self {
        Self {
            id: id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: vec![Artifact {
                artifact_id: format!("{id}-result"),
                name: Some("result".to_string()),
                parts: vec![Part::Text {
                    text: text.to_string(),
                }],
            }],
            kind: "task".to_string(),
        }
    }

    /// A failed task carrying the error text.
    pub fn failed(id: &str, context_id: &str, reason: &str) -> Self {
        Self {
            id: id.to_string(),
            context_id: context_id.to_string(),
            status: TaskStatus {
                state: TaskState::Failed,
                message: Some(Message::agent_text(&format!("{id}-err"), reason)),
                timestamp: None,
            },
            artifacts: Vec::new(),
            kind: "task".to_string(),
        }
    }

    /// The text of the first artifact (the agent's answer), if any.
    pub fn result_text(&self) -> Option<String> {
        let artifact = self.artifacts.first()?;
        let text: String = artifact
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                Part::Data { .. } => None,
            })
            .collect();
        Some(text)
    }
}

// ---- Agent Card (discovery) ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

/// The Agent Card an agent publishes for discovery, per the A2A spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub protocol_version: String,
    pub name: String,
    pub description: String,
    /// Base URL where this agent's A2A JSON-RPC endpoint is served.
    pub url: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
}

// ---- JSON-RPC 2.0 envelopes ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

impl JsonRpcRequest {
    pub fn new(id: Value, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Value, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
        }
    }
}

/// Params for `message/send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// A fresh random id (for message/task ids).
pub fn gen_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_send_request_matches_the_spec_shape() {
        let msg = Message::user_text("m-1", "do the thing");
        let req = JsonRpcRequest::new(
            serde_json::json!(1),
            method::MESSAGE_SEND,
            serde_json::to_value(SendMessageParams {
                message: msg,
                metadata: None,
            })
            .unwrap(),
        );
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "message/send");
        // The message is camelCase with the kind discriminators the spec requires.
        let m = &v["params"]["message"];
        assert_eq!(m["role"], "user");
        assert_eq!(m["kind"], "message");
        assert_eq!(m["messageId"], "m-1");
        assert_eq!(m["parts"][0]["kind"], "text");
        assert_eq!(m["parts"][0]["text"], "do the thing");
    }

    #[test]
    fn task_round_trips_and_extracts_result_text() {
        let task = Task::completed_text("t-1", "ctx-1", "the answer");
        let v = serde_json::to_value(&task).unwrap();
        assert_eq!(v["kind"], "task");
        assert_eq!(v["status"]["state"], "completed");
        assert_eq!(v["contextId"], "ctx-1");
        assert_eq!(v["artifacts"][0]["parts"][0]["text"], "the answer");

        let back: Task = serde_json::from_value(v).unwrap();
        assert_eq!(back, task);
        assert_eq!(back.result_text().as_deref(), Some("the answer"));
    }

    #[test]
    fn task_state_is_kebab_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).unwrap(),
            "input-required"
        );
        let s: TaskState = serde_json::from_value(serde_json::json!("working")).unwrap();
        assert_eq!(s, TaskState::Working);
    }

    #[test]
    fn agent_card_serializes_camelcase_with_protocol_version() {
        let card = AgentCard {
            protocol_version: PROTOCOL_VERSION.to_string(),
            name: "codex-firecracker".to_string(),
            description: "A coding worker.".to_string(),
            url: "http://127.0.0.1:47837/a2a/codex-firecracker".to_string(),
            version: "0.1.0".to_string(),
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "develop".to_string(),
                name: "develop".to_string(),
                description: "writes code".to_string(),
                tags: vec!["code".to_string()],
                examples: vec![],
            }],
        };
        let v = serde_json::to_value(&card).unwrap();
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["defaultInputModes"][0], "text/plain");
        assert_eq!(v["skills"][0]["id"], "develop");
        // Round-trips.
        let back: AgentCard = serde_json::from_value(v).unwrap();
        assert_eq!(back, card);
    }

    #[test]
    fn jsonrpc_response_carries_result_or_error_not_both() {
        let ok = JsonRpcResponse::ok(serde_json::json!(7), serde_json::json!({"x": 1}));
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["id"], 7);
        assert!(v.get("error").is_none(), "ok response omits error");
        assert_eq!(v["result"]["x"], 1);

        let err = JsonRpcResponse::err(serde_json::json!("a"), -32601, "method not found");
        let v = serde_json::to_value(&err).unwrap();
        assert!(v.get("result").is_none(), "error response omits result");
        assert_eq!(v["error"]["code"], -32601);
    }
}
