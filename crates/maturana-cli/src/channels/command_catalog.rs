use maturana_core::state::MaturanaHome;

use super::{
    load_channel_settings, model_button_choices, save_channel_settings, ChannelSettings,
    REASONING_LEVELS,
};

/// Grouped command catalog — single source of truth for /help and /commands.
pub(super) const COMMAND_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Session",
        &[
            ("/new", "start a new session"),
            ("/reset", "reset the current session"),
            ("/stop", "stop the current run"),
            ("/compact", "compact the session context"),
            ("/session", "session settings (e.g. /session idle)"),
            ("/onboard", "(re)run the first-run interview"),
        ],
    ),
    (
        "Options",
        &[
            ("/model", "show or set the model (/model <id>)"),
            ("/models", "list available models"),
            ("/reasoning", "codex reasoning effort (low|medium|high)"),
        ],
    ),
    (
        "Status",
        &[
            ("/help", "show available commands"),
            ("/commands", "list all slash commands"),
            ("/tools", "list available runtime tools"),
            ("/status", "model, channel, harness, time, OS"),
        ],
    ),
    (
        "Management",
        &[
            (
                "/loop",
                "run a multi-agent loop on a goal (/loop <goal>); posts progress here",
            ),
            ("/subagents", "inspect subagent runs for this session"),
            ("/skill", "run a skill by name (/skill <name> [args])"),
            ("/emerge", "run a sub-agent on a task (/emerge <task>)"),
        ],
    ),
    (
        "MaturanaGraph",
        &[
            ("/graph-query", "GraphRAG query (/graph-query <terms>)"),
            ("/graph-insert", "add content to MaturanaGraph"),
        ],
    ),
    (
        "Voice",
        &[
            ("/tts", "enable/disable text-to-speech"),
            ("/tts-provider", "set TTS provider (e.g. elevenlabs)"),
        ],
    ),
];

const TTS_PROVIDERS: &[&str] = &["elevenlabs", "openai", "deepgram"];

/// The full slash-command catalog the console TUI advertises (autocomplete +
/// /help) — the Telegram command menu plus the TUI-local commands.
pub(crate) fn console_command_catalog() -> Vec<(&'static str, &'static str)> {
    let mut out: Vec<(&'static str, &'static str)> = vec![
        ("/help", "show commands and keybindings"),
        ("/clear", "clear the transcript view"),
        ("/quit", "exit the chat"),
    ];
    for (_, cmds) in COMMAND_GROUPS {
        for (name, desc) in *cmds {
            if !out.iter().any(|(n, _)| n == name) {
                out.push((name, desc));
            }
        }
    }
    out.push(("/good", "rate the last reply"));
    out.push(("/bad", "rate the last reply"));
    out
}

pub(super) fn help_text() -> String {
    let mut out = String::from("Maturana commands:\n");
    for (group, cmds) in COMMAND_GROUPS {
        out.push_str(&format!("\n{group}\n"));
        for (name, desc) in *cmds {
            out.push_str(&format!("  {name} — {desc}\n"));
        }
    }
    out.push_str("\nAny other message is sent to the agent.");
    out
}

pub(super) fn commands_text() -> String {
    let mut names: Vec<&str> = Vec::new();
    for (_, cmds) in COMMAND_GROUPS {
        for (name, _) in *cmds {
            names.push(name);
        }
    }
    format!("{}\n/good /bad — rate the last reply", names.join("  "))
}

/// The button source shared by Telegram inline keyboards, Discord selection
/// prompts, and the console TUI picker. Returns (prompt, [(label, callback_data)],
/// columns); `None` for non-selectable commands. callback_data is always
/// `<action>:<value>`.
pub(super) fn command_selector_buttons(
    home: &MaturanaHome,
    agent_id: &str,
    name: &str,
) -> Option<(String, Vec<(String, String)>, usize)> {
    let settings = load_channel_settings(home, agent_id);
    match name {
        "models" | "model" => {
            let current = settings
                .model
                .unwrap_or_else(|| "(harness default)".to_string());
            // callback_data is capped at 64 bytes; drop any id that wouldn't fit.
            let buttons: Vec<(String, String)> = model_button_choices(home, agent_id)
                .into_iter()
                .map(|id| {
                    let data = format!("model:{id}");
                    (id, data)
                })
                .filter(|(_, data)| data.len() <= 64)
                .collect();
            if buttons.is_empty() {
                return None;
            }
            Some((
                format!("Current model: {current}\nTap a recent model, or send /model <id> for any model:"),
                buttons,
                2,
            ))
        }
        "reasoning" => {
            let current = settings
                .reasoning
                .unwrap_or_else(|| "low (default)".to_string());
            let buttons: Vec<(String, String)> = REASONING_LEVELS
                .iter()
                .map(|lvl| (lvl.to_string(), format!("reasoning:{lvl}")))
                .collect();
            Some((
                format!("Reasoning effort: {current} (codex/gpt-5)\nTap a level:"),
                buttons,
                2,
            ))
        }
        "tts-provider" => {
            let current = settings
                .tts_provider
                .unwrap_or_else(|| "(none)".to_string());
            let buttons = TTS_PROVIDERS
                .iter()
                .map(|p| (p.to_string(), format!("ttsprov:{p}")))
                .collect();
            Some((format!("TTS provider: {current}\nPick one:"), buttons, 1))
        }
        "tts" => {
            let buttons = vec![
                ("Enable".to_string(), "tts:on".to_string()),
                ("Disable".to_string(), "tts:off".to_string()),
            ];
            Some((
                format!(
                    "Text-to-speech is {}.",
                    if settings.tts_enabled { "ON" } else { "off" }
                ),
                buttons,
                2,
            ))
        }
        "session" => {
            let buttons = vec![
                ("Active".to_string(), "session:active".to_string()),
                ("Idle".to_string(), "session:idle".to_string()),
            ];
            Some((
                format!(
                    "Session is {}.",
                    if settings.idle { "idle" } else { "active" }
                ),
                buttons,
                2,
            ))
        }
        _ => None,
    }
}

/// Apply one `<action>:<value>` selection: set exactly one ChannelSettings field
/// and persist it. Shared by Telegram inline-keyboard callbacks and console/TUI
/// pickers so the surfaces cannot drift.
pub(crate) fn apply_channel_selection(home: &MaturanaHome, agent_id: &str, data: &str) -> String {
    let (action, value) = data.split_once(':').unwrap_or((data, ""));
    let mut settings = load_channel_settings(home, agent_id);
    let save = |settings: &ChannelSettings| save_channel_settings(home, agent_id, settings);
    match action {
        "model" => {
            settings.model = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("Model set to `{value}` (applies to new turns)."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "reasoning" => {
            settings.reasoning = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("Reasoning effort set to `{value}` (applies to new turns)."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "ttsprov" => {
            settings.tts_provider = Some(value.to_string());
            match save(&settings) {
                Ok(_) => format!("TTS provider set to `{value}`."),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "tts" => {
            settings.tts_enabled = value == "on";
            match save(&settings) {
                Ok(_) => format!(
                    "Text-to-speech {}.",
                    if settings.tts_enabled {
                        "ENABLED"
                    } else {
                        "disabled"
                    }
                ),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        "session" => {
            settings.idle = value == "idle";
            match save(&settings) {
                Ok(_) => format!("Session {}.", if settings.idle { "idle" } else { "active" }),
                Err(e) => format!("Could not save: {e:#}"),
            }
        }
        _ => "Unknown selection.".to_string(),
    }
}
