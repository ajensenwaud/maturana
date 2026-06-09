use crate::{
    spec::{AgentSpec, HostProvider},
    state::MaturanaHome,
};
use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::Component,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub name: String,
    pub provider: HostProvider,
    pub kind: SnapshotKind,
    pub created_at: String,
    pub state_path: Option<PathBuf>,
    pub memory_path: Option<PathBuf>,
    pub disk_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotKind {
    LocalMarker,
    HyperVCheckpoint,
    FirecrackerFull,
}

pub fn list_snapshots(
    home: &MaturanaHome,
    agent_id: &str,
    live: bool,
) -> anyhow::Result<Vec<SnapshotRecord>> {
    let agent_dir = home.agent_dir(agent_id);
    let spec = load_agent_spec(&agent_dir)?;
    if live && spec.vm.provider == HostProvider::HyperV {
        return list_hyperv_snapshots(agent_id);
    }
    list_local_snapshot_records(&agent_dir, spec.vm.provider)
}

pub fn take_snapshot(
    home: &MaturanaHome,
    agent_id: &str,
    name: &str,
    live: bool,
) -> anyhow::Result<SnapshotRecord> {
    let agent_dir = home.agent_dir(agent_id);
    let spec = load_agent_spec(&agent_dir)?;
    match (spec.vm.provider, live) {
        (HostProvider::HyperV, true) => take_hyperv_snapshot(&agent_dir, agent_id, name),
        (HostProvider::Firecracker, true) => take_firecracker_snapshot(&agent_dir, name),
        (provider, false) => create_local_marker(&agent_dir, provider, name),
    }
}

pub fn restore_snapshot(
    home: &MaturanaHome,
    agent_id: &str,
    name: &str,
    live: bool,
) -> anyhow::Result<SnapshotRecord> {
    if !live {
        anyhow::bail!("snapshot restore requires --live; local markers are not restorable");
    }
    let agent_dir = home.agent_dir(agent_id);
    let spec = load_agent_spec(&agent_dir)?;
    match spec.vm.provider {
        HostProvider::HyperV => restore_hyperv_snapshot(&agent_dir, agent_id, name),
        HostProvider::Firecracker => restore_firecracker_snapshot(&agent_dir, name),
    }
}

fn load_agent_spec(agent_dir: &Path) -> anyhow::Result<AgentSpec> {
    let spec_path = agent_dir.join("MATURANA.md");
    if !spec_path.exists() {
        anyhow::bail!(
            "agent does not exist or has no MATURANA.md: {}",
            agent_dir.display()
        );
    }
    AgentSpec::from_maturana_markdown(&spec_path)
        .with_context(|| format!("failed to parse {}", spec_path.display()))
}

fn list_local_snapshot_records(
    agent_dir: &Path,
    provider: HostProvider,
) -> anyhow::Result<Vec<SnapshotRecord>> {
    let snapshots_dir = agent_dir.join("snapshots");
    if !snapshots_dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(snapshots_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let record_path = entry.path().join("snapshot.json");
        if record_path.exists() {
            let raw = fs::read_to_string(&record_path)
                .with_context(|| format!("failed to read {}", record_path.display()))?;
            records.push(serde_json::from_str(&raw)?);
        } else {
            records.push(SnapshotRecord {
                name: entry.file_name().to_string_lossy().to_string(),
                provider: provider.clone(),
                kind: SnapshotKind::LocalMarker,
                created_at: String::new(),
                state_path: None,
                memory_path: None,
                disk_path: None,
            });
        }
    }
    records.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(records)
}

fn create_local_marker(
    agent_dir: &Path,
    provider: HostProvider,
    name: &str,
) -> anyhow::Result<SnapshotRecord> {
    let snapshot_dir = create_new_snapshot_dir(agent_dir, name)?;
    fs::write(
        snapshot_dir.join("README.md"),
        "Local snapshot marker only. Use --live for restorable VM snapshots.\n",
    )?;
    let record = SnapshotRecord {
        name: name.to_string(),
        provider,
        kind: SnapshotKind::LocalMarker,
        created_at: Utc::now().to_rfc3339(),
        state_path: None,
        memory_path: None,
        disk_path: None,
    };
    write_record(&snapshot_dir, &record)?;
    Ok(record)
}

fn take_hyperv_snapshot(
    agent_dir: &Path,
    agent_id: &str,
    name: &str,
) -> anyhow::Result<SnapshotRecord> {
    validate_snapshot_name(name)?;
    let snapshot_dir = create_new_snapshot_dir(agent_dir, name)?;
    let payload = match hostd_post_json(
        "/agents/snapshot/take",
        json!({
            "agent_id": agent_id,
            "name": name,
        }),
    ) {
        Ok(payload) => payload,
        Err(error) => {
            let _ = fs::remove_dir_all(&snapshot_dir);
            return Err(error);
        }
    };
    if let Err(error) = ensure_hostd_ok("snapshot take", &payload) {
        let _ = fs::remove_dir_all(&snapshot_dir);
        return Err(error);
    }
    let record = hyperv_snapshot_record(name);
    write_record(&snapshot_dir, &record)?;
    Ok(record)
}

