use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context;
use maturana_core::{inspect_agent, state::MaturanaHome, AgentSpec, HostProvider};

use crate::orchestration::run_dir;
use crate::ssh::GuestHostKey;

/// Normalize a manifest path to a safe relative path under an output directory.
/// Leading slash, `.`, `..`, empty components, and Windows separators are
/// stripped so a file manifest cannot escape the destination root.
pub fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in raw.replace('\\', "/").split('/') {
        match part {
            "" | "." | ".." => continue,
            p => out.push(p),
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The directory inside a worker VM where it writes produced files for a run.
pub fn remote_out_dir(run_id: &str) -> String {
    format!("/workspace/maturana-out-{run_id}")
}

/// The trailing component of a remote output directory.
pub fn out_basename(remote: &str) -> String {
    remote
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("out")
        .to_string()
}

/// Count regular files under `dir`, recursively. Missing/unreadable directories
/// count as zero so callers can use this for best-effort artifact collection.
pub fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files(&path);
            } else if path.is_file() {
                count += 1;
            }
        }
    }
    count
}

/// Recursively copy every file under `src` into `dst`, preserving layout.
/// Returns the relative paths written, normalized to forward slashes.
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    copy_tree_inner(src, src, dst, &mut names)?;
    Ok(names)
}

fn copy_tree_inner(
    root: &Path,
    cur: &Path,
    dst: &Path,
    names: &mut Vec<String>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(cur)? {
        let path = entry?.path();
        if path.is_dir() {
            copy_tree_inner(root, &path, dst, names)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let target = dst.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target)?;
            names.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Where collected files and summaries should be written: explicit `--output`
/// if given, otherwise `<home>/orchestration/<run_id>/output`.
pub fn output_dir_for(
    home: &MaturanaHome,
    run_id: &str,
    output: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    match output {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(run_dir(home, run_id)?.join("output")),
    }
}

/// The private key for SSHing into a guest, selected by provider. Firecracker
/// guests use the baked image key; anything else uses the default agent key.
pub fn guest_ssh_key(home: &MaturanaHome, agent_id: &str) -> PathBuf {
    let provider = AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md"))
        .ok()
        .map(|spec| spec.vm.provider);
    match provider {
        Some(HostProvider::Firecracker) => home
            .root()
            .join("images/firecracker/maturana-firecracker.id_rsa"),
        _ => home.root().join("keys/maturana-agent-ed25519"),
    }
}

pub fn resolve_transfer_ip(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<String> {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    if let Ok(spec) = AgentSpec::from_maturana_markdown(&spec_path) {
        if spec.vm.provider == HostProvider::Firecracker {
            if let Some(ip) = spec
                .vm
                .firecracker
                .as_ref()
                .map(|firecracker| firecracker.guest_ip.trim().to_string())
                .filter(|ip| !ip.is_empty())
            {
                return Ok(ip);
            }
            let metadata_path = home
                .agent_dir(agent_id)
                .join("state/firecracker-metadata.json");
            if let Ok(raw) = std::fs::read_to_string(&metadata_path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(ip) = value
                        .get("guest_ip")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|ip| !ip.is_empty())
                    {
                        return Ok(ip.to_string());
                    }
                }
            }
        }
    }
    inspect_agent(home, agent_id)?.ipv4.ok_or_else(|| {
        anyhow::anyhow!("could not discover live IP for {agent_id}; pass --guest-ip explicitly")
    })
}

pub fn fetch_live_path(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key: &GuestHostKey,
    remote_path: &str,
    local_path: &Path,
    allowed_roots: &[String],
    recursive: bool,
) -> anyhow::Result<()> {
    validate_guest_transfer_path(remote_path, allowed_roots)?;
    if let Some(parent) = local_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut command = Command::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PreferredAuthentications=publickey")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    command
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .arg(local_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command.output().context("failed to start scp")?;
    if !output.status.success() {
        anyhow::bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Copy the files a worker wrote into `remote_dir` out of its VM and into
/// `staging_dir`. Returns how many new files landed. Best-effort: unreachable
/// agents, empty dirs, and missing keys return 0 without aborting the run.
pub fn collect_step_artifacts(
    home: &MaturanaHome,
    agent_id: &str,
    remote_dir: &str,
    staging_dir: &Path,
) -> usize {
    let ip = match resolve_transfer_ip(home, agent_id) {
        Ok(ip) => ip,
        Err(error) => {
            eprintln!("  (could not resolve {agent_id} guest IP to collect files: {error})");
            return 0;
        }
    };
    let key = guest_ssh_key(home, agent_id);
    if !key.exists() {
        eprintln!(
            "  (no guest SSH key for {agent_id} at {}; cannot collect files)",
            key.display()
        );
        return 0;
    }
    let host_key = match GuestHostKey::resolve(home, agent_id, &ip) {
        Ok(host_key) => host_key,
        Err(error) => {
            eprintln!("  (could not prepare host key for {agent_id}: {error})");
            return 0;
        }
    };

    let probe = format!("ls -A {remote_dir} 2>/dev/null | head -1");
    match crate::ssh::run_ssh_with_stdin(
        &ip,
        "ubuntu",
        &key,
        &host_key,
        &probe,
        None,
        crate::ssh::SSH_TIMEOUT_QUICK,
    ) {
        Ok(listing) if !listing.trim().is_empty() => {}
        Ok(_) => return 0,
        Err(error) => {
            eprintln!("  (could not reach {agent_id} guest to collect files: {error})");
            return 0;
        }
    }
    if std::fs::create_dir_all(staging_dir).is_err() {
        return 0;
    }
    let roots = agent_transfer_roots(home, agent_id, false)
        .unwrap_or_else(|_| vec!["/workspace".to_string()]);
    let landing = staging_dir.join(out_basename(remote_dir));
    let before = count_files(&landing);
    match fetch_live_path(
        &ip,
        "ubuntu",
        &key,
        &host_key,
        remote_dir,
        staging_dir,
        &roots,
        true,
    ) {
        Ok(()) => count_files(&landing).saturating_sub(before),
        Err(error) => {
            eprintln!("  (could not collect files from {agent_id}: {error})");
            0
        }
    }
}

pub fn agent_transfer_roots(
    home: &MaturanaHome,
    agent_id: &str,
    writable_only: bool,
) -> anyhow::Result<Vec<String>> {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    let mut roots = default_guest_transfer_roots();
    if spec_path.exists() {
        let spec = AgentSpec::from_maturana_markdown(&spec_path)
            .with_context(|| format!("failed to parse {}", spec_path.display()))?;
        for mount in spec.filesystem.mounts {
            if writable_only && !mount.writable {
                continue;
            }
            if let Some(root) = normalize_guest_transfer_root(&mount.guest_path) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
    }
    Ok(roots)
}

pub fn default_guest_transfer_roots() -> Vec<String> {
    vec![
        "/workspace".to_string(),
        "/memory".to_string(),
        "/wiki".to_string(),
    ]
}

pub fn normalize_guest_transfer_root(root: &str) -> Option<String> {
    let root = root.trim().trim_end_matches('/');
    if root.is_empty() || root == "/" || !root.starts_with('/') {
        return None;
    }
    if root.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(root.to_string())
}

pub fn validate_guest_transfer_path(
    remote_path: &str,
    allowed_roots: &[String],
) -> anyhow::Result<()> {
    let path = remote_path.trim();
    if path.is_empty() {
        anyhow::bail!("remote path must not be empty");
    }
    if !path.starts_with('/') {
        anyhow::bail!("remote path must be absolute: {path}");
    }
    if path.split('/').any(|segment| segment == "..") {
        anyhow::bail!("remote path must not contain '..': {path}");
    }
    if !is_allowed_guest_transfer_path(path, allowed_roots) {
        anyhow::bail!(
            "remote path is outside allowed guest transfer roots ({}): {path}",
            allowed_roots.join(", ")
        );
    }
    Ok(())
}

fn is_allowed_guest_transfer_path(path: &str, allowed_roots: &[String]) -> bool {
    allowed_roots
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{root}/")))
}

pub fn remote_parent(remote_path: &str) -> Option<String> {
    let trimmed = remote_path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    if parent.is_empty() {
        Some("/".to_string())
    } else {
        Some(parent.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::state::MaturanaHome;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn safe_relative_path_blocks_traversal_and_absolutes() {
        assert_eq!(
            safe_relative_path("a/b.txt").unwrap(),
            PathBuf::from("a/b.txt")
        );
        assert_eq!(
            safe_relative_path("/etc/passwd").unwrap(),
            PathBuf::from("etc/passwd")
        );
        assert_eq!(safe_relative_path("../../x").unwrap(), PathBuf::from("x"));
        assert_eq!(
            safe_relative_path("src/./main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert!(safe_relative_path("../..").is_none());
        assert!(safe_relative_path("").is_none());
    }

    #[test]
    fn out_basename_is_the_last_segment() {
        assert_eq!(
            out_basename("/workspace/maturana-out-run-123"),
            "maturana-out-run-123"
        );
        assert_eq!(
            out_basename("/workspace/maturana-out-run-123/"),
            "maturana-out-run-123"
        );
    }

    #[test]
    fn copy_tree_preserves_layout_and_counts_files() {
        let base = temp_root("copytree");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("css")).unwrap();
        std::fs::write(src.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(src.join("css/style.css"), "body{}").unwrap();
        assert_eq!(count_files(&src), 2);
        assert_eq!(count_files(&base.join("nope")), 0);

        let mut names = copy_tree(&src, &dst).unwrap();
        names.sort();
        assert_eq!(
            names,
            vec!["css/style.css".to_string(), "index.html".to_string()]
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("index.html")).unwrap(),
            "<h1>hi</h1>"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("css/style.css")).unwrap(),
            "body{}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn output_dir_defaults_under_validated_run_dir() {
        let root = temp_root("output");
        let home = MaturanaHome::new(&root);

        assert_eq!(
            output_dir_for(&home, "run-1", None).unwrap(),
            root.join("orchestration/run-1/output")
        );
        assert!(output_dir_for(&home, "../escape", None).is_err());
        assert_eq!(
            output_dir_for(&home, "../escape", Some(Path::new("/tmp/out"))).unwrap(),
            PathBuf::from("/tmp/out")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn guest_ssh_key_uses_firecracker_image_key_only_for_firecracker_specs() {
        let root = temp_root("ssh-key");
        let home = MaturanaHome::new(&root);
        let firecracker = home.agent_dir("fc");
        std::fs::create_dir_all(&firecracker).unwrap();
        std::fs::write(
            firecracker.join("MATURANA.md"),
            "---\nidentity: { id: fc, name: FC, purpose: test }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux }\n---\n# FC\n",
        )
        .unwrap();
        let hyperv = home.agent_dir("hv");
        std::fs::create_dir_all(&hyperv).unwrap();
        std::fs::write(
            hyperv.join("MATURANA.md"),
            "---\nidentity: { id: hv, name: HV, purpose: test }\nruntime: { harness: codex }\nvm: { provider: hyper-v, guest_os: linux }\n---\n# HV\n",
        )
        .unwrap();

        assert_eq!(
            guest_ssh_key(&home, "fc"),
            root.join("images/firecracker/maturana-firecracker.id_rsa")
        );
        assert_eq!(
            guest_ssh_key(&home, "hv"),
            root.join("keys/maturana-agent-ed25519")
        );
        assert_eq!(
            guest_ssh_key(&home, "missing"),
            root.join("keys/maturana-agent-ed25519")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn guest_transfer_paths_allow_declared_roots() {
        let roots = default_guest_transfer_roots();
        assert!(validate_guest_transfer_path("/workspace/output.txt", &roots).is_ok());
        assert!(validate_guest_transfer_path("/memory/state.json", &roots).is_ok());
        assert!(validate_guest_transfer_path("/wiki/page.md", &roots).is_ok());
        assert!(validate_guest_transfer_path("/workspace", &roots).is_ok());
    }

    #[test]
    fn guest_transfer_paths_allow_extra_declared_roots() {
        let roots = vec!["/workspace".to_string(), "/scratch".to_string()];
        assert!(validate_guest_transfer_path("/scratch/output.txt", &roots).is_ok());
    }

    #[test]
    fn guest_transfer_paths_reject_escape_paths() {
        let roots = default_guest_transfer_roots();
        assert!(validate_guest_transfer_path("", &roots).is_err());
        assert!(validate_guest_transfer_path("workspace/output.txt", &roots).is_err());
        assert!(validate_guest_transfer_path("/workspace/../etc/passwd", &roots).is_err());
        assert!(validate_guest_transfer_path("/etc/passwd", &roots).is_err());
        assert!(validate_guest_transfer_path("/home/ubuntu/.codex/auth.json", &roots).is_err());
    }

    #[test]
    fn guest_transfer_roots_ignore_unsafe_mount_roots() {
        assert_eq!(
            normalize_guest_transfer_root("/scratch/"),
            Some("/scratch".to_string())
        );
        assert_eq!(normalize_guest_transfer_root("/"), None);
        assert_eq!(normalize_guest_transfer_root("relative"), None);
        assert_eq!(normalize_guest_transfer_root("/workspace/../etc"), None);
    }

    #[test]
    fn remote_parent_handles_absolute_guest_paths() {
        assert_eq!(
            remote_parent("/workspace/file.txt"),
            Some("/workspace".to_string())
        );
        assert_eq!(
            remote_parent("/workspace/dir/"),
            Some("/workspace".to_string())
        );
        assert_eq!(remote_parent("/file.txt"), Some("/".to_string()));
        assert_eq!(remote_parent("file.txt"), None);
    }

    #[test]
    fn transfer_ip_prefers_firecracker_spec_then_metadata() {
        let root = temp_root("transfer-ip");
        let home = MaturanaHome::new(&root);
        let spec_dir = home.agent_dir("spec-ip");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(
            spec_dir.join("MATURANA.md"),
            "---\nidentity: { id: spec-ip, name: Spec IP, purpose: test }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux, firecracker: { kernel_image: img/vmlinux.bin, rootfs_image: img/rootfs.ext4, guest_ip: 172.16.0.9 } }\n---\n# Spec IP\n",
        )
        .unwrap();
        assert_eq!(resolve_transfer_ip(&home, "spec-ip").unwrap(), "172.16.0.9");

        let metadata_dir = home.agent_dir("metadata-ip");
        std::fs::create_dir_all(metadata_dir.join("state")).unwrap();
        std::fs::write(
            metadata_dir.join("MATURANA.md"),
            "---\nidentity: { id: metadata-ip, name: Metadata IP, purpose: test }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux }\n---\n# Metadata IP\n",
        )
        .unwrap();
        std::fs::write(
            metadata_dir.join("state/firecracker-metadata.json"),
            r#"{"guest_ip":"172.16.0.10"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_transfer_ip(&home, "metadata-ip").unwrap(),
            "172.16.0.10"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_step_artifacts_returns_zero_when_key_is_missing() {
        let root = temp_root("collect-no-key");
        let home = MaturanaHome::new(&root);
        let agent_dir = home.agent_dir("spec-ip");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("MATURANA.md"),
            "---\nidentity: { id: spec-ip, name: Spec IP, purpose: test }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux, firecracker: { kernel_image: img/vmlinux.bin, rootfs_image: img/rootfs.ext4, guest_ip: 172.16.0.9 } }\n---\n# Spec IP\n",
        )
        .unwrap();

        assert_eq!(
            collect_step_artifacts(
                &home,
                "spec-ip",
                "/workspace/maturana-out-run-1",
                &root.join("staging")
            ),
            0
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "maturana-ops-artifacts-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
