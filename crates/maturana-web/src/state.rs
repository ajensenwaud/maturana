use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::auth::SessionStore;
use crate::ws::protocol::{ServerMsg, Topic};

/// Server-side fan-out to every connected socket. Dash updates are filtered
/// by each socket's topic subscriptions; session events always forward.
#[derive(Debug, Clone)]
pub enum Broadcast {
    Dash(Topic, serde_json::Value),
    Session(ServerMsg),
}

/// The shared channel front door, injected by maturana-cli (which owns the
/// context builder). Args: home_root, agent_id, session_id, user text; returns
/// the enqueued message id. The web cockpit calls THIS instead of inserting a raw
/// inbound, so its turns get the same transcript memory + model/reasoning + routing
/// as Telegram/TUI/Discord. See `channels::enqueue_turn` for the implementation.
pub type EnqueueTurnFn =
    Arc<dyn Fn(&std::path::Path, &str, &str, &str) -> anyhow::Result<String> + Send + Sync>;

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
        }
    }
}
