//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, plus an append-only `(seq, doc_id)` log for `_changes`.
//! See plan §4.3, §5.

use crate::revtree::RevTree;
use std::sync::Arc;

#[derive(Clone)]
pub struct Db {
    pub docs: sled::Tree,
    pub local: sled::Tree,
    pub seq_log: sled::Tree,
    pub meta: sled::Tree,
}

impl Db {
    pub fn open(base: &sled::Db, name: &str) -> sled::Result<Self> {
        Ok(Self {
            docs: base.open_tree(format!("{name}::docs"))?,
            local: base.open_tree(format!("{name}::local"))?,
            seq_log: base.open_tree(format!("{name}::seq"))?,
            meta: base.open_tree(format!("{name}::meta"))?,
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
        let seq = self.next_seq()?;
        self.seq_log.insert(seq.to_be_bytes(), doc_id)?;
        Ok(seq)
    }

    fn next_seq(&self) -> sled::Result<u64> {
        let seq = self
            .meta
            .update_and_fetch("update_seq", |old| {
                let n = old
                    .map(|b| u64::from_be_bytes(b.try_into().unwrap_or_default()))
                    .unwrap_or(0)
                    + 1;
                Some(n.to_be_bytes().to_vec())
            })?
            .expect("update_and_fetch always returns Some");
        Ok(u64::from_be_bytes(seq.as_ref().try_into().unwrap()))
    }
}

pub type SharedRoot = Arc<sled::Db>;
