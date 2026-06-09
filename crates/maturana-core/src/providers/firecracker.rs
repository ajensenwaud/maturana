use super::{LiveAgentStatus, Provider, ProviderCommand};
use crate::spec::AgentSpec;
use anyhow::Context;
use serde::Deserialize;
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::{
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub struct FirecrackerProvider;

#[derive(Debug, Eq, PartialEq)]
enum ExistingFirecrackerProcess {
    None,
    Stale(u32),
    StaleSocket,
    UntrackedReadySocket,
    RunningReady(u32),
    RunningMissingSocket(u32),
}

impl Provider for FirecrackerProvider {
    fn plan_launch(
        &self,
        spec: &AgentSpec,
        agent_dir: &Path,
    ) -> anyhow::Result<Vec<ProviderCommand>> {
        let state_dir = agent_dir.join("state");
        fs::create_dir_all(&state_dir)?;
        let socket = state_dir.join("firecracker.socket");
        let config_path = state_dir.join("firecracker-config.json");
        let pid_path = state_dir.join("firecracker.pid");
        let log_path = state_dir.join("firecracker.log");
        let metrics_path = state_dir.join("firecracker-metrics.json");
        let metadata_path = state_dir.join("firecracker-metadata.json");
        let firecracker = spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vm.firecracker is required for Firecracker"))?;
        let kernel_image = absolute_path(&firecracker.kernel_image)?;
        let rootfs_image = absolute_path(&firecracker.rootfs_image)?;
        let proxy_port = proxy_port(spec)?;

        let config = json!({
            "boot-source": {
                "kernel_image_path": kernel_image,
                "boot_args": firecracker.kernel_args,
            },
            "drives": [
                {
                    "drive_id": "rootfs",
                    "path_on_host": rootfs_image,
                    "is_root_device": true,
                    "is_read_only": false
                }
            ],
            "machine-config": {
                "vcpu_count": spec.vm.vcpu,
                "mem_size_mib": spec.vm.memory_mib,
                "smt": false,
                "track_dirty_pages": spec.snapshots.on_launch
            },
            "network-interfaces": [
                {
                    "iface_id": "net1",
                    "guest_mac": firecracker.guest_mac,
                    "host_dev_name": firecracker.tap_name
                }
            ],
            "logger": {
                "log_path": log_path,
                "level": "Info",
                "show_level": true,
                "show_log_origin": true
            },
            "metrics": {
                "metrics_path": metrics_path
            }
        });
        fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

        let metadata = json!({
            "agent_id": spec.identity.id,
            "runtime": format!("{:?}", spec.runtime.harness),
            "tap_name": firecracker.tap_name,
            "host_ip": firecracker.host_ip,
            "guest_ip": firecracker.guest_ip,
            "guest_mac": firecracker.guest_mac,
            "socket": socket,
            "config": config_path,
            "pid": pid_path,
            "log": log_path,
            "metrics": metrics_path,
            "proxy_port": proxy_port,
            "proxy_https": proxy_port.is_some()
        });
        fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;
        validate_firecracker_plan_files(spec, agent_dir)?;

        let mut commands = Vec::new();
        commands.push(ProviderCommand {
            program: "firecracker".to_string(),
            args: vec![
                "--api-sock".to_string(),
                socket.display().to_string(),
                "--config-file".to_string(),
                config_path.display().to_string(),
            ],
            description: "Rust provider validates Firecracker prerequisites and starts this config"
                .to_string(),
        });
        Ok(commands)
    }

    fn launch(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()> {
        if cfg!(windows) {
            anyhow::bail!("Firecracker launch requires Linux; use aidev for this provider");
        }

        let state_dir = agent_dir.join("state");
        let socket = state_dir.join("firecracker.socket");
        let config_path = state_dir.join("firecracker-config.json");
        let pid_path = state_dir.join("firecracker.pid");
        let stdout_path = state_dir.join("firecracker.stdout.log");
        let stderr_path = state_dir.join("firecracker.stderr.log");
        let firecracker = spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vm.firecracker is required for Firecracker"))?;
        validate_firecracker_plan_files(spec, agent_dir)?;
        let kernel_image = absolute_path(&firecracker.kernel_image)?;
        let rootfs_image = absolute_path(&firecracker.rootfs_image)?;
        validate_firecracker_prerequisites(&firecracker.tap_name, &kernel_image, &rootfs_image)?;
        start_firecracker_process(
            &firecracker.tap_name,
            &socket,
            &config_path,
            &pid_path,
            &stdout_path,
            &stderr_path,
        )
    }

    fn stop(&self, _spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()> {
        if cfg!(windows) {
            anyhow::bail!("Firecracker stop requires Linux; use aidev for this provider");
        }
        let state_dir = agent_dir.join("state");
        stop_firecracker_process(
            &state_dir.join("firecracker.pid"),
            &state_dir.join("firecracker.socket"),
        )?;
        Ok(())
    }

    fn inspect(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<LiveAgentStatus> {
        let state_dir = agent_dir.join("state");
        let pid_path = state_dir.join("firecracker.pid");
        let socket_path = state_dir.join("firecracker.socket");
        let config_path = state_dir.join("firecracker-config.json");
        let metadata_path = state_dir.join("firecracker-metadata.json");
        validate_firecracker_plan_files(spec, agent_dir)?;
        let metrics_path = state_dir.join("firecracker-metrics.json");
        let process_state = inspect_existing_firecracker_process(&pid_path, &socket_path)?;
        let api_ready = match process_state {
            ExistingFirecrackerProcess::RunningReady(_) => {
                Some(firecracker_api_get(&socket_path, "/").is_ok())
            }
            _ => None,
        };
        let (state, pid) = live_state_from_existing_process(process_state, api_ready);
        Ok(LiveAgentStatus {
            provider: "firecracker".to_string(),
            state,
            vm_name: None,
            pid,
            ipv4: firecracker_guest_ip(spec, &metadata_path)?,
            uptime: None,
            socket_path: Some(socket_path),
            config_path: Some(config_path),
            metadata_path: Some(metadata_path),
            metrics_tail: read_tail_lines(&metrics_path, 5)?,
        })
    }
}

fn firecracker_guest_ip(spec: &AgentSpec, metadata_path: &Path) -> anyhow::Result<Option<String>> {
    if let Some(firecracker) = spec.vm.firecracker.as_ref() {
        if !firecracker.guest_ip.trim().is_empty() {
            return Ok(Some(firecracker.guest_ip.trim().to_string()));
        }
    }
    if metadata_path.exists() {
        let raw = fs::read_to_string(metadata_path)?;
        let metadata: serde_json::Value = serde_json::from_str(&raw)?;
        if let Some(ip) = metadata
            .get("guest_ip")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(Some(ip.trim().to_string()));
        }
    }
    Ok(None)
}

fn validate_firecracker_plan_files(spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()> {
    let firecracker = spec
        .vm
        .firecracker
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("vm.firecracker is required for Firecracker"))?;
    let state_dir = agent_dir.join("state");
    let config_path = state_dir.join("firecracker-config.json");
    let metadata_path = state_dir.join("firecracker-metadata.json");
    let config = read_json_file(&config_path)?;
    let metadata = read_json_file(&metadata_path)?;

    expect_json_string(
        &config,
        &["boot-source", "kernel_image_path"],
        &absolute_path(&firecracker.kernel_image)?
            .display()
            .to_string(),
    )?;
    expect_json_string(
        &config,
        &["drives", "0", "path_on_host"],
        &absolute_path(&firecracker.rootfs_image)?
            .display()
            .to_string(),
    )?;
    expect_json_u64(
        &config,
        &["machine-config", "vcpu_count"],
        u64::from(spec.vm.vcpu),
    )?;
    expect_json_u64(
        &config,
        &["machine-config", "mem_size_mib"],
        u64::from(spec.vm.memory_mib),
    )?;
    expect_json_bool(
        &config,
        &["machine-config", "track_dirty_pages"],
        spec.snapshots.on_launch,
    )?;
    expect_json_string(
        &config,
        &["network-interfaces", "0", "guest_mac"],
        &firecracker.guest_mac,
    )?;
    expect_json_string(
        &config,
        &["network-interfaces", "0", "host_dev_name"],
        &firecracker.tap_name,
    )?;

    expect_json_string(&metadata, &["agent_id"], &spec.identity.id)?;
    expect_json_string(
        &metadata,
        &["runtime"],
        &format!("{:?}", spec.runtime.harness),
    )?;
    expect_json_string(&metadata, &["tap_name"], &firecracker.tap_name)?;
    expect_json_string(&metadata, &["host_ip"], &firecracker.host_ip)?;
    expect_json_string(&metadata, &["guest_ip"], &firecracker.guest_ip)?;
    expect_json_string(&metadata, &["guest_mac"], &firecracker.guest_mac)?;
    expect_json_bool(
        &metadata,
        &["proxy_https"],
        spec.network
            .proxy
            .as_ref()
            .is_some_and(|proxy| proxy.enabled),
    )?;
    match proxy_port(spec)? {
        Some(port) => expect_json_u64(&metadata, &["proxy_port"], u64::from(port))?,
        None => expect_json_null(&metadata, &["proxy_port"])?,
    }

    validate_state_path(&state_dir, &metadata, "socket", "firecracker.socket")?;
    validate_state_path(&state_dir, &metadata, "config", "firecracker-config.json")?;
    validate_state_path(&state_dir, &metadata, "pid", "firecracker.pid")?;
    validate_state_path(&state_dir, &metadata, "log", "firecracker.log")?;
    validate_state_path(&state_dir, &metadata, "metrics", "firecracker-metrics.json")?;
    Ok(())
}

fn read_json_file(path: &Path) -> anyhow::Result<serde_json::Value> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn expect_json_string(
    value: &serde_json::Value,
    path: &[&str],
    expected: &str,
) -> anyhow::Result<()> {
    let actual = json_path(value, path)
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("Firecracker plan missing string {}", path.join(".")))?;
    if actual != expected {
        anyhow::bail!(
            "Firecracker plan {} mismatch: expected {}, got {}",
            path.join("."),
            expected,
            actual
        );
    }
    Ok(())
}

fn expect_json_u64(value: &serde_json::Value, path: &[&str], expected: u64) -> anyhow::Result<()> {
    let actual = json_path(value, path)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| anyhow::anyhow!("Firecracker plan missing integer {}", path.join(".")))?;
    if actual != expected {
        anyhow::bail!(
            "Firecracker plan {} mismatch: expected {}, got {}",
            path.join("."),
            expected,
            actual
        );
    }
    Ok(())
}

fn expect_json_bool(
    value: &serde_json::Value,
    path: &[&str],
    expected: bool,
) -> anyhow::Result<()> {
    let actual = json_path(value, path)
        .and_then(|value| value.as_bool())
        .ok_or_else(|| anyhow::anyhow!("Firecracker plan missing boolean {}", path.join(".")))?;
    if actual != expected {
        anyhow::bail!(
            "Firecracker plan {} mismatch: expected {}, got {}",
            path.join("."),
            expected,
            actual
        );
    }
    Ok(())
}

fn expect_json_null(value: &serde_json::Value, path: &[&str]) -> anyhow::Result<()> {
    let actual = json_path(value, path)
        .ok_or_else(|| anyhow::anyhow!("Firecracker plan missing null {}", path.join(".")))?;
    if !actual.is_null() {
        anyhow::bail!(
            "Firecracker plan {} mismatch: expected null, got {}",
            path.join("."),
            actual
        );
    }
    Ok(())
}

fn json_path<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path {
        if let Ok(index) = segment.parse::<usize>() {
            current = current.as_array()?.get(index)?;
        } else {
            current = current.get(*segment)?;
        }
    }
    Some(current)
}

