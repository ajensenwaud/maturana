use std::path::PathBuf;

use clap::{Args, Subcommand};
use maturana_core::{inspect_agent, spec::HarnessRuntime, state::MaturanaHome};
use maturana_ops::{
    firecracker::{repair_firecracker_harnesses, FirecrackerHarnessRepair},
    guest_worker::{install_guest_worker, GuestWorkerInstall},
    host_setup::{ensure_agent_ssh_key, repair_ubuntu_cloudimg, UbuntuCloudimgRepair},
    windows_harness::{repair_windows_config, repair_windows_harnesses},
};

use crate::commands::doctor::{run_doctor, DoctorCommand};

#[derive(Debug, Args)]
pub(crate) struct RepairCommand {
    #[command(subcommand)]
    command: RepairSubcommand,
}

#[derive(Debug, Subcommand)]
enum RepairSubcommand {
    UbuntuCloudimg {
        #[arg(long, default_value = "noble")]
        release: String,
        #[arg(long, default_value = "amd64")]
        arch: String,
        #[arg(long = "image-url")]
        image_url: Option<String>,
        #[arg(long = "sha256sums-url")]
        sha256sums_url: Option<String>,
        #[arg(long = "qemu-img")]
        qemu_img: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    SshKey {
        #[arg(
            long = "key-path",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        key_path: PathBuf,
        #[arg(long)]
        force: bool,
    },
    WindowsHarnesses {
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(long = "session-id")]
        session_ids: Vec<String>,
        #[arg(long = "harness")]
        harnesses: Vec<String>,
        #[arg(long = "harness-auth-guest-path")]
        harness_auth_guest_paths: Vec<String>,
        #[arg(long = "telegram-token-source")]
        telegram_token_sources: Vec<String>,
        #[arg(long)]
        register_tasks: bool,
        #[arg(long)]
        skip_guest_worker_refresh: bool,
    },
    GuestWorker {
        #[arg(long = "agent-id")]
        agent_id: String,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long)]
        harness: String,
        #[arg(long = "guest-ip")]
        guest_ip: Option<String>,
        #[arg(long, default_value = "ubuntu")]
        ssh_user: String,
        #[arg(
            long,
            env = "MATURANA_AGENT_SSH_KEY",
            default_value = ".maturana/keys/maturana-agent-ed25519"
        )]
        ssh_key: PathBuf,
        #[arg(long = "harness-auth-guest-path")]
        harness_auth_guest_path: String,
        #[arg(
            long = "sessiond-url",
            default_value = "__MATURANA_DEFAULT_SESSIOND_URL__"
        )]
        sessiond_url: String,
        #[arg(
            long = "sessiond-token-path",
            default_value = ".maturana/sessiond/token"
        )]
        sessiond_token_path: PathBuf,
        #[arg(long = "auth-source")]
        auth_source: Option<PathBuf>,
        #[arg(long)]
        install_harness: bool,
        /// Re-seed the guest's harness auth from --auth-source even if the guest
        /// already has a live `.credentials.json`. Only for recovering a dead
        /// guest: claude-code self-refreshes its single-use OAuth token in-guest,
        /// so re-seeding a live guest clobbers it and logs the agent out.
        #[arg(long)]
        force_reseed_auth: bool,
    },
    FirecrackerHarnesses {
        #[arg(long = "agent-id")]
        agent_ids: Vec<String>,
        #[arg(
            long = "ssh-key",
            default_value = ".maturana/images/firecracker/maturana-firecracker.id_rsa"
        )]
        ssh_key: PathBuf,
        #[arg(long = "sessiond-bind", default_value = "0.0.0.0:47834")]
        sessiond_bind: String,
        #[arg(
            long = "sessiond-token-path",
            default_value = ".maturana/sessiond/token"
        )]
        sessiond_token_path: PathBuf,
        #[arg(long)]
        skip_assets: bool,
        /// Skip recreating the per-agent TAP device + NAT rule. The TAP is
        /// ephemeral (gone after a host reboot) and cheap to recreate, so boot
        /// recovery wants it ON even with --skip-assets. Only set this when the
        /// networking is known-good and you want a pure no-op relaunch.
        #[arg(long)]
        skip_net: bool,
        #[arg(long)]
        skip_launch: bool,
        #[arg(long)]
        skip_worker_refresh: bool,
        /// Skip starting sessiond + the MaturanaGraph service. Tokens are still
        /// ensured (so guest artifacts and `maturana up`'s graph supervision can
        /// find them), but the plane processes are left for `maturana up` to
        /// own, avoiding a port collision on 47834/47835 with the systemd
        /// `maturana-up` service.
        #[arg(long)]
        skip_services: bool,
        #[arg(long)]
        no_install_harness: bool,
        #[arg(long, default_value_t = 120)]
        ssh_wait_seconds: u64,
        /// Re-seed each guest's harness auth from the profile's host-auth even if
        /// the guest already has a live `.credentials.json`. Default off: a
        /// firecracker claude guest self-refreshes its own single-use OAuth token,
        /// so a routine repair must NOT clobber it. Only set this to recover a
        /// dead guest.
        #[arg(long)]
        force_reseed_auth: bool,
    },
}

