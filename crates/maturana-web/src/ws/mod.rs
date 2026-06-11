//! WebSocket endpoint: one socket per client, all topics multiplexed.
//!
//! Phase 1 handles the connection lifecycle, Ping/Pong, and topic
//! subscriptions feeding `DashUpdate` fan-out. Prompt turns (Phase 2) and
//! session sends (Phase 3) answer `not_implemented` until their phases land.

pub mod protocol;

use std::collections::HashSet;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::auth;
use crate::state::AppState;
use protocol::{ClientMsg, ServerMsg, Topic, PROTOCOL_VERSION};

pub async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // The auth middleware already validated the session cookie; re-check here
    // (defence in depth) and enforce Origin==Host, which only matters on
    // browser-initiated upgrades.
    if !auth::has_valid_session(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !auth::origin_matches_host(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(state, socket))
}

async fn handle_socket(state: AppState, mut socket: WebSocket) {
    let hello = ServerMsg::Hello {
        v: PROTOCOL_VERSION,
        server: "maturana-web".to_string(),
    };
    if send(&mut socket, &hello).await.is_err() {
        return;
    }

    let mut topics: HashSet<Topic> = HashSet::new();
    let mut dash_rx = state.dash_tx.subscribe();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else { continue };
                let parsed: Result<ClientMsg, _> = serde_json::from_str(&text);
                let reply = match parsed {
                    Ok(msg) => handle_client_msg(msg, &mut topics),
                    Err(error) => Some(ServerMsg::Error {
                        code: "bad_message".to_string(),
                        message: error.to_string(),
                        turn_id: None,
                    }),
                };
                if let Some(reply) = reply {
                    if send(&mut socket, &reply).await.is_err() {
                        break;
                    }
                }
            }
            event = dash_rx.recv() => {
                match event {
                    Ok(event) if topics.contains(&event.topic) => {
                        let update = ServerMsg::DashUpdate {
                            topic: event.topic,
                            data: event.data,
                        };
                        if send(&mut socket, &update).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    // Lagged: drop missed updates; pollers re-publish shortly.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn handle_client_msg(msg: ClientMsg, topics: &mut HashSet<Topic>) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Ping { id } => Some(ServerMsg::Pong { id }),
        ClientMsg::Subscribe { topics: wanted } => {
            topics.extend(wanted);
            None
        }
        ClientMsg::Unsubscribe { topics: unwanted } => {
            for topic in unwanted {
                topics.remove(&topic);
            }
            None
        }
        ClientMsg::PromptSubmit { turn_id, .. } | ClientMsg::PromptCancel { turn_id } => {
            Some(ServerMsg::Error {
                code: "not_implemented".to_string(),
                message: "prompt console lands in phase 2".to_string(),
                turn_id: Some(turn_id),
            })
        }
        ClientMsg::SessionSend { .. } => Some(ServerMsg::Error {
            code: "not_implemented".to_string(),
            message: "session sends land in phase 3".to_string(),
            turn_id: None,
        }),
    }
}

async fn send(socket: &mut WebSocket, msg: &ServerMsg) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("server messages always serialize");
    socket.send(Message::Text(text)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_gets_pong_and_subscriptions_track() {
        let mut topics = HashSet::new();
        assert_eq!(
            handle_client_msg(ClientMsg::Ping { id: 3 }, &mut topics),
            Some(ServerMsg::Pong { id: 3 })
        );
        handle_client_msg(
            ClientMsg::Subscribe {
                topics: vec![Topic::Agents, Topic::Graph],
            },
            &mut topics,
        );
        assert!(topics.contains(&Topic::Agents));
        handle_client_msg(
            ClientMsg::Unsubscribe {
                topics: vec![Topic::Agents],
            },
            &mut topics,
        );
        assert!(!topics.contains(&Topic::Agents));
        assert!(topics.contains(&Topic::Graph));
    }

    #[test]
    fn unimplemented_messages_answer_with_error_code() {
        let mut topics = HashSet::new();
        let reply = handle_client_msg(
            ClientMsg::PromptSubmit {
                turn_id: "t9".into(),
                harness: protocol::HarnessKind::Codex,
                model: None,
                text: "hi".into(),
            },
            &mut topics,
        );
        match reply {
            Some(ServerMsg::Error { code, turn_id, .. }) => {
                assert_eq!(code, "not_implemented");
                assert_eq!(turn_id.as_deref(), Some("t9"));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }
}
