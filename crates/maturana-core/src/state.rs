use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct MaturanaHome {
    root: PathBuf,
}

impl MaturanaHome {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn default_for_cwd(cwd: impl AsRef<Path>) -> Self {
        Self::new(cwd.as_ref().join(".maturana"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    pub fn agent_dir(&self, id: &str) -> PathBuf {
        self.agents_dir().join(id)
    }

    pub fn audit_dir(&self) -> PathBuf {
        self.root.join("audit")
    }

    pub fn rooms_dir(&self) -> PathBuf {
        self.root.join("rooms")
    }

    pub fn room_dir(&self, id: &str) -> PathBuf {
        self.rooms_dir().join(id)
    }

    pub fn pipelock_dir(&self) -> PathBuf {
        self.root.join("pipelock")
    }
}
