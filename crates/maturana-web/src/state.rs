use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::auth::SessionStore;
use crate::ws::protocol::Topic;

/// Shared application state. Cheap to clone (everything is Arc'd or a handle).
#[derive(Clone)]
pub struct AppState {
    pub home_root: PathBuf,
    /// The login token loaded at startup from `<home>/web/token`.
    pub login_token: Arc<String>,
    pub sessions: SessionStore,
    /// Dashboard fan-out: background pollers publish topic updates; each
    /// socket forwards only the topics it subscribed to.
    pub dash_tx: broadcast::Sender<DashEvent>,
}

#[derive(Debug, Clone)]
pub struct DashEvent {
    pub topic: Topic,
    pub data: serde_json::Value,
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
