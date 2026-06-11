//! Host service registration: `maturana service install|uninstall|status|restart`.
//!
//! Rust owns the lifecycle (docs/script-boundary.md); the install scripts are
//! thin leaf adapters that call this. Linux registers systemd **user** units
//! (mention `loginctl enable-linger` for boot-time start); Windows registers
//! Scheduled Tasks following the hostd pattern.

use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::state::MaturanaHome;

#[derive(Debug, Args)]
pub struct ServiceCommand {
    #[command(subcommand)]
    pub command: ServiceSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceSubcommand {
    /// Register and start host services (default: up + web).
    Install {
        #[arg(default_values_t = vec!["up".to_string(), "web".to_string()])]
        services: Vec<String>,
    },
    /// Stop and unregister host services.
    Uninstall {
        #[arg(default_values_t = vec!["up".to_string(), "web".to_string()])]
        services: Vec<String>,
    },
    /// Show registration/run state.
    Status {
        #[arg(default_values_t = vec!["up".to_string(), "web".to_string()])]
        services: Vec<String>,
    },
    /// Restart registered services.
    Restart {
        #[arg(default_values_t = vec!["up".to_string(), "web".to_string()])]
        services: Vec<String>,
    },
}

/// A registrable host service: the maturana subcommand it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct HostService {
    pub name: &'static str,
    pub args: Vec<String>,
    pub description: &'static str,
}

pub fn known_service(name: &str) -> anyhow::Result<HostService> {
    match name {
        "up" => Ok(HostService {
            name: "up",
            args: vec!["up".to_string()],
            description: "Maturana runtime plane (sessiond + graph + channels + schedules)",
        }),
        "web" => Ok(HostService {
            name: "web",
            args: vec!["web".to_string()],
            description: "Maturana web cockpit",
        }),
        other => anyhow::bail!("unknown service: {other} (up|web)"),
    }
}

pub fn handle_service(command: ServiceCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    let (action, names): (&str, Vec<String>) = match command.command {
        ServiceSubcommand::Install { services } => ("install", services),
        ServiceSubcommand::Uninstall { services } => ("uninstall", services),
        ServiceSubcommand::Status { services } => ("status", services),
        ServiceSubcommand::Restart { services } => ("restart", services),
    };
    let exe = std::env::current_exe().context("failed to resolve maturana executable")?;
    for name in names {
        let service = known_service(&name)?;
        if cfg!(windows) {
            windows_service(action, &service, &exe, home)?;
        } else {
            linux_service(action, &service, &exe, home)?;
        }
    }
    if action == "install" && !cfg!(windows) {
        println!("note: run `loginctl enable-linger $USER` so user services start at boot");
    }
    Ok(())
}

/// Render the systemd user unit for a host service.
pub fn render_host_systemd_unit(service: &HostService, exe: &str, home_root: &str) -> String {
    format!(
        "[Unit]\nDescription={}\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={} --home {} {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        service.description,
        exe,
        home_root,
        service.args.join(" "),
    )
}

fn unit_name(service: &HostService) -> String {
    format!("maturana-{}.service", service.name)
}