fn hyperv_snapshot_record(name: &str) -> SnapshotRecord {
    SnapshotRecord {
        name: name.to_string(),
        provider: HostProvider::HyperV,
        kind: SnapshotKind::HyperVCheckpoint,
        created_at: Utc::now().to_rfc3339(),
        state_path: None,
        memory_path: None,
        disk_path: None,
    }
}

fn list_hyperv_snapshots(agent_id: &str) -> anyhow::Result<Vec<SnapshotRecord>> {
    let payload = hostd_get_json(&format!("/agents/snapshot/list?agent_id={agent_id}"))?;
    ensure_hostd_ok("snapshot list", &payload)?;
    let snapshots = payload
        .get("snapshots")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(snapshots
        .into_iter()
        .map(|snapshot| {
            let name = snapshot
                .get("Name")
                .or_else(|| snapshot.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or("<unnamed>")
                .to_string();
            let created_at = snapshot
                .get("CreationTime")
                .or_else(|| snapshot.get("creation_time"))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            SnapshotRecord {
                name,
                provider: HostProvider::HyperV,
                kind: SnapshotKind::HyperVCheckpoint,
                created_at,
                state_path: None,
                memory_path: None,
                disk_path: None,
            }
        })
        .collect())
}

fn restore_hyperv_snapshot(
    agent_dir: &Path,
    agent_id: &str,
    name: &str,
) -> anyhow::Result<SnapshotRecord> {
    validate_snapshot_name(name)?;
    validate_hyperv_restore_record_if_present(agent_dir, name)?;
    let payload = hostd_post_json(
        "/agents/snapshot/restore",
        json!({
            "agent_id": agent_id,
            "name": name,
        }),
    )?;
    ensure_hostd_ok("snapshot restore", &payload)?;
    Ok(SnapshotRecord {
        name: name.to_string(),
        provider: HostProvider::HyperV,
        kind: SnapshotKind::HyperVCheckpoint,
        created_at: Utc::now().to_rfc3339(),
        state_path: None,
        memory_path: None,
        disk_path: None,
    })
}

fn validate_hyperv_restore_record_if_present(agent_dir: &Path, name: &str) -> anyhow::Result<()> {
    let snapshot_dir = snapshot_dir(agent_dir, name)?;
    let record_path = snapshot_dir.join("snapshot.json");
    if !record_path.exists() {
        return Ok(());
    }
    let record = read_record(&snapshot_dir)?;
    if record.provider != HostProvider::HyperV || record.kind != SnapshotKind::HyperVCheckpoint {
        anyhow::bail!(
            "snapshot {} is {:?}/{:?}, not a restorable Hyper-V checkpoint",
            record.name,
            record.provider,
            record.kind
        );
    }
    Ok(())
}

fn take_firecracker_snapshot(agent_dir: &Path, name: &str) -> anyhow::Result<SnapshotRecord> {
    if cfg!(windows) {
        anyhow::bail!("Firecracker snapshots require a Linux host");
    }
    let metadata = firecracker_metadata(agent_dir)?;
    let snapshot_dir = create_new_snapshot_dir(agent_dir, name)?;
    let state_path = snapshot_dir.join("vm-state.snap");
    let memory_path = snapshot_dir.join("memory.mem");
    let disk_path = snapshot_dir.join("rootfs.ext4");

    if let Err(error) = firecracker_api(
        &metadata.socket,
        "PATCH",
        "/vm",
        json!({ "state": "Paused" }),
    )
    .context("failed to pause Firecracker VM before snapshot")
    {
        let _ = fs::remove_dir_all(&snapshot_dir);
        return Err(error);
    }
    let snapshot_result = (|| -> anyhow::Result<()> {
        firecracker_api(
            &metadata.socket,
            "PUT",
            "/snapshot/create",
            json!({
                "snapshot_type": "Full",
                "snapshot_path": state_path,
                "mem_file_path": memory_path,
            }),
        )?;
        fs::copy(&metadata.rootfs_path, &disk_path).with_context(|| {
            format!(
                "failed to copy rootfs {} to {}",
                metadata.rootfs_path.display(),
                disk_path.display()
            )
        })?;
        Ok(())
    })();
    let resume_result = firecracker_api(
        &metadata.socket,
        "PATCH",
        "/vm",
        json!({ "state": "Resumed" }),
    );
    if let Err(error) = reconcile_firecracker_snapshot_resume(snapshot_result, resume_result) {
        let _ = fs::remove_dir_all(&snapshot_dir);
        return Err(error);
    }

    let record = SnapshotRecord {
        name: name.to_string(),
        provider: HostProvider::Firecracker,
        kind: SnapshotKind::FirecrackerFull,
        created_at: Utc::now().to_rfc3339(),
        state_path: Some(state_path),
        memory_path: Some(memory_path),
        disk_path: Some(disk_path),
    };
    write_record(&snapshot_dir, &record)?;
    Ok(record)
}

fn reconcile_firecracker_snapshot_resume(
    snapshot_result: anyhow::Result<()>,
    resume_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (snapshot_result, resume_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(resume_error)) => {
            Err(resume_error.context("snapshot was created but Firecracker VM did not resume"))
        }
        (Err(snapshot_error), Ok(())) => Err(snapshot_error),
        (Err(snapshot_error), Err(resume_error)) => Err(snapshot_error.context(format!(
            "snapshot failed and Firecracker VM did not resume: {resume_error:#}"
        ))),
    }
}

