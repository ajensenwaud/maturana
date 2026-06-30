use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::{audit::AuditEvent, state::MaturanaHome};
use std::fs;

#[derive(Debug, Args)]
pub(crate) struct AuditCommand {
    #[command(subcommand)]
    pub(crate) command: AuditSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AuditSubcommand {
    List {
        agent_id: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn handle_audit(command: AuditCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        AuditSubcommand::List {
            agent_id,
            limit,
            json,
        } => {
            let events = read_agent_audit_events(home, &agent_id)?;
            let start = events.len().saturating_sub(limit);
            let events = &events[start..];
            if json {
                println!("{}", serde_json::to_string_pretty(events)?);
            } else if events.is_empty() {
                println!("no audit events for {agent_id}");
            } else {
                for event in events {
                    println!(
                        "{} {} {}",
                        event.at.to_rfc3339(),
                        event.action,
                        event.message
                    );
                }
            }
        }
    }
    Ok(())
}

fn read_agent_audit_events(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<Vec<AuditEvent>> {
    let path = home.audit_dir().join(format!("{agent_id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read audit log {}", path.display()))?;
    let mut events = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: AuditEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse audit log {} at line {}",
                path.display(),
                index + 1
            )
        })?;
        events.push(event);
    }
    Ok(events)
}
