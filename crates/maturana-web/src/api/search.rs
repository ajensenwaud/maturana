//! Cockpit web search: same providers/keys as `maturana search`.

use axum::extract::State;
use axum::response::Response;
use axum::Json;

use super::{blocking, ok};
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct SearchBody {
    query: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    count: Option<usize>,
}

pub async fn search(State(state): State<AppState>, Json(body): Json<SearchBody>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let provider: maturana_core::search::SearchProviderKind =
            body.provider.as_deref().unwrap_or("brave").parse()?;
        let results = maturana_core::search::search(
            &root,
            provider,
            &maturana_core::search::SearchRequest {
                query: body.query,
                count: body.count.unwrap_or(5),
            },
        )?;
        Ok(serde_json::to_value(results)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