pub(crate) fn run_repair(home: &MaturanaHome, command: RepairCommand) -> anyhow::Result<()> {
    match command.command {
        RepairSubcommand::UbuntuCloudimg {
            release,
            arch,
            image_url,
            sha256sums_url,
            qemu_img,
            force,
        } => repair_ubuntu_cloudimg(UbuntuCloudimgRepair {
            home: home.clone(),
            release,
            arch,
            image_url,
            sha256sums_url,
            qemu_img,
            force,
        }),
        RepairSubcommand::SshKey { key_path, force } => {
            ensure_agent_ssh_key(absolute_or_cwd(key_path)?, force)
        }
        RepairSubcommand::WindowsHarnesses {
            agent_ids,
            session_ids,
            harnesses,
            harness_auth_guest_paths,
            telegram_token_sources,
            register_tasks,
            skip_guest_worker_refresh,
        } => {
            let config = repair_windows_config(
                agent_ids,
                session_ids,
                harnesses,
                harness_auth_guest_paths,
                telegram_token_sources,
            )?;
            repair_windows_harnesses(home, &config, register_tasks, skip_guest_worker_refresh)?;
            run_doctor(
                home,
                DoctorCommand {
                    agent_ids: config.agent_ids.clone(),
                    json: false,
                    sessiond_url: "http://127.0.0.1:47834".to_string(),
                },
            )
        }
        RepairSubcommand::GuestWorker {
            agent_id,
            session_id,
            harness,
            guest_ip,
            ssh_user,
            ssh_key,
            harness_auth_guest_path,
            sessiond_url,
            sessiond_token_path,
            auth_source,
            install_harness,
            force_reseed_auth,
        } => install_guest_worker(
            home,
            GuestWorkerInstall {
                guest_ip: resolve_guest_ip(home, &agent_id, guest_ip)?,
                agent_id,
                session_id,
                harness: parse_harness_runtime(&harness)?,
                ssh_user,
                ssh_key,
                harness_auth_guest_path,
                sessiond_url,
                sessiond_token_path,
                auth_source,
                install_harness,
                force_reseed_auth,
            },
        ),
        RepairSubcommand::FirecrackerHarnesses {
            agent_ids,
            ssh_key,
            sessiond_bind,
            sessiond_token_path,
            skip_assets,
            skip_net,
            skip_launch,
            skip_worker_refresh,
            skip_services,
            no_install_harness,
            ssh_wait_seconds,
            force_reseed_auth,
        } => repair_firecracker_harnesses(
            home,
            FirecrackerHarnessRepair {
                agent_ids,
                ssh_key,
                sessiond_bind,
                sessiond_token_path,
                skip_assets,
                skip_net,
                skip_launch,
                skip_worker_refresh,
                skip_services,
                install_harness: !no_install_harness,
                ssh_wait_seconds,
                force_reseed_auth,
            },
        ),
    }
}

