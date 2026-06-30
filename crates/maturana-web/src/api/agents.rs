//! Agent fleet API. Spec edits follow the safety flow: validate → dry-run →
//! explicit apply; the egress editor rewrites only the `network` block of the
//! spec frontmatter and re-validates before anything is written.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use maturana_core::spec::AgentSpec;
use maturana_core::state::MaturanaHome;
use maturana_core::validation::validate_spec;

use super::{blocking, err, ok};
use crate::state::AppState;

fn home(state: &AppState) -> MaturanaHome {
    MaturanaHome::new(state.home_root.clone())
}

fn agent_spec_path(state: &AppState, agent_id: &str) -> PathBuf {
    home(state).agent_dir(agent_id).join("MATURANA.md")
}

/// Reject ids that could traverse out of the agents directory.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

macro_rules! check_id {
    ($id:expr) => {
        if !valid_agent_id(&$id) {
            return err(StatusCode::BAD_REQUEST, "invalid agent id");
        }
    };
}

pub async fn list(State(state): State<AppState>) -> Response {
    let root = state.home_root.clone();
    match blocking(move || snapshot(&root)).await {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// One JSON snapshot of the fleet; shared by the REST list and the agents
/// dashboard topic poller.
pub(crate) fn snapshot(root: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let home = MaturanaHome::new(root.to_path_buf());
    Ok(serde_json::to_value(
        maturana_ops::agents::list_agent_summaries(&home)?,
    )?)
}

pub async fn status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        let status = maturana_ops::agents::inspect_live_agent(&home, &id)?;
        Ok(serde_json::to_value(status)?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn stop(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        maturana_ops::agents::stop_live_agent(&home, &id)?;
        Ok(serde_json::json!({ "stopped": id }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Human-readable agent summary for the cockpit: identity, harness, the
/// tools/skills/MCP/capabilities it actually carries, channels, egress size,
/// worker state, graph — i.e. "what it is and what it can do" instead of a raw
/// inspect dump.
pub async fn detail(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let spec_path = agent_spec_path(&state, &id);
    let dir = home(&state).agent_dir(&id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
        let worker: Option<serde_json::Value> =
            std::fs::read_to_string(dir.join("worker-status.json"))
                .ok()
                .and_then(|r| serde_json::from_str(&r).ok());
        let mut channels: Vec<&str> = Vec::new();
        let c = &spec.channels;
        if c.tui {
            channels.push("tui");
        }
        if c.telegram.is_some() {
            channels.push("telegram");
        }
        if c.discord.is_some() {
            channels.push("discord");
        }
        if c.slack.is_some() {
            channels.push("slack");
        }
        if c.agentmail.is_some() {
            channels.push("agentmail");
        }
        let mcp: Vec<String> = spec.mcp_servers.iter().map(|m| m.name.clone()).collect();
        Ok(serde_json::json!({
            "agent_id": id,
            "name": spec.identity.name,
            "purpose": spec.identity.purpose,
            "harness": maturana_core::worker::harness_name(&spec.runtime.harness),
            "provider": format!("{:?}", spec.vm.provider),
            "vcpu": spec.vm.vcpu,
            "memory_mib": spec.vm.memory_mib,
            "skills": spec.skills,
            "tools": spec.tools,
            "mcp_servers": mcp,
            "capabilities": spec.capabilities,
            "channels": channels,
            "knowledge_graph": spec.knowledge_graph.enabled,
            "graph_name": spec.knowledge_graph.enabled.then(|| spec.knowledge_graph.graph_name(&id)),
            "egress_allowlist": spec.network.egress_allowlist,
            "egress_allow_all": spec.network.egress_allow_all,
            "schedules": spec.schedules.len(),
            "worker_status": worker,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Restart a Firecracker agent: relaunch its microVM from the baked rootfs.
pub async fn restart(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        Ok(serde_json::to_value(
            maturana_ops::agents::restart_firecracker_agent(&home, &id)?,
        )?)
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct DeploySkillBody {
    skill: String,
}

/// Deploy a host-side skill into a running Firecracker agent's guest: copy the
/// skill's SKILL.md into /agent/skills over the SSH-pinned channel, via the
/// CLI's `deploy skill` command. The guest IP is read from the agent's spec, so
/// the operator only picks the skill — no manual IP/SSH details.
pub async fn deploy_skill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeploySkillBody>,
) -> Response {
    check_id!(id);
    let skill = body.skill.trim().to_string();
    if skill.is_empty() || skill.contains('/') || skill.contains('\\') || skill.contains("..") {
        return err(StatusCode::BAD_REQUEST, "invalid skill name");
    }
    let home_root = state.home_root.clone();
    let spec_path = agent_spec_path(&state, &id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
        let fc = spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("agent has no firecracker guest to deploy into"))?;
        let guest_ip = fc.guest_ip.clone();
        // Skills live at <repo root>/skills/<name>; the repo root is the parent of the home dir.
        let skills_dir = home_root
            .parent()
            .unwrap_or(home_root.as_path())
            .join("skills");
        let skill_path = skills_dir.join(&skill);
        if !skill_path.exists() {
            anyhow::bail!("skill not found at {}", skill_path.display());
        }
        let home = maturana_core::state::MaturanaHome::new(&home_root);
        let result = maturana_ops::deploy::deploy_item(
            &home,
            maturana_ops::deploy::DeployKind::Skill,
            maturana_ops::deploy::DeployRequest {
                agent_id: id.clone(),
                path: skill_path,
                ip: guest_ip,
                ssh_user: "ubuntu".to_string(),
                ssh_key: home_root.join("keys").join("maturana-agent-ed25519"),
                guest_path: None,
            },
        )?;
        Ok(serde_json::json!({
            "deployed": skill,
            "agent": id,
            "guest_path": result.guest_path,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn spec_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || Ok(std::fs::read_to_string(&path)?)).await {
        Ok(markdown) => ok(serde_json::json!({ "markdown": markdown })),
        Err(response) => response,
    }
}

// SECURITY: the agent's host directory holds SECRETS the zero-trust model keeps
// host-side — `state/` (sessiond + MaturanaGraph token VALUES, the guest SSH host
// PRIVATE key), `cloud-init/` (that key again, in user-data), `host-auth/`, the
// pipelock material, and the session DBs. The cockpit file view MUST NOT serve any
// of these to the browser. We default-deny: a file is browsable only if it is not
// under a sensitive directory, has no secret-hinting name, and has a known
// text/document extension. (A traversal guard alone is NOT enough — these paths
// are legitimately *inside* agent_dir.)

/// Directories never descended into or read from (secrets, auth, raw session DBs).
const SKIP_DIRS: &[&str] = &[
    "state",
    "cloud-init",
    "host-auth",
    "secrets",
    "pipelock",
    ".ssh",
    ".codex",
    ".claude",
    "sessions",
];
/// Only these extensions are previewable (no `.env`, `.pem`, `.key`, keys, sqlite…).
const SAFE_EXTS: &[&str] = &["md", "json", "txt", "yaml", "yml", "toml", "log", "csv"];
/// Filename substrings that mark a secret even with a safe extension.
const SECRET_HINTS: &[&str] = &[
    "secret",
    "token",
    "password",
    "credential",
    "private",
    "ed25519",
    "id_rsa",
];

/// Default-deny: is this relative path safe to list/read in the cockpit?
fn is_browsable_rel(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    if lower.split('/').any(|c| SKIP_DIRS.contains(&c)) {
        return false;
    }
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if SECRET_HINTS.iter().any(|h| name.contains(h)) {
        return false;
    }
    match std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some(ext) => SAFE_EXTS.contains(&ext),
        None => false,
    }
}

/// List the agent's HOST-side *document* files (spec, AGENTS.md, IDENTITY.md,
/// worker status) as a flat, relative tree. Read-only and default-deny: secret
/// state, host auth, and session DBs are never listed. The agent's in-VM working
/// files are isolated and are not exposed here.
pub async fn files(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let dir = home(&state).agent_dir(&id);
    match blocking(move || {
        let mut out = Vec::new();
        walk_files(&dir, &dir, 0, &mut out);
        out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(serde_json::json!(out))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

fn walk_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<serde_json::Value>,
) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        // Cap checked INSIDE the loop so one large directory can't blow up the
        // response.
        if out.len() >= 800 {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if path.is_dir() {
            // Never descend into secret/auth/session directories.
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_files(root, &path, depth + 1, out);
        } else if is_browsable_rel(&rel) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(serde_json::json!({ "path": rel, "dir": false, "size": size }));
        }
    }
}

#[derive(serde::Deserialize)]
pub struct FileQuery {
    path: String,
}

/// Read one previewable text file under the agent's host directory. Guards, in
/// order: no traversal, must be a browsable document (default-deny secrets), stays
/// within the dir (canonicalized), not a dir, size- and binary-capped.
pub async fn file_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Response {
    check_id!(id);
    let dir = home(&state).agent_dir(&id);
    let rel = q.path.clone();
    match blocking(move || {
        if rel.is_empty() || rel.contains("..") || rel.starts_with('/') || rel.starts_with('\\') {
            anyhow::bail!("invalid path");
        }
        // Default-deny: refuse secret state, host auth, keys, session DBs — these
        // live legitimately inside agent_dir, so the traversal guard alone misses
        // them.
        if !is_browsable_rel(&rel) {
            anyhow::bail!("not a browsable file");
        }
        let target = dir.join(&rel);
        let canon = target.canonicalize()?;
        let base = dir.canonicalize()?;
        if !canon.starts_with(&base) {
            anyhow::bail!("path escapes the agent directory");
        }
        let meta = std::fs::metadata(&canon)?;
        if meta.is_dir() {
            anyhow::bail!("that is a directory");
        }
        if meta.len() > 256 * 1024 {
            anyhow::bail!("file is too large to preview ({} bytes)", meta.len());
        }
        let bytes = std::fs::read(&canon)?;
        if bytes.iter().take(8000).any(|b| *b == 0) {
            anyhow::bail!("binary file — not previewable");
        }
        Ok(serde_json::json!({ "path": rel, "text": String::from_utf8_lossy(&bytes) }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Documents the generic file editor must NOT write: the spec has its own
/// validated edit flow (Agents panel), and worker-status.json is machine-written.
const NO_DIRECT_EDIT: &[&str] = &["MATURANA.md", "worker-status.json"];

#[derive(serde::Deserialize)]
pub struct FileWriteBody {
    path: String,
    text: String,
}

/// Write one editable text document under the agent's host directory (e.g.
/// AGENTS.md, SOUL.md, IDENTITY.md, notes). Guards mirror `file_read` — no
/// traversal, must be a browsable document (default-deny secrets), stays within
/// the dir — plus: refuse the spec + machine files, and cap the size. Creating a
/// new top-level doc is allowed (its parent is the agent dir, which already
/// exists and canonicalizes inside the base).
pub async fn file_write(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FileWriteBody>,
) -> Response {
    check_id!(id);
    let dir = home(&state).agent_dir(&id);
    let rel = body.path.clone();
    let text = body.text;
    match blocking(move || {
        if rel.is_empty() || rel.contains("..") || rel.starts_with('/') || rel.starts_with('\\') {
            anyhow::bail!("invalid path");
        }
        // Default-deny: same gate as reading — only browsable docs, never secrets.
        if !is_browsable_rel(&rel) {
            anyhow::bail!("not an editable document");
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if NO_DIRECT_EDIT.contains(&name) {
            anyhow::bail!("{name} is edited from the Agents panel, not here");
        }
        if text.len() > 512 * 1024 {
            anyhow::bail!("content too large ({} bytes)", text.len());
        }
        let base = dir.canonicalize()?;
        let target = dir.join(&rel);
        // Existing file → canonicalize it; new file → canonicalize its parent.
        // Either resolved path must stay inside the agent dir (no symlink escape).
        let probe = if target.exists() {
            target.clone()
        } else {
            target
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| dir.clone())
        };
        let canon = probe.canonicalize()?;
        if !canon.starts_with(&base) {
            anyhow::bail!("path escapes the agent directory");
        }
        if target.is_dir() {
            anyhow::bail!("that is a directory");
        }
        std::fs::write(&target, text.as_bytes())?;
        Ok(serde_json::json!({ "path": rel, "written": true, "size": text.len() }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Download any (non-secret) file under the agent's host directory as a binary
/// attachment — used for files an agent attaches to a chat reply (the
/// `{"text":…,"files":[…]}` outbound convention). Accepts an absolute host path
/// (how outbound files are recorded) OR a path relative to the agent dir; both
/// must canonicalize INSIDE the agent dir, and secret dirs/names are refused even
/// for an explicit download. Unlike the previewer, any extension is allowed.
pub async fn chat_download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Response {
    check_id!(id);
    let dir = home(&state).agent_dir(&id);
    let req = q.path.clone();
    let result = blocking(move || {
        if req.is_empty() || req.contains("..") {
            anyhow::bail!("invalid path");
        }
        let p = std::path::Path::new(&req);
        let target = if p.is_absolute() {
            p.to_path_buf()
        } else {
            dir.join(&req)
        };
        let canon = target.canonicalize()?;
        let base = dir.canonicalize()?;
        if !canon.starts_with(&base) {
            anyhow::bail!("path escapes the agent directory");
        }
        // Default-deny secrets even for an explicit download.
        let rel = canon
            .strip_prefix(&base)
            .unwrap_or(&canon)
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if rel.split('/').any(|c| SKIP_DIRS.contains(&c)) {
            anyhow::bail!("not a downloadable file");
        }
        let name = canon
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        if SECRET_HINTS
            .iter()
            .any(|h| name.to_ascii_lowercase().contains(h))
        {
            anyhow::bail!("not a downloadable file");
        }
        let meta = std::fs::metadata(&canon)?;
        if meta.is_dir() {
            anyhow::bail!("that is a directory");
        }
        if meta.len() > 50 * 1024 * 1024 {
            anyhow::bail!("file is too large to download ({} bytes)", meta.len());
        }
        let bytes = std::fs::read(&canon)?;
        Ok((name, bytes))
    })
    .await;
    match result {
        Ok((name, bytes)) => {
            let disp = format!(
                "attachment; filename=\"{}\"",
                name.replace(['"', '\n', '\r'], "")
            );
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

#[derive(serde::Deserialize)]
pub struct UploadQuery {
    name: String,
}

/// Receive a file attached in the chat composer (raw request body, filename in
/// `?name=`), store it under `<agent>/inbox/<stamp>-<name>`, then best-effort
/// ingest it into the agent's knowledge graph via the injected hook — the SAME
/// path a Telegram document upload takes, so the VM-isolated agent can retrieve
/// it. Returns the stored name + chunk count (or the ingest error, surfaced to
/// the operator rather than swallowed).
pub async fn chat_upload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    check_id!(id);
    let dir = home(&state).agent_dir(&id);
    let ingest = state.ingest.clone();
    let home_root = state.home_root.clone();
    let agent_id = id.clone();
    let raw_name = q.name.clone();
    match blocking(move || {
        if body.is_empty() {
            anyhow::bail!("empty upload");
        }
        if body.len() > 32 * 1024 * 1024 {
            anyhow::bail!("file is too large to upload ({} bytes)", body.len());
        }
        let base_name = raw_name.rsplit(['/', '\\']).next().unwrap_or("file");
        let safe: String = base_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = if safe.trim_matches('.').is_empty() {
            "upload.bin".to_string()
        } else {
            safe
        };
        let inbox = dir.join("inbox");
        std::fs::create_dir_all(&inbox)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let stored = format!("{stamp}-{safe}");
        let dest = inbox.join(&stored);
        std::fs::write(&dest, &body)?;
        let (ingested, ingest_error) = match &ingest {
            Some(f) => match f(&home_root, &agent_id, &dest) {
                Ok(chunks) => (Some(chunks), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            },
            None => (None, Some("graph ingest is not available".to_string())),
        };
        Ok(serde_json::json!({
            "name": safe,
            "stored": stored,
            "size": body.len(),
            "ingested_chunks": ingested,
            "ingest_error": ingest_error,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct CreateBody {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    purpose: String,
    #[serde(default = "default_harness")]
    harness: String,
}

fn default_harness() -> String {
    "codex".to_string()
}

/// Scaffold a NEW agent: write a minimal, validated starter spec to
/// `agents/<id>/MATURANA.md`. This only creates the declarative spec — no VM is
/// provisioned. The operator then refines the spec and runs dry-run → apply
/// (which does the real, slow materialize) from the Agents panel. Refuses to
/// clobber an existing agent.
pub async fn create(State(state): State<AppState>, Json(body): Json<CreateBody>) -> Response {
    let id = body.id.trim().to_string();
    check_id!(id);
    if !matches!(body.harness.as_str(), "codex" | "claude" | "opencode") {
        return err(
            StatusCode::BAD_REQUEST,
            "harness must be codex, claude, or opencode",
        );
    }
    let dir = home(&state).agent_dir(&id);
    let path = agent_spec_path(&state, &id);
    let name = if body.name.trim().is_empty() {
        id.clone()
    } else {
        body.name.trim().to_string()
    };
    let purpose = if body.purpose.trim().is_empty() {
        format!("{name} — a Maturana agent.")
    } else {
        body.purpose.trim().to_string()
    };
    // The UI uses the friendly harness names; the spec enum wants the canonical
    // ids (the Claude harness is `claude-code` in the spec).
    let spec_harness = match body.harness.as_str() {
        "claude" => "claude-code",
        other => other,
    }
    .to_string();
    match blocking(move || {
        if path.exists() {
            anyhow::bail!("agent '{id}' already exists");
        }
        let markdown = starter_spec(&id, &name, &purpose, &spec_harness);
        // Validate before committing anything to disk.
        let report = validate_markdown(&markdown)?;
        if !report.valid {
            anyhow::bail!(
                "starter spec failed validation: {}",
                report.errors.join("; ")
            );
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, &markdown)?;
        Ok(serde_json::json!({ "created": id, "report": report }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// A structurally-complete, parseable starter spec for a Firecracker agent.
/// Image paths follow the per-agent convention; the network identifiers
/// (tap/host_ip/guest_ip/guest_mac) are PLACEHOLDERS the operator must make
/// unique before provisioning — the collision guard refuses to launch a duplicate
/// anyway. Egress starts empty; harness defaults are applied on parse. The
/// operator broadens tools/skills/channels/egress from here.
fn starter_spec(id: &str, name: &str, purpose: &str, harness: &str) -> String {
    // tap_name must be <= 15 chars: "tap-mat-" (8) + up to 7 from the id.
    let tap_suffix: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(7)
        .collect();
    // SECURITY: build the frontmatter as data and let serde_yaml serialize it, so
    // a newline (or any YAML metacharacter) in `name`/`purpose` is escaped into a
    // string value — NOT interpolated as new sibling keys. String-formatting these
    // user inputs into YAML let a crafted `purpose` inject e.g. a `harness_auth`
    // block (host arbitrary-file read into the guest) or a `filesystem.mounts`.
    // Seed the egress allowlist with the harness's model API (+ github) so a new
    // agent can actually reach its backend, and pair it with the proxy below
    // (Firecracker egress is proxy-routed; a proxy with no allowlist is rejected,
    // and an allowlist with no proxy gets ConnectionRefused). The operator widens
    // the allowlist as needed.
    let egress: Vec<&str> = match harness {
        "claude-code" => vec!["api.anthropic.com", "platform.claude.com", "github.com"],
        "codex" => vec!["api.openai.com", "chatgpt.com", "github.com"],
        "opencode" => vec!["openrouter.ai", "github.com"],
        _ => vec!["github.com"],
    };
    let frontmatter = serde_json::json!({
        "identity": { "id": id, "name": name, "purpose": purpose },
        "runtime": { "harness": harness },
        "vm": {
            "provider": "firecracker",
            "guest_os": "linux",
            "vcpu": 2,
            "memory_mib": 2048,
            "firecracker": {
                "kernel_image": format!(".maturana/images/firecracker/{id}/vmlinux.bin"),
                "rootfs_image": format!(".maturana/images/firecracker/{id}/ubuntu-rootfs.ext4"),
                "tap_name": format!("tap-mat-{tap_suffix}"),
                "host_ip": "172.30.90.1",
                "guest_ip": "172.30.90.2",
                "guest_mac": "AA:FC:00:00:90:01",
                "kernel_args": "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5",
            },
        },
        // A Firecracker guest's egress is proxy-routed (the worker bakes
        // HTTP_PROXY=<host_ip>:47833), so a new agent needs the proxy block from the
        // start — without it the host launches no proxy and every turn fails with
        // ConnectionRefused. Bind tracks the (placeholder) host_ip.
        "network": {
            "egress_allowlist": egress,
            "proxy": { "enabled": true, "bind": "172.30.90.1:47833" },
        },
        "memory": { "wiki_path": ".maturana/wiki" },
        "knowledge_graph": { "enabled": true },
    });
    let yaml = serde_yaml::to_string(&frontmatter).unwrap_or_default();
    // The body is free markdown (not parsed as YAML), but flatten newlines in the
    // heading/system-prompt for tidiness.
    let name1 = name.replace(['\n', '\r'], " ");
    let purpose1 = purpose.replace(['\n', '\r'], " ");
    format!(
        "---\n{yaml}---\n\n# {name1}\n\n{purpose1}\n\n\
You are {name1}, running in an isolated Maturana microVM. Be concise and helpful.\n\n\
> Before provisioning: give this agent a UNIQUE tap_name / host_ip / guest_ip / guest_mac \
(the 172.30.90.x placeholders will collide with another agent) and set network.proxy.bind \
to <host_ip>:47833 to match, point kernel_image / rootfs_image at prepared images, then \
dry-run -> apply.\n"
    )
}

#[derive(serde::Deserialize)]
pub struct SpecBody {
    markdown: String,
}

/// Validate the submitted spec; write it only when the report is clean. The
/// report comes back either way so the UI can render it as the gate.
pub async fn spec_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SpecBody>,
) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let report = validate_markdown(&body.markdown)?;
        let written = report.valid;
        if written {
            std::fs::write(&path, &body.markdown)?;
        }
        Ok(serde_json::json!({ "report": report, "written": written }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn spec_validate(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SpecBody>,
) -> Response {
    check_id!(id);
    match blocking(move || Ok(serde_json::to_value(validate_markdown(&body.markdown)?)?)).await {
        Ok(report) => ok(serde_json::json!({ "report": report })),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct ApplyBody {
    #[serde(default)]
    dry_run: bool,
}

/// Materialize the agent's current on-disk spec. `dry_run: true` is the
/// default-safe preview; the UI requires it before offering the real apply.
pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ApplyBody>,
) -> Response {
    check_id!(id);
    let state_home = state.home_root.clone();
    match blocking(move || {
        let home = MaturanaHome::new(state_home);
        let path = home.agent_dir(&id).join("MATURANA.md");
        let raw = std::fs::read_to_string(&path)?;
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        let mode = if body.dry_run {
            maturana_core::materialize::LaunchMode::DryRun
        } else {
            maturana_core::materialize::LaunchMode::Apply
        };
        let result = maturana_core::materialize::materialize_agent(&spec, &raw, &home, mode)?;
        Ok(serde_json::json!({ "dry_run": body.dry_run, "result": result }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

pub async fn egress_get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        Ok(serde_json::json!({
            "egress_allowlist": spec.network.egress_allowlist,
            "egress_allow_all": spec.network.egress_allow_all,
            "inject_headers": spec.network.proxy.as_ref().map(|p| p.inject_headers.clone()).unwrap_or_default(),
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct EgressBody {
    egress_allowlist: Vec<String>,
    #[serde(default)]
    inject_headers: Vec<maturana_core::spec::NetworkProxyHeader>,
    /// Open egress toggle. Omitted → preserve the spec's current value; present →
    /// set `network.egress_allow_all` (removes the allowlist requirement).
    #[serde(default)]
    egress_allow_all: Option<bool>,
}

pub async fn egress_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<EgressBody>,
) -> Response {
    check_id!(id);
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let markdown = std::fs::read_to_string(&path)?;
        let updated = update_network_block(
            &markdown,
            &body.egress_allowlist,
            &body.inject_headers,
            body.egress_allow_all,
        )?;
        let report = validate_markdown(&updated)?;
        if !report.valid {
            anyhow::bail!(
                "edited spec failed validation: {}",
                report.errors.join("; ")
            );
        }
        std::fs::write(&path, &updated)?;
        Ok(serde_json::json!({ "report": report }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Spec sections the cockpit's Config panel may read/write directly. Editing is
/// confined to these declarative blocks — never the identity/vm/runtime that
/// define the agent's isolation.
pub const CONFIG_SECTIONS: &[&str] = &[
    "schedules",
    "mcp_servers",
    "channels",
    "skills",
    "tools",
    "capabilities",
];

#[derive(serde::Deserialize)]
pub struct ConfigQuery {
    section: String,
}

/// Read one config section of an agent's spec as JSON.
pub async fn config_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ConfigQuery>,
) -> Response {
    check_id!(id);
    if !CONFIG_SECTIONS.contains(&q.section.as_str()) {
        return err(StatusCode::BAD_REQUEST, "unknown config section");
    }
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let spec = AgentSpec::from_maturana_markdown(&path)?;
        let full = serde_json::to_value(&spec)?;
        Ok(serde_json::json!({
            "section": q.section,
            "value": full.get(&q.section).cloned().unwrap_or(serde_json::Value::Null),
            "editable": CONFIG_SECTIONS,
        }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
pub struct ConfigBody {
    section: String,
    value: serde_json::Value,
}

/// Replace one config section, re-validate the whole spec, and write it. A
/// running agent picks up channel/MCP/schedule changes on its next
/// materialize/restart (the spec is the source of truth, not live RPC).
pub async fn config_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ConfigBody>,
) -> Response {
    check_id!(id);
    if !CONFIG_SECTIONS.contains(&body.section.as_str()) {
        return err(StatusCode::BAD_REQUEST, "unknown config section");
    }
    let path = agent_spec_path(&state, &id);
    match blocking(move || {
        let markdown = std::fs::read_to_string(&path)?;
        let updated = update_spec_section(&markdown, &body.section, &body.value)?;
        let report = validate_markdown(&updated)?;
        if !report.valid {
            anyhow::bail!(
                "edited spec failed validation: {}",
                report.errors.join("; ")
            );
        }
        std::fs::write(&path, &updated)?;
        Ok(serde_json::json!({ "section": body.section, "report": report }))
    })
    .await
    {
        Ok(data) => ok(data),
        Err(response) => response,
    }
}

/// Replace ONE top-level frontmatter key with the given JSON value (null
/// removes it), preserving the rest of the spec byte-for-byte structurally.
pub fn update_spec_section(
    markdown: &str,
    section: &str,
    value: &serde_json::Value,
) -> anyhow::Result<String> {
    let rest = markdown
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("spec has no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is unterminated"))?;
    let frontmatter = &rest[..end];
    let body = &rest[end + 4..];
    let mut doc: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is not a mapping"))?;
    let key = serde_yaml::Value::String(section.to_string());
    if value.is_null() {
        map.remove(&key);
    } else {
        map.insert(key, serde_yaml::to_value(value)?);
    }
    let new_frontmatter = serde_yaml::to_string(&doc)?;
    Ok(format!("---\n{new_frontmatter}---{body}"))
}

fn validate_markdown(
    markdown: &str,
) -> anyhow::Result<maturana_core::validation::ValidationReport> {
    // from_maturana_markdown reads a file; validate in-memory via a temp file
    // so unsaved editor content can be checked without touching the spec.
    let tmp = std::env::temp_dir().join(format!("mweb-spec-{}.md", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, markdown)?;
    let parsed = AgentSpec::from_maturana_markdown(&tmp);
    let _ = std::fs::remove_file(&tmp);
    let spec = parsed?;
    Ok(validate_spec(&spec))
}

/// Rewrite ONLY the `network` block of the spec frontmatter, preserving every
/// other field. The frontmatter is YAML between the leading `---` fences; we
/// round-trip it as a `serde_yaml::Value` mapping (insertion order preserved)
/// rather than through `AgentSpec`, so unknown-to-us formatting like field
/// order survives.
pub fn update_network_block(
    markdown: &str,
    allowlist: &[String],
    headers: &[maturana_core::spec::NetworkProxyHeader],
    // `Some(v)` sets `network.egress_allow_all`; `None` preserves whatever the spec
    // already has (so an allowlist edit never silently flips the open-egress flag).
    allow_all: Option<bool>,
) -> anyhow::Result<String> {
    let rest = markdown
        .strip_prefix("---")
        .ok_or_else(|| anyhow::anyhow!("spec has no frontmatter"))?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is unterminated"))?;
    let frontmatter = &rest[..end];
    let body = &rest[end + 4..];

    let mut doc: serde_yaml::Value = serde_yaml::from_str(frontmatter)?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("spec frontmatter is not a mapping"))?;

    let network_key = serde_yaml::Value::String("network".to_string());
    let mut network = map
        .get(&network_key)
        .cloned()
        .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
    {
        let network_map = network
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("network block is not a mapping"))?;
        network_map.insert(
            serde_yaml::Value::String("egress_allowlist".to_string()),
            serde_yaml::to_value(allowlist)?,
        );
        if let Some(open) = allow_all {
            network_map.insert(
                serde_yaml::Value::String("egress_allow_all".to_string()),
                serde_yaml::Value::Bool(open),
            );
        }
        let proxy_key = serde_yaml::Value::String("proxy".to_string());
        if headers.is_empty() {
            // Leave an existing proxy block alone but clear its injections.
            if let Some(proxy) = network_map
                .get_mut(&proxy_key)
                .and_then(|p| p.as_mapping_mut())
            {
                proxy.insert(
                    serde_yaml::Value::String("inject_headers".to_string()),
                    serde_yaml::Value::Sequence(Vec::new()),
                );
            }
        } else {
            let mut proxy = network_map
                .get(&proxy_key)
                .cloned()
                .unwrap_or(serde_yaml::Value::Mapping(Default::default()));
            if let Some(proxy_map) = proxy.as_mapping_mut() {
                proxy_map.insert(
                    serde_yaml::Value::String("inject_headers".to_string()),
                    serde_yaml::to_value(headers)?,
                );
            }
            network_map.insert(proxy_key, proxy);
        }
    }
    map.insert(network_key, network);

    let new_frontmatter = serde_yaml::to_string(&doc)?;
    Ok(format!("---\n{new_frontmatter}---{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::spec::NetworkProxyHeader;

    const SPEC: &str = r#"---
identity:
  id: demo
  name: Demo
  purpose: Test agent for egress editing.
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 1024
network:
  egress_allowlist:
    - api.openai.com
memory:
  wiki_path: .maturana/wiki
---

# Demo Agent
body text stays put
"#;

    #[test]
    fn egress_rewrite_round_trips_and_preserves_other_fields() {
        let headers = vec![NetworkProxyHeader {
            host: "api.search.brave.com".into(),
            header: "X-Subscription-Token".into(),
            source: "pipelock:brave/api-key".into(),
            prefix: None,
        }];
        let allowlist = vec![
            "api.openai.com".to_string(),
            "api.search.brave.com".to_string(),
        ];
        let updated = update_network_block(SPEC, &allowlist, &headers, None).unwrap();

        // Re-parse: the edited model holds, the rest is untouched.
        let tmp = std::env::temp_dir().join(format!("mweb-egress-{}.md", std::process::id()));
        std::fs::write(&tmp, &updated).unwrap();
        let spec = AgentSpec::from_maturana_markdown(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        // Parsing a codex spec now applies the harness egress defaults
        // (auth.openai.com, chatgpt.com via apply_egress_defaults), so assert the
        // explicitly-set hosts are present rather than exact set equality.
        for host in &allowlist {
            assert!(
                spec.network.egress_allowlist.contains(host),
                "egress allowlist missing {host}: {:?}",
                spec.network.egress_allowlist
            );
        }
        let proxy = spec.network.proxy.unwrap();
        assert_eq!(proxy.inject_headers.len(), 1);
        assert_eq!(proxy.inject_headers[0].host, "api.search.brave.com");
        assert_eq!(spec.identity.id, "demo");
        assert_eq!(spec.identity.purpose, "Test agent for egress editing.");
        assert_eq!(spec.memory.wiki_path.as_deref(), Some(".maturana/wiki"));
        assert!(updated.contains("body text stays put"));

        // Idempotent: applying the same edit again changes nothing.
        let again = update_network_block(&updated, &allowlist, &headers, None).unwrap();
        assert_eq!(again, updated);
    }

    #[test]
    fn egress_allow_all_round_trips_through_network_block() {
        // Setting the flag writes egress_allow_all: true and parses back true; a
        // later edit with None preserves it (an allowlist edit can't silently
        // disable open egress).
        let opened =
            update_network_block(SPEC, &["api.openai.com".to_string()], &[], Some(true)).unwrap();
        assert!(opened.contains("egress_allow_all: true"), "{opened}");
        let tmp = std::env::temp_dir().join(format!("mweb-allowall-{}.md", std::process::id()));
        std::fs::write(&tmp, &opened).unwrap();
        let spec = AgentSpec::from_maturana_markdown(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        assert!(spec.network.egress_allow_all);
        // None preserves the existing true.
        let preserved =
            update_network_block(&opened, &["api.openai.com".to_string()], &[], None).unwrap();
        assert!(preserved.contains("egress_allow_all: true"), "{preserved}");
        // Some(false) turns it back off.
        let closed =
            update_network_block(&opened, &["api.openai.com".to_string()], &[], Some(false))
                .unwrap();
        assert!(closed.contains("egress_allow_all: false"), "{closed}");
    }

    #[test]
    fn egress_rewrite_rejects_specs_without_frontmatter() {
        assert!(update_network_block("# no frontmatter", &[], &[], None).is_err());
    }

    #[test]
    fn agent_id_validation_blocks_traversal() {
        assert!(valid_agent_id("codex-firecracker"));
        for bad in ["", "..", "a/b", "a\\b", "x y"] {
            assert!(!valid_agent_id(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn starter_spec_parses_and_validates() {
        // The scaffold must produce a structurally-valid spec (nested firecracker
        // block at the right YAML depth) for every harness, or "add agent" writes
        // a spec the fleet can't read.
        for harness in ["codex", "claude-code", "opencode"] {
            let md = starter_spec("new-bot", "New Bot", "A scaffolded agent.", harness);
            let tmp = std::env::temp_dir()
                .join(format!("mweb-starter-{harness}-{}.md", std::process::id()));
            std::fs::write(&tmp, &md).unwrap();
            let spec = AgentSpec::from_maturana_markdown(&tmp).expect("starter spec must parse");
            let _ = std::fs::remove_file(&tmp);
            assert_eq!(spec.identity.id, "new-bot");
            let report = validate_spec(&spec);
            assert!(
                report.valid,
                "starter spec for {harness} must validate: {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn starter_spec_resists_yaml_injection() {
        // A crafted `purpose` that tries to inject a sibling `harness_auth` block
        // (which would read a host file into the guest) must NOT become a real key —
        // it must round-trip as a plain string value of `purpose`.
        let evil =
            "ok\nharness_auth:\n- runtime: codex\n  source_path: /etc/shadow\n  guest_path: /tmp/x";
        let md = starter_spec("bot", "Bot", evil, "codex");
        let tmp = std::env::temp_dir().join(format!("mweb-inj-{}.md", std::process::id()));
        std::fs::write(&tmp, &md).unwrap();
        let spec = AgentSpec::from_maturana_markdown(&tmp).expect("must still parse");
        let _ = std::fs::remove_file(&tmp);
        // The injected block did not take effect: harness_auth stays empty, and the
        // payload is contained verbatim in purpose.
        assert!(
            spec.harness_auth.is_empty(),
            "injection created harness_auth entries"
        );
        assert!(
            spec.identity.purpose.contains("/etc/shadow"),
            "purpose lost the literal text"
        );
    }

    #[test]
    fn file_browser_denies_secrets_allows_docs() {
        // The confirmed HIGH leaks: state tokens, the SSH host private key, cloud-init.
        for deny in [
            "state/sessiond.env",
            "state/proxy.env",
            "state/ssh_host_ed25519",
            "cloud-init/user-data",
            "host-auth/codex/auth.json",
            "secrets/whatever.json",
            ".ssh/id_rsa",
            "graph-token.json",
            "MATURANA.md/../state/sessiond.env",
        ] {
            assert!(!is_browsable_rel(deny), "should DENY {deny}");
        }
        // The user-facing identity/contract docs stay browsable.
        for allow in [
            "MATURANA.md",
            "AGENTS.md",
            "IDENTITY.md",
            "worker-status.json",
        ] {
            assert!(is_browsable_rel(allow), "should ALLOW {allow}");
        }
    }

    #[test]
    fn file_write_allows_prose_docs_denies_spec_and_secrets() {
        // The generic editor may write prose docs (AGENTS.md, SOUL.md, …) but never
        // the validated spec, machine files, or anything the read guard denies.
        let editable = |rel: &str| {
            let name = rel.rsplit('/').next().unwrap_or(rel);
            is_browsable_rel(rel) && !NO_DIRECT_EDIT.contains(&name)
        };
        for ok in ["AGENTS.md", "SOUL.md", "IDENTITY.md", "notes/todo.md"] {
            assert!(editable(ok), "should allow editing {ok}");
        }
        for deny in [
            "MATURANA.md",
            "worker-status.json",
            "state/sessiond.env",
            ".ssh/id_rsa",
            "secrets/x.json",
            "host-auth/codex/auth.json",
        ] {
            assert!(!editable(deny), "should refuse editing {deny}");
        }
    }
}
