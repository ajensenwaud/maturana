use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntime {
    Codex,
    ClaudeCode,
    Opencode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HostProvider {
    HyperV,
    Firecracker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub identity: Identity,
    pub runtime: Runtime,
    pub vm: Vm,
    #[serde(default)]
    pub filesystem: Filesystem,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub harness_auth: Vec<HarnessAuth>,
    #[serde(default)]
    pub agent_run: AgentRun,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub knowledge_graph: KnowledgeGraph,
    #[serde(default)]
    pub browser: Browser,
    /// Model-Context-Protocol servers the guest harness should connect to.
    /// Rendered into the harness's native config and shipped with auth.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// Opt-in agent capabilities that gate egress defaults + skills.
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub channels: Channels,
    #[serde(default)]
    pub snapshots: SnapshotPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub purpose: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub harness: HarnessRuntime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAuth {
    pub runtime: HarnessRuntime,
    pub source_path: String,
    pub guest_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRun {
    #[serde(default)]
    pub install_harness: bool,
    #[serde(default)]
    pub start_on_boot: bool,
}

impl Default for AgentRun {
    fn default() -> Self {
        Self {
            install_harness: true,
            start_on_boot: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    pub provider: HostProvider,
    #[serde(default)]
    pub guest_os: GuestOs,
    #[serde(default = "default_vcpu")]
    pub vcpu: u8,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u32,
    #[serde(default)]
    pub boot_image: Option<String>,
    #[serde(default)]
    pub switch_name: Option<String>,
    #[serde(default)]
    pub cloud_init: Option<CloudInit>,
    #[serde(default)]
    pub firecracker: Option<FirecrackerVm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudInit {
    pub username: String,
    pub ssh_public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirecrackerVm {
    pub kernel_image: String,
    pub rootfs_image: String,
    #[serde(default = "default_firecracker_tap")]
    pub tap_name: String,
    #[serde(default = "default_firecracker_host_ip")]
    pub host_ip: String,
    #[serde(default = "default_firecracker_guest_ip")]
    pub guest_ip: String,
    #[serde(default = "default_firecracker_guest_mac")]
    pub guest_mac: String,
    #[serde(default = "default_firecracker_kernel_args")]
    pub kernel_args: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GuestOs {
    Linux,
    Windows,
}

impl Default for GuestOs {
    fn default() -> Self {
        Self::Linux
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Filesystem {
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub host_path: String,
    pub guest_path: String,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(default)]
    pub egress_allowlist: Vec<String>,
    /// Open egress: when true the pipelock proxy permits ANY host instead of only
    /// `egress_allowlist`. Traffic still flows THROUGH the proxy (header injection
    /// + audit keep working; requests audit as `grant_source=allow_all`) — the
    /// allowlist is simply not enforced. The deliberate "let this agent reach the
    /// whole web" opt-in; prefer a scoped `egress_allowlist` when the hosts are known.
    #[serde(default)]
    pub egress_allow_all: bool,
    #[serde(default)]
    pub proxy: Option<NetworkProxy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProxy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_proxy_bind")]
    pub bind: String,
    #[serde(default)]
    pub inject_headers: Vec<NetworkProxyHeader>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProxyHeader {
    pub host: String,
    pub header: String,
    pub source: String,
    /// Optional literal prepended to the injected secret (e.g. `"Bearer "`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    #[serde(default)]
    pub wiki_path: Option<String>,
    #[serde(default)]
    pub agent_memory_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Browser {
    #[serde(default)]
    pub headless_chrome: bool,
}

/// Opt-in MaturanaGraph (knowledge graph + GraphRAG) for the agent. When
/// enabled the guest worker is given the graph service URL + token and the
/// `maturana-graph` skill so the agent can read/write a knowledge graph.
///
/// `graph` names the graph to connect to. Multiple agents naming the **same**
/// graph share it (multi-agent knowledge graph); omitting it gives the agent a
/// private graph named after its id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeGraph {
    /// On by default — the built-in graph store is a headline feature, so agents
    /// get private memory + GraphRAG unless a spec opts out with `enabled: false`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub graph: Option<String>,
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self {
            enabled: true,
            graph: None,
        }
    }
}

impl KnowledgeGraph {
    /// The graph name this agent connects to: the explicit `graph`, else the
    /// agent's own id (a private-by-convention graph).
    pub fn graph_name(&self, agent_id: &str) -> String {
        self.graph
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| agent_id.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub name: String,
    pub cron: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Channels {
    /// Local console chat: declares the agent is reachable from the terminal via
    /// `maturana agent chat <id>` (an interactive TUI over sessiond). Informational
    /// — the TUI works for any running agent; this records it as an intended surface.
    #[serde(default)]
    pub tui: bool,
    #[serde(default)]
    pub telegram: Option<TelegramChannel>,
    #[serde(default)]
    pub discord: Option<DiscordChannel>,
    #[serde(default)]
    pub slack: Option<SlackChannel>,
    #[serde(default)]
    pub agentmail: Option<AgentMailChannel>,
}

/// Slack via Socket Mode: a bot token (`xoxb-…`) for posting and an app-level
/// token (`xapp-…`) for the events WebSocket. Both resolved host-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackChannel {
    pub bot_token_source: String,
    pub app_token_source: String,
}

/// AgentMail (agentmail.to) inbox polled via its REST API. `inbox` selects a
/// specific inbox id; omitted uses the account's default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMailChannel {
    pub api_key_source: String,
    #[serde(default)]
    pub inbox: Option<String>,
}

/// A Model-Context-Protocol server the guest harness connects to. Rendered
/// into the harness's native MCP config (codex `config.toml`, claude
/// `.mcp.json`, …) at install time. Secrets in `env` are resolved host-side so
/// they never live in the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    #[serde(default)]
    pub transport: McpTransport,
    /// Program to spawn for a stdio server (e.g. `npx`).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Endpoint for an http/sse server.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment for the server process; each value resolved host-side from a
    /// `pipelock:`/`env:`/path source.
    #[serde(default)]
    pub env: Vec<McpEnvVar>,
    /// Hosts this server reaches; folded into the egress allowlist so the
    /// proxy permits them without a separate spec edit.
    #[serde(default)]
    pub egress_hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEnvVar {
    pub name: String,
    pub source: String,
}

/// Opt-in agent capabilities. Each gates an egress default + a deployed skill
/// so the agent can call the relevant provider through the pipelock proxy.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub image_gen: bool,
    #[serde(default)]
    pub voice: bool,
    /// Allow the in-guest agent to build and run its own sandboxed WebAssembly
    /// capabilities on the fly via `/session/forge` (the self-mutation runtime).
    /// Off by default: an agent can only self-forge when explicitly granted.
    #[serde(default)]
    pub self_forge: bool,
}

impl Capabilities {
    /// Egress hosts an enabled capability needs reachable through the pipelock
    /// proxy. The operator still injects the key via `network.proxy.inject_headers`
    /// — these are just the hostnames the guest must be allowed to reach.
    pub fn egress_defaults(&self) -> Vec<&'static str> {
        let mut hosts = Vec::new();
        if self.image_gen {
            hosts.push("api.openai.com");
        }
        hosts
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramChannel {
    pub token_source: String,
    #[serde(default)]
    pub chat_id_source: Option<String>,
}

/// Discord as a full two-way channel: a bot connected to the Discord Gateway
/// (WebSocket) for inbound messages and the REST API for replies. `bot_token`
/// is resolved host-side; the bot needs the MESSAGE CONTENT intent enabled in
/// the Developer Portal. (One-off outbound pings still use `maturana notify
/// discord --webhook-source ...`, which is separate from this channel.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordChannel {
    pub bot_token_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPolicy {
    #[serde(default)]
    pub on_launch: bool,
    #[serde(default)]
    pub retain: u8,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            on_launch: true,
            retain: 5,
        }
    }
}

impl AgentSpec {
    pub fn from_maturana_markdown(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        let frontmatter = extract_yaml_frontmatter(&raw)?;
        let mut spec: Self = serde_yaml::from_str(frontmatter)?;
        spec.apply_egress_defaults();
        Ok(spec)
    }

    /// Serialize this spec back to `MATURANA.md` form — YAML frontmatter plus a
    /// heading — the inverse of [`AgentSpec::from_maturana_markdown`]. Used when
    /// the orchestrator derives a spawned worker's spec and must write a
    /// `MATURANA.md` that the launcher and guest-worker install can re-read.
    pub fn to_maturana_markdown(&self) -> anyhow::Result<String> {
        let yaml = serde_yaml::to_string(self)?;
        Ok(format!("---\n{yaml}---\n\n# {}\n", self.identity.name))
    }

    /// Realize the egress defaults that opt-in capabilities promise (see
    /// [`Capabilities`]): e.g. `image_gen: true` reaches the OpenAI images API,
    /// so `api.openai.com` must be allowed through the egress proxy. Added only
    /// if the spec didn't already list it; the operator still supplies the key
    /// via a proxy `inject_headers` entry (the key never enters the guest), as
    /// the maturana-image-gen skill documents.
    fn apply_egress_defaults(&mut self) {
        let mut defaults: Vec<&'static str> = self.capabilities.egress_defaults();
        // The opencode harness fetches its model registry (context window, tool
        // support, pricing) from models.dev on every turn. When the proxy denies
        // it, opencode's HTTP client retries/backs off — seconds of dead latency
        // per turn — and never caches the result, so it repeats forever. Allow the
        // host so the lookup succeeds fast and is cached.
        if matches!(self.runtime.harness, HarnessRuntime::Opencode) {
            defaults.push("models.dev");
        }
        // Codex on a ChatGPT-account login refreshes its OAuth token via
        // auth.openai.com (chatgpt.com serves the API). A denied auth.openai.com
        // breaks the refresh, so once the token expires every turn fails with
        // "I hit an error while processing that message".
        if matches!(self.runtime.harness, HarnessRuntime::Codex) {
            defaults.push("auth.openai.com");
            defaults.push("chatgpt.com");
        }
        for host in defaults {
            if !self
                .network
                .egress_allowlist
                .iter()
                .any(|h| h.eq_ignore_ascii_case(host))
            {
                self.network.egress_allowlist.push(host.to_string());
            }
        }
    }
}

fn extract_yaml_frontmatter(raw: &str) -> anyhow::Result<&str> {
    let body = raw
        .strip_prefix("---\r\n")
        .or_else(|| raw.strip_prefix("---\n"))
        .ok_or_else(|| {
            anyhow::anyhow!("MATURANA.md must start with YAML front matter delimited by ---")
        })?;

    let end = body
        .find("\r\n---")
        .or_else(|| body.find("\n---"))
        .ok_or_else(|| anyhow::anyhow!("MATURANA.md front matter is missing closing ---"))?;

    let frontmatter = &body[..end];
    if frontmatter.trim().is_empty() {
        anyhow::bail!("MATURANA.md must start with YAML front matter delimited by ---");
    }
    Ok(frontmatter)
}

fn default_vcpu() -> u8 {
    2
}

fn default_memory_mib() -> u32 {
    2048
}

fn default_firecracker_tap() -> String {
    "tap-maturana0".to_string()
}

fn default_firecracker_host_ip() -> String {
    "172.30.0.1".to_string()
}

fn default_firecracker_guest_ip() -> String {
    "172.30.0.2".to_string()
}

fn default_firecracker_guest_mac() -> String {
    "AA:FC:00:00:00:01".to_string()
}

fn default_firecracker_kernel_args() -> String {
    "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5"
        .to_string()
}

fn default_true() -> bool {
    true
}

fn default_proxy_bind() -> String {
    "0.0.0.0:47833".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let raw = "---\nidentity:\n  id: demo\n  name: Demo\n  purpose: Test\nruntime:\n  harness: codex\nvm:\n  provider: hyper-v\n---\n# Demo\n";
        let frontmatter = extract_yaml_frontmatter(raw).unwrap();
        let spec: AgentSpec = serde_yaml::from_str(frontmatter).unwrap();
        assert_eq!(spec.identity.id, "demo");
        assert_eq!(spec.runtime.harness, HarnessRuntime::Codex);
    }

    #[test]
    fn proxy_block_survives_markdown_round_trip() {
        // Egress-proxy outage class: a spec with `network.proxy` must round-trip
        // through to_maturana_markdown -> from_maturana_markdown WITHOUT losing the
        // block (Network::proxy has no `skip_serializing_if`). If this regresses, a
        // regenerated spec silently drops the proxy and the guest gets ConnectionRefused.
        let yaml = r#"
identity: { id: fc, name: FC, purpose: Firecracker agent with a bounded policy. }
runtime: { harness: claude-code }
vm:
  provider: firecracker
  guest_os: linux
  firecracker:
    kernel_image: img/vmlinux.bin
    rootfs_image: img/rootfs.ext4
    tap_name: tap-mat-fc
    host_ip: 172.30.10.9
    guest_ip: 172.30.10.10
    guest_mac: AA:FC:00:00:10:03
network:
  egress_allowlist: [api.anthropic.com]
  proxy:
    enabled: true
    bind: 172.30.10.9:47833
"#;
        let spec: AgentSpec = serde_yaml::from_str(yaml).unwrap();
        let markdown = spec.to_maturana_markdown().unwrap();
        assert!(
            markdown.contains("proxy:") && markdown.contains("172.30.10.9:47833"),
            "serialized spec must keep the proxy block:\n{markdown}"
        );
        let tmp = std::env::temp_dir().join(format!("mat-spec-rt-{}.md", std::process::id()));
        std::fs::write(&tmp, &markdown).unwrap();
        let reparsed = AgentSpec::from_maturana_markdown(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        let proxy = reparsed
            .network
            .proxy
            .expect("proxy block must survive the markdown round-trip");
        assert!(proxy.enabled);
        assert_eq!(proxy.bind, "172.30.10.9:47833");
    }

    #[test]
    fn mcp_servers_parse_stdio_and_http() {
        let yaml = r#"
identity: { id: demo, name: Demo, purpose: Test agent for MCP. }
runtime: { harness: claude-code }
vm: { provider: firecracker, guest_os: linux }
mcp_servers:
  - name: notion
    transport: stdio
    command: npx
    args: ["-y", "@notionhq/notion-mcp-server"]
    env: [{ name: NOTION_TOKEN, source: "pipelock:notion/integration-token" }]
    egress_hosts: ["api.notion.com"]
  - name: remote
    transport: http
    url: "https://mcp.example.com/sse"
"#;
        let spec: AgentSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.mcp_servers.len(), 2);
        let notion = &spec.mcp_servers[0];
        assert_eq!(notion.name, "notion");
        assert_eq!(notion.transport, McpTransport::Stdio);
        assert_eq!(notion.command.as_deref(), Some("npx"));
        assert_eq!(notion.env[0].source, "pipelock:notion/integration-token");
        assert_eq!(notion.egress_hosts, vec!["api.notion.com"]);
        assert_eq!(spec.mcp_servers[1].transport, McpTransport::Http);
        // Transport defaults to stdio when omitted.
        let bare: McpServer =
            serde_yaml::from_str("name: x\ncommand: run").unwrap();
        assert_eq!(bare.transport, McpTransport::Stdio);
    }

    #[test]
    fn channels_parse_slack_and_agentmail_and_capabilities() {
        let yaml = r#"
identity: { id: demo, name: Demo, purpose: Test agent for channels. }
runtime: { harness: codex }
vm: { provider: firecracker, guest_os: linux }
capabilities: { image_gen: true, voice: true }
channels:
  slack: { bot_token_source: "pipelock:slack/bot-token", app_token_source: "pipelock:slack/app-token" }
  agentmail: { api_key_source: "pipelock:agentmail/api-key", inbox: "agent@agentmail.to" }
"#;
        let spec: AgentSpec = serde_yaml::from_str(yaml).unwrap();
        assert!(spec.capabilities.image_gen && spec.capabilities.voice);
        let slack = spec.channels.slack.unwrap();
        assert_eq!(slack.bot_token_source, "pipelock:slack/bot-token");
        let mail = spec.channels.agentmail.unwrap();
        assert_eq!(mail.inbox.as_deref(), Some("agent@agentmail.to"));
    }

    #[test]
    fn image_gen_capability_adds_openai_egress_default() {
        // The capability declares the host it needs reachable…
        let caps = Capabilities {
            image_gen: true,
            ..Default::default()
        };
        assert!(caps.egress_defaults().contains(&"api.openai.com"));

        // …and loading a spec realizes it on the egress allowlist.
        let dir = std::env::temp_dir().join("maturana-spec-imagegen-egress");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("MATURANA.md");
        std::fs::write(
            &path,
            "---\nidentity: { id: i, name: N, purpose: P }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux }\ncapabilities: { image_gen: true }\n---\n# body\n",
        )
        .unwrap();
        let spec = AgentSpec::from_maturana_markdown(&path).unwrap();
        assert!(spec
            .network
            .egress_allowlist
            .iter()
            .any(|h| h == "api.openai.com"));

        // Off by default: no capability → no injected host.
        std::fs::write(
            &path,
            "---\nidentity: { id: i, name: N, purpose: P }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux }\n---\n# body\n",
        )
        .unwrap();
        let plain = AgentSpec::from_maturana_markdown(&path).unwrap();
        assert!(!plain
            .network
            .egress_allowlist
            .iter()
            .any(|h| h == "api.openai.com"));
    }

    #[test]
    fn opencode_harness_adds_models_dev_egress_default() {
        // opencode fetches its model registry from models.dev every turn; the
        // proxy must allow it or each turn pays a denied-fetch retry stall.
        let dir = std::env::temp_dir().join("maturana-spec-opencode-modelsdev");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("MATURANA.md");
        std::fs::write(
            &path,
            "---\nidentity: { id: i, name: N, purpose: P }\nruntime: { harness: opencode }\nvm: { provider: firecracker, guest_os: linux }\n---\n# body\n",
        )
        .unwrap();
        let spec = AgentSpec::from_maturana_markdown(&path).unwrap();
        assert!(spec.network.egress_allowlist.iter().any(|h| h == "models.dev"));

        // Other harnesses don't get it (they don't use models.dev).
        std::fs::write(
            &path,
            "---\nidentity: { id: i, name: N, purpose: P }\nruntime: { harness: codex }\nvm: { provider: firecracker, guest_os: linux }\n---\n# body\n",
        )
        .unwrap();
        let codex = AgentSpec::from_maturana_markdown(&path).unwrap();
        assert!(!codex.network.egress_allowlist.iter().any(|h| h == "models.dev"));
    }
}
