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

/// Shared application state. Cheap to clone (everything is Arc'd or a handle).
#[derive(Clone)]
pub struct AppState {
    pub home_root: PathBuf,
    /// The login token loaded at startup from `<home>/web/token`.
    pub login_token: Arc<String>,
    pub sessions: SessionStore,
    pub dash_tx: broadcast::Sender<Broadcast>,
}

impl AppState {
    pub fn new(home_root: PathBuf, login_token: String) -> Self {
        let (dash_tx, _) = broadcast::channel(256);
        Self {
            home_root,
            login_token: Arc::new(login_token),
            sessions: SessionStore::default(),
            dash_tx,
        }
    }
}
