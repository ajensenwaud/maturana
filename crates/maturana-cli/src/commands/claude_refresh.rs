use clap::{Args, Subcommand};
use maturana_core::{spec::HarnessRuntime, state::MaturanaHome};
use maturana_ops::{
    firecracker::firecracker_profile_for,
    guest_worker::{install_guest_worker, GuestWorkerInstall},
};
use std::{path::PathBuf, thread, time::Duration};

/// Keep claude-code OAuth tokens fresh host-side.
#[derive(Debug, Args)]
pub(crate) struct ClaudeRefreshCommand {
    #[command(subcommand)]
    pub(crate) command: ClaudeRefreshSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ClaudeRefreshSubcommand {
    /// Do ONE real refresh against the host-auth creds to verify the endpoint,
    /// rotating + writing the result. Prints success/expiry only, never tokens.
    Probe {
        #[arg(
            long,
            default_value = ".maturana/host-auth/claude-code/.credentials.json"
        )]
        creds: PathBuf,
    },
    /// Run the refresh daemon: watch host-auth creds and refresh before expiry.
    Serve {
        /// claude-code agent ids to keep refreshed + re-pushed. Empty = all.
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(
            long,
            default_value = ".maturana/host-auth/claude-code/.credentials.json"
        )]
        creds: PathBuf,
        #[arg(long, default_value_t = 300)]
        poll_seconds: u64,
    },
}

pub(crate) fn run_claude_refresh(
    home: &MaturanaHome,
    command: ClaudeRefreshCommand,
) -> anyhow::Result<()> {
    use maturana_core::claude_refresh as cr;
    match command.command {
        ClaudeRefreshSubcommand::Probe { creds } => {
            let creds_path = resolve_claude_creds(home, creds);
            let current = cr::read_claude_creds(&creds_path)?;
            let now = chrono::Utc::now().timestamp_millis();
            let mins = (current.expires_at_ms - now) / 60000;
            println!("current token expires in {mins} min; attempting one refresh…");
            let rotated = cr::refresh_claude_token(&current)?;
            cr::write_claude_creds(&creds_path, &rotated)?;
            let new_mins = (rotated.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 60000;
            println!("refresh OK — endpoint verified; new token expires in {new_mins} min");
            println!("(rotated creds written to {})", creds_path.display());
            Ok(())
        }
        ClaudeRefreshSubcommand::Serve {
            agent_ids,
            creds,
            poll_seconds,
        } => {
            let creds_path = resolve_claude_creds(home, creds);
            println!("claude-refresh daemon: watching {}", creds_path.display());
            loop {
                if let Err(error) = claude_refresh_tick(home, &creds_path, &agent_ids) {
                    eprintln!("claude-refresh: {error:#}");
                }
                thread::sleep(Duration::from_secs(poll_seconds.max(30)));
            }
        }
    }
}

/// Resolve the Claude credentials path. The default (`.maturana/host-auth/...`)
/// is repo-root-relative, so a bare relative path must resolve against the repo
/// root (the parent of `--home`) -- not the cwd. Under the boot scheduled task
/// cwd is `System32`, which is exactly where `absolute_or_cwd` went wrong and
/// left the claude token un-refreshed at boot.
fn resolve_claude_creds(home: &MaturanaHome, creds: PathBuf) -> PathBuf {
    if creds.is_absolute() {
        return creds;
    }
    let repo_root = home.root().parent().unwrap_or_else(|| home.root());
    repo_root.join(creds)
}

/// One daemon cycle: refresh the host token if near expiry, then re-push to any
/// idle named claude agents (skips busy ones; the wide pre-expiry window means
/// the next cycle catches them).
fn claude_refresh_tick(
    home: &MaturanaHome,
    creds_path: &std::path::Path,
    agent_ids: &[String],
) -> anyhow::Result<()> {
    use maturana_core::claude_refresh as cr;
    let creds = cr::read_claude_creds(creds_path)?;
    let now = chrono::Utc::now().timestamp_millis();
    if !cr::needs_refresh(&creds, now, cr::REFRESH_SKEW) {
        return Ok(());
    }
    let rotated = cr::refresh_claude_token(&creds)?;
    cr::write_claude_creds(creds_path, &rotated)?;
    let mins = (rotated.expires_at_ms - chrono::Utc::now().timestamp_millis()) / 60000;
    println!("claude-refresh: rotated host token (expires in {mins} min)");
    for agent_id in agent_ids {
        match worker_is_idle(home, agent_id) {
            true => {
                if let Err(error) = repush_claude_auth(home, agent_id) {
                    eprintln!("claude-refresh: re-push {agent_id} failed: {error:#}");
                } else {
                    println!("claude-refresh: re-pushed fresh creds to {agent_id}");
                }
            }
            false => println!("claude-refresh: {agent_id} busy; will re-push next cycle"),
        }
    }
    Ok(())
}

fn worker_is_idle(home: &MaturanaHome, agent_id: &str) -> bool {
    let path = home.agent_dir(agent_id).join("worker-status.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
        .map(|s| s == "idle" || s == "polling")
        .unwrap_or(true)
}

/// Re-push fresh claude creds to a running guest via the existing auth install
/// path (resolves the guest IP from the live provider status).
fn repush_claude_auth(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let status = maturana_core::inspect_agent(home, agent_id)?;
    let ip = status
        .ipv4
        .ok_or_else(|| anyhow::anyhow!("{agent_id} has no IPv4 yet"))?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: agent_id.to_string(),
            // Use the SAME canonical session id as the launch/repair path
            // (`profile.session_id`, e.g. `claude-main`), not `{agent_id}-main`.
            // The claude-refresh daemon re-renders the guest worker + the
            // materialized `sessiond.env` every idle cycle; if it used a
            // different session id than the one the guest worker actually claims
            // from, it silently repoints the host plane (and Telegram) at a dead
            // session while the guest answers on the original one.
            session_id: firecracker_profile_for(agent_id)
                .map(|profile| profile.session_id.to_string())
                .unwrap_or_else(|| format!("{agent_id}-main")),
            harness: HarnessRuntime::ClaudeCode,
            guest_ip: ip,
            ssh_user: "ubuntu".to_string(),
            ssh_key: PathBuf::from(".maturana/keys/maturana-firecracker.id_rsa"),
            harness_auth_guest_path: "/home/ubuntu/.claude".to_string(),
            sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
            sessiond_token_path: home.root().join("sessiond/token"),
            // Do NOT re-push host-auth over a live guest. Claude Code rotates
            // its OAuth refresh token on every self-refresh, so the guest's
            // `.credentials.json` is newer than the host staging copy. The guest
            // owns its refresh lineage; the daemon only keeps the host seed
            // fresh + re-renders env/runner.
            auth_source: None,
            install_harness: false,
            force_reseed_auth: false,
        },
    )
}
