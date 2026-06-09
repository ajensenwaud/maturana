use super::{LiveAgentStatus, Provider, ProviderCommand};
use crate::{
    pipelock_proxy::ensure_mitm_ca_cert,
    spec::{AgentSpec, HarnessRuntime},
    worker::{
        render_guest_bootstrap, render_harness_install, render_run_agent, render_session_env,
        render_systemd_service, GuestWorkerConfig,
    },
};
use anyhow::Context;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub struct HyperVProvider;

impl Provider for HyperVProvider {
    fn plan_launch(
        &self,
        spec: &AgentSpec,
        _agent_dir: &Path,
    ) -> anyhow::Result<Vec<ProviderCommand>> {
        let base_image = hyperv_base_vhdx();
        let switch_name = spec
            .vm
            .switch_name
            .clone()
            .unwrap_or_else(|| "Default Switch".to_string());

        Ok(vec![
            ProviderCommand {
                program: "maturana".to_string(),
                args: vec!["hostd".to_string(), "status".to_string()],
                description: "verify the privileged Windows host daemon is reachable".to_string(),
            },
            ProviderCommand {
                program: "POST".to_string(),
                args: vec![
                    hostd_url("/agents/launch/ubuntu"),
                    "agent_id".to_string(),
                    spec.identity.id.clone(),
                    "base_vhdx_path".to_string(),
                    base_image.display().to_string(),
                    "switch_name".to_string(),
                    switch_name,
                ],
                description: "ask hostd to launch the official Ubuntu Hyper-V guest".to_string(),
            },
        ])
    }