fn restore_firecracker_snapshot(agent_dir: &Path, name: &str) -> anyhow::Result<SnapshotRecord> {
    if cfg!(windows) {
        anyhow::bail!("Firecracker restore requires a Linux host");
    }
    let snapshot_dir = snapshot_dir(agent_dir, name)?;
    let record = read_record(&snapshot_dir)?;
    validate_firecracker_restore_record(&record)?;
    let state_path =
        snapshot_component_path(&snapshot_dir, record.state_path.as_deref(), "vm-state.snap")?;
    let memory_path =
        snapshot_component_path(&snapshot_dir, record.memory_path.as_deref(), "memory.mem")?;
    let disk_path =
        snapshot_component_path(&snapshot_dir, record.disk_path.as_deref(), "rootfs.ext4")?;

    let metadata = firecracker_metadata(agent_dir)?;
    stop_firecracker_process(&metadata)?;
    let backup_path = replace_rootfs_with_backup(&metadata.rootfs_path, &disk_path)?;
    let restore_result = (|| -> anyhow::Result<()> {
        start_firecracker_empty(&metadata)?;
        wait_firecracker_api_ready(&metadata.socket)
            .context("Firecracker API did not become ready for snapshot restore")?;
        firecracker_api(
            &metadata.socket,
            "PUT",
            "/snapshot/load",
            json!({
                "snapshot_path": state_path,
                "mem_backend": {
                    "backend_path": memory_path,
                    "backend_type": "File",
                },
                "track_dirty_pages": true,
                "resume_vm": true,
            }),
        )
        .context("failed to load Firecracker snapshot")?;
        Ok(())
    })();
    if let Err(error) = restore_result {
        let mut message = format!("{error:#}");
        if let Err(stop_error) = stop_firecracker_process(&metadata) {
            message.push_str(&format!(
                "; also failed to stop Firecracker after restore failure: {stop_error:#}"
            ));
        }
        if let Err(rollback_error) =
            rollback_rootfs_backup(&metadata.rootfs_path, backup_path.as_deref())
        {
            message.push_str(&format!(
                "; also failed to roll back rootfs after restore failure: {rollback_error:#}"
            ));
        }
        anyhow::bail!(message);
    }
    cleanup_rootfs_backup(backup_path.as_deref())?;
    Ok(record)
}

fn validate_firecracker_restore_record(record: &SnapshotRecord) -> anyhow::Result<()> {
    if record.provider != HostProvider::Firecracker || record.kind != SnapshotKind::FirecrackerFull
    {
        anyhow::bail!(
            "snapshot {} is {:?}/{:?}, not a restorable Firecracker full snapshot",
            record.name,
            record.provider,
            record.kind
        );
    }
    if record.state_path.is_none() || record.memory_path.is_none() || record.disk_path.is_none() {
        anyhow::bail!(
            "snapshot {} is missing Firecracker state, memory, or disk paths",
            record.name
        );
    }
    Ok(())
}

fn snapshot_component_path(
    snapshot_dir: &Path,
    configured_path: Option<&Path>,
    default_name: &str,
) -> anyhow::Result<PathBuf> {
    let raw_path = configured_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| snapshot_dir.join(default_name));
    let candidate = if raw_path.is_absolute() {
        raw_path
    } else {
        snapshot_dir.join(raw_path)
    };
    if !candidate.exists() {
        anyhow::bail!("snapshot component is missing: {}", candidate.display());
    }
    let snapshot_root = snapshot_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize snapshot directory {}",
            snapshot_dir.display()
        )
    })?;
    let component = candidate.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize snapshot component {}",
            candidate.display()
        )
    })?;
    if !component.starts_with(&snapshot_root) {
        anyhow::bail!(
            "snapshot component escapes snapshot directory: {}",
            component.display()
        );
    }
    Ok(component)
}

fn replace_rootfs_with_backup(
    rootfs_path: &Path,
    disk_path: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let backup_path = if rootfs_path.exists() {
        let file_name = rootfs_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!("rootfs path has no file name: {}", rootfs_path.display())
            })?;
        let backup_path = rootfs_path.with_file_name(format!(
            "{file_name}.maturana-restore-backup-{}",
            uuid::Uuid::new_v4()
        ));
        fs::copy(rootfs_path, &backup_path).with_context(|| {
            format!(
                "failed to back up rootfs {} to {}",
                rootfs_path.display(),
                backup_path.display()
            )
        })?;
        Some(backup_path)
    } else {
        None
    };

    if let Err(error) = fs::copy(disk_path, rootfs_path).with_context(|| {
        format!(
            "failed to restore rootfs {} to {}",
            disk_path.display(),
            rootfs_path.display()
        )
    }) {
        rollback_rootfs_backup(rootfs_path, backup_path.as_deref())?;
        return Err(error);
    }

    Ok(backup_path)
}

