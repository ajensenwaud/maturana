//! Self-improvement substrate: trajectory capture, reward attribution, and
//! dataset curation for a Hermes-style data flywheel.
//!
//! The loop Maturana implements is:
//!
//! 1. **Capture** — every agent turn is recorded as a [`Trajectory`]
//!    (input/state, the model's output, and any tool calls).
//! 2. **Reward** — signals are attached as [`Reward`] rows: explicit user
//!    feedback (👍/👎 from Telegram), task success, and strong negatives such
//!    as a snapshot rollback (the operator had to undo the agent).
//! 3. **Curate** — high-reward turns become supervised examples and high/low
//!    pairs become preference data, exported as JSONL.
//! 4. **Improve** — that dataset feeds offline SFT/DPO/distillation *outside*
//!    the host (the host never trains in-process).
//! 5. **Evaluate & redeploy** — a candidate is gated on an eval win-rate, then
//!    rolled out behind a snapshot so a regression can be reversed instantly.
//!
//! This module owns steps 1-3 and the export for step 4; the training and
//! evaluation gate are described in `docs/self-improvement-rl.md`.

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trajectory {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub kind: String,
    pub input: String,
    pub output: String,
    pub tool_calls: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reward {
    pub trajectory_id: String,
    pub source: String,
    pub value: f64,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Aggregate reward for a trajectory: the summed signal plus how many signals
/// contributed (so a single 👍 is distinguishable from a strong consensus).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RewardSummary {
    pub total: f64,
    pub count: i64,
}

/// One curated training example with its aggregate reward.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CuratedExample {
    pub trajectory: Trajectory,
    pub reward: RewardSummary,
}

pub struct TrajectoryStore {
    db: Connection,
}

