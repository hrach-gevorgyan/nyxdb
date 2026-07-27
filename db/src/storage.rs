//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, plus an append-only `(seq, doc_id)` log for `_changes`.
//! See plan §4.3, §5.

use crate::revtree::{RevId, RevNode, RevTree};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

/// Reads can fail two genuinely different ways: sled itself erroring
/// (disk I/O, etc. — already a `Result` from sled), or the bytes we get
/// back not decoding into what we expect. The second case used to be a
/// `.expect()` that panicked the request instead of failing gracefully
/// — found in the project audit (`doc/AUDIT.md`). A panic here doesn't
/// crash the whole server (tokio isolates it to one request), but it's
/// still an ungracious failure for something that should just be a
/// clean 500: a corrupted data directory, a manual edit, or (in the
/// future) a storage-format change without a migration path.
#[derive(Debug)]
pub enum StorageError {
    Sled(sled::Error),
    Corrupt(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Sled(e) => write!(f, "storage error: {e}"),
            StorageError::Corrupt(msg) => write!(f, "corrupt stored data: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<sled::Error> for StorageError {
    fn from(e: sled::Error) -> Self {
        StorageError::Sled(e)
    }
}

pub type StorageResult<T> = Result<T, StorageError>;

/// `bincode::serialize`/`deserialize` (the top-level convenience
/// functions) use fixed-width 8-byte integers and 8-byte length
/// prefixes for every `String`/`Vec`, even for tiny values — confirmed
/// in bincode's own source (`config/legacy.rs`). Varint encoding shrinks
/// every one of those length prefixes and integer fields for the small
/// values that dominate here (a handful of revisions, short strings),
/// with no change to what's actually stored — pure encoding-density win,
/// unlike the reverted dictionary-compression attempt which changed the
/// underlying format. See `doc/BENCHMARKS.md`.
fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new().with_varint_encoding()
}

/// On-disk encoding for a `RevTree`. JSON-wrapping the tree directly
/// (as an earlier version of this code did) repeats field names
/// (`"parent"`, `"deleted"`, `"body"`) and the `HashMap` key for every
/// single revision, which is pure overhead sled's per-value zstd
/// compression can't fully reclaim (each value is compressed
/// independently, so there's no cross-document dictionary to lean on —
/// see `doc/BENCHMARKS.md`). `bincode` can't handle `serde_json::Value`
/// directly (`DeserializeAnyNotSupported` — hit this once already, see
/// `doc/changelog.md`), so the doc body is kept as pre-serialized raw
/// JSON bytes instead of asking bincode to understand its shape.
#[derive(Serialize, Deserialize)]
struct StoredNode {
    parent: Option<RevId>,
    deleted: bool,
    body: Vec<u8>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredTree {
    nodes: Vec<(RevId, StoredNode)>,
}

impl TryFrom<&RevTree> for StoredTree {
    type Error = serde_json::Error;

    fn try_from(tree: &RevTree) -> Result<Self, Self::Error> {
        let nodes = tree
            .nodes
            .iter()
            .map(|(rev, node)| {
                let body = serde_json::to_vec(&node.body)?;
                Ok((rev.clone(), StoredNode { parent: node.parent.clone(), deleted: node.deleted, body }))
            })
            .collect::<Result<_, serde_json::Error>>()?;
        Ok(StoredTree { nodes })
    }
}

impl TryFrom<StoredTree> for RevTree {
    type Error = serde_json::Error;

    fn try_from(stored: StoredTree) -> Result<Self, Self::Error> {
        let nodes = stored
            .nodes
            .into_iter()
            .map(|(rev, node)| {
                let body = serde_json::from_slice(&node.body)?;
                Ok((rev, RevNode { parent: node.parent, deleted: node.deleted, body }))
            })
            .collect::<Result<_, serde_json::Error>>()?;
        Ok(RevTree { nodes })
    }
}

#[derive(Clone)]
pub struct Db {
    /// The shared root, kept around for `generate_id()` — a lock-free,
    /// mostly-in-memory monotonic counter (batches of ids are persisted
    /// together, not one disk write per id) that replaced a hand-rolled
    /// `update_and_fetch` counter tree. That counter tree cost one full
    /// extra sled write per document write for no benefit over sled's
    /// own primitive — removing it was one of two changes (the other
    /// being zstd compression, see `main.rs`) that closed most of the
    /// on-disk-size gap found in `doc/BENCHMARKS.md`.
    base: sled::Db,
    pub docs: sled::Tree,
    pub local: sled::Tree,
    pub seq_log: sled::Tree,
    /// Attachment bytes, keyed by content digest (`"sha256-<hex>"`) —
    /// not by doc id/rev/filename. Content-addressed storage means
    /// identical attachment content is only ever stored once, even if
    /// referenced by many revisions or documents; document bodies only
    /// ever hold a small stub (`{"stub":true,"digest":...,"length":...}`)
    /// pointing here, not the raw bytes. See `crate::attachments`.
    pub attachments: sled::Tree,
}

impl Db {
    pub fn open(base: &sled::Db, name: &str) -> sled::Result<Self> {
        Ok(Self {
            base: base.clone(),
            docs: base.open_tree(format!("{name}::docs"))?,
            local: base.open_tree(format!("{name}::local"))?,
            seq_log: base.open_tree(format!("{name}::seq"))?,
            attachments: base.open_tree(format!("{name}::attachments"))?,
        })
    }

    pub fn get_attachment(&self, digest: &str) -> StorageResult<Option<Vec<u8>>> {
        Ok(self.attachments.get(digest)?.map(|bytes| bytes.to_vec()))
    }