fn rollback_rootfs_backup(rootfs_path: &Path, backup_path: Option<&Path>) -> anyhow::Result<()> {
    if let Some(backup_path) = backup_path {
        fs::copy(backup_path, rootfs_path).with_context(|| {
            format!(
                "failed to roll back rootfs {} to {}",
                backup_path.display(),
                rootfs_path.display()
            )
        })?;
        fs::remove_file(backup_path)
            .with_context(|| format!("failed to remove rootfs backup {}", backup_path.display()))?;
    }
    Ok(())
}

fn cleanup_rootfs_backup(backup_path: Option<&Path>) -> anyhow::Result<()> {
    if let Some(backup_path) = backup_path {
        match fs::remove_file(backup_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!("failed to remove rootfs backup {}", backup_path.display())
            }),
        }?;
    }
    Ok(())
}

#[derive(Debug)]
struct FirecrackerMetadata {
    socket: PathBuf,
    pid: PathBuf,
    rootfs_path: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

fn firecracker_metadata(agent_dir: &Path) -> anyhow::Result<FirecrackerMetadata> {
    let state_dir = agent_dir.join("state");
    let metadata_path = state_dir.join("firecracker-metadata.json");
    let config_path = state_dir.join("firecracker-config.json");
    let spec = load_agent_spec(agent_dir)?;
    let metadata: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?,
    )?;
    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?,
    )?;
    let socket = state_metadata_path(&state_dir, &metadata, "socket")?;
    let pid = state_metadata_path(&state_dir, &metadata, "pid")?;
    let rootfs_path = config
        .get("drives")
        .and_then(|value| value.as_array())
        .and_then(|drives| drives.first())
        .and_then(|drive| drive.get("path_on_host"))
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("firecracker config has no rootfs drive path"))?;
    validate_firecracker_rootfs_path(agent_dir, &spec, &rootfs_path)?;
    Ok(FirecrackerMetadata {
        socket,
        pid,
        rootfs_path,
        stdout_log: agent_dir.join("state/firecracker.stdout.log"),
        stderr_log: agent_dir.join("state/firecracker.stderr.log"),
    })
}

fn validate_firecracker_rootfs_path(
    agent_dir: &Path,
    spec: &AgentSpec,
    configured_rootfs_path: &Path,
) -> anyhow::Result<()> {
    let firecracker = spec
        .vm
        .firecracker
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("vm.firecracker is required for Firecracker restore"))?;
    let expected = absolute_normalized_path(Path::new(&firecracker.rootfs_image))?;
    let configured = absolute_normalized_path(configured_rootfs_path)?;
    if configured != expected {
        anyhow::bail!(
            "firecracker config rootfs {} does not match spec rootfs {}; refusing restore for {}",
            configured.display(),
            expected.display(),
            agent_dir.display()
        );
    }
    Ok(())
}

