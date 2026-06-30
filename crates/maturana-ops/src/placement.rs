use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use maturana_core::{orchestrator_budget::SlotCounter, roles::RolePlacement};
use maturana_core::{roles::RoleRegistry, state::MaturanaHome, AgentSpec, HarnessRuntime};

/// How an orchestration run picks workers. Mutually resolved in priority order:
/// roles file, dedicated base spec, named reusable agents, then auto-discovery.
#[derive(Debug, Clone, Default)]
pub struct PlacementChoice {
    pub roles_file: Option<PathBuf>,
    pub agents: Option<String>,
    pub base_spec: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlacementResolution {
    pub registry: RoleRegistry,
    pub base_spec: String,
    pub status_line: Option<String>,
}

/// A resolved place to run a role's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub agent_id: String,
    pub model: Option<String>,
}

struct SpawnedVm {
    agent_id: String,
    tap_name: String,
}

/// Resolves each role to a concrete worker once per run, caching role placement
/// and tracking spawned worker VMs for teardown.
pub struct WorkerPool<'a> {
    home: &'a MaturanaHome,
    registry: &'a RoleRegistry,
    run_id: String,
    base_spec: String,
    cache: std::collections::HashMap<String, Worker>,
    spawned: Vec<SpawnedVm>,
    vm_slots: SlotCounter,
    spawn_elapsed: Duration,
}

impl<'a> WorkerPool<'a> {
    pub fn new(
        home: &'a MaturanaHome,
        registry: &'a RoleRegistry,
        run_id: String,
        base_spec: String,
        max_vms: u32,
    ) -> Self {
        Self {
            home,
            registry,
            run_id,
            base_spec,
            cache: std::collections::HashMap::new(),
            spawned: Vec::new(),
            vm_slots: SlotCounter::new(max_vms),
            spawn_elapsed: Duration::ZERO,
        }
    }

    /// Total time spent spawning VMs so far, excluded from the work wall budget.
    pub fn spawn_elapsed(&self) -> Duration {
        self.spawn_elapsed
    }

    pub fn resolve(&mut self, role_name: &str) -> anyhow::Result<Worker> {
        if let Some(worker) = self.cache.get(role_name) {
            return Ok(worker.clone());
        }
        let role = self
            .registry
            .get(role_name)
            .ok_or_else(|| anyhow::anyhow!("unknown role '{role_name}'"))?;
        let model = role.model.clone();
        let worker = match role.placement.clone() {
            RolePlacement::Reuse { agent_id } => Worker { agent_id, model },
            RolePlacement::Spawn { .. } => {
                if !self.vm_slots.try_acquire() {
                    anyhow::bail!(
                        "the concurrent worker-VM cap is reached; raise --max-vms or use fewer roles"
                    );
                }
                let used = maturana_core::orchestrator_spawn::used_host_octets(
                    self.home,
                    maturana_core::orchestrator_spawn::DEFAULT_SUBNET,
                );
                let net = maturana_core::orchestrator_spawn::allocate_net(
                    maturana_core::orchestrator_spawn::DEFAULT_SUBNET,
                    &used,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!("no free network address for a spawned worker VM")
                })?;
                let new_id = format!("orch-{}-{}", self.run_id, role_name);
                let session_id = format!("{new_id}-main");
                println!(
                    "  spawning a specialized VM for role '{role_name}' (cloning {})",
                    self.base_spec
                );
                let spawn_start = Instant::now();
                crate::firecracker::orchestrator_spawn_worker(
                    self.home,
                    &self.base_spec,
                    &new_id,
                    &session_id,
                    &net,
                )?;
                self.spawn_elapsed += spawn_start.elapsed();
                self.spawned.push(SpawnedVm {
                    agent_id: new_id.clone(),
                    tap_name: net.tap_name.clone(),
                });
                Worker {
                    agent_id: new_id,
                    model,
                }
            }
        };
        self.cache.insert(role_name.to_string(), worker.clone());
        Ok(worker)
    }

    /// Resolve a board/card assignee: known role names go through placement,
    /// unknown names are concrete reusable agent ids, and empty assignees use
    /// the default developer role.
    pub fn resolve_assignee(&mut self, assignee: Option<&str>) -> anyhow::Result<Worker> {
        match assignee {
            Some(assignee) if self.registry.get(assignee).is_some() => self.resolve(assignee),
            Some(agent_id) => Ok(Worker {
                agent_id: agent_id.to_string(),
                model: None,
            }),
            None => self.resolve("developer"),
        }
    }

    pub fn teardown(&mut self) {
        for vm in std::mem::take(&mut self.spawned) {
            println!("  tearing down spawned worker {}", vm.agent_id);
            let _ = crate::firecracker::orchestrator_teardown_worker(
                self.home,
                &vm.agent_id,
                &vm.tap_name,
            );
        }
    }
}

/// Reusable standing agents, strongest-coder first. Reads materialized specs
/// under `<home>/agents`, skips orchestrator-spawned workers (`orch-*`), and
/// orders by harness: Codex, Claude Code, OpenCode, then unknown.
pub fn discover_reusable_agents(home: &MaturanaHome) -> Vec<String> {
    let rank = |harness: Option<HarnessRuntime>| match harness {
        Some(HarnessRuntime::Codex) => 0u8,
        Some(HarnessRuntime::ClaudeCode) => 1,
        Some(HarnessRuntime::Opencode) => 2,
        None => 3,
    };
    let Ok(entries) = std::fs::read_dir(home.agents_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<(u8, String)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if id.starts_with("orch-") {
            continue;
        }
        let spec_path = entry.path().join("MATURANA.md");
        if !spec_path.exists() {
            continue;
        }
        let harness = AgentSpec::from_maturana_markdown(&spec_path)
            .ok()
            .map(|s| s.runtime.harness);
        found.push((rank(harness), id));
    }
    found.sort();
    found.into_iter().map(|(_, id)| id).collect()
}

