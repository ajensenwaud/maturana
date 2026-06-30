use anyhow::Context;
use maturana_core::state::MaturanaHome;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// SSH command time budgets. QUICK bounds readiness/probe/log commands;
/// PROVISION covers first-boot installs and service restarts.
pub const SSH_TIMEOUT_QUICK: Duration = Duration::from_secs(30);
pub const SSH_TIMEOUT_PROVISION: Duration = Duration::from_secs(600);

/// Per-agent SSH host-key verification material: which known_hosts file to use
/// and whether to verify strictly (a pinned key is recorded) or `accept-new`
/// (trust-on-first-use migration for images created before host-key pinning).
#[derive(Debug, Clone)]
pub struct GuestHostKey {
    known_hosts: PathBuf,
    strict: bool,
}

impl GuestHostKey {
    pub fn resolve(home: &MaturanaHome, agent_id: &str, ip: &str) -> anyhow::Result<Self> {
        let state_dir = home.agent_dir(agent_id).join("state");
        let (known_hosts, strict) = maturana_core::ssh_pin::prepare_known_hosts(&state_dir, ip)?;
        Ok(Self {
            known_hosts,
            strict,
        })
    }

    pub fn options(&self) -> Vec<String> {
        maturana_core::ssh_pin::ssh_host_key_options(&self.known_hosts, self.strict)
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn run_ssh_with_stdin(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key: &GuestHostKey,
    remote_command: &str,
    stdin_text: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let mut command = Command::new("ssh");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        // Key-based only, never interactive: a not-yet-ready or auth-rejecting
        // guest must fail fast instead of stalling on a password/passphrase
        // prompt with no TTY.
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PreferredAuthentications=publickey")
        .arg("-i")
        .arg(ssh_key)
        .arg(format!("{ssh_user}@{ip}"))
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_text.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command.spawn().context("failed to start ssh")?;
    if let Some(stdin_text) = stdin_text {
        let mut stdin = child.stdin.take().context("failed to open ssh stdin")?;
        stdin.write_all(stdin_text.as_bytes())?;
    }

    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("ssh timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "ssh command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn wait_for_guest_ssh(
    guest_ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key: &GuestHostKey,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    // Keep the last attempt's error so the timeout failure names the real cause
    // (host-key mismatch / auth / connection reset) instead of a black box.
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match run_ssh_with_stdin(
            guest_ip,
            ssh_user,
            ssh_key,
            host_key,
            "echo ok",
            None,
            SSH_TIMEOUT_QUICK,
        ) {
            Ok(_) => return Ok(()),
            Err(error) => last_err = Some(format!("{error:#}")),
        }
        thread::sleep(Duration::from_secs(2));
    }
    anyhow::bail!(
        "guest SSH did not become reachable at {} within {}s (last SSH error: {})",
        guest_ip,
        timeout.as_secs(),
        last_err.as_deref().unwrap_or("none captured")
    )
}

pub fn copy_path_to_guest(
    ip: &str,
    ssh_user: &str,
    ssh_key: &Path,
    host_key: &GuestHostKey,
    local_path: &Path,
    remote_path: &str,
    recursive: bool,
) -> anyhow::Result<()> {
    let mut command = Command::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    let output = command
        .arg(local_path)
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start scp for {}", local_path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
