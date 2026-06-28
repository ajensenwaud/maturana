//! Cockpit WebSocket protocol v1.
//!
//! One socket per client; all topics are multiplexed over it (the user
//! explicitly chose WebSockets over SSE). Messages are internally-tagged JSON.
//! `ServerMsg::Hello { v }` is the version gate — the client refuses a major
//! it doesn't speak.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    Codex,
    Openrouter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Agents,
    Runtime,
    Sessions,
    Graph,
    Pipelock,
    Tools,
    Skills,
    /// Live pipelock proxy audit feed (allowed/denied egress).
    Egress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Ping {
        id: u64,
    },
    PromptSubmit {
        turn_id: String,
        harness: HarnessKind,
        #[serde(default)]
        model: Option<String>,
        text: String,
    },
    PromptCancel {
        turn_id: String,
    },
    Subscribe {
        topics: Vec<Topic>,
    },
    Unsubscribe {
        topics: Vec<Topic>,
    },
    SessionSend {
        agent_id: String,
        session_id: String,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        v: u32,
        server: String,
    },
    Pong {
        id: u64,
    },
    TurnStarted {
        turn_id: String,
        harness: HarnessKind,
    },
    /// Raw streaming output text.
    TurnDelta {
        turn_id: String,
        text: String,
    },
    /// Animation driver: one card per `span_id`; the client swipes the card
    /// away when the phase goes terminal (Done/Failed).
    TurnPhase {
        turn_id: String,
        span_id: String,
        phase: WirePhase,
    },
    /// Structured harness events (e.g. parsed `codex exec --json` items),
    /// passed through for richer rendering.
    TurnItem {
        turn_id: String,
        item: serde_json::Value,
    },
    TurnCompleted {
        turn_id: String,
        ok: bool,
        #[serde(default)]
        detail: Option<String>,
    },
    DashUpdate {
        topic: Topic,
        data: serde_json::Value,
    },
    SessionOutbound {
        agent_id: String,
        session_id: String,
        message: serde_json::Value,
    },
    /// Live, pre-final progress for an in-flight chat turn — the same side-lane
    /// Telegram reads ("Thinking…" + tool lines + cumulative answer text). `kind`
    /// is "tool" | "thinking" | "text" | "status"; for "text" the payload is the
    /// WHOLE answer-so-far (cumulative, not a delta), so the client replaces. A
    /// `status` of "done"/"error" marks the turn terminal; the authoritative final
    /// reply still arrives as `SessionOutbound`.
    SessionProgress {
        agent_id: String,
        session_id: String,
        message_id: String,
        seq: u64,
        kind: String,
        text: String,
    },
    Error {
        code: String,
        message: String,
        #[serde(default)]
        turn_id: Option<String>,
    },
}

/// Wire mirror of `maturana_core::animation::Phase` — core stays untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WirePhase {
    Queued,
    Building { tool: String },
    Running { tool: String },
    Done { detail: Option<String> },
    Failed { detail: Option<String> },
}

#[cfg(test)]
impl WirePhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, WirePhase::Done { .. } | WirePhase::Failed { .. })
    }
}

