//! Worker roles for multi-agent orchestration, and the small set of control
//! markers agents use to talk to the orchestration loop.
//!
//! A *role* is just a name, a short instruction prefix that is prepended to a
//! step's task text, an optional model override, and where the role runs:
//! either a spec template the orchestrator spawns a dedicated microVM from, or
//! an existing standing agent to reuse. Roles never rewrite an agent's identity
//! files (`AGENTS.md` / `SOUL.md` / `MEMORY.md`) — the instruction prefix is
//! added to the task only, so the same standing agent can safely play different
//! roles across runs without being permanently mutated.
//!
//! The defaults are deliberately harness-agnostic and useful out of the box for
//! any Maturana install; an operator can override or extend them with a
//! `roles.toml`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Control markers an agent prints on its own line so the host loop can act on a
/// reply deterministically (the same idea as the proactivity silence sentinel).
/// They are matched as exact substrings of a reply; everything else is treated
/// as the reply body.
pub mod marker {
    /// The synthesizer prints this when the goal is fully met; the loop stops.
    pub const DONE: &str = "[[ORCH_DONE]]";
    /// Any role prints this when it cannot proceed and needs a human; the loop
    /// stops and surfaces the reason that follows the marker.
    pub const BLOCKED: &str = "[[ORCH_BLOCKED]]";
    /// A reviewer prints this when a worker's result meets the acceptance
    /// criteria; the reviewed step is accepted.
    pub const REVIEW_APPROVE: &str = "[[REVIEW: APPROVE]]";
    /// A reviewer prints this (followed by feedback) when a result needs another
    /// pass; the original worker is re-sent the step with the feedback appended,
    /// bounded by `max_review_cycles`.
    pub const REVIEW_REVISE: &str = "[[REVIEW: REVISE]]";
}

/// Where a role's work runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolePlacement {
    /// Spawn a dedicated microVM for this role from a named spec template. This
    /// is the default model — the orchestrator is not limited to whatever agents
    /// happen to be running; it brings up specialized workers as needed and
    /// tears them down after, bounded by `max_concurrent_vms`.
    Spawn { base_spec: String },
    /// Reuse an existing standing agent by id (cheaper; no boot cost; but that
    /// agent must be idle of orchestration work, enforced by the loop's per-VM
    /// single-flight). Handy on small installs that don't want to spawn VMs.
    Reuse { agent_id: String },
}

/// One worker role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// Stable lowercase name, e.g. "researcher". How steps address the role.
    pub name: String,
    /// One line on what this role is for (shown in status + docs).
    pub description: String,
    /// Instruction text prepended to every task sent to this role. Never written
    /// into the agent's identity files — added to the task only.
    pub system_prompt: String,
    /// Optional per-turn model override for this role (None = the agent default).
    #[serde(default)]
    pub model: Option<String>,
    /// Where this role runs.
    pub placement: RolePlacement,
}

/// The set of roles available to a run, keyed by role name. Loaded from
/// `roles.toml` if present, otherwise the built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRegistry {
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
}

impl RoleRegistry {
    /// The built-in, harness-agnostic default roles. `default_base_spec` is the
    /// spec template new role VMs are spawned from (so a fresh install works
    /// without a `roles.toml`); operators point it at whatever base agent they
    /// want specialized.
    pub fn defaults(default_base_spec: &str) -> Self {
        let spawn = |s: &str| RolePlacement::Spawn { base_spec: s.to_string() };
        let role = |name: &str, description: &str, prompt: &str| Role {
            name: name.to_string(),
            description: description.to_string(),
            system_prompt: prompt.to_string(),
            model: None,
            placement: spawn(default_base_spec),
        };
        let mut roles = BTreeMap::new();
        roles.insert(
            "coordinator".to_string(),
            role(
                "coordinator",
                "Breaks the goal into steps with dependencies and decides when it is done.",
                "You are the COORDINATOR. Break the goal into the smallest sufficient list of \
                 concrete steps, note which steps depend on which, and assign each to a role. \
                 Keep the plan as small as will satisfy the goal. When the work is complete, \
                 reply with the final answer and end with the marker on its own line.",
            ),
        );
        roles.insert(
            "researcher".to_string(),
            role(
                "researcher",
                "Gathers facts and source material.",
                "You are the RESEARCHER. Gather only the information the task asks for, cite \
                 sources, and return a tight, factual result. Do not write the final answer.",
            ),
        );
        roles.insert(
            "developer".to_string(),
            role(
                "developer",
                "Writes and edits code or concrete artifacts.",
                "You are the DEVELOPER. Produce the specific artifact the task asks for \
                 (code, file, command). Be concrete and minimal; return exactly what was asked.",
            ),
        );
        roles.insert(
            "reviewer".to_string(),
            role(
                "reviewer",
                "Checks a result against acceptance criteria and approves or returns it.",
                "You are the REVIEWER. Check the result against the stated acceptance criteria. \
                 If it passes, end your reply with [[REVIEW: APPROVE]] on its own line. If it \
                 needs another pass, end with [[REVIEW: REVISE]] followed by specific, \
                 actionable feedback.",
            ),
        );
        roles.insert(
            "synthesizer".to_string(),
            role(
                "synthesizer",
                "Combines step results into the final answer for the user.",
                "You are the SYNTHESIZER. Combine the completed step results into one clear \
                 final answer for the user. When the answer is complete, end with [[ORCH_DONE]] \
                 on its own line. If you cannot complete it, end with [[ORCH_BLOCKED]] and the \
                 reason.",
            ),
        );
        Self { roles }
    }