fn validate_state_path(
    state_dir: &Path,
    metadata: &serde_json::Value,
    key: &str,
    expected_name: &str,
) -> anyhow::Result<()> {
    let raw = metadata
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("Firecracker metadata missing {key} path"))?;
    let path = PathBuf::from(raw);
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name) {
        anyhow::bail!(
            "Firecracker metadata {key} path must end with {expected_name}: {}",
            path.display()
        );
    }
    let expected_parent = state_dir
        .canonicalize()
        .unwrap_or_else(|_| absolute_without_filesystem(state_dir));
    let actual_parent = path
        .parent()
        .map(|parent| {
            parent
                .canonicalize()
                .unwrap_or_else(|_| absolute_without_filesystem(parent))
        })
        .ok_or_else(|| anyhow::anyhow!("Firecracker metadata {key} path has no parent"))?;
    if actual_parent != expected_parent {
        anyhow::bail!(
            "Firecracker metadata {key} path escapes agent state directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn absolute_without_filesystem(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn start_firecracker_process(
    tap_name: &str,
    socket_path: &Path,
    config_path: &Path,
    pid_path: &Path,
    stdout_path: &Path,
    stderr_path: &Path,
) -> anyhow::Result<()> {
    if !config_path.exists() {
        anyhow::bail!("Firecracker config not found: {}", config_path.display());
    }
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match inspect_existing_firecracker_process(pid_path, socket_path)? {
        ExistingFirecrackerProcess::RunningReady(pid) => {
            wait_firecracker_api_ready(socket_path).with_context(|| {
                format!("Firecracker pid {pid} is running but API is not ready")
            })?;
            println!("Firecracker already running with pid {pid}");
            return Ok(());
        }
        ExistingFirecrackerProcess::RunningMissingSocket(pid) => {
            anyhow::bail!(
                "Firecracker pid {pid} is running, but API socket is missing at {}; stop or repair the agent before relaunching",
                socket_path.display()
            );
        }
        ExistingFirecrackerProcess::UntrackedReadySocket => {
            anyhow::bail!(
                "Firecracker API socket responds at {} but no pid file exists at {}; refusing to remove a live untracked socket",
                socket_path.display(),
                pid_path.display()
            );
        }
        ExistingFirecrackerProcess::Stale(_) => {
            let _ = fs::remove_file(pid_path);
        }
        ExistingFirecrackerProcess::StaleSocket => {}
        ExistingFirecrackerProcess::None => {}
    }
    remove_socket_if_present(socket_path)?;
    ensure_tap_exists(tap_name)?;
    let stdout = fs::File::create(stdout_path)?;
    let stderr = fs::File::create(stderr_path)?;
    let mut child = Command::new("firecracker")
        .arg("--api-sock")
        .arg(socket_path)
        .arg("--config-file")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("failed to start firecracker")?;
    fs::write(pid_path, child.id().to_string())?;
    if let Err(error) = wait_firecracker_api_ready(socket_path) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(pid_path);
        let _ = fs::remove_file(socket_path);
        let stderr_tail = read_tail_lines(stderr_path, 20).unwrap_or_default();
        anyhow::bail!(
            "Firecracker launched but API did not become ready: {error:#}\nstderr tail:\n{}",
            stderr_tail.join("\n")
        );
    }
    println!("Firecracker launched");
    println!("pid: {}", child.id());
    println!("socket: {}", socket_path.display());
    Ok(())
}

