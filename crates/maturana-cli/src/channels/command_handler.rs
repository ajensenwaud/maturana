use std::{fs, path::PathBuf};

use chrono::Utc;
use maturana_core::{
    session_db::{cancel_pending_inbound, request_cancel_in_progress, session_paths},
    spec::AgentSpec,
    state::MaturanaHome,
    tools::ToolRegistry,
};

use super::{
    command_catalog::commands_text,
    loops::handle_loop_command,
    media::agent_knowledge_graph,
    models::{harness_label, models_text},
    settings::{load_channel_settings, save_channel_settings, REASONING_LEVELS},
};

const TRANSCRIPT_CONTEXT_CHARS: usize = 8000;

pub(super) fn truncate_inline(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}

pub(super) fn status_text(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
) -> String {
    let settings = load_channel_settings(home, agent_id);
    let harness = harness_label(home, agent_id);
    let model = settings
        .model
        .unwrap_or_else(|| "(harness default)".to_string());
    let reasoning = settings
        .reasoning
        .unwrap_or_else(|| "low (default)".to_string());
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC");
    format!(
        "Status\n  agent: {}\n  channel: {} (session {})\n  presence: {}\n  harness: {}\n  model: {}\n  reasoning: {}\n  OS: {}\n  time: {}\n  idle: {}",
        agent_id,
        channel,
        session_id,
        channel_presence(home, agent_id),
        harness,
        model,
        reasoning,
        std::env::consts::OS,
        now,
        if settings.idle { "on" } else { "off" },
    )
}

fn tools_text(home: &MaturanaHome, agent_id: &str) -> String {
    let mut sections: Vec<String> = Vec::new();

    // The agent's real tools live in its spec — MCP servers + opt-in capabilities
    // (the same set render_guest_agents writes into AGENTS.md). The WASM registry
    // below is separate (forged/installed tools) and is usually empty.
    if let Ok(spec) =
        AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md"))
    {
        if !spec.mcp_servers.is_empty() {
            let names = spec
                .mcp_servers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            sections.push(format!("MCP servers: {names}"));
        }
        let egress = &spec.network.egress_allowlist;
        let allows = |needle: &str| egress.iter().any(|h| h.contains(needle));
        let mut caps: Vec<&str> = Vec::new();
        if spec.network.egress_allow_all {
            caps.push("open web (allow-all egress)");
        }
        if allows("brave") || allows("tavily") {
            caps.push("web search");
        }
        if spec.browser.headless_chrome {
            caps.push("browse (headless Chrome)");
        }
        if spec.knowledge_graph.enabled {
            caps.push("knowledge graph (GraphRAG)");
        }
        if spec.capabilities.image_gen {
            caps.push("image generation");
        }
        if spec.capabilities.self_forge {
            caps.push("self-forge (build WASM tools)");
        }
        if !caps.is_empty() {
            sections.push(format!("Capabilities: {}", caps.join(", ")));
        }
    }

    match ToolRegistry::new(home.root().join("tools")).list() {
        Ok(tools) if !tools.is_empty() => {
            let mut out = String::from("Runtime (WASM) tools:\n");
            for t in tools {
                let desc = t.description.lines().next().unwrap_or("").trim();
                out.push_str(&format!("  {} — {}\n", t.name, truncate_inline(desc, 80)));
            }
            sections.push(out.trim_end().to_string());
        }
        Ok(_) => {}
        Err(error) => sections.push(format!("Could not list runtime tools: {error:#}")),
    }

    if sections.is_empty() {
        "No tools or capabilities configured for this agent yet.".to_string()
    } else {
        sections.join("\n")
    }
}

fn subagents_text(home: &MaturanaHome, agent_id: &str) -> String {
    let dir = home.agent_dir(agent_id).join("subagents");
    let mut entries: Vec<String> = Vec::new();
    if let Ok(read) = fs::read_dir(&dir) {
        for e in read.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json") {
                if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                    let mode = fs::read_to_string(e.path())
                        .ok()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(String::from))
                        .unwrap_or_else(|| "ephemeral".to_string());
                    entries.push(format!("  {stem} ({mode})"));
                }
            }
        }
    }
    if entries.is_empty() {
        "No sub-tasks dispatched yet. Run one with /emerge <task>.".to_string()
    } else {
        entries.sort();
        format!(
            "Sub-tasks dispatched via /emerge (each runs as a turn and replies here):\n{}",
            entries.join("\n")
        )
    }
}

