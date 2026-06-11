//! Harness adapters: how the cockpit's prompt console actually runs a turn.
//!
//! Both adapters spawn a child process — `codex exec --json` (default, uses
//! the operator's Codex subscription) or `opencode run -m openrouter/<model>`
//! (the pluggable OpenRouter path, reusing the same precedent as the guest
//! worker). One process-spawning shape means one cancellation story: kill the
//! child's whole process tree on cancel or socket drop.

pub mod codex;
pub mod opencode;
pub mod parse;

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::ws::protocol::{HarnessKind, WirePhase};

/// Events produced by a running turn, forwarded to the WebSocket as
/// `TurnDelta` / `TurnPhase` / `TurnItem` / `TurnCompleted`.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    Delta(String),
    Phase { span_id: String, phase: WirePhase },
    Item(serde_json::Value),
    Completed { ok: bool, detail: Option<String> },
}

#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub turn_id: String,
    pub text: String,
    pub model: Option<String>,
    /// Working directory for the child — the maturana repo root (the parent
    /// of the home dir by convention), so `AGENTS.md` + `skills/` orient the
    /// harness exactly like an interactive CLI session.
    pub cwd: PathBuf,
    /// The `.maturana` home root (pipelock lives here).
    pub home_root: PathBuf,
}

/// Handle to a running turn: dropping it does nothing; call `cancel()` to
/// kill the child's process tree.
pub struct TurnHandle {
    pub turn_id: String,
    pub(crate) pid: Option<u32>,
    pub(crate) child_kill: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TurnHandle {
    pub fn cancel(mut self) {
        // Tree first: once the direct child dies its children re-parent to
        // init and the descendant walk can no longer find them.
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
        if let Some(kill) = self.child_kill.take() {
            let _ = kill.send(());
        }
    }
}

pub trait HarnessAdapter: Send + Sync {
    fn kind(&self) -> HarnessKind;
    fn start_turn(
        &self,
        request: TurnRequest,
        tx: mpsc::Sender<TurnEvent>,
    ) -> anyhow::Result<TurnHandle>;
}

pub fn adapter_for(kind: &HarnessKind) -> Box<dyn HarnessAdapter> {
    match kind {
        HarnessKind::Codex => Box::new(codex::CodexExecAdapter),
        HarnessKind::Openrouter => Box::new(opencode::OpencodeAdapter),
    }
}

/// Spawn `command`, stream stdout lines through `map_line`, forward stderr
/// tails into the completion detail on failure, and emit a synthetic
/// completion if the parser never produced one. Shared by both adapters.
pub(crate) fn spawn_streaming(
    mut command: Command,
    turn_id: String,
    tx: mpsc::Sender<TurnEvent>,
    map_line: fn(&str) -> Vec<TurnEvent>,
) -> anyhow::Result<TurnHandle> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child: Child = command.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();
        let mut stderr_tail: Vec<String> = Vec::new();
        let mut completed = false;
        loop {
            tokio::select! {
                _ = &mut kill_rx => {
                    let _ = child.kill().await;
                    let _ = tx.send(TurnEvent::Completed {
                        ok: false,
                        detail: Some("cancelled".to_string()),
                    }).await;
                    return;
                }
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            for event in map_line(&line) {
                                if matches!(event, TurnEvent::Completed { .. }) {
                                    completed = true;
                                }
                                if tx.send(event).await.is_err() {
                                    let _ = child.kill().await;
                                    return;
                                }
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                err = err_lines.next_line() => {
                    if let Ok(Some(line)) = err {
                        stderr_tail.push(line);
                        if stderr_tail.len() > 10 {
                            stderr_tail.remove(0);
                        }
                    }
                }
            }
        }
        // Stdout closed: reap the child and finish the turn if the stream
        // never carried an explicit completion event.
        let status = child.wait().await;
        if !completed {
            let ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
            let detail = if ok {
                None
            } else if stderr_tail.is_empty() {
                status.ok().map(|s| s.to_string())
            } else {
                Some(stderr_tail.join("\n"))
            };
            let _ = tx.send(TurnEvent::Completed { ok, detail }).await;
        }
    });

    Ok(TurnHandle {
        turn_id,
        pid,
        child_kill: Some(kill_tx),
    })
}

/// Kill the whole process tree rooted at `pid` — harness children spawn their
/// own subprocesses (shells, tools) that must not outlive a cancelled turn.
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // Two passes: signal the spawn-time process group, then walk the
        // descendant tree depth-first — harness sandboxes (codex) re-group
        // the shell commands they run, so the group signal alone leaves
        // grandchildren behind.
        let script = format!(
            "kill -TERM -{pid} 2>/dev/null; \
             kill_tree() {{ for c in $(pgrep -P \"$1\" 2>/dev/null); do kill_tree \"$c\"; done; \
             kill -TERM \"$1\" 2>/dev/null; }}; kill_tree {pid}"
        );
        let _ = std::process::Command::new("sh")
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
