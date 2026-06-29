//! Router assembly + bind + background pollers. The cockpit listens on
//! 0.0.0.0:47836 by default (LAN/Tailscale reach); the token login + cookie
//! session gate everything that matters. No TLS in v1 — front with Tailscale
//! Serve/Caddy if exposed beyond a trusted network.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::state::{AppState, Broadcast, EnqueueTurnFn, IngestFileFn};
use crate::ws::protocol::{ServerMsg, Topic};
use crate::{api, assets, auth, ws};

pub async fn serve(
    home_root: PathBuf,
    bind: &str,
    enqueue: EnqueueTurnFn,
    ingest: Option<IngestFileFn>,
) -> anyhow::Result<()> {
    let login_token = auth::ensure_web_token(&home_root)?;
    // Providers resolve several conventional paths (.maturana/keys, images,
    // host-auth, hostd token) relative to the repo root. Under a service the
    // cwd is somewhere else entirely (System32 for Scheduled Tasks), so
    // anchor the process cwd to the home's parent — the same position every
    // interactive CLI invocation runs from.
    if let Some(repo_root) = home_root.parent() {
        let _ = std::env::set_current_dir(repo_root);
    }
    // Belt-and-braces for the hostd token specifically (documented override).
    if std::env::var("MATURANA_HOSTD_TOKEN_PATH").is_err() {
        let hostd_token = home_root.join("hostd").join("token");
        if hostd_token.exists() {
            std::env::set_var("MATURANA_HOSTD_TOKEN_PATH", &hostd_token);
        }
    }
    let token_for_banner = login_token.clone();
    let state = AppState::new(home_root, login_token, enqueue).with_ingest(ingest);

    tokio::spawn(dashboard_poller(state.clone()));
    tokio::spawn(web_outbound_poller(state.clone()));
    tokio::spawn(web_progress_poller(state.clone()));
    tokio::spawn(egress_poller(state.clone()));

    let app = Router::new()
        .route("/", get(assets::index))
        .route("/health", get(health))
        .route("/login", get(assets::login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/ws", get(ws::upgrade))
        .route("/assets/*path", get(assets::asset))
        .merge(api::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    println!("maturana web cockpit on http://{bind}");
    println!("  login token: {token_for_banner}");
    println!("  (also at <home>/web/token)");
    axum::serve(listener, app).await.context("web server exited")
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Push agents + runtime snapshots to subscribed sockets every few seconds.
/// Skips the work entirely while nobody is connected.
async fn dashboard_poller(state: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(4)).await;
        if state.dash_tx.receiver_count() == 0 {
            continue;
        }
        let root = state.home_root.clone();
        if let Ok(Ok(agents)) =
            tokio::task::spawn_blocking(move || api::agents::snapshot(&root)).await
        {
            let _ = state.dash_tx.send(Broadcast::Dash(Topic::Agents, agents));
        }
        let up_path = state.home_root.join("up").join("state.json");
        let up = tokio::task::spawn_blocking(move || {
            std::fs::read_to_string(&up_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .unwrap_or(serde_json::json!({ "running": false }))
        })
        .await
        .unwrap_or(serde_json::json!({ "running": false }));
        let _ = state.dash_tx.send(Broadcast::Dash(Topic::Runtime, up));
    }
}

/// Deliver guest replies on the "web" channel back to connected operators.
/// Messages are only marked delivered once at least one socket received the
/// broadcast — with nobody connected they stay queued for the next poll.
async fn web_outbound_poller(state: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if state.dash_tx.receiver_count() == 0 {
            continue;
        }
        let root = state.home_root.clone();
        let pending = tokio::task::spawn_blocking(move || collect_web_outbound(&root))
            .await
            .unwrap_or_default();
        for (agent_id, session_id, message) in pending {
            let delivered = state.dash_tx.send(Broadcast::Session(ServerMsg::SessionOutbound {
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                message: serde_json::to_value(&message).unwrap_or_default(),
            }));
            if delivered.is_ok() {
                let root = state.home_root.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let paths = maturana_core::session_db::session_paths(
                        &root.join("agents").join(&agent_id),
                        &session_id,
                    );
                    maturana_core::session_db::mark_delivered(&paths, &message.id, None)
                })
                .await;
            }
        }
    }
}

