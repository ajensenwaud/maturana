//! Maturana web cockpit: a browser-based control surface that complements the
//! Codex CLI control plane (it never replaces it — both drive the same
//! `AGENTS.md` + `skills/` contract).
//!
//! This is deliberately the only crate in the workspace with an async runtime.
//! Everything below it (maturana-core, the platform services) stays sync; core
//! calls are wrapped in `spawn_blocking`. The "host never calls model APIs"
//! invariant applies to platform services — the cockpit is the *operator's*
//! seat, so spawning `codex exec` here automates the existing human workflow.

mod api;
mod assets;
mod auth;
mod harness;
mod server;
mod state;
mod ws;

pub use state::{EnqueueTurnFn, IngestFileFn};

use std::path::Path;
use std::path::PathBuf;

/// Run the cockpit server, blocking the calling (sync) thread. The CLI calls
/// this directly; the tokio runtime lives entirely inside. `enqueue` is the shared
/// channel front door (the CLI owns the context builder), so the cockpit routes
/// turns through the SAME path as every other channel. `ingest` is an optional
/// hook that pushes chat-uploaded files into the agent's knowledge graph.
pub fn run_web(
    home_root: PathBuf,
    bind: &str,
    enqueue: EnqueueTurnFn,
    ingest: Option<IngestFileFn>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(server::serve(home_root, bind, enqueue, ingest))
}

/// Read the cockpit login token (`<home>/web/token`), generating it on first
/// use. Exposed so `maturana web token` can print it without starting the
/// server.
pub fn login_token(home_root: &Path) -> anyhow::Result<String> {
    auth::ensure_web_token(home_root)
}