impl TrajectoryStore {
    pub fn store_path(home_root: &Path) -> PathBuf {
        home_root.join("improvement").join("trajectories.sqlite")
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let db =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        db.pragma_update(None, "busy_timeout", 5000)?;
        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS trajectories (
                id TEXT PRIMARY KEY,
                seq INTEGER NOT NULL,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                input TEXT NOT NULL,
                output TEXT NOT NULL,
                tool_calls TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS rewards (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                trajectory_id TEXT NOT NULL,
                source TEXT NOT NULL,
                value REAL NOT NULL,
                note TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS rewards_by_trajectory ON rewards(trajectory_id);
            "#,
        )?;
        Ok(Self { db })
    }

    pub fn record(
        &self,
        agent_id: &str,
        session_id: &str,
        kind: &str,
        input: &str,
        output: &str,
        tool_calls: &str,
    ) -> anyhow::Result<String> {
        let id = format!("traj-{}", Uuid::new_v4());
        let seq: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM trajectories",
            [],
            |row| row.get(0),
        )?;
        self.db.execute(
            r#"
            INSERT INTO trajectories
              (id, seq, agent_id, session_id, kind, input, output, tool_calls, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                id,
                seq,
                agent_id,
                session_id,
                kind,
                input,
                output,
                tool_calls,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }

    pub fn attach_reward(
        &self,
        trajectory_id: &str,
        source: &str,
        value: f64,
        note: Option<&str>,
    ) -> anyhow::Result<()> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM trajectories WHERE id = ?1)",
            params![trajectory_id],
            |row| row.get(0),
        )?;
        if !exists {
            anyhow::bail!("no trajectory {trajectory_id} to reward");
        }
        self.db.execute(
            "INSERT INTO rewards (trajectory_id, source, value, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![trajectory_id, source, value, note, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Attach a reward to the most recent trajectory for an agent/session. This
    /// is what a Telegram 👍/👎 hooks into: feedback lands on the turn the user
    /// just saw without the channel needing to track trajectory ids.
    pub fn reward_latest(
        &self,
        agent_id: &str,
        session_id: &str,
        source: &str,
        value: f64,
        note: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM trajectories WHERE agent_id = ?1 AND session_id = ?2 ORDER BY seq DESC LIMIT 1",
                params![agent_id, session_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = &id {
            self.attach_reward(id, source, value, note)?;
        }
        Ok(id)
    }

    /// Like [`reward_latest`](Self::reward_latest) but session-agnostic: attach a
    /// reward to the most recent trajectory for an agent regardless of which
    /// session recorded it. Snapshot rollback uses this — it knows the agent
    /// being rolled back but not the exact session id the bad turn was logged
    /// under (channel turns record under e.g. `codex-main`, not `<agent>-main`).
    pub fn reward_latest_for_agent(
        &self,
        agent_id: &str,
        source: &str,
        value: f64,
        note: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM trajectories WHERE agent_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![agent_id],
                |row| row.get(0),
            )
            .ok();
        if let Some(id) = &id {
            self.attach_reward(id, source, value, note)?;
        }
        Ok(id)
    }

    pub fn reward_summary(&self, trajectory_id: &str) -> anyhow::Result<RewardSummary> {
        let (total, count): (Option<f64>, i64) = self.db.query_row(
            "SELECT SUM(value), COUNT(*) FROM rewards WHERE trajectory_id = ?1",
            params![trajectory_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(RewardSummary {
            total: total.unwrap_or(0.0),
            count,
        })
    }

    /// All trajectories whose aggregate reward is at least `min_reward`,
    /// highest first — the supervised fine-tuning set.
    pub fn curate(&self, min_reward: f64) -> anyhow::Result<Vec<CuratedExample>> {
        let mut examples = self.scored_trajectories()?;
        examples.retain(|example| example.reward.total >= min_reward);
        examples.sort_by(|a, b| {
            b.reward
                .total
                .partial_cmp(&a.reward.total)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(examples)
    }

    /// Render the top-`n` highest-reward chat turns for `agent_id` as few-shot
    /// "learned examples" markdown, injected into later prompts. This is the
    /// in-context consumer that closes the loop without any fine-tuning: a
    /// rewarded turn shapes the next turn's context. Empty string when there is
    /// nothing above `min_reward`.
    pub fn learned_examples_markdown(
        &self,
        agent_id: &str,
        n: usize,
        min_reward: f64,
    ) -> anyhow::Result<String> {
        let examples: Vec<_> = self
            .curate(min_reward)?
            .into_iter()
            .filter(|e| e.trajectory.agent_id == agent_id && e.trajectory.kind == "chat")
            .filter(|e| !e.trajectory.input.trim().is_empty())
            .take(n)
            .collect();
        if examples.is_empty() {
            return Ok(String::new());
        }
        let mut out = String::from(
            "These past exchanges were rated positively; match their style and substance.\n",
        );
        for (i, ex) in examples.iter().enumerate() {
            out.push_str(&format!(
                "\n### Example {} (reward {:+.0})\nUser: {}\nYou: {}\n",
                i + 1,
                ex.reward.total,
                truncate(&ex.trajectory.input, 400),
                truncate(&ex.trajectory.output, 800),
            ));
        }
        Ok(out)
    }

    /// Export curated examples as chat-style SFT JSONL (one record per line).
    pub fn export_sft_jsonl(&self, min_reward: f64) -> anyhow::Result<String> {
        let mut out = String::new();
        for example in self.curate(min_reward)? {
            let record = serde_json::json!({
                "messages": [
                    {"role": "user", "content": example.trajectory.input},
                    {"role": "assistant", "content": example.trajectory.output},
                ],
                "reward": example.reward.total,
                "agent_id": example.trajectory.agent_id,
            });
            out.push_str(&serde_json::to_string(&record)?);
            out.push('\n');
        }
        Ok(out)
    }

    fn scored_trajectories(&self) -> anyhow::Result<Vec<CuratedExample>> {
        let mut stmt = self.db.prepare(
            "SELECT id, agent_id, session_id, kind, input, output, tool_calls, created_at FROM trajectories ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Trajectory {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    kind: row.get(3)?,
                    input: row.get(4)?,
                    output: row.get(5)?,
                    tool_calls: row.get(6)?,
                    created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(7)?)
                        .map(|dt| dt.with_timezone(&Utc))
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut examples = Vec::with_capacity(rows.len());
        for trajectory in rows {
            let reward = self.reward_summary(&trajectory.id)?;
            examples.push(CuratedExample { trajectory, reward });
        }
        Ok(examples)
    }

    pub fn report(&self) -> anyhow::Result<ImprovementReport> {
        let total: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM trajectories", [], |row| row.get(0))?;
        let rewarded: i64 = self.db.query_row(
            "SELECT COUNT(DISTINCT trajectory_id) FROM rewards",
            [],
            |row| row.get(0),
        )?;
        let positive: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM (SELECT trajectory_id, SUM(value) total FROM rewards GROUP BY trajectory_id HAVING total > 0)",
            [],
            |row| row.get(0),
        )?;
        let negative: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM (SELECT trajectory_id, SUM(value) total FROM rewards GROUP BY trajectory_id HAVING total < 0)",
            [],
            |row| row.get(0),
        )?;
        Ok(ImprovementReport {
            trajectories: total,
            rewarded,
            positive,
            negative,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImprovementReport {
    pub trajectories: i64,
    pub rewarded: i64,
    pub positive: i64,
    pub negative: i64,
}

/// Canonical reward values for common signals, so every call site agrees on
/// the sign and rough magnitude. A rollback dominates a single thumbs-down.
pub mod signals {
    pub const THUMBS_UP: f64 = 1.0;
    pub const THUMBS_DOWN: f64 = -1.0;
    pub const TASK_SUCCESS: f64 = 1.0;
    pub const TASK_FAILURE: f64 = -1.0;
    pub const SNAPSHOT_ROLLBACK: f64 = -5.0;
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TrajectoryStore {
        let path = std::env::temp_dir()
            .join(format!("maturana-improve-{}", Uuid::new_v4()))
            .join("trajectories.sqlite");
        TrajectoryStore::open(&path).unwrap()
    }

    #[test]
    fn records_and_rewards_a_trajectory() {
        let store = store();
        let id = store
            .record("agent", "telegram-main", "chat", "hi", "hello", "[]")
            .unwrap();
        store
            .attach_reward(&id, "user", signals::THUMBS_UP, Some("nice"))
            .unwrap();
        store
            .attach_reward(&id, "task", signals::TASK_SUCCESS, None)
            .unwrap();
        let summary = store.reward_summary(&id).unwrap();
        assert_eq!(summary.count, 2);
        assert!((summary.total - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn learned_examples_ranks_rewarded_turns_for_the_agent() {
        let store = store();
        let good = store
            .record("a", "s", "chat", "what is X?", "X is the answer", "[]")
            .unwrap();
        store
            .attach_reward(&good, "user", signals::THUMBS_UP, None)
            .unwrap();
        // Unrewarded + other-agent + tool turns are excluded.
        store
            .record("a", "s", "chat", "meh", "whatever", "[]")
            .unwrap();
        store
            .record("b", "s", "chat", "other agent", "nope", "[]")
            .unwrap();
        let other = store.record("a", "s", "tool", "ran", "out", "[]").unwrap();
        store
            .attach_reward(&other, "user", signals::THUMBS_UP, None)
            .unwrap();

        let md = store.learned_examples_markdown("a", 5, 0.5).unwrap();
        assert!(md.contains("what is X?"));
        assert!(md.contains("X is the answer"));
        assert!(!md.contains("other agent"));
        assert!(!md.contains("ran")); // tool kind excluded
                                      // Nothing above a high bar → empty.
        assert!(store
            .learned_examples_markdown("a", 5, 99.0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reward_latest_targets_the_most_recent_turn() {
        let store = store();
        store
            .record("agent", "s", "chat", "first", "a", "[]")
            .unwrap();
        let second = store
            .record("agent", "s", "chat", "second", "b", "[]")
            .unwrap();
        let target = store
            .reward_latest("agent", "s", "user", signals::THUMBS_DOWN, None)
            .unwrap();
        assert_eq!(target.as_deref(), Some(second.as_str()));
        assert_eq!(store.reward_summary(&second).unwrap().count, 1);
    }

    #[test]
    fn reward_latest_for_agent_is_session_agnostic() {
        let store = store();
        store
            .record("agent", "codex-main", "chat", "q", "a", "[]")
            .unwrap();
        let newest = store
            .record("agent", "telegram-main", "chat", "q2", "b", "[]")
            .unwrap();
        // The old snapshot-rollback bug targeted "<agent>-main", which never
        // matches the real session id → no penalty applied.
        assert!(store
            .reward_latest(
                "agent",
                "agent-main",
                "snapshot",
                signals::SNAPSHOT_ROLLBACK,
                None
            )
            .unwrap()
            .is_none());
        // The session-agnostic call lands on the agent's most recent turn
        // regardless of which session recorded it.
        let target = store
            .reward_latest_for_agent("agent", "snapshot", signals::SNAPSHOT_ROLLBACK, None)
            .unwrap();
        assert_eq!(target.as_deref(), Some(newest.as_str()));
        assert_eq!(store.reward_summary(&newest).unwrap().count, 1);
    }

    #[test]
    fn curation_filters_and_orders_by_reward() {
        let store = store();
        let good = store
            .record("agent", "s", "chat", "q1", "great", "[]")
            .unwrap();
        let bad = store
            .record("agent", "s", "chat", "q2", "bad", "[]")
            .unwrap();
        let neutral = store
            .record("agent", "s", "chat", "q3", "meh", "[]")
            .unwrap();
        store.attach_reward(&good, "user", 3.0, None).unwrap();
        store
            .attach_reward(&bad, "user", signals::SNAPSHOT_ROLLBACK, None)
            .unwrap();
        store.attach_reward(&neutral, "user", 0.5, None).unwrap();

        let curated = store.curate(1.0).unwrap();
        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].trajectory.id, good);

        let jsonl = store.export_sft_jsonl(1.0).unwrap();
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.contains("\"great\""));
        assert!(!jsonl.contains("\"bad\""));

        let report = store.report().unwrap();
        assert_eq!(report.trajectories, 3);
        assert_eq!(report.positive, 2); // good + neutral
        assert_eq!(report.negative, 1); // rolled-back
    }
}
