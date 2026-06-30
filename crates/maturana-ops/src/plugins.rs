use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use maturana_core::state::MaturanaHome;
pub use maturana_plugin::{
    discover_plugins, load_manifest, validate_manifest, DiscoveredPlugin, PluginFeature,
    PluginRoot, PluginValidationReport,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PluginConfig {
    pub plugins: BTreeMap<String, PluginEnablement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PluginEnablement {
    pub enabled: Option<bool>,
    pub features: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginFeatureStatus {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub default_enabled: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginStatus {
    pub manifest: maturana_plugin::PluginManifest,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub scope: String,
    pub validation: PluginValidationReport,
    pub enabled: bool,
    pub features: Vec<PluginFeatureStatus>,
    pub effective_permissions: maturana_plugin::PluginPermissions,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginSkillAsset {
    pub plugin: String,
    pub feature: Option<String>,
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginActiveAsset {
    pub plugin: String,
    pub feature: Option<String>,
    pub kind: String,
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginPermissionGrant {
    pub plugin: String,
    pub filesystem: Vec<String>,
    pub egress: Vec<String>,
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInstallOptions {
    pub enable: bool,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginInstallResult {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub replaced: bool,
    pub status: PluginStatus,
}

pub const BUILTINS_PLUGIN: &str = "maturana-builtins";
pub const CORE_PLUGIN_COMMAND: &str = "plugin";

pub fn repo_root_for_home(home: &MaturanaHome) -> PathBuf {
    home.root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.root().to_path_buf())
}

pub fn default_plugin_roots(home: &MaturanaHome) -> Vec<PluginRoot> {
    let repo_root = repo_root_for_home(home);
    vec![
        PluginRoot::new("workspace", repo_root.join("plugins")),
        PluginRoot::new("home", home.root().join("plugins")),
    ]
}

pub fn list_plugins(home: &MaturanaHome) -> anyhow::Result<Vec<DiscoveredPlugin>> {
    discover_plugins(&default_plugin_roots(home))
}

pub fn inspect_plugin(home: &MaturanaHome, name: &str) -> anyhow::Result<Option<DiscoveredPlugin>> {
    Ok(list_plugins(home)?
        .into_iter()
        .find(|plugin| plugin.manifest.name == name))
}

pub fn plugin_config_path(home: &MaturanaHome) -> PathBuf {
    home.root().join("plugins").join("config.json")
}

pub fn load_plugin_config(home: &MaturanaHome) -> anyhow::Result<PluginConfig> {
    let path = plugin_config_path(home);
    if !path.exists() {
        return Ok(PluginConfig::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

pub fn save_plugin_config(home: &MaturanaHome, config: &PluginConfig) -> anyhow::Result<()> {
    let path = plugin_config_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(config)?)?;
    Ok(())
}

pub fn list_plugin_statuses(home: &MaturanaHome) -> anyhow::Result<Vec<PluginStatus>> {
    let config = load_plugin_config(home)?;
    Ok(list_plugins(home)?
        .into_iter()
        .map(|plugin| plugin_status(plugin, &config))
        .collect())
}

pub fn inspect_plugin_status(
    home: &MaturanaHome,
    name: &str,
) -> anyhow::Result<Option<PluginStatus>> {
    let config = load_plugin_config(home)?;
    Ok(inspect_plugin(home, name)?.map(|plugin| plugin_status(plugin, &config)))
}

pub fn is_plugin_enabled(home: &MaturanaHome, name: &str) -> anyhow::Result<bool> {
    Ok(inspect_plugin_status(home, name)?
        .map(|status| status.enabled)
        .unwrap_or(false))
}

pub fn is_feature_enabled(
    home: &MaturanaHome,
    plugin_name: &str,
    feature_name: &str,
) -> anyhow::Result<bool> {
    Ok(inspect_plugin_status(home, plugin_name)?
        .and_then(|status| {
            status
                .features
                .into_iter()
                .find(|feature| feature.name == feature_name)
                .map(|feature| feature.enabled)
        })
        .unwrap_or(false))
}

pub fn enabled_plugin_skills(home: &MaturanaHome) -> anyhow::Result<Vec<PluginSkillAsset>> {
    Ok(enabled_plugin_assets(home)?
        .into_iter()
        .filter(|asset| asset.kind == "skill")
        .map(|asset| PluginSkillAsset {
            plugin: asset.plugin,
            feature: asset.feature,
            name: asset.name,
            path: asset.path,
            description: asset.description,
        })
        .collect())
}

pub fn enabled_plugin_assets(home: &MaturanaHome) -> anyhow::Result<Vec<PluginActiveAsset>> {
    let mut out = Vec::new();
    for status in list_plugin_statuses(home)? {
        if !status.enabled {
            continue;
        }
        if !status.validation.valid {
            anyhow::bail!(
                "enabled plugin {} is invalid: {}",
                status.manifest.name,
                status.validation.errors.join("; ")
            );
        }

        for skill in &status.manifest.skills {
            if asset_feature_enabled(&status, skill.feature.as_deref()) {
                out.push(PluginActiveAsset {
                    plugin: status.manifest.name.clone(),
                    feature: skill.feature.clone(),
                    kind: "skill".to_string(),
                    name: skill.name.clone(),
                    path: status.root.join(&skill.path),
                    description: skill.description.clone(),
                });
            }
        }
        for tool in &status.manifest.tools {
            if asset_feature_enabled(&status, tool.feature.as_deref()) {
                out.push(PluginActiveAsset {
                    plugin: status.manifest.name.clone(),
                    feature: tool.feature.clone(),
                    kind: "tool".to_string(),
                    name: tool.name.clone(),
                    path: status.root.join(&tool.path),
                    description: tool.description.clone(),
                });
            }
        }
        for command in &status.manifest.commands {
            if asset_feature_enabled(&status, command.feature.as_deref()) {
                out.push(PluginActiveAsset {
                    plugin: status.manifest.name.clone(),
                    feature: command.feature.clone(),
                    kind: "command".to_string(),
                    name: command.name.clone(),
                    path: status.root.join(&command.entrypoint),
                    description: Some(command.description.clone()),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        a.plugin
            .cmp(&b.plugin)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

pub fn enabled_plugin_permissions(
    home: &MaturanaHome,
) -> anyhow::Result<Vec<PluginPermissionGrant>> {
    let mut out = Vec::new();
    for status in list_plugin_statuses(home)? {
        if !status.enabled {
            continue;
        }
        if !status.validation.valid {
            anyhow::bail!(
                "enabled plugin {} is invalid: {}",
                status.manifest.name,
                status.validation.errors.join("; ")
            );
        }
        if status.effective_permissions.filesystem.is_empty()
            && status.effective_permissions.egress.is_empty()
            && status.effective_permissions.secrets.is_empty()
        {
            continue;
        }
        out.push(PluginPermissionGrant {
            plugin: status.manifest.name,
            filesystem: status.effective_permissions.filesystem,
            egress: status.effective_permissions.egress,
            secrets: status.effective_permissions.secrets,
        });
    }
    out.sort_by(|a, b| a.plugin.cmp(&b.plugin));
    Ok(out)
}

pub fn builtin_command_feature(
    home: &MaturanaHome,
    command_name: &str,
) -> anyhow::Result<Option<String>> {
    let Some(status) = inspect_plugin_status(home, BUILTINS_PLUGIN)? else {
        return Ok(None);
    };
    if !status.validation.valid {
        anyhow::bail!(
            "built-in plugin catalog is invalid: {}",
            status.validation.errors.join("; ")
        );
    }
    Ok(status
        .manifest
        .commands
        .iter()
        .find(|command| command.name == command_name)
        .and_then(|command| command.feature.clone()))
}

pub fn builtin_command_enabled(home: &MaturanaHome, command_name: &str) -> anyhow::Result<bool> {
    if command_name == CORE_PLUGIN_COMMAND {
        return Ok(true);
    }
    let Some(status) = inspect_plugin_status(home, BUILTINS_PLUGIN)? else {
        return Ok(true);
    };
    if !status.validation.valid {
        anyhow::bail!(
            "built-in plugin catalog is invalid: {}",
            status.validation.errors.join("; ")
        );
    }
    let Some(command) = status
        .manifest
        .commands
        .iter()
        .find(|command| command.name == command_name)
    else {
        return Ok(true);
    };
    Ok(asset_feature_enabled(&status, command.feature.as_deref()))
}

pub fn ensure_builtin_command_enabled(
    home: &MaturanaHome,
    command_name: &str,
) -> anyhow::Result<()> {
    if builtin_command_enabled(home, command_name)? {
        return Ok(());
    }
    let feature = builtin_command_feature(home, command_name)?;
    match feature {
        Some(feature) => anyhow::bail!(
            "built-in command '{command_name}' is disabled by plugin feature {BUILTINS_PLUGIN}/{feature}; re-enable it with `maturana plugin enable {BUILTINS_PLUGIN} --feature {feature}`"
        ),
        None => anyhow::bail!(
            "built-in command '{command_name}' is disabled by plugin {BUILTINS_PLUGIN}; re-enable it with `maturana plugin enable {BUILTINS_PLUGIN}`"
        ),
    }
}

pub fn set_plugin_enabled(
    home: &MaturanaHome,
    name: &str,
    enabled: bool,
) -> anyhow::Result<PluginStatus> {
    let plugin =
        inspect_plugin(home, name)?.ok_or_else(|| anyhow::anyhow!("plugin not found: {name}"))?;
    if enabled && !plugin.validation.valid {
        anyhow::bail!("cannot enable invalid plugin: {name}");
    }
    let mut config = load_plugin_config(home)?;
    config.plugins.entry(name.to_string()).or_default().enabled = Some(enabled);
    save_plugin_config(home, &config)?;
    Ok(plugin_status(plugin, &config))
}

pub fn set_plugin_feature_enabled(
    home: &MaturanaHome,
    plugin_name: &str,
    feature_name: &str,
    enabled: bool,
) -> anyhow::Result<PluginStatus> {
    let plugin = inspect_plugin(home, plugin_name)?
        .ok_or_else(|| anyhow::anyhow!("plugin not found: {plugin_name}"))?;
    if enabled && !plugin.validation.valid {
        anyhow::bail!("cannot enable feature from invalid plugin: {plugin_name}");
    }
    ensure_feature_exists(&plugin.manifest.features, feature_name, plugin_name)?;
    let mut config = load_plugin_config(home)?;
    let entry = config.plugins.entry(plugin_name.to_string()).or_default();
    if enabled {
        entry.enabled = Some(true);
    }
    entry.features.insert(feature_name.to_string(), enabled);
    save_plugin_config(home, &config)?;
    Ok(plugin_status(plugin, &config))
}

pub fn validate_plugin_path(path: &Path) -> anyhow::Result<PluginValidationReport> {
    let source = plugin_source(path)?;
    Ok(source.validation)
}

pub fn install_plugin(
    home: &MaturanaHome,
    source_path: &Path,
    options: PluginInstallOptions,
) -> anyhow::Result<PluginInstallResult> {
    let source = plugin_source(source_path)?;
    if !source.validation.valid {
        anyhow::bail!("plugin is invalid: {}", source.validation.errors.join("; "));
    }

    let destination = home.root().join("plugins").join(&source.manifest.name);
    ensure_no_external_name_collision(home, &source.manifest.name, &destination)?;

    let source_canonical = fs::canonicalize(&source.root)?;
    let destination_canonical = fs::canonicalize(&destination).ok();
    let already_in_place = destination_canonical
        .as_ref()
        .is_some_and(|canonical| canonical == &source_canonical);
    let replaced = destination.exists() && !already_in_place;

    if destination.exists() && !already_in_place && !options.force {
        anyhow::bail!(
            "plugin already installed at {}; pass --force to replace it",
            destination.display()
        );
    }

    if !already_in_place {
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("plugin destination has no parent"))?;
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(
            ".install-{}-{}",
            source.manifest.name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        copy_plugin_dir(&source.root, &temp)?;
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(&temp, &destination)?;
    }

    let mut config = load_plugin_config(home)?;
    if options.enable {
        config
            .plugins
            .entry(source.manifest.name.clone())
            .or_default()
            .enabled = Some(true);
        save_plugin_config(home, &config)?;
    }

    let manifest_path = destination.join(
        source
            .manifest_path
            .strip_prefix(&source.root)
            .unwrap_or_else(|_| Path::new(maturana_plugin::MATURANA_PLUGIN_TOML)),
    );
    let status = plugin_status(
        DiscoveredPlugin {
            manifest: source.manifest,
            root: destination.clone(),
            manifest_path,
            scope: "home".to_string(),
            validation: source.validation,
        },
        &config,
    );

    Ok(PluginInstallResult {
        source: source.root,
        destination,
        replaced,
        status,
    })
}

pub fn plugin_roots_json(home: &MaturanaHome) -> serde_json::Value {
    serde_json::to_value(default_plugin_roots(home)).unwrap_or_else(|_| serde_json::json!([]))
}

fn plugin_status(plugin: DiscoveredPlugin, config: &PluginConfig) -> PluginStatus {
    let entry = config.plugins.get(&plugin.manifest.name);
    let enabled_feature_override = entry
        .map(|entry| entry.features.values().any(|enabled| *enabled))
        .unwrap_or(false);
    let default_enabled = plugin
        .manifest
        .features
        .iter()
        .any(|feature| feature.default_enabled);
    let enabled = entry
        .and_then(|entry| entry.enabled)
        .unwrap_or(default_enabled || enabled_feature_override);
    let features = plugin
        .manifest
        .features
        .iter()
        .map(|feature| {
            let feature_enabled = enabled
                && entry
                    .and_then(|entry| entry.features.get(&feature.name))
                    .copied()
                    .unwrap_or(feature.default_enabled);
            PluginFeatureStatus {
                name: feature.name.clone(),
                kind: feature.kind.clone(),
                description: feature.description.clone(),
                default_enabled: feature.default_enabled,
                enabled: feature_enabled,
            }
        })
        .collect();
    let effective_permissions = if enabled && plugin.validation.valid {
        plugin.manifest.permissions.clone()
    } else {
        maturana_plugin::PluginPermissions::default()
    };
    PluginStatus {
        manifest: plugin.manifest,
        root: plugin.root,
        manifest_path: plugin.manifest_path,
        scope: plugin.scope,
        validation: plugin.validation,
        enabled,
        features,
        effective_permissions,
    }
}

fn ensure_feature_exists(
    features: &[PluginFeature],
    feature_name: &str,
    plugin_name: &str,
) -> anyhow::Result<()> {
    if features.iter().any(|feature| feature.name == feature_name) {
        return Ok(());
    }
    anyhow::bail!("plugin {plugin_name} has no feature named {feature_name}");
}

fn asset_feature_enabled(status: &PluginStatus, feature: Option<&str>) -> bool {
    let Some(feature) = feature else {
        return true;
    };
    status
        .features
        .iter()
        .find(|candidate| candidate.name == feature)
        .map(|feature| feature.enabled)
        .unwrap_or(false)
}

struct PluginSource {
    manifest: maturana_plugin::PluginManifest,
    root: PathBuf,
    manifest_path: PathBuf,
    validation: PluginValidationReport,
}

fn plugin_source(path: &Path) -> anyhow::Result<PluginSource> {
    let manifest_path =
        maturana_plugin::manifest_path_for(path).unwrap_or_else(|| path.to_path_buf());
    if !manifest_path.is_file() {
        anyhow::bail!("plugin manifest not found at {}", path.display());
    }
    let root = plugin_root_for(path, &manifest_path)?;
    let manifest = load_manifest(&manifest_path)?;
    let validation = validate_manifest(&manifest, &root);
    Ok(PluginSource {
        manifest,
        root,
        manifest_path,
        validation,
    })
}

fn plugin_root_for(input_path: &Path, manifest_path: &Path) -> anyhow::Result<PathBuf> {
    if input_path.is_dir() {
        return Ok(input_path.to_path_buf());
    }
    let parent = manifest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("plugin manifest has no parent"))?;
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".maturana-plugin")
    {
        return Ok(parent
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.to_path_buf()));
    }
    Ok(parent.to_path_buf())
}

fn ensure_no_external_name_collision(
    home: &MaturanaHome,
    name: &str,
    destination: &Path,
) -> anyhow::Result<()> {
    let destination_canonical = fs::canonicalize(destination).ok();
    for plugin in list_plugins(home)? {
        if plugin.manifest.name != name {
            continue;
        }
        let plugin_canonical = fs::canonicalize(&plugin.root).ok();
        if plugin_canonical.is_some() && plugin_canonical == destination_canonical {
            continue;
        }
        if plugin.scope != "home" {
            anyhow::bail!(
                "plugin name '{name}' already exists in {} scope at {}",
                plugin.scope,
                plugin.root.display()
            );
        }
    }
    Ok(())
}

fn copy_plugin_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("plugin install refuses symlinked paths: {}", src.display());
    }
    if !metadata.is_dir() {
        anyhow::bail!("plugin source must be a directory: {}", src.display());
    }
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "plugin install refuses symlinked entries: {}",
                source_path.display()
            );
        }
        if metadata.is_dir() {
            copy_plugin_dir(&source_path, &target_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &target_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_roots_are_workspace_then_home() {
        let home = MaturanaHome::new("/tmp/maturana-test/.maturana");
        let roots = default_plugin_roots(&home);
        assert_eq!(roots[0].scope, "workspace");
        assert!(roots[0].path.ends_with("plugins"));
        assert_eq!(roots[1].scope, "home");
        assert!(roots[1].path.ends_with(".maturana/plugins"));
    }

    #[test]
    fn plugin_enablement_overrides_manifest_defaults() {
        let root =
            std::env::temp_dir().join(format!("maturana-ops-plugin-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let plugin = root.join("plugins/demo");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin for enablement tests"

[[features]]
name = "alpha"
kind = "skill"
description = "Alpha feature"
default_enabled = false

[[features]]
name = "beta"
kind = "skill"
description = "Beta feature"
default_enabled = true
"#,
        )
        .unwrap();

        let status = inspect_plugin_status(&home, "demo").unwrap().unwrap();
        assert!(status.enabled);
        assert!(
            !status
                .features
                .iter()
                .find(|f| f.name == "alpha")
                .unwrap()
                .enabled
        );
        assert!(
            status
                .features
                .iter()
                .find(|f| f.name == "beta")
                .unwrap()
                .enabled
        );

        let status = set_plugin_feature_enabled(&home, "demo", "alpha", true).unwrap();
        assert!(status.enabled);
        assert!(
            status
                .features
                .iter()
                .find(|f| f.name == "alpha")
                .unwrap()
                .enabled
        );
        assert!(is_plugin_enabled(&home, "demo").unwrap());
        assert!(is_feature_enabled(&home, "demo", "alpha").unwrap());

        let status = set_plugin_enabled(&home, "demo", false).unwrap();
        assert!(!status.enabled);
        assert!(!status.features.iter().any(|feature| feature.enabled));
        assert!(!is_plugin_enabled(&home, "demo").unwrap());
        assert!(!is_feature_enabled(&home, "demo", "alpha").unwrap());
        assert!(plugin_config_path(&home).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn enabled_plugin_permissions_report_effective_grants() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-plugin-permissions-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let plugin = root.join("plugins/demo");
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin with permissions"

[[features]]
name = "demo-feature"
kind = "skill"
description = "Demo feature"
default_enabled = true

[permissions]
filesystem = ["/workspace/project"]
egress = ["api.example.com"]
secrets = ["demo/api-key"]
"#,
        )
        .unwrap();

        let status = inspect_plugin_status(&home, "demo").unwrap().unwrap();
        assert_eq!(
            status.effective_permissions.egress,
            vec!["api.example.com".to_string()]
        );
        let grants = enabled_plugin_permissions(&home).unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].plugin, "demo");
        assert_eq!(grants[0].secrets, vec!["demo/api-key".to_string()]);

        let status = set_plugin_enabled(&home, "demo", false).unwrap();
        assert!(status.effective_permissions.egress.is_empty());
        assert!(enabled_plugin_permissions(&home).unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn enabled_plugin_skills_resolve_inside_enabled_valid_plugins() {
        let root =
            std::env::temp_dir().join(format!("maturana-ops-plugin-skills-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let plugin = root.join("plugins/demo");
        fs::create_dir_all(plugin.join("skills/demo")).unwrap();
        fs::write(plugin.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        fs::write(
            plugin.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin with a skill"

[[features]]
name = "demo-skill"
kind = "skill"
description = "Demo skill feature"
default_enabled = true

	[[skills]]
	name = "demo"
	path = "skills/demo/SKILL.md"
	description = "Demo plugin skill"
	feature = "demo-skill"
	"#,
        )
        .unwrap();

        let skills = enabled_plugin_skills(&home).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].plugin, "demo");
        assert_eq!(skills[0].feature.as_deref(), Some("demo-skill"));
        assert_eq!(skills[0].name, "demo");
        assert!(skills[0].path.ends_with("skills/demo/SKILL.md"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn enabled_plugin_assets_follow_feature_enablement() {
        let root =
            std::env::temp_dir().join(format!("maturana-ops-plugin-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let plugin = root.join("plugins/demo");
        fs::create_dir_all(plugin.join("skills/on")).unwrap();
        fs::create_dir_all(plugin.join("skills/off")).unwrap();
        fs::create_dir_all(plugin.join("tools/on")).unwrap();
        fs::create_dir_all(plugin.join("commands")).unwrap();
        fs::write(plugin.join("skills/on/SKILL.md"), "# on\n").unwrap();
        fs::write(plugin.join("skills/off/SKILL.md"), "# off\n").unwrap();
        fs::write(plugin.join("tools/on/tool.txt"), "tool\n").unwrap();
        fs::write(plugin.join("commands/on.toml"), "name = \"command-on\"\n").unwrap();
        fs::write(
            plugin.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin with feature-gated assets"

[[features]]
name = "enabled-feature"
kind = "skill"
description = "Enabled feature"
default_enabled = true

[[features]]
name = "disabled-feature"
kind = "skill"
description = "Disabled feature"
default_enabled = false

[[skills]]
name = "on"
path = "skills/on/SKILL.md"
feature = "enabled-feature"

[[skills]]
name = "off"
path = "skills/off/SKILL.md"
feature = "disabled-feature"

[[tools]]
name = "tool-on"
path = "tools/on"
feature = "enabled-feature"

[[commands]]
name = "command-on"
description = "Command declaration only"
entrypoint = "commands/on.toml"
feature = "enabled-feature"
"#,
        )
        .unwrap();

        let assets = enabled_plugin_assets(&home).unwrap();
        let names: Vec<_> = assets.iter().map(|asset| asset.name.as_str()).collect();
        assert_eq!(names, vec!["command-on", "on", "tool-on"]);
        assert!(!names.contains(&"off"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builtins_catalog_exposes_default_command_assets() {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let home = MaturanaHome::new(repo_root.join(".maturana-test-builtins"));
        let status = inspect_plugin_status(&home, "maturana-builtins")
            .unwrap()
            .expect("builtins plugin discovered");
        assert!(status.validation.valid);
        assert!(status.enabled);
        assert!(status
            .features
            .iter()
            .any(|feature| feature.name == "web-cockpit" && !feature.enabled));

        let assets = enabled_plugin_assets(&home).unwrap();
        let commands = assets
            .iter()
            .filter(|asset| asset.plugin == "maturana-builtins" && asset.kind == "command")
            .map(|asset| asset.name.as_str())
            .collect::<Vec<_>>();
        assert!(commands.contains(&"agent"));
        assert!(commands.contains(&"plugin"));
        assert!(commands.contains(&"tool"));
        assert!(
            !commands.contains(&"web"),
            "web command is gated by the disabled web-cockpit feature"
        );
    }

    #[test]
    fn builtins_command_gate_follows_feature_enablement() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-plugin-builtins-gate-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let plugin = root.join("plugins/maturana-builtins");
        fs::create_dir_all(plugin.join("commands")).unwrap();
        fs::write(
            plugin.join("commands/channels.toml"),
            "name = \"channels\"\n",
        )
        .unwrap();
        fs::write(plugin.join("commands/dev.toml"), "name = \"dev\"\n").unwrap();
        fs::write(
            plugin.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "maturana-builtins"
version = "0.1.0"
description = "Built-in catalog for gate tests"

[[features]]
name = "channels"
kind = "channel"
description = "Channel feature"
default_enabled = true

[[features]]
name = "developer-tools"
kind = "host-op"
description = "Developer feature"
default_enabled = false

[[commands]]
name = "channel"
description = "Channel command"
entrypoint = "commands/channels.toml"
feature = "channels"

[[commands]]
name = "plugin"
description = "Plugin command"
entrypoint = "commands/dev.toml"
feature = "developer-tools"
"#,
        )
        .unwrap();

        assert!(builtin_command_enabled(&home, "channel").unwrap());
        assert!(builtin_command_enabled(&home, "missing").unwrap());
        assert!(
            builtin_command_enabled(&home, "plugin").unwrap(),
            "plugin management remains core so disabled features can be re-enabled"
        );

        set_plugin_feature_enabled(&home, BUILTINS_PLUGIN, "channels", false).unwrap();
        assert!(!builtin_command_enabled(&home, "channel").unwrap());
        let error = ensure_builtin_command_enabled(&home, "channel")
            .unwrap_err()
            .to_string();
        assert!(error.contains("maturana-builtins/channels"));

        set_plugin_feature_enabled(&home, BUILTINS_PLUGIN, "channels", true).unwrap();
        assert!(ensure_builtin_command_enabled(&home, "channel").is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_plugin_copies_to_home_and_can_enable() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-plugin-install-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let source = root.join("source/demo");
        fs::create_dir_all(source.join("skills/demo")).unwrap();
        fs::write(source.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        fs::write(
            source.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin to install"

[[features]]
name = "demo-skill"
kind = "skill"
description = "Demo skill"
default_enabled = false

[[skills]]
name = "demo"
path = "skills/demo/SKILL.md"
feature = "demo-skill"
"#,
        )
        .unwrap();

        let result = install_plugin(
            &home,
            &source,
            PluginInstallOptions {
                enable: true,
                force: false,
            },
        )
        .unwrap();

        assert!(result.destination.ends_with(".maturana/plugins/demo"));
        assert!(result.destination.join("skills/demo/SKILL.md").exists());
        assert!(result.status.enabled);
        assert!(plugin_config_path(&home).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_plugin_rejects_invalid_manifest() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-plugin-install-invalid-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));
        let source = root.join("source/bad");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join(maturana_plugin::MATURANA_PLUGIN_TOML),
            r#"
name = "bad"
version = "0.1.0"
description = "Bad plugin"

[[skills]]
name = "bad"
path = "/etc/passwd"
"#,
        )
        .unwrap();

        let error = install_plugin(
            &home,
            &source,
            PluginInstallOptions {
                enable: false,
                force: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("plugin is invalid"));
        assert!(!home.root().join("plugins/bad").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validate_plugin_path_uses_directory_root_for_nested_manifest() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-plugin-nested-manifest-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let plugin = root.join("plugin");
        fs::create_dir_all(plugin.join(".maturana-plugin")).unwrap();
        fs::create_dir_all(plugin.join("skills/demo")).unwrap();
        fs::write(plugin.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        fs::write(
            plugin.join(".maturana-plugin/plugin.toml"),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin with nested manifest"

[[skills]]
name = "demo"
path = "skills/demo/SKILL.md"
"#,
        )
        .unwrap();

        let report = validate_plugin_path(&plugin).unwrap();
        assert!(report.valid, "{:?}", report.errors);
        let _ = fs::remove_dir_all(root);
    }
}