fn validate_firecracker_prerequisites(
    tap_name: &str,
    kernel_image: &Path,
    rootfs_image: &Path,
) -> anyhow::Result<()> {
    if cfg!(windows) {
        anyhow::bail!("Firecracker requires a Linux host");
    }
    ensure_command_exists("firecracker")?;
    ensure_command_exists("curl")?;
    ensure_command_exists("ip")?;
    ensure_kvm_ready()?;
    ensure_regular_file(kernel_image, "kernel image")?;
    ensure_elf_file(kernel_image)?;
    ensure_regular_file(rootfs_image, "rootfs image")?;
    ensure_tap_exists(tap_name)?;
    Ok(())
}

fn ensure_kvm_ready() -> anyhow::Result<()> {
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        anyhow::bail!("/dev/kvm does not exist");
    }
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path).context("failed to inspect /dev/kvm")?;
        if !metadata.file_type().is_char_device() {
            anyhow::bail!("/dev/kvm is not a character device");
        }
    }
    let readable = fs::OpenOptions::new().read(true).open(path).is_ok();
    let writable = fs::OpenOptions::new().write(true).open(path).is_ok();
    if !readable || !writable {
        anyhow::bail!("/dev/kvm must be readable and writable by the current user");
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("{label} not found: {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("{label} is not a file: {}", path.display());
    }
    if metadata.len() == 0 {
        anyhow::bail!("{label} is empty: {}", path.display());
    }
    Ok(())
}

