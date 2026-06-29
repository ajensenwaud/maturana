//! Durable orchestration board — the persistent coordination layer a dispatcher
//! loop works through, claiming ready cards and running each on its assignee. It
//! is Maturana's zero-trust task board: the data model lives here (pure +
//! serializable); the dispatcher that actually runs a card over A2A lives in the
//! CLI, because running a card means giving an agent VM work — and every agent
//! runs in its own VM, the same as everywhere else. The board never becomes a
//! new, weaker execution substrate.
//!
//! Coordination is "state on the board": a card reads its dependencies' results
//! from the board (`dependency_context`) and writes its own back; agents never
//! share memory or talk directly. The board JSON is the single source of truth
//! and survives restarts — an interrupted run is reclaimed (`reclaim_in_flight`)
//! rather than left stuck, and every transition is appended to a run log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::state::MaturanaHome;

/// A card's column. "Ready" is not a stored status — it is computed (a `Todo`
/// card whose dependencies are all `Done`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    /// Parking column — an idea awaiting `decompose`/`specify` into real cards.
    Triage,
    Todo,
    Doing,
    Done,
    Blocked,
    /// Hidden from the active board; recoverable.
    Archived,
}

impl CardStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "triage" => Some(Self::Triage),
            "todo" => Some(Self::Todo),
            "doing" | "in_progress" | "in-progress" | "running" => Some(Self::Doing),
            "done" => Some(Self::Done),
            "blocked" => Some(Self::Blocked),
            "archived" | "archive" => Some(Self::Archived),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Archived => "archived",
        }
    }
}

/// A human/agent note on a card (append-only thread, shown to the worker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub author: String,
    pub body: String,
}

/// One execution attempt of a card — the auditable run history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub attempt: u32,
    #[serde(default)]
    pub agent: Option<String>,
    /// "completed" | "blocked" | "crashed" | "timed_out" | "reclaimed" | "gave_up".
    pub outcome: String,
    #[serde(default)]
    pub summary: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// One unit of work on the board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    /// Stable id, e.g. `c1`. Assigned on add.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    /// Who runs it: a role name (resolved against the orchestrator role set) or a
    /// concrete agent id. `None` falls back to the dispatcher's default role.
    #[serde(default)]
    pub assignee: Option<String>,
    pub status: CardStatus,
    /// Higher runs first when more cards are ready than `max_parallel` allows.
    #[serde(default)]
    pub priority: i64,
    /// Card ids that must be `Done` before this one is ready.
    #[serde(default)]
    pub deps: Vec<String>,
    /// The worker's reply once the card has run.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// Optional namespace tag (soft filter).
    #[serde(default)]
    pub tenant: Option<String>,
    /// Why a card is Blocked: "dependency"|"needs_input"|"capability"|"transient"|free text.
    #[serde(default)]
    pub block_kind: Option<String>,
    /// Don't dispatch until this time (RFC3339). None = immediately.
    #[serde(default)]
    pub scheduled_at: Option<DateTime<Utc>>,
    /// Auto-retry a failed card up to this many attempts before Blocking. 0/None = no retry.
    #[serde(default)]
    pub max_retries: u32,
    /// Goal mode: re-run the card with an acceptance-criteria judge until it passes.
    #[serde(default)]
    pub goal: bool,
    #[serde(default)]
    pub goal_max_turns: u32,
    /// Host-side files delivered into the worker's VM before it runs.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Append-only note thread (shown to the worker as context).
    #[serde(default)]
    pub comments: Vec<Comment>,
    /// Per-attempt run history.
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// A named, persistent board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub name: String,
    #[serde(default)]
    pub cards: Vec<Card>,
}

