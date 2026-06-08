use crate::{
    audit::{append_event, AuditEvent},
    providers::{
        firecracker::FirecrackerProvider, hyperv::HyperVProvider, Provider, ProviderCommand,
    },
    spec::{AgentSpec, HostProvider},
    state::MaturanaHome,
    validation::{validate_spec, ValidationReport},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedAgent {
    pub agent_id: String,
    pub agent_dir: PathBuf,
    pub validation: ValidationReport,
    pub provider_commands: Vec<ProviderCommand>,
}

pub fn materialize_agent(
    spec: &AgentSpec,
    source_markdown: &str,
    home: &MaturanaHome,
    mode: LaunchMode,
) -> anyhow::Result<MaterializedAgent> {
    let validation = validate_spec(spec);
    if !validation.valid {
        anyhow::bail!("spec validation failed: {}", validation.errors.join("; "));
    }

    let agent_dir = home.agent_dir(&spec.identity.id);
    fs::create_dir_all(agent_dir.join("state"))?;
    fs::create_dir_all(agent_dir.join("workspace"))?;
    fs::create_dir_all(agent_dir.join("memory"))?;
    fs::create_dir_all(agent_dir.join("snapshots"))?;

    fs::write(agent_dir.join("MATURANA.md"), source_markdown)?;
    fs::write(agent_dir.join("AGENTS.md"), render_guest_agents(spec))?;
    fs::write(agent_dir.join("SOUL.md"), render_soul(spec))?;

    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };

    let commands = provider.plan_launch(spec, &agent_dir)?;
    fs::write(
        agent_dir.join("launch-plan.json"),
        serde_json::to_string_pretty(&commands)?,
    )?;

    append_event(
        home.audit_dir().join(format!("{}.jsonl", spec.identity.id)),
        &AuditEvent {
            at: Utc::now(),
            agent_id: spec.identity.id.clone(),
            action: match mode {
                LaunchMode::DryRun => "launch.dry-run".to_string(),
                LaunchMode::Apply => "launch.apply".to_string(),
            },
            message: format!("materialized {}", agent_dir.display()),
        },
    )?;

    if mode == LaunchMode::Apply {
        provider.launch(spec, &agent_dir)?;
    }

    Ok(MaterializedAgent {
        agent_id: spec.identity.id.clone(),
        agent_dir,
        validation,
        provider_commands: commands,
    })
}

fn render_guest_agents(spec: &AgentSpec) -> String {
    format!(
        "# {}\n\nYou are a Maturana worker agent.\n\nPurpose: {}\n\nOperate only inside the mounted workspace and obey the MATURANA.md contract.\n",
        spec.identity.name, spec.identity.purpose
    )
}

fn render_soul(spec: &AgentSpec) -> String {
    format!(
        "# {}\n\nDefault posture: secure, bounded, inspectable, and reversible.\n\nNever request credentials directly. Use declared credential sources only.\n",
        spec.identity.name
    )
}