impl From<&maturana_core::animation::Phase> for WirePhase {
    fn from(phase: &maturana_core::animation::Phase) -> Self {
        use maturana_core::animation::Phase;
        match phase {
            Phase::Queued => WirePhase::Queued,
            Phase::Building { tool } => WirePhase::Building { tool: tool.clone() },
            Phase::Running { tool } => WirePhase::Running { tool: tool.clone() },
            Phase::Done { detail } => WirePhase::Done {
                detail: detail.clone(),
            },
            Phase::Failed { detail } => WirePhase::Failed {
                detail: detail.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(msg: &ClientMsg) {
        let json = serde_json::to_string(msg).unwrap();
        let back: ClientMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, msg, "client round trip failed for {json}");
    }

    fn round_trip_server(msg: &ServerMsg) {
        let json = serde_json::to_string(msg).unwrap();
        let back: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, msg, "server round trip failed for {json}");
    }

    #[test]
    fn client_messages_round_trip() {
        round_trip_client(&ClientMsg::Ping { id: 7 });
        round_trip_client(&ClientMsg::PromptSubmit {
            turn_id: "t1".into(),
            harness: HarnessKind::Codex,
            model: None,
            text: "list agents".into(),
        });
        round_trip_client(&ClientMsg::PromptSubmit {
            turn_id: "t2".into(),
            harness: HarnessKind::Openrouter,
            model: Some("anthropic/claude-sonnet-4.5".into()),
            text: "hello".into(),
        });
        round_trip_client(&ClientMsg::PromptCancel { turn_id: "t1".into() });
        round_trip_client(&ClientMsg::Subscribe {
            topics: vec![Topic::Agents, Topic::Runtime],
        });
        round_trip_client(&ClientMsg::Unsubscribe {
            topics: vec![Topic::Graph],
        });
        round_trip_client(&ClientMsg::SessionSend {
            agent_id: "codex-firecracker".into(),
            session_id: "codex-main".into(),
            text: "hi".into(),
        });
    }

    #[test]
    fn server_messages_round_trip() {
        round_trip_server(&ServerMsg::Hello {
            v: PROTOCOL_VERSION,
            server: "maturana-web".into(),
        });
        round_trip_server(&ServerMsg::Pong { id: 7 });
        round_trip_server(&ServerMsg::TurnStarted {
            turn_id: "t1".into(),
            harness: HarnessKind::Codex,
        });
        round_trip_server(&ServerMsg::TurnDelta {
            turn_id: "t1".into(),
            text: "chunk".into(),
        });
        round_trip_server(&ServerMsg::TurnPhase {
            turn_id: "t1".into(),
            span_id: "s1".into(),
            phase: WirePhase::Running {
                tool: "maturana agent inspect".into(),
            },
        });
        round_trip_server(&ServerMsg::TurnItem {
            turn_id: "t1".into(),
            item: serde_json::json!({"type": "agent_message", "text": "hi"}),
        });
        round_trip_server(&ServerMsg::TurnCompleted {
            turn_id: "t1".into(),
            ok: true,
            detail: Some("4.2s".into()),
        });
        round_trip_server(&ServerMsg::DashUpdate {
            topic: Topic::Agents,
            data: serde_json::json!([{"agent_id": "demo"}]),
        });
        round_trip_server(&ServerMsg::SessionOutbound {
            agent_id: "a".into(),
            session_id: "s".into(),
            message: serde_json::json!({"text": "reply"}),
        });
        round_trip_server(&ServerMsg::SessionProgress {
            agent_id: "a".into(),
            session_id: "s".into(),
            message_id: "m1".into(),
            seq: 3,
            kind: "text".into(),
            text: "partial answer".into(),
        });
        round_trip_server(&ServerMsg::Error {
            code: "not_implemented".into(),
            message: "soon".into(),
            turn_id: None,
        });
    }

    #[test]
    fn wire_phase_mirrors_core_animation_phase() {
        use maturana_core::animation::Phase;
        let pairs: Vec<(Phase, WirePhase)> = vec![
            (Phase::Queued, WirePhase::Queued),
            (
                Phase::Running {
                    tool: "x".into(),
                },
                WirePhase::Running { tool: "x".into() },
            ),
            (
                Phase::Done {
                    detail: Some("ok".into()),
                },
                WirePhase::Done {
                    detail: Some("ok".into()),
                },
            ),
            (
                Phase::Failed { detail: None },
                WirePhase::Failed { detail: None },
            ),
        ];
        for (core, wire) in pairs {
            assert_eq!(WirePhase::from(&core), wire);
            assert_eq!(
                maturana_core::animation::is_terminal(&core),
                wire.is_terminal()
            );
        }
    }

    #[test]
    fn tagged_wire_format_is_stable() {
        // The JS client matches on these exact strings; pin them.
        let json = serde_json::to_value(ServerMsg::Hello {
            v: 1,
            server: "maturana-web".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "hello");
        let json = serde_json::to_value(ServerMsg::TurnPhase {
            turn_id: "t".into(),
            span_id: "s".into(),
            phase: WirePhase::Building { tool: "b".into() },
        })
        .unwrap();
        assert_eq!(json["type"], "turn_phase");
        assert_eq!(json["phase"]["kind"], "building");
        let json = serde_json::to_value(ClientMsg::PromptSubmit {
            turn_id: "t".into(),
            harness: HarnessKind::Codex,
            model: None,
            text: "p".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "prompt_submit");
        assert_eq!(json["harness"], "codex");
    }
}
