use super::{Provider, ProviderCommand};
use crate::{pipelock_proxy::ensure_mitm_ca_cert, spec::AgentSpec};
use anyhow::Context;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub struct FirecrackerProvider;

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
        let proxy_ca_cert_path = if proxy_port.is_some() {
            Some(absolute_path_pathbuf(ensure_mitm_ca_cert(
                &home_root_from_agent_dir(agent_dir)?,
            )?)?)
        } else {
            None
        };

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
            "guest_mac": firecracker.guest_mac,
            "socket": socket,
            "config": config_path,
            "pid": pid_path,
            "log": log_path,
            "metrics": metrics_path,
            "proxy_port": proxy_port,
            "proxy_https": proxy_port.is_some(),
            "proxy_ca_cert_path": proxy_ca_cert_path
        });
        fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

        let mut commands = Vec::new();
        let mut prepare_args = vec![];
        if let Some(port) = proxy_port {
            prepare_args.push(format!("MATURANA_PROXY_PORT={port}"));
            prepare_args.push("MATURANA_PROXY_HTTPS=1".to_string());
            if let Some(ca_cert) = &proxy_ca_cert_path {
                prepare_args.push(format!("MATURANA_PROXY_CA_CERT_PATH={}", ca_cert.display()));
            }
        }
        prepare_args.insert(0, "env".to_string());
        prepare_args.insert(0, "sudo".to_string());
        prepare_args.push("./scripts/firecracker-prepare-assets.sh".to_string());
        commands.push(ProviderCommand {
            program: prepare_args.remove(0),
            args: prepare_args,
            description: "prepare the Ubuntu Firecracker kernel/rootfs with guest proxy settings"
                .to_string(),
        });
        commands.extend([
            ProviderCommand {
                program: "scripts/firecracker-doctor.sh".to_string(),
                args: vec![
                    kernel_image.display().to_string(),
                    rootfs_image.display().to_string(),
                    firecracker.tap_name.clone(),
                ],
                description: "verify Firecracker host prerequisites and declared assets"
                    .to_string(),
            },
            ProviderCommand {
                program: "firecracker".to_string(),
                args: vec![
                    "--api-sock".to_string(),
                    socket.display().to_string(),
                    "--config-file".to_string(),
                    config_path.display().to_string(),
                ],
                description: "start Firecracker with the materialized microVM config".to_string(),
            },
            ProviderCommand {
                program: "scripts/firecracker-launch.sh".to_string(),
                args: vec![
                    agent_dir.display().to_string(),
                    firecracker.tap_name.clone(),
                    socket.display().to_string(),
                    config_path.display().to_string(),
                    pid_path.display().to_string(),
                ],
                description: "launch the Firecracker microVM on aidev".to_string(),
            },
        ]);
        Ok(commands)
    }

    fn launch(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()> {
        if cfg!(windows) {
            anyhow::bail!("Firecracker launch requires Linux; use aidev for this provider");
        }

        let firecracker = spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vm.firecracker is required for Firecracker"))?;
        let state_dir = agent_dir.join("state");
        let socket = state_dir.join("firecracker.socket");
        let config_path = state_dir.join("firecracker-config.json");
        let pid_path = state_dir.join("firecracker.pid");
        let status = Command::new("bash")
            .arg("scripts/firecracker-launch.sh")
            .arg(agent_dir)
            .arg(&firecracker.tap_name)
            .arg(socket)
            .arg(config_path)
            .arg(pid_path)
            .stdin(Stdio::null())
            .status()?;
        if !status.success() {
            anyhow::bail!("Firecracker launch failed with status {status}");
        }
        Ok(())
    }
}

fn absolute_path(path: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

fn absolute_path_pathbuf(path: PathBuf) -> anyhow::Result<PathBuf> {
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

fn home_root_from_agent_dir(agent_dir: &Path) -> anyhow::Result<PathBuf> {
    let agents_dir = agent_dir.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine agents dir from {}",
            agent_dir.display()
        )
    })?;
    let home_root = agents_dir.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine Maturana home from {}",
            agent_dir.display()
        )
    })?;
    Ok(home_root.to_path_buf())
}
