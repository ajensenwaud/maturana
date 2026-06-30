use anyhow::Context;
use chrono::{DateTime, Utc};
use maturana_core::{
    hooks::{HookContext, HookEvent},
    improvement::TrajectoryStore,
    pipelock::PipelockVault,
    session_db::{ensure_session, insert_inbound, session_paths},
    spec::{AgentSpec, KnowledgeGraph},
    state::MaturanaHome,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
};

use crate::graph;

const TELEGRAM_CHAT_ID: &str = "telegram/chat-id";

// AGENTS.md is the agent's own contract + operational recipes (capabilities,
// peer delegation, honesty limits). It is authored and bounded, not unbounded
// context like wiki or transcript. 8000 fits a maxed AGENTS.md with headroom.
const IDENTITY_CONTEXT_CHARS: usize = 8000;
const SOUL_CONTEXT_CHARS: usize = 4000;
const CONTRACT_CONTEXT_CHARS: usize = 5000;
const MEMORY_CONTEXT_CHARS: usize = 5000;
const AGENT_CONTEXT_CHARS: usize = 3000;
const TRANSCRIPT_CONTEXT_CHARS: usize = 8000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChannelSettings {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tts_enabled: bool,
    #[serde(default)]
    tts_provider: Option<String>,
    #[serde(default)]
    idle: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChannelContextManifest {
    at: DateTime<Utc>,
    agent_id: String,
    chat_id: i64,
    source_files: Vec<LoadedContextFile>,
    wiki_query_terms: Vec<String>,
    wiki_term_sources: Vec<WikiTermSource>,
    #[serde(default)]
    graph_name: Option<String>,
    #[serde(default)]
    graph_context_chars: usize,
    context_policy: ContextPolicySummary,
    loaded_context_chars: usize,
    transcript_path: String,
    transcript_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoadedContextFile {
    label: String,
    path: String,
    chars: usize,
    missing: bool,
}

#[derive(Debug, Clone)]
struct ContextFile {
    contents: String,
    summary: LoadedContextFile,
}

#[derive(Debug)]
struct ChannelContextBundle {
    identity: ContextFile,
    soul: ContextFile,
    contract: ContextFile,
    memory: ContextFile,
    agent_context: ContextFile,
    wiki_query_terms: Vec<String>,
    wiki_term_sources: Vec<WikiTermSource>,
    graph_context: Option<GraphChannelContext>,
    learned_examples: String,
    self_forge: bool,
    onboarding_active: bool,
    transcript: String,
    transcript_path: PathBuf,
}

#[derive(Debug, Clone)]
struct GraphChannelContext {
    graph: String,
    rendered: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WikiTermSource {
    term: String,
    sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextPolicySummary {
    strategy: String,
    transcript_char_budget: usize,
    excludes_reset_marker: bool,
}

pub fn stable_chat_key(platform_id: &str) -> i64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    platform_id.hash(&mut hasher);
    (hasher.finish() >> 1) as i64
}

pub fn console_chat_key() -> i64 {
    stable_chat_key("console:tui")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxTarget {
    pub channel: String,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub agent_id: String,
    pub session_id: String,
}

pub fn post_outbox_text(
    home: &MaturanaHome,
    target: Option<&OutboxTarget>,
    text: &str,
) -> anyhow::Result<bool> {
    let Some(target) = target else {
        return Ok(false);
    };
    write_outbox_body(home, target, serde_json::json!({ "text": text }))
}

pub fn post_outbox_files(
    home: &MaturanaHome,
    target: Option<&OutboxTarget>,
    text: &str,
    files: &[String],
) -> anyhow::Result<bool> {
    let Some(target) = target else {
        return Ok(false);
    };
    let existing: Vec<String> = files
        .iter()
        .filter(|file| Path::new(file).is_file())
        .cloned()
        .collect();
    if existing.is_empty() {
        return post_outbox_text(home, Some(target), text);
    }
    write_outbox_body(
        home,
        target,
        serde_json::json!({ "text": text, "files": existing }),
    )
}

fn write_outbox_body(
    home: &MaturanaHome,
    target: &OutboxTarget,
    body: serde_json::Value,
) -> anyhow::Result<bool> {
    let paths = session_paths(&home.agent_dir(&target.agent_id), &target.session_id);
    if let Some(parent) = paths.outbound_db.parent() {
        fs::create_dir_all(parent)?;
    }
    maturana_core::session_db::write_outbound(
        &paths,
        None,
        "chat",
        &target.channel,
        &target.platform_id,
        target.thread_id.as_deref(),
        &body.to_string(),
    )?;
    Ok(true)
}

pub fn channel_transcript_path(home: &MaturanaHome, agent_id: &str, chat_id: i64) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/telegram")
        .join(format!("{chat_id}.md"))
}

pub fn channel_context_manifest_path(home: &MaturanaHome, agent_id: &str, chat_id: i64) -> PathBuf {
    home.agent_dir(agent_id)
        .join("channels/telegram")
        .join(format!("{chat_id}.context.json"))
}

pub fn append_channel_turn(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    role: &str,
    text: &str,
) -> anyhow::Result<()> {
    let path = channel_transcript_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = format!(
        "\n## {} {}\n\n{}\n",
        Utc::now().to_rfc3339(),
        role,
        text.trim()
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;
    Ok(())
}

pub fn reset_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
) -> anyhow::Result<()> {
    let path = channel_transcript_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let reset_id = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    if path.exists() {
        let archive_dir = path
            .parent()
            .expect("transcript path always has a parent")
            .join("archive");
        fs::create_dir_all(&archive_dir)?;
        let archive = archive_dir.join(format!("{chat_id}-{reset_id}.md"));
        fs::rename(&path, archive)?;
    }
    let manifest_path = channel_context_manifest_path(home, agent_id, chat_id);
    if manifest_path.exists() {
        let archive_dir = manifest_path
            .parent()
            .expect("context manifest path always has a parent")
            .join("archive");
        fs::create_dir_all(&archive_dir)?;
        let archive = archive_dir.join(format!("{chat_id}-{reset_id}.context.json"));
        fs::rename(&manifest_path, archive)?;
    }
    fs::write(
        &path,
        format!(
            "# Telegram Session\n\nStarted: {}\n\nMemory and wiki context will be reloaded on the next turn.\n",
            Utc::now().to_rfc3339()
        ),
    )?;
    Ok(())
}

pub fn build_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<String> {
    let context = load_channel_context(home, agent_id, chat_id, user_message)?;
    write_channel_context_manifest(home, agent_id, chat_id, &context)?;
    Ok(render_channel_prompt(&context, user_message))
}

pub fn maybe_remember_user_message(
    home: &MaturanaHome,
    agent_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let Some(fact) = extract_memory_fact(text) else {
        return Ok(());
    };

    let path = home.agent_dir(agent_id).join("memory/MEMORY.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(&path, "# Memory\n")?;
    }
    let entry = format!("\n- {}: {}\n", Utc::now().date_naive(), fact);
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(entry.as_bytes())?;

    if let Some(token) = maturana_core::worker::read_graph_token(home.root()) {
        let agent_graph = graph::agent_graph_name(agent_id);
        let dir = home.agent_dir(agent_id).join("inbox");
        if fs::create_dir_all(&dir).is_ok() {
            let note = dir.join(format!("memory-{}.md", Utc::now().timestamp_millis()));
            if fs::write(&note, &fact).is_ok() {
                let _ = graph::ingest_file_into_service(
                    graph::DEFAULT_LOCAL_URL,
                    &token,
                    &agent_graph,
                    &note,
                    1200,
                );
            }
        }
    }
    Ok(())
}

pub fn enqueue_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    chat_key: i64,
    thread_id: Option<&str>,
    text: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    append_channel_turn(home, agent_id, chat_key, "user", text)?;
    maybe_remember_user_message(home, agent_id, text)?;
    let prompt = build_channel_prompt(home, agent_id, chat_key, text)?;
    let settings = load_channel_settings(home, agent_id);
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut content = serde_json::json!({
        "text": text,
        "prompt": prompt,
        "model": settings.model,
        "reasoning": settings.reasoning,
    });
    if let (Some(obj), serde_json::Value::Object(extra_map)) = (content.as_object_mut(), extra) {
        for (key, value) in extra_map {
            obj.insert(key, value);
        }
    }
    let id = insert_inbound(
        &paths,
        "chat",
        channel,
        platform_id,
        thread_id,
        &content.to_string(),
    )?;
    fire_agent_hooks(
        home,
        HookContext::new(HookEvent::MessageIn, agent_id)
            .channel(channel)
            .text(text),
    );
    Ok(id)
}

pub fn enqueue_channel_prompt(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    thread_id: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    enqueue_turn(
        home,
        agent_id,
        session_id,
        channel,
        platform_id,
        stable_chat_key(platform_id),
        thread_id,
        text,
        serde_json::json!({}),
    )?;
    Ok(())
}

pub fn enqueue_outreach_turn(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    directive: &str,
    kind: &str,
    extra: serde_json::Value,
) -> anyhow::Result<String> {
    let prompt = build_channel_prompt(home, agent_id, chat_id, directive)?;
    let settings = load_channel_settings(home, agent_id);
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut content = serde_json::json!({
        "text": directive,
        "prompt": prompt,
        "model": settings.model,
        "reasoning": settings.reasoning,
    });
    if let (Some(obj), serde_json::Value::Object(extra_map)) = (content.as_object_mut(), extra) {
        for (key, value) in extra_map {
            obj.insert(key, value);
        }
    }
    insert_inbound(
        &paths,
        kind,
        "telegram",
        &chat_id.to_string(),
        None,
        &content.to_string(),
    )
}

pub fn fire_agent_hooks(home: &MaturanaHome, ctx: HookContext) {
    let spec_path = home.agent_dir(&ctx.agent_id).join("MATURANA.md");
    let spec = match AgentSpec::from_maturana_markdown(&spec_path) {
        Ok(spec) => spec,
        Err(_) => return,
    };
    if spec.hooks.on.is_empty() {
        return;
    }
    let home = home.clone();
    std::thread::spawn(move || {
        let enqueue = |target: &str, prompt: &str| -> anyhow::Result<()> {
            let session = format!("{target}-main");
            match current_paired_telegram_chat_id(&home, target) {
                Some(chat_id) => {
                    enqueue_outreach_turn(
                        &home,
                        target,
                        &session,
                        chat_id,
                        prompt,
                        "hook",
                        serde_json::json!({}),
                    )?;
                    Ok(())
                }
                None => anyhow::bail!(
                    "agent '{target}' has no paired channel to receive a hook-enqueued turn"
                ),
            }
        };
        maturana_core::hooks::fire(&spec, &ctx, Some(&enqueue));
    });
}

pub fn current_paired_telegram_chat_id(home: &MaturanaHome, agent_id: &str) -> Option<i64> {
    let vault = PipelockVault::new(home.pipelock_dir());
    vault
        .get(&telegram_chat_id_key(agent_id))
        .or_else(|_| vault.get(TELEGRAM_CHAT_ID))
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
}

fn load_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    user_message: &str,
) -> anyhow::Result<ChannelContextBundle> {
    let agent_dir = home.agent_dir(agent_id);
    let transcript_path = channel_transcript_path(home, agent_id, chat_id);
    let transcript = tail_context_file(&transcript_path, TRANSCRIPT_CONTEXT_CHARS)?;
    let wiki_query = build_wiki_query_policy(user_message, &transcript);
    let wiki_query_terms = wiki_query
        .term_sources
        .iter()
        .map(|term| term.term.clone())
        .collect::<Vec<_>>();
    let graph_context = load_graph_channel_context(home, agent_id, &wiki_query_terms);
    let learned_examples = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))
        .and_then(|store| store.learned_examples_markdown(agent_id, 3, 0.5))
        .unwrap_or_default();

    Ok(ChannelContextBundle {
        identity: read_context_file(
            "AGENTS.md",
            &agent_dir.join("AGENTS.md"),
            IDENTITY_CONTEXT_CHARS,
        )?,
        soul: read_context_file("SOUL.md", &agent_dir.join("SOUL.md"), SOUL_CONTEXT_CHARS)?,
        contract: read_context_file(
            "MATURANA.md",
            &agent_dir.join("MATURANA.md"),
            CONTRACT_CONTEXT_CHARS,
        )?,
        memory: read_context_file(
            "memory/MEMORY.md",
            &agent_dir.join("memory/MEMORY.md"),
            MEMORY_CONTEXT_CHARS,
        )?,
        agent_context: read_context_file(
            "context/README.md",
            &agent_dir.join("context/README.md"),
            AGENT_CONTEXT_CHARS,
        )?,
        wiki_query_terms,
        wiki_term_sources: wiki_query.term_sources,
        graph_context,
        learned_examples,
        self_forge: AgentSpec::from_maturana_markdown(agent_dir.join("MATURANA.md"))
            .map(|spec| spec.capabilities.self_forge)
            .unwrap_or(false),
        onboarding_active: is_onboarding_active(home, agent_id),
        transcript,
        transcript_path,
    })
}