/// Stream the live progress side-lane for in-flight web chat turns (the same
/// JSONL Telegram reads: tool lines, thinking, and cumulative answer text) so
/// replies appear as they're generated instead of all at once on completion.
/// Turns are registered by the `SessionSend` handler and removed here once they
/// go terminal (status done/error) or age out (worker died mid-turn).
async fn web_progress_poller(state: AppState) {
    loop {
        // Faster than the outbound poll (2s) — this is the live-typing feed.
        tokio::time::sleep(Duration::from_millis(700)).await;
        if state.dash_tx.receiver_count() == 0 {
            continue;
        }
        // Snapshot the watch list (key + how far we've streamed each).
        let watching: Vec<((String, String, String), Option<u64>)> = {
            let Ok(guard) = state.active_turns.lock() else {
                continue;
            };
            guard
                .iter()
                .map(|(key, watch)| (key.clone(), watch.last_seq))
                .collect()
        };
        if watching.is_empty() {
            continue;
        }
        let root = state.home_root.clone();
        let progress = tokio::task::spawn_blocking(move || read_new_progress(&root, watching))
            .await
            .unwrap_or_default();
        let mut terminal: Vec<(String, String, String)> = Vec::new();
        for (key, events, is_terminal) in progress {
            let mut max_seq: Option<u64> = None;
            for event in events {
                max_seq = Some(max_seq.map_or(event.seq, |m: u64| m.max(event.seq)));
                let _ = state.dash_tx.send(Broadcast::Session(ServerMsg::SessionProgress {
                    agent_id: key.0.clone(),
                    session_id: key.1.clone(),
                    message_id: key.2.clone(),
                    seq: event.seq,
                    kind: event.kind,
                    text: event.text,
                }));
            }
            if let (Some(max), Ok(mut guard)) = (max_seq, state.active_turns.lock()) {
                if let Some(watch) = guard.get_mut(&key) {
                    watch.last_seq = Some(watch.last_seq.map_or(max, |prev| prev.max(max)));
                }
            }
            if is_terminal {
                terminal.push(key);
            }
        }
        // Drop finished turns + age out any whose worker died without a terminal
        // status (15 min), so the map never grows unbounded.
        let mut to_clear = terminal.clone();
        if let Ok(mut guard) = state.active_turns.lock() {
            for key in &terminal {
                guard.remove(key);
            }
            let now = std::time::Instant::now();
            guard.retain(|key, watch| {
                let fresh = now.duration_since(watch.started) < Duration::from_secs(900);
                if !fresh {
                    to_clear.push(key.clone());
                }
                fresh
            });
        }
        // Remove the now-finished side-lane files (mirrors Telegram's cleanup).
        for key in to_clear {
            let root = state.home_root.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let paths = maturana_core::session_db::session_paths(
                    &root.join("agents").join(&key.0),
                    &key.1,
                );
                maturana_core::session_db::clear_progress(&paths, &key.2)
            })
            .await;
        }
    }
}

/// For each watched turn, read its progress side-lane and return only the events
/// newer than `last_seq`, plus whether the turn reached a terminal status. A
/// missing file yields no events (the worker hasn't written one yet).
#[allow(clippy::type_complexity)]
fn read_new_progress(
    root: &std::path::Path,
    watching: Vec<((String, String, String), Option<u64>)>,
) -> Vec<(
    (String, String, String),
    Vec<maturana_core::session_db::ProgressEvent>,
    bool,
)> {
    let mut out = Vec::new();
    for (key, last_seq) in watching {
        let paths = maturana_core::session_db::session_paths(
            &root.join("agents").join(&key.0),
            &key.1,
        );
        let all = maturana_core::session_db::read_progress(&paths, &key.2).unwrap_or_default();
        let terminal = all
            .iter()
            .any(|e| e.kind == "status" && (e.text == "done" || e.text == "error"));
        let fresh: Vec<_> = all
            .into_iter()
            .filter(|e| last_seq.map_or(true, |seen| e.seq > seen))
            .collect();
        if !fresh.is_empty() || terminal {
            out.push((key, fresh, terminal));
        }
    }
    out
}

