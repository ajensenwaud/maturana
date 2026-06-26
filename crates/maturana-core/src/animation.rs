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

/// The "dust" glyph a dissolving status crumbles into.
pub const DUST: char = '·';

/// Frames that "dissolve" a status line into dust — the swoosh shown when the
/// thinking indicator is replaced by the answer (or removed). A chat channel has
/// no animation primitive, so the effect is simulated: the channel layer plays
/// these frames in quick succession via `editMessageText`. Pure + deterministic so
/// it is testable.
///
/// Each frame replaces a growing, SCATTERED fraction of the visible characters
/// with the dust glyph (so the text crumbles like sand rather than wiping straight
/// across), ending on a sparse frame with just a few motes drifting. Whitespace is
/// preserved so a multi-line block keeps its shape as it falls apart. Returns an
/// empty vec for blank input (nothing to dissolve).
pub fn dissolve_frames(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut order: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .collect();
    let total = order.len();
    if total == 0 {
        return Vec::new();
    }
    // Scatter the crumble order deterministically (Knuth multiplicative hash on the
    // index) so dissolution looks like dust, not a left-to-right wipe.
    order.sort_by_key(|&i| i.wrapping_mul(2_654_435_761) & 0xffff);
    let mut frames = Vec::new();
    for &pct in &[45usize, 75, 100] {
        let take = (total * pct + 99) / 100; // ceil(total * pct / 100)
        let mut buf = chars.clone();
        for &i in order.iter().take(take) {
            buf[i] = DUST;
        }
        frames.push(buf.into_iter().collect());
    }
    // Final sparse frame: thin most of the dust back to spaces, leaving a few motes.
    let mut sparse: Vec<char> = chars
        .iter()
        .map(|c| if c.is_whitespace() { *c } else { ' ' })
        .collect();
    for (n, &i) in order.iter().enumerate() {
        if n % 4 == 0 {
            sparse[i] = DUST;
        }
    }
    frames.push(sparse.into_iter().collect());
    frames
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
        let build = frame(&Phase::Building { tool: "weather".to_string() }, 2);
        assert!(build.contains("Building `weather`"));
        let run = frame(&Phase::Running { tool: "weather".to_string() }, 3);
        assert!(run.contains("Running `weather`"));
        assert!(!is_terminal(&Phase::Running { tool: "weather".to_string() }));
    }

    #[test]
    fn dissolve_crumbles_text_to_dust() {
        let frames = dissolve_frames("Working 00:12");
        // Four frames: 45% / 75% / 100% dust, then a sparse mote frame.
        assert_eq!(frames.len(), 4);
        let dust = |s: &str| s.chars().filter(|&c| c == DUST).count();
        // Increasing dust through the first three frames…
        assert!(dust(&frames[0]) < dust(&frames[1]));
        assert!(dust(&frames[1]) < dust(&frames[2]));
        // …the third is fully dusted (every visible glyph), the last is sparser.
        let visible = "Working 00:12".chars().filter(|c| !c.is_whitespace()).count();
        assert_eq!(dust(&frames[2]), visible);
        assert!(dust(&frames[3]) < dust(&frames[2]));
        // Whitespace positions are preserved (the space stays a space everywhere).
        for f in &frames {
            assert_eq!(f.chars().count(), "Working 00:12".chars().count());
            assert_eq!(f.chars().nth(7), Some(' '));
        }
        // Deterministic.
        assert_eq!(dissolve_frames("Working 00:12"), frames);
        // Nothing to dissolve → no frames.
        assert!(dissolve_frames("   ").is_empty());
    }

    #[test]
    fn terminal_frames_drop_the_spinner() {
        let done = frame(&Phase::Done { detail: Some("1.2s".to_string()) }, 5);
        assert_eq!(done, "✅ Done — 1.2s");
        assert!(is_terminal(&Phase::Done { detail: None }));
        let failed = frame(&Phase::Failed { detail: Some("timeout".to_string()) }, 1);
        assert_eq!(failed, "❌ Failed — timeout");
        assert!(is_terminal(&Phase::Failed { detail: None }));
    }
}