impl Board {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cards: Vec::new(),
        }
    }

    pub fn dir(home: &MaturanaHome) -> PathBuf {
        home.root().join("board")
    }

    pub fn path(home: &MaturanaHome, name: &str) -> PathBuf {
        Self::dir(home).join(format!("{name}.json"))
    }

    /// All board names on disk (the `board/<name>.json` files).
    pub fn list_names(home: &MaturanaHome) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(Self::dir(home)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(stem) = name.strip_suffix(".json") {
                    if !stem.ends_with(".events") {
                        out.push(stem.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Load a board, or an empty one if it doesn't exist yet.
    pub fn load(home: &MaturanaHome, name: &str) -> anyhow::Result<Self> {
        let path = Self::path(home, name);
        if !path.exists() {
            return Ok(Self::new(name));
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Persist atomically (write a sibling temp file, then rename) so a crash
    /// mid-write can never leave a half-written, unparseable board.
    pub fn save(&self, home: &MaturanaHome) -> anyhow::Result<()> {
        let path = Self::path(home, &self.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?; // bytes durable in the temp file before the atomic swap
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Add a `Todo` card and return its new id.
    pub fn add(
        &mut self,
        title: &str,
        detail: &str,
        assignee: Option<String>,
        deps: Vec<String>,
    ) -> String {
        let id = self.next_id();
        let now = Utc::now();
        self.cards.push(Card {
            id: id.clone(),
            title: title.to_string(),
            detail: detail.to_string(),
            assignee,
            status: CardStatus::Todo,
            priority: 0,
            deps,
            result: None,
            attempts: 0,
            tenant: None,
            block_kind: None,
            scheduled_at: None,
            max_retries: 0,
            goal: false,
            goal_max_turns: 0,
            attachments: Vec::new(),
            comments: Vec::new(),
            runs: Vec::new(),
            created_at: Some(now),
            updated_at: Some(now),
        });
        id
    }

    /// Record a finished attempt on a card (the auditable run history).
    pub fn record_run(
        &mut self,
        id: &str,
        agent: Option<String>,
        outcome: &str,
        summary: &str,
        started: DateTime<Utc>,
    ) {
        let now = Utc::now();
        if let Some(c) = self.card_mut(id) {
            let attempt = c.attempts;
            c.runs.push(RunRecord {
                attempt,
                agent,
                outcome: outcome.to_string(),
                summary: summary.chars().take(400).collect(),
                started_at: started,
                ended_at: now,
            });
            c.updated_at = Some(now);
        }
    }

    /// Append a comment to a card's thread.
    pub fn comment(&mut self, id: &str, author: &str, body: &str) -> bool {
        let now = Utc::now();
        if let Some(c) = self.card_mut(id) {
            c.comments.push(Comment { at: now, author: author.to_string(), body: body.to_string() });
            c.updated_at = Some(now);
            true
        } else {
            false
        }
    }

    fn next_id(&self) -> String {
        let max = self
            .cards
            .iter()
            .filter_map(|c| c.id.strip_prefix('c').and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0);
        format!("c{}", max + 1)
    }

    pub fn card(&self, id: &str) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    pub fn card_mut(&mut self, id: &str) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.id == id)
    }

    pub fn remove_card(&mut self, id: &str) -> bool {
        let before = self.cards.len();
        self.cards.retain(|c| c.id != id);
        // Drop dangling deps so the board stays runnable after a removal.
        for card in &mut self.cards {
            card.deps.retain(|d| d != id);
        }
        self.cards.len() != before
    }

    /// Validate the board before running it: every dependency must reference an
    /// existing card, no card may depend on itself, and there must be no cycle —
    /// so a dispatcher run always drains rather than deadlocking.
    pub fn validate(&self) -> Result<(), String> {
        let ids: HashSet<&str> = self.cards.iter().map(|c| c.id.as_str()).collect();
        for card in &self.cards {
            for dep in &card.deps {
                if dep == &card.id {
                    return Err(format!("card {} depends on itself", card.id));
                }
                if !ids.contains(dep.as_str()) {
                    return Err(format!("card {} depends on unknown card {}", card.id, dep));
                }
            }
        }
        let mut state: HashMap<String, u8> = HashMap::new();
        for card in &self.cards {
            visit(&card.id, self, &mut state)?;
        }
        Ok(())
    }

    /// Cards ready to run now: `Todo` with every dependency `Done`, highest
    /// `priority` first (then by id) so a capped dispatch picks the most
    /// important ready cards.
    pub fn ready(&self) -> Vec<&Card> {
        let now = Utc::now();
        let mut ready: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| {
                c.status == CardStatus::Todo
                    && c.scheduled_at.map_or(true, |t| t <= now)
                    && c.deps.iter().all(|d| {
                        self.card(d)
                            .map(|dc| dc.status == CardStatus::Done)
                            .unwrap_or(false)
                    })
            })
            .collect();
        ready.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));
        ready
    }

    pub fn is_complete(&self) -> bool {
        let active: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| c.status != CardStatus::Archived)
            .collect();
        !active.is_empty() && active.iter().all(|c| c.status == CardStatus::Done)
    }

    /// True if there's outstanding work that a dispatcher run could advance.
    pub fn has_runnable(&self) -> bool {
        self.cards
            .iter()
            .any(|c| matches!(c.status, CardStatus::Todo | CardStatus::Doing))
    }

    /// Reclaim a previous run interrupted by a crash/restart: any card left
    /// `Doing` is reset to `Todo` so the next dispatcher pass picks it up again
    /// (Hermes "a dead task gets reclaimed and respawned"). Returns how many were
    /// reclaimed. `attempts` is preserved so a retry cap can still bite.
    pub fn reclaim_in_flight(&mut self) -> usize {
        let mut n = 0;
        for card in &mut self.cards {
            if card.status == CardStatus::Doing {
                card.status = CardStatus::Todo;
                n += 1;
            }
        }
        n
    }

    /// Reset finished/failed cards back to `Todo` for a clean re-run (keeps the
    /// definitions, drops prior results). Returns how many were reset.
    pub fn reset_for_rerun(&mut self) -> usize {
        let mut n = 0;
        for card in &mut self.cards {
            if matches!(card.status, CardStatus::Done | CardStatus::Doing | CardStatus::Blocked) {
                card.status = CardStatus::Todo;
                card.result = None;
                n += 1;
            }
        }
        n
    }

    /// (todo, doing, done, blocked) counts. Triage/archived are tracked
    /// separately by the UI and not part of the active-work tuple.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for card in &self.cards {
            match card.status {
                CardStatus::Todo => c.0 += 1,
                CardStatus::Doing => c.1 += 1,
                CardStatus::Done => c.2 += 1,
                CardStatus::Blocked => c.3 += 1,
                CardStatus::Triage | CardStatus::Archived => {}
            }
        }
        c
    }

    /// The results of a card's dependencies, formatted as input for the worker.
    pub fn dependency_context(&self, card: &Card) -> String {
        let mut out = String::new();
        for dep in &card.deps {
            if let Some(dc) = self.card(dep) {
                if let Some(result) = &dc.result {
                    out.push_str(&format!("\n## from {} ({})\n{}\n", dc.id, dc.title, result));
                }
            }
        }
        out
    }
}