fn resolve_guest_ip(
    home: &MaturanaHome,
    agent_id: &str,
    explicit_ip: Option<String>,
) -> anyhow::Result<String> {
    if let Some(ip) = explicit_ip {
        return Ok(ip);
    }
    inspect_agent(home, agent_id)?.ipv4.ok_or_else(|| {
        anyhow::anyhow!("could not discover live IP for {agent_id}; pass --guest-ip explicitly")
    })
}

fn parse_harness_runtime(harness: &str) -> anyhow::Result<HarnessRuntime> {
    match harness {
        "codex" => Ok(HarnessRuntime::Codex),
        "claude-code" => Ok(HarnessRuntime::ClaudeCode),
        "opencode" => Ok(HarnessRuntime::Opencode),
        _ => anyhow::bail!("unsupported harness: {harness}"),
    }
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Debug, Subcommand)]
    enum TestCommand {
        #[command(name = "setup", visible_alias = "repair")]
        Repair(RepairCommand),
    }

    fn parse_firecracker_repair(args: &[&str]) -> FirecrackerHarnessRepair {
        let cli = TestCli::try_parse_from(args).expect("parse repair firecracker-harnesses");
        match cli.command {
            TestCommand::Repair(RepairCommand {
                command:
                    RepairSubcommand::FirecrackerHarnesses {
                        agent_ids,
                        ssh_key,
                        sessiond_bind,
                        sessiond_token_path,
                        skip_assets,
                        skip_net,
                        skip_launch,
                        skip_worker_refresh,
                        skip_services,
                        no_install_harness,
                        ssh_wait_seconds,
                        force_reseed_auth,
                    },
            }) => FirecrackerHarnessRepair {
                agent_ids,
                ssh_key,
                sessiond_bind,
                sessiond_token_path,
                skip_assets,
                skip_net,
                skip_launch,
                skip_worker_refresh,
                skip_services,
                install_harness: !no_install_harness,
                ssh_wait_seconds,
                force_reseed_auth,
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn skip_services_flag_parses_and_defaults_false() {
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.skip_services,
            "--skip-services must default to false so a bare repair still owns the plane"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--skip-services",
        ]);
        assert!(with.skip_services);
    }

    #[test]
    fn force_reseed_auth_defaults_false_and_parses() {
        // Default off: a routine firecracker repair must NOT re-seed (clobber) a
        // live claude guest's self-refreshing OAuth token. See install_guest_worker.
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.force_reseed_auth,
            "--force-reseed-auth must default to false so repair never clobbers a live guest token"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--force-reseed-auth",
        ]);
        assert!(with.force_reseed_auth);
    }

    #[test]
    fn guest_worker_force_reseed_auth_flag_parses() {
        // The manual `setup/repair guest-worker --auth-source ...` path also gates
        // the destructive re-seed behind --force-reseed-auth.
        fn parse(args: &[&str]) -> bool {
            match TestCli::try_parse_from(args)
                .expect("parse guest-worker")
                .command
            {
                TestCommand::Repair(RepairCommand {
                    command:
                        RepairSubcommand::GuestWorker {
                            force_reseed_auth, ..
                        },
                }) => force_reseed_auth,
                other => panic!("unexpected command: {other:?}"),
            }
        }
        let base = [
            "maturana",
            "repair",
            "guest-worker",
            "--agent-id",
            "claude-firecracker",
            "--session-id",
            "claude-main",
            "--harness",
            "claude-code",
            "--harness-auth-guest-path",
            "/home/ubuntu/.claude",
        ];
        assert!(
            !parse(&base),
            "guest-worker --force-reseed-auth defaults false"
        );
        let mut forced = base.to_vec();
        forced.push("--force-reseed-auth");
        assert!(parse(&forced));
    }

    #[test]
    fn skip_net_flag_defaults_false_and_parses() {
        let without = parse_firecracker_repair(&["maturana", "repair", "firecracker-harnesses"]);
        assert!(
            !without.skip_net,
            "--skip-net must default to false so boot recovery recreates the ephemeral TAP"
        );

        let with = parse_firecracker_repair(&[
            "maturana",
            "repair",
            "firecracker-harnesses",
            "--skip-net",
        ]);
        assert!(with.skip_net);
    }
}
