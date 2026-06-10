//! Host-plane orchestration plan.
//!
//! Maturana's runtime plane is a set of long-lived host processes: the session
//! queue server (`sessiond`), one Telegram channel bridge per agent, and one
//! schedule runner per agent. Historically an operator had to start each of
//! these by hand and keep their `--session-id`, bind address, and token in
//! sync. A single mismatch (for example the channel writing to `telegram-main`
//! while the guest worker claims from `default`) silently breaks the agent: the
//! message is enqueued to one queue and claimed from another, so no reply is
//! ever produced.
//!
//! This module turns one declarative [`OrchestratorConfig`] into the exact set
//! of [`SupervisedProcess`] commands, deriving every cross-process value from a
//! single source of truth. The CLI `maturana up` command supervises the plan;
//! the guest-worker installer reads [`guest_session_id`] so the worker always
//! claims from the same queue the channel writes to.

use serde::{Deserialize, Serialize};

/// Default session id shared by the Telegram channel bridge and the guest
/// worker for an agent. Exposed so the guest-worker installer cannot drift.
pub fn guest_session_id(agent: &AgentRuntime) -> String {
    agent.session_id.clone()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Address `sessiond` binds to. Guests reach it on the host gateway IP.
    pub sessiond_bind: String,
    /// Optional shared bearer token required on every non-health sessiond call.
    pub sessiond_token: Option<String>,
    pub channel_poll_seconds: u64,
    pub schedule_poll_seconds: u64,
    pub agents: Vec<AgentRuntime>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            sessiond_bind: "0.0.0.0:47834".to_string(),
            sessiond_token: None,
            channel_poll_seconds: 5,
            schedule_poll_seconds: 60,
            agents: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRuntime {
    pub agent_id: String,
    /// The one session id used by both the channel bridge and the guest worker.
    pub session_id: String,
    pub telegram: bool,
    pub telegram_token_source: String,
    pub schedules: bool,
}

impl AgentRuntime {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            session_id: "telegram-main".to_string(),
            telegram: true,
            telegram_token_source: "pipelock:telegram/bot-token".to_string(),
            schedules: true,
        }
    }
}

/// One supervised child process: a name for logs/health, the `maturana`
/// sub-command arguments, and whether its failure should take the plane down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisedProcess {
    pub name: String,
    pub args: Vec<String>,
    pub critical: bool,
}

/// Build the full, internally-consistent process plan for the runtime plane.
///
/// `sessiond` is emitted first and marked critical (everything depends on it).
/// Each agent contributes a Telegram bridge and/or a schedule runner, all
/// pinned to the agent's single `session_id`.
pub fn plan_processes(config: &OrchestratorConfig) -> Vec<SupervisedProcess> {
    let mut processes = Vec::new();

    let mut sessiond_args = vec![
        "session".to_string(),
        "serve".to_string(),
        "--bind".to_string(),
        config.sessiond_bind.clone(),
    ];
    if let Some(token) = &config.sessiond_token {
        sessiond_args.push("--token".to_string());
        sessiond_args.push(token.clone());
    }
    processes.push(SupervisedProcess {
        name: "sessiond".to_string(),
        args: sessiond_args,
        critical: true,
    });

    for agent in &config.agents {
        if agent.telegram {
            processes.push(SupervisedProcess {
                name: format!("channel:telegram:{}", agent.agent_id),
                args: vec![
                    "channel".to_string(),
                    "serve".to_string(),
                    "telegram".to_string(),
                    "--agent-id".to_string(),
                    agent.agent_id.clone(),
                    "--session-id".to_string(),
                    agent.session_id.clone(),
                    "--token-source".to_string(),
                    agent.telegram_token_source.clone(),
                    "--poll-seconds".to_string(),
                    config.channel_poll_seconds.to_string(),
                ],
                critical: false,
            });
        }
        if agent.schedules {
            processes.push(SupervisedProcess {
                name: format!("schedule:{}", agent.agent_id),
                args: vec![
                    "schedule".to_string(),
                    "serve".to_string(),
                    agent.agent_id.clone(),
                    "--session-id".to_string(),
                    agent.session_id.clone(),
                    "--poll-seconds".to_string(),
                    config.schedule_poll_seconds.to_string(),
                ],
                critical: false,
            });
        }
    }

    processes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_and_schedule_share_one_session_id() {
        let mut config = OrchestratorConfig::default();
        config.sessiond_token = Some("tok".to_string());
        let mut agent = AgentRuntime::new("personal");
        agent.session_id = "telegram-main".to_string();
        config.agents.push(agent.clone());

        let processes = plan_processes(&config);
        // sessiond + telegram + schedule
        assert_eq!(processes.len(), 3);
        assert_eq!(processes[0].name, "sessiond");
        assert!(processes[0].critical);
        assert!(processes[0].args.contains(&"--token".to_string()));

        let channel = &processes[1];
        let schedule = &processes[2];
        let channel_session = session_id_arg(&channel.args);
        let schedule_session = session_id_arg(&schedule.args);
        // The whole point: these can never drift apart.
        assert_eq!(channel_session, schedule_session);
        assert_eq!(channel_session.as_deref(), Some("telegram-main"));
        assert_eq!(guest_session_id(&agent), "telegram-main");
    }

    #[test]
    fn disabled_channels_are_omitted() {
        let mut config = OrchestratorConfig::default();
        let mut agent = AgentRuntime::new("worker");
        agent.telegram = false;
        agent.schedules = true;
        config.agents.push(agent);

        let processes = plan_processes(&config);
        assert_eq!(processes.len(), 2);
        assert!(processes.iter().all(|p| !p.name.starts_with("channel:")));
        assert!(processes.iter().any(|p| p.name == "schedule:worker"));
    }

    fn session_id_arg(args: &[String]) -> Option<String> {
        args.iter()
            .position(|arg| arg == "--session-id")
            .and_then(|index| args.get(index + 1))
            .cloned()
    }
}