fn graph_query_text(home: &MaturanaHome, agent_id: &str, terms: &str) -> String {
    if terms.trim().is_empty() {
        return "Usage: /graph-query <terms>".to_string();
    }
    let kg = agent_knowledge_graph(home, agent_id);
    if !kg.enabled {
        return "Knowledge graph is not enabled for this agent.".to_string();
    }
    let Some(token) = maturana_core::worker::read_graph_token(home.root()) else {
        return "Knowledge graph service is not available (no graph token).".to_string();
    };
    let agent_graph = crate::graph::agent_graph_name(agent_id);
    let graphs = vec![agent_graph.clone(), kg.graph_name(agent_id)];
    let term_list: Vec<String> = terms.split_whitespace().map(String::from).collect();
    let rendered = crate::graph::query_blended_context(
        crate::graph::DEFAULT_LOCAL_URL,
        &token,
        &graphs,
        &term_list,
        2,
    );
    format!(
        "GraphRAG (private + shared):\n{}",
        truncate_inline(&rendered, 3500)
    )
}

/// A short presence line for /status: the channel's last heartbeat.
pub(super) fn channel_presence(home: &MaturanaHome, agent_id: &str) -> String {
    match fs::read_to_string(telegram_heartbeat_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    {
        Some(hb) => {
            let status = hb
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown");
            let at = hb.get("at").and_then(|a| a.as_str()).unwrap_or("?");
            format!("{status} (last beat {at})")
        }
        None => "not started".to_string(),
    }
}

fn telegram_heartbeat_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    if agent_id == "default" {
        home.root().join("channels/telegram/heartbeat.json")
    } else {
        home.agent_dir(agent_id)
            .join("channels/telegram/heartbeat.json")
    }
}

/// Truncate an on-disk channel transcript to its most recent `keep_chars` (the
/// same tail the per-turn context uses), returning the number of bytes freed.
/// The live context is always the recent tail, so this bounds disk growth
/// without dropping anything the agent would have seen this turn.
fn compact_transcript_file(path: &std::path::Path, keep_chars: usize) -> anyhow::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let contents = fs::read_to_string(path)?;
    let char_count = contents.chars().count();
    if char_count <= keep_chars {
        return Ok(0);
    }
    let before = contents.len();
    let tail: String = contents
        .chars()
        .skip(char_count.saturating_sub(keep_chars))
        .collect();
    // Align to the next line boundary so we never slice a transcript line in half.
    let trimmed = match tail.find('\n') {
        Some(idx) => &tail[idx + 1..],
        None => tail.as_str(),
    };
    let new_contents = format!("[older transcript compacted]\n{trimmed}");
    fs::write(path, &new_contents)?;
    Ok(before.saturating_sub(new_contents.len()))
}

