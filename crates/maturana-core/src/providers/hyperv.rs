use super::{Provider, ProviderCommand};
use crate::{
    pipelock_proxy::ensure_mitm_ca_cert,
    spec::{AgentSpec, HarnessRuntime},
};
use anyhow::Context;
use serde::Serialize;
use std::path::{Path, PathBuf};

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
                program: "maturana-hostd".to_string(),
                args: vec!["GET".to_string(), hostd_url("/health")],
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
        let proxy_port = proxy_port(spec)?;
        let proxy_ca_cert_path = if proxy_port.is_some() {
            Some(absolute_path(ensure_mitm_ca_cert(
                &home_root_from_agent_dir(agent_dir)?,
            )?)?)
        } else {
            None
        };
        let request = HostdUbuntuLaunchRequest {
            agent_id: &spec.identity.id,
            harness: harness_name(&spec.runtime.harness),
            base_vhdx_path: absolute_path(hyperv_base_vhdx())?,
            switch_name: spec.vm.switch_name.as_deref().unwrap_or("Default Switch"),
            ssh_user: "ubuntu",
            ssh_key_path: absolute_path(agent_ssh_key())?,
            harness_auth_source: auth
                .map(|auth| absolute_path(PathBuf::from(&auth.source_path)))
                .transpose()?,
            harness_auth_guest_path: auth.map(|auth| auth.guest_path.as_str()),
            agent_prompt: spec.agent_run.prompt.as_deref(),
            agent_command: spec.agent_run.command.as_deref(),
            session_id: session_id(&spec.identity.id),
            sessiond_url: None,
            sessiond_token_path: Some(absolute_path(sessiond_token_path())?),
            install_harness: spec.agent_run.install_harness,
            start_harness: spec.agent_run.start_on_boot,
            force: launch_force(),
            disk_size_gb: disk_size_gb(),
            vcpu: spec.vm.vcpu,
            memory_mib: spec.vm.memory_mib,
            proxy_port,
            proxy_https: proxy_port.is_some(),
            proxy_ca_cert_path,
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

        if body.accepted {
            println!(
                "hostd accepted Hyper-V launch for agent {} from {}",
                spec.identity.id,
                agent_dir.display()
            );
            if let Some(job_id) = body.job_id.as_deref() {
                println!("hostd launch job: {job_id}");
            }
            if let Some(status_url) = body.status_url.as_deref() {
                println!("hostd launch status: {status_url}");
            }
        } else {
            println!(
                "hostd launched agent {} from {}",
                spec.identity.id,
                agent_dir.display()
            );
        }
        if let Some(log) = body.log {
            println!("hostd log: {log}");
        }
        Ok(())
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
    harness_auth_source: Option<PathBuf>,
    harness_auth_guest_path: Option<&'a str>,
    agent_prompt: Option<&'a str>,
    agent_command: Option<&'a str>,
    session_id: &'a str,
    sessiond_url: Option<&'a str>,
    sessiond_token_path: Option<PathBuf>,
    install_harness: bool,
    start_harness: bool,
    force: bool,
    disk_size_gb: u32,
    vcpu: u8,
    memory_mib: u32,
    proxy_port: Option<u16>,
    proxy_https: bool,
    proxy_ca_cert_path: Option<PathBuf>,
}

#[derive(Debug, serde::Deserialize)]
struct HostdResponse {
    ok: bool,
    #[serde(default)]
    accepted: bool,
    job_id: Option<String>,
    status_url: Option<String>,
    exit_code: Option<i32>,
    #[serde(default)]
    output: Vec<String>,
    log: Option<String>,
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
