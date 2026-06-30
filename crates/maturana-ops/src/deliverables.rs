use std::path::{Path, PathBuf};

use serde::Deserialize;

use maturana_core::state::MaturanaHome;

use crate::{artifacts::safe_relative_path, orchestration::run_dir};

/// What an orchestration run produced and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deliverable {
    Files { dir: PathBuf, names: Vec<String> },
    Prose { path: PathBuf },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct FileManifest {
    files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub content: String,
}

/// Pull a `{"files":[...]}` manifest out of a synthesizer reply, if present.
/// Accepts a bare JSON object or one inside a ```json fence. Returns `None` for
/// prose so callers can fall back to writing the text as-is.
pub fn extract_file_manifest(reply: &str) -> Option<Vec<ManifestFile>> {
    let candidate = if let Some(start) = reply.find("```json") {
        let rest = &reply[start + "```json".len()..];
        rest.find("```").map(|end| rest[..end].trim().to_string())
    } else {
        let start = reply.find('{')?;
        let end = reply.rfind('}')?;
        if end > start {
            Some(reply[start..=end].to_string())
        } else {
            None
        }
    }?;
    let manifest: FileManifest = serde_json::from_str(&candidate).ok()?;
    if manifest.files.is_empty() {
        return None;
    }
    Some(manifest.files)
}

/// Write the synthesizer's deliverable. A file manifest becomes real files under
/// the output directory (`--output` or `<run>/output/`); prose becomes a single
/// file (`--output` or `<run>/answer.md`).
pub fn write_deliverable(
    home: &MaturanaHome,
    run_id: &str,
    output: Option<&Path>,
    reply: &str,
) -> anyhow::Result<Deliverable> {
    if let Some(files) = extract_file_manifest(reply) {
        let dir = match output {
            Some(path) => path.to_path_buf(),
            None => run_dir(home, run_id)?.join("output"),
        };
        std::fs::create_dir_all(&dir)?;
        let mut names = Vec::new();
        for file in files {
            let Some(rel) = safe_relative_path(&file.path) else {
                continue;
            };
            let dest = dir.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, file.content)?;
            names.push(rel.to_string_lossy().to_string());
        }
        let _ = std::fs::write(run_dir(home, run_id)?.join("answer.md"), reply);
        return Ok(Deliverable::Files { dir, names });
    }

    let path = match output {
        Some(p) if p.is_dir() || p.to_string_lossy().ends_with('/') => {
            std::fs::create_dir_all(p)?;
            p.join("answer.md")
        }
        Some(p) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p.to_path_buf()
        }
        None => run_dir(home, run_id)?.join("answer.md"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, reply)?;
    Ok(Deliverable::Prose { path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::state::MaturanaHome;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn manifest_extracted_from_bare_json_and_fenced() {
        let files = extract_file_manifest(
            r#"{"files":[{"path":"index.html","content":"<h1>hi</h1>"},{"path":"game.js","content":"x=1"}]}"#,
        )
        .expect("bare json manifest");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "index.html");

        let fenced = "Here you go:\n```json\n{\"files\":[{\"path\":\"a.py\",\"content\":\"print(1)\"}]}\n```\nDone.";
        let files = extract_file_manifest(fenced).expect("fenced manifest");
        assert_eq!(files[0].path, "a.py");
    }

    #[test]
    fn prose_is_not_a_manifest() {
        assert!(extract_file_manifest("The answer is 42. Paris has ~2.1M people.").is_none());
        assert!(extract_file_manifest(r#"{"answer":"42"}"#).is_none());
        assert!(extract_file_manifest(r#"{"files":[]}"#).is_none());
    }

    #[test]
    fn write_deliverable_materializes_files_and_keeps_raw_answer() {
        let root = temp_root("files");
        let home = MaturanaHome::new(&root);

        let deliverable = write_deliverable(
            &home,
            "run-1",
            None,
            r#"{"files":[{"path":"/app/../index.html","content":"<h1>hi</h1>"}]}"#,
        )
        .unwrap();

        let Deliverable::Files { dir, names } = deliverable else {
            panic!("expected files");
        };
        assert_eq!(dir, root.join("orchestration/run-1/output"));
        assert_eq!(names, vec!["app/index.html".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("app/index.html")).unwrap(),
            "<h1>hi</h1>"
        );
        assert!(
            std::fs::read_to_string(root.join("orchestration/run-1/answer.md"))
                .unwrap()
                .contains("\"files\"")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn write_deliverable_writes_prose_answer() {
        let root = temp_root("prose");
        let home = MaturanaHome::new(&root);

        let deliverable = write_deliverable(&home, "run-1", None, "hello").unwrap();

        let Deliverable::Prose { path } = deliverable else {
            panic!("expected prose");
        };
        assert_eq!(path, root.join("orchestration/run-1/answer.md"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "maturana-ops-deliverables-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
