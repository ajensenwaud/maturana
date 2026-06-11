//! Default harness: `codex exec --json` — the operator's Codex subscription,
//! oriented by the maturana repo's AGENTS.md + skills exactly like an
//! interactive CLI session.

use tokio::process::Command;
use tokio::sync::mpsc;

use crate::harness::{parse, spawn_streaming, HarnessAdapter, TurnEvent, TurnHandle, TurnRequest};
use crate::ws::protocol::HarnessKind;

pub struct CodexExecAdapter;

/// Resolve the codex program. On Windows the npm `codex` command is a
/// PowerShell/cmd shim that `CreateProcess` cannot spawn directly; prefer the
/// native exe the npm package vendors, then the `.cmd` shim (which Rust can
/// spawn when named with its extension).
pub fn codex_program() -> String {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let npm = std::path::Path::new(&appdata).join("npm");
            for arch_pkg in ["codex-win32-x64", "codex-win32-arm64"] {
                let vendor = npm
                    .join("node_modules/@openai/codex/node_modules/@openai")
                    .join(arch_pkg)
                    .join("vendor");
                if let Ok(entries) = std::fs::read_dir(&vendor) {
                    for entry in entries.flatten() {
                        let native = entry.path().join("bin").join("codex.exe");
                        if native.exists() {
                            return native.display().to_string();
                        }
                    }
                }
            }
        }
        "codex.cmd".to_string()
    }
    #[cfg(not(windows))]
    {
        "codex".to_string()
    }
}

impl CodexExecAdapter {
    /// The argv after the program name; split out for testing.
    pub fn args(request: &TurnRequest) -> Vec<String> {
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
        ];
        if let Some(model) = &request.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args.push(request.text.clone());
        args
    }
}

impl HarnessAdapter for CodexExecAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn start_turn(
        &self,
        request: TurnRequest,
        tx: mpsc::Sender<TurnEvent>,
    ) -> anyhow::Result<TurnHandle> {
        let mut command = Command::new(codex_program());
        command.args(Self::args(&request)).current_dir(&request.cwd);
        spawn_streaming(command, request.turn_id, tx, parse::parse_codex_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn codex_argv_shape() {
        let request = TurnRequest {
            turn_id: "t".into(),
            text: "list the agents".into(),
            model: None,
            cwd: PathBuf::from("."),
            home_root: PathBuf::from("./.maturana"),
        };
        assert_eq!(
            CodexExecAdapter::args(&request),
            vec!["exec", "--json", "--skip-git-repo-check", "list the agents"]
        );
        let with_model = TurnRequest {
            model: Some("o4-mini".into()),
            ..request
        };
        let args = CodexExecAdapter::args(&with_model);
        assert!(args.windows(2).any(|w| w == ["--model", "o4-mini"]));
        // The prompt is always the final positional argument.
        assert_eq!(args.last().map(String::as_str), Some("list the agents"));
    }
}
