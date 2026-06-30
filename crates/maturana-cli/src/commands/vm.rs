use std::path::PathBuf;

use clap::{Args, Subcommand};
use maturana_core::{cow, state::MaturanaHome, AgentSpec};

/// Copy-on-write VM storage operations.
#[derive(Debug, Args)]
pub struct VmCommand {
    #[command(subcommand)]
    command: VmSubcommand,
}

#[derive(Debug, Subcommand)]
enum VmSubcommand {
    /// Report a path's filesystem and whether it supports reflink (CoW) clones.
    Fstype {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Copy-on-write clone a file: an instant, space-shared reflink on
    /// Btrfs/XFS/ZFS-2.2+, a full byte copy elsewhere. This is the primitive
    /// agent provisioning uses to clone a golden rootfs.
    Clone { src: PathBuf, dest: PathBuf },
    /// Snapshot a (stopped) agent's rootfs via CoW, under its snapshots dir.
    Snapshot {
        agent_id: String,
        #[arg(long, default_value = "snap")]
        name: String,
    },
    /// Roll a (stopped) agent's rootfs back to a previously taken snapshot.
    Rollback {
        agent_id: String,
        #[arg(long, default_value = "snap")]
        name: String,
    },
    /// List an agent's CoW rootfs snapshots.
    Snapshots { agent_id: String },
}

pub fn handle_vm(command: VmCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        VmSubcommand::Fstype { path } => {
            match cow::detect_fstype(&path) {
                Some(fs) => {
                    let cap = if cow::fstype_supports_reflink(&fs) {
                        "copy-on-write: reflink supported (instant, space-shared clones)"
                    } else {
                        "no reflink - clones are full byte copies"
                    };
                    println!("{}: {fs} - {cap}", path.display());
                }
                None => println!(
                    "{}: filesystem could not be detected - clones will be full copies",
                    path.display()
                ),
            }
            Ok(())
        }
        VmSubcommand::Clone { src, dest } => {
            let kind = cow::provision_clone(&src, &dest)?;
            println!(
                "cloned {} -> {} [{}]",
                src.display(),
                dest.display(),
                kind.label()
            );
            Ok(())
        }
        VmSubcommand::Snapshot { agent_id, name } => {
            let rootfs = agent_rootfs_path(home, &agent_id)?;
            let dir = home.agent_dir(&agent_id).join("cow-snapshots");
            std::fs::create_dir_all(&dir)?;
            let snap = dir.join(format!("{name}.ext4"));
            let kind = cow::snapshot(&rootfs, &snap)?;
            println!(
                "snapshot '{name}' of {agent_id} [{}] -> {}",
                kind.label(),
                snap.display()
            );
            Ok(())
        }
        VmSubcommand::Rollback { agent_id, name } => {
            let rootfs = agent_rootfs_path(home, &agent_id)?;
            let snap = home
                .agent_dir(&agent_id)
                .join("cow-snapshots")
                .join(format!("{name}.ext4"));
            let kind = cow::rollback(&snap, &rootfs)?;
            println!(
                "rolled {agent_id} back to '{name}' [{}] - restart the agent to boot the restored rootfs",
                kind.label()
            );
            Ok(())
        }
        VmSubcommand::Snapshots { agent_id } => {
            let dir = home.agent_dir(&agent_id).join("cow-snapshots");
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect();
                    names.sort();
                    if names.is_empty() {
                        println!("no CoW snapshots for {agent_id}");
                    } else {
                        for name in names {
                            println!("{name}");
                        }
                    }
                }
                Err(_) => println!("no CoW snapshots for {agent_id}"),
            }
            Ok(())
        }
    }
}

/// Resolve an agent's live Firecracker rootfs image file (the thing CoW
/// snapshots/rewinds operate on). Spec rootfs paths are stored relative to the
/// repo root (the parent of the `.maturana` home dir).
fn agent_rootfs_path(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<PathBuf> {
    let spec = AgentSpec::from_maturana_markdown(home.agent_dir(agent_id).join("MATURANA.md"))?;
    let firecracker = spec.vm.firecracker.ok_or_else(|| {
        anyhow::anyhow!(
            "{agent_id} is not a Firecracker agent - CoW rootfs snapshots apply to Firecracker"
        )
    })?;
    let raw = PathBuf::from(&firecracker.rootfs_image);
    if raw.is_absolute() {
        Ok(raw)
    } else {
        let base = home.root().parent().unwrap_or_else(|| home.root());
        Ok(base.join(raw))
    }
}