/// Tail the pipelock proxy audit JSONL files and stream new lines to
/// subscribers on the Egress topic. Tracks a byte offset per file so each line
/// is emitted once; a shrunk file (rotation/truncation) resets its offset.
async fn egress_poller(state: AppState) {
    use std::collections::HashMap;
    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if state.dash_tx.receiver_count() == 0 {
            continue;
        }
        let audit_dir = state.home_root.join("audit");
        let known = offsets.clone();
        let (events, new_offsets) =
            tokio::task::spawn_blocking(move || read_new_egress(&audit_dir, known))
                .await
                .unwrap_or_default();
        offsets = new_offsets;
        for event in events {
            let _ = state.dash_tx.send(Broadcast::Dash(Topic::Egress, event));
        }
    }
}

/// Read newly-appended audit lines across all `*-pipelock-proxy.jsonl` files,
/// returning the parsed JSON objects (with the agent id derived from the file
/// name) and the updated offset map.
fn read_new_egress(
    audit_dir: &std::path::Path,
    mut offsets: std::collections::HashMap<PathBuf, u64>,
) -> (Vec<serde_json::Value>, std::collections::HashMap<PathBuf, u64>) {
    use std::io::{Read, Seek, SeekFrom};
    let mut events = Vec::new();
    let Ok(entries) = std::fs::read_dir(audit_dir) else {
        return (events, offsets);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with("pipelock-proxy.jsonl") {
            continue;
        }
        let agent = name
            .strip_suffix("-pipelock-proxy.jsonl")
            .filter(|a| *a != "pipelock")
            .map(str::to_string);
        let Ok(mut file) = std::fs::File::open(&path) else {
            continue;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        // First sight: start at the current end so the feed shows only egress
        // that happens while the cockpit is open (avoids replaying history and
        // mid-line parsing). A shrunk file (rotation) resets to its start.
        let start = match offsets.get(&path).copied() {
            Some(prev) if prev <= len => prev,
            Some(_) => 0,
            None => len,
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            continue;
        }
        offsets.insert(path.clone(), start + buf.len() as u64);
        for line in buf.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) {
                if let (Some(obj), Some(agent)) = (value.as_object_mut(), agent.as_ref()) {
                    obj.insert("agent_id".into(), serde_json::json!(agent));
                }
                events.push(value);
            }
        }
    }
    (events, offsets)
}

fn collect_web_outbound(
    root: &std::path::Path,
) -> Vec<(String, String, maturana_core::session_db::OutboundMessage)> {
    let mut pending = Vec::new();
    let Ok(agents) = std::fs::read_dir(root.join("agents")) else {
        return pending;
    };
    for agent in agents.flatten() {
        let agent_id = agent.file_name().to_string_lossy().to_string();
        let Ok(sessions) = std::fs::read_dir(agent.path().join("sessions")) else {
            continue;
        };
        for session in sessions.flatten() {
            if !session.path().is_dir() {
                continue;
            }
            let session_id = session.file_name().to_string_lossy().to_string();
            let paths = maturana_core::session_db::session_paths(&agent.path(), &session_id);
            let Ok(undelivered) = maturana_core::session_db::list_undelivered(&paths) else {
                continue;
            };
            for message in undelivered {
                if message.channel == "web" {
                    pending.push((agent_id.clone(), session_id.clone(), message));
                }
            }
        }
    }
    pending
}
