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
}

impl Db {
    pub fn open(base: &sled::Db, name: &str) -> sled::Result<Self> {
        Ok(Self {
            base: base.clone(),
            docs: base.open_tree(format!("{name}::docs"))?,
            local: base.open_tree(format!("{name}::local"))?,
            seq_log: base.open_tree(format!("{name}::seq"))?,
        })
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
        let seq = self.base.generate_id()?;
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
