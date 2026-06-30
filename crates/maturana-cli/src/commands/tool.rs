use std::{fs, path::PathBuf};

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use maturana_core::{
    audit::{append_event, AuditEvent},
    state::MaturanaHome,
    tools::{run_tool, Capabilities, ResourceLimits, ToolManifest, ToolRegistry},
};

/// Author, register, and run sandboxed WebAssembly tools that agents build on
/// the fly. Tools live under `<home>/tools/<name>/`.
#[derive(Debug, Args)]
pub struct ToolCommand {
    #[command(subcommand)]
    command: ToolSubcommand,
}

#[derive(Debug, Subcommand)]
enum ToolSubcommand {
    /// Register a compiled `.wasm` module under a manifest into the registry.
    Register {
        name: String,
        #[arg(long)]
        wasm: PathBuf,
        /// Optional manifest JSON; defaults to a pure-compute manifest.
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long, default_value = "0.1.0")]
        version: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// List registered tools.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a tool's manifest.
    Inspect { name: String },
    /// Run a registered tool with a JSON input (requires the wasm-runtime build).
    Run {
        name: String,
        #[arg(long, default_value = "{}")]
        input: String,
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Record this run as a self-improvement trajectory step.
        #[arg(long)]
        agent_id: Option<String>,
    },
}

pub fn run_tool_command(home: &MaturanaHome, command: ToolCommand) -> anyhow::Result<()> {
    let registry = tool_registry(home);
    match command.command {
        ToolSubcommand::Register {
            name,
            wasm,
            manifest,
            version,
            description,
        } => {
            let wasm_bytes = fs::read(&wasm)
                .with_context(|| format!("failed to read wasm {}", wasm.display()))?;
            let manifest = match manifest {
                Some(path) => {
                    let raw = fs::read_to_string(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?;
                    let mut parsed: ToolManifest = serde_json::from_str(&raw)
                        .with_context(|| format!("failed to parse manifest {}", path.display()))?;
                    parsed.name = name.clone();
                    parsed
                }
                None => ToolManifest {
                    name: name.clone(),
                    version,
                    description,
                    wasm: "module.wasm".to_string(),
                    capabilities: Capabilities::default(),
                    limits: ResourceLimits::default(),
                    input_schema: serde_json::Value::Null,
                    output_schema: serde_json::Value::Null,
                },
            };
            let stored = registry.register(&manifest, &wasm_bytes)?;
            audit_agent_event(
                home,
                &name,
                "tool.register",
                format!("registered wasm tool {} v{}", stored.name, stored.version),
            )
            .ok();
            println!(
                "registered tool {} v{} ({})",
                stored.name,
                stored.version,
                registry.tool_dir(&stored.name).display()
            );
            if !stored.capabilities.is_pure() {
                println!(
                    "capabilities: fs_read={:?} fs_write={:?} env={:?} net={:?}",
                    stored.capabilities.fs_read,
                    stored.capabilities.fs_write,
                    stored.capabilities.env,
                    stored.capabilities.net
                );
            }
        }
        ToolSubcommand::List { json } => {
            let tools = registry.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tools)?);
            } else if tools.is_empty() {
                println!("no tools registered under {}", registry.root().display());
            } else {
                for tool in tools {
                    println!(
                        "{} v{} pure={} :: {}",
                        tool.name,
                        tool.version,
                        tool.capabilities.is_pure(),
                        tool.description
                    );
                }
            }
        }
        ToolSubcommand::Inspect { name } => {
            let manifest = registry.load(&name)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        ToolSubcommand::Run {
            name,
            input,
            input_file,
            agent_id,
        } => {
            let input = match input_file {
                Some(path) => fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
                None => input,
            };
            let result = run_tool(&registry, &name, &input)?;
            if let Some(agent_id) = agent_id {
                audit_agent_event(
                    home,
                    &agent_id,
                    "tool.run",
                    format!(
                        "ran tool {} ok={} duration_ms={}",
                        result.tool, result.ok, result.duration_ms
                    ),
                )
                .ok();
            }
            print!("{}", result.stdout);
            if !result.stdout.ends_with('\n') && !result.stdout.is_empty() {
                println!();
            }
            if !result.ok {
                anyhow::bail!("tool {} failed: {}", result.tool, result.stderr.trim());
            }
        }
    }
    Ok(())
}

fn tool_registry(home: &MaturanaHome) -> ToolRegistry {
    ToolRegistry::new(home.root().join("tools"))
}

fn audit_agent_event(
    home: &MaturanaHome,
    agent_id: &str,
    action: &str,
    message: impl Into<String>,
) -> anyhow::Result<()> {
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            message: message.into(),
        },
    )
}
