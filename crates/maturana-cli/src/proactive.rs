//! Open-ended proactivity loop. Periodically gives the agent a turn to act or
//! reach out on its own - the thing that makes it feel like an agent rather than
//! a request/response bot. The agent itself decides whether anything is worth
//! saying; a silence sentinel + a minimum gap keep it from nagging.

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use maturana_core::{
    audit::{append_event, AuditEvent},
    state::MaturanaHome,
};

/// The agent emits exactly this when a proactive check yields nothing worth
/// sending; the channel outbox delivery drops it instead of messaging the user.
pub const SILENCE_SENTINEL: &str = "[[MATURANA_SILENT]]";

#[derive(Debug, Args)]
pub struct ProactiveCommand {
    #[command(subcommand)]
    pub command: ProactiveSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ProactiveSubcommand {
    /// Run the proactivity loop for an agent (supervised by `maturana up`).
    Serve {
        agent_id: String,
        #[arg(long, default_value = "telegram-main")]
        session_id: String,
        /// How often to wake and consider acting.
        #[arg(long, default_value_t = 900)]
        interval_seconds: u64,
        /// Minimum spacing between proactive messages (anti-nag).
        #[arg(long, default_value_t = 7200)]
        min_gap_seconds: u64,
        /// Override the self-check directive with a specific nudge (e.g. a scheduled
        /// "morning briefing"). Default: the open-ended "is anything worth saying?"
        /// self-check that may stay silent.
        #[arg(long)]
        directive: Option<String>,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProactiveState {
    #[serde(default)]
    last_fired_ms: i64,
}

pub fn handle_proactive(command: ProactiveCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        ProactiveSubcommand::Serve {
            agent_id,
            session_id,
            interval_seconds,
            min_gap_seconds,
            directive,
            once,
        } => serve(
            home,
            &agent_id,
            &session_id,
            interval_seconds.max(30),
            min_gap_seconds,
            directive.as_deref(),
            once,
        ),
    }
}

fn state_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("proactive-state.json")
}

fn load_state(home: &MaturanaHome, agent_id: &str) -> ProactiveState {
    std::fs::read_to_string(state_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_state(home: &MaturanaHome, agent_id: &str, state: &ProactiveState) -> anyhow::Result<()> {
    let path = state_path(home, agent_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// The agent honours `/session idle`: while idle, no proactive turns fire.
fn is_idle(home: &MaturanaHome, agent_id: &str) -> bool {
    std::fs::read_to_string(home.agent_dir(agent_id).join("channel-settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("idle").and_then(|i| i.as_bool()))
        .unwrap_or(false)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn serve(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    interval_seconds: u64,
    min_gap_seconds: u64,
    directive: Option<&str>,
    once: bool,
) -> anyhow::Result<()> {
    println!("proactive loop serving agent {agent_id}");
    loop {
        if let Err(error) = maybe_fire(home, agent_id, session_id, min_gap_seconds, directive) {
            eprintln!("proactive: {error:#}");
        }
        if once {
            break;
        }
        thread::sleep(Duration::from_secs(interval_seconds));
    }
    Ok(())
}

/// Pure gate: not idle AND the anti-nag gap has elapsed. No side effects, so the
/// idle/gap policy stays unit-testable without a running plane or a paired chat.
fn should_fire(home: &MaturanaHome, agent_id: &str, min_gap_seconds: u64) -> bool {
    if is_idle(home, agent_id) {
        return false;
    }
    let state = load_state(home, agent_id);
    let gap_ms = (min_gap_seconds as i64) * 1000;
    now_ms() - state.last_fired_ms >= gap_ms
}

/// Enqueue a proactive turn if the gate allows AND the agent has a paired chat to
/// reach. Returns whether a turn was enqueued.
fn maybe_fire(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    min_gap_seconds: u64,
    directive: Option<&str>,
) -> anyhow::Result<bool> {
    if !should_fire(home, agent_id, min_gap_seconds) {
        return Ok(false);
    }
    // No paired outreach channel => no one to message => don't fire (and don't
    // pile up undeliverable rows). Route the turn through the shared front door
    // tagged for the REAL chat: a turn tagged "proactive" had its reply filtered
    // out by every channel's delivery loop and never reached the user. Going
    // through the front door also injects context (memory + recent conversation +
    // graph), so the agent can actually find something worth saying instead of
    // defaulting to silence with nothing to reference.
    let Some(chat_id) = crate::channels::current_paired_telegram_chat_id(home, agent_id) else {
        return Ok(false);
    };
    let prompt = directive.map(str::to_string).unwrap_or_else(proactive_prompt);
    crate::channels::enqueue_outreach_turn(
        home,
        agent_id,
        session_id,
        chat_id,
        &prompt,
        "proactive",
        serde_json::json!({}),
    )?;
    let mut state = load_state(home, agent_id);
    state.last_fired_ms = now_ms();
    save_state(home, agent_id, &state)?;
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: chrono::Utc::now(),
            agent_id: agent_id.to_string(),
            action: "proactive.turn".to_string(),
            message: "enqueued proactive self-check".to_string(),
        },
    )?;
    Ok(true)
}

fn proactive_prompt() -> String {
    format!(
        "[PROACTIVE CHECK - you initiated this; the user did NOT message you.]\n\n\
         Review your memory (MEMORY.md), recent conversation, and any follow-ups or \
         commitments you've recorded. Decide whether there is something genuinely \
         worth telling the user right now - a due reminder, a finished task, a timely \
         update, or a thoughtful, relevant check-in.\n\n\
         - If yes: write ONLY that message to the user, in your own voice.\n\
         - If there is nothing worth interrupting them with: reply with exactly \
         {SILENCE_SENTINEL} and nothing else.\n\n\
         Be sparing. Silence is better than nagging."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_gap_gate_firing() {
        let temp = std::env::temp_dir().join(format!("mat-proactive-{}", now_ms()));
        let home = MaturanaHome::new(temp.join(".maturana"));
        std::fs::create_dir_all(home.agent_dir("a")).unwrap();

        // Gap elapsed (no prior fire) => the gate opens.
        assert!(should_fire(&home, "a", 7200));
        // Record a fire just now => the anti-nag gap closes the gate.
        save_state(
            &home,
            "a",
            &ProactiveState {
                last_fired_ms: now_ms(),
            },
        )
        .unwrap();
        assert!(!should_fire(&home, "a", 7200));

        // Idle suppresses firing even with the gap elapsed.
        std::fs::write(
            home.agent_dir("a").join("channel-settings.json"),
            "{\"idle\":true}",
        )
        .unwrap();
        assert!(!should_fire(&home, "a", 0));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn prompt_carries_silence_sentinel() {
        assert!(proactive_prompt().contains(SILENCE_SENTINEL));
    }
}
