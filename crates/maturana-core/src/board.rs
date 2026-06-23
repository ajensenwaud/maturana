//! Persistent multi-agent Kanban board — the durable coordination layer a
//! dispatcher loop works through, claiming ready cards and running each on its
//! assignee. It is Maturana's zero-trust task board: the
//! data model lives here (pure + serializable); the dispatcher that actually
//! runs a card over A2A lives in the CLI, because running a card means giving an
//! agent VM work — and every agent runs in its own VM, the same as everywhere
//! else. The board never becomes a new, weaker execution substrate.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::state::MaturanaHome;

/// A card's column. "Ready" is not a stored status — it is computed (a `Todo`
/// card whose dependencies are all `Done`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    Todo,
    Doing,
    Done,
    Blocked,
}

impl CardStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "todo" => Some(Self::Todo),
            "doing" | "in_progress" | "in-progress" | "running" => Some(Self::Doing),
            "done" => Some(Self::Done),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
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
    /// Card ids that must be `Done` before this one is ready.
    #[serde(default)]
    pub deps: Vec<String>,
    /// The worker's reply once the card has run.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub attempts: u32,
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

    pub fn path(home: &MaturanaHome, name: &str) -> PathBuf {
        home.root().join("board").join(format!("{name}.json"))
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

    pub fn save(&self, home: &MaturanaHome) -> anyhow::Result<()> {
        let path = Self::path(home, &self.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
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
        self.cards.push(Card {
            id: id.clone(),
            title: title.to_string(),
            detail: detail.to_string(),
            assignee,
            status: CardStatus::Todo,
            deps,
            result: None,
            attempts: 0,
        });
        id
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

    /// Cards ready to run now: `Todo` with every dependency `Done`.
    pub fn ready(&self) -> Vec<&Card> {
        self.cards
            .iter()
            .filter(|c| {
                c.status == CardStatus::Todo
                    && c.deps.iter().all(|d| {
                        self.card(d)
                            .map(|dc| dc.status == CardStatus::Done)
                            .unwrap_or(false)
                    })
            })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        !self.cards.is_empty() && self.cards.iter().all(|c| c.status == CardStatus::Done)
    }

    /// (todo, doing, done, blocked) counts.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for card in &self.cards {
            match card.status {
                CardStatus::Todo => c.0 += 1,
                CardStatus::Doing => c.1 += 1,
                CardStatus::Done => c.2 += 1,
                CardStatus::Blocked => c.3 += 1,
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
        // Only c1 is ready while c2 waits on it.
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
