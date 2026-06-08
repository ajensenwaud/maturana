use crate::spec::{AgentSpec, HarnessRuntime, HostProvider};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn validate_spec(spec: &AgentSpec) -> ValidationReport {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    validate_id(&spec.identity.id, &mut errors);

    if spec.identity.name.trim().is_empty() {
        errors.push("identity.name must not be empty".to_string());
    }
    if spec.identity.purpose.trim().len() < 12 {
        warnings
            .push("identity.purpose is short; include the agent's operating boundary".to_string());
    }

    if matches!(
        spec.runtime.harness,
        HarnessRuntime::Codex | HarnessRuntime::ClaudeCode | HarnessRuntime::Opencode
    ) && !spec
        .harness_auth
        .iter()
        .any(|auth| auth.runtime == spec.runtime.harness)
    {
        warnings.push(format!(
            "{:?} needs direct guest auth/config injection; declare harness_auth to run authenticated guest harnesses",
            spec.runtime.harness
        ));
    }

    match spec.vm.provider {
        HostProvider::HyperV => {
            if cfg!(not(windows)) {
                warnings.push("hyper-v provider can only launch on Windows hosts".to_string());
            }
        }
        HostProvider::Firecracker => {
            if cfg!(windows) {
                warnings.push(
                    "firecracker provider is intended for Linux hosts such as aidev".to_string(),
                );
            }
            if spec.vm.firecracker.is_none() {
                errors.push(
                    "vm.firecracker with kernel_image and rootfs_image is required for firecracker"
                        .to_string(),
                );
            }
        }
    }

    if spec.vm.vcpu == 0 {
        errors.push("vm.vcpu must be at least 1".to_string());
    }
    if spec.vm.memory_mib < 512 {
        errors.push("vm.memory_mib must be at least 512".to_string());
    }
    if let Some(cloud_init) = &spec.vm.cloud_init {
        if cloud_init.username.trim().is_empty() {
            errors.push("vm.cloud_init.username must not be empty".to_string());
        }
        if !cloud_init.ssh_public_key.starts_with("ssh-") {
            errors.push("vm.cloud_init.ssh_public_key must be an SSH public key".to_string());
        }
    }

    for mount in &spec.filesystem.mounts {
        if mount.host_path.trim().is_empty() || mount.guest_path.trim().is_empty() {
            errors.push("filesystem.mounts entries require host_path and guest_path".to_string());
        }
        if mount.writable && mount.host_path == "/" {
            errors.push("filesystem writable mount of / is forbidden".to_string());
        }
    }

    for credential in &spec.credentials {
        validate_secret_source(
            &credential.source,
            &format!("credentials.{}", credential.name),
            &mut errors,
        );
    }
    for auth in &spec.harness_auth {
        if auth.source_path.trim().is_empty() {
            errors.push("harness_auth.source_path must not be empty".to_string());
        }
        if auth.guest_path.trim().is_empty() || !auth.guest_path.starts_with('/') {
            errors.push("harness_auth.guest_path must be an absolute guest path".to_string());
        }
    }

    if let Some(telegram) = &spec.channels.telegram {
        validate_secret_source(
            &telegram.token_source,
            "channels.telegram.token_source",
            &mut errors,
        );
        if let Some(chat_id_source) = &telegram.chat_id_source {
            validate_secret_source(
                chat_id_source,
                "channels.telegram.chat_id_source",
                &mut errors,
            );
        }
    }
    if let Some(discord) = &spec.channels.discord {
        validate_secret_source(
            &discord.webhook_source,
            "channels.discord.webhook_source",
            &mut errors,
        );
    }
    if let Some(wiki_path) = &spec.memory.wiki_path {
        if wiki_path.trim().is_empty() {
            errors.push("memory.wiki_path must not be empty".to_string());
        }
    }
    if let Some(agent_memory_path) = &spec.memory.agent_memory_path {
        if agent_memory_path.trim().is_empty() {
            errors.push("memory.agent_memory_path must not be empty".to_string());
        }
    }
    for skill in &spec.skills {
        if skill.trim().is_empty() {
            errors.push("skills entries must not be empty".to_string());
        }
    }
    for tool in &spec.tools {
        if tool.trim().is_empty() {
            errors.push("tools entries must not be empty".to_string());
        }
    }
    for schedule in &spec.schedules {
        if schedule.name.trim().is_empty() {
            errors.push("schedules.name must not be empty".to_string());
        }
        if schedule.cron.trim().is_empty() {
            errors.push("schedules.cron must not be empty".to_string());
        }
    }

    if spec.network.egress_allowlist.is_empty() {
        warnings.push(
            "network.egress_allowlist is empty; the agent will have no outbound network by default"
                .to_string(),
        );
    }
    if let Some(proxy) = &spec.network.proxy {
        if proxy.enabled && spec.network.egress_allowlist.is_empty() {
            errors.push("network.proxy requires network.egress_allowlist entries".to_string());
        }
        if proxy.enabled && proxy.bind.trim().is_empty() {
            errors.push("network.proxy.bind must not be empty".to_string());
        }
        for injection in &proxy.inject_headers {
            if injection.host.trim().is_empty() {
                errors.push("network.proxy.inject_headers.host must not be empty".to_string());
            }
            if injection.header.trim().is_empty() {
                errors.push("network.proxy.inject_headers.header must not be empty".to_string());
            }
            if !injection.source.starts_with("pipelock:") {
                errors.push(
                    "network.proxy.inject_headers.source must reference pipelock:".to_string(),
                );
            }
            let host = injection.host.to_ascii_lowercase();
            if proxy.enabled
                && !spec
                    .network
                    .egress_allowlist
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&host))
            {
                errors.push(format!(
                    "network.proxy.inject_headers host {} must also be in network.egress_allowlist",
                    injection.host
                ));
            }
        }
    }

    ValidationReport {
        valid: errors.is_empty(),
        errors,
        warnings,
    }
}

