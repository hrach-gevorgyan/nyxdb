//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, plus an append-only `(seq, doc_id)` log for `_changes`.
//! See plan §4.3, §5.

use crate::revtree::{RevId, RevNode, RevTree};
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

impl From<&RevTree> for StoredTree {
    fn from(tree: &RevTree) -> Self {
        let nodes = tree
            .nodes
            .iter()
            .map(|(rev, node)| {
                let body = serde_json::to_vec(&node.body).expect("doc body must serialize");
                (rev.clone(), StoredNode { parent: node.parent.clone(), deleted: node.deleted, body })
            })
            .collect();
        StoredTree { nodes }
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

    pub fn get_tree(&self, doc_id: &str) -> sled::Result<Option<RevTree>> {
        let Some(bytes) = self.docs.get(doc_id)? else { return Ok(None) };
        let stored: StoredTree = bincode_options().deserialize(&bytes).expect("corrupt revtree in storage");
        Ok(Some(RevTree::try_from(stored).expect("corrupt doc body JSON in storage")))
    }

    pub fn put_tree(&self, doc_id: &str, tree: &RevTree) -> sled::Result<u64> {
        let stored = StoredTree::from(tree);
        let bytes = bincode_options().serialize(&stored).expect("revtree must serialize");
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
    pub fn changes_since(&self, since: u64) -> sled::Result<Vec<(u64, String)>> {
        let start = (since + 1).to_be_bytes();
        self.seq_log
            .range(start..)
            .map(|entry| {
                let (seq_bytes, doc_id_bytes) = entry?;
                let seq = u64::from_be_bytes(seq_bytes.as_ref().try_into().unwrap());
                let doc_id = String::from_utf8(doc_id_bytes.to_vec()).expect("doc id must be utf8");
                Ok((seq, doc_id))
            })
            .collect()
    }

    /// Highest seq this db has assigned, read directly off `seq_log`'s
    /// last key rather than a separately maintained counter — one less
    /// piece of state that could drift out of sync with reality.
    pub fn current_seq(&self) -> sled::Result<u64> {
        Ok(self
            .seq_log
            .iter()
            .keys()
            .next_back()
            .transpose()?
            .map(|k| u64::from_be_bytes(k.as_ref().try_into().unwrap()))
            .unwrap_or(0))
    }
}

pub type SharedRoot = Arc<sled::Db>;