    fn launch(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()> {
        if !cfg!(windows) {
            anyhow::bail!("Hyper-V launch requires a Windows host");
        }

        let auth = spec
            .harness_auth
            .iter()
            .find(|auth| auth.runtime == spec.runtime.harness);
        let state_dir = agent_dir.join("state");
        std::fs::create_dir_all(&state_dir)?;
        let proxy_env_path = if let Some(proxy_env) = render_guest_proxy_env(spec)? {
            let path = state_dir.join("proxy.env");
            std::fs::write(&path, proxy_env)?;
            Some(absolute_path(path)?)
        } else {
            None
        };
        let proxy_ca_cert_path = if proxy_env_path.is_some() {
            Some(absolute_path(ensure_mitm_ca_cert(
                &home_root_from_agent_dir(agent_dir)?,
            )?)?)
        } else {
            None
        };
        let sessiond_token_path = absolute_path(sessiond_token_path())?;
        let sessiond_token = read_optional_trimmed(&sessiond_token_path)?;
        let sessiond_env_path = state_dir.join("sessiond.env");
        let runner_path = state_dir.join("run-agent.sh");
        let service_path = state_dir.join("maturana-agent.service");
        let bootstrap_path = state_dir.join("guest-bootstrap.sh");
        let harness_install_path = state_dir.join("install-harness.sh");
        std::fs::write(
            &sessiond_env_path,
            render_session_env(&GuestWorkerConfig {
                agent_id: spec.identity.id.clone(),
                session_id: session_id(&spec.identity.id).to_string(),
                sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
                sessiond_token,
                harness: spec.runtime.harness.clone(),
                harness_auth_guest_path: auth.map(|auth| auth.guest_path.clone()).unwrap_or_else(
                    || default_harness_auth_guest_path(&spec.runtime.harness).to_string(),
                ),
                headless_chrome: spec.browser.headless_chrome,
            }),
        )?;
        std::fs::write(&runner_path, render_run_agent())?;
        std::fs::write(&bootstrap_path, render_guest_bootstrap())?;
        std::fs::write(
            &harness_install_path,
            render_harness_install(&spec.runtime.harness, spec.browser.headless_chrome),
        )?;
        std::fs::write(
            &service_path,
            render_systemd_service(
                &format!(
                    "Maturana {} agent {}",
                    harness_name(&spec.runtime.harness),
                    spec.identity.id
                ),
                "ubuntu",
            ),
        )?;
        let cloud_init_dir = state_dir.join("cloud-init");
        std::fs::create_dir_all(&cloud_init_dir)?;
        let cloud_init_user_data_path = cloud_init_dir.join("user-data");
        let cloud_init_meta_data_path = cloud_init_dir.join("meta-data");
        let public_key = read_ssh_public_key(&absolute_path(agent_ssh_key())?)?;
        std::fs::write(
            &cloud_init_user_data_path,
            render_cloud_init_user_data(
                "ubuntu",
                &format!("maturana-{}", spec.identity.id),
                &public_key,
            ),
        )?;
        std::fs::write(
            &cloud_init_meta_data_path,
            render_cloud_init_meta_data(
                &spec.identity.id,
                &format!("maturana-{}", spec.identity.id),
            ),
        )?;
        let harness_auth_source_path = auth
            .map(|auth| absolute_path(PathBuf::from(&auth.source_path)))
            .transpose()?;
        let request = HostdUbuntuLaunchRequest {
            agent_id: &spec.identity.id,
            harness: harness_name(&spec.runtime.harness),
            base_vhdx_path: absolute_path(hyperv_base_vhdx())?,
            switch_name: spec.vm.switch_name.as_deref().unwrap_or("Default Switch"),
            ssh_user: "ubuntu",
            ssh_key_path: absolute_path(agent_ssh_key())?,
            cloud_init_user_data_path: Some(cloud_init_user_data_path.clone()),
            cloud_init_meta_data_path: Some(cloud_init_meta_data_path.clone()),
            force: launch_force(),
            disk_size_gb: disk_size_gb(),
            vcpu: spec.vm.vcpu,
            memory_mib: spec.vm.memory_mib,
        };

        let url = hostd_url("/agents/launch/ubuntu");
        let mut http_request = ureq::post(&url);
        if let Some(token) = hostd_token()? {
            http_request = http_request.set("X-Maturana-Hostd-Token", &token);
        }
        let response = http_request.send_json(serde_json::to_value(&request)?)?;
        let status = response.status();
        let body: HostdResponse = response.into_json()?;
        if status >= 400 || !body.ok {
            anyhow::bail!(
                "hostd Hyper-V launch failed: status={status} exit_code={:?} output={}",
                body.exit_code,
                body.output.join("\n")
            );
        }
        let ipv4 = extract_create_only_ipv4(&body.output)
            .or_else(|| {
                self.inspect(spec, agent_dir)
                    .ok()
                    .and_then(|status| status.ipv4)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("hostd launch succeeded but no guest IPv4 was reported")
            })?;

        provision_hyperv_guest(HyperVGuestProvision {
            ip: &ipv4,
            ssh_user: "ubuntu",
            ssh_key_path: &absolute_path(agent_ssh_key())?,
            agent_dir,
            bootstrap_path: &bootstrap_path,
            proxy_env_path: proxy_env_path.as_deref(),
            proxy_ca_cert_path: proxy_ca_cert_path.as_deref(),
            harness_auth_source: harness_auth_source_path.as_deref(),
            harness_auth_guest_path: auth
                .map(|auth| auth.guest_path.as_str())
                .unwrap_or_else(|| default_harness_auth_guest_path(&spec.runtime.harness)),
            harness: &spec.runtime.harness,
            install_harness: spec.agent_run.install_harness,
            start_harness: spec.agent_run.start_on_boot,
            harness_install_path: &harness_install_path,
            sessiond_env_path: &sessiond_env_path,
            runner_path: &runner_path,
            service_path: &service_path,
        })?;

        println!(
            "hostd launched agent {} from {}",
            spec.identity.id,
            agent_dir.display()
        );
        if let Some(log) = body.log {
            println!("hostd log: {log}");
        }
        Ok(())
    }

    fn stop(&self, spec: &AgentSpec, _agent_dir: &Path) -> anyhow::Result<()> {
        let url = hostd_url("/agents/stop");
        let mut request = ureq::post(&url);
        if let Some(token) = hostd_token()? {
            request = request.set("X-Maturana-Hostd-Token", &token);
        }
        let response = request.send_json(serde_json::json!({
            "agent_id": spec.identity.id,
        }))?;
        let status = response.status();
        let body: HostdResponse = response.into_json()?;
        if status >= 400 || !body.ok {
            anyhow::bail!(
                "hostd Hyper-V stop failed: status={status} exit_code={:?} output={}",
                body.exit_code,
                body.output.join("\n")
            );
        }
        println!("hostd stopped Hyper-V agent {}", spec.identity.id);
        Ok(())
    }

    fn inspect(&self, spec: &AgentSpec, _agent_dir: &Path) -> anyhow::Result<LiveAgentStatus> {
        let url = hostd_url("/vms");
        let mut request = ureq::get(&url);
        if let Some(token) = hostd_token()? {
            request = request.set("X-Maturana-Hostd-Token", &token);
        }
        let payload: serde_json::Value = request.call()?.into_json()?;
        if !payload
            .get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            anyhow::bail!("hostd /vms returned an error: {payload}");
        }

        let expected_name = format!("maturana-{}", spec.identity.id);
        let vm = payload
            .get("vms")
            .and_then(|value| value.as_array())
            .and_then(|vms| {
                vms.iter().find(|vm| {
                    vm.get("name")
                        .and_then(|name| name.as_str())
                        .map(|name| name == expected_name)
                        .unwrap_or(false)
                })
            });

        Ok(LiveAgentStatus {
            provider: "hyper-v".to_string(),
            state: vm
                .and_then(|vm| vm.get("state"))
                .and_then(|value| value.as_str())
                .unwrap_or("not-found")
                .to_string(),
            vm_name: Some(expected_name),
            pid: None,
            ipv4: vm
                .and_then(|vm| vm.get("ipv4"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string),
            uptime: vm
                .and_then(|vm| vm.get("uptime"))
                .and_then(|value| value.as_str())
                .map(ToString::to_string),
            socket_path: None,
            config_path: None,
            metadata_path: None,
            metrics_tail: Vec::new(),
        })
    }
}

#[derive(Debug, Serialize)]
struct HostdUbuntuLaunchRequest<'a> {
    agent_id: &'a str,
    harness: &'a str,
    base_vhdx_path: PathBuf,
    switch_name: &'a str,
    ssh_user: &'a str,
    ssh_key_path: PathBuf,
    cloud_init_user_data_path: Option<PathBuf>,
    cloud_init_meta_data_path: Option<PathBuf>,
    force: bool,
    disk_size_gb: u32,
    vcpu: u8,
    memory_mib: u32,
}

