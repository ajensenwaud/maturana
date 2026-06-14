use crate::{
    audit::{append_event, AuditEvent},
    providers::{
        firecracker::FirecrackerProvider, hyperv::HyperVProvider, LiveAgentStatus, Provider,
        ProviderCommand,
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
    // IDENTITY.md (who the agent is + who its owner is) and SOUL.md (voice,
    // values, behavior) are authored personality files. Scaffold them only when
    // absent so the setup wizard's / user's authored versions are never
    // clobbered on re-materialize.
    write_if_absent(&agent_dir.join("IDENTITY.md"), || render_identity(spec))?;
    write_if_absent(&agent_dir.join("SOUL.md"), || render_soul(spec))?;

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

pub fn stop_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let agent_dir = home.agent_dir(agent_id);
    let spec_path = agent_dir.join("MATURANA.md");
    if !spec_path.exists() {
        anyhow::bail!("agent does not exist or has no MATURANA.md: {agent_id}");
    }
    let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };
    provider.stop(&spec, &agent_dir)?;
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: "agent.stop.live".to_string(),
            message: format!(
                "stopped {} provider agent",
                provider_name(&spec.vm.provider)
            ),
        },
    )?;
    Ok(())
}

pub fn inspect_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<LiveAgentStatus> {
    let agent_dir = home.agent_dir(agent_id);
    let spec_path = agent_dir.join("MATURANA.md");
    if !spec_path.exists() {
        anyhow::bail!("agent does not exist or has no MATURANA.md: {agent_id}");
    }
    let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };
    provider.inspect(&spec, &agent_dir)
}

fn provider_name(provider: &HostProvider) -> &'static str {
    match provider {
        HostProvider::HyperV => "Hyper-V",
        HostProvider::Firecracker => "Firecracker",
    }
}

fn render_guest_agents(spec: &AgentSpec) -> String {
    format!(
        "# {}\n\nYou are a Maturana worker agent.\n\nPurpose: {}\n\nOperate only inside the mounted workspace and obey the MATURANA.md contract.\n",
        spec.identity.name, spec.identity.purpose
    )
}

fn write_if_absent<F: FnOnce() -> String>(path: &std::path::Path, content: F) -> std::io::Result<()> {
    if path.exists() {
        Ok(())
    } else {
        fs::write(path, content())
    }
}

/// Rich scaffold for IDENTITY.md: who the agent is and who its owner is. The
/// setup wizard fills the angle-bracket prompts from the interview; left as-is it
/// still reads as a usable template.
fn render_identity(spec: &AgentSpec) -> String {
    format!(
        "# Identity — {name}\n\
         <!-- id: {id} -->\n\n\
         ## Who I am\n\
         {name} — {purpose}\n\n\
         <Expand: my role, what I help with, and why I exist.>\n\n\
         ## Who you are to me\n\
         <Your owner: name, how to address you, timezone, working hours, and what\n\
         you rely on me for.>\n\n\
         ## Scope & boundaries\n\
         - In scope: <what I should do>\n\
         - Out of scope: <what I must not do without asking>\n\n\
         ## How we work together\n\
         <Channels you reach me on, when to ping you, response expectations.>\n",
        name = spec.identity.name,
        id = spec.identity.id,
        purpose = spec.identity.purpose,
    )
}

/// Rich scaffold for SOUL.md: the durable personality + operating posture.
fn render_soul(spec: &AgentSpec) -> String {
    format!(
        "# Soul — {name}\n\n\
         My durable personality and posture across every conversation. Edit freely.\n\n\
         ## Voice\n\
         <Tone, formality, brevity, humor — how I should sound.>\n\n\
         ## Values\n\
         - Secure, bounded, inspectable, and reversible by default.\n\
         - <Your values…>\n\n\
         ## Behavior\n\
         - Do: <…>\n\
         - Don't: <…>\n\
         - Never request credentials directly; use declared credential sources only.\n\n\
         ## Memory & continuity\n\
         I persist durable facts to memory and shared context to the wiki; I do not\n\
         rely on the chat window to remember.\n",
        name = spec.identity.name,
    )
}
