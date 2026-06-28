//! Durable orchestration boards: define cards (title, assignee, deps), run them
//! across agents, and monitor live. The board engine + dispatcher live in the
//! CLI (`maturana board …`, runs each card in its assignee's VM over A2A); this
//! API edits the typed `maturana_core::board::Board` store directly and triggers
//! a run by shelling out to the binary detached — the cockpit never becomes a
//! second, weaker execution path.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use maturana_core::board::{Board, CardStatus};
use maturana_core::state::MaturanaHome;

use super::{blocking, err, ok, valid_id};
use crate::state::AppState;

fn home(state: &AppState) -> MaturanaHome {
    MaturanaHome::new(state.home_root.clone())
}

/// A board is "running" if a card is in flight, or its run log's last event
/// isn't a run_end (a dispatcher is between batches).
fn is_running(home: &MaturanaHome, board: &Board) -> bool {
    if board.cards.iter().any(|c| c.status == CardStatus::Doing) {
        return true;
    }
    let events = maturana_core::board::read_events(home, &board.name);
    matches!(events.last(), Some(e) if e.kind != "run_end")
}

/// Every board + its column counts + whether it's currently running.
pub async fn list(State(state): State<AppState>) -> Response {
    let h = home(&state);
    match blocking(move || {
        let mut out = Vec::new();
        for name in Board::list_names(&h) {
            let Ok(board) = Board::load(&h, &name) else { continue };
            let (todo, doing, done, blocked) = board.counts();
            out.push(serde_json::json!({
                "name": name,
                "total": board.cards.len(),
                "todo": todo, "doing": doing, "done": done, "blocked": blocked,
                "running": is_running(&h, &board),
            }));
        }
        Ok(serde_json::json!(out))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct CreateBody {
    name: String,
}

/// Create a new (empty) board.
pub async fn create(State(state): State<AppState>, Json(body): Json<CreateBody>) -> Response {
    let h = home(&state);
    let name = body.name.trim().to_string();
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name (use letters, digits, - _ .)");
    }
    match blocking(move || {
        if Board::path(&h, &name).exists() {
            anyhow::bail!("board '{name}' already exists");
        }
        Board::new(&name).save(&h)?;
        Ok(serde_json::json!({ "created": name }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// The full board (cards) + its run log, for the editor + live monitor.
pub async fn detail(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    match blocking(move || {
        if !Board::path(&h, &name).exists() {
            anyhow::bail!("no such board");
        }
        let board = Board::load(&h, &name)?;
        let events = maturana_core::board::read_events(&h, &name);
        Ok(serde_json::json!({
            "name": board.name,
            "running": is_running(&h, &board),
            "cards": board.cards,
            "events": events,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Delete a board (and its run log).
pub async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    match blocking(move || {
        let _ = std::fs::remove_file(Board::path(&h, &name));
        maturana_core::board::clear_events(&h, &name);
        Ok(serde_json::json!({ "deleted": name }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct AddCardBody {
    title: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    needs: Vec<String>,
}

/// Add a card to a board.
pub async fn add_card(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<AddCardBody>,
) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    match blocking(move || {
        let title = body.title.trim();
        if title.is_empty() {
            anyhow::bail!("a card needs a title");
        }
        let mut board = Board::load(&h, &name)?;
        let deps: Vec<String> = body
            .needs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for dep in &deps {
            if board.card(dep).is_none() {
                anyhow::bail!("card depends on unknown card '{dep}'");
            }
        }
        let assignee = body.assignee.and_then(|a| {
            let a = a.trim().to_string();
            if a.is_empty() { None } else { Some(a) }
        });
        let id = board.add(title, body.detail.as_deref().unwrap_or("").trim(), assignee, deps);
        board.validate().map_err(|e| anyhow::anyhow!(e))?;
        board.save(&h)?;
        Ok(serde_json::json!({ "added": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct EditCardBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    deps: Option<Vec<String>>,
    #[serde(default)]
    status: Option<String>,
}

/// Edit a card (any subset of fields). Re-validates the board.
pub async fn edit_card(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    Json(body): Json<EditCardBody>,
) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let h = home(&state);
    match blocking(move || {
        let mut board = Board::load(&h, &name)?;
        // Resolve the new status (if any) before the mutable borrow.
        let new_status = match &body.status {
            Some(s) => Some(
                CardStatus::parse(s)
                    .ok_or_else(|| anyhow::anyhow!("unknown status '{s}'"))?,
            ),
            None => None,
        };
        // Validate deps reference existing cards (and not itself) up front.
        if let Some(deps) = &body.deps {
            for dep in deps {
                if dep == &id {
                    anyhow::bail!("a card cannot depend on itself");
                }
                if board.card(dep).is_none() {
                    anyhow::bail!("card depends on unknown card '{dep}'");
                }
            }
        }
        {
            let card = board.card_mut(&id).ok_or_else(|| anyhow::anyhow!("no such card"))?;
            if let Some(t) = body.title {
                let t = t.trim();
                if !t.is_empty() {
                    card.title = t.to_string();
                }
            }
            if let Some(d) = body.detail {
                card.detail = d.trim().to_string();
            }
            if let Some(a) = body.assignee {
                let a = a.trim().to_string();
                card.assignee = if a.is_empty() { None } else { Some(a) };
            }
            if let Some(deps) = body.deps {
                card.deps = deps.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            if let Some(st) = new_status {
                card.status = st;
            }
        }
        board.validate().map_err(|e| anyhow::anyhow!(e))?;
        board.save(&h)?;
        Ok(serde_json::json!({ "updated": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Remove a card (and drop it from other cards' deps).
pub async fn delete_card(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let h = home(&state);
    match blocking(move || {
        let mut board = Board::load(&h, &name)?;
        if !board.remove_card(&id) {
            anyhow::bail!("no such card");
        }
        board.save(&h)?;
        Ok(serde_json::json!({ "deleted": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Reset a board's finished/failed cards to todo for a clean re-run.
pub async fn reset(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    match blocking(move || {
        let mut board = Board::load(&h, &name)?;
        let n = board.reset_for_rerun();
        board.save(&h)?;
        maturana_core::board::clear_events(&h, &name);
        Ok(serde_json::json!({ "reset": n }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Run a board: spawn `maturana board run <name>` DETACHED (the real dispatcher),
/// logging to board/<name>.run.log. The board JSON + run log update as cards
/// progress; the cockpit polls them for live status.
pub async fn run(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let home_root = state.home_root.clone();
    let h = home(&state);
    match blocking(move || {
        let board = Board::load(&h, &name)?;
        if board.cards.is_empty() {
            anyhow::bail!("board is empty — add cards first");
        }
        if is_running(&h, &board) {
            anyhow::bail!("board is already running");
        }
        let exe = std::env::current_exe()?;
        let log_path = Board::dir(&h).join(format!("{name}.run.log"));
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::File::create(&log_path)?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--home")
            .arg(&home_root)
            .arg("board")
            .arg("run")
            .arg("--board")
            .arg(&name)
            .stdin(std::process::Stdio::null())
            .stderr(log.try_clone()?)
            .stdout(log);
        let mut child = cmd.spawn()?;
        // Reap off-thread so the child never zombies after we return.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(serde_json::json!({ "running": name }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}