#[derive(Debug, serde::Deserialize)]
struct HostdResponse {
    ok: bool,
    exit_code: Option<i32>,
    #[serde(default)]
    output: Vec<String>,
    log: Option<String>,
}

struct HyperVGuestProvision<'a> {
    ip: &'a str,
    ssh_user: &'a str,
    ssh_key_path: &'a Path,
    agent_dir: &'a Path,
    bootstrap_path: &'a Path,
    proxy_env_path: Option<&'a Path>,
    proxy_ca_cert_path: Option<&'a Path>,
    harness_auth_source: Option<&'a Path>,
    harness_auth_guest_path: &'a str,
    harness: &'a HarnessRuntime,
    install_harness: bool,
    start_harness: bool,
    harness_install_path: &'a Path,
    sessiond_env_path: &'a Path,
    runner_path: &'a Path,
    service_path: &'a Path,
}

fn provision_hyperv_guest(config: HyperVGuestProvision<'_>) -> anyhow::Result<()> {
    copy_to_guest(&config, config.bootstrap_path, "/tmp/guest-bootstrap.sh")?;
    invoke_guest(
        &config,
        "chmod 0755 /tmp/guest-bootstrap.sh && /tmp/guest-bootstrap.sh && rm -f /tmp/guest-bootstrap.sh",
    )?;

    for name in ["MATURANA.md", "AGENTS.md", "SOUL.md"] {
        let path = config.agent_dir.join(name);
        if path.exists() {
            copy_to_guest(&config, &path, &format!("/tmp/{name}"))?;
            invoke_guest(
                &config,
                &format!(
                    "sudo mv /tmp/{name} /agent/{name} && sudo chown {}:{} /agent/{name}",
                    shell_quote(config.ssh_user),
                    shell_quote(config.ssh_user)
                ),
            )?;
        }
    }

    if let Some(proxy_env_path) = config.proxy_env_path {
        copy_to_guest(&config, proxy_env_path, "/tmp/proxy.env")?;
        invoke_guest(
            &config,
            &format!(
                "sudo mv /tmp/proxy.env /agent/proxy.env && sudo chown {}:{} /agent/proxy.env && sudo chmod 0644 /agent/proxy.env",
                shell_quote(config.ssh_user),
                shell_quote(config.ssh_user)
            ),
        )?;
        if let Some(ca_cert_path) = config.proxy_ca_cert_path {
            copy_to_guest(&config, ca_cert_path, "/tmp/maturana-pipelock-ca.crt")?;
            invoke_guest(
                &config,
                "sudo mv /tmp/maturana-pipelock-ca.crt /usr/local/share/ca-certificates/maturana-pipelock-ca.crt && sudo chmod 0644 /usr/local/share/ca-certificates/maturana-pipelock-ca.crt && sudo update-ca-certificates",
            )?;
        }
    }

    if let Some(auth_source) = config.harness_auth_source {
        copy_to_guest(&config, auth_source, "/tmp/maturana-harness-auth")?;
        if matches!(config.harness, HarnessRuntime::Opencode) {
            invoke_guest(
                &config,
                &format!(
                    "sudo mkdir -p {} && sudo cp -a /tmp/maturana-harness-auth/. {}/ && sudo rm -rf /tmp/maturana-harness-auth && sudo chown -R {}:{} {}/.config {}/.local 2>/dev/null || true && chmod -R go-rwx {}/.config {}/.local 2>/dev/null || true && if [ -f {}/.maturana-env ]; then sudo chown {}:{} {}/.maturana-env && sudo chmod 0600 {}/.maturana-env; fi",
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.ssh_user),
                    shell_quote(config.ssh_user),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.ssh_user),
                    shell_quote(config.ssh_user),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                ),
            )?;
        } else {
            let parent = guest_parent_path(config.harness_auth_guest_path)?;
            invoke_guest(
                &config,
                &format!(
                    "sudo mkdir -p {} && sudo rm -rf {} && sudo mv /tmp/maturana-harness-auth {} && sudo chown -R {}:{} {} && chmod -R go-rwx {}",
                    shell_quote(&parent),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.ssh_user),
                    shell_quote(config.ssh_user),
                    shell_quote(config.harness_auth_guest_path),
                    shell_quote(config.harness_auth_guest_path),
                ),
            )?;
        }
    }

    if config.install_harness {
        copy_to_guest(
            &config,
            config.harness_install_path,
            "/tmp/install-harness.sh",
        )?;
        invoke_guest(
            &config,
            "chmod 0755 /tmp/install-harness.sh && /tmp/install-harness.sh && rm -f /tmp/install-harness.sh",
        )?;
    }

    copy_to_guest(&config, config.sessiond_env_path, "/tmp/sessiond.env")?;
    invoke_guest(
        &config,
        &format!(
            "sudo mv /tmp/sessiond.env /agent/sessiond.env && sudo chown {}:{} /agent/sessiond.env && sudo chmod 0600 /agent/sessiond.env",
            shell_quote(config.ssh_user),
            shell_quote(config.ssh_user)
        ),
    )?;
    copy_to_guest(&config, config.runner_path, "/tmp/run-agent.sh")?;
    invoke_guest(
        &config,
        "sudo mv /tmp/run-agent.sh /opt/maturana/bin/run-agent.sh && sudo chmod 0755 /opt/maturana/bin/run-agent.sh",
    )?;
    copy_to_guest(&config, config.service_path, "/tmp/maturana-agent.service")?;
    invoke_guest(
        &config,
        "sudo mv /tmp/maturana-agent.service /etc/systemd/system/maturana-agent.service && sudo systemctl daemon-reload && sudo systemctl enable maturana-agent.service",
    )?;
    if config.start_harness {
        invoke_guest(&config, "sudo systemctl restart maturana-agent.service")?;
    }
    Ok(())
}

