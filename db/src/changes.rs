//! `_changes` feed plumbing: broadcast channel every write publishes to.
//! Longpoll/continuous modes subscribe to the same channel. See plan §5.

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