    pub fn put_attachment(&self, digest: &str, bytes: &[u8]) -> StorageResult<()> {
        // Content-addressed: if this exact content is already stored
        // (same digest), skip the write entirely rather than overwrite
        // with identical bytes.
        if self.attachments.contains_key(digest)? {
            return Ok(());
        }
        self.attachments.insert(digest, bytes)?;
        Ok(())
    }

    pub fn get_tree(&self, doc_id: &str) -> StorageResult<Option<RevTree>> {
        let Some(bytes) = self.docs.get(doc_id)? else { return Ok(None) };
        let stored: StoredTree = bincode_options()
            .deserialize(&bytes)
            .map_err(|e| StorageError::Corrupt(format!("revtree for {doc_id:?}: {e}")))?;
        let tree = RevTree::try_from(stored)
            .map_err(|e| StorageError::Corrupt(format!("doc body JSON for {doc_id:?}: {e}")))?;
        Ok(Some(tree))
    }

    pub fn put_tree(&self, doc_id: &str, tree: &RevTree) -> StorageResult<u64> {
        let stored = StoredTree::try_from(tree)
            .map_err(|e| StorageError::Corrupt(format!("failed to encode doc body for {doc_id:?}: {e}")))?;
        let bytes = bincode_options()
            .serialize(&stored)
            .map_err(|e| StorageError::Corrupt(format!("failed to encode revtree for {doc_id:?}: {e}")))?;
        self.docs.insert(doc_id, bytes)?;
        // `generate_id()` starts at 0, but `since=0` means "from the very
        // start" (no prior checkpoint) — real CouchDB's own convention,
        // where a seq token is never the same value as the "nothing seen
        // yet" starting point. `changes_since` correctly treats `since` as
        // exclusive (`seq > since`), so a raw 0 here would make the very
        // first document ever written to a fresh database permanently
        // invisible to any `_changes?since=0` poll — which is what every
        // fresh `db.sync()` starts with. Shifting by 1 keeps persisted seq
        // values in 1..=u64::MAX, so 0 is never a real assigned value and
        // can safely mean only "nothing yet." Found live-testing; see
        // `doc/changelog.md`.
        let seq = self.base.generate_id()? + 1;
        self.seq_log.insert(seq.to_be_bytes(), doc_id)?;
        Ok(seq)
    }

    /// `_changes?since=N` support: every `(seq, doc_id)` entry with
    /// `seq > since`, in order. Multiple entries can name the same
    /// `doc_id` (each write appends one) — callers dedupe, keeping the
    /// highest seq per doc, same as real CouchDB's `_changes` semantics.
    ///
    /// Note: `generate_id()` is a global counter shared across every
    /// database, so a given db's sequence numbers can have gaps where
    /// another db's writes landed — that's fine, callers only need
    /// "monotonically increasing for this db," not "contiguous."
    pub fn changes_since(&self, since: u64) -> StorageResult<Vec<(u64, String)>> {
        let start = (since + 1).to_be_bytes();
        self.seq_log
            .range(start..)
            .map(|entry| {
                let (seq_bytes, doc_id_bytes) = entry?;
                let seq_array: [u8; 8] = seq_bytes
                    .as_ref()
                    .try_into()
                    .map_err(|_| StorageError::Corrupt(format!("seq_log key is not 8 bytes: {seq_bytes:?}")))?;
                let seq = u64::from_be_bytes(seq_array);
                let doc_id = String::from_utf8(doc_id_bytes.to_vec())
                    .map_err(|e| StorageError::Corrupt(format!("seq_log value is not valid UTF-8: {e}")))?;
                Ok((seq, doc_id))
            })
            .collect()
    }

    /// Highest seq this db has assigned, read directly off `seq_log`'s
    /// last key rather than a separately maintained counter — one less
    /// piece of state that could drift out of sync with reality.
    pub fn current_seq(&self) -> StorageResult<u64> {
        let Some(key) = self.seq_log.iter().keys().next_back().transpose()? else {
            return Ok(0);
        };
        let array: [u8; 8] =
            key.as_ref().try_into().map_err(|_| StorageError::Corrupt(format!("seq_log key is not 8 bytes: {key:?}")))?;
        Ok(u64::from_be_bytes(array))
    }
}

pub type SharedRoot = Arc<sled::Db>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::revtree::RevTree;

    /// Real bug found live-testing against a real PouchDB app: sled's
    /// `generate_id()` starts at 0, but `_changes?since=0` (what every
    /// fresh `db.sync()` starts with) means "since the very beginning" —
    /// `changes_since` correctly treats `since` as exclusive, so a raw
    /// seq of 0 made the very first document ever written to a fresh
    /// database permanently invisible to any client doing an initial
    /// sync. `doc_count` would say 1, `_changes` would say 0 results.
    /// See `doc/changelog.md`.
    #[test]
    fn first_document_ever_written_is_visible_in_changes_since_zero() {
        // `temporary(true)`: an ephemeral, self-cleaning sled instance —
        // no real dependency on disk state, and avoids adding a dev-only
        // crate like `tempfile` just for one test (see doc/MAINTENANCE.md
        // on keeping the dependency list small).
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "testdb").unwrap();

        let mut tree = RevTree::default();
        tree.insert_revision_chain(&["1-aaa".to_string()], false, serde_json::json!({"x": 1}));
        db.put_tree("only-doc", &tree).unwrap();

        let changes = db.changes_since(0).unwrap();
        assert_eq!(changes.len(), 1, "the first-ever write must be visible to since=0, got: {changes:?}");
        assert_eq!(changes[0].1, "only-doc");

        // Also confirm no persisted seq is ever the sentinel value 0 —
        // that's what "since=0" needs to mean exclusively "nothing yet."
        assert!(changes[0].0 > 0);
    }
}
