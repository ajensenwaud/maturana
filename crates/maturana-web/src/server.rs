//! Router assembly + bind. The cockpit listens on 0.0.0.0:47836 by default
//! (LAN/Tailscale reach); the token login + cookie session gate everything
//! that matters. No TLS in v1 — front with Tailscale Serve/Caddy if exposed
//! beyond a trusted network.

use std::path::PathBuf;

use anyhow::Context;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::state::AppState;
use crate::{assets, auth, ws};

pub async fn serve(home_root: PathBuf, bind: &str) -> anyhow::Result<()> {
    let login_token = auth::ensure_web_token(&home_root)?;
    let state = AppState::new(home_root, login_token);

    let app = Router::new()
        .route("/", get(assets::index))
        .route("/health", get(health))
        .route("/login", get(assets::login_page).post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/ws", get(ws::upgrade))
        .route("/assets/*path", get(assets::asset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    println!("maturana web cockpit on http://{bind} (token: <home>/web/token)");
    axum::serve(listener, app).await.context("web server exited")
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}