fn invoke_guest(config: &HyperVGuestProvision<'_>, command: &str) -> anyhow::Result<()> {
    let destination = format!("{}@{}", config.ssh_user, config.ip);
    let status = Command::new("ssh")
        .args(ssh_options(config.ssh_key_path))
        .arg(destination)
        .arg(command)
        .status()
        .with_context(|| format!("failed to execute guest command: {command}"))?;
    if !status.success() {
        anyhow::bail!("guest command failed with status {status}: {command}");
    }
    Ok(())
}

fn copy_to_guest(
    config: &HyperVGuestProvision<'_>,
    source: &Path,
    destination: &str,
) -> anyhow::Result<()> {
    if !source.exists() {
        anyhow::bail!(
            "guest provision source does not exist: {}",
            source.display()
        );
    }
    let target = format!("{}@{}:{destination}", config.ssh_user, config.ip);
    let status = Command::new("scp")
        .args(ssh_options(config.ssh_key_path))
        .arg("-r")
        .arg(source)
        .arg(target)
        .status()
        .with_context(|| {
            format!(
                "failed to copy guest provision source {} to {destination}",
                source.display()
            )
        })?;
    if !status.success() {
        anyhow::bail!(
            "guest copy failed with status {status}: {} -> {destination}",
            source.display()
        );
    }
    Ok(())
}

