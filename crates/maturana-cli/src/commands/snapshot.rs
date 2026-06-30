use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use maturana_core::{
    audit::{append_event, AuditEvent},
    improvement::{signals, TrajectoryStore},
    snapshots::{list_snapshots, restore_snapshot, take_snapshot, SnapshotRecord},
    state::MaturanaHome,
};

#[derive(Debug, Args)]
pub(crate) struct SnapshotCommand {
    #[command(subcommand)]
    pub(crate) command: SnapshotSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SnapshotSubcommand {
    List {
        agent_id: String,
        #[arg(long)]
        live: bool,
    },
    Take {
        agent_id: String,
        name: String,
        #[arg(long)]
        live: bool,
    },
    Restore {
        agent_id: String,
        name: String,
        #[arg(long)]
        live: bool,
    },
}

pub(crate) fn handle_snapshot(command: SnapshotCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        SnapshotSubcommand::List { agent_id, live } => {
            let snapshots = match list_snapshots(home, &agent_id, live) {
                Ok(snapshots) => snapshots,
                Err(error) => {
                    audit_agent_event(
                        home,
                        &agent_id,
                        snapshot_audit_event("list", live, true),
                        format!("failed to list snapshots: {error:#}"),
                    )?;
                    return Err(error);
                }
            };
            for snapshot in snapshots {
                print_snapshot_record(&snapshot);
            }
            audit_agent_event(
                home,
                &agent_id,
                snapshot_audit_event("list", live, false),
                "listed snapshots through provider-aware Rust snapshot manager",
            )?;
        }
        SnapshotSubcommand::Take {
            agent_id,
            name,
            live,
        } => {
            let snapshot = match take_snapshot(home, &agent_id, &name, live) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    audit_agent_event(
                        home,
                        &agent_id,
                        snapshot_audit_event("take", live, true),
                        format!("failed to take snapshot {name}: {error:#}"),
                    )?;
                    return Err(error);
                }
            };
            print_snapshot_record(&snapshot);
            audit_agent_event(
                home,
                &agent_id,
                snapshot_audit_event("take", live, false),
                format!("created {:?} snapshot {name}", snapshot.kind),
            )?;
        }
        SnapshotSubcommand::Restore {
            agent_id,
            name,
            live,
        } => {
            let snapshot = match restore_snapshot(home, &agent_id, &name, live) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    audit_agent_event(
                        home,
                        &agent_id,
                        snapshot_audit_event("restore", live, true),
                        format!("failed to restore snapshot {name}: {error:#}"),
                    )?;
                    return Err(error);
                }
            };
            print_snapshot_record(&snapshot);
            audit_agent_event(
                home,
                &agent_id,
                snapshot_audit_event("restore", live, false),
                format!("restored {:?} snapshot {name}", snapshot.kind),
            )?;
            record_restore_penalty(home, &agent_id, &name)?;
        }
    }
    Ok(())
}

fn audit_agent_event(
    home: &MaturanaHome,
    agent_id: &str,
    action: &str,
    message: impl Into<String>,
) -> anyhow::Result<()> {
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: action.to_string(),
            message: message.into(),
        },
    )
}

fn print_snapshot_record(snapshot: &SnapshotRecord) {
    println!(
        "{} {:?} {:?} {}",
        snapshot.name, snapshot.provider, snapshot.kind, snapshot.created_at
    );
    if let Some(path) = &snapshot.state_path {
        println!("  state: {}", path.display());
    }
    if let Some(path) = &snapshot.memory_path {
        println!("  memory: {}", path.display());
    }
    if let Some(path) = &snapshot.disk_path {
        println!("  disk: {}", path.display());
    }
}

fn record_restore_penalty(home: &MaturanaHome, agent_id: &str, name: &str) -> anyhow::Result<()> {
    // A rollback is a negative training signal: the turn(s) since the snapshot
    // were bad enough to undo. Penalize the latest turn.
    let store_path = TrajectoryStore::store_path(home.root());
    let Ok(store) = TrajectoryStore::open(&store_path) else {
        return Ok(());
    };
    match store
        .reward_latest_for_agent(
            agent_id,
            "snapshot",
            signals::SNAPSHOT_ROLLBACK,
            Some(&format!("rollback to {name}")),
        )
        .with_context(|| format!("failed to record rollback penalty for {agent_id}"))
    {
        Ok(Some(_)) => {}
        Ok(None) => eprintln!(
            "[maturana] note: no recorded turn for agent {agent_id}; rollback penalty not applied"
        ),
        Err(error) => eprintln!("[maturana] warning: could not record rollback penalty: {error:#}"),
    }
    Ok(())
}

pub(crate) fn snapshot_audit_event(operation: &str, live: bool, failed: bool) -> &'static str {
    match (operation, live, failed) {
        ("list", true, false) => "snapshot.list.live",
        ("list", false, false) => "snapshot.list.local",
        ("list", true, true) => "snapshot.list.live.failed",
        ("list", false, true) => "snapshot.list.local.failed",
        ("take", true, false) => "snapshot.take.live",
        ("take", false, false) => "snapshot.take.local",
        ("take", true, true) => "snapshot.take.live.failed",
        ("take", false, true) => "snapshot.take.local.failed",
        ("restore", _, false) => "snapshot.restore.live",
        ("restore", _, true) => "snapshot.restore.live.failed",
        _ => "snapshot.unknown.failed",
    }
}