fn validate_id(id: &str, errors: &mut Vec<String>) {
    if id.trim().is_empty() {
        errors.push("identity.id must not be empty".to_string());
        return;
    }

    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        errors.push("identity.id must use lowercase letters, digits, and dashes only".to_string());
    }
}

fn validate_secret_source(source: &str, field: &str, errors: &mut Vec<String>) {
    let allowed = ["env:", "pipelock:", "file:"];
    if !allowed.iter().any(|prefix| source.starts_with(prefix)) {
        errors.push(format!(
            "{field} must reference a secret source, not contain a raw secret"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::*;

    #[test]
    fn rejects_raw_secret() {
        let spec = AgentSpec {
            identity: Identity {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                purpose: "A demo agent with a boundary".to_string(),
            },
            runtime: Runtime {
                harness: HarnessRuntime::Codex,
            },
            vm: Vm {
                provider: HostProvider::HyperV,
                guest_os: GuestOs::Linux,
                vcpu: 2,
                memory_mib: 2048,
                boot_image: None,
                switch_name: None,
                cloud_init: None,
                firecracker: None,
            },
            filesystem: Filesystem::default(),
            network: Network::default(),
            credentials: vec![Credential {
                name: "telegram".to_string(),
                source: "123:raw".to_string(),
            }],
            harness_auth: vec![],
            agent_run: AgentRun::default(),
            memory: Memory::default(),
            browser: Browser::default(),
            skills: vec![],
            tools: vec![],
            schedules: vec![],
            channels: Channels::default(),
            snapshots: SnapshotPolicy::default(),
        };

        let report = validate_spec(&spec);
        assert!(!report.valid);
    }

    #[test]
    fn accepts_proxy_injection_for_allowlisted_host() {
        let raw = r#"
identity:
  id: demo
  name: Demo
  purpose: A demo agent with a bounded network policy.
runtime:
  harness: codex
vm:
  provider: hyper-v
network:
  egress_allowlist:
    - api.example.test
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers:
      - host: api.example.test
        header: Authorization
        source: pipelock:api/token
"#;
        let spec: AgentSpec = serde_yaml::from_str(raw).unwrap();
        let report = validate_spec(&spec);
        assert!(report.valid, "{:?}", report.errors);
    }

    #[test]
    fn rejects_proxy_injection_outside_allowlist() {
        let raw = r#"
identity:
  id: demo
  name: Demo
  purpose: A demo agent with a bounded network policy.
runtime:
  harness: codex
vm:
  provider: hyper-v
network:
  egress_allowlist:
    - api.example.test
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers:
      - host: blocked.example.test
        header: Authorization
        source: pipelock:api/token
"#;
        let spec: AgentSpec = serde_yaml::from_str(raw).unwrap();
        let report = validate_spec(&spec);
        assert!(!report.valid);
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("must also be in network.egress_allowlist")));
    }
}
