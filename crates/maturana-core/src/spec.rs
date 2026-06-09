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
    pub browser: Browser,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub name: String,
    pub cron: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Channels {
    #[serde(default)]
    pub tui: bool,
    #[serde(default)]
    pub telegram: Option<TelegramChannel>,
    #[serde(default)]
    pub discord: Option<DiscordChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramChannel {
    pub token_source: String,
    #[serde(default)]
    pub chat_id_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordChannel {
    pub webhook_source: String,
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
        Ok(serde_yaml::from_str(frontmatter)?)
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
}
