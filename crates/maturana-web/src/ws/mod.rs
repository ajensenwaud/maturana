//! WebSocket endpoint: one socket per client, all topics multiplexed.
//!
//! Prompt turns spawn a harness adapter (codex exec --json by default,
//! opencode/OpenRouter as the pluggable alternative); their event streams are
//! forwarded as `TurnDelta`/`TurnPhase`/`TurnItem`/`TurnCompleted`. Turns are
//! owned by the socket task: a `PromptCancel` — or the socket dropping — kills
//! the child's whole process tree. Session sends land in phase 3.

pub mod protocol;

use std::collections::{HashMap, HashSet};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::mpsc;

use crate::auth;
use crate::harness::{adapter_for, TurnEvent, TurnHandle, TurnRequest};
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
    // Turns owned by THIS socket: turn_id → handle. Killed on socket drop.
    let mut turns: HashMap<String, TurnHandle> = HashMap::new();
    let (turn_tx, mut turn_rx) = mpsc::channel::<(String, TurnEvent)>(256);

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                let Message::Text(text) = message else { continue };
                let parsed: Result<ClientMsg, _> = serde_json::from_str(&text);
                let reply = match parsed {
                    Ok(msg) => handle_client_msg(&state, msg, &mut topics, &mut turns, &turn_tx),
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
            turn_event = turn_rx.recv() => {
                let Some((turn_id, event)) = turn_event else { break };
                let message = turn_event_to_msg(&turn_id, event, &mut turns);
                if send(&mut socket, &message).await.is_err() {
                    break;
                }
            }
            event = dash_rx.recv() => {
                match event {
                    Ok(crate::state::Broadcast::Dash(topic, data)) => {
                        if topics.contains(&topic) {
                            let update = ServerMsg::DashUpdate { topic, data };
                            if send(&mut socket, &update).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(crate::state::Broadcast::Session(message)) => {
                        if send(&mut socket, &message).await.is_err() {
                            break;
                        }
                    }
                    // Lagged: drop missed updates; pollers re-publish shortly.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Socket gone: no orphaned harness children.
    for (_, handle) in turns.drain() {
        handle.cancel();
    }
}

fn handle_client_msg(
    state: &AppState,
    msg: ClientMsg,
    topics: &mut HashSet<Topic>,
    turns: &mut HashMap<String, TurnHandle>,
    turn_tx: &mpsc::Sender<(String, TurnEvent)>,
) -> Option<ServerMsg> {
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
        ClientMsg::PromptSubmit {
            turn_id,
            harness,
            model,
            text,
        } => {
            if turns.contains_key(&turn_id) {
                return Some(ServerMsg::Error {
                    code: "duplicate_turn".to_string(),
                    message: "turn id already running".to_string(),
                    turn_id: Some(turn_id),
                });
            }
            let adapter = adapter_for(&harness);
            // cwd = repo root (parent of the home dir by convention) so the
            // harness reads the same AGENTS.md + skills/ as a CLI session.
            let cwd = state
                .home_root
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| state.home_root.clone());
            let request = TurnRequest {
                turn_id: turn_id.clone(),
                text,
                model,
                cwd,
                home_root: state.home_root.clone(),
            };
            let (event_tx, mut event_rx) = mpsc::channel::<TurnEvent>(256);
            match adapter.start_turn(request, event_tx) {
                Ok(handle) => {
                    turns.insert(turn_id.clone(), handle);
                    // Re-tag this turn's events onto the socket's single queue.
                    let forward = turn_tx.clone();
                    let forwarded_id = turn_id.clone();
                    tokio::spawn(async move {
                        while let Some(event) = event_rx.recv().await {
                            if forward.send((forwarded_id.clone(), event)).await.is_err() {
                                break;
                            }
                        }
                    });
                    Some(ServerMsg::TurnStarted { turn_id, harness })
                }
                Err(error) => Some(ServerMsg::Error {
                    code: "turn_spawn_failed".to_string(),
                    message: format!("{error:#}"),
                    turn_id: Some(turn_id),
                }),
            }
        }
        ClientMsg::PromptCancel { turn_id } => match turns.remove(&turn_id) {
            Some(handle) => {
                handle.cancel();
                None
            }
            None => Some(ServerMsg::Error {
                code: "unknown_turn".to_string(),
                message: "no such running turn".to_string(),
                turn_id: Some(turn_id),
            }),
        },
        ClientMsg::SessionSend {
            agent_id,
            session_id,
            text,
        } => {
            // SECURITY: the enqueue closure builds filesystem paths from these ids
            // (agents/<agent_id>/sessions/<session_id>/…), so validate them with the
            // SAME guard the REST layer uses — the WS extractor does no path checks,
            // and an unvalidated `agent_id="../../.."` would write outside the home
            // tree.
            if !crate::api::valid_id(&agent_id) || !crate::api::valid_id(&session_id) {
                return Some(ServerMsg::Error {
                    code: "session_send_failed".to_string(),
                    message: "invalid agent or session id".to_string(),
                    turn_id: None,
                });
            }
            // Route through the SHARED channel front door (injected by the CLI),
            // exactly like Telegram/TUI/Discord — so the cockpit turn gets the
            // recent-transcript context (memory), model/reasoning, and routing
            // instead of a bare prompt. The outbound poller delivers the reply
            // back over WS.
            let result = (state.enqueue)(&state.home_root, &agent_id, &session_id, &text);
            match result {
                Ok(message_id) => {
                    // Register the turn so the progress poller streams its
                    // side-lane (tool/thinking/answer-text) back to the chat
                    // until it goes terminal.
                    if let Ok(mut active) = state.active_turns.lock() {
                        active.insert(
                            (agent_id.clone(), session_id.clone(), message_id.clone()),
                            crate::state::TurnWatch {
                                last_seq: None,
                                started: std::time::Instant::now(),
                            },
                        );
                    }
                    Some(ServerMsg::SessionOutbound {
                        agent_id,
                        session_id,
                        message: serde_json::json!({ "queued": message_id }),
                    })
                }
                Err(error) => Some(ServerMsg::Error {
                    code: "session_send_failed".to_string(),
                    message: format!("{error:#}"),
                    turn_id: None,
                }),
            }
        }
    }
}

fn turn_event_to_msg(
    turn_id: &str,
    event: TurnEvent,
    turns: &mut HashMap<String, TurnHandle>,
) -> ServerMsg {
    match event {
        TurnEvent::Delta(text) => ServerMsg::TurnDelta {
            turn_id: turn_id.to_string(),
            text,
        },
        TurnEvent::Phase { span_id, phase } => ServerMsg::TurnPhase {
            turn_id: turn_id.to_string(),
            span_id,
            phase,
        },
        TurnEvent::Item(item) => ServerMsg::TurnItem {
            turn_id: turn_id.to_string(),
            item,
        },
        TurnEvent::Completed { ok, detail } => {
            turns.remove(turn_id);
            ServerMsg::TurnCompleted {
                turn_id: turn_id.to_string(),
                ok,
                detail,
            }
        }
    }
}

async fn send(socket: &mut WebSocket, msg: &ServerMsg) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("server messages always serialize");
    socket.send(Message::Text(text)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        AppState::new(
            std::env::temp_dir().join("mweb-ws-test"),
            "tok".into(),
            std::sync::Arc::new(|_, _, _, _| Ok("test-msg".to_string())),
        )
    }

    #[test]
    fn ping_gets_pong_and_subscriptions_track() {
        let state = test_state();
        let (tx, _rx) = mpsc::channel(8);
        let mut topics = HashSet::new();
        let mut turns = HashMap::new();
        assert_eq!(
            handle_client_msg(&state, ClientMsg::Ping { id: 3 }, &mut topics, &mut turns, &tx),
            Some(ServerMsg::Pong { id: 3 })
        );
        handle_client_msg(
            &state,
            ClientMsg::Subscribe {
                topics: vec![Topic::Agents, Topic::Graph],
            },
            &mut topics,
            &mut turns,
            &tx,
        );
        assert!(topics.contains(&Topic::Agents));
        handle_client_msg(
            &state,
            ClientMsg::Unsubscribe {
                topics: vec![Topic::Agents],
            },
            &mut topics,
            &mut turns,
            &tx,
        );
        assert!(!topics.contains(&Topic::Agents));
        assert!(topics.contains(&Topic::Graph));
    }

    #[test]
    fn cancel_of_unknown_turn_errors() {
        let state = test_state();
        let (tx, _rx) = mpsc::channel(8);
        let reply = handle_client_msg(
            &state,
            ClientMsg::PromptCancel { turn_id: "t9".into() },
            &mut HashSet::new(),
            &mut HashMap::new(),
            &tx,
        );
        match reply {
            Some(ServerMsg::Error { code, turn_id, .. }) => {
                assert_eq!(code, "unknown_turn");
                assert_eq!(turn_id.as_deref(), Some("t9"));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn completion_event_clears_turn_registry() {
        let mut turns = HashMap::new();
        turns.insert(
            "t1".to_string(),
            TurnHandle {
                pid: None,
                child_kill: None,
            },
        );
        let msg = turn_event_to_msg(
            "t1",
            TurnEvent::Completed {
                ok: true,
                detail: None,
            },
            &mut turns,
        );
        assert!(matches!(msg, ServerMsg::TurnCompleted { ok: true, .. }));
        assert!(turns.is_empty());
    }
}