fn load_graph_channel_context(
    home: &MaturanaHome,
    agent_id: &str,
    terms: &[String],
) -> Option<GraphChannelContext> {
    let knowledge_graph = agent_knowledge_graph(home, agent_id);
    if !knowledge_graph.enabled {
        return None;
    }
    let token = maturana_core::worker::read_graph_token(home.root())?;
    let shared = knowledge_graph.graph_name(agent_id);
    let agent_graph = graph::agent_graph_name(agent_id);
    let graphs = vec![agent_graph.clone(), shared.clone()];
    let rendered =
        graph::query_blended_context(graph::DEFAULT_LOCAL_URL, &token, &graphs, terms, 2);
    Some(GraphChannelContext {
        graph: format!("{agent_graph} + {shared}"),
        rendered,
    })
}

fn render_channel_prompt(context: &ChannelContextBundle, user_message: &str) -> String {
    let forge_section = if context.self_forge {
        forge_prompt_section()
    } else {
        ""
    };
    let graph_section = match &context.graph_context {
        Some(graph) => format!(
            "\n## Knowledge Graph Context (GraphRAG, graph `{}`)\n\nEntities and relationships retrieved from your knowledge graph for this message. Treat them as ground truth about ingested documents and recorded facts.\n\n{}\n",
            graph.graph, graph.rendered
        ),
        None => String::new(),
    };
    let learned_section = if context.learned_examples.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n## Learned Examples (positively rated)\n\n{}\n",
            context.learned_examples
        )
    };
    let onboarding_section = if context.onboarding_active {
        "\n## Onboarding in progress — KEEP THE INTERVIEW GOING\n\
         You are still in your first-run onboarding interview with your owner. This \
         is a short, warm conversation, not a one-off Q&A. After briefly acknowledging \
         what they just told you, ASK THE NEXT THING you don't yet know — one question \
         at a time — until you have learned ALL of: their name and how they'd like to \
         be addressed; their timezone / working hours; and the main things they want \
         your help with. Save durable facts to memory and fill IDENTITY.md's \"Who you \
         are to me\" section as you learn them. Until you have all of that, your reply \
         MUST end with the next question. Only when you genuinely have everything, give \
         a short warm wrap-up and put [[ONBOARDING_COMPLETE]] on its own final line (it \
         is removed before the message is sent).\n"
    } else {
        ""
    };
    format!(
        r#"You are a Maturana personal agent running inside an isolated VM.

Answer the current Telegram message directly and conversationally.
Use the durable memory and recent channel transcript for continuity.
Do not say you cannot remember earlier messages if the transcript contains them.
If the user asks you to remember something, acknowledge it briefly; the host has already stored the raw user memory note.
Return only the message that should be sent back to Telegram.
{onboarding_section}
## AGENTS.md
{identity}

## SOUL.md
{soul}

## MATURANA.md
{contract}

## Durable Memory
{memory}

## Agent Context
{agent_context}
{graph_section}{learned_section}{forge_section}
## Recent Telegram Transcript
{transcript}

## Current Telegram Message
{user_message}
"#,
        identity = context.identity.contents,
        soul = context.soul.contents,
        contract = context.contract.contents,
        memory = context.memory.contents,
        agent_context = context.agent_context.contents,
        transcript = context.transcript,
    )
}

