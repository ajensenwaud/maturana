use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use maturana_core::{
    audit::{append_event, AuditEvent},
    inspect_agent, materialize_agent,
    session_db::{list_undelivered, mark_delivered, session_paths},
    spec::AgentSpec,
    state::MaturanaHome,
    stop_agent, LaunchMode, LiveAgentStatus,
};
use maturana_ops::{
    artifacts::{
        agent_transfer_roots, fetch_live_path, guest_ssh_key, remote_parent, resolve_transfer_ip,
        validate_guest_transfer_path,
    },
    hostd::live_agent_ip,
    runtime_plane::ensure_graph_token,
    ssh::{run_ssh_with_stdin, shell_quote, GuestHostKey, SSH_TIMEOUT_QUICK},
};

use crate::{channels, tui};

#[derive(Debug, Args)]
pub(crate) struct AgentCommand {
    #[command(subcommand)]
    command: AgentSubcommand,
}

#[derive(Debug, Subcommand)]
enum AgentSubcommand {
    Launch {
        #[arg(default_value = "MATURANA.md")]
        spec: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    Inspect {
        agent_id: String,
        #[arg(long)]
        live: bool,
        #[arg(long)]
        guest: bool,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
    },
    Stop {
        agent_id: String,
        #[arg(long)]
        live: bool,
    },
    /// Open an interactive console TUI to chat with a running agent.
    Chat {
        agent_id: String,
        /// Seconds to wait for each reply before showing a timeout.
        #[arg(long, default_value_t = 180)]
        timeout_seconds: u64,
    },
    Run {
        agent_id: String,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
    },
    Logs {
        agent_id: String,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long, value_enum, default_value_t = LogKind::Agent)]
        kind: LogKind,
        #[arg(long, default_value_t = 80)]
        lines: u16,
    },
    Fetch {
        agent_id: String,
        remote_path: String,
        local_path: PathBuf,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        recursive: bool,
    },
    Push {
        agent_id: String,
        local_path: PathBuf,
        remote_path: String,
        #[arg(long)]
        ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long)]
        recursive: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogKind {
    Agent,
    Error,
    Stdout,
    Stderr,
    LastMessage,
}

