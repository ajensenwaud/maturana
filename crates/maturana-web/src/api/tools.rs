//! WASM tool registry view.

use axum::extract::State;
use axum::response::Response;
use maturana_core::tools::ToolRegistry;

use super::{blocking, ok};
use crate::state::AppState;

pub async fn list(State(state): State<AppState>) -> Response {
    let registry = ToolRegistry::new(state.home_root.join("tools"));
    match blocking(move || {
        let manifests = registry.list()?;
        Ok(serde_json::to_value(manifests)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
