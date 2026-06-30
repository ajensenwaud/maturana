use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::state::MaturanaHome;
use maturana_ops::plugins::PluginSkillAsset;

#[derive(Debug, Args)]
pub struct SkillCommand {
    #[command(subcommand)]
    pub command: SkillSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillSubcommand {
    Validate {
        #[arg(default_value = "skills")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Install Maturana's skills as native Codex skills (discovered via the
    /// `/skills` menu, `$name` mention, or implicitly). Writes
    /// `<dest>/<name>/SKILL.md` with the required frontmatter; default dest is
    /// the user-level Codex skill root `~/.agents/skills`.
    #[command(alias = "codex")]
    CodexPrompts {
        #[arg(default_value = "skills")]
        root: PathBuf,
        /// Override the Codex skills directory (default ~/.agents/skills).
        #[arg(long, alias = "prompts-dir")]
        dest: Option<PathBuf>,
    },
}

#[derive(Debug, serde::Serialize)]
struct SkillValidationReport {
    root: PathBuf,
    checked: usize,
    failures: Vec<String>,
}

pub fn handle_skill(command: SkillCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        SkillSubcommand::Validate { root, json } => {
            let report = validate_skill_pack(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.failures.is_empty() {
                println!(
                    "valid skill pack: {} skills checked under {}",
                    report.checked,
                    report.root.display()
                );
            } else {
                for failure in &report.failures {
                    eprintln!("{failure}");
                }
            }
            if report.failures.is_empty() {
                Ok(())
            } else {
                anyhow::bail!(
                    "skill validation failed: {} issue(s)",
                    report.failures.len()
                )
            }
        }
        SkillSubcommand::CodexPrompts { root, dest } => {
            let plugin_skills = maturana_ops::plugins::enabled_plugin_skills(home)?;
            let count = sync_codex_prompts(&root, dest.as_deref(), &plugin_skills)?;
            println!("installed {count} Codex skill(s); use /skills or $<name> in Codex");
            Ok(())
        }
    }
}

/// Install Maturana's skills as native **Codex skills** so Codex discovers them
/// (`/skills` menu, `$name` mention, or implicit selection). Current Codex
/// (0.117+) reads skills from `<dest>/<name>/SKILL.md` under one of its skill
/// roots (default user-level `~/.agents/skills`); the deprecated
/// `~/.codex/prompts` slash-command path no longer applies. Each emitted
/// `SKILL.md` gets the required `name`/`description` YAML frontmatter (Codex caps
/// the description in the initial list) followed by the canonical skill body.
/// Idempotent.
fn sync_codex_prompts(
    root: &Path,
    dest_dir: Option<&Path>,
    plugin_skills: &[PluginSkillAsset],
) -> anyhow::Result<usize> {
    let skills_root = absolute_or_cwd(root.to_path_buf())?;
    if !skills_root.is_dir() {
        anyhow::bail!("skills directory not found: {}", skills_root.display());
    }
    let dest = match dest_dir {
        Some(p) => p.to_path_buf(),
        None => dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?
            .join(".agents")
            .join("skills"),
    };
    fs::create_dir_all(&dest)?;

    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(&skills_root)? {
        let dir = entry?.path();
        if dir.is_dir() && dir.join("SKILL.md").exists() {
            if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();

    let mut installed = BTreeSet::new();
    for name in &names {
        let src = skills_root.join(name).join("SKILL.md");
        install_skill_file(&dest, name, &src, None)?;
        installed.insert(name.clone());
    }

    for skill in plugin_skills {
        if !installed.insert(skill.name.clone()) {
            anyhow::bail!(
                "plugin skill '{}' from plugin '{}' conflicts with an installed Codex skill",
                skill.name,
                skill.plugin
            );
        }
        install_skill_file(
            &dest,
            &skill.name,
            &skill.path,
            skill.description.as_deref(),
        )?;
    }
    Ok(names.len() + plugin_skills.len())
}

fn install_skill_file(
    dest: &Path,
    name: &str,
    src: &Path,
    description: Option<&str>,
) -> anyhow::Result<()> {
    let body =
        fs::read_to_string(src).with_context(|| format!("failed to read {}", src.display()))?;
    let contents = render_codex_skill(name, &body, description);
    let out_dir = dest.join(name);
    fs::create_dir_all(&out_dir)?;
    fs::write(out_dir.join("SKILL.md"), contents)?;
    Ok(())
}

fn render_codex_skill(name: &str, body: &str, description: Option<&str>) -> String {
    // If the canonical file already has frontmatter, copy as-is; else derive
    // a one-line description and prepend the required frontmatter.
    if body.trim_start().starts_with("---") {
        return body.to_string();
    }
    let description = description
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| derive_skill_description(body));
    // Quote both values: a derived description routinely contains a colon
    // (e.g. "when running the flywheel: capture ..."), which unquoted YAML
    // parses as a nested mapping and Codex then rejects the whole skill.
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n{body}",
        yaml_quote_scalar(name),
        yaml_quote_scalar(&description)
    )
}

/// Render a string as a YAML double-quoted scalar so values containing `:`, `#`,
/// quotes, etc. survive parsing. Only `\` and `"` need escaping inside a
/// double-quoted scalar (derived text is already newline-free).
fn yaml_quote_scalar(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// First meaningful line of a skill body as its Codex `description` (single
/// line, trimmed, capped). Prefers the "Use this skill when ..." sentence.
fn derive_skill_description(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("A Maturana skill.");
    let line = line.trim_start_matches("Use this skill ").trim();
    let one_line: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = one_line.chars().take(300).collect();
    capped.replace(['\n', '\r'], " ")
}

fn validate_skill_pack(root: &Path) -> anyhow::Result<SkillValidationReport> {
    let required_sections = [
        "## Grounding",
        "## Preflight",
        "## Decision Path",
        "## Actions",
        "## Evidence",
        "## Recovery",
        "## Boundaries",
    ];
    let mut failures = Vec::new();
    let mut checked = 0usize;

    if !root.exists() {
        anyhow::bail!("skill root does not exist: {}", root.display());
    }

    for required_skill in required_initial_skills() {
        let skill_path = root.join(required_skill).join("SKILL.md");
        if !skill_path.exists() {
            failures.push(format!(
                "missing AGENTS.md initial skill: {}",
                skill_path.display()
            ));
        }
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.exists() {
            failures.push(format!("missing {}", skill_path.display()));
            continue;
        }
        checked += 1;
        let raw = fs::read_to_string(&skill_path)
            .with_context(|| format!("failed to read {}", skill_path.display()))?;
        if !raw.trim_start().starts_with("# ") {
            failures.push(format!(
                "{} must start with a level-1 title",
                skill_path.display()
            ));
        }
        if !raw.contains("Use this skill when") {
            failures.push(format!(
                "{} must describe when to use the skill",
                skill_path.display()
            ));
        }
        if !raw.contains("Read `AGENTS.md`") {
            failures.push(format!(
                "{} grounding must read AGENTS.md",
                skill_path.display()
            ));
        }
        for section in required_sections {
            if !raw.contains(section) {
                failures.push(format!("{} missing {section}", skill_path.display()));
            }
        }
        if raw.contains("## Procedure") {
            failures.push(format!(
                "{} still uses catch-all Procedure section",
                skill_path.display()
            ));
        }
        if raw.contains("just run") || raw.contains("simply run") {
            failures.push(format!(
                "{} uses thin-wrapper language; add grounding/evidence/recovery instead",
                skill_path.display()
            ));
        }
        let evidence_bullets = section_bullet_count(&raw, "## Evidence", Some("## Recovery"));
        if evidence_bullets < 4 {
            failures.push(format!(
                "{} evidence section must list at least four concrete proof points",
                skill_path.display()
            ));
        }
        let recovery_bullets = section_bullet_count(&raw, "## Recovery", Some("## Boundaries"));
        if recovery_bullets < 4 {
            failures.push(format!(
                "{} recovery section must list at least four failure-handling paths",
                skill_path.display()
            ));
        }
        let boundary_do_nots = section_prefixed_line_count(&raw, "## Boundaries", None, "- Do not");
        if boundary_do_nots < 3 {
            failures.push(format!(
                "{} boundaries section must include at least three explicit 'Do not' limits",
                skill_path.display()
            ));
        }
    }

    Ok(SkillValidationReport {
        root: root.to_path_buf(),
        checked,
        failures,
    })
}

fn required_initial_skills() -> &'static [&'static str] {
    &[
        "maturana-agent-create",
        "maturana-agent-validate",
        "maturana-agent-launch",
        "maturana-agent-inspect",
        "maturana-agent-update",
        "maturana-skill-create",
        "maturana-tool-create",
        "maturana-skill-deploy",
        "maturana-security-review",
        "maturana-snapshot",
    ]
}

fn section_text<'a>(raw: &'a str, start: &str, end: Option<&str>) -> &'a str {
    let Some((_, after_start)) = raw.split_once(start) else {
        return "";
    };
    if let Some(end) = end {
        after_start
            .split_once(end)
            .map(|(section, _)| section)
            .unwrap_or(after_start)
    } else {
        after_start
    }
}