pub(crate) fn handle_agent(home: &MaturanaHome, command: AgentCommand) -> anyhow::Result<()> {
    match command.command {
        AgentSubcommand::Launch { spec, apply } => {
            let raw = fs::read_to_string(&spec)
                .with_context(|| format!("failed to read {}", spec.display()))?;
            let parsed = AgentSpec::from_maturana_markdown(&spec)
                .with_context(|| format!("failed to parse {}", spec.display()))?;
            // Graph is on by default; guest provisioning embeds the graph
            // token (read_graph_token). Generate it now so an --apply launch
            // gives the guest working graph access without a later re-provision.
            if apply && parsed.knowledge_graph.enabled {
                ensure_graph_token(home)?;
            }
            let mode = if apply {
                LaunchMode::Apply
            } else {
                LaunchMode::DryRun
            };
            let materialized = materialize_agent(&parsed, &raw, home, mode)?;
            println!(
                "agent {} materialized at {}",
                materialized.agent_id,
                materialized.agent_dir.display()
            );
            println!(
                "launch plan: {}",
                materialized.agent_dir.join("launch-plan.json").display()
            );
        }
        AgentSubcommand::Inspect {
            agent_id,
            live,
            guest,
            ip,
            ssh_user,
            ssh_key,
        } => {
            let agent_dir = home.agent_dir(&agent_id);
            if !agent_dir.exists() {
                anyhow::bail!("agent does not exist: {}", agent_id);
            }
            println!("agent: {agent_id}");
            println!("dir: {}", agent_dir.display());
            println!("spec: {}", agent_dir.join("MATURANA.md").display());
            println!("plan: {}", agent_dir.join("launch-plan.json").display());
            if guest && !live {
                anyhow::bail!("agent inspect --guest requires --live");
            }
            let spec = if guest || ip.is_some() {
                Some(AgentSpec::from_maturana_markdown(
                    agent_dir.join("MATURANA.md"),
                )?)
            } else {
                None
            };
            if live {
                let status = inspect_agent(home, &agent_id)?;
                print_live_agent_status(&status);
                audit_agent_event(
                    home,
                    &agent_id,
                    "agent.inspect.live",
                    format!("inspected live {} state", status.provider),
                )?;

                if guest || ip.is_some() {
                    let guest_ip = ip.or_else(|| status.ipv4.clone()).ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not discover live IP for {agent_id}; pass --ip explicitly"
                        )
                    })?;
                    let host_key = GuestHostKey::resolve(home, &agent_id, &guest_ip)?;
                    print_live_guest_state(
                        &guest_ip,
                        &ssh_user,
                        &ssh_key,
                        &host_key,
                        spec.as_ref()
                            .map(|spec| spec.browser.headless_chrome)
                            .unwrap_or(false),
                    )?;
                    audit_agent_event(
                        home,
                        &agent_id,
                        "agent.inspect.live.guest",
                        format!("inspected live guest at {guest_ip} over ssh"),
                    )?;
                }
            }
        }
        AgentSubcommand::Stop { agent_id, live } => {
            if !live {
                anyhow::bail!("agent stop currently requires --live");
            }
            stop_agent(home, &agent_id)?;
        }
        AgentSubcommand::Chat {
            agent_id,
            timeout_seconds,
        } => {
            tui::run_chat(home, &agent_id, timeout_seconds)?;
        }
        AgentSubcommand::Run {
            agent_id,
            prompt,
            prompt_file,
            ip: _,
            ssh_user: _,
            ssh_key: _,
            wait,
            timeout_seconds,
        } => {
            let prompt = read_agent_prompt(prompt, prompt_file)?;
            let queued = enqueue_agent_run(home, &agent_id, &prompt)?;
            audit_agent_event(
                home,
                &agent_id,
                "agent.run.live",
                format!("queued live prompt in session {}", queued.session_id),
            )?;
            println!(
                "queued prompt for {agent_id} session {} message {}",
                queued.session_id, queued.message_id
            );
            if wait {
                let output = wait_for_agent_run(home, &agent_id, &queued, timeout_seconds)?;
                audit_agent_event(
                    home,
                    &agent_id,
                    "agent.run.live.completed",
                    format!("session prompt completed as {}", output.message_id),
                )?;
                println!("{}", output.text);
            }
        }
        AgentSubcommand::Logs {
            agent_id,
            ip,
            ssh_user,
            ssh_key,
            kind,
            lines,
        } => {
            let ip = match ip {
                Some(ip) => ip,
                None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not discover live IP for {agent_id}; pass --ip explicitly"
                    )
                })?,
            };
            let host_key = GuestHostKey::resolve(home, &agent_id, &ip)?;
            let output = read_live_log(&ip, &ssh_user, &ssh_key, &host_key, kind, lines)?;
            audit_agent_event(
                home,
                &agent_id,
                "agent.logs.live",
                format!("read live guest log {} at {ip}", kind.as_str()),
            )?;
            print!("{output}");
        }
        AgentSubcommand::Fetch {
            agent_id,
            remote_path,
            local_path,
            ip,
            ssh_user,
            ssh_key,
            recursive,
        } => {
            let ip = match ip {
                Some(ip) => ip,
                None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not discover live IP for {agent_id}; pass --ip explicitly"
                    )
                })?,
            };
            let transfer_roots = agent_transfer_roots(home, &agent_id, false)?;
            let host_key = GuestHostKey::resolve(home, &agent_id, &ip)?;
            fetch_live_path(
                &ip,
                &ssh_user,
                &ssh_key,
                &host_key,
                &remote_path,
                &local_path,
                &transfer_roots,
                recursive,
            )?;
            audit_agent_event(
                home,
                &agent_id,
                "agent.fetch.live",
                format!(
                    "fetched {remote_path} from guest at {ip} to {}",
                    local_path.display()
                ),
            )?;
            println!(
                "fetched {agent_id}:{remote_path} from {ip} to {}",
                local_path.display()
            );
        }
        AgentSubcommand::Push {
            agent_id,
            local_path,
            remote_path,
            ip,
            ssh_user,
            ssh_key,
            recursive,
        } => {
            let ip = match ip {
                Some(ip) => ip,
                None => live_agent_ip(&agent_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not discover live IP for {agent_id}; pass --ip explicitly"
                    )
                })?,
            };
            let transfer_roots = agent_transfer_roots(home, &agent_id, true)?;
            let host_key = GuestHostKey::resolve(home, &agent_id, &ip)?;
            push_live_path(
                &ip,
                &ssh_user,
                &ssh_key,
                &host_key,
                &local_path,
                &remote_path,
                &transfer_roots,
                recursive,
            )?;
            audit_agent_event(
                home,
                &agent_id,
                "agent.push.live",
                format!(
                    "pushed {} to guest at {ip}:{remote_path}",
                    local_path.display()
                ),
            )?;
            println!(
                "pushed {} to {agent_id}:{remote_path} at {ip}",
                local_path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn deliver_image_to_guest(
    home: &MaturanaHome,
    agent_id: &str,
    local_path: &Path,
) -> anyhow::Result<String> {
    let ip = resolve_transfer_ip(home, agent_id)?;
    let key = guest_ssh_key(home, agent_id);
    if !key.exists() {
        anyhow::bail!("no guest SSH key for {agent_id}");
    }
    let host_key = GuestHostKey::resolve(home, agent_id, &ip)?;
    let file_name = local_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let remote = format!("/workspace/inbox/{file_name}");
    let roots = agent_transfer_roots(home, agent_id, true)
        .unwrap_or_else(|_| vec!["/workspace".to_string()]);
    push_live_path(
        &ip,
        "ubuntu",
        &key,
        &host_key,
        &local_path.to_path_buf(),
        &remote,
        &roots,
        false,
    )?;
    Ok(remote)
}

pub(crate) fn vision_prompt_text(caption: Option<&str>, guest_path: &str) -> String {
    match caption.map(str::trim).filter(|c| !c.is_empty()) {
        Some(caption) => format!(
            "{caption}\n\n[The user attached an image, saved in your workspace at {guest_path}. Open and view it to answer.]"
        ),
        None => format!(
            "[The user sent an image, saved in your workspace at {guest_path}. Open and view it, then describe it or act on it.]"
        ),
    }
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

pub(crate) fn agent_chat_turn(
    home: &MaturanaHome,
    agent_id: &str,
    prompt: &str,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    // The console TUI goes through the SAME shared front door as every other
    // channel (Telegram, Discord, web), so it gets identical behaviour: the user
    // turn recorded under `console_chat_key`, the recent transcript injected
    // (turn-to-turn memory), and the current model/reasoning attached. This is the
    // single place channel turns are enqueued: maturana-ops' conversation front door.
    let session_id = infer_agent_session_id(home, agent_id)?;
    let message_id = maturana_ops::conversation::enqueue_turn(
        home,
        agent_id,
        &session_id,
        "console",
        "console:tui",
        maturana_ops::conversation::console_chat_key(),
        None,
        prompt,
        serde_json::json!({}),
    )?;
    let queued = QueuedAgentRun {
        session_id,
        message_id,
    };
    let completed = wait_for_agent_run(home, agent_id, &queued, timeout_seconds)?;
    Ok(channels::finalize_onboarding_reply(
        home,
        agent_id,
        &completed.text,
    ))
}

pub(crate) fn infer_agent_session_id(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<String> {
    maturana_ops::agents::infer_agent_session_id(home, agent_id)
}

fn print_live_agent_status(status: &LiveAgentStatus) {
    println!("live.provider: {}", status.provider);
    println!("live.state: {}", status.state);
    if let Some(vm_name) = &status.vm_name {
        println!("live.vm: {vm_name}");
    }
    if let Some(pid) = status.pid {
        println!("live.pid: {pid}");
    }
    if let Some(ipv4) = &status.ipv4 {
        println!("live.ipv4: {ipv4}");
    }
    if let Some(uptime) = &status.uptime {
        println!("live.uptime: {uptime}");
    }
    if let Some(path) = &status.socket_path {
        println!("live.socket: {}", path.display());
    }
    if let Some(path) = &status.config_path {
        println!("live.config: {}", path.display());
    }
    if let Some(path) = &status.metadata_path {
        println!("live.metadata: {}", path.display());
    }
    if !status.metrics_tail.is_empty() {
        println!("live.metrics_tail:");
        for line in &status.metrics_tail {
            println!("{line}");
        }
    }
}

fn print_live_guest_state(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    headless_chrome: bool,
) -> anyhow::Result<()> {
    let remote = render_live_guest_state_script(headless_chrome);
    let output = run_ssh_with_stdin(
        ip,
        ssh_user,
        ssh_key,
        host_key,
        &remote,
        None,
        SSH_TIMEOUT_QUICK,
    )?;
    print!("{output}");
    Ok(())
}

fn render_live_guest_state_script(headless_chrome: bool) -> String {
    let browser_probe = if headless_chrome {
        r#"echo "live.browser_expected: true"
echo "live.browser_smoke:"
if [ -f /opt/maturana/bin/browser-smoke.js ]; then
  PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/opt/maturana/browsers}" node /opt/maturana/bin/browser-smoke.js 2>&1 | sed 's/^/live.browser_smoke_output: /' || true
else
  echo "live.browser_smoke_output: missing /opt/maturana/bin/browser-smoke.js"
fi
"#
    } else {
        r#"echo "live.browser_expected: false"
"#
    };
    format!(
        r#"set -eu
echo "live.guest_ip: $(hostname -I 2>/dev/null | awk '{{print $1}}')"
echo "live.guest: $(hostname)"
echo "live.codex: $(command -v codex 2>/dev/null || true)"
codex --version 2>/dev/null | sed 's/^/live.codex_version: /' || true
echo "live.claude: $(command -v claude 2>/dev/null || command -v claude-code 2>/dev/null || true)"
(claude --version 2>/dev/null || claude-code --version 2>/dev/null || true) | sed 's/^/live.claude_version: /' || true
echo "live.opencode: $(command -v opencode 2>/dev/null || true)"
opencode --version 2>/dev/null | sed 's/^/live.opencode_version: /' || true
echo "live.service: $(systemctl is-active maturana-agent.service 2>/dev/null || true)"
echo "live.rootfs: $(df -h / | awk 'NR==2 {{print $2 " total, " $4 " free"}}')"
echo "live.heartbeat: $(cat /var/log/maturana/heartbeat 2>/dev/null || true)"
echo "live.last_message:"
cat /var/log/maturana/last-message.txt 2>/dev/null || true
echo "live.agent_log_tail:"
tail -n 20 /var/log/maturana/agent.log 2>/dev/null || true
{browser_probe}"#
    )
}

fn read_agent_prompt(
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    match (prompt, prompt_file) {
        (Some(prompt), None) if !prompt.trim().is_empty() => Ok(prompt),
        (None, Some(path)) => {
            let prompt = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if prompt.trim().is_empty() {
                anyhow::bail!("prompt file is empty: {}", path.display());
            }
            Ok(prompt)
        }
        (Some(_), Some(_)) => anyhow::bail!("pass either --prompt or --prompt-file, not both"),
        _ => anyhow::bail!("agent run requires --prompt or --prompt-file"),
    }
}

#[derive(Debug, Clone)]
struct QueuedAgentRun {
    session_id: String,
    message_id: String,
}

#[derive(Debug, Clone)]
struct CompletedAgentRun {
    message_id: String,
    text: String,
}

fn enqueue_agent_run(
    home: &MaturanaHome,
    agent_id: &str,
    text: &str,
) -> anyhow::Result<QueuedAgentRun> {
    // `agent run` is a chat surface too — route it through the SAME front door as
    // every channel (records the turn, injects the recent transcript for memory,
    // attaches model/reasoning), keyed `cli`/`agent-run`. So a multi-turn
    // `agent run` conversation remembers, exactly like Telegram/TUI/web.
    let session_id = infer_agent_session_id(home, agent_id)?;
    let message_id = maturana_ops::conversation::enqueue_turn(
        home,
        agent_id,
        &session_id,
        "cli",
        "agent-run",
        maturana_ops::conversation::stable_chat_key("agent-run"),
        None,
        text,
        serde_json::json!({}),
    )?;
    Ok(QueuedAgentRun {
        session_id,
        message_id,
    })
}

fn wait_for_agent_run(
    home: &MaturanaHome,
    agent_id: &str,
    queued: &QueuedAgentRun,
    timeout_seconds: u64,
) -> anyhow::Result<CompletedAgentRun> {
    let paths = session_paths(&home.agent_dir(agent_id), &queued.session_id);
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        let outbox = list_undelivered(&paths)?;
        if let Some(message) = outbox
            .into_iter()
            .find(|message| message.in_reply_to.as_deref() == Some(&queued.message_id))
        {
            let text = outbound_message_text(&message.content)?;
            mark_delivered(&paths, &message.id, Some("agent-run"))?;
            return Ok(CompletedAgentRun {
                message_id: message.id,
                text,
            });
        }
        thread::sleep(Duration::from_secs(1));
    }
    anyhow::bail!(
        "timed out waiting for response to {} in session {}",
        queued.message_id,
        queued.session_id
    )
}

fn outbound_message_text(content: &str) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(content)
        .with_context(|| format!("outbound message content was not JSON: {content}"))?;
    value
        .get("text")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("outbound message did not contain text"))
}

