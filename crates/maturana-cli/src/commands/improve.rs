use clap::{Args, Subcommand};
use maturana_core::{improvement::TrajectoryStore, state::MaturanaHome};

/// Self-improvement flywheel: capture agent trajectories, attach reward
/// signals, and curate training/preference datasets.
#[derive(Debug, Args)]
pub struct ImproveCommand {
    #[command(subcommand)]
    pub command: ImproveSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ImproveSubcommand {
    /// Record one agent turn as a trajectory.
    Record {
        agent_id: String,
        #[arg(long, default_value = "telegram-main")]
        session_id: String,
        #[arg(long, default_value = "chat")]
        kind: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        output: String,
        #[arg(long, default_value = "[]")]
        tool_calls: String,
    },
    /// Attach a reward signal to a trajectory (or the latest one for an agent).
    Reward {
        #[arg(long, conflicts_with_all = ["agent_id", "session_id"])]
        trajectory_id: Option<String>,
        #[arg(long)]
        agent_id: Option<String>,
        #[arg(long, default_value = "telegram-main")]
        session_id: String,
        #[arg(long, default_value = "user")]
        source: String,
        #[arg(long, allow_hyphen_values = true)]
        value: f64,
        #[arg(long)]
        note: Option<String>,
    },
    /// List curated examples at or above a reward threshold.
    Curate {
        #[arg(long, default_value_t = 1.0, allow_hyphen_values = true)]
        min_reward: f64,
        #[arg(long)]
        jsonl: bool,
    },
    /// Summary of the trajectory/reward corpus.
    Report {
        #[arg(long)]
        json: bool,
    },
}

pub fn run_improve_command(home: &MaturanaHome, command: ImproveCommand) -> anyhow::Result<()> {
    let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
    match command.command {
        ImproveSubcommand::Record {
            agent_id,
            session_id,
            kind,
            input,
            output,
            tool_calls,
        } => {
            let id = store.record(&agent_id, &session_id, &kind, &input, &output, &tool_calls)?;
            println!("recorded trajectory {id}");
        }
        ImproveSubcommand::Reward {
            trajectory_id,
            agent_id,
            session_id,
            source,
            value,
            note,
        } => match (trajectory_id, agent_id) {
            (Some(id), _) => {
                store.attach_reward(&id, &source, value, note.as_deref())?;
                println!("rewarded {id} ({source} {value:+})");
            }
            (None, Some(agent_id)) => {
                match store.reward_latest(
                    &agent_id,
                    &session_id,
                    &source,
                    value,
                    note.as_deref(),
                )? {
                    Some(id) => println!("rewarded latest trajectory {id} ({source} {value:+})"),
                    None => println!("no trajectory for {agent_id}/{session_id} to reward"),
                }
            }
            (None, None) => anyhow::bail!("pass --trajectory-id or --agent-id"),
        },
        ImproveSubcommand::Curate { min_reward, jsonl } => {
            if jsonl {
                print!("{}", store.export_sft_jsonl(min_reward)?);
            } else {
                let curated = store.curate(min_reward)?;
                if curated.is_empty() {
                    println!("no trajectories at or above reward {min_reward}");
                }
                for example in curated {
                    println!(
                        "{} reward={:+} ({} signals) :: {}",
                        example.trajectory.id,
                        example.reward.total,
                        example.reward.count,
                        truncate_inline(&example.trajectory.input, 60)
                    );
                }
            }
        }
        ImproveSubcommand::Report { json } => {
            let report = store.report()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("trajectories: {}", report.trajectories);
                println!("rewarded: {}", report.rewarded);
                println!("net-positive: {}", report.positive);
                println!("net-negative: {}", report.negative);
            }
        }
    }
    Ok(())
}

fn truncate_inline(value: &str, limit: usize) -> String {
    let value = value.replace('\n', " ");
    if value.chars().count() <= limit {
        value
    } else {
        value.chars().take(limit).collect::<String>() + "…"
    }
}