fn section_bullet_count(raw: &str, start: &str, end: Option<&str>) -> usize {
    section_prefixed_line_count(raw, start, end, "- ")
}

fn section_prefixed_line_count(raw: &str, start: &str, end: Option<&str>, prefix: &str) -> usize {
    section_text(raw, start, end)
        .lines()
        .filter(|line| line.trim_start().starts_with(prefix))
        .count()
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn skill_pack_uses_workflow_shape() {
        let skills_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("skills");
        let report = validate_skill_pack(&skills_dir).unwrap();

        assert!(
            report.failures.is_empty(),
            "skill workflow shape failures:\n{}",
            report.failures.join("\n")
        );
    }

    #[test]
    fn skill_validator_enforces_agents_initial_skill_contract() {
        let required = required_initial_skills();
        assert!(required.contains(&"maturana-agent-create"));
        assert!(required.contains(&"maturana-agent-update"));
        assert!(required.contains(&"maturana-skill-create"));
        assert!(required.contains(&"maturana-tool-create"));
        assert!(required.contains(&"maturana-skill-deploy"));
        assert!(required.contains(&"maturana-security-review"));
        assert_eq!(required.len(), 10);
    }

    #[test]
    fn skill_validator_rejects_wrapper_shape() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-skill-validator-test-{}",
            std::process::id()
        ));
        let skill_dir = temp.join("thin-wrapper");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# thin-wrapper\n\nUse this skill when testing.\n\n## Procedure\n\nsimply run `maturana --help`.\n",
        )
        .unwrap();

        let report = validate_skill_pack(&temp).unwrap();
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("## Grounding")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("catch-all Procedure")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("thin-wrapper language")));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn codex_prompts_quote_descriptions_with_colons() {
        // A derived description whose first line contains a colon ("speech:")
        // must be emitted as a quoted YAML scalar, else Codex rejects the skill
        // ("mapping values are not allowed in this context").
        let temp = std::env::temp_dir().join(format!(
            "maturana-codex-prompts-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&temp);
        let skills_root = temp.join("skills");
        let skill_dir = skills_root.join("voicey");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# voicey\n\nUse this skill when an agent needs speech: transcribe audio to text.\n",
        )
        .unwrap();
        let dest = temp.join("dest");
        let count = sync_codex_prompts(&skills_root, Some(&dest), &[]).unwrap();
        assert_eq!(count, 1);

        let generated = fs::read_to_string(dest.join("voicey").join("SKILL.md")).unwrap();
        let desc_line = generated
            .lines()
            .find(|l| l.starts_with("description:"))
            .expect("description line present");
        // The value must be wrapped in double quotes so the inner colon is safe.
        let value = desc_line.trim_start_matches("description:").trim();
        assert!(
            value.starts_with('"') && value.ends_with('"'),
            "description not quoted: {desc_line}"
        );
        assert!(
            value.contains("speech:"),
            "colon-bearing text preserved: {desc_line}"
        );
        // name is quoted too.
        assert!(generated.contains("name: \"voicey\""));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn codex_prompts_install_enabled_plugin_skills() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-codex-plugin-skills-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&temp);
        let skills_root = temp.join("skills");
        let core_dir = skills_root.join("corey");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(
            core_dir.join("SKILL.md"),
            "# corey\n\nUse this skill when testing core install.\n",
        )
        .unwrap();
        let plugin_skill_path = temp.join("plugin/demo/SKILL.md");
        fs::create_dir_all(plugin_skill_path.parent().unwrap()).unwrap();
        fs::write(
            &plugin_skill_path,
            "# demo-plugin\n\nUse this skill when testing plugin install.\n",
        )
        .unwrap();
        let plugin_skills = vec![PluginSkillAsset {
            plugin: "demo".to_string(),
            feature: Some("demo-feature".to_string()),
            name: "demo-plugin".to_string(),
            path: plugin_skill_path,
            description: Some("Plugin-provided Codex skill".to_string()),
        }];
        let dest = temp.join("dest");

        let count = sync_codex_prompts(&skills_root, Some(&dest), &plugin_skills).unwrap();
        assert_eq!(count, 2);
        let generated = fs::read_to_string(dest.join("demo-plugin").join("SKILL.md")).unwrap();
        assert!(generated.contains("name: \"demo-plugin\""));
        assert!(generated.contains("description: \"Plugin-provided Codex skill\""));
        assert!(dest.join("corey").join("SKILL.md").exists());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn codex_prompts_reject_plugin_skill_shadowing() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-codex-plugin-shadow-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&temp);
        let skills_root = temp.join("skills");
        let core_dir = skills_root.join("same");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(
            core_dir.join("SKILL.md"),
            "# same\n\nUse this skill when testing core install.\n",
        )
        .unwrap();
        let plugin_skill_path = temp.join("plugin/same/SKILL.md");
        fs::create_dir_all(plugin_skill_path.parent().unwrap()).unwrap();
        fs::write(
            &plugin_skill_path,
            "# same\n\nUse this skill when testing plugin install.\n",
        )
        .unwrap();
        let plugin_skills = vec![PluginSkillAsset {
            plugin: "demo".to_string(),
            feature: None,
            name: "same".to_string(),
            path: plugin_skill_path,
            description: None,
        }];

        let error = sync_codex_prompts(&skills_root, Some(&temp.join("dest")), &plugin_skills)
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicts with an installed Codex skill"));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn skill_validator_requires_evidence_recovery_and_boundaries() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-skill-validator-sections-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let skill_dir = temp.join("thin-proof");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# thin-proof

Use this skill when testing.

## Grounding

Read `AGENTS.md` first.

## Preflight

- Check.

## Decision Path

- Decide.

## Actions

- Act.

## Evidence

- Command returned.

## Recovery

- Retry.

## Boundaries

- Do not bypass validation.
"#,
        )
        .unwrap();

        let report = validate_skill_pack(&temp).unwrap();
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least four concrete proof points")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least four failure-handling paths")));
        assert!(report
            .failures
            .iter()
            .any(|failure| failure.contains("at least three explicit 'Do not' limits")));

        let _ = fs::remove_dir_all(&temp);
    }
}