fn read_live_log(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    kind: LogKind,
    lines: u16,
) -> anyhow::Result<String> {
    let path = kind.guest_path();
    let command = if matches!(kind, LogKind::LastMessage) {
        format!("test -f {path} && cat {path} || true")
    } else {
        format!("test -f {path} && tail -n {} {path} || true", lines.max(1))
    };
    run_ssh_with_stdin(
        ip,
        ssh_user,
        ssh_key,
        host_key,
        &command,
        None,
        SSH_TIMEOUT_QUICK,
    )
}

impl LogKind {
    fn as_str(self) -> &'static str {
        match self {
            LogKind::Agent => "agent",
            LogKind::Error => "error",
            LogKind::Stdout => "stdout",
            LogKind::Stderr => "stderr",
            LogKind::LastMessage => "last-message",
        }
    }

    fn guest_path(self) -> &'static str {
        match self {
            LogKind::Agent => "/var/log/maturana/agent.log",
            LogKind::Error => "/var/log/maturana/agent.err.log",
            LogKind::Stdout => "/var/log/maturana/harness.out.log",
            LogKind::Stderr => "/var/log/maturana/harness.err.log",
            LogKind::LastMessage => "/var/log/maturana/last-message.txt",
        }
    }
}

pub(crate) fn push_live_path(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    local_path: &PathBuf,
    remote_path: &str,
    allowed_roots: &[String],
    recursive: bool,
) -> anyhow::Result<()> {
    if !local_path.exists() {
        anyhow::bail!("local path does not exist: {}", local_path.display());
    }
    validate_guest_transfer_path(remote_path, allowed_roots)?;

    if let Some(parent) = remote_parent(remote_path) {
        let mkdir = format!("mkdir -p {}", shell_quote(&parent));
        run_ssh_with_stdin(
            ip,
            ssh_user,
            ssh_key,
            host_key,
            &mkdir,
            None,
            SSH_TIMEOUT_QUICK,
        )?;
    }

    let mut command = ProcessCommand::new("scp");
    command
        .args(host_key.options())
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-i")
        .arg(ssh_key);
    if recursive {
        command.arg("-r");
    }
    command
        .arg(local_path)
        .arg(format!("{ssh_user}@{ip}:{remote_path}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::session_db::{claim_pending_inbound, write_outbound};

    #[test]
    fn live_guest_state_script_smokes_browser_only_when_requested() {
        let without_browser = render_live_guest_state_script(false);
        assert!(without_browser.contains("live.browser_expected: false"));
        assert!(!without_browser.contains("browser-smoke.js"));

        let with_browser = render_live_guest_state_script(true);
        assert!(with_browser.contains("live.browser_expected: true"));
        assert!(with_browser.contains("/opt/maturana/bin/browser-smoke.js"));
        assert!(with_browser.contains("PLAYWRIGHT_BROWSERS_PATH"));
        assert!(with_browser.contains("live.browser_smoke_output"));
        assert!(with_browser.contains("live.agent_log_tail"));
    }

    #[test]
    fn agent_run_uses_rendered_session_id() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-agent-run-session-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);
        let state_dir = home.agent_dir("agent").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='custom-session'\n",
        )
        .unwrap();

        assert_eq!(
            infer_agent_session_id(&home, "agent").unwrap(),
            "custom-session"
        );
        assert_eq!(
            infer_agent_session_id(&home, "opencode-firecracker").unwrap(),
            "opencode-main"
        );

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn agent_run_queues_and_waits_through_session_db() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-agent-run-queue-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        let home = MaturanaHome::new(&temp);
        let state_dir = home.agent_dir("agent").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("sessiond.env"),
            "MATURANA_SESSION_ID='cli-main'\n",
        )
        .unwrap();

        let queued = enqueue_agent_run(&home, "agent", "hello from cli").unwrap();
        assert_eq!(queued.session_id, "cli-main");
        let paths = session_paths(&home.agent_dir("agent"), "cli-main");
        let pending = claim_pending_inbound(&paths, 1).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, queued.message_id);
        assert!(pending[0].content.contains("hello from cli"));

        write_outbound(
            &paths,
            Some(&queued.message_id),
            "chat",
            "cli",
            "agent-run",
            None,
            &serde_json::json!({"text": "hello back"}).to_string(),
        )
        .unwrap();
        let completed = wait_for_agent_run(&home, "agent", &queued, 1).unwrap();
        assert_eq!(completed.text, "hello back");
        assert!(list_undelivered(&paths).unwrap().is_empty());

        let _ = fs::remove_dir_all(&temp);
    }
}