/// One entry in a board's append-only run log — the auditable trail of every
/// claim / completion / failure, tailed live by the cockpit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardEvent {
    pub at: DateTime<Utc>,
    /// "run_start" | "reclaim" | "claim" | "done" | "blocked" | "run_end".
    pub kind: String,
    #[serde(default)]
    pub card: Option<String>,
    #[serde(default)]
    pub text: String,
}

fn events_path(home: &MaturanaHome, board: &str) -> PathBuf {
    Board::dir(home).join(format!("{board}.events.jsonl"))
}

/// Append an event to a board's run log (best-effort; never fails a run).
pub fn log_event(home: &MaturanaHome, board: &str, kind: &str, card: Option<&str>, text: &str) {
    use std::io::Write;
    let event = BoardEvent {
        at: Utc::now(),
        kind: kind.to_string(),
        card: card.map(|c| c.to_string()),
        text: text.to_string(),
    };
    let path = events_path(home, board);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let (Ok(mut file), Ok(line)) = (
        std::fs::OpenOptions::new().create(true).append(true).open(&path),
        serde_json::to_string(&event),
    ) {
        let _ = writeln!(file, "{line}");
    }
}

/// Read a board's run log (newest events are last). Missing file → empty.
pub fn read_events(home: &MaturanaHome, board: &str) -> Vec<BoardEvent> {
    let path = events_path(home, board);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<BoardEvent>(l.trim()).ok())
        .collect()
}

/// Drop a board's run log (e.g. on a clean re-run).
pub fn clear_events(home: &MaturanaHome, board: &str) {
    let _ = std::fs::remove_file(events_path(home, board));
}