pub fn resolve_role_registry(
    home: &MaturanaHome,
    placement: &PlacementChoice,
) -> anyhow::Result<PlacementResolution> {
    if let Some(path) = &placement.roles_file {
        let base_spec = placement
            .base_spec
            .clone()
            .unwrap_or_else(|| "worker-base".to_string());
        return Ok(PlacementResolution {
            registry: RoleRegistry::load_or_default(path, &base_spec)?,
            base_spec,
            status_line: None,
        });
    }
    if let Some(spec) = &placement.base_spec {
        return Ok(PlacementResolution {
            registry: RoleRegistry::defaults(spec),
            base_spec: spec.clone(),
            status_line: Some(format!(
                "placement: spawning dedicated VMs (cloning {spec})"
            )),
        });
    }
    let agents: Vec<String> = match &placement.agents {
        Some(csv) => csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => discover_reusable_agents(home),
    };
    if agents.is_empty() {
        anyhow::bail!(
            "no running agents to reuse. Launch agents first (`maturana list` to see them), \
             pass --agents <id,id>, or spawn dedicated VMs with --base-spec <agent-or-spec>."
        );
    }
    Ok(PlacementResolution {
        registry: RoleRegistry::reuse_across(&agents),
        base_spec: String::new(),
        status_line: Some(format!("placement: reusing agents {}", agents.join(", "))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::roles::RolePlacement;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home(name: &str) -> (PathBuf, MaturanaHome) {
        let root = std::env::temp_dir().join(format!(
            "maturana-placement-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = MaturanaHome::new(&root);
        (root, home)
    }

    fn write_spec(home: &MaturanaHome, id: &str, harness: &str) {
        let dir = home.agent_dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("MATURANA.md"),
            format!(
                "---\nidentity: {{ id: {id}, name: {id}, purpose: test }}\n\
                 runtime: {{ harness: {harness} }}\n\
                 vm: {{ provider: firecracker, guest_os: linux }}\n---\n# {id}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn discover_reusable_agents_orders_by_harness_and_skips_orch_workers() {
        let (root, home) = temp_home("discover");
        write_spec(&home, "zed-open", "opencode");
        write_spec(&home, "beta-claude", "claude-code");
        write_spec(&home, "alpha-codex", "codex");
        write_spec(&home, "orch-old-developer", "codex");

        assert_eq!(
            discover_reusable_agents(&home),
            vec![
                "alpha-codex".to_string(),
                "beta-claude".to_string(),
                "zed-open".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_registry_reuses_explicit_agents_in_csv_order() {
        let (root, home) = temp_home("reuse");
        let resolved = resolve_role_registry(
            &home,
            &PlacementChoice {
                agents: Some("coder, reviewer".to_string()),
                ..PlacementChoice::default()
            },
        )
        .unwrap();

        let developer = resolved.registry.get("developer").unwrap();
        assert_eq!(
            developer.placement,
            RolePlacement::Reuse {
                agent_id: "coder".to_string()
            }
        );
        assert_eq!(
            resolved.status_line.as_deref(),
            Some("placement: reusing agents coder, reviewer")
        );
        assert!(resolved.base_spec.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_registry_carries_spawn_base_spec() {
        let (root, home) = temp_home("spawn");
        let resolved = resolve_role_registry(
            &home,
            &PlacementChoice {
                base_spec: Some("worker-template".to_string()),
                ..PlacementChoice::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.base_spec, "worker-template");
        assert_eq!(
            resolved.status_line.as_deref(),
            Some("placement: spawning dedicated VMs (cloning worker-template)")
        );
        let developer = resolved.registry.get("developer").unwrap();
        assert_eq!(
            developer.placement,
            RolePlacement::Spawn {
                base_spec: "worker-template".to_string()
            }
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worker_pool_resolves_reuse_roles_and_caches_workers() {
        let (root, home) = temp_home("pool-reuse");
        let mut registry = RoleRegistry::reuse_across(&["coder".to_string()]);
        registry.roles.get_mut("developer").unwrap().model = Some("gpt-test".to_string());
        let mut pool = WorkerPool::new(&home, &registry, "run-1".to_string(), String::new(), 1);

        let first = pool.resolve("developer").unwrap();
        let second = pool.resolve("developer").unwrap();

        assert_eq!(
            first,
            Worker {
                agent_id: "coder".to_string(),
                model: Some("gpt-test".to_string())
            }
        );
        assert_eq!(second, first);
        assert_eq!(pool.spawn_elapsed(), Duration::ZERO);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worker_pool_rejects_unknown_roles_before_side_effects() {
        let (root, home) = temp_home("pool-unknown");
        let registry = RoleRegistry::reuse_across(&["coder".to_string()]);
        let mut pool = WorkerPool::new(&home, &registry, "run-1".to_string(), String::new(), 1);

        let error = pool.resolve("missing").unwrap_err().to_string();

        assert!(error.contains("unknown role"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worker_pool_resolves_board_assignees() {
        let (root, home) = temp_home("pool-assignee");
        let registry = RoleRegistry::reuse_across(&["coder".to_string()]);
        let mut pool = WorkerPool::new(&home, &registry, "run-1".to_string(), String::new(), 1);

        assert_eq!(
            pool.resolve_assignee(Some("reviewer")).unwrap(),
            pool.resolve("reviewer").unwrap()
        );
        assert_eq!(
            pool.resolve_assignee(Some("named-agent")).unwrap(),
            Worker {
                agent_id: "named-agent".to_string(),
                model: None
            }
        );
        assert_eq!(
            pool.resolve_assignee(None).unwrap(),
            pool.resolve("developer").unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
