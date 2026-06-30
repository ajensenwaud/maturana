use std::path::{Path, PathBuf};

use chrono::Utc;
use maturana_core::audit::{append_event, AuditEvent};
use maturana_core::state::MaturanaHome;

use crate::ssh::{
    copy_path_to_guest, run_ssh_with_stdin, shell_quote, GuestHostKey, SSH_TIMEOUT_QUICK,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployKind {
    Skill,
    Tool,
}

#[derive(Debug, Clone)]
pub struct DeployRequest {
    pub agent_id: String,
    pub path: PathBuf,
    pub ip: String,
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    pub guest_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployResult {
    pub agent_id: String,
    pub local_path: PathBuf,
    pub guest_path: String,
}

pub fn deploy_item(
    home: &MaturanaHome,
    kind: DeployKind,
    request: DeployRequest,
) -> anyhow::Result<DeployResult> {
    if !request.path.exists() {
        anyhow::bail!("deploy path does not exist: {}", request.path.display());
    }
    let guest_path = guest_path_for(kind, &request.path, request.guest_path.as_deref())?;
    let base = base_guest_dir(kind);
    let parent = guest_path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .unwrap_or(base);

    // Verify the guest host key (strict if pinned for this agent, else accept-new)
    // so a deploy cannot push skills/tools to an impostor.
    let host_key = GuestHostKey::resolve(home, &request.agent_id, &request.ip)?;

    run_ssh_with_stdin(
        &request.ip,
        &request.ssh_user,
        &request.ssh_key,
        &host_key,
        &format!("mkdir -p {}", shell_quote(parent)),
        None,
        SSH_TIMEOUT_QUICK,
    )?;
    copy_path_to_guest(
        &request.ip,
        &request.ssh_user,
        &request.ssh_key,
        &host_key,
        &request.path,
        &guest_path,
        request.path.is_dir(),
    )?;
    append_event(
        home.audit_dir().join(format!("{}.jsonl", request.agent_id)),
        &AuditEvent {
            at: Utc::now(),
            agent_id: request.agent_id.clone(),
            action: format!("deploy.{}", deploy_kind_name(kind)),
            message: format!("deployed {} to {}", request.path.display(), guest_path),
        },
    )?;

    Ok(DeployResult {
        agent_id: request.agent_id,
        local_path: request.path,
        guest_path,
    })
}

fn base_guest_dir(kind: DeployKind) -> &'static str {
    match kind {
        DeployKind::Skill => "/agent/skills",
        DeployKind::Tool => "/agent/tools",
    }
}

fn deploy_kind_name(kind: DeployKind) -> &'static str {
    match kind {
        DeployKind::Skill => "skill",
        DeployKind::Tool => "tool",
    }
}

fn guest_path_for(
    kind: DeployKind,
    local_path: &Path,
    override_path: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(path) = override_path {
        let path = path.trim();
        if path.is_empty() {
            anyhow::bail!("guest path must not be empty");
        }
        return Ok(path.to_string());
    }
    let name = local_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("deploy path has no file name"))?;
    Ok(format!("{}/{}", base_guest_dir(kind), name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_path_defaults_by_kind() {
        assert_eq!(
            guest_path_for(DeployKind::Skill, Path::new("skills/demo"), None).unwrap(),
            "/agent/skills/demo"
        );
        assert_eq!(
            guest_path_for(DeployKind::Tool, Path::new("tools/runner"), None).unwrap(),
            "/agent/tools/runner"
        );
    }

    #[test]
    fn guest_path_override_is_trimmed_and_required() {
        assert_eq!(
            guest_path_for(
                DeployKind::Skill,
                Path::new("skills/demo"),
                Some(" /custom/demo ")
            )
            .unwrap(),
            "/custom/demo"
        );
        assert!(guest_path_for(DeployKind::Skill, Path::new("skills/demo"), Some(" ")).is_err());
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/tmp/it's-here"), "'/tmp/it'\"'\"'s-here'");
    }
}
