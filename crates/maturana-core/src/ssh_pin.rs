//! Guest SSH host-key pinning.
//!
//! The host provisions guests over SSH/SCP, pushing OAuth credentials, the
//! pipelock CA, and the sessiond token. Without host-key verification anything
//! that can win the guest's IP can impersonate the SSH server and capture those
//! secrets. This module is the single place that:
//!
//! - generates an ed25519 **host** keypair the host installs into a guest
//!   (Hyper-V via cloud-init `ssh_keys:`; Firecracker bakes it into the rootfs),
//! - records the pinned public key per agent and writes a `known_hosts` file,
//! - builds the `ssh`/`scp` options that verify against it.
//!
//! Migration is graceful: an agent with no pinned key yet (e.g. a VM created
//! before pinning existed) uses `accept-new` — trust-on-first-use that pins the
//! live key and detects later swaps — instead of the old `=no` that trusted any
//! server on every connection.

use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// File name (under an agent's `state/` dir) holding the pinned host public key.
pub const HOST_PUBLIC_KEY_FILE: &str = "ssh_host_ed25519.pub";
/// File name (under an agent's `state/` dir) of the generated known_hosts file.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";
/// Base name of the generated host private key.
pub const HOST_KEY_FILE: &str = "ssh_host_ed25519";

/// A freshly generated ed25519 host keypair on the host side.
pub struct HostKeypair {
    pub private_key_path: PathBuf,
    pub public_key_path: PathBuf,
    /// The `ssh-ed25519 AAAA… comment` line.
    pub public_line: String,
    /// The OpenSSH private key file contents (for cloud-init injection).
    pub private_pem: String,
}

/// Generate an ed25519 host keypair under `dir`, overwriting any existing one
/// (each launch recreates the guest, so its host key is regenerated too).
pub fn generate_host_keypair(dir: &Path) -> anyhow::Result<HostKeypair> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let private_key_path = dir.join(HOST_KEY_FILE);
    let public_key_path = dir.join(format!("{HOST_KEY_FILE}.pub"));
    // ssh-keygen refuses to overwrite, so clear any prior key first.
    let _ = fs::remove_file(&private_key_path);
    let _ = fs::remove_file(&public_key_path);

    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg("maturana-host")
        .arg("-f")
        .arg(&private_key_path)
        .status()
        .context("failed to run ssh-keygen for guest host key")?;
    if !status.success() {
        anyhow::bail!("ssh-keygen failed to generate the guest host key");
    }

    let private_pem = fs::read_to_string(&private_key_path)
        .with_context(|| format!("failed to read {}", private_key_path.display()))?;
    let public_line = read_public_line(&public_key_path)?;
    Ok(HostKeypair {
        private_key_path,
        public_key_path,
        public_line,
        private_pem,
    })
}

/// Read and trim a `*.pub` line (`ssh-ed25519 AAAA… comment`).
pub fn read_public_line(public_key_path: &Path) -> anyhow::Result<String> {
    let raw = fs::read_to_string(public_key_path)
        .with_context(|| format!("failed to read {}", public_key_path.display()))?;
    let line = raw.trim().to_string();
    if line.is_empty() {
        anyhow::bail!("host public key {} is empty", public_key_path.display());
    }
    Ok(line)
}

/// Write a single-host `known_hosts` file pinning `public_line` for `host`.
/// Overwrites: the Hyper-V guest IP is dynamic, so this is rewritten with the
/// current address before each connection.
pub fn write_known_hosts(
    known_hosts_path: &Path,
    host: &str,
    public_line: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = known_hosts_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(known_hosts_path, format!("{host} {public_line}\n"))
        .with_context(|| format!("failed to write {}", known_hosts_path.display()))
}