fn ensure_elf_file(path: &Path) -> anyhow::Result<()> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open kernel image {}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("failed to read kernel image {}", path.display()))?;
    if magic != [0x7f, b'E', b'L', b'F'] {
        anyhow::bail!(
            "kernel image is not an ELF vmlinux file: {}",
            path.display()
        );
    }
    Ok(())
}

fn stop_firecracker_process(pid_path: &Path, socket_path: &Path) -> anyhow::Result<()> {
    match inspect_existing_firecracker_process(pid_path, socket_path)? {
        ExistingFirecrackerProcess::RunningReady(pid)
        | ExistingFirecrackerProcess::RunningMissingSocket(pid) => {
            send_signal(pid, None)
                .with_context(|| format!("failed to send TERM to Firecracker pid {pid}"))?;
            if wait_for_process_exit(pid, Duration::from_secs(10))?.is_err() {
                send_signal(pid, Some("KILL"))
                    .with_context(|| format!("failed to send KILL to Firecracker pid {pid}"))?;
                wait_for_process_exit(pid, Duration::from_secs(5))?
                    .with_context(|| format!("Firecracker pid {pid} did not exit after KILL"))?;
            }
            let _ = fs::remove_file(pid_path);
            remove_socket_if_present(socket_path)?;
        }
        ExistingFirecrackerProcess::Stale(_) => {
            let _ = fs::remove_file(pid_path);
            remove_socket_if_present(socket_path)?;
        }
        ExistingFirecrackerProcess::StaleSocket => {
            remove_socket_if_present(socket_path)?;
        }
        ExistingFirecrackerProcess::UntrackedReadySocket => {
            anyhow::bail!(
                "Firecracker API socket responds at {} but no pid file exists at {}; refusing to remove a live untracked socket",
                socket_path.display(),
                pid_path.display()
            );
        }
        ExistingFirecrackerProcess::None => {}
    }
    println!("Firecracker stopped");
    Ok(())
}