fn visit(id: &str, board: &Board, state: &mut HashMap<String, u8>) -> Result<(), String> {
    match state.get(id) {
        Some(2) => return Ok(()),
        Some(1) => return Err(format!("dependency cycle through card {id}")),
        _ => {}
    }
    state.insert(id.to_string(), 1);
    if let Some(card) = board.card(id) {
        for dep in &card.deps {
            visit(dep, board, state)?;
        }
    }
    state.insert(id.to_string(), 2);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(board: &mut Board, id: &str) {
        board.card_mut(id).unwrap().status = CardStatus::Done;
    }

    #[test]
    fn add_assigns_sequential_ids() {
        let mut b = Board::new("demo");
        assert_eq!(b.add("first", "", None, vec![]), "c1");
        assert_eq!(b.add("second", "", Some("developer".into()), vec![]), "c2");
        assert_eq!(b.cards.len(), 2);
        assert_eq!(b.card("c2").unwrap().assignee.as_deref(), Some("developer"));
    }

    #[test]
    fn ready_respects_dependencies() {
        let mut b = Board::new("demo");
        b.add("a", "", None, vec![]);
        b.add("b", "", None, vec!["c1".into()]);
        let ready: Vec<_> = b.ready().iter().map(|c| c.id.clone()).collect();
        assert_eq!(ready, vec!["c1"]);
        done(&mut b, "c1");
        let ready: Vec<_> = b.ready().iter().map(|c| c.id.clone()).collect();
        assert_eq!(ready, vec!["c2"]);
        assert!(!b.is_complete());
        done(&mut b, "c2");
        assert!(b.is_complete());
    }

    #[test]
    fn validate_rejects_cycles_and_dangling_deps() {
        let mut b = Board::new("demo");
        b.add("a", "", None, vec!["c2".into()]);
        b.add("b", "", None, vec!["c1".into()]);
        assert!(b.validate().unwrap_err().contains("cycle"));

        let mut d = Board::new("demo");
        d.add("a", "", None, vec!["c9".into()]);
        assert!(d.validate().unwrap_err().contains("unknown card"));

        let mut ok = Board::new("demo");
        ok.add("a", "", None, vec![]);
        ok.add("b", "", None, vec!["c1".into()]);
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn dependency_context_gathers_done_results() {
        let mut b = Board::new("demo");
        b.add("research", "", Some("researcher".into()), vec![]);
        b.add("build", "", Some("developer".into()), vec!["c1".into()]);
        b.card_mut("c1").unwrap().result = Some("found three frameworks".into());
        let ctx = b.dependency_context(b.card("c2").unwrap());
        assert!(ctx.contains("from c1 (research)"));
        assert!(ctx.contains("found three frameworks"));
    }

    #[test]
    fn reclaim_resets_doing_to_todo() {
        let mut b = Board::new("demo");
        b.add("a", "", None, vec![]);
        b.card_mut("c1").unwrap().status = CardStatus::Doing;
        b.card_mut("c1").unwrap().attempts = 1;
        assert_eq!(b.reclaim_in_flight(), 1);
        assert_eq!(b.card("c1").unwrap().status, CardStatus::Todo);
        // attempts preserved so a retry cap still bites.
        assert_eq!(b.card("c1").unwrap().attempts, 1);
        // ready again
        assert_eq!(b.ready().len(), 1);
    }

    #[test]
    fn remove_card_drops_dangling_deps() {
        let mut b = Board::new("demo");
        b.add("a", "", None, vec![]);
        b.add("b", "", None, vec!["c1".into()]);
        assert!(b.remove_card("c1"));
        assert!(b.card("c2").unwrap().deps.is_empty());
        assert!(b.validate().is_ok());
    }

    #[test]
    fn ready_orders_by_priority_then_id() {
        let mut b = Board::new("demo");
        b.add("low", "", None, vec![]);    // c1
        b.add("high", "", None, vec![]);   // c2
        b.add("mid", "", None, vec![]);    // c3
        b.card_mut("c2").unwrap().priority = 10;
        b.card_mut("c3").unwrap().priority = 5;
        let order: Vec<_> = b.ready().iter().map(|c| c.id.clone()).collect();
        assert_eq!(order, vec!["c2", "c3", "c1"]);
    }

    #[test]
    fn reset_for_rerun_clears_results() {
        let mut b = Board::new("demo");
        b.add("a", "", None, vec![]);
        b.card_mut("c1").unwrap().status = CardStatus::Done;
        b.card_mut("c1").unwrap().result = Some("x".into());
        assert_eq!(b.reset_for_rerun(), 1);
        assert_eq!(b.card("c1").unwrap().status, CardStatus::Todo);
        assert!(b.card("c1").unwrap().result.is_none());
    }

    #[test]
    fn status_parses_and_round_trips_json() {
        assert_eq!(CardStatus::parse("doing"), Some(CardStatus::Doing));
        assert_eq!(CardStatus::parse("IN-PROGRESS"), Some(CardStatus::Doing));
        assert_eq!(CardStatus::parse("nope"), None);
        let mut b = Board::new("demo");
        b.add("a", "detail", Some("codex-firecracker".into()), vec![]);
        let json = serde_json::to_string(&b).unwrap();
        let back: Board = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cards[0].title, "a");
        assert_eq!(back.cards[0].status, CardStatus::Todo);
    }
}
