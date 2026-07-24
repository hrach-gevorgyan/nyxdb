//! `_changes` feed plumbing: broadcast channel every write publishes to.
//! Longpoll/continuous modes subscribe to the same channel. See plan §5.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub seq: u64,
    pub doc_id: String,
}

#[derive(Clone)]
pub struct ChangeFeed {
    tx: broadcast::Sender<ChangeEvent>,
}

impl ChangeFeed {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1024);
        Self { tx }
    }

    pub fn publish(&self, event: ChangeEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ChangeEvent> {
        self.tx.subscribe()
    }
}

impl Default for ChangeFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// One `ChangeFeed` per database, created lazily and shared across
/// requests via `AppState` — a broadcast channel must outlive any single
/// request for continuous/longpoll subscribers to actually see events
/// published by later, unrelated requests.
#[derive(Clone, Default)]
pub struct ChangeFeedRegistry {
    feeds: Arc<Mutex<HashMap<String, ChangeFeed>>>,
}

impl ChangeFeedRegistry {
    pub fn get_or_create(&self, db_name: &str) -> ChangeFeed {
        let mut feeds = self.feeds.lock().expect("change feed registry lock poisoned");
        feeds.entry(db_name.to_string()).or_default().clone()
    }
}
