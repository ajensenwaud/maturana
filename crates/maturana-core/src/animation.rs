//! OpenClaw-style progress animation for long-running agent actions surfaced
//! over chat channels (Telegram).
//!
//! A chat channel cannot stream, so progress is shown by editing a single
//! status message in place. This module is the pure, testable core: given a
//! [`Phase`] and a tick counter it renders the exact message text. The channel
//! layer owns the side effects (send once, then `editMessageText` on each
//! tick), keeping all the wording and frame logic here under test.

/// What the agent is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting to start (e.g. queued behind another turn).
    Queued,
    /// Compiling a tool the agent just authored.
    Building { tool: String },
    /// Executing a tool / running the harness turn.
    Running { tool: String },
    /// Finished successfully; `detail` is an optional suffix (e.g. timing).
    Done { detail: Option<String> },
    /// Finished with an error.
    Failed { detail: Option<String> },
}

/// Braille spinner frames — the same eight-step cycle OpenClaw uses.
pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Render the status line for a phase at a given tick.
pub fn frame(phase: &Phase, tick: usize) -> String {
    let spin = SPINNER[tick % SPINNER.len()];
    match phase {
        Phase::Queued => format!("{spin} Queued…"),
        Phase::Building { tool } => format!("{spin} 🔨 Building `{tool}`…"),
        Phase::Running { tool } => format!("{spin} ⚙️ Running `{tool}`…"),
        Phase::Done { detail } => match detail {
            Some(detail) => format!("✅ Done — {detail}"),
            None => "✅ Done".to_string(),
        },
        Phase::Failed { detail } => match detail {
            Some(detail) => format!("❌ Failed — {detail}"),
            None => "❌ Failed".to_string(),
        },
    }
}

/// True once the phase is terminal and the animation should stop ticking.
pub fn is_terminal(phase: &Phase) -> bool {
    matches!(phase, Phase::Done { .. } | Phase::Failed { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles_through_eight_frames() {
        assert_eq!(frame(&Phase::Queued, 0), "⠋ Queued…");
        assert_eq!(frame(&Phase::Queued, 8), "⠋ Queued…");
        assert_ne!(frame(&Phase::Queued, 1), frame(&Phase::Queued, 0));
    }

    #[test]
    fn build_and_run_frames_name_the_tool() {
        let build = frame(
            &Phase::Building {
                tool: "weather".to_string(),
            },
            2,
        );
        assert!(build.contains("Building `weather`"));
        let run = frame(
            &Phase::Running {
                tool: "weather".to_string(),
            },
            3,
        );
        assert!(run.contains("Running `weather`"));
        assert!(!is_terminal(&Phase::Running {
            tool: "weather".to_string()
        }));
    }

    #[test]
    fn terminal_frames_drop_the_spinner() {
        let done = frame(
            &Phase::Done {
                detail: Some("1.2s".to_string()),
            },
            5,
        );
        assert_eq!(done, "✅ Done — 1.2s");
        assert!(is_terminal(&Phase::Done { detail: None }));
        let failed = frame(
            &Phase::Failed {
                detail: Some("timeout".to_string()),
            },
            1,
        );
        assert_eq!(failed, "❌ Failed — timeout");
        assert!(is_terminal(&Phase::Failed { detail: None }));
    }
}
