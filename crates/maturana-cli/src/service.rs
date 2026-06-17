//! Host service registration: `maturana service install|uninstall|status|restart`.
//!
//! Rust owns the lifecycle (docs/script-boundary.md); the install scripts are
//! thin leaf adapters that call this. Linux registers systemd **user** units
//! (mention `loginctl enable-linger` for boot-time start); Windows registers
//! Scheduled Tasks following the hostd pattern.

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
    /// Register and start host services (default: up). The `web` cockpit is
    /// experimental and opt-in until stabilized — install it explicitly with
    /// `maturana service install web` to try it.
    Install {
        #[arg(default_values_t = vec!["up".to_string()])]
        services: Vec<String>,
        /// Windows only: the current user's Windows password. Registers the
        /// task with logon type Password so it runs at boot in the user profile
        /// (where codex/claude auth lives) WITHOUT an interactive login. Stored
        /// by Windows in the LSA credential vault, never written to disk/XML.
        /// install.ps1 prompts for this; pass it explicitly only for scripting.
        #[arg(long = "windows-password")]
        windows_password: Option<String>,
    },
    /// Stop and unregister host services.
    Uninstall {
        #[arg(default_values_t = vec!["up".to_string()])]
        services: Vec<String>,
    },
    /// Show registration/run state.
    Status {
        #[arg(default_values_t = vec!["up".to_string()])]
        services: Vec<String>,
    },
    /// Restart registered services.
    Restart {
        #[arg(default_values_t = vec!["up".to_string()])]
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
        // Opt-in (not in the default install vec): relaunches the Firecracker
        // microVMs at boot. --skip-services because the systemd `maturana-up`
        // unit owns sessiond/graph; --skip-assets to reuse the baked rootfs
        // (no libguestfs rebuild); --skip-worker-refresh because the guest's
        // baked, enabled maturana-agent.service self-recovers the worker, so
        // there's no need to SSH in and reinstall it (which would also block
        // boot on a slow guest). The TAP is recreated regardless (it's
        // ephemeral), and the un-baked guard makes this a clean no-op on hosts
        // with no images yet.
        "fleet" => Ok(HostService {
            name: "fleet",
            args: vec![
                "repair".to_string(),
                "firecracker-harnesses".to_string(),
                "--skip-services".to_string(),
                "--skip-assets".to_string(),
                "--skip-worker-refresh".to_string(),
            ],
            description: "Maturana Firecracker fleet (relaunch microVMs at boot)",
        }),
        other => anyhow::bail!("unknown service: {other} (up|web|fleet)"),
    }
}

pub fn handle_service(command: ServiceCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    let mut windows_password: Option<String> = None;
    let (action, names): (&str, Vec<String>) = match command.command {
        ServiceSubcommand::Install {
            services,
            windows_password: pw,
        } => {
            windows_password = pw;
            ("install", services)
        }
        ServiceSubcommand::Uninstall { services } => ("uninstall", services),
        ServiceSubcommand::Status { services } => ("status", services),
        ServiceSubcommand::Restart { services } => ("restart", services),
    };
    let exe = std::env::current_exe().context("failed to resolve maturana executable")?;
    for name in names {
        let service = known_service(&name)?;
        if cfg!(windows) {
            windows_service(action, &service, &exe, home, windows_password.as_deref())?;
        } else {
            linux_service(action, &service, &exe, home)?;
        }
    }
    if action == "install" && !cfg!(windows) {
        // Linger makes the user manager (and its enabled units) start at boot
        // without an interactive login — the crux of zero-touch reboot
        // recovery. Best-effort: print a hint if it fails (e.g. no loginctl).
        match enable_linger() {
            Ok(()) => println!("enabled linger so user services start at boot"),
            Err(err) => println!(
                "note: could not auto-enable linger ({err}); run `loginctl enable-linger $USER`"
            ),
        }
    }
    Ok(())
}

/// Enable systemd linger for the current user so user units start at boot
/// without a login. Idempotent.
fn enable_linger() -> anyhow::Result<()> {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .context("cannot resolve current user for linger")?;
    let status = ProcessCommand::new("loginctl")
        .args(["enable-linger", &user])
        .status()
        .context("failed to run loginctl")?;
    if !status.success() {
        anyhow::bail!("loginctl enable-linger {user} failed with {status}");
    }
    Ok(())
}

/// Render the systemd user unit for a host service.
///
/// `up`/`web` are long-running daemons (Restart=on-failure). `fleet` is a
/// boot-time oneshot that relaunches the microVMs and exits: it must order
/// After=maturana-up.service (so sessiond/graph exist first) and run from the
/// repo root — the `repair firecracker-harnesses` path uses repo-relative
/// `./scripts/...`, `examples/...`, `.maturana/...`, so WorkingDirectory is
/// load-bearing.
pub fn render_host_systemd_unit(service: &HostService, exe: &str, home_root: &str) -> String {
    if service.name == "fleet" {
        // The repo root is the parent of the `.maturana` home dir.
        let working_dir = std::path::Path::new(home_root)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| home_root.to_string());
        return format!(
            "[Unit]\nDescription={}\nAfter=network-online.target maturana-up.service\nWants=network-online.target\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nWorkingDirectory={}\nExecStart={} --home {} {}\n\n[Install]\nWantedBy=default.target\n",
            service.description,
            working_dir,
            exe,
            home_root,
            service.args.join(" "),
        );
    }
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