fn ssh_options(ssh_key_path: &Path) -> Vec<String> {
    vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=NUL".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-i".to_string(),
        ssh_key_path.display().to_string(),
    ]
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn guest_parent_path(path: &str) -> anyhow::Result<String> {
    let trimmed = path.trim_end_matches('/');
    let Some((parent, _)) = trimmed.rsplit_once('/') else {
        anyhow::bail!("guest path must be absolute: {path}");
    };
    if parent.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(parent.to_string())
    }
}

fn extract_create_only_ipv4(output: &[String]) -> Option<String> {
    output.iter().find_map(|line| {
        let json = line.strip_prefix("MATURANA_RESULT_JSON=")?;
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        value
            .get("ipv4")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
    })
}

fn hostd_url(path: &str) -> String {
    let base =
        std::env::var("MATURANA_HOSTD_URL").unwrap_or_else(|_| "http://127.0.0.1:47832".into());
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn hyperv_base_vhdx() -> PathBuf {
    std::env::var("MATURANA_HYPERV_BASE_VHDX")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(".maturana/images/ubuntu-noble/noble-server-cloudimg-amd64.vhdx")
        })
}

fn agent_ssh_key() -> PathBuf {
    std::env::var("MATURANA_AGENT_SSH_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/keys/maturana-agent-ed25519"))
}

fn sessiond_token_path() -> PathBuf {
    std::env::var("MATURANA_SESSIOND_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/sessiond/token"))
}