    /// Load roles from `path` (TOML) if it exists, otherwise the defaults. A
    /// partial file is merged over the defaults so an operator can override one
    /// role without restating them all.
    pub fn load_or_default(path: &Path, default_base_spec: &str) -> anyhow::Result<Self> {
        let mut registry = Self::defaults(default_base_spec);
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let overrides: RoleRegistry = toml::from_str(&raw)?;
            for (name, role) in overrides.roles {
                registry.roles.insert(name, role);
            }
        }
        Ok(registry)
    }

    pub fn get(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    /// All role names, sorted — for plan validation and `--help`/status output.
    pub fn names(&self) -> Vec<String> {
        self.roles.keys().cloned().collect()
    }

    /// Build the full task text sent to a role's agent: the role's instruction
    /// prefix, then the task. This is the ONLY place a role's persona is applied
    /// — it is never written to the agent's identity files.
    pub fn frame_task(&self, role_name: &str, task: &str) -> Option<String> {
        self.get(role_name)
            .map(|role| format!("{}\n\n--- TASK ---\n{}", role.system_prompt.trim(), task.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_the_core_roles_and_spawn_by_default() {
        let reg = RoleRegistry::defaults("worker-base");
        for name in ["coordinator", "researcher", "developer", "reviewer", "synthesizer"] {
            let role = reg.get(name).unwrap_or_else(|| panic!("missing role {name}"));
            // Default placement spawns a dedicated VM (not tied to standing agents).
            assert_eq!(
                role.placement,
                RolePlacement::Spawn { base_spec: "worker-base".to_string() }
            );
        }
    }

    #[test]
    fn review_and_done_prompts_carry_their_markers() {
        let reg = RoleRegistry::defaults("worker-base");
        assert!(reg.get("reviewer").unwrap().system_prompt.contains(marker::REVIEW_APPROVE));
        assert!(reg.get("reviewer").unwrap().system_prompt.contains(marker::REVIEW_REVISE));
        assert!(reg.get("synthesizer").unwrap().system_prompt.contains(marker::DONE));
    }

    #[test]
    fn frame_task_prepends_the_role_prompt_only() {
        let reg = RoleRegistry::defaults("worker-base");
        let framed = reg.frame_task("researcher", "find the population of Paris").unwrap();
        assert!(framed.starts_with("You are the RESEARCHER"));
        assert!(framed.contains("find the population of Paris"));
        // An unknown role yields nothing rather than a bare task with no guidance.
        assert!(reg.frame_task("nope", "x").is_none());
    }

    #[test]
    fn toml_overrides_merge_over_defaults() {
        // Override just the developer to reuse a standing agent; others stay.
        let toml_src = r#"
[roles.developer]
name = "developer"
description = "custom"
system_prompt = "Be terse."
[roles.developer.placement]
reuse = { agent_id = "codex-firecracker" }
"#;
        let dir = std::env::temp_dir().join(format!("roles-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roles.toml");
        std::fs::write(&path, toml_src).unwrap();

        let reg = RoleRegistry::load_or_default(&path, "worker-base").unwrap();
        // The override applied...
        assert_eq!(
            reg.get("developer").unwrap().placement,
            RolePlacement::Reuse { agent_id: "codex-firecracker".to_string() }
        );
        // ...and the un-overridden roles kept their spawn defaults.
        assert_eq!(
            reg.get("researcher").unwrap().placement,
            RolePlacement::Spawn { base_spec: "worker-base".to_string() }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
