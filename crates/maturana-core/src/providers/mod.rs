pub mod firecracker;
pub mod hyperv;

use crate::spec::AgentSpec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCommand {
    pub program: String,
    pub args: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAgentStatus {
    pub provider: String,
    pub state: String,
    pub vm_name: Option<String>,
    pub pid: Option<u32>,
    pub ipv4: Option<String>,
    pub uptime: Option<String>,
    pub socket_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub metadata_path: Option<PathBuf>,
    pub metrics_tail: Vec<String>,
}

pub trait Provider {
    fn plan_launch(
        &self,
        spec: &AgentSpec,
        agent_dir: &Path,
    ) -> anyhow::Result<Vec<ProviderCommand>>;

    fn launch(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()>;

    fn stop(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()>;

    fn inspect(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<LiveAgentStatus>;
}
