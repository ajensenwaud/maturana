use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

pub const MATURANA_PLUGIN_TOML: &str = "MATURANA_PLUGIN.toml";
pub const PLUGIN_TOML: &str = "plugin.toml";
pub const PLUGIN_JSON: &str = "plugin.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub features: Vec<PluginFeature>,
    #[serde(default)]
    pub skills: Vec<PluginAsset>,
    #[serde(default)]
    pub tools: Vec<PluginAsset>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub permissions: PluginPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginFeature {
    pub name: String,
    /// Open string by design: first-party and third-party plugins can introduce
    /// feature families without requiring a core enum bump.
    pub kind: String,
    pub description: String,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub default_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginAsset {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Optional feature gate. When set, the asset is active only when that
    /// manifest feature is enabled.
    #[serde(default)]
    pub feature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    /// Command entrypoint relative to the plugin root. Host execution policy is
    /// owned by Maturana ops; the manifest only declares what exists.
    pub entrypoint: String,
    /// Optional feature gate. When set, the command is active only when that
    /// manifest feature is enabled.
    #[serde(default)]
    pub feature: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginPermissions {
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRoot {
    pub scope: String,
    pub path: PathBuf,
}

impl PluginRoot {
    pub fn new(scope: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            scope: scope.into(),
            path: path.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredPlugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub scope: String,
    pub validation: PluginValidationReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn discover_plugins(roots: &[PluginRoot]) -> anyhow::Result<Vec<DiscoveredPlugin>> {
    let mut plugins = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        for plugin_dir in plugin_dirs(&root.path)? {
            let Some(manifest_path) = manifest_path_for(&plugin_dir) else {
                continue;
            };
            let manifest = load_manifest(&manifest_path)?;
            let validation = validate_manifest(&manifest, &plugin_dir);
            let key = format!("{}:{}", root.scope, manifest.name);
            if !seen.insert(key) {
                continue;
            }
            plugins.push(DiscoveredPlugin {
                manifest,
                root: plugin_dir,
                manifest_path,
                scope: root.scope.clone(),
                validation,
            });
        }
    }
    plugins.sort_by(|a, b| {
        a.manifest
            .name
            .cmp(&b.manifest.name)
            .then_with(|| a.scope.cmp(&b.scope))
    });
    Ok(plugins)
}

pub fn load_manifest(path: &Path) -> anyhow::Result<PluginManifest> {
    let raw = fs::read_to_string(path)?;
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("json") => Ok(serde_json::from_str(&raw)?),
        _ => Ok(toml::from_str(&raw)?),
    }
}

pub fn validate_manifest(manifest: &PluginManifest, plugin_root: &Path) -> PluginValidationReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    validate_id("name", &manifest.name, &mut errors);
    if manifest.version.trim().is_empty() {
        errors.push("version must not be empty".to_string());
    }
    if manifest.description.trim().len() < 12 {
        warnings.push("description is short; describe the plugin boundary".to_string());
    }

    let mut feature_names = HashSet::new();
    for feature in &manifest.features {
        validate_id("features.name", &feature.name, &mut errors);
        validate_id("features.kind", &feature.kind, &mut errors);
        if !feature_names.insert(feature.name.clone()) {
            errors.push(format!("duplicate feature '{}'", feature.name));
        }
        if feature.description.trim().is_empty() {
            warnings.push(format!("feature '{}' has no description", feature.name));
        }
        if let Some(entrypoint) = &feature.entrypoint {
            validate_relative_path("features.entrypoint", entrypoint, plugin_root, &mut errors);
        }
    }

    validate_assets(
        "skills",
        &manifest.skills,
        plugin_root,
        &feature_names,
        &mut errors,
    );
    validate_assets(
        "tools",
        &manifest.tools,
        plugin_root,
        &feature_names,
        &mut errors,
    );
    let tool_paths = manifest
        .tools
        .iter()
        .map(|tool| tool.path.as_str())
        .collect::<Vec<_>>();

    let mut command_names = HashSet::new();
    for command in &manifest.commands {
        validate_id("commands.name", &command.name, &mut errors);
        if !command_names.insert(command.name.clone()) {
            errors.push(format!("duplicate command '{}'", command.name));
        }
        if command.description.trim().is_empty() {
            warnings.push(format!("command '{}' has no description", command.name));
        }
        validate_command_entrypoint(
            "commands.entrypoint",
            &command.entrypoint,
            plugin_root,
            &tool_paths,
            &mut errors,
        );
        validate_feature_ref(
            "commands.feature",
            &command.name,
            command.feature.as_deref(),
            &feature_names,
            &mut errors,
        );
    }

    for path in &manifest.permissions.filesystem {
        if path.trim().is_empty() {
            errors.push("permissions.filesystem entries must not be empty".to_string());
        }
    }
    for host in &manifest.permissions.egress {
        if host.trim().is_empty() || host.contains('/') {
            errors.push(format!("invalid egress host permission '{host}'"));
        }
    }
    for secret in &manifest.permissions.secrets {
        if secret.trim().is_empty() || secret.contains("..") {
            errors.push(format!("invalid secret permission '{secret}'"));
        }
    }

    PluginValidationReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

pub fn manifest_path_for(plugin_dir: &Path) -> Option<PathBuf> {
    manifest_candidates(plugin_dir)
        .into_iter()
        .find(|path| path.exists())
}

pub fn manifest_candidates(plugin_dir: &Path) -> Vec<PathBuf> {
    vec![
        plugin_dir.join(MATURANA_PLUGIN_TOML),
        plugin_dir.join(PLUGIN_TOML),
        plugin_dir.join(".maturana-plugin").join(PLUGIN_TOML),
        plugin_dir.join(PLUGIN_JSON),
        plugin_dir.join(".maturana-plugin").join(PLUGIN_JSON),
    ]
}

fn plugin_dirs(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if manifest_path_for(root).is_some() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn validate_assets(
    label: &str,
    assets: &[PluginAsset],
    plugin_root: &Path,
    feature_names: &HashSet<String>,
    errors: &mut Vec<String>,
) {
    let mut names = HashSet::new();
    for asset in assets {
        validate_id(&format!("{label}.name"), &asset.name, errors);
        if !names.insert(asset.name.clone()) {
            errors.push(format!("duplicate {label} asset '{}'", asset.name));
        }
        validate_relative_path(&format!("{label}.path"), &asset.path, plugin_root, errors);
        validate_feature_ref(
            &format!("{label}.feature"),
            &asset.name,
            asset.feature.as_deref(),
            feature_names,
            errors,
        );
    }
}

fn validate_feature_ref(
    field: &str,
    asset_name: &str,
    feature: Option<&str>,
    feature_names: &HashSet<String>,
    errors: &mut Vec<String>,
) {
    let Some(feature) = feature else {
        return;
    };
    validate_id(field, feature, errors);
    if !feature_names.contains(feature) {
        errors.push(format!(
            "{field} for '{asset_name}' references unknown feature '{feature}'"
        ));
    }
}

fn validate_id(field: &str, value: &str, errors: &mut Vec<String>) {
    let ok = !value.trim().is_empty()
        && value.len() <= 128
        && !value.contains("..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if !ok {
        errors.push(format!(
            "{field} must be a safe id (letters, digits, -, _, .; no traversal)"
        ));
    }
}

fn validate_relative_path(field: &str, value: &str, plugin_root: &Path, errors: &mut Vec<String>) {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        errors.push(format!("{field} must be a relative path inside the plugin"));
        return;
    }
    let joined = plugin_root.join(path);
    if !joined.exists() {
        errors.push(format!("{field} does not exist: {}", joined.display()));
        return;
    }
    if let (Ok(root), Ok(joined)) = (fs::canonicalize(plugin_root), fs::canonicalize(&joined)) {
        if !joined.starts_with(root) {
            errors.push(format!("{field} must resolve inside the plugin root"));
        }
    }
}

fn validate_command_entrypoint(
    field: &str,
    value: &str,
    plugin_root: &Path,
    tool_paths: &[&str],
    errors: &mut Vec<String>,
) {
    validate_relative_path(field, value, plugin_root, errors);
    let path = Path::new(value);
    let under_commands = path.starts_with("commands");
    let under_declared_tool = tool_paths
        .iter()
        .any(|tool_path| path.starts_with(Path::new(tool_path)));
    if !under_commands && !under_declared_tool {
        errors.push(format!(
            "{field} must point under commands/ or inside a declared tool path"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maturana-plugin-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_plugin_toml_under_root_children() {
        let root = tmp_dir("discover");
        let plugin = root.join("demo");
        fs::create_dir_all(plugin.join("skills/demo")).unwrap();
        fs::write(plugin.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        fs::write(
            plugin.join(PLUGIN_TOML),
            r#"
name = "demo"
version = "0.1.0"
description = "Demo plugin for testing"

[[features]]
name = "demo-skill"
kind = "skill"
description = "Provides a test skill"
entrypoint = "skills/demo/SKILL.md"

[[skills]]
name = "demo"
path = "skills/demo/SKILL.md"
"#,
        )
        .unwrap();

        let plugins = discover_plugins(&[PluginRoot::new("workspace", &root)]).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.name, "demo");
        assert!(plugins[0].validation.valid);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validation_rejects_absolute_asset_paths() {
        let root = tmp_dir("validate");
        let manifest = PluginManifest {
            name: "bad".to_string(),
            version: "0.1.0".to_string(),
            description: "Bad plugin for testing".to_string(),
            author: None,
            homepage: None,
            features: Vec::new(),
            skills: vec![PluginAsset {
                name: "bad".to_string(),
                path: "/etc/passwd".to_string(),
                description: None,
                feature: None,
            }],
            tools: Vec::new(),
            commands: Vec::new(),
            permissions: PluginPermissions::default(),
        };
        let report = validate_manifest(&manifest, &root);
        assert!(!report.valid);
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("relative path")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validation_rejects_unknown_asset_feature_refs() {
        let root = tmp_dir("feature-ref");
        fs::create_dir_all(root.join("skills/demo")).unwrap();
        fs::write(root.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        let manifest = PluginManifest {
            name: "bad-feature".to_string(),
            version: "0.1.0".to_string(),
            description: "Bad plugin with an unknown feature reference".to_string(),
            author: None,
            homepage: None,
            features: vec![PluginFeature {
                name: "known".to_string(),
                kind: "skill".to_string(),
                description: "Known feature".to_string(),
                entrypoint: None,
                default_enabled: true,
            }],
            skills: vec![PluginAsset {
                name: "demo".to_string(),
                path: "skills/demo/SKILL.md".to_string(),
                description: None,
                feature: Some("missing".to_string()),
            }],
            tools: Vec::new(),
            commands: vec![PluginCommand {
                name: "demo-command".to_string(),
                description: "Demo command".to_string(),
                entrypoint: "skills/demo/SKILL.md".to_string(),
                feature: Some("missing".to_string()),
            }],
            permissions: PluginPermissions::default(),
        };
        let report = validate_manifest(&manifest, &root);
        assert!(!report.valid);
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("unknown feature 'missing'")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validation_restricts_command_entrypoints() {
        let root = tmp_dir("command-entrypoint");
        fs::create_dir_all(root.join("skills/demo")).unwrap();
        fs::create_dir_all(root.join("commands")).unwrap();
        fs::create_dir_all(root.join("tools/demo")).unwrap();
        fs::write(root.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
        fs::write(root.join("commands/demo.toml"), "name = \"demo\"\n").unwrap();
        fs::write(root.join("tools/demo/run"), "demo\n").unwrap();

        let manifest = PluginManifest {
            name: "commands".to_string(),
            version: "0.1.0".to_string(),
            description: "Command plugin for validation".to_string(),
            author: None,
            homepage: None,
            features: Vec::new(),
            skills: vec![PluginAsset {
                name: "demo".to_string(),
                path: "skills/demo/SKILL.md".to_string(),
                description: None,
                feature: None,
            }],
            tools: vec![PluginAsset {
                name: "demo-tool".to_string(),
                path: "tools/demo".to_string(),
                description: None,
                feature: None,
            }],
            commands: vec![
                PluginCommand {
                    name: "metadata-command".to_string(),
                    description: "Command metadata".to_string(),
                    entrypoint: "commands/demo.toml".to_string(),
                    feature: None,
                },
                PluginCommand {
                    name: "tool-command".to_string(),
                    description: "Tool-backed command".to_string(),
                    entrypoint: "tools/demo/run".to_string(),
                    feature: None,
                },
            ],
            permissions: PluginPermissions::default(),
        };
        assert!(validate_manifest(&manifest, &root).valid);

        let mut bad = manifest;
        bad.commands.push(PluginCommand {
            name: "skill-command".to_string(),
            description: "Invalid skill-backed command".to_string(),
            entrypoint: "skills/demo/SKILL.md".to_string(),
            feature: None,
        });
        let report = validate_manifest(&bad, &root);
        assert!(!report.valid);
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("commands/ or inside a declared tool path")));
        let _ = fs::remove_dir_all(root);
    }
}
