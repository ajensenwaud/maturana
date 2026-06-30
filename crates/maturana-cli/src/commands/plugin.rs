use std::path::PathBuf;

use clap::{Args, Subcommand};
use maturana_core::state::MaturanaHome;
use maturana_ops::plugins;

#[derive(Debug, Args)]
pub struct PluginCommand {
    #[command(subcommand)]
    pub command: PluginSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum PluginSubcommand {
    /// List plugins discovered from the workspace and Maturana home.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one plugin manifest and validation state.
    Inspect {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate a plugin directory or manifest path.
    Validate {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Validate and copy a local plugin into the active Maturana home.
    Install {
        path: PathBuf,
        /// Enable the plugin after installation.
        #[arg(long)]
        enable: bool,
        /// Replace an existing installed plugin with the same name.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show plugin search roots in precedence order.
    Roots {
        #[arg(long)]
        json: bool,
    },
    /// List assets contributed by enabled plugins after feature gates are applied.
    Assets {
        /// Filter to one asset kind, such as skill, tool, or command.
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Enable a plugin or one feature within it.
    Enable {
        name: String,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Disable a plugin or one feature within it.
    Disable {
        name: String,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub fn handle_plugin(command: PluginCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        PluginSubcommand::List { json } => list_plugins(home, json),
        PluginSubcommand::Inspect { name, json } => inspect_plugin(home, &name, json),
        PluginSubcommand::Validate { path, json } => validate_plugin(&path, json),
        PluginSubcommand::Install {
            path,
            enable,
            force,
            json,
        } => install_plugin(home, &path, enable, force, json),
        PluginSubcommand::Roots { json } => print_roots(home, json),
        PluginSubcommand::Assets { kind, json } => list_assets(home, kind.as_deref(), json),
        PluginSubcommand::Enable {
            name,
            feature,
            json,
        } => set_plugin_enabled(home, &name, feature.as_deref(), true, json),
        PluginSubcommand::Disable {
            name,
            feature,
            json,
        } => set_plugin_enabled(home, &name, feature.as_deref(), false, json),
    }
}

fn list_plugins(home: &MaturanaHome, json: bool) -> anyhow::Result<()> {
    let catalog = plugins::list_plugin_statuses(home)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }
    if catalog.is_empty() {
        println!("No plugins found.");
        println!("Search roots:");
        for root in plugins::default_plugin_roots(home) {
            println!("  {}: {}", root.scope, root.path.display());
        }
        return Ok(());
    }
    println!(
        "{:<28} {:<10} {:<10} {:<8} {:<8} {:<8} ROOT",
        "PLUGIN", "VERSION", "SCOPE", "FEATURES", "ENABLED", "STATUS"
    );
    for plugin in catalog {
        println!(
            "{:<28} {:<10} {:<10} {:<8} {:<8} {:<8} {}",
            plugin.manifest.name,
            plugin.manifest.version,
            plugin.scope,
            plugin.manifest.features.len(),
            if plugin.enabled { "yes" } else { "no" },
            if plugin.validation.valid {
                "ok"
            } else {
                "invalid"
            },
            plugin.root.display(),
        );
    }
    Ok(())
}

fn inspect_plugin(home: &MaturanaHome, name: &str, json: bool) -> anyhow::Result<()> {
    let Some(plugin) = plugins::inspect_plugin_status(home, name)? else {
        anyhow::bail!("plugin not found: {name}");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&plugin)?);
        return Ok(());
    }
    println!("plugin: {}", plugin.manifest.name);
    println!("version: {}", plugin.manifest.version);
    println!("scope: {}", plugin.scope);
    println!("root: {}", plugin.root.display());
    println!("manifest: {}", plugin.manifest_path.display());
    println!("enabled: {}", plugin.enabled);
    println!("description: {}", plugin.manifest.description);
    if !plugin.features.is_empty() {
        println!("features:");
        for feature in &plugin.features {
            let enabled = if feature.default_enabled {
                " default"
            } else {
                ""
            };
            println!(
                "  {} [{}{}] {} - {}",
                feature.name,
                feature.kind,
                enabled,
                if feature.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                feature.description
            );
        }
    }
    if !plugin.manifest.skills.is_empty() {
        println!("skills:");
        for skill in &plugin.manifest.skills {
            println!(
                "  {}{} -> {}",
                skill.name,
                feature_suffix(skill.feature.as_deref()),
                skill.path
            );
        }
    }
    if !plugin.manifest.tools.is_empty() {
        println!("tools:");
        for tool in &plugin.manifest.tools {
            println!(
                "  {}{} -> {}",
                tool.name,
                feature_suffix(tool.feature.as_deref()),
                tool.path
            );
        }
    }
    if !plugin.manifest.commands.is_empty() {
        println!("commands:");
        for command in &plugin.manifest.commands {
            println!(
                "  {}{} -> {}",
                command.name,
                feature_suffix(command.feature.as_deref()),
                command.entrypoint
            );
        }
    }
    if !plugin.effective_permissions.filesystem.is_empty()
        || !plugin.effective_permissions.egress.is_empty()
        || !plugin.effective_permissions.secrets.is_empty()
    {
        println!("effective permissions:");
        print_permission_list("filesystem", &plugin.effective_permissions.filesystem);
        print_permission_list("egress", &plugin.effective_permissions.egress);
        print_permission_list("secrets", &plugin.effective_permissions.secrets);
    }
    print_validation(&plugin.validation);
    Ok(())
}

fn set_plugin_enabled(
    home: &MaturanaHome,
    name: &str,
    feature: Option<&str>,
    enabled: bool,
    json: bool,
) -> anyhow::Result<()> {
    let status = if let Some(feature) = feature {
        plugins::set_plugin_feature_enabled(home, name, feature, enabled)?
    } else {
        plugins::set_plugin_enabled(home, name, enabled)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    println!(
        "{} {}",
        if enabled { "enabled" } else { "disabled" },
        match feature {
            Some(feature) => format!("feature {}/{}", status.manifest.name, feature),
            None => format!("plugin {}", status.manifest.name),
        }
    );
    println!("config: {}", plugins::plugin_config_path(home).display());
    Ok(())
}

fn validate_plugin(path: &PathBuf, json: bool) -> anyhow::Result<()> {
    let report = plugins::validate_plugin_path(path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_validation(&report);
    }
    if !report.valid {
        anyhow::bail!("plugin is invalid");
    }
    Ok(())
}

fn install_plugin(
    home: &MaturanaHome,
    path: &PathBuf,
    enable: bool,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    let result =
        plugins::install_plugin(home, path, plugins::PluginInstallOptions { enable, force })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "installed plugin {} v{}",
        result.status.manifest.name, result.status.manifest.version
    );
    println!("source: {}", result.source.display());
    println!("destination: {}", result.destination.display());
    println!("enabled: {}", result.status.enabled);
    if result.replaced {
        println!("replaced: true");
    }
    Ok(())
}

fn print_roots(home: &MaturanaHome, json: bool) -> anyhow::Result<()> {
    let roots = plugins::default_plugin_roots(home);
    if json {
        println!("{}", serde_json::to_string_pretty(&roots)?);
    } else {
        for root in roots {
            println!("{}: {}", root.scope, root.path.display());
        }
    }
    Ok(())
}

fn list_assets(home: &MaturanaHome, kind: Option<&str>, json: bool) -> anyhow::Result<()> {
    let mut assets = plugins::enabled_plugin_assets(home)?;
    if let Some(kind) = kind {
        assets.retain(|asset| asset.kind == kind);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&assets)?);
        return Ok(());
    }

    if assets.is_empty() {
        println!("No enabled plugin assets.");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<24} {:<24} PATH",
        "PLUGIN", "KIND", "ASSET", "FEATURE"
    );
    for asset in assets {
        println!(
            "{:<24} {:<10} {:<24} {:<24} {}",
            asset.plugin,
            asset.kind,
            asset.name,
            asset.feature.unwrap_or_else(|| "-".to_string()),
            asset.path.display()
        );
    }
    Ok(())
}

fn print_validation(report: &plugins::PluginValidationReport) {
    println!("valid: {}", report.valid);
    for error in &report.errors {
        println!("error: {error}");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

fn print_permission_list(label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    println!("  {label}: {}", values.join(", "));
}

fn feature_suffix(feature: Option<&str>) -> String {
    feature
        .map(|feature| format!(" [feature:{feature}]"))
        .unwrap_or_default()
}
