//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, plus an append-only `(seq, doc_id)` log for `_changes`.
//! See plan §4.3, §5.

use crate::revtree::RevTree;
use std::sync::Arc;

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
        Ok(self
            .docs
            .get(doc_id)?
            .map(|bytes| serde_json::from_slice(&bytes).expect("corrupt revtree in storage")))
    }

    pub fn put_tree(&self, doc_id: &str, tree: &RevTree) -> sled::Result<u64> {
        // RevTree embeds serde_json::Value doc bodies, which bincode's
        // non-self-describing format can't deserialize (DeserializeAnyNotSupported).
        let bytes = serde_json::to_vec(tree).expect("revtree must serialize");
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
