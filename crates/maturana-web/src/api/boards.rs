//! Durable orchestration boards: define cards (title, assignee, deps), run them
//! across agents, and monitor live. The board engine + dispatcher live in the
//! CLI (`maturana board …`, runs each card in its assignee's VM over A2A); this
//! API edits the typed `maturana_core::board::Board` store directly and triggers
//! a run by shelling out to the binary detached — the cockpit never becomes a
//! second, weaker execution path.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
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
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    scheduled_at: Option<String>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    goal: Option<bool>,
    #[serde(default)]
    goal_max_turns: Option<u32>,
    #[serde(default)]
    triage: Option<bool>,
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
        let scheduled = match body.scheduled_at.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| anyhow::anyhow!("invalid scheduled_at (use RFC3339): {e}"))?
                    .with_timezone(&chrono::Utc),
            ),
            None => None,
        };
        let id = board.add(title, body.detail.as_deref().unwrap_or("").trim(), assignee, deps);
        if let Some(c) = board.card_mut(&id) {
            if let Some(p) = body.priority {
                c.priority = p;
            }
            c.tenant = body.tenant.and_then(|t| {
                let t = t.trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            });
            c.scheduled_at = scheduled;
            c.max_retries = body.max_retries.unwrap_or(0);
            c.goal = body.goal.unwrap_or(false);
            c.goal_max_turns = body.goal_max_turns.unwrap_or(0);
            if body.triage.unwrap_or(false) {
                c.status = maturana_core::board::CardStatus::Triage;
            }
        }
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
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    scheduled_at: Option<String>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    goal: Option<bool>,
    #[serde(default)]
    goal_max_turns: Option<u32>,
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
        let scheduled = match body.scheduled_at.as_deref().map(str::trim) {
            Some("") => Some(None),               // explicit clear
            Some(s) => Some(Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .map_err(|e| anyhow::anyhow!("invalid scheduled_at: {e}"))?
                    .with_timezone(&chrono::Utc),
            )),
            None => None,                          // leave unchanged
        };
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
            if let Some(p) = body.priority {
                card.priority = p;
            }
            if let Some(t) = body.tenant {
                let t = t.trim().to_string();
                card.tenant = if t.is_empty() { None } else { Some(t) };
            }
            if let Some(s) = scheduled {
                card.scheduled_at = s;
            }
            if let Some(r) = body.max_retries {
                card.max_retries = r;
            }
            if let Some(g) = body.goal {
                card.goal = g;
            }
            if let Some(gt) = body.goal_max_turns {
                card.goal_max_turns = gt;
            }
            card.updated_at = Some(chrono::Utc::now());
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