fn write_channel_context_manifest(
    home: &MaturanaHome,
    agent_id: &str,
    chat_id: i64,
    context: &ChannelContextBundle,
) -> anyhow::Result<()> {
    let path = channel_context_manifest_path(home, agent_id, chat_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let source_files = vec![
        context.identity.summary.clone(),
        context.soul.summary.clone(),
        context.contract.summary.clone(),
        context.memory.summary.clone(),
        context.agent_context.summary.clone(),
    ];
    let graph_context_chars = context
        .graph_context
        .as_ref()
        .map(|graph| graph.rendered.chars().count())
        .unwrap_or(0);
    let loaded_context_chars = source_files.iter().map(|file| file.chars).sum::<usize>()
        + graph_context_chars
        + context.transcript.chars().count();
    let manifest = ChannelContextManifest {
        at: Utc::now(),
        agent_id: agent_id.to_string(),
        chat_id,
        source_files,
        wiki_query_terms: context.wiki_query_terms.clone(),
        wiki_term_sources: context.wiki_term_sources.clone(),
        graph_name: context
            .graph_context
            .as_ref()
            .map(|graph| graph.graph.clone()),
        graph_context_chars,
        context_policy: ContextPolicySummary {
            strategy: "durable-files-plus-current-message-and-recent-transcript-graph-terms"
                .to_string(),
            transcript_char_budget: TRANSCRIPT_CONTEXT_CHARS,
            excludes_reset_marker: true,
        },
        loaded_context_chars,
        transcript_path: context.transcript_path.display().to_string(),
        transcript_chars: context.transcript.chars().count(),
    };
    fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(())
}

#[derive(Debug)]
struct WikiQueryPolicy {
    term_sources: Vec<WikiTermSource>,
}

fn build_wiki_query_policy(user_message: &str, transcript: &str) -> WikiQueryPolicy {
    let mut terms = BTreeMap::<String, Vec<String>>::new();
    collect_wiki_query_terms("current_message", user_message, &mut terms);
    collect_wiki_query_terms(
        "recent_transcript",
        &transcript_for_wiki_query(transcript),
        &mut terms,
    );
    WikiQueryPolicy {
        term_sources: terms
            .into_iter()
            .map(|(term, sources)| WikiTermSource { term, sources })
            .collect(),
    }
}

fn collect_wiki_query_terms(source: &str, text: &str, terms: &mut BTreeMap<String, Vec<String>>) {
    for term in extract_wiki_query_terms(text) {
        let sources = terms.entry(term).or_default();
        if !sources.iter().any(|existing| existing == source) {
            sources.push(source.to_string());
        }
    }
}

fn extract_wiki_query_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split_whitespace()
        .map(normalize_wiki_query_term)
        .filter(|term| term.len() >= 3 && !is_wiki_query_stopword(term))
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

fn normalize_wiki_query_term(term: &str) -> String {
    term.trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

fn is_wiki_query_stopword(term: &str) -> bool {
    matches!(
        term,
        "about"
            | "again"
            | "agent"
            | "context"
            | "current"
            | "durable"
            | "hello"
            | "memory"
            | "maturana"
            | "message"
            | "please"
            | "reload"
            | "reloaded"
            | "session"
            | "should"
            | "telegram"
            | "transcript"
            | "turn"
            | "what"
            | "wiki"
            | "with"
    )
}

fn transcript_for_wiki_query(transcript: &str) -> String {
    let lines = transcript
        .lines()
        .filter(|line| !line.starts_with("# Telegram Session"))
        .filter(|line| !line.starts_with("Started:"))
        .filter(|line| !line.contains("Memory and wiki context will be reloaded"))
        .collect::<Vec<_>>();
    lines.join("\n")
}

fn read_context_file(label: &str, path: &Path, limit: usize) -> anyhow::Result<ContextFile> {
    if !path.exists() {
        return Ok(ContextFile {
            contents: "(missing)".to_string(),
            summary: LoadedContextFile {
                label: label.to_string(),
                path: path.display().to_string(),
                chars: 0,
                missing: true,
            },
        });
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let contents = truncate_chars(&contents, limit);
    Ok(ContextFile {
        summary: LoadedContextFile {
            label: label.to_string(),
            path: path.display().to_string(),
            chars: contents.chars().count(),
            missing: false,
        },
        contents,
    })
}

fn tail_context_file(path: &Path, limit: usize) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok("(no transcript yet)".to_string());
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let char_count = contents.chars().count();
    if char_count <= limit {
        return Ok(contents);
    }
    Ok(format!(
        "[older transcript omitted]\n{}",
        contents
            .chars()
            .skip(char_count.saturating_sub(limit))
            .collect::<String>()
    ))
}

pub fn extract_memory_fact(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    for cue in [
        "/remember ",
        "remember that ",
        "remember this: ",
        "remember this:",
        "remember: ",
        "remember:",
        "please remember ",
        "remember ",
    ] {
        if let Some(rest) = lower.strip_prefix(cue) {
            let fact = t[t.len() - rest.len()..].trim();
            if !fact.is_empty() {
                return Some(fact.to_string());
            }
        }
    }
    const HEURISTICS: &[&str] = &[
        "my name is",
        "call me ",
        "i prefer",
        "i live in",
        "i work at",
        "my email",
        "my phone",
        "my timezone",
        "my birthday",
        "remind me",
        "deadline",
        "due by",
        "due on",
    ];
    if HEURISTICS.iter().any(|h| lower.contains(h)) {
        return Some(t.to_string());
    }
    None
}

fn load_channel_settings(home: &MaturanaHome, agent_id: &str) -> ChannelSettings {
    fs::read_to_string(channel_settings_path(home, agent_id))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn channel_settings_path(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("channel-settings.json")
}

fn agent_knowledge_graph(home: &MaturanaHome, agent_id: &str) -> KnowledgeGraph {
    AgentSpec::from_maturana_markdown(&home.agent_dir(agent_id).join("MATURANA.md"))
        .ok()
        .map(|spec| spec.knowledge_graph)
        .unwrap_or_default()
}

fn onboarding_active_marker(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    home.agent_dir(agent_id).join("onboarding-active")
}

fn is_onboarding_active(home: &MaturanaHome, agent_id: &str) -> bool {
    onboarding_active_marker(home, agent_id).exists()
}

fn telegram_chat_id_key(agent_id: &str) -> String {
    if agent_id == "default" {
        TELEGRAM_CHAT_ID.to_string()
    } else {
        format!("telegram/{agent_id}/chat-id")
    }
}

fn forge_prompt_section() -> &'static str {
    r#"
## Self-Forge — build and run a capability on the fly
You are allowed to extend yourself at runtime. When a task needs computation or
transformation you don't already have, author a small WebAssembly capability and
run it immediately, the same turn, in a sandbox — no host rebuild. Use the
`maturana-forge` shell helper:

```
maturana-forge <name> --input '{"n": 7}' <<'WAT'
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; ... compute, then write the result to stdout (fd 1) via fd_write ...
  (func (export "_start") ...))
WAT
```

It assembles your WAT, runs the module under a fuel/memory/timeout sandbox (no
ambient filesystem or network unless you declare it), and returns the module's
stdout. Submit a precompiled module with `--wasm <base64>` instead of heredoc
WAT. The channel shows a 🔨 Building / ⚙️ Running animation while it happens.
Forge sparingly and only when it helps; then describe in your reply what you
built and what it returned.
"#
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    value.chars().take(limit).collect::<String>() + "\n...[truncated]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home(name: &str) -> (PathBuf, MaturanaHome) {
        let root = std::env::temp_dir().join(format!(
            "maturana-conversation-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let home = MaturanaHome::new(&root);
        (root, home)
    }

    fn target() -> OutboxTarget {
        OutboxTarget {
            channel: "telegram".to_string(),
            platform_id: "123".to_string(),
            thread_id: Some("thread-1".to_string()),
            agent_id: "agent-a".to_string(),
            session_id: "agent-a-main".to_string(),
        }
    }

    #[test]
    fn stable_chat_key_is_deterministic_and_positive() {
        let a = stable_chat_key("C123");
        assert_eq!(a, stable_chat_key("C123"));
        assert!(a >= 0);
        assert_ne!(a, stable_chat_key("C124"));
    }

    #[test]
    fn post_outbox_text_creates_session_and_writes_chat_body() {
        let (root, home) = temp_home("text");
        let target = target();

        assert!(post_outbox_text(&home, Some(&target), "hello").unwrap());

        let paths =
            maturana_core::session_db::session_paths(&home.agent_dir("agent-a"), "agent-a-main");
        let messages = maturana_core::session_db::list_recent_outbound(&paths, 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel, "telegram");
        assert_eq!(messages[0].platform_id, "123");
        assert_eq!(messages[0].thread_id.as_deref(), Some("thread-1"));
        let body: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(body["text"], "hello");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn post_outbox_files_filters_missing_files_and_degrades_to_text() {
        let (root, home) = temp_home("files");
        let target = target();
        let existing = root.join("report.txt");
        std::fs::write(&existing, "ok").unwrap();

        assert!(post_outbox_files(
            &home,
            Some(&target),
            "done",
            &[
                existing.display().to_string(),
                root.join("missing.txt").display().to_string()
            ],
        )
        .unwrap());
        assert!(!post_outbox_files(&home, None, "ignored", &[]).unwrap());

        let paths =
            maturana_core::session_db::session_paths(&home.agent_dir("agent-a"), "agent-a-main");
        let messages = maturana_core::session_db::list_recent_outbound(&paths, 10).unwrap();
        assert_eq!(messages.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(body["text"], "done");
        assert_eq!(
            body["files"].as_array().unwrap(),
            &[serde_json::Value::String(existing.display().to_string())]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