fn state_metadata_path(
    state_dir: &Path,
    value: &serde_json::Value,
    key: &str,
) -> anyhow::Result<PathBuf> {
    let raw_path = value
        .get(key)
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("firecracker metadata has no {key} path"))?;
    let state_root = absolute_normalized_path(state_dir)?;
    let candidate = absolute_normalized_path(&raw_path)?;
    if !candidate.starts_with(&state_root) {
        anyhow::bail!(
            "firecracker metadata {key} path escapes agent state directory: {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn absolute_normalized_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    normalize_path(&absolute)
}

fn normalize_path(path: &Path) -> anyhow::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("path escapes filesystem root: {}", path.display());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn firecracker_api(
    socket: &Path,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let output = Command::new("curl")
        .arg("--fail-with-body")
        .arg("--silent")
        .arg("--show-error")
        .arg("--connect-timeout")
        .arg("1")
        .arg("--max-time")
        .arg("5")
        .arg("--unix-socket")
        .arg(socket)
        .arg("-X")
        .arg(method)
        .arg(format!("http://localhost{path}"))
        .arg("-H")
        .arg("Accept: application/json")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(body.to_string())
        .stdin(Stdio::null())
        .output()
        .context("failed to execute curl for Firecracker API")?;
    if !output.status.success() {
        anyhow::bail!(
            "Firecracker API {method} {path} failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
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

fn stop_firecracker_process(metadata: &FirecrackerMetadata) -> anyhow::Result<()> {
    let pid = read_firecracker_pid(&metadata.pid)?;
    let socket_exists = metadata.socket.exists();
    let running = if let Some(pid) = pid {
        Some(process_running(pid)?)
    } else {
        None
    };
    let api_ready = if socket_exists && running != Some(true) {
        Some(firecracker_api_get(&metadata.socket, "/").is_ok())
    } else {
        None
    };
    match classify_firecracker_stop_action(pid, running, socket_exists, api_ready) {
        FirecrackerStopAction::StopTracked(pid) => {
            send_signal(pid, None)
                .with_context(|| format!("failed to send TERM to Firecracker pid {pid}"))?;
            if wait_for_process_exit(pid, Duration::from_secs(10))?.is_err() {
                send_signal(pid, Some("KILL"))
                    .with_context(|| format!("failed to send KILL to Firecracker pid {pid}"))?;
                wait_for_process_exit(pid, Duration::from_secs(5))?
                    .with_context(|| format!("Firecracker pid {pid} did not exit after KILL"))?;
            }
            remove_file_if_present(&metadata.pid)?;
            remove_file_if_present(&metadata.socket)?;
        }
        FirecrackerStopAction::CleanStale => {
            remove_file_if_present(&metadata.pid)?;
            remove_file_if_present(&metadata.socket)?;
        }
        FirecrackerStopAction::RefuseUntrackedSocket => {
            anyhow::bail!(
                "Firecracker API socket responds at {} but no tracked running pid exists at {}; refusing to remove a live untracked socket during snapshot restore",
                metadata.socket.display(),
                metadata.pid.display()
            );
        }
        FirecrackerStopAction::Nothing => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirecrackerStopAction {
    StopTracked(u32),
    CleanStale,
    RefuseUntrackedSocket,
    Nothing,
}

fn classify_firecracker_stop_action(
    pid: Option<u32>,
    running: Option<bool>,
    socket_exists: bool,
    api_ready: Option<bool>,
) -> FirecrackerStopAction {
    match (pid, running, socket_exists, api_ready) {
        (Some(pid), Some(true), _, _) => FirecrackerStopAction::StopTracked(pid),
        (_, _, true, Some(true)) => FirecrackerStopAction::RefuseUntrackedSocket,
        (Some(_), Some(false), _, _) | (Some(_), None, _, _) => FirecrackerStopAction::CleanStale,
        (None, _, true, _) => FirecrackerStopAction::CleanStale,
        (None, _, false, _) => FirecrackerStopAction::Nothing,
    }
}

fn start_firecracker_empty(metadata: &FirecrackerMetadata) -> anyhow::Result<()> {
    stop_firecracker_process(metadata)?;
    let stdout = fs::File::create(&metadata.stdout_log)?;
    let stderr = fs::File::create(&metadata.stderr_log)?;
    let child = Command::new("firecracker")
        .arg("--api-sock")
        .arg(&metadata.socket)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("failed to start Firecracker for snapshot restore")?;
    fs::write(&metadata.pid, child.id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if metadata.socket.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let stderr_tail = read_tail_lines(&metadata.stderr_log, 20).unwrap_or_default();
    anyhow::bail!(
        "Firecracker API socket did not appear at {}\nstderr tail:\n{}",
        metadata.socket.display(),
        stderr_tail.join("\n")
    )
}

fn read_firecracker_pid(path: &Path) -> anyhow::Result<Option<u32>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };
    parse_firecracker_pid(&raw)
        .with_context(|| format!("invalid Firecracker pid in {}", path.display()))
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
        .context("failed to check Firecracker process status")?
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

fn remove_file_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn read_tail_lines(path: &Path, limit: usize) -> anyhow::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let lines = raw.lines().map(ToString::to_string).collect::<Vec<_>>();
            let start = lines.len().saturating_sub(limit);
            Ok(lines[start..].to_vec())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
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

fn snapshot_dir(agent_dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    validate_snapshot_name(name)?;
    Ok(agent_dir.join("snapshots").join(name))
}

fn create_new_snapshot_dir(agent_dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let snapshot_dir = snapshot_dir(agent_dir, name)?;
    let parent = snapshot_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("snapshot directory has no parent"))?;
    fs::create_dir_all(parent)?;
    match fs::create_dir(&snapshot_dir) {
        Ok(()) => Ok(snapshot_dir),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!("snapshot already exists: {name}")
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to create snapshot {}", snapshot_dir.display())),
    }
}

fn validate_snapshot_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
    {
        anyhow::bail!("snapshot name must be a simple path segment: {name}");
    }
    Ok(())
}

fn write_record(snapshot_dir: &Path, record: &SnapshotRecord) -> anyhow::Result<()> {
    let record_path = snapshot_dir.join("snapshot.json");
    let temp_path = snapshot_dir.join(format!("snapshot.json.{}.tmp", uuid::Uuid::new_v4()));
    let payload = serde_json::to_string_pretty(record)?;
    {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        file.write_all(payload.as_bytes())
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
    }
    fs::rename(&temp_path, &record_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            record_path.display()
        )
    })?;
    Ok(())
}

fn read_record(snapshot_dir: &Path) -> anyhow::Result<SnapshotRecord> {
    let record_path = snapshot_dir.join("snapshot.json");
    serde_json::from_str(
        &fs::read_to_string(&record_path)
            .with_context(|| format!("failed to read {}", record_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", record_path.display()))
}

fn hostd_get_json(path: &str) -> anyhow::Result<serde_json::Value> {
    let mut request = ureq::get(&hostd_url(path));
    if let Some(token) = hostd_token()? {
        request = request.set("X-Maturana-Hostd-Token", &token);
    }
    Ok(request.call()?.into_json()?)
}

fn hostd_post_json(path: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let mut request = ureq::post(&hostd_url(path));
    if let Some(token) = hostd_token()? {
        request = request.set("X-Maturana-Hostd-Token", &token);
    }
    Ok(request.send_json(body)?.into_json()?)
}

fn ensure_hostd_ok(operation: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    if payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        Ok(())
    } else {
        anyhow::bail!("hostd {operation} returned an error: {payload}")
    }
}

fn hostd_url(path: &str) -> String {
    format!(
        "{}{}",
        std::env::var("MATURANA_HOSTD_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:47832".to_string())
            .trim_end_matches('/'),
        path
    )
}

fn hostd_token() -> anyhow::Result<Option<String>> {
    if let Ok(token) = std::env::var("MATURANA_HOSTD_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    let path = std::env::var("MATURANA_HOSTD_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/hostd/token"));
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.exists() {
        let token = fs::read_to_string(path)?;
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_snapshot_path_escape_names() {
        assert!(validate_snapshot_name("safe-name").is_ok());
        assert!(validate_snapshot_name("../bad").is_err());
        assert!(validate_snapshot_name("bad/name").is_err());
        assert!(validate_snapshot_name("bad\\name").is_err());
    }

    #[test]
    fn local_marker_writes_structured_record() {
        let root =
            std::env::temp_dir().join(format!("maturana-snapshot-test-{}", uuid::Uuid::new_v4()));
        let agent_dir = root.join("agents/demo");
        fs::create_dir_all(&agent_dir).unwrap();
        let record =
            create_local_marker(&agent_dir, HostProvider::Firecracker, "baseline").unwrap();
        assert_eq!(record.kind, SnapshotKind::LocalMarker);
        let records = list_local_snapshot_records(&agent_dir, HostProvider::Firecracker).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "baseline");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hyperv_live_snapshot_records_are_local_structured_metadata() {
        let root = std::env::temp_dir().join(format!(
            "maturana-hyperv-snapshot-record-test-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_dir = root.join("agents/demo");
        let snapshot_dir = create_new_snapshot_dir(&agent_dir, "before-upgrade").unwrap();
        let record = hyperv_snapshot_record("before-upgrade");
        write_record(&snapshot_dir, &record).unwrap();

        let records = list_local_snapshot_records(&agent_dir, HostProvider::HyperV).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "before-upgrade");
        assert_eq!(records[0].provider, HostProvider::HyperV);
        assert_eq!(records[0].kind, SnapshotKind::HyperVCheckpoint);
        assert!(records[0].state_path.is_none());
        assert!(records[0].memory_path.is_none());
        assert!(records[0].disk_path.is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_creation_rejects_existing_names() {
        let root = std::env::temp_dir().join(format!(
            "maturana-snapshot-existing-test-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_dir = root.join("agents/demo");
        fs::create_dir_all(&agent_dir).unwrap();

        create_local_marker(&agent_dir, HostProvider::Firecracker, "baseline").unwrap();
        let duplicate = create_local_marker(&agent_dir, HostProvider::Firecracker, "baseline")
            .unwrap_err()
            .to_string();
        assert!(duplicate.contains("snapshot already exists: baseline"));

        let direct_duplicate = create_new_snapshot_dir(&agent_dir, "baseline")
            .unwrap_err()
            .to_string();
        assert!(direct_duplicate.contains("snapshot already exists: baseline"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_record_write_is_atomic_and_leaves_no_temp_file() {
        let root = std::env::temp_dir().join(format!(
            "maturana-snapshot-record-test-{}",
            uuid::Uuid::new_v4()
        ));
        let snapshot_dir = root.join("agents/demo/snapshots/baseline");
        fs::create_dir_all(&snapshot_dir).unwrap();
        let record = SnapshotRecord {
            name: "baseline".to_string(),
            provider: HostProvider::Firecracker,
            kind: SnapshotKind::FirecrackerFull,
            created_at: "2026-06-09T00:00:00Z".to_string(),
            state_path: Some(PathBuf::from("vm-state.snap")),
            memory_path: Some(PathBuf::from("memory.mem")),
            disk_path: Some(PathBuf::from("rootfs.ext4")),
        };

        write_record(&snapshot_dir, &record).unwrap();
        assert_eq!(read_record(&snapshot_dir).unwrap(), record);
        let temp_files = fs::read_dir(&snapshot_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temp_files, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn firecracker_snapshot_reports_resume_failures() {
        let error =
            reconcile_firecracker_snapshot_resume(Ok(()), Err(anyhow::anyhow!("resume failed")))
                .unwrap_err()
                .to_string();
        assert!(error.contains("snapshot was created but Firecracker VM did not resume"));

        let error = reconcile_firecracker_snapshot_resume(
            Err(anyhow::anyhow!("snapshot failed")),
            Err(anyhow::anyhow!("resume failed")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("snapshot failed and Firecracker VM did not resume"));
    }

    #[test]
    fn firecracker_restore_stop_refuses_untracked_ready_socket() {
        assert_eq!(
            classify_firecracker_stop_action(Some(42), Some(true), true, None),
            FirecrackerStopAction::StopTracked(42)
        );
        assert_eq!(
            classify_firecracker_stop_action(None, None, true, Some(true)),
            FirecrackerStopAction::RefuseUntrackedSocket
        );
        assert_eq!(
            classify_firecracker_stop_action(Some(42), Some(false), true, Some(true)),
            FirecrackerStopAction::RefuseUntrackedSocket
        );
        assert_eq!(
            classify_firecracker_stop_action(Some(42), Some(false), true, Some(false)),
            FirecrackerStopAction::CleanStale
        );
        assert_eq!(
            classify_firecracker_stop_action(None, None, true, Some(false)),
            FirecrackerStopAction::CleanStale
        );
        assert_eq!(
            classify_firecracker_stop_action(None, None, false, None),
            FirecrackerStopAction::Nothing
        );
    }

    #[test]
    fn firecracker_restore_rejects_non_firecracker_full_records() {
        let record = SnapshotRecord {
            name: "marker".to_string(),
            provider: HostProvider::Firecracker,
            kind: SnapshotKind::LocalMarker,
            created_at: String::new(),
            state_path: None,
            memory_path: None,
            disk_path: None,
        };
        let error = validate_firecracker_restore_record(&record)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a restorable Firecracker full snapshot"));

        let record = SnapshotRecord {
            name: "hyperv".to_string(),
            provider: HostProvider::HyperV,
            kind: SnapshotKind::HyperVCheckpoint,
            created_at: String::new(),
            state_path: Some(PathBuf::from("vm-state.snap")),
            memory_path: Some(PathBuf::from("memory.mem")),
            disk_path: Some(PathBuf::from("rootfs.ext4")),
        };
        let error = validate_firecracker_restore_record(&record)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a restorable Firecracker full snapshot"));
    }

    #[test]
    fn hyperv_restore_rejects_mismatched_local_records_before_hostd_calls() {
        let root = std::env::temp_dir().join(format!(
            "maturana-hyperv-restore-record-test-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_dir = root.join("agents/demo");
        let marker_dir = create_new_snapshot_dir(&agent_dir, "marker").unwrap();
        let marker = SnapshotRecord {
            name: "marker".to_string(),
            provider: HostProvider::HyperV,
            kind: SnapshotKind::LocalMarker,
            created_at: String::new(),
            state_path: None,
            memory_path: None,
            disk_path: None,
        };
        write_record(&marker_dir, &marker).unwrap();
        let error = validate_hyperv_restore_record_if_present(&agent_dir, "marker")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a restorable Hyper-V checkpoint"));

        let firecracker_dir = create_new_snapshot_dir(&agent_dir, "firecracker").unwrap();
        let firecracker = SnapshotRecord {
            name: "firecracker".to_string(),
            provider: HostProvider::Firecracker,
            kind: SnapshotKind::FirecrackerFull,
            created_at: String::new(),
            state_path: Some(PathBuf::from("vm-state.snap")),
            memory_path: Some(PathBuf::from("memory.mem")),
            disk_path: Some(PathBuf::from("rootfs.ext4")),
        };
        write_record(&firecracker_dir, &firecracker).unwrap();
        let error = validate_hyperv_restore_record_if_present(&agent_dir, "firecracker")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a restorable Hyper-V checkpoint"));

        let checkpoint_dir = create_new_snapshot_dir(&agent_dir, "checkpoint").unwrap();
        write_record(&checkpoint_dir, &hyperv_snapshot_record("checkpoint")).unwrap();
        validate_hyperv_restore_record_if_present(&agent_dir, "checkpoint").unwrap();
        validate_hyperv_restore_record_if_present(&agent_dir, "hostd-only").unwrap();

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn firecracker_snapshot_components_must_stay_inside_snapshot_dir() {
        let root = std::env::temp_dir().join(format!(
            "maturana-snapshot-component-test-{}",
            uuid::Uuid::new_v4()
        ));
        let snapshot_dir = root.join("agents/demo/snapshots/baseline");
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(snapshot_dir.join("vm-state.snap"), "state").unwrap();
        fs::write(root.join("outside-rootfs.ext4"), "disk").unwrap();

        let resolved = snapshot_component_path(
            &snapshot_dir,
            Some(Path::new("vm-state.snap")),
            "vm-state.snap",
        )
        .unwrap();
        assert!(resolved.ends_with("vm-state.snap"));

        let relative_escape = snapshot_component_path(
            &snapshot_dir,
            Some(Path::new("../../../../outside-rootfs.ext4")),
            "rootfs.ext4",
        )
        .unwrap_err()
        .to_string();
        assert!(relative_escape.contains("escapes snapshot directory"));

        let absolute_escape = snapshot_component_path(
            &snapshot_dir,
            Some(&root.join("outside-rootfs.ext4")),
            "rootfs.ext4",
        )
        .unwrap_err()
        .to_string();
        assert!(absolute_escape.contains("escapes snapshot directory"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rootfs_restore_backup_can_commit_and_cleanup() {
        let root = std::env::temp_dir().join(format!(
            "maturana-rootfs-restore-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let rootfs = root.join("rootfs.ext4");
        let snapshot_disk = root.join("snapshot-rootfs.ext4");
        fs::write(&rootfs, "current").unwrap();
        fs::write(&snapshot_disk, "snapshot").unwrap();

        let backup = replace_rootfs_with_backup(&rootfs, &snapshot_disk)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(&rootfs).unwrap(), "snapshot");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "current");

        cleanup_rootfs_backup(Some(&backup)).unwrap();
        assert!(!backup.exists());
        assert_eq!(fs::read_to_string(&rootfs).unwrap(), "snapshot");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rootfs_restore_rolls_back_when_snapshot_copy_fails() {
        let root = std::env::temp_dir().join(format!(
            "maturana-rootfs-rollback-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let rootfs = root.join("rootfs.ext4");
        let missing_snapshot_disk = root.join("missing-rootfs.ext4");
        fs::write(&rootfs, "current").unwrap();

        let error = replace_rootfs_with_backup(&rootfs, &missing_snapshot_disk)
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to restore rootfs"));
        assert_eq!(fs::read_to_string(&rootfs).unwrap(), "current");
        let backup_count = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("maturana-restore-backup")
            })
            .count();
        assert_eq!(backup_count, 0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn firecracker_pid_parser_rejects_invalid_or_zero_pid() {
        assert_eq!(parse_firecracker_pid("").unwrap(), None);
        assert_eq!(parse_firecracker_pid(" 123 \n").unwrap(), Some(123));
        assert!(parse_firecracker_pid("0").is_err());
        assert!(parse_firecracker_pid("not-a-pid").is_err());
    }

    #[test]
    fn firecracker_metadata_paths_must_stay_inside_agent_state() {
        let root = std::env::temp_dir().join(format!(
            "maturana-metadata-path-test-{}",
            uuid::Uuid::new_v4()
        ));
        let state_dir = root.join("agents/demo/state");
        fs::create_dir_all(&state_dir).unwrap();
        let metadata = json!({
            "socket": state_dir.join("firecracker.socket"),
            "pid": state_dir.join("nested/../firecracker.pid"),
        });

        let socket = state_metadata_path(&state_dir, &metadata, "socket").unwrap();
        assert!(socket.ends_with("firecracker.socket"));
        let pid = state_metadata_path(&state_dir, &metadata, "pid").unwrap();
        assert!(pid.ends_with("firecracker.pid"));

        let escaping = json!({
            "socket": root.join("outside.socket"),
        });
        let error = state_metadata_path(&state_dir, &escaping, "socket")
            .unwrap_err()
            .to_string();
        assert!(error.contains("escapes agent state directory"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn firecracker_restore_rootfs_must_match_spec() {
        let root = std::env::temp_dir().join(format!(
            "maturana-rootfs-contract-test-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_dir = root.join("agents/demo");
        let state_dir = agent_dir.join("state");
        let images_dir = root.join("images");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();
        let expected_rootfs = images_dir.join("expected-rootfs.ext4");
        let other_rootfs = images_dir.join("other-rootfs.ext4");
        fs::write(&expected_rootfs, "expected").unwrap();
        fs::write(&other_rootfs, "other").unwrap();
        fs::write(
            agent_dir.join("MATURANA.md"),
            format!(
                "---\nidentity:\n  id: demo\n  name: Demo\n  purpose: Test\nruntime:\n  harness: codex\nvm:\n  provider: firecracker\n  firecracker:\n    kernel_image: {}\n    rootfs_image: {}\n---\n# Demo\n",
                images_dir.join("vmlinux.bin").display(),
                expected_rootfs.display()
            ),
        )
        .unwrap();
        fs::write(
            state_dir.join("firecracker-metadata.json"),
            serde_json::to_string_pretty(&json!({
                "socket": state_dir.join("firecracker.socket"),
                "pid": state_dir.join("firecracker.pid"),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            state_dir.join("firecracker-config.json"),
            serde_json::to_string_pretty(&json!({
                "drives": [{
                    "drive_id": "rootfs",
                    "path_on_host": other_rootfs,
                    "is_root_device": true,
                    "is_read_only": false
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let error = firecracker_metadata(&agent_dir).unwrap_err().to_string();
        assert!(error.contains("does not match spec rootfs"));

        fs::write(
            state_dir.join("firecracker-config.json"),
            serde_json::to_string_pretty(&json!({
                "drives": [{
                    "drive_id": "rootfs",
                    "path_on_host": expected_rootfs,
                    "is_root_device": true,
                    "is_read_only": false
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let metadata = firecracker_metadata(&agent_dir).unwrap();
        assert_eq!(metadata.rootfs_path, expected_rootfs);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hyperv_snapshot_names_are_validated_before_hostd_calls() {
        let root = std::env::temp_dir().join(format!(
            "maturana-hyperv-name-validation-test-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_dir = root.join("agents/demo");
        fs::create_dir_all(&agent_dir).unwrap();

        let take_error = take_hyperv_snapshot(&agent_dir, "demo", "../bad")
            .unwrap_err()
            .to_string();
        assert!(take_error.contains("snapshot name must be a simple path segment"));
        assert!(!agent_dir.join("snapshots").exists());

        let restore_error = restore_hyperv_snapshot(&agent_dir, "demo", "bad/name")
            .unwrap_err()
            .to_string();
        assert!(restore_error.contains("snapshot name must be a simple path segment"));

        fs::remove_dir_all(root).unwrap();
    }
}
