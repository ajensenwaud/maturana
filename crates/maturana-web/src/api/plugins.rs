//! Plugin catalog API. Plugins are discovered through `maturana-ops`, so the
//! cockpit and CLI share one manifest contract instead of maintaining parallel
//! discovery rules.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use maturana_core::state::MaturanaHome;

use super::{blocking, err, ok};
use crate::state::AppState;

fn home(state: &AppState) -> MaturanaHome {
    MaturanaHome::new(state.home_root.clone())
}

pub async fn list(State(state): State<AppState>) -> Response {
    let home = home(&state);
    match blocking(move || {
        let plugins = maturana_ops::plugins::list_plugin_statuses(&home)?;
        Ok(serde_json::to_value(plugins)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn roots(State(state): State<AppState>) -> Response {
    let home = home(&state);
    match blocking(move || Ok(maturana_ops::plugins::plugin_roots_json(&home))).await {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct AssetsQuery {
    kind: Option<String>,
}

pub async fn assets(State(state): State<AppState>, Query(query): Query<AssetsQuery>) -> Response {
    if let Some(kind) = query.kind.as_deref() {
        if !super::valid_id(kind) {
            return err(StatusCode::BAD_REQUEST, "invalid asset kind");
        }
    }
    let home = home(&state);
    match blocking(move || {
        let mut assets = maturana_ops::plugins::enabled_plugin_assets(&home)?;
        if let Some(kind) = query.kind {
            assets.retain(|asset| asset.kind == kind);
        }
        Ok(serde_json::to_value(assets)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn detail(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !super::valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid plugin name");
    }
    let home = home(&state);
    match blocking(move || {
        maturana_ops::plugins::inspect_plugin_status(&home, &name)?
            .map(|plugin| serde_json::to_value(plugin).map_err(Into::into))
            .unwrap_or_else(|| anyhow::bail!("plugin not found: {name}"))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