fn launch_force() -> bool {
    std::env::var("MATURANA_HYPERV_FORCE")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn disk_size_gb() -> u32 {
    std::env::var("MATURANA_HYPERV_DISK_SIZE_GB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24)
}

fn render_guest_proxy_env(spec: &AgentSpec) -> anyhow::Result<Option<String>> {
    let Some(proxy) = &spec.network.proxy else {
        return Ok(None);
    };
    if !proxy.enabled {
        return Ok(None);
    }
    let port = parse_bind_port(&proxy.bind)?;
    Ok(Some(format!(
        "MATURANA_USE_HOST_PROXY=1\nMATURANA_PROXY_PORT={port}\nMATURANA_PROXY_HTTPS=1\nNO_PROXY=localhost,127.0.0.1,::1\n"
    )))
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

fn hostd_token() -> anyhow::Result<Option<String>> {
    if let Ok(token) = std::env::var("MATURANA_HOSTD_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    let path = std::env::var("MATURANA_HOSTD_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/hostd/token"));
    let path = absolute_path(path)?;
    if path.exists() {
        let token = std::fs::read_to_string(path)?;
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    Ok(None)
}

fn harness_name(harness: &HarnessRuntime) -> &'static str {
    match harness {
        HarnessRuntime::Codex => "codex",
        HarnessRuntime::ClaudeCode => "claude-code",
        HarnessRuntime::Opencode => "opencode",
    }
}

fn default_harness_auth_guest_path(harness: &HarnessRuntime) -> &'static str {
    match harness {
        HarnessRuntime::Codex => "/home/ubuntu/.codex",
        HarnessRuntime::ClaudeCode => "/home/ubuntu/.claude",
        HarnessRuntime::Opencode => "/home/ubuntu",
    }
}

fn session_id(agent_id: &str) -> &str {
    match agent_id {
        "claude-demo" => "claude-main",
        "opencode-demo" => "opencode-main",
        _ => "telegram-main",
    }
}

fn absolute_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

fn read_optional_trimmed(path: &Path) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    Ok(std::fs::read_to_string(path)?.trim().to_string())
}

fn read_ssh_public_key(private_key_path: &Path) -> anyhow::Result<String> {
    let public_key_path = PathBuf::from(format!("{}.pub", private_key_path.display()));
    if !public_key_path.exists() {
        anyhow::bail!(
            "SSH public key is missing: {}. Run `maturana repair ssh-key` first.",
            public_key_path.display()
        );
    }
    let public_key = std::fs::read_to_string(&public_key_path)
        .with_context(|| format!("failed to read {}", public_key_path.display()))?;
    let public_key = public_key.trim();
    if public_key.is_empty() {
        anyhow::bail!("SSH public key is empty: {}", public_key_path.display());
    }
    Ok(public_key.to_string())
}

fn render_cloud_init_user_data(ssh_user: &str, hostname: &str, public_key: &str) -> String {
    format!(
        "#cloud-config\nhostname: {hostname}\nmanage_etc_hosts: true\nssh_pwauth: false\ndisable_root: true\nusers:\n  - default\n  - name: {ssh_user}\n    gecos: Maturana Agent\n    groups: [adm, sudo]\n    shell: /bin/bash\n    sudo: ALL=(ALL) NOPASSWD:ALL\n    lock_passwd: true\n    ssh_authorized_keys:\n      - {public_key}\nruncmd:\n  - [ systemctl, enable, --now, ssh ]\n"
    )
}

fn render_cloud_init_meta_data(agent_id: &str, hostname: &str) -> String {
    format!("instance-id: {agent_id}\nlocal-hostname: {hostname}\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_cloud_init_user_and_metadata() {
        let user_data = render_cloud_init_user_data(
            "ubuntu",
            "maturana-codex-demo",
            "ssh-ed25519 AAAA maturana-agent",
        );
        assert!(user_data.contains("hostname: maturana-codex-demo"));
        assert!(user_data.contains("name: ubuntu"));
        assert!(user_data.contains("sudo: ALL=(ALL) NOPASSWD:ALL"));
        assert!(user_data.contains("ssh-ed25519 AAAA maturana-agent"));
        assert!(user_data.contains("[ systemctl, enable, --now, ssh ]"));

        let meta_data = render_cloud_init_meta_data("codex-demo", "maturana-codex-demo");
        assert!(meta_data.contains("instance-id: codex-demo"));
        assert!(meta_data.contains("local-hostname: maturana-codex-demo"));
    }

    #[test]
    fn renders_hyperv_proxy_env_from_spec() {
        let spec = AgentSpec::from_maturana_markdown(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("examples/MATURANA.codex-hyperv.md"),
        )
        .unwrap();
        let proxy_env = render_guest_proxy_env(&spec).unwrap().unwrap();

        assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
        assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));
        assert!(proxy_env.contains("MATURANA_PROXY_HTTPS=1"));
        assert!(proxy_env.contains("NO_PROXY=localhost,127.0.0.1,::1"));
        assert!(!proxy_env.contains("MATURANA_PROXY_HOST="));
    }

    #[test]
    fn hyperv_guest_scripts_are_rust_rendered() {
        let spec = AgentSpec::from_maturana_markdown(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("examples/MATURANA.claude-hyperv.md"),
        )
        .unwrap();
        let install = render_harness_install(&spec.runtime.harness, spec.browser.headless_chrome);

        assert!(install.contains("@anthropic-ai/claude-code"));
        assert!(install.contains("playwright install --with-deps chromium"));
        assert!(render_guest_bootstrap().contains("/opt/maturana/bin"));
    }

    #[test]
    fn reads_explicit_public_key_file_for_cloud_init() {
        let temp = std::env::temp_dir().join(format!(
            "maturana-hyperv-public-key-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let key_path = temp.join("maturana-agent-ed25519");

        let missing = read_ssh_public_key(&key_path).unwrap_err().to_string();
        assert!(missing.contains("SSH public key is missing"));

        std::fs::write(
            PathBuf::from(format!("{}.pub", key_path.display())),
            "ssh-ed25519 AAAA maturana-agent\n",
        )
        .unwrap();
        assert_eq!(
            read_ssh_public_key(&key_path).unwrap(),
            "ssh-ed25519 AAAA maturana-agent"
        );

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn parses_create_only_ipv4_from_hostd_output() {
        let output = vec![
            "launch starting".to_string(),
            "MATURANA_RESULT_JSON={\"ok\":true,\"ipv4\":\"172.26.1.10\"}".to_string(),
        ];
        assert_eq!(
            extract_create_only_ipv4(&output).as_deref(),
            Some("172.26.1.10")
        );

        assert!(extract_create_only_ipv4(&["MATURANA_RESULT_JSON={bad".to_string()]).is_none());
        assert!(extract_create_only_ipv4(&["launch complete".to_string()]).is_none());
    }

    #[test]
    fn guest_path_helpers_are_safe_for_shell_commands() {
        assert_eq!(shell_quote("/home/ubuntu/.codex"), "'/home/ubuntu/.codex'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
        assert_eq!(
            guest_parent_path("/home/ubuntu/.codex").unwrap(),
            "/home/ubuntu"
        );
        assert_eq!(guest_parent_path("/agent").unwrap(), "/");
        assert!(guest_parent_path("relative").is_err());
    }

    #[test]
    fn hostd_launch_request_excludes_guest_provisioning_fields() {
        let request = HostdUbuntuLaunchRequest {
            agent_id: "codex-demo",
            harness: "codex",
            base_vhdx_path: PathBuf::from("base.vhdx"),
            switch_name: "Default Switch",
            ssh_user: "ubuntu",
            ssh_key_path: PathBuf::from("key"),
            cloud_init_user_data_path: Some(PathBuf::from("user-data")),
            cloud_init_meta_data_path: Some(PathBuf::from("meta-data")),
            force: true,
            disk_size_gb: 24,
            vcpu: 2,
            memory_mib: 2048,
        };
        let json = serde_json::to_value(&request).unwrap();
        let object = json.as_object().unwrap();

        assert!(object.contains_key("cloud_init_user_data_path"));
        for forbidden in [
            "harness_auth_source",
            "sessiond_env_path",
            "runner_path",
            "service_path",
            "bootstrap_path",
            "harness_install_path",
            "install_harness",
            "start_harness",
            "proxy_env_path",
            "proxy_ca_cert_path",
        ] {
            assert!(
                !object.contains_key(forbidden),
                "hostd request should not include {forbidden}"
            );
        }
    }
}
