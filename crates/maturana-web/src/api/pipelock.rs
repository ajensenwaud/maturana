//! Pipelock panel: secret NAMES only — values never serialize toward the
//! browser, in either direction except `set` (write-only).

use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use maturana_core::pipelock::PipelockVault;

use super::{blocking, ok};
use crate::state::AppState;

fn vault(state: &AppState) -> PipelockVault {
    PipelockVault::new(state.home_root.join("pipelock"))
}

pub async fn list(State(state): State<AppState>) -> Response {
    let vault = vault(&state);
    match blocking(move || vault.list()).await {
        Ok(names) => ok(serde_json::json!({ "names": names })),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct SetBody {
    name: String,
    value: String,
}

pub async fn set(State(state): State<AppState>, Json(body): Json<SetBody>) -> Response {
    let vault = vault(&state);
    match blocking(move || {
        vault.set(&body.name, &body.value)?;
        Ok(body.name)
    })
    .await
    {
        Ok(name) => ok(serde_json::json!({ "set": name })),
        Err(response) => response,
    }
}

pub async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let vault = vault(&state);
    match blocking(move || vault.delete(&name).map(|deleted| (name, deleted))).await {
        Ok((name, deleted)) => ok(serde_json::json!({ "name": name, "deleted": deleted })),
        Err(response) => response,
    }
}