/// Resolve the known_hosts file and verification mode for an agent connecting to
/// `host`. If a pinned public key has been recorded under `state_dir`, write the
/// known_hosts entry and verify strictly; otherwise fall back to `accept-new`
/// (trust-on-first-use) so a pre-pinning guest still connects and gets pinned.
pub fn prepare_known_hosts(state_dir: &Path, host: &str) -> anyhow::Result<(PathBuf, bool)> {
    let known_hosts_path = state_dir.join(KNOWN_HOSTS_FILE);
    let pinned = state_dir.join(HOST_PUBLIC_KEY_FILE);
    if pinned.exists() {
        let public_line = read_public_line(&pinned)?;
        write_known_hosts(&known_hosts_path, host, &public_line)?;
        Ok((known_hosts_path, true))
    } else {
        // accept-new needs the file to exist and be writable.
        if let Some(parent) = known_hosts_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !known_hosts_path.exists() {
            fs::write(&known_hosts_path, "")?;
        }
        Ok((known_hosts_path, false))
    }
}

/// The `ssh`/`scp` `-o` options that verify the guest's host key against
/// `known_hosts`. `strict` selects `yes` (a key is pinned) vs `accept-new`
/// (pin-on-first-use migration).
pub fn ssh_host_key_options(known_hosts: &Path, strict: bool) -> Vec<String> {
    let mode = if strict { "yes" } else { "accept-new" };
    vec![
        "-o".to_string(),
        format!("StrictHostKeyChecking={mode}"),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
    ]
}

/// Render the cloud-init `ssh_keys:` block that installs the host keypair into a
/// Hyper-V guest on first boot. Indented to nest under a top-level cloud-config.
pub fn cloud_init_ssh_keys_block(private_pem: &str, public_line: &str) -> String {
    let mut block = String::from("ssh_keys:\n  ed25519_private: |\n");
    for line in private_pem.lines() {
        block.push_str("    ");
        block.push_str(line);
        block.push('\n');
    }
    block.push_str("  ed25519_public: ");
    block.push_str(public_line.trim());
    block.push('\n');
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("maturana-sshpin-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn options_select_strict_or_accept_new() {
        let kh = Path::new("/tmp/known_hosts");
        let strict = ssh_host_key_options(kh, true);
        assert!(strict.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(strict
            .iter()
            .any(|a| a.starts_with("UserKnownHostsFile=") && a.contains("known_hosts")));
        let lax = ssh_host_key_options(kh, false);
        assert!(lax.contains(&"StrictHostKeyChecking=accept-new".to_string()));
    }

    #[test]
    fn known_hosts_pins_host_and_key() {
        let dir = temp_dir("kh");
        let path = dir.join("known_hosts");
        write_known_hosts(&path, "172.26.1.2", "ssh-ed25519 AAAAKEY maturana-host").unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert_eq!(body, "172.26.1.2 ssh-ed25519 AAAAKEY maturana-host\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_is_strict_only_when_pinned_key_present() {
        let dir = temp_dir("prep");
        // No pinned key yet -> accept-new migration.
        let (kh, strict) = prepare_known_hosts(&dir, "10.0.0.2").unwrap();
        assert!(!strict);
        assert!(kh.exists());

        // Record a pinned key -> strict, and known_hosts gets the entry.
        fs::write(
            dir.join(HOST_PUBLIC_KEY_FILE),
            "ssh-ed25519 AAAAPINNED maturana-host\n",
        )
        .unwrap();
        let (kh2, strict2) = prepare_known_hosts(&dir, "10.0.0.2").unwrap();
        assert!(strict2);
        let body = fs::read_to_string(&kh2).unwrap();
        assert_eq!(body, "10.0.0.2 ssh-ed25519 AAAAPINNED maturana-host\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cloud_init_block_indents_private_key_and_emits_public() {
        let pem =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\ndef\n-----END OPENSSH PRIVATE KEY-----";
        let block = cloud_init_ssh_keys_block(pem, "ssh-ed25519 AAAAPUB maturana-host\n");
        assert!(block.starts_with("ssh_keys:\n  ed25519_private: |\n"));
        assert!(block.contains("    -----BEGIN OPENSSH PRIVATE KEY-----\n"));
        assert!(block.contains("    abc\n"));
        assert!(block
            .trim_end()
            .ends_with("ed25519_public: ssh-ed25519 AAAAPUB maturana-host"));
    }
}
