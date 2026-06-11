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

use crate::state::{AppState, Broadcast};
use crate::ws::protocol::{ServerMsg, Topic};
use crate::{api, assets, auth, ws};

pub async fn serve(home_root: PathBuf, bind: &str) -> anyhow::Result<()> {
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
    let state = AppState::new(home_root, login_token);

    tokio::spawn(dashboard_poller(state.clone()));
    tokio::spawn(web_outbound_poller(state.clone()));

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
