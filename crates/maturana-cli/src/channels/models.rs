use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use maturana_core::{
    spec::{AgentSpec, HarnessRuntime},
    state::MaturanaHome,
};

use super::settings::load_channel_settings;

/// One entry from the live OpenRouter catalog, with the fields the picker needs
/// to rank by recency and keep only models you'd actually chat with.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct OpenRouterModel {
    pub(super) id: String,
    /// Unix epoch seconds the model was added to OpenRouter (newest = largest).
    pub(super) created: i64,
    /// Whether the model emits text (a chat model) vs. image/embedding-only.
    pub(super) text_output: bool,
    /// Whether the model accepts a `tools` array. opencode always sends one, so a
    /// model without tool support APIErrors every turn — exclude it from the picker.
    pub(super) supports_tools: bool,
}

/// The `n` most recently-added chat models from the LIVE OpenRouter catalog,
/// newest first. Filters out non-text models (image, embedding) and classifier/
/// guard models so the picker only shows things you'd actually chat with — but
/// any catalog id still works via `/model <id>`. Replaces the old hardcoded
/// "mainstream" allowlist, which went stale as new model families shipped.
pub(super) fn recent_openrouter_models(models: &[OpenRouterModel], n: usize) -> Vec<String> {
    // Backstop name filters: image is also caught by `text_output`, but classifier/
    // safety/embedding models report text output yet aren't chat models.
    const DENY_SUBSTR: &[&str] = &[
        "embed",
        "moderation",
        "content-safety",
        "guard",
        "image",
        "rerank",
    ];
    let mut picked: Vec<&OpenRouterModel> = models
        .iter()
        .filter(|m| m.text_output)
        // opencode always sends tools; a model without tool support APIErrors.
        .filter(|m| m.supports_tools)
        .filter(|m| {
            let id = m.id.to_ascii_lowercase();
            !DENY_SUBSTR.iter().any(|deny| id.contains(deny))
        })
        .collect();
    // Newest first; ties broken by id for a stable order.
    picked.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    picked.into_iter().take(n).map(|m| m.id.clone()).collect()
}

pub(super) fn fetch_openrouter_catalog() -> anyhow::Result<Vec<OpenRouterModel>> {
    let resp: serde_json::Value = ureq::get("https://openrouter.ai/api/v1/models")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .context("OpenRouter request failed")?
        .into_json()
        .context("failed to parse OpenRouter response")?;
    let models = resp
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|i| i.as_str())?.to_string();
                    let created = m.get("created").and_then(|c| c.as_i64()).unwrap_or(0);
                    // A chat model lists "text" among its output modalities. Image
                    // and embedding models do not. Absent field => assume text.
                    let text_output = m
                        .get("architecture")
                        .and_then(|a| a.get("output_modalities"))
                        .and_then(|o| o.as_array())
                        .map(|arr| arr.iter().any(|v| v.as_str() == Some("text")))
                        .unwrap_or(true);
                    // Require tool support when the field is present; absent => don't
                    // penalize on missing metadata.
                    let supports_tools =
                        match m.get("supported_parameters").and_then(|p| p.as_array()) {
                            Some(arr) => arr.iter().any(|v| v.as_str() == Some("tools")),
                            None => true,
                        };
                    Some(OpenRouterModel {
                        id,
                        created,
                        text_output,
                        supports_tools,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(models)
}

/// Curated codex model ids, split by auth mode because the OpenAI backend gates
/// the catalog on it. A ChatGPT (OAuth) login only accepts the safe set; API-key
/// auth can use the wider catalog. The operator's seeded default is unioned into
/// the picker so the supported id can change without a code bump.
const CODEX_MODELS_CHATGPT: &[&str] = &["gpt-5.5"];
const CODEX_MODELS_APIKEY: &[&str] = &[
    "gpt-5.5",
    "gpt-5.5-codex",
    "gpt-5",
    "gpt-5-codex",
    "gpt-5-mini",
];

// Claude Code resolves these aliases to the current model versions. Use the
// aliases, not invented dotted ids; `claude --model` rejects those.
const CLAUDE_MODELS: &[&str] = &["opus", "sonnet", "haiku"];

/// How the agent's codex login authenticates, read from the host-side `auth.json`
/// its `harness_auth` entry points at. Never returns secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexAuthMode {
    ChatGpt,
    ApiKey,
    Unknown,
}

/// Resolve the host-side codex auth/config directory for `agent_id` from its
/// spec's `harness_auth` entry.
fn codex_host_auth_dir(home: &MaturanaHome, agent_id: &str) -> Option<PathBuf> {
    let spec =
        AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md")).ok()?;
    let auth = spec
        .harness_auth
        .iter()
        .find(|a| a.runtime == HarnessRuntime::Codex)?;
    let source = PathBuf::from(&auth.source_path);
    if source.is_absolute() {
        return Some(source);
    }
    // `source_path` is conventionally relative to the maturana project root (the
    // parent of the `.maturana` home dir). Long-running channel daemons may have a
    // different cwd, so prefer the project-root resolution and fall back to cwd.
    let project_root = home.root().parent().map(|p| p.join(&source));
    let cwd_relative = std::env::current_dir().ok().map(|c| c.join(&source));
    [project_root.clone(), cwd_relative]
        .into_iter()
        .flatten()
        .find(|p| p.join("auth.json").exists())
        .or(project_root)
}