/// PowerShell command that registers the boot task with a stored password.
///
/// Trigger is `-AtStartup` and the principal uses logon type **Password**
/// (`-User`/`-Password`) so the task runs in the user's profile at boot WITHOUT
/// an interactive login — codex/claude auth lives in that profile, which an S4U
/// (password-less) logon can't unlock. `-RunLevel Highest` matches the prior
/// behaviour. The password reaches the LSA credential vault, never disk/XML.
/// The privileged hostd keeps its dedicated SYSTEM installer.
pub fn windows_register_command(
    service: &HostService,
    exe: &str,
    home_root: &str,
    password: &str,
) -> String {
    let task = task_name(service);
    let exe = exe.replace('\'', "''");
    let argument = format!("--home \"{home_root}\" {}", service.args.join(" ")).replace('\'', "''");
    let password = password.replace('\'', "''");
    format!(
        "$a = New-ScheduledTaskAction -Execute '{exe}' -Argument '{argument}'; \
         $t = New-ScheduledTaskTrigger -AtStartup; \
         $s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero); \
         Register-ScheduledTask -TaskName '{task}' -Action $a -Trigger $t -Settings $s -User $env:USERNAME -Password '{password}' -RunLevel Highest -Force | Out-Null"
    )
}

/// Set `AutomaticStartAction Start` on every `maturana-*` Hyper-V VM so the
/// agents boot with the host (staggered delays to avoid a thundering herd).
/// Best-effort and inline (no cwd/script-path dependency); silent if Hyper-V is
/// absent or no VMs match.
fn ensure_hyperv_vm_autostart() {
    const SCRIPT: &str = "$i = 0; \
         Get-VM -Name 'maturana-*' -ErrorAction SilentlyContinue | ForEach-Object { \
           Set-VM -VM $_ -AutomaticStartAction Start -AutomaticStartDelay (30 + 15 * $i); \
           $i++ }";
    let _ = ProcessCommand::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .status();
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
    password: Option<&str>,
) -> anyhow::Result<()> {
    if action == "install" {
        let password = password.ok_or_else(|| {
            anyhow::anyhow!(
                "Windows install needs the user's password for a boot task that runs without \
                 login. Run scripts/install.ps1 (it prompts securely), or pass \
                 --windows-password <PW>."
            )
        })?;
        let command = windows_register_command(
            service,
            &exe.display().to_string(),
            &home.root().display().to_string(),
            password,
        );
        let status = ProcessCommand::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .status()
            .context("failed to run powershell")?;
        if !status.success() {
            anyhow::bail!("Register-ScheduledTask failed with {status}");
        }
        // Start it now rather than waiting for the next boot. A stored-password
        // task sometimes ignores `schtasks /Run`; fall back to Start-ScheduledTask.
        let ran = ProcessCommand::new("schtasks")
            .args(["/Run", "/TN", &task_name(service)])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ran {
            let _ = ProcessCommand::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!("Start-ScheduledTask -TaskName '{}'", task_name(service)),
                ])
                .status();
        }
        println!("registered + started task {}", task_name(service));
        // Best-effort: ensure the Hyper-V agent VMs auto-boot too, so the whole
        // stack returns after a reboot with no login.
        ensure_hyperv_vm_autostart();
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
            windows_register_command(&service, r"C:\m\maturana.exe", r"C:\m\.maturana", "p@ss'w");
        assert!(register.contains("New-ScheduledTaskAction -Execute 'C:\\m\\maturana.exe'"));
        assert!(register.contains(r#"-Argument '--home "C:\m\.maturana" up'"#));
        // Boot trigger + stored-password principal running in the user profile.
        assert!(register.contains("New-ScheduledTaskTrigger -AtStartup"));
        assert!(register.contains("-User $env:USERNAME -Password 'p@ss''w'"));
        assert!(register.contains("-RunLevel Highest"));
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

    #[test]
    fn fleet_service_is_known() {
        let fleet = known_service("fleet").unwrap();
        assert_eq!(fleet.name, "fleet");
        assert_eq!(
            fleet.args,
            vec![
                "repair",
                "firecracker-harnesses",
                "--skip-services",
                "--skip-assets",
                "--skip-worker-refresh"
            ]
        );
    }

    #[test]
    fn systemd_fleet_unit_golden() {
        let unit = render_host_systemd_unit(
            &known_service("fleet").unwrap(),
            "/usr/local/bin/maturana",
            "/home/aj/maturana/.maturana",
        );
        // Oneshot boot relauncher: ordered after the plane, runs from the repo
        // root (repo-relative paths), no Restart.
        assert!(unit.contains("Type=oneshot"));
        assert!(unit.contains("RemainAfterExit=yes"));
        assert!(unit.contains("After=network-online.target maturana-up.service"));
        assert!(unit.contains("WorkingDirectory=/home/aj/maturana"));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/maturana --home /home/aj/maturana/.maturana \
             repair firecracker-harnesses --skip-services --skip-assets --skip-worker-refresh"
        ));
        assert!(!unit.contains("Restart="));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
