use anyhow::Context;
use maturana_core::{inspect_agent, spec::HarnessRuntime, state::MaturanaHome};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    guest_worker::{install_guest_worker, GuestWorkerInstall},
    runtime_plane::ensure_sessiond_token,
};

#[derive(Debug, Clone)]
pub struct RepairWindowsHarnessConfig {
    pub agent_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub harnesses: Vec<String>,
    pub harness_auth_guest_paths: Vec<String>,
    pub telegram_token_sources: Vec<String>,
}

pub fn repair_windows_config(
    agent_ids: Vec<String>,
    session_ids: Vec<String>,
    harnesses: Vec<String>,
    harness_auth_guest_paths: Vec<String>,
    telegram_token_sources: Vec<String>,
) -> anyhow::Result<RepairWindowsHarnessConfig> {
    let agent_ids = default_if_empty(agent_ids, &["codex-demo", "opencode-demo", "claude-demo"]);
    let session_ids =
        default_if_empty(session_ids, &["codex-main", "opencode-main", "claude-main"]);
    let harnesses = default_if_empty(harnesses, &["codex", "opencode", "claude-code"]);
    let harness_auth_guest_paths = default_if_empty(
        harness_auth_guest_paths,
        &[
            "/home/ubuntu/.codex",
            "/home/ubuntu",
            "/home/ubuntu/.claude",
        ],
    );
    let telegram_token_sources = default_if_empty(
        telegram_token_sources,
        &[
            "pipelock:telegram/bot-token",
            "pipelock:telegram/opencode-bot-token",
            "pipelock:telegram/claude-bot-token",
        ],
    );

    let expected = agent_ids.len();
    for (name, values) in [
        ("session-id", &session_ids),
        ("harness", &harnesses),
        ("harness-auth-guest-path", &harness_auth_guest_paths),
        ("telegram-token-source", &telegram_token_sources),
    ] {
        if values.len() != expected {
            anyhow::bail!(
                "--{name} count ({}) must match --agent-id count ({expected})",
                values.len()
            );
        }
    }
    for harness in &harnesses {
        if !matches!(harness.as_str(), "codex" | "opencode" | "claude-code") {
            anyhow::bail!("unsupported harness for repair: {harness}");
        }
    }

    Ok(RepairWindowsHarnessConfig {
        agent_ids,
        session_ids,
        harnesses,
        harness_auth_guest_paths,
        telegram_token_sources,
    })
}

pub fn repair_windows_harnesses(
    home: &MaturanaHome,
    config: &RepairWindowsHarnessConfig,
    register_tasks: bool,
    skip_guest_worker_refresh: bool,
) -> anyhow::Result<()> {
    if !cfg!(windows) {
        anyhow::bail!("windows harness repair requires a Windows host");
    }

    stop_windows_harness_processes(home)?;

    let repo_root = std::env::current_dir()?;
    start_windows_sessiond(home, &repo_root, register_tasks)?;

    for index in 0..config.agent_ids.len() {
        let agent_id = &config.agent_ids[index];
        let session_id = &config.session_ids[index];
        let harness = &config.harnesses[index];
        let auth_path = &config.harness_auth_guest_paths[index];
        let token_source = &config.telegram_token_sources[index];

        if !skip_guest_worker_refresh {
            refresh_live_guest_worker(home, agent_id, session_id, harness, auth_path)?;
        }

        start_windows_telegram_channel(
            home,
            &repo_root,
            agent_id,
            session_id,
            token_source,
            register_tasks,
        )?;
    }

    Ok(())
}

fn default_if_empty(values: Vec<String>, defaults: &[&str]) -> Vec<String> {
    if values.is_empty() {
        defaults.iter().map(|value| (*value).to_string()).collect()
    } else {
        values
    }
}

fn start_windows_sessiond(
    home: &MaturanaHome,
    repo_root: &Path,
    register_task: bool,
) -> anyhow::Result<()> {
    let sessiond_token_path = home.root().join("sessiond/token");
    let token = ensure_sessiond_token(&sessiond_token_path)?;
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let pid_path = home.root().join("sessiond/runner.pid");
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let args = vec![
        "session".to_string(),
        "serve".to_string(),
        "--bind".to_string(),
        "0.0.0.0:47834".to_string(),
        "--token".to_string(),
        token,
    ];
    let exe = windows_maturana_exe(repo_root)?;
    if register_task {
        register_windows_task(
            "MaturanaSessiond",
            &exe,
            &args,
            repo_root,
            &logs_dir.join("sessiond.out.log"),
            &logs_dir.join("sessiond.err.log"),
        )?;
    }
    start_windows_runner(
        &exe,
        &args,
        repo_root,
        &logs_dir.join("sessiond.out.log"),
        &logs_dir.join("sessiond.err.log"),
        &pid_path,
        "sessiond",
    )
}

fn start_windows_telegram_channel(
    home: &MaturanaHome,
    repo_root: &Path,
    agent_id: &str,
    session_id: &str,
    token_source: &str,
    register_task: bool,
) -> anyhow::Result<()> {
    let safe_agent_id = safe_windows_task_suffix(agent_id);
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let pid_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/runner.pid");
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let args = vec![
        "channel".to_string(),
        "serve".to_string(),
        "telegram".to_string(),
        "--agent-id".to_string(),
        agent_id.to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
        "--token-source".to_string(),
        token_source.to_string(),
    ];
    let exe = windows_maturana_exe(repo_root)?;
    if register_task {
        register_windows_task(
            &format!("MaturanaTelegramChannel-{safe_agent_id}"),
            &exe,
            &args,
            repo_root,
            &logs_dir.join(format!("telegram-channel-{safe_agent_id}.out.log")),
            &logs_dir.join(format!("telegram-channel-{safe_agent_id}.err.log")),
        )?;
    }
    start_windows_runner(
        &exe,
        &args,
        repo_root,
        &logs_dir.join(format!("telegram-channel-{safe_agent_id}.out.log")),
        &logs_dir.join(format!("telegram-channel-{safe_agent_id}.err.log")),
        &pid_path,
        &format!("telegram channel {agent_id}"),
    )
}

