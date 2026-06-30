use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::{spec::AgentSpec, state::MaturanaHome, validate_spec};

#[derive(Debug, Args)]
pub struct SpecCommand {
    #[command(subcommand)]
    pub command: SpecSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SpecSubcommand {
    Validate {
        #[arg(default_value = "MATURANA.md")]
        spec: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

pub fn handle_spec(command: SpecCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        SpecSubcommand::Validate { spec, json } => {
            let spec = AgentSpec::from_maturana_markdown(&spec)
                .with_context(|| format!("failed to read {}", spec.display()))?;
            let report = validate_spec(&spec);
            let collisions = maturana_core::check_network_collisions(home, &spec);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
                if !collisions.is_empty() {
                    eprintln!("\nnetwork/image collisions:");
                    for collision in &collisions {
                        eprintln!("  ✗ {collision}");
                    }
                }
            }
            if !report.valid {
                anyhow::bail!("spec is invalid");
            }
            if !collisions.is_empty() {
                anyhow::bail!(
                    "spec collides with an existing agent — {}",
                    collisions.join("; ")
                );
            }
            Ok(())
        }
    }
}

fn print_report(report: &maturana_core::ValidationReport) {
    if report.valid {
        println!("valid");
    } else {
        println!("invalid");
    }

    for error in &report.errors {
        println!("error: {error}");
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}