/// Detect the codex auth mode from `<dir>/auth.json`.
pub(super) fn codex_auth_mode_from_dir(dir: &Path) -> CodexAuthMode {
    let Ok(raw) = fs::read_to_string(dir.join("auth.json")) else {
        return CodexAuthMode::Unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return CodexAuthMode::Unknown;
    };
    if let Some(mode) = value.get("auth_mode").and_then(|m| m.as_str()) {
        match mode.to_ascii_lowercase().as_str() {
            "chatgpt" => return CodexAuthMode::ChatGpt,
            "apikey" => return CodexAuthMode::ApiKey,
            _ => {}
        }
    }
    let has_tokens = value.get("tokens").map(|t| !t.is_null()).unwrap_or(false);
    let has_api_key = value
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    if has_tokens {
        CodexAuthMode::ChatGpt
    } else if has_api_key {
        CodexAuthMode::ApiKey
    } else {
        CodexAuthMode::Unknown
    }
}

/// The operator's seeded default model from `<dir>/config.toml`.
fn codex_default_model(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join("config.toml")).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        if let Some((key, val)) = line.split_once('=') {
            if key.trim() == "model" {
                let val = val.trim().trim_matches(|c| c == '"' || c == '\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

/// Curated codex model set for a host-auth `dir`, unioned with the seeded
/// default. Unknown/unreadable auth falls back to the ChatGPT-safe set.
pub(super) fn codex_models_for_auth(dir: Option<&Path>) -> Vec<String> {
    let mode = dir
        .map(codex_auth_mode_from_dir)
        .unwrap_or(CodexAuthMode::Unknown);
    let base: &[&str] = match mode {
        CodexAuthMode::ApiKey => CODEX_MODELS_APIKEY,
        CodexAuthMode::ChatGpt | CodexAuthMode::Unknown => CODEX_MODELS_CHATGPT,
    };
    let mut out: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    if let Some(def) = dir.and_then(codex_default_model) {
        if !out.iter().any(|m| m == &def) {
            out.insert(0, def);
        }
    }
    out
}

fn codex_models(home: &MaturanaHome, agent_id: &str) -> Vec<String> {
    codex_models_for_auth(codex_host_auth_dir(home, agent_id).as_deref())
}

pub(super) fn harness_label(home: &MaturanaHome, agent_id: &str) -> String {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    match AgentSpec::from_maturana_markdown(&spec_path) {
        Ok(spec) => match spec.runtime.harness {
            HarnessRuntime::Codex => "codex",
            HarnessRuntime::ClaudeCode => "claude-code",
            HarnessRuntime::Opencode => "opencode",
        }
        .to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Models offered as tappable buttons in the interactive selector.
pub(super) fn model_button_choices(home: &MaturanaHome, agent_id: &str) -> Vec<String> {
    match harness_label(home, agent_id).as_str() {
        "opencode" => fetch_openrouter_catalog()
            .map(|models| recent_openrouter_models(&models, 20))
            .unwrap_or_default(),
        "claude-code" => CLAUDE_MODELS.iter().map(|s| s.to_string()).collect(),
        _ => codex_models(home, agent_id),
    }
}

/// Live OpenRouter catalog for OpenCode/OpenRouter; a short curated set otherwise.
pub(super) fn models_text(home: &MaturanaHome, agent_id: &str) -> String {
    let settings = load_channel_settings(home, agent_id);
    let current = settings
        .model
        .clone()
        .unwrap_or_else(|| "(harness default)".to_string());
    let harness = harness_label(home, agent_id);
    let body = if harness == "opencode" {
        match fetch_openrouter_catalog() {
            Ok(models) if !models.is_empty() => {
                let shown = recent_openrouter_models(&models, 30);
                format!(
                    "OpenRouter — {} most recent chat models (newest first):\n{}",
                    shown.len(),
                    shown.join("\n")
                )
            }
            Ok(_) => "OpenRouter returned no models.".to_string(),
            Err(error) => format!("Could not fetch OpenRouter catalog: {error:#}"),
        }
    } else if harness == "codex" {
        let dir = codex_host_auth_dir(home, agent_id);
        let mode = dir
            .as_deref()
            .map(codex_auth_mode_from_dir)
            .unwrap_or(CodexAuthMode::Unknown);
        let models = codex_models_for_auth(dir.as_deref()).join(", ");
        let note = match mode {
            CodexAuthMode::ChatGpt => {
                "ChatGPT login: only gpt-5.5 is accepted — vary effort with /reasoning"
            }
            CodexAuthMode::ApiKey => "API-key login: any catalog id also works via /model <id>",
            CodexAuthMode::Unknown => "could not read codex auth; showing the ChatGPT-safe default",
        };
        format!("Codex models: {models}\n({note})")
    } else {
        format!("claude-code models: {}", CLAUDE_MODELS.join(", "))
    };
    format!("Current: {current}\nSet with /model <id>\n\n{body}")
}
