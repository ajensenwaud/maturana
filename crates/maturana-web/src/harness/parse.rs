//! Pure parser for `codex exec --json` JSONL events → [`TurnEvent`]s.
//!
//! Schema captured from codex-cli 0.135.0 (fixtures under
//! `tests/fixtures/codex-exec-json/`):
//!
//! ```text
//! {"type":"thread.started","thread_id":"..."}
//! {"type":"turn.started"}
//! {"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"...","status":"in_progress"}}
//! {"type":"item.completed","item":{"id":"item_0","type":"command_execution","aggregated_output":"...","exit_code":0,"status":"completed"}}
//! {"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"..."}}
//! {"type":"turn.completed","usage":{"input_tokens":...,"output_tokens":...}}
//! ```
//!
//! Unknown event/item types pass through as [`TurnEvent::Item`] so newer codex
//! versions degrade to "rich JSON in the timeline" instead of breaking.

use crate::harness::TurnEvent;
use crate::ws::protocol::WirePhase;

/// Map one JSONL line to zero or more turn events. Returns an empty vec for
/// blank or non-JSON lines (codex occasionally logs plain text to stdout).
pub fn parse_codex_line(line: &str) -> Vec<TurnEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        // Not JSON: surface it as raw output rather than losing it.
        return vec![TurnEvent::Delta(format!("{line}\n"))];
    };
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match event_type {
        "thread.started" | "turn.started" => vec![TurnEvent::Item(event)],
        "item.started" | "item.updated" => item_phase(&event, false)
            .into_iter()
            .chain(std::iter::once(TurnEvent::Item(event.clone())))
            .collect(),
        "item.completed" => {
            let mut events = Vec::new();
            let item = event.get("item").cloned().unwrap_or_default();
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if item_type == "agent_message" {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    events.push(TurnEvent::Delta(format!("{text}\n")));
                }
            } else if let Some(phase) = item_phase(&event, true) {
                events.push(phase);
            }
            events.push(TurnEvent::Item(event));
            events
        }
        "turn.completed" => {
            let detail = event.get("usage").map(render_usage);
            vec![TurnEvent::Completed { ok: true, detail }]
        }
        "turn.failed" | "error" => {
            let detail = event
                .get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| event.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string);
            vec![TurnEvent::Completed { ok: false, detail }]
        }
        _ => vec![TurnEvent::Item(event)],
    }
}

/// Animation phase for a work item (anything that isn't the final message).
/// `item.started` → Running, `item.completed` → Done/Failed by exit code.
fn item_phase(event: &serde_json::Value, completed: bool) -> Option<TurnEvent> {
    let item = event.get("item")?;
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if item_type == "agent_message" {
        return None;
    }
    let span_id = item.get("id").and_then(|i| i.as_str())?.to_string();
    let tool = match item_type {
        "command_execution" => item
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| truncate(c, 80))
            .unwrap_or_else(|| "shell".to_string()),
        other => other.replace('_', " "),
    };
    let phase = if completed {
        let failed = item
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .map(|code| code != 0)
            .unwrap_or(false)
            || item.get("status").and_then(|s| s.as_str()) == Some("failed");
        if failed {
            WirePhase::Failed {
                detail: item
                    .get("exit_code")
                    .and_then(|c| c.as_i64())
                    .map(|c| format!("exit {c}")),
            }
        } else {
            WirePhase::Done { detail: None }
        }
    } else {
        WirePhase::Running { tool: tool.clone() }
    };
    Some(TurnEvent::Phase { span_id, phase })
}

fn render_usage(usage: &serde_json::Value) -> String {
    let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    format!("{input} in / {output} out tokens")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fixture(name: &str) -> Vec<TurnEvent> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-exec-json")
            .join(name);
        let raw = std::fs::read_to_string(path).unwrap();
        raw.lines().flat_map(parse_codex_line).collect()
    }

    #[test]
    fn simple_fixture_yields_message_and_completion() {
        let events = parse_fixture("simple.jsonl");
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::Delta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["READY\n"]);
        match events.last() {
            Some(TurnEvent::Completed { ok: true, detail: Some(detail) }) => {
                assert!(detail.contains("tokens"), "usage detail: {detail}");
            }
            other => panic!("expected successful completion, got {other:?}"),
        }
    }

    #[test]
    fn tooluse_fixture_yields_running_then_done_phases() {
        let events = parse_fixture("tooluse.jsonl");
        let phases: Vec<(&str, &WirePhase)> = events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::Phase { span_id, phase } => Some((span_id.as_str(), phase)),
                _ => None,
            })
            .collect();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].0, "item_0");
        match phases[0].1 {
            WirePhase::Running { tool } => assert!(tool.contains("echo maturana-fixture-ok")),
            other => panic!("expected running, got {other:?}"),
        }
        assert_eq!(phases[1].0, "item_0");
        assert!(matches!(phases[1].1, WirePhase::Done { .. }));
        // The final agent message still streams as a delta.
        assert!(events.iter().any(
            |e| matches!(e, TurnEvent::Delta(text) if text.contains("maturana-fixture-ok"))
        ));
    }

    #[test]
    fn failed_command_maps_to_failed_phase() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","command":"false","aggregated_output":"","exit_code":1,"status":"failed"}}"#;
        let events = parse_codex_line(line);
        assert!(events.iter().any(|e| matches!(
            e,
            TurnEvent::Phase { span_id, phase: WirePhase::Failed { detail: Some(d) } }
                if span_id == "item_3" && d == "exit 1"
        )));
    }

    #[test]
    fn turn_failure_and_garbage_lines_degrade_gracefully() {
        let events =
            parse_codex_line(r#"{"type":"turn.failed","error":{"message":"model unavailable"}}"#);
        assert!(matches!(
            events.as_slice(),
            [TurnEvent::Completed { ok: false, detail: Some(d) }] if d == "model unavailable"
        ));
        let raw = parse_codex_line("plain text noise");
        assert!(matches!(&raw[..], [TurnEvent::Delta(t)] if t == "plain text noise\n"));
        assert!(parse_codex_line("").is_empty());
        // Unknown event types pass through as items.
        let unknown = parse_codex_line(r#"{"type":"future.event","x":1}"#);
        assert!(matches!(&unknown[..], [TurnEvent::Item(_)]));
    }
}