fn read_pid(path: &Path) -> anyhow::Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(raw) => parse_firecracker_pid(&raw)
            .with_context(|| format!("invalid pid in {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn parse_firecracker_pid(raw: &str) -> anyhow::Result<Option<u32>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let pid = trimmed.parse::<u32>()?;
    if pid == 0 {
        anyhow::bail!("pid must be greater than zero");
    }
    Ok(Some(pid))
}

fn process_running(pid: u32) -> anyhow::Result<bool> {
    Ok(Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to check process status")?
        .success())
}

fn send_signal(pid: u32, signal: Option<&str>) -> anyhow::Result<()> {
    let mut command = Command::new("kill");
    if let Some(signal) = signal {
        command.arg(format!("-{signal}"));
    }
    let status = command
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .status()
        .context("failed to execute kill")?;
    if !status.success() {
        anyhow::bail!("kill returned status {status}");
    }
    Ok(())
}

fn inspect_existing_firecracker_process(
    pid_path: &Path,
    socket_path: &Path,
) -> anyhow::Result<ExistingFirecrackerProcess> {
    if let Some(pid) = read_pid(pid_path)? {
        return Ok(classify_existing_firecracker_process(
            Some(pid),
            Some(process_running(pid)?),
            socket_path.exists(),
            None,
        ));
    }
    let socket_exists = socket_path.exists();
    let api_ready = if socket_exists {
        Some(firecracker_api_get(socket_path, "/").is_ok())
    } else {
        None
    };
    Ok(classify_existing_firecracker_process(
        None,
        None,
        socket_exists,
        api_ready,
    ))
}

fn classify_existing_firecracker_process(
    pid: Option<u32>,
    running: Option<bool>,
    socket_exists: bool,
    api_ready: Option<bool>,
) -> ExistingFirecrackerProcess {
    match (pid, running, socket_exists, api_ready) {
        (Some(pid), Some(true), true, _) => ExistingFirecrackerProcess::RunningReady(pid),
        (Some(pid), Some(true), false, _) => ExistingFirecrackerProcess::RunningMissingSocket(pid),
        (Some(pid), Some(false), _, _) => ExistingFirecrackerProcess::Stale(pid),
        (None, _, true, Some(true)) => ExistingFirecrackerProcess::UntrackedReadySocket,
        (None, _, true, _) => ExistingFirecrackerProcess::StaleSocket,
        (None, _, false, _) => ExistingFirecrackerProcess::None,
        (Some(pid), None, _, _) => ExistingFirecrackerProcess::Stale(pid),
    }
}

fn live_state_from_existing_process(
    process: ExistingFirecrackerProcess,
    api_ready: Option<bool>,
) -> (String, Option<u32>) {
    match process {
        ExistingFirecrackerProcess::None => ("stopped".to_string(), None),
        ExistingFirecrackerProcess::Stale(pid) => ("stale-pid".to_string(), Some(pid)),
        ExistingFirecrackerProcess::StaleSocket => ("stale-socket".to_string(), None),
        ExistingFirecrackerProcess::UntrackedReadySocket => {
            ("untracked-api-socket".to_string(), None)
        }
        ExistingFirecrackerProcess::RunningReady(pid) => match api_ready {
            Some(false) => ("running-api-unresponsive".to_string(), Some(pid)),
            _ => ("running".to_string(), Some(pid)),
        },
        ExistingFirecrackerProcess::RunningMissingSocket(pid) => {
            ("running-missing-socket".to_string(), Some(pid))
        }
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> anyhow::Result<anyhow::Result<()>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_running(pid)? {
            return Ok(Ok(()));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(Err(anyhow::anyhow!(
        "Firecracker pid {pid} did not exit within {}s",
        timeout.as_secs()
    )))
}

fn ensure_command_exists(program: &str) -> anyhow::Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", shell_quote(program)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to check command availability")?;
    if !status.success() {
        anyhow::bail!("{program} binary not found on PATH");
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn ensure_tap_exists(tap_name: &str) -> anyhow::Result<()> {
    let output = Command::new("ip")
        .arg("-j")
        .arg("link")
        .arg("show")
        .arg("dev")
        .arg(tap_name)
        .stdin(Stdio::null())
        .output()
        .context("failed to inspect tap device")?;
    if !output.status.success() {
        anyhow::bail!(
            "tap device not found or unreadable: {tap_name}; create it with scripts/firecracker-setup-tap.sh or equivalent host setup: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    validate_tap_ip_link_json(tap_name, &String::from_utf8_lossy(&output.stdout))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct IpLinkInfo {
    ifname: String,
    #[serde(default)]
    flags: Vec<String>,
    operstate: Option<String>,
}

fn validate_tap_ip_link_json(tap_name: &str, raw: &str) -> anyhow::Result<()> {
    let links: Vec<IpLinkInfo> = serde_json::from_str(raw)
        .with_context(|| format!("failed to parse `ip -j link show dev {tap_name}` output"))?;
    let Some(link) = links.iter().find(|link| link.ifname == tap_name) else {
        anyhow::bail!("tap device not found in ip output: {tap_name}");
    };
    if !link.flags.iter().any(|flag| flag == "UP") {
        anyhow::bail!(
            "tap device {tap_name} is not administratively UP (operstate={}, flags={})",
            link.operstate.as_deref().unwrap_or("unknown"),
            link.flags.join(",")
        );
    }
    Ok(())
}

fn remove_socket_if_present(socket_path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", socket_path.display()))
        }
    }
}

fn firecracker_api_get(socket: &Path, path: &str) -> anyhow::Result<()> {
    let output = Command::new("curl")
        .arg("--fail-with-body")
        .arg("--silent")
        .arg("--show-error")
        .arg("--connect-timeout")
        .arg("1")
        .arg("--max-time")
        .arg("2")
        .arg("--unix-socket")
        .arg(socket)
        .arg(format!("http://localhost{path}"))
        .stdin(Stdio::null())
        .output()
        .context("failed to execute curl for Firecracker API")?;
    if !output.status.success() {
        anyhow::bail!(
            "Firecracker API GET {path} failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

fn wait_firecracker_api_ready(socket: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    while Instant::now() < deadline {
        match firecracker_api_get(socket, "/") {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(error) = last_error {
        Err(error).context(format!(
            "timed out waiting for Firecracker API at {}",
            socket.display()
        ))
    } else {
        anyhow::bail!(
            "timed out waiting for Firecracker API at {}",
            socket.display()
        )
    }
}

fn read_tail_lines(path: &Path, limit: usize) -> anyhow::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let lines = raw.lines().map(ToString::to_string).collect::<Vec<_>>();
            let start = lines.len().saturating_sub(limit);
            Ok(lines[start..].to_vec())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn absolute_path(path: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

fn proxy_port(spec: &AgentSpec) -> anyhow::Result<Option<u16>> {
    let Some(proxy) = &spec.network.proxy else {
        return Ok(None);
    };
    if !proxy.enabled {
        return Ok(None);
    }
    Ok(Some(parse_bind_port(&proxy.bind)?))
}

fn parse_bind_port(bind: &str) -> anyhow::Result<u16> {
    let port = bind
        .trim()
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or(bind.trim());
    port.parse()
        .with_context(|| format!("network.proxy.bind must include a TCP port: {bind}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider;
    use crate::spec::{
        AgentRun, Browser, Channels, Filesystem, FirecrackerVm, HarnessRuntime, HostProvider,
        Identity, Memory, Network, NetworkProxy, Runtime, SnapshotPolicy, Vm,
    };

    #[test]
    fn classifies_existing_firecracker_state_without_deleting_live_socket() {
        assert_eq!(
            classify_existing_firecracker_process(Some(42), Some(true), true, None),
            ExistingFirecrackerProcess::RunningReady(42)
        );
        assert_eq!(
            classify_existing_firecracker_process(Some(42), Some(true), false, None),
            ExistingFirecrackerProcess::RunningMissingSocket(42)
        );
        assert_eq!(
            classify_existing_firecracker_process(Some(42), Some(false), true, None),
            ExistingFirecrackerProcess::Stale(42)
        );
        assert_eq!(
            classify_existing_firecracker_process(Some(42), Some(false), false, None),
            ExistingFirecrackerProcess::Stale(42)
        );
        assert_eq!(
            classify_existing_firecracker_process(None, None, true, Some(true)),
            ExistingFirecrackerProcess::UntrackedReadySocket
        );
        assert_eq!(
            classify_existing_firecracker_process(None, None, true, Some(false)),
            ExistingFirecrackerProcess::StaleSocket
        );
        assert_eq!(
            classify_existing_firecracker_process(None, None, false, None),
            ExistingFirecrackerProcess::None
        );
    }

    #[test]
    fn maps_firecracker_process_state_to_operator_status() {
        assert_eq!(
            live_state_from_existing_process(ExistingFirecrackerProcess::None, None),
            ("stopped".to_string(), None)
        );
        assert_eq!(
            live_state_from_existing_process(
                ExistingFirecrackerProcess::RunningReady(42),
                Some(true)
            ),
            ("running".to_string(), Some(42))
        );
        assert_eq!(
            live_state_from_existing_process(
                ExistingFirecrackerProcess::RunningReady(42),
                Some(false)
            ),
            ("running-api-unresponsive".to_string(), Some(42))
        );
        assert_eq!(
            live_state_from_existing_process(ExistingFirecrackerProcess::RunningReady(42), None),
            ("running".to_string(), Some(42))
        );
        assert_eq!(
            live_state_from_existing_process(
                ExistingFirecrackerProcess::RunningMissingSocket(42),
                None
            ),
            ("running-missing-socket".to_string(), Some(42))
        );
        assert_eq!(
            live_state_from_existing_process(ExistingFirecrackerProcess::Stale(42), None),
            ("stale-pid".to_string(), Some(42))
        );
        assert_eq!(
            live_state_from_existing_process(ExistingFirecrackerProcess::StaleSocket, None),
            ("stale-socket".to_string(), None)
        );
        assert_eq!(
            live_state_from_existing_process(
                ExistingFirecrackerProcess::UntrackedReadySocket,
                None
            ),
            ("untracked-api-socket".to_string(), None)
        );
    }

    #[test]
    fn provider_pid_parser_rejects_invalid_or_zero_pid() {
        assert_eq!(parse_firecracker_pid("").unwrap(), None);
        assert_eq!(parse_firecracker_pid(" 123 \n").unwrap(), Some(123));
        assert!(parse_firecracker_pid("0").is_err());
        assert!(parse_firecracker_pid("not-a-pid").is_err());
    }

    #[test]
    fn validates_tap_ip_link_json_requires_up_flag() {
        let up = r#"[{"ifname":"tap-mat-codex","operstate":"UNKNOWN","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"]}]"#;
        validate_tap_ip_link_json("tap-mat-codex", up).unwrap();

        let down =
            r#"[{"ifname":"tap-mat-codex","operstate":"DOWN","flags":["BROADCAST","MULTICAST"]}]"#;
        let error = validate_tap_ip_link_json("tap-mat-codex", down)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not administratively UP"));

        let missing = r#"[{"ifname":"other0","flags":["UP"]}]"#;
        let error = validate_tap_ip_link_json("tap-mat-codex", missing)
            .unwrap_err()
            .to_string();
        assert!(error.contains("tap device not found"));
    }

    #[test]
    fn firecracker_plan_validation_accepts_rendered_files() {
        let agent_dir = temp_agent_dir("firecracker-plan-ok");
        let spec = test_firecracker_spec(&agent_dir);
        FirecrackerProvider.plan_launch(&spec, &agent_dir).unwrap();

        validate_firecracker_plan_files(&spec, &agent_dir).unwrap();
        let _ = fs::remove_dir_all(agent_dir);
    }

    #[test]
    fn firecracker_plan_validation_rejects_stale_config() {
        let agent_dir = temp_agent_dir("firecracker-plan-stale-config");
        let spec = test_firecracker_spec(&agent_dir);
        FirecrackerProvider.plan_launch(&spec, &agent_dir).unwrap();
        let config_path = agent_dir.join("state/firecracker-config.json");
        let mut config = read_json_file(&config_path).unwrap();
        config["network-interfaces"][0]["host_dev_name"] = json!("tap-other");
        fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let error = validate_firecracker_plan_files(&spec, &agent_dir)
            .unwrap_err()
            .to_string();
        assert!(error.contains("network-interfaces.0.host_dev_name mismatch"));
        let _ = fs::remove_dir_all(agent_dir);
    }

    #[test]
    fn firecracker_plan_validation_rejects_metadata_escape() {
        let agent_dir = temp_agent_dir("firecracker-plan-metadata-escape");
        let spec = test_firecracker_spec(&agent_dir);
        FirecrackerProvider.plan_launch(&spec, &agent_dir).unwrap();
        let metadata_path = agent_dir.join("state/firecracker-metadata.json");
        let mut metadata = read_json_file(&metadata_path).unwrap();
        metadata["socket"] = json!(agent_dir.join("outside/firecracker.socket"));
        fs::write(
            &metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let error = validate_firecracker_plan_files(&spec, &agent_dir)
            .unwrap_err()
            .to_string();
        assert!(error.contains("socket path escapes agent state directory"));
        let _ = fs::remove_dir_all(agent_dir);
    }

    fn temp_agent_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_firecracker_spec(agent_dir: &Path) -> AgentSpec {
        let image_dir = agent_dir.join("images");
        fs::create_dir_all(&image_dir).unwrap();
        AgentSpec {
            identity: Identity {
                id: "codex-firecracker".to_string(),
                name: "Codex Firecracker".to_string(),
                purpose: "test".to_string(),
            },
            runtime: Runtime {
                harness: HarnessRuntime::Codex,
            },
            vm: Vm {
                provider: HostProvider::Firecracker,
                guest_os: Default::default(),
                vcpu: 2,
                memory_mib: 1024,
                boot_image: None,
                switch_name: None,
                cloud_init: None,
                firecracker: Some(FirecrackerVm {
                    kernel_image: image_dir.join("vmlinux").display().to_string(),
                    rootfs_image: image_dir.join("rootfs.ext4").display().to_string(),
                    tap_name: "tap-mat-codex".to_string(),
                    host_ip: "172.30.0.1".to_string(),
                    guest_ip: "172.30.0.2".to_string(),
                    guest_mac: "AA:FC:00:00:00:01".to_string(),
                    kernel_args: "console=ttyS0 root=/dev/vda rw".to_string(),
                }),
            },
            filesystem: Filesystem::default(),
            network: Network {
                egress_allowlist: vec![],
                proxy: Some(NetworkProxy {
                    enabled: true,
                    bind: "172.30.0.1:47833".to_string(),
                    inject_headers: vec![],
                }),
            },
            credentials: vec![],
            harness_auth: vec![],
            agent_run: AgentRun::default(),
            memory: Memory::default(),
            browser: Browser::default(),
            skills: vec![],
            tools: vec![],
            schedules: vec![],
            channels: Channels::default(),
            snapshots: SnapshotPolicy {
                on_launch: true,
                retain: 5,
            },
        }
    }
}
