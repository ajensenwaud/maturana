//! Maturana web cockpit: a browser-based control surface that complements the
//! Codex CLI control plane (it never replaces it — both drive the same
//! `AGENTS.md` + `skills/` contract).
//!
//! This is deliberately the only crate in the workspace with an async runtime.
//! Everything below it (maturana-core, the platform services) stays sync; core
//! calls are wrapped in `spawn_blocking`. The "host never calls model APIs"
//! invariant applies to platform services — the cockpit is the *operator's*
//! seat, so spawning `codex exec` here automates the existing human workflow.

mod assets;
mod auth;
mod harness;
mod server;
mod state;
mod ws;

use std::path::PathBuf;

/// Run the cockpit server, blocking the calling (sync) thread. The CLI calls
/// this directly; the tokio runtime lives entirely inside.
pub fn run_web(home_root: PathBuf, bind: &str) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(server::serve(home_root, bind))
}
