use std::{fs, path::PathBuf, process::Command, time::SystemTime};

use maturana_core::{pipelock::PipelockVault, state::MaturanaHome};
use serde::Serialize;

use crate::hostd::{hostd_status, hostd_vms};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub home: String,
    pub hostd: DoctorCheck,
    pub sessiond: DoctorCheck,
    pub agents: Vec<DoctorAgentReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorAgentReport {
    pub agent_id: String,
    pub vm: DoctorCheck,
    pub telegram: DoctorCheck,
    pub guest_worker: DoctorCheck,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub ok: bool,
    pub message: String,
}

pub fn build_report(home: &MaturanaHome, agent_ids: &[String], sessiond_url: &str) -> DoctorReport {
    let agent_ids = if agent_ids.is_empty() {
        crate::agents::list_agent_ids(home).unwrap_or_default()
    } else {
        agent_ids.to_vec()
    };
    let hostd = doctor_hostd();
    let vms = doctor_vms().unwrap_or_default();
    let sessiond = doctor_http_health(&format!("{}/health", sessiond_url.trim_end_matches('/')));
    let agents = agent_ids
        .iter()
        .map(|agent_id| doctor_agent(home, agent_id, &vms))
        .collect::<Vec<_>>();
    let ok = hostd.ok
        && sessiond.ok
        && agents
            .iter()
            .all(|agent| agent.vm.ok && agent.telegram.ok && agent.guest_worker.ok);
    DoctorReport {
        ok,
        home: home.root().display().to_string(),
        hostd,
        sessiond,
        agents,
    }
}

fn doctor_hostd() -> DoctorCheck {
    match hostd_status() {
        Ok(status) if status.reachable => DoctorCheck {
            ok: true,
            message: status.url,
        },
        Ok(status) => DoctorCheck {
            ok: false,
            message: status.error.unwrap_or(status.url),
        },
        Err(error) => DoctorCheck {
            ok: false,
            message: error.to_string(),
        },
    }
}

fn doctor_http_health(url: &str) -> DoctorCheck {
    let check = crate::health::http_health(url);
    DoctorCheck {
        ok: check.ok,
        message: check.message,
    }
}

fn doctor_vms() -> anyhow::Result<Vec<serde_json::Value>> {
    hostd_vms()
}

fn doctor_agent(
    home: &MaturanaHome,
    agent_id: &str,
    vms: &[serde_json::Value],
) -> DoctorAgentReport {
    DoctorAgentReport {
        agent_id: agent_id.to_string(),
        vm: doctor_agent_vm(agent_id, vms),
        telegram: doctor_agent_telegram(home, agent_id),
        guest_worker: doctor_guest_worker(home, agent_id),
    }
}

fn doctor_agent_vm(agent_id: &str, vms: &[serde_json::Value]) -> DoctorCheck {
    let expected_name = format!("maturana-{agent_id}");
    let Some(vm) = vms.iter().find(|vm| {
        vm.get("name")
            .and_then(|value| value.as_str())
            .map(|name| name == expected_name)
            .unwrap_or(false)
    }) else {
        return DoctorCheck {
            ok: false,
            message: format!("{expected_name} not found"),
        };
    };
    let state = vm
        .get("state")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ip = vm
        .get("ipv4")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ok = state.eq_ignore_ascii_case("Running") && !ip.trim().is_empty();
    DoctorCheck {
        ok,
        message: format!("state={state} ipv4={ip}"),
    }
}

fn doctor_agent_telegram(home: &MaturanaHome, agent_id: &str) -> DoctorCheck {
    let vault = PipelockVault::new(home.pipelock_dir());
    let paired = vault.get(&format!("telegram/{agent_id}/chat-id")).is_ok()
        || vault.get("telegram/chat-id").is_ok();
    let pid_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/runner.pid");
    let pid = read_pid(&pid_path);
    let pid_alive = pid.map(process_alive).unwrap_or(false);
    let state_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/state.json");
    let state_age = file_age_seconds(&state_path);
    let heartbeat_path = home
        .agent_dir(agent_id)
        .join("channels/telegram/heartbeat.json");
    let heartbeat_age = file_age_seconds(&heartbeat_path);
    let heartbeat_ok = heartbeat_age.map(|age| age <= 30).unwrap_or(false);
    let ok = paired && (pid_alive || heartbeat_ok);
    DoctorCheck {
        ok,
        message: format!(
            "paired={paired} pid={} pid_alive={pid_alive} state_age_s={} heartbeat_age_s={}",
            pid.map(|value| value.to_string()).unwrap_or_default(),
            state_age
                .map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string()),
            heartbeat_age
                .map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string())
        ),
    }
}

fn doctor_guest_worker(home: &MaturanaHome, agent_id: &str) -> DoctorCheck {
    let path = home.agent_dir(agent_id).join("worker-status.json");
    let age = file_age_seconds(&path);
    let payload = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let status = payload
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let ok = age.map(|value| value <= 60).unwrap_or(false)
        && (status.is_empty() || status == "idle" || status == "completed" || status == "claimed");
    DoctorCheck {
        ok,
        message: format!(
            "status={} age_s={}",
            if status.is_empty() { "unknown" } else { status },
            age.map(|value| value.to_string())
                .unwrap_or_else(|| "missing".to_string())
        ),
    }
}

fn read_pid(path: &PathBuf) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()
}

fn file_age_seconds(path: &PathBuf) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age.as_secs())
}

fn process_alive(pid: u32) -> bool {
    process_alive_impl(pid).unwrap_or(false)
}

#[cfg(windows)]
fn process_alive_impl(pid: u32) -> anyhow::Result<bool> {
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "try {{ Get-Process -Id {pid} -ErrorAction Stop | Out-Null; exit 0 }} catch {{ exit 1 }}"
            ),
        ])
        .status()?;
    Ok(status.success())
}

#[cfg(not(windows))]
fn process_alive_impl(pid: u32) -> anyhow::Result<bool> {
    let status = Command::new("sh")
        .args(["-c", &format!("kill -0 {pid} >/dev/null 2>&1")])
        .status()?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_without_agents_preserves_shape() {
        let root =
            std::env::temp_dir().join(format!("maturana-ops-doctor-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = MaturanaHome::new(root.join(".maturana"));

        let report = build_report(&home, &[], "http://127.0.0.1:47834");
        assert_eq!(report.home, home.root().display().to_string());
        assert!(report.agents.is_empty());
        assert!(!report.ok);

        let _ = fs::remove_dir_all(root);
    }
}
