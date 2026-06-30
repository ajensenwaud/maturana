use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;

use crate::auth::SessionStore;
use crate::ws::protocol::{ServerMsg, Topic};

/// One web-originated chat turn the progress poller is tailing. Keyed in
/// `ActiveTurns` by (agent_id, session_id, message_id).
#[derive(Clone)]
pub struct TurnWatch {
    /// Highest progress `seq` already broadcast (None = nothing sent yet).
    pub last_seq: Option<u64>,
    /// When the turn was registered — used to age out turns whose worker died
    /// without writing a terminal status, so the map never leaks.
    pub started: Instant,
}

/// Chat turns sent from the web that are still streaming. The progress poller
/// reads this to know which side-lane files to tail and how far it has gotten;
/// entries are removed on a terminal status or after a generous TTL.
pub type ActiveTurns = Arc<Mutex<HashMap<(String, String, String), TurnWatch>>>;

/// Server-side fan-out to every connected socket. Dash updates are filtered
/// by each socket's topic subscriptions; session events always forward.
#[derive(Debug, Clone)]
pub enum Broadcast {
    Dash(Topic, serde_json::Value),
    Session(ServerMsg),
}

/// The web chat front door, injected by the CLI so slash commands can reuse the
/// channel command catalog. Plain chat turns flow through `maturana-ops`
/// conversation enqueueing instead of raw session DB writes, so they keep the
/// same transcript memory, model/reasoning, and routing as Telegram/TUI/Discord.
pub type EnqueueTurnFn =
    Arc<dyn Fn(&std::path::Path, &str, &str, &str) -> anyhow::Result<String> + Send + Sync>;

/// Ingest an uploaded file into an agent's knowledge graph (the same path
/// Telegram document uploads take), injected by the CLI because graph
/// resolution + ingestion live in maturana-cli. Args: home_root, agent_id, file
/// path; returns the number of chunks stored. The web crate calls THIS so a
/// chat-uploaded file becomes retrievable by the (VM-isolated) agent.
pub type IngestFileFn =
    Arc<dyn Fn(&std::path::Path, &str, &std::path::Path) -> anyhow::Result<usize> + Send + Sync>;

/// Shared application state. Cheap to clone (everything is Arc'd or a handle).
#[derive(Clone)]
pub struct AppState {
    pub home_root: PathBuf,
    /// The login token loaded at startup from `<home>/web/token`.
    pub login_token: Arc<String>,
    pub sessions: SessionStore,
    pub dash_tx: broadcast::Sender<Broadcast>,
    /// The shared channel front door (injected by the CLI).
    pub enqueue: EnqueueTurnFn,
    /// Optional knowledge-graph ingest hook (injected by the CLI) for chat file
    /// uploads. None when the cockpit runs without the CLI-provided closure.
    pub ingest: Option<IngestFileFn>,
    /// In-flight web chat turns the progress poller streams (see `ActiveTurns`).
    pub active_turns: ActiveTurns,
}

impl AppState {
    pub fn new(home_root: PathBuf, login_token: String, enqueue: EnqueueTurnFn) -> Self {
        let (dash_tx, _) = broadcast::channel(256);
        Self {
            home_root,
            login_token: Arc::new(login_token),
            sessions: SessionStore::default(),
            dash_tx,
            enqueue,
            ingest: None,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach the CLI-provided graph-ingest hook (builder style).
    pub fn with_ingest(mut self, ingest: Option<IngestFileFn>) -> Self {
        self.ingest = ingest;
        self
    }
}