fn linux_service(
    action: &str,
    service: &HostService,
    exe: &std::path::Path,
    home: &MaturanaHome,
) -> anyhow::Result<()> {
    let unit = unit_name(service);
    let unit_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?
        .join(".config/systemd/user");
    let unit_path = unit_dir.join(&unit);
    match action {
        "install" => {
            std::fs::create_dir_all(&unit_dir)?;
            std::fs::write(
                &unit_path,
                render_host_systemd_unit(
                    service,
                    &exe.display().to_string(),
                    &home.root().display().to_string(),
                ),
            )?;
            systemctl_user(&["daemon-reload"])?;
            systemctl_user(&["enable", "--now", &unit])?;
            println!("installed + started {unit}");
        }
        "uninstall" => {
            let _ = systemctl_user(&["disable", "--now", &unit]);
            if unit_path.exists() {
                std::fs::remove_file(&unit_path)?;
            }
            let _ = systemctl_user(&["daemon-reload"]);
            println!("uninstalled {unit}");
        }
        "status" => {
            let registered = unit_path.exists();
            let active = ProcessCommand::new("systemctl")
                .args(["--user", "is-active", &unit])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            println!("{}: registered={registered} state={active}", service.name);
        }
        "restart" => {
            systemctl_user(&["restart", &unit])?;
            println!("restarted {unit}");
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn systemctl_user(args: &[&str]) -> anyhow::Result<()> {
    let status = ProcessCommand::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl")?;
    if !status.success() {
        anyhow::bail!("systemctl --user {} failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Scheduled-task name, matching the MaturanaHostd convention.
fn task_name(service: &HostService) -> String {
    let mut chars = service.name.chars();
    let capitalized = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    format!("Maturana{capitalized}")
}

/// PowerShell command that registers the logon task. Register-ScheduledTask
/// (unlike `schtasks /Create /SC ONLOGON`) works WITHOUT elevation when the
/// trigger is scoped to the calling user; the privileged hostd keeps its
/// dedicated elevated installer.
pub fn windows_register_command(service: &HostService, exe: &str, home_root: &str) -> String {
    let task = task_name(service);
    let exe = exe.replace('\'', "''");
    let argument = format!("--home \"{home_root}\" {}", service.args.join(" ")).replace('\'', "''");
    format!(
        "$a = New-ScheduledTaskAction -Execute '{exe}' -Argument '{argument}'; \
         $t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME; \
         Register-ScheduledTask -TaskName '{task}' -Action $a -Trigger $t -Force | Out-Null"
    )
}

/// Argv for schtasks (after the program name); split out for testing.
/// Install goes through [`windows_register_command`] instead.
pub fn schtasks_args(action: &str, service: &HostService) -> Vec<String> {
    let task = task_name(service);
    match action {
        "uninstall" => vec!["/Delete".into(), "/F".into(), "/TN".into(), task],
        "status" => vec!["/Query".into(), "/TN".into(), task],
        "restart" => vec!["/Run".into(), "/TN".into(), task],
        _ => unreachable!(),
    }
}

fn windows_service(
    action: &str,
    service: &HostService,
    exe: &std::path::Path,
    home: &MaturanaHome,
) -> anyhow::Result<()> {
    if action == "install" {
        let command = windows_register_command(
            service,
            &exe.display().to_string(),
            &home.root().display().to_string(),
        );
        let status = ProcessCommand::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .status()
            .context("failed to run powershell")?;
        if !status.success() {
            anyhow::bail!("Register-ScheduledTask failed with {status}");
        }
        // Start it now rather than waiting for the next logon.
        let _ = ProcessCommand::new("schtasks")
            .args(["/Run", "/TN", &task_name(service)])
            .status();
        println!("registered + started task {}", task_name(service));
        return Ok(());
    }
    if action == "restart" {
        // End first; ignore failure (task may not be running). Give the old
        // instance a moment to release its port or /Run races the teardown
        // and the task lands back in Ready without a live process.
        let _ = ProcessCommand::new("schtasks")
            .args(["/End", "/TN", &task_name(service)])
            .status();
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let args = schtasks_args(action, service);
    let status = ProcessCommand::new("schtasks")
        .args(&args)
        .status()
        .context("failed to run schtasks")?;
    if !status.success() && action != "status" {
        anyhow::bail!("schtasks {} failed with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_golden() {
        let unit = render_host_systemd_unit(
            &known_service("web").unwrap(),
            "/usr/local/bin/maturana",
            "/home/aj/maturana/.maturana",
        );
        assert_eq!(
            unit,
            "[Unit]\nDescription=Maturana web cockpit\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart=/usr/local/bin/maturana --home /home/aj/maturana/.maturana web\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
        );
        let up = render_host_systemd_unit(
            &known_service("up").unwrap(),
            "/usr/local/bin/maturana",
            "/h/.maturana",
        );
        assert!(up.contains("ExecStart=/usr/local/bin/maturana --home /h/.maturana up"));
        assert!(up.contains("Restart=on-failure"));
    }

    #[test]
    fn windows_commands_golden() {
        let service = known_service("up").unwrap();
        let register =
            windows_register_command(&service, r"C:\m\maturana.exe", r"C:\m\.maturana");
        assert!(register.contains("New-ScheduledTaskAction -Execute 'C:\\m\\maturana.exe'"));
        assert!(register.contains(r#"-Argument '--home "C:\m\.maturana" up'"#));
        assert!(register.contains("-AtLogOn -User $env:USERNAME"));
        assert!(register.contains("-TaskName 'MaturanaUp'"));
        assert_eq!(
            schtasks_args("uninstall", &service),
            vec!["/Delete", "/F", "/TN", "MaturanaUp"]
        );
        assert_eq!(
            schtasks_args("restart", &service),
            vec!["/Run", "/TN", "MaturanaUp"]
        );
        assert_eq!(
            schtasks_args("status", &service),
            vec!["/Query", "/TN", "MaturanaUp"]
        );
    }

    #[test]
    fn unknown_services_are_rejected() {
        assert!(known_service("hostd").is_err()); // hostd keeps its dedicated installer
        assert!(known_service("bogus").is_err());
        assert_eq!(known_service("web").unwrap().name, "web");
    }
}
