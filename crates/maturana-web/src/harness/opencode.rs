//! OpenRouter harness: `opencode run -m openrouter/<model>` — the pluggable
//! bring-your-own-model path, mirroring the guest worker's existing
//! OPENROUTER_API_KEY + opencode precedent. opencode has no JSONL event
//! stream, so output streams as plain deltas under one synthetic phase card.

use tokio::process::Command;
use tokio::sync::mpsc;

use crate::harness::{spawn_streaming, HarnessAdapter, TurnEvent, TurnHandle, TurnRequest};

pub const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4.5";
const API_KEY_SECRET: &str = "openrouter/api-key";

pub struct OpencodeAdapter;

impl OpencodeAdapter {
    pub fn args(request: &TurnRequest) -> Vec<String> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_OPENROUTER_MODEL.to_string());
        vec![
            "run".to_string(),
            "-m".to_string(),
            format!("openrouter/{model}"),
            request.text.clone(),
        ]
    }
}

impl HarnessAdapter for OpencodeAdapter {
    fn start_turn(
        &self,
        request: TurnRequest,
        tx: mpsc::Sender<TurnEvent>,
    ) -> anyhow::Result<TurnHandle> {
        // The key lives in pipelock and is injected into the child env only —
        // it never reaches the browser or the WS stream.
        let vault = maturana_core::pipelock::PipelockVault::new(request.home_root.join("pipelock"));
        let api_key = vault.get(API_KEY_SECRET).map_err(|_| {
            anyhow::anyhow!(
                "OpenRouter key missing: `maturana pipelock set {API_KEY_SECRET} <key>` first"
            )
        })?;
        // Same Windows shim consideration as codex: spawn the .cmd by name.
        // On Unix resolve the absolute path so a minimal systemd --user PATH
        // doesn't ENOENT (same fix as codex_program).
        let program = if cfg!(windows) {
            "opencode.cmd".to_string()
        } else {
            crate::harness::resolve_program("opencode")
        };
        let mut command = Command::new(program);
        command
            .args(Self::args(&request))
            .current_dir(&request.cwd)
            .env("OPENROUTER_API_KEY", api_key);
        spawn_streaming(command, request.turn_id, tx, plain_delta)
    }
}

fn plain_delta(line: &str) -> Vec<TurnEvent> {
    vec![TurnEvent::Delta(format!("{line}\n"))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn opencode_argv_defaults_and_overrides_model() {
        let request = TurnRequest {
            turn_id: "t".into(),
            text: "hello".into(),
            model: None,
            cwd: PathBuf::from("."),
            home_root: PathBuf::from("./.maturana"),
        };
        assert_eq!(
            OpencodeAdapter::args(&request),
            vec![
                "run",
                "-m",
                &format!("openrouter/{DEFAULT_OPENROUTER_MODEL}"),
                "hello"
            ]
        );
        let custom = TurnRequest {
            model: Some("meta-llama/llama-3.3-70b-instruct".into()),
            ..request
        };
        assert_eq!(
            OpencodeAdapter::args(&custom)[2],
            "openrouter/meta-llama/llama-3.3-70b-instruct"
        );
    }
}
