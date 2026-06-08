pub mod firecracker;
pub mod hyperv;

use crate::spec::AgentSpec;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCommand {
    pub program: String,
    pub args: Vec<String>,
    pub description: String,
}

pub trait Provider {
    fn plan_launch(
        &self,
        spec: &AgentSpec,
        agent_dir: &Path,
    ) -> anyhow::Result<Vec<ProviderCommand>>;

    fn launch(&self, spec: &AgentSpec, agent_dir: &Path) -> anyhow::Result<()>;
}