pub(super) fn handle_channel_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    name: &str,
    args: &str,
) -> anyhow::Result<String> {
    let reply = match name {
        "commands" => commands_text(),
        "tools" => tools_text(home, agent_id),
        "subagents" => subagents_text(home, agent_id),
        "models" => models_text(home, agent_id),
        "model" => {
            let mut settings = load_channel_settings(home, agent_id);
            if args.trim().is_empty() {
                format!(
                    "Model: {}",
                    settings
                        .model
                        .clone()
                        .unwrap_or_else(|| "(harness default)".to_string())
                )
            } else {
                settings.model = Some(args.trim().to_string());
                save_channel_settings(home, agent_id, &settings)?;
                format!("Model set to `{}` (applies to new turns).", args.trim())
            }
        }
        "reasoning" => {
            let mut settings = load_channel_settings(home, agent_id);
            let arg = args.trim().to_lowercase();
            if arg.is_empty() {
                format!(
                    "Reasoning effort: {} (codex/gpt-5). Set with /reasoning <{}>",
                    settings
                        .reasoning
                        .clone()
                        .unwrap_or_else(|| "low (default)".to_string()),
                    REASONING_LEVELS.join("|"),
                )
            } else if REASONING_LEVELS.contains(&arg.as_str()) {
                settings.reasoning = Some(arg.clone());
                save_channel_settings(home, agent_id, &settings)?;
                format!("Reasoning effort set to `{arg}` (applies to new turns; codex/gpt-5).")
            } else {
                format!(
                    "Unknown level `{arg}`. Choose one of: {}",
                    REASONING_LEVELS.join(", ")
                )
            }
        }
        "reset" => {
            maturana_ops::conversation::reset_channel_context(home, agent_id, chat_id)?;
            "Session reset — durable memory and wiki are preserved.".to_string()
        }
        "stop" => {
            // Two halves: drop queued-but-unclaimed turns, AND flag any IN-PROGRESS
            // turn so the guest worker kills the running harness mid-turn.
            let paths = session_paths(&home.agent_dir(agent_id), session_id);
            let queued = cancel_pending_inbound(&paths).unwrap_or(0);
            let in_progress = request_cancel_in_progress(&paths).unwrap_or(0);
            match (queued, in_progress) {
                (0, 0) => "Nothing to stop — nothing is queued or in progress.".to_string(),
                (q, 0) => format!(
                    "Stopped {q} queued message{}.",
                    if q == 1 { "" } else { "s" }
                ),
                (0, _) => "Stopping the reply in progress…".to_string(),
                (q, _) => format!(
                    "Stopping the reply in progress and dropped {q} queued message{}.",
                    if q == 1 { "" } else { "s" }
                ),
            }
        }
        "compact" => {
            let path = maturana_ops::conversation::channel_transcript_path(home, agent_id, chat_id);
            match compact_transcript_file(&path, TRANSCRIPT_CONTEXT_CHARS) {
                Ok(0) => "Nothing to compact — the transcript is already within the live context window.".to_string(),
                Ok(freed) => format!(
                    "Compacted the stored transcript (freed ~{} KB). Recent context is preserved; durable facts live in memory + the wiki.",
                    (freed + 1023) / 1024
                ),
                Err(error) => format!("Couldn't compact the transcript: {error:#}"),
            }
        }
        "session" => {
            let mut settings = load_channel_settings(home, agent_id);
            let sub = args.split_whitespace().next().unwrap_or("");
            match sub {
                "idle" => {
                    settings.idle = true;
                    save_channel_settings(home, agent_id, &settings)?;
                    "Session set to idle.".to_string()
                }
                "active" | "wake" => {
                    settings.idle = false;
                    save_channel_settings(home, agent_id, &settings)?;
                    "Session active.".to_string()
                }
                _ => format!(
                    "Session {}\n  idle: {}\n  model: {}\nSet with: /session idle | /session active",
                    session_id,
                    if settings.idle { "on" } else { "off" },
                    settings.model.clone().unwrap_or_else(|| "(default)".to_string()),
                ),
            }
        }
        "tts" => {
            let mut settings = load_channel_settings(home, agent_id);
            settings.tts_enabled = !settings.tts_enabled;
            save_channel_settings(home, agent_id, &settings)?;
            let prov = settings
                .tts_provider
                .clone()
                .unwrap_or_else(|| "none set".to_string());
            format!(
                "Text-to-speech {} (provider: {}). Set a provider with /tts-provider <name>.",
                if settings.tts_enabled {
                    "ENABLED"
                } else {
                    "disabled"
                },
                prov
            )
        }
        "tts-provider" => {
            if args.trim().is_empty() {
                let s = load_channel_settings(home, agent_id);
                format!(
                    "TTS provider: {}",
                    s.tts_provider.unwrap_or_else(|| "(none)".to_string())
                )
            } else {
                let mut settings = load_channel_settings(home, agent_id);
                settings.tts_provider = Some(args.trim().to_string());
                save_channel_settings(home, agent_id, &settings)?;
                format!("TTS provider set to `{}`.", args.trim())
            }
        }
        "graph-query" => graph_query_text(home, agent_id, args),
        "graph-insert" => {
            if args.trim().is_empty() {
                "Usage: /graph-insert <text> — adds a note to your private memory graph. (Or attach a document to ingest it.)".to_string()
            } else {
                match maturana_core::worker::read_graph_token(home.root()) {
                    Some(token) => {
                        let agent_graph = crate::graph::agent_graph_name(agent_id);
                        let dir = home.agent_dir(agent_id).join("inbox");
                        let _ = fs::create_dir_all(&dir);
                        let path = dir.join(format!("note-{}.md", Utc::now().timestamp_millis()));
                        match fs::write(&path, args) {
                            Ok(()) => match crate::graph::ingest_file_into_service(
                                crate::graph::DEFAULT_LOCAL_URL,
                                &token,
                                &agent_graph,
                                &path,
                                1200,
                            ) {
                                Ok(chunks) => format!("Added to your memory graph `{agent_graph}` ({chunks} chunk(s))."),
                                Err(error) => format!("Graph insert failed: {error:#}"),
                            },
                            Err(error) => format!("Could not stage note: {error:#}"),
                        }
                    }
                    None => "Knowledge graph service is not available.".to_string(),
                }
            }
        }
        "emerge" => "Usage: /emerge <task> — runs a sub-agent on the task.".to_string(),
        "skill" => {
            let skills_dir = std::path::Path::new("skills");
            let mut names: Vec<String> = Vec::new();
            if let Ok(read) = fs::read_dir(skills_dir) {
                for e in read.flatten() {
                    if e.path().join("SKILL.md").exists() {
                        if let Some(n) = e.path().file_name().and_then(|s| s.to_str()) {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            names.sort();
            if names.is_empty() {
                "Usage: /skill <name> [args]".to_string()
            } else {
                format!("Usage: /skill <name> [args]\nSkills:\n{}", names.join(", "))
            }
        }
        "loop" => handle_loop_command(
            home,
            agent_id,
            session_id,
            chat_id,
            channel,
            platform_id,
            args,
        ),
        _ => format!("Unknown command `/{name}`. Try /help."),
    };
    Ok(reply)
}