fn refresh_live_guest_worker(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    harness: &str,
    harness_auth_guest_path: &str,
) -> anyhow::Result<()> {
    let harness = parse_harness_runtime(harness)?;
    let status = inspect_agent(home, agent_id)?;
    let ip = status.ipv4.ok_or_else(|| {
        anyhow::anyhow!("could not discover live IP for {agent_id}; run live inspect first")
    })?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            harness,
            guest_ip: ip,
            ssh_user: "ubuntu".to_string(),
            ssh_key: default_agent_ssh_key()?,
            harness_auth_guest_path: harness_auth_guest_path.to_string(),
            sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
            sessiond_token_path: home.root().join("sessiond/token"),
            auth_source: None,
            install_harness: false,
            // Moot (auth_source is None): the live refresh never re-seeds auth.
            force_reseed_auth: false,
        },
    )
}

fn stop_windows_harness_processes(home: &MaturanaHome) -> anyhow::Result<()> {
    stop_windows_pid_file(&home.root().join("sessiond/runner.pid"), "sessiond")?;
    let agents_dir = home.agents_dir();
    if agents_dir.exists() {
        for entry in fs::read_dir(agents_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stop_windows_pid_file(
                    &entry.path().join("channels/telegram/runner.pid"),
                    &format!("telegram channel {}", entry.file_name().to_string_lossy()),
                )?;
            }
        }
    }
    Ok(())
}

fn stop_windows_pid_file(path: &Path, label: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let pid = fs::read_to_string(path)?.trim().to_string();
    if pid.is_empty() {
        return Ok(());
    }
    let status = Command::new("taskkill")
        .arg("/PID")
        .arg(&pid)
        .arg("/F")
        .status()
        .with_context(|| format!("failed to stop {label} pid {pid}"))?;
    if !status.success() {
        eprintln!("warning: failed to stop {label} pid {pid}: {status}");
    } else {
        println!("stopped {label} pid={pid}");
    }
    let _ = fs::remove_file(path);
    Ok(())
}

fn start_windows_runner(
    exe: &Path,
    args: &[String],
    repo_root: &Path,
    log_path: &Path,
    err_path: &Path,
    pid_path: &Path,
    label: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = err_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = fs::File::create(log_path)?;
    let stderr = fs::File::create(err_path)?;
    let child = Command::new(exe)
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("failed to start {label}"))?;
    fs::write(pid_path, child.id().to_string())?;
    println!("started {label} pid={}", child.id());
    Ok(())
}

fn register_windows_task(
    task_name: &str,
    exe: &Path,
    args: &[String],
    repo_root: &Path,
    log_path: &Path,
    err_path: &Path,
) -> anyhow::Result<()> {
    let mut command = format!(
        "cmd.exe /c \"cd /d {} && {} {} >> {} 2>> {}\"",
        quote_cmd_arg(repo_root),
        quote_cmd_arg(exe),
        args.iter()
            .map(|arg| quote_cmd_arg(arg))
            .collect::<Vec<_>>()
            .join(" "),
        quote_cmd_arg(log_path),
        quote_cmd_arg(err_path)
    );
    if command.len() > 260 {
        command = format!(
            "{} {}",
            quote_cmd_arg(exe),
            args.iter()
                .map(|arg| quote_cmd_arg(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let status = Command::new("schtasks")
        .arg("/Create")
        .arg("/TN")
        .arg(task_name)
        .arg("/SC")
        .arg("ONLOGON")
        .arg("/TR")
        .arg(command)
        .arg("/F")
        .status()
        .with_context(|| format!("failed to register Windows task {task_name}"))?;
    if !status.success() {
        eprintln!("warning: could not register Windows task {task_name}: {status}");
    }
    Ok(())
}

fn windows_maturana_exe(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    if current.exists() {
        return Ok(current);
    }
    for path in [
        repo_root.join("target/x86_64-pc-windows-msvc/debug/maturana.exe"),
        repo_root.join("target/x86_64-pc-windows-gnu/debug/maturana.exe"),
        repo_root.join("target/debug/maturana.exe"),
    ] {
        if path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("maturana.exe not found; build the Windows binary first")
}

pub fn safe_windows_task_suffix(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

pub fn quote_cmd_arg(value: impl AsRef<Path>) -> String {
    let raw = value.as_ref().display().to_string();
    format!("\"{}\"", raw.replace('"', "\\\""))
}

fn parse_harness_runtime(harness: &str) -> anyhow::Result<HarnessRuntime> {
    match harness {
        "codex" => Ok(HarnessRuntime::Codex),
        "claude-code" => Ok(HarnessRuntime::ClaudeCode),
        "opencode" => Ok(HarnessRuntime::Opencode),
        _ => anyhow::bail!("unsupported harness: {harness}"),
    }
}

fn default_agent_ssh_key() -> anyhow::Result<PathBuf> {
    std::env::var("MATURANA_AGENT_SSH_KEY")
        .map(PathBuf::from)
        .or_else(|_| {
            Ok::<PathBuf, std::env::VarError>(PathBuf::from(
                ".maturana/keys/maturana-agent-ed25519",
            ))
        })
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            }
        })
        .map_err(Into::into)
}
