//! Orchestrator / board view: surface durable multi-agent runs
//! (`<home>/orchestration/<run_id>/plan.json`). Each run's steps ARE the board
//! cards (id, role, task, deps, status, result), so the cockpit renders the plan
//! as a status board. Read-only except `abort`, which writes the same durable
//! abort marker as the CLI.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{blocking, err, ok};
use crate::state::AppState;

/// All orchestration runs, newest first, each with its goal + step tally.
pub async fn list_runs(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || {
        let home = maturana_core::state::MaturanaHome::new(&root);
        Ok(serde_json::json!(
            maturana_ops::orchestration::list_orchestration_runs(&home)?
        ))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// The full plan for one run (goal + every step), so the board can render cards.
pub async fn run_detail(State(state): State<AppState>, Path(run_id): Path<String>) -> Response {
    if !maturana_ops::orchestration::valid_run_id(&run_id) {
        return err(StatusCode::BAD_REQUEST, "invalid run id");
    }
    let root = state.home_root.clone();
    match blocking(move || {
        let home = maturana_core::state::MaturanaHome::new(&root);
        Ok(serde_json::json!(
            maturana_ops::orchestration::orchestration_run_detail(&home, &run_id)?
        ))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Abort a run by writing the same durable cancellation marker as the CLI.
pub async fn abort_run(State(state): State<AppState>, Path(run_id): Path<String>) -> Response {
    if !maturana_ops::orchestration::valid_run_id(&run_id) {
        return err(StatusCode::BAD_REQUEST, "invalid run id");
    }
    let h = maturana_core::state::MaturanaHome::new(&state.home_root);
    match blocking(move || {
        maturana_ops::orchestration::request_abort(&h, &run_id)?;
        Ok(serde_json::json!({ "aborted": run_id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
