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
            // A Firecracker guest's egress is proxy-routed: the worker bakes
            // HTTP_PROXY=<host_ip>:47833 into the guest, and the host plane only
            // launches that proxy when `network.proxy.enabled` is set. A spec that
            // declares egress (an allowlist or allow-all) but omits/disables the
            // proxy can't enforce that egress AND leaves a proxy-provisioned guest
            // hitting a dead port → "ConnectionRefused" on every turn (this is
            // exactly how claude/codex broke when their `network.proxy` block went
            // missing). Refuse it loudly instead of shipping a silently-broken agent.
            let declares_egress =
                !spec.network.egress_allowlist.is_empty() || spec.network.egress_allow_all;
            let proxy_on = spec
                .network
                .proxy
                .as_ref()
                .map(|proxy| proxy.enabled)
                .unwrap_or(false);
            if declares_egress && !proxy_on {
                let bind = spec
                    .vm
                    .firecracker
                    .as_ref()
                    .map(|fc| format!("{}:47833", fc.host_ip))
                    .unwrap_or_else(|| "<host_ip>:47833".to_string());
                errors.push(format!(
                    "network: this Firecracker agent declares egress (egress_allowlist/egress_allow_all) \
                     but has no enabled network.proxy. Firecracker egress is proxy-routed, so without it \
                     the allowlist is unenforced and a proxy-provisioned guest gets ConnectionRefused on \
                     every turn. Add:\n  proxy:\n    enabled: true\n    bind: {bind}"
                ));
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
        } else if !is_safe_guest_path(&auth.guest_path) {
            // The provisioner runs `rm -rf <guest_path>` before staging creds,
            // so an unconstrained value (`/`, `/etc`, `..`) would wipe a guest
            // system directory. Confine it to the agent-owned subtrees.
            errors.push(format!(
                "harness_auth.guest_path '{}' must live under /home, /agent, or /opt/maturana and contain no '..'",
                auth.guest_path
            ));
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
            &discord.bot_token_source,
            "channels.discord.bot_token_source",
            &mut errors,
        );
    }
    if let Some(slack) = &spec.channels.slack {
        validate_secret_source(
            &slack.bot_token_source,
            "channels.slack.bot_token_source",
            &mut errors,
        );
        validate_secret_source(
            &slack.app_token_source,
            "channels.slack.app_token_source",
            &mut errors,
        );
        if !host_allowlisted(spec, "api.slack.com") {
            warnings.push(
                "channels.slack: add api.slack.com to network.egress_allowlist".to_string(),
            );
        }
    }
    if let Some(agentmail) = &spec.channels.agentmail {
        validate_secret_source(
            &agentmail.api_key_source,
            "channels.agentmail.api_key_source",
            &mut errors,
        );
        if !host_allowlisted(spec, "api.agentmail.to") {
            warnings.push(
                "channels.agentmail: add api.agentmail.to to network.egress_allowlist".to_string(),
            );
        }
    }

    for (i, server) in spec.mcp_servers.iter().enumerate() {
        let label = format!("mcp_servers[{i}] ({})", server.name);
        if server.name.trim().is_empty() {
            errors.push("mcp_servers entries require a name".to_string());
        }
        match server.transport {
            crate::spec::McpTransport::Stdio => {
                if server.command.as_deref().unwrap_or("").trim().is_empty() {
                    errors.push(format!("{label}: stdio transport requires a command"));
                }
            }
            crate::spec::McpTransport::Http => {
                if server.url.as_deref().unwrap_or("").trim().is_empty() {
                    errors.push(format!("{label}: http transport requires a url"));
                }
            }
        }
        for env in &server.env {
            validate_secret_source(&env.source, &format!("{label}.env.{}", env.name), &mut errors);
        }
        for host in &server.egress_hosts {
            if !host_allowlisted(spec, host) {
                warnings.push(format!(
                    "{label}: egress host '{host}' is auto-allowed by the proxy but not in network.egress_allowlist"
                ));
            }
        }
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
    if spec.knowledge_graph.enabled {
        if let Some(graph) = &spec.knowledge_graph.graph {
            // The name becomes a directory under <home>/graphs/, so keep it a
            // safe identifier (no traversal).
            let ok = !graph.trim().is_empty()
                && graph.len() <= 128
                && !graph.contains("..")
                && graph
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !ok {
                errors.push(
                    "knowledge_graph.graph must be a safe name (letters, digits, -, _, .)"
                        .to_string(),
                );
            }
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

    // Open egress: the explicit flag, or a literal `*` wildcard left in the list.
    // It satisfies the allowlist requirements below (every host is reachable) but
    // is a deliberate removal of a zero-trust boundary, so warn loudly.
    let egress_open = spec.network.egress_allow_all
        || spec
            .network
            .egress_allowlist
            .iter()
            .any(|h| h.trim() == "*");
    if egress_open {
        warnings.push(
            "network.egress_allow_all is set: this agent can reach ANY host — egress governance \
             is OFF (traffic is still proxied and audited as allow_all). Prefer a scoped \
             network.egress_allowlist when the hosts are known."
                .to_string(),
        );
    } else if spec.network.egress_allowlist.is_empty() {
        warnings.push(
            "network.egress_allowlist is empty; the agent will have no outbound network by default"
                .to_string(),
        );
    }
    if let Some(proxy) = &spec.network.proxy {
        if proxy.enabled && !egress_open && spec.network.egress_allowlist.is_empty() {
            errors.push(
                "network.proxy requires network.egress_allowlist entries (or network.egress_allow_all)"
                    .to_string(),
            );
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
            // allow-all covers every host, so the explicit-allowlist requirement
            // for an injection target only applies when egress is scoped.
            if proxy.enabled
                && !egress_open
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

/// Whether `host` is already covered by the spec's egress allowlist (exact or
/// suffix match, matching the proxy's `host_allowed`).
fn host_allowlisted(spec: &AgentSpec, host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    spec.network.egress_allowlist.iter().any(|allowed| {
        let allowed = allowed.trim().to_ascii_lowercase();
        host == allowed || host.ends_with(&format!(".{allowed}"))
    })
}

fn is_safe_guest_path(path: &str) -> bool {
    let allowed_root = ["/home/", "/agent/", "/opt/maturana/"]
        .iter()
        .any(|root| path.starts_with(root));
    let allowed_exact = ["/agent", "/opt/maturana"].contains(&path);
    (allowed_root || allowed_exact)
        && !path.split('/').any(|seg| seg == "..")
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
    fn guest_path_safety() {
        // The example specs must stay valid.
        assert!(is_safe_guest_path("/home/ubuntu"));
        assert!(is_safe_guest_path("/home/ubuntu/.codex"));
        assert!(is_safe_guest_path("/home/ubuntu/.claude"));
        assert!(is_safe_guest_path("/agent"));
        assert!(is_safe_guest_path("/opt/maturana/bin"));
        // Dangerous destinations that would be `rm -rf`'d during provisioning.
        for bad in ["/", "/etc", "/usr/lib", "/home/ubuntu/../../etc", "/bin"] {
            assert!(!is_safe_guest_path(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_raw_secret() {
        let spec = AgentSpec {
            identity: Identity {
                id: "demo".to_string(),
                name: "Demo".to_string(),
                purpose: "A demo agent with a boundary".to_string(),
            },
            knowledge_graph: Default::default(),
            mcp_servers: Default::default(),
            capabilities: Default::default(),
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
    fn firecracker_egress_without_proxy_is_rejected() {
        // The claude/codex outage: a Firecracker agent that declares egress but has
        // no enabled proxy can't enforce its allowlist AND a proxy-routed guest gets
        // ConnectionRefused on every turn. Validation must refuse it loudly so a
        // regenerated/edited spec can never silently lose its egress proxy again.
        let spec_yaml = |proxy: &str| {
            format!(
                r#"
identity:
  id: fc-demo
  name: FC Demo
  purpose: A firecracker agent with a bounded network policy.
runtime:
  harness: claude-code
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  firecracker:
    kernel_image: .maturana/images/firecracker/x/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/x/ubuntu-rootfs.ext4
    tap_name: tap-mat-x
    host_ip: 172.30.10.9
    guest_ip: 172.30.10.10
    guest_mac: AA:FC:00:00:10:03
filesystem:
  mounts:
    - host_path: .maturana/agents/fc-demo/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.anthropic.com
{proxy}memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/fc-demo/memory
"#
            )
        };
        // No proxy block → rejected, with an actionable message.
        let no_proxy: AgentSpec = serde_yaml::from_str(&spec_yaml("")).unwrap();
        let report = validate_spec(&no_proxy);
        assert!(!report.valid, "egress without a proxy must be invalid");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("network.proxy") && e.contains("ConnectionRefused")),
            "error should name the missing proxy + the failure: {:?}",
            report.errors
        );
        // With the proxy block (bind derived from host_ip) → valid.
        let with_proxy: AgentSpec = serde_yaml::from_str(&spec_yaml(
            "  proxy:\n    enabled: true\n    bind: 172.30.10.9:47833\n",
        ))
        .unwrap();
        let report = validate_spec(&with_proxy);
        assert!(
            report.valid,
            "egress WITH a proxy must be valid: {:?}",
            report.errors
        );
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