/// Tail of the detached `board run` process's stdout+stderr (board/<name>.run.log).
/// The run is fire-and-forget, and failures like "no running agents to reuse" or
/// "nothing ready" never reach the board event log — this is how the cockpit
/// surfaces WHY a run did nothing.
pub async fn run_log(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    match blocking(move || {
        let path = Board::dir(&h).join(format!("{name}.run.log"));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(120);
        Ok(serde_json::json!({ "log": lines[start..].join("\n") }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct CommentBody {
    #[serde(default)]
    author: Option<String>,
    body: String,
}

/// Append a comment to a card's thread.
pub async fn comment_card(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    Json(body): Json<CommentBody>,
) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let h = home(&state);
    match blocking(move || {
        let text = body.body.trim();
        if text.is_empty() {
            anyhow::bail!("empty comment");
        }
        let author = body.author.as_deref().map(str::trim).filter(|a| !a.is_empty()).unwrap_or("operator");
        let mut board = Board::load(&h, &name)?;
        if !board.comment(&id, author, text) {
            anyhow::bail!("no such card");
        }
        board.save(&h)?;
        Ok(serde_json::json!({ "commented": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Spawn a detached `maturana board <args>` (decompose/specify run in the
/// background; the board JSON + run log update as they progress).
fn spawn_board(home_root: &std::path::Path, args: &[String], log: &std::path::Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let logf = std::fs::File::create(log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home").arg(home_root).args(args)
        .stdin(std::process::Stdio::null())
        .stderr(logf.try_clone()?)
        .stdout(logf);
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Decompose a card into children via the coordinator agent (LLM) — detached.
pub async fn decompose(State(state): State<AppState>, Path((name, id)): Path<(String, String)>) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let home_root = state.home_root.clone();
    let h = home(&state);
    match blocking(move || {
        let board = Board::load(&h, &name)?;
        if board.card(&id).is_none() {
            anyhow::bail!("no such card");
        }
        let log = Board::dir(&h).join(format!("{name}.decompose.log"));
        spawn_board(
            &home_root,
            &["board".into(), "decompose".into(), id.clone(), "--board".into(), name.clone()],
            &log,
        )?;
        Ok(serde_json::json!({ "decomposing": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Flesh out a card via an agent (LLM) — detached.
pub async fn specify(State(state): State<AppState>, Path((name, id)): Path<(String, String)>) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let home_root = state.home_root.clone();
    let h = home(&state);
    match blocking(move || {
        let board = Board::load(&h, &name)?;
        if board.card(&id).is_none() {
            anyhow::bail!("no such card");
        }
        let log = Board::dir(&h).join(format!("{name}.specify.log"));
        spawn_board(
            &home_root,
            &["board".into(), "specify".into(), id.clone(), "--board".into(), name.clone()],
            &log,
        )?;
        Ok(serde_json::json!({ "specifying": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct RenameBody {
    name: String,
}

/// Rename a board (moves its JSON + run log; cards' absolute attachment paths
/// are unaffected).
pub async fn rename_board(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RenameBody>,
) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let new = body.name.trim().to_string();
    if !valid_id(&new) {
        return err(StatusCode::BAD_REQUEST, "invalid new board name");
    }
    let h = home(&state);
    match blocking(move || {
        if !Board::path(&h, &name).exists() {
            anyhow::bail!("no such board");
        }
        if Board::path(&h, &new).exists() {
            anyhow::bail!("a board named '{new}' already exists");
        }
        let mut board = Board::load(&h, &name)?;
        board.name = new.clone();
        board.save(&h)?;
        let _ = std::fs::remove_file(Board::path(&h, &name));
        // Move the run log if present.
        let old_events = Board::dir(&h).join(format!("{name}.events.jsonl"));
        let new_events = Board::dir(&h).join(format!("{new}.events.jsonl"));
        let _ = std::fs::rename(old_events, new_events);
        Ok(serde_json::json!({ "renamed": new }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn attachments_dir(h: &MaturanaHome, board: &str, card: &str) -> std::path::PathBuf {
    Board::dir(h).join("attachments").join(board).join(card)
}

#[derive(serde::Deserialize)]
pub struct AttachQuery {
    name: String,
}

/// Upload a file attached to a card (raw body). Stored host-side; the dispatcher
/// inlines small text attachments into the worker's prompt.
pub async fn upload_attachment(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    Query(q): Query<AttachQuery>,
    bytes: Bytes,
) -> Response {
    if !valid_id(&name) || !valid_id(&id) {
        return err(StatusCode::BAD_REQUEST, "invalid id");
    }
    let h = home(&state);
    let raw_name = q.name.clone();
    match blocking(move || {
        if bytes.is_empty() {
            anyhow::bail!("empty upload");
        }
        if bytes.len() > 25 * 1024 * 1024 {
            anyhow::bail!("attachment too large (25 MB max)");
        }
        let mut board = Board::load(&h, &name)?;
        if board.card(&id).is_none() {
            anyhow::bail!("no such card");
        }
        let base = raw_name.rsplit(['/', '\\']).next().unwrap_or("file");
        let safe: String = base
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
            .collect();
        let safe = if safe.trim_matches('.').is_empty() { "upload.bin".to_string() } else { safe };
        let dir = attachments_dir(&h, &name, &id);
        std::fs::create_dir_all(&dir)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let dest = dir.join(format!("{stamp}-{safe}"));
        std::fs::write(&dest, &bytes)?;
        let path = dest.to_string_lossy().to_string();
        if let Some(c) = board.card_mut(&id) {
            c.attachments.push(path.clone());
        }
        board.save(&h)?;
        Ok(serde_json::json!({ "name": safe, "path": path, "size": bytes.len() }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct DownloadQuery {
    path: String,
}

/// Download a card attachment (guarded to the board attachments tree).
pub async fn download_attachment(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    if !valid_id(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid board name");
    }
    let h = home(&state);
    let req = q.path.clone();
    let result = blocking(move || {
        if req.is_empty() || req.contains("..") {
            anyhow::bail!("invalid path");
        }
        let canon = std::path::Path::new(&req).canonicalize()?;
        let base = Board::dir(&h).join("attachments").canonicalize()?;
        if !canon.starts_with(&base) {
            anyhow::bail!("path escapes the attachments directory");
        }
        let meta = std::fs::metadata(&canon)?;
        if meta.is_dir() {
            anyhow::bail!("that is a directory");
        }
        if meta.len() > 50 * 1024 * 1024 {
            anyhow::bail!("file too large to download");
        }
        let fname = canon.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let bytes = std::fs::read(&canon)?;
        Ok((fname, bytes))
    })
    .await;
    match result {
        Ok((fname, bytes)) => {
            let disp = format!("attachment; filename=\"{}\"", fname.replace(['"', '\n', '\r'], ""));
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_DISPOSITION, disp),
                ],
                bytes,
            )
                .into_response()
        }
        Err(response) => response,
    }
}
