//! WASM tool registry: list the registered tools and define (register) new ones.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::Response;
use maturana_core::tools::{Capabilities, ResourceLimits, ToolManifest, ToolRegistry};

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

/// Manifest fields for a tool registration, carried as query params alongside
/// the raw `.wasm` request body (same upload shape as `/api/voice/stt`). List
/// fields (`net`/`env`/`fs_read`/`fs_write`) are comma-separated.
#[derive(serde::Deserialize)]
pub struct RegisterQuery {
    name: String,
    version: Option<String>,
    description: Option<String>,
    net: Option<String>,
    env: Option<String>,
    fs_read: Option<String>,
    fs_write: Option<String>,
}

fn split_list(value: &Option<String>) -> Vec<String> {
    value
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Define a tool: write the uploaded WASM module + a normalized manifest into
/// the host registry (`<home>/tools/<name>/`). The registry validates the name,
/// capability paths, and that the bytes are a real WASM module. This mirrors
/// `maturana tool register`; the operator is already authenticated by the
/// session middleware, and registering does not run the module.
pub async fn register(
    State(state): State<AppState>,
    Query(query): Query<RegisterQuery>,
    body: Bytes,
) -> Response {
    let registry = ToolRegistry::new(state.home_root.join("tools"));
    let wasm_bytes = body.to_vec();
    match blocking(move || {
        let manifest = ToolManifest {
            name: query.name.trim().to_string(),
            version: query
                .version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .unwrap_or("0.1.0")
                .to_string(),
            description: query.description.unwrap_or_default(),
            wasm: "module.wasm".to_string(),
            capabilities: Capabilities {
                fs_read: split_list(&query.fs_read),
                fs_write: split_list(&query.fs_write),
                env: split_list(&query.env),
                net: split_list(&query.net),
            },
            limits: ResourceLimits::default(),
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
        };
        let stored = registry.register(&manifest, &wasm_bytes)?;
        Ok(serde_json::to_value(stored)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
