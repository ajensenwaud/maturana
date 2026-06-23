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
    /// The verifier prints this after actually running the produced files and
    /// finding they work — the deliverable is accepted as runnable.
    pub const VERIFY_PASS: &str = "[[VERIFY: PASS]]";
    /// The verifier prints this (followed by what's broken) after running the
    /// produced files and finding they do NOT work; it fixes them in place and
    /// the loop re-verifies, bounded by `max_review_cycles`.
    pub const VERIFY_FAIL: &str = "[[VERIFY: FAIL]]";
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

/// A partial role override read from `roles.toml` — every field optional so an
/// operator can change just one thing (commonly the placement) without restating
/// the whole role. Merged over the defaults by [`RoleRegistry::load_or_default`].
#[derive(Debug, Clone, Default, Deserialize)]
struct RolePatch {
    name: Option<String>,
    description: Option<String>,
    system_prompt: Option<String>,
    model: Option<String>,
    placement: Option<RolePlacement>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RegistryPatch {
    #[serde(default)]
    roles: BTreeMap<String, RolePatch>,
}

impl RoleRegistry {
    /// The five built-in roles as `(name, description, system_prompt)` — the
    /// harness-agnostic personas, with no placement decided yet. Both `defaults`
    /// (spawn) and `reuse_across` (reuse) build from this one list so the prompts
    /// never drift between the two.
    fn core_role_defs() -> [(&'static str, &'static str, &'static str); 5] {
        [
            (
                "coordinator",
                "Breaks the goal into steps with dependencies and decides when it is done.",
                "You are the COORDINATOR. Break the goal into the smallest sufficient list of \
                 concrete steps, note which steps depend on which, and assign each to a role. \
                 Keep the plan as small as will satisfy the goal. When the work is complete, \
                 reply with the final answer and end with the marker on its own line.",
            ),
            (
                "researcher",
                "Gathers facts and source material.",
                "You are the RESEARCHER. Gather only the information the task asks for, cite \
                 sources, and return a tight, factual result. Do not write the final answer.",
            ),
            (
                "developer",
                "Writes and edits code or concrete artifacts.",
                "You are the DEVELOPER. Produce the specific artifact the task asks for \
                 (code, file, command). Be concrete and minimal; return exactly what was asked.",
            ),
            (
                "reviewer",
                "Checks a result against acceptance criteria and approves or returns it.",
                "You are the REVIEWER. Check the result against the stated acceptance criteria. \
                 If it passes, end your reply with [[REVIEW: APPROVE]] on its own line. If it \
                 needs another pass, end with [[REVIEW: REVISE]] followed by specific, \
                 actionable feedback.",
            ),
            (
                "synthesizer",
                "Combines step results into the final answer for the user.",
                "You are the SYNTHESIZER. Combine the completed step results into one clear \
                 final answer for the user. When the answer is complete, end with [[ORCH_DONE]] \
                 on its own line. If you cannot complete it, end with [[ORCH_BLOCKED]] and the \
                 reason.",
            ),
        ]
    }

    /// Build a registry from the core roles, deciding each role's placement with
    /// `placement_for(role_name)`.
    fn from_core(placement_for: impl Fn(&str) -> RolePlacement) -> Self {
        let mut roles = BTreeMap::new();
        for (name, description, prompt) in Self::core_role_defs() {
            roles.insert(
                name.to_string(),
                Role {
                    name: name.to_string(),
                    description: description.to_string(),
                    system_prompt: prompt.to_string(),
                    model: None,
                    placement: placement_for(name),
                },
            );
        }
        Self { roles }
    }

    /// The built-in roles, each spawning a dedicated microVM from `default_base_spec`.
    /// Use this only when you explicitly want on-demand specialized VMs; for the
    /// common case prefer [`reuse_across`](Self::reuse_across).
    pub fn defaults(default_base_spec: &str) -> Self {
        Self::from_core(|_| RolePlacement::Spawn {
            base_spec: default_base_spec.to_string(),
        })
    }

    /// The built-in roles, all REUSING standing agents — the zero-config default
    /// when agents are already running (no `roles.toml`, no VM spawning). Agents
    /// are assigned best-first: pass `agent_ids` ordered strongest-coder-first and
    /// the heavy roles (developer, coordinator) land on `agent_ids[0]`, review +
    /// synthesis on the next, research on the third — wrapping when there are
    /// fewer agents than that. `agent_ids` must be non-empty.
    pub fn reuse_across(agent_ids: &[String]) -> Self {
        assert!(!agent_ids.is_empty(), "reuse_across needs at least one agent");
        let pick = |i: usize| agent_ids[i.min(agent_ids.len() - 1)].clone();
        Self::from_core(|name| {
            let agent_id = match name {
                "developer" | "coordinator" => pick(0),
                "reviewer" | "synthesizer" => pick(1),
                "researcher" => pick(2),
                _ => pick(0),
            };
            RolePlacement::Reuse { agent_id }
        })
    }

    /// Load roles from `path` (TOML) if it exists, otherwise the defaults. The
    /// file is merged over the defaults FIELD BY FIELD: an operator can override
    /// just where a role runs (its placement) or just its model while keeping the
    /// default instruction prompt, e.g.
    ///
    /// ```toml
    /// [roles.researcher.placement]
    /// reuse = { agent_id = "opencode-firecracker" }
    /// ```
    ///
    /// A role name not in the defaults defines a brand-new role and must at least
    /// supply a placement.
    pub fn load_or_default(path: &Path, default_base_spec: &str) -> anyhow::Result<Self> {
        let mut registry = Self::defaults(default_base_spec);
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let patch: RegistryPatch = toml::from_str(&raw)?;
            for (name, fields) in patch.roles {
                let merged = match registry.roles.get(&name).cloned() {
                    Some(mut base) => {
                        if let Some(v) = fields.name {
                            base.name = v;
                        }
                        if let Some(v) = fields.description {
                            base.description = v;
                        }
                        if let Some(v) = fields.system_prompt {
                            base.system_prompt = v;
                        }
                        if fields.model.is_some() {
                            base.model = fields.model;
                        }
                        if let Some(v) = fields.placement {
                            base.placement = v;
                        }
                        base
                    }
                    None => Role {
                        name: fields.name.unwrap_or_else(|| name.clone()),
                        description: fields.description.unwrap_or_default(),
                        system_prompt: fields.system_prompt.unwrap_or_default(),
                        model: fields.model,
                        placement: fields.placement.ok_or_else(|| {
                            anyhow::anyhow!("new role '{name}' in roles.toml needs a placement")
                        })?,
                    },
                };
                registry.roles.insert(name, merged);
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
    fn reuse_across_assigns_heavy_roles_to_the_first_agent() {
        let agents = vec![
            "codex-firecracker".to_string(),
            "claude-firecracker".to_string(),
            "opencode-firecracker".to_string(),
        ];
        let reg = RoleRegistry::reuse_across(&agents);
        // No role spawns; every role reuses a standing agent.
        for name in ["coordinator", "researcher", "developer", "reviewer", "synthesizer"] {
            assert!(matches!(
                reg.get(name).unwrap().placement,
                RolePlacement::Reuse { .. }
            ));
        }
        let agent_of = |n: &str| match &reg.get(n).unwrap().placement {
            RolePlacement::Reuse { agent_id } => agent_id.clone(),
            _ => unreachable!(),
        };
        // Heavy roles on the strongest (first) agent; review/synth next; research third.
        assert_eq!(agent_of("developer"), "codex-firecracker");
        assert_eq!(agent_of("coordinator"), "codex-firecracker");
        assert_eq!(agent_of("reviewer"), "claude-firecracker");
        assert_eq!(agent_of("synthesizer"), "claude-firecracker");
        assert_eq!(agent_of("researcher"), "opencode-firecracker");
    }

    #[test]
    fn reuse_across_wraps_when_fewer_agents_than_roles() {
        let reg = RoleRegistry::reuse_across(&["solo".to_string()]);
        for name in ["coordinator", "researcher", "developer", "reviewer", "synthesizer"] {
            assert_eq!(
                reg.get(name).unwrap().placement,
                RolePlacement::Reuse { agent_id: "solo".to_string() }
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
        // Override ONLY where the developer runs — a field-level patch; the
        // default instruction prompt must be kept.
        let toml_src = r#"
[roles.developer.placement]
reuse = { agent_id = "codex-firecracker" }
"#;
        let dir = std::env::temp_dir().join(format!("roles-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roles.toml");
        std::fs::write(&path, toml_src).unwrap();

        let reg = RoleRegistry::load_or_default(&path, "worker-base").unwrap();
        // The placement override applied...
        assert_eq!(
            reg.get("developer").unwrap().placement,
            RolePlacement::Reuse { agent_id: "codex-firecracker".to_string() }
        );
        // ...while the default DEVELOPER prompt was kept (field-level merge)...
        assert!(reg.get("developer").unwrap().system_prompt.contains("DEVELOPER"));
        // ...and the un-overridden roles kept their spawn defaults.
        assert_eq!(
            reg.get("researcher").unwrap().placement,
            RolePlacement::Spawn { base_spec: "worker-base".to_string() }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
