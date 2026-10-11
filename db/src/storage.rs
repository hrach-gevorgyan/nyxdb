//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, a `(seq, doc_id)` log for `_changes` (one entry per doc,
//! its latest write), attachment bytes, and `_local` docs.

use crate::changes::{ChangeEvent, ChangeFeed};
use crate::revtree::{RevId, RevNode, RevTree, REVS_LIMIT};
use bincode::Options;
use sled::transaction::{ConflictableTransactionError, TransactionError};
use sled::Transactional;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

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

/// Most docs `Db::try_update_trees` writes in one transaction.
const BATCH_CHUNK: usize = 128;

/// One write lock per database, shared across requests (a `Db` is opened
/// fresh per request, so the lock has to live somewhere longer-lived —
/// `AppState`). See `Db::update_tree` for why it exists.
#[derive(Clone, Default)]
pub struct WriteLocks(Arc<Mutex<HashMap<String, DbLocks>>>);

#[derive(Clone, Default)]
struct DbLocks {
    write: Arc<Mutex<()>>,
    /// Shared by every write that stores or checks attachment bytes, from
    /// the blob store until its tree is committed; exclusive for
    /// `Db::compact_attachments`, so a blob a write has stored but not yet
    /// referenced can't be collected under it.
    attachments: Arc<RwLock<()>>,
}

impl WriteLocks {
    fn for_db(&self, name: &str) -> DbLocks {
        let mut locks = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(name.to_string()).or_default().clone()
    }
}

#[derive(Clone)]
pub struct Db {
    write_lock: Arc<Mutex<()>>,
    attachment_gate: Arc<RwLock<()>>,
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
    /// doc id → that doc's current seq, so a write can delete the doc's
    /// previous `seq_log` entry: one `_changes` entry per doc, not one per
    /// write (see `put_tree`).
    seq_by_doc: sled::Tree,
    /// Attachment bytes, keyed by content digest (`"sha256-<hex>"`) —
    /// not by doc id/rev/filename. Content-addressed storage means
    /// identical attachment content is only ever stored once, even if
    /// referenced by many revisions or documents; document bodies only
    /// ever hold a small stub (`{"stub":true,"digest":...,"length":...}`)
    /// pointing here, not the raw bytes. See `crate::attachments`.
    pub attachments: sled::Tree,
    /// md5 digest (`"md5-<base64>"`, what stubs expose) → content key.
    attachment_index: sled::Tree,
}

impl Db {
    pub fn open(base: &sled::Db, name: &str, locks: &WriteLocks) -> StorageResult<Self> {
        let locks = locks.for_db(name);
        let db = Self {
            write_lock: locks.write,
            attachment_gate: locks.attachments,
            base: base.clone(),
            docs: base.open_tree(format!("{name}::docs"))?,
            local: base.open_tree(format!("{name}::local"))?,
            seq_log: base.open_tree(format!("{name}::seq"))?,
            seq_by_doc: base.open_tree(format!("{name}::seqidx"))?,
            attachments: base.open_tree(format!("{name}::attachments"))?,
            attachment_index: base.open_tree(format!("{name}::attmd5"))?,
        };
        if db.seq_by_doc.is_empty() && !db.seq_log.is_empty() {
            db.migrate_seq_index()?;
        }
        Ok(db)
    }

    /// One-time upgrade for databases written before v0.1.7, whose
    /// `seq_log` holds an entry for every write ever made: keep each doc's
    /// newest entry, delete the rest, and build the doc → seq index.
    fn migrate_seq_index(&self) -> StorageResult<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.seq_by_doc.is_empty() {
            return Ok(()); // another request migrated while we waited
        }
        let mut newest: HashMap<String, u64> = HashMap::new();
        let mut stale = Vec::new();
        for (seq, doc_id) in self.changes_since(0)? {
            if let Some(previous) = newest.insert(doc_id, seq) {
                stale.push(previous);
            }
        }
        let mut batch = sled::Batch::default();
        for seq in stale {
            batch.remove(&seq.to_be_bytes());
        }
        self.seq_log.apply_batch(batch)?;
        let mut index = sled::Batch::default();
        for (doc_id, seq) in newest {
            index.insert(doc_id.as_bytes(), &seq.to_be_bytes());
        }
        self.seq_by_doc.apply_batch(index)?;
        self.base.flush()?;
        Ok(())
    }

    /// Resolves a stub digest — `md5-…` (current) or `sha256-…` (stubs
    /// written before v0.1.7, which were the content key itself).
    fn attachment_key(&self, digest: &str) -> StorageResult<Option<sled::IVec>> {
        if digest.starts_with("md5-") {
            Ok(self.attachment_index.get(digest)?)
        } else {
            Ok(Some(sled::IVec::from(digest.as_bytes())))
        }
    }

    pub fn get_attachment(&self, digest: &str) -> StorageResult<Option<Vec<u8>>> {
        let Some(key) = self.attachment_key(digest)? else { return Ok(None) };
        Ok(self.attachments.get(key)?.map(|bytes| bytes.to_vec()))
    }

    pub fn has_attachment(&self, digest: &str) -> StorageResult<bool> {
        let Some(key) = self.attachment_key(digest)? else { return Ok(false) };
        Ok(self.attachments.contains_key(key)?)
    }

    /// Stores `bytes` under their content key and indexes their md5
    /// digest. Content-addressed: identical bytes are only stored once.
    /// The first content to claim an md5 keeps it (a deliberately crafted
    /// md5 collision can't redirect an existing stub to other bytes).
    pub fn put_attachment(&self, key: &str, md5: &str, bytes: &[u8]) -> StorageResult<()> {
        if !self.attachments.contains_key(key)? {
            self.attachments.insert(key, bytes)?;
        }
        let _ = self.attachment_index.compare_and_swap(md5, None as Option<&[u8]>, Some(key.as_bytes()))?;
        Ok(())
    }

    /// Held by a write from storing (or checking) attachment bytes until
    /// its tree is committed. See `DbLocks::attachments`.
    pub fn attachment_writer(&self) -> RwLockReadGuard<'_, ()> {
        self.attachment_gate.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Deletes attachment bytes no stored revision refers to any more, and
    /// their md5 index entries. Only leaves keep bodies (`RevTree::prune`),
    /// so bytes only an old revision used are collected too. Returns how
    /// many blobs were deleted.
    pub fn compact_attachments(&self) -> StorageResult<usize> {
        let _gate = self.attachment_gate.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut referenced: HashSet<sled::IVec> = HashSet::new();
        for id in self.doc_ids()? {
            let Some(tree) = self.get_tree(&id)? else { continue };
            for node in tree.nodes.values() {
                let Some(attachments) = node.body.get("_attachments").and_then(|a| a.as_object()) else { continue };
                for digest in attachments.values().filter_map(|a| a.get("digest")?.as_str()) {
                    if let Some(key) = self.attachment_key(digest)? {
                        referenced.insert(key);
                    }
                }
            }
        }
        let mut removed = 0;
        for key in self.attachments.iter().keys() {
            let key = key?;
            if !referenced.contains(&key) {
                self.attachments.remove(&key)?;
                removed += 1;
            }
        }
        for entry in self.attachment_index.iter() {
            let (md5, key) = entry?;
            if !referenced.contains(&key) {
                self.attachment_index.remove(md5)?;
            }
        }
        self.base.flush()?;
        Ok(removed)
    }

    /// Every stored doc id (tombstones included), in byte order.
    pub fn doc_ids(&self) -> StorageResult<Vec<String>> {
        self.docs
            .iter()
            .keys()
            .map(|key| Ok(String::from_utf8_lossy(&key?).into_owned()))
            .collect()
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

    /// The only way a document gets written. Holds this database's write
    /// lock across read tree → `modify` → write tree → seq assignment →
    /// publish, so concurrent writers to the same doc serialise instead of
    /// each reading the same tree and the last one silently overwriting
    /// the other's branch (both used to get a success response; the lost
    /// branch was never re-sent — see `doc/changelog.md`, v0.1.7).
    ///
    /// Seq assignment and publishing happen inside the same lock, so
    /// within a database seqs become visible, and are broadcast, strictly
    /// in order — `_changes` followers that drop `seq <= last_seq` can't
    /// skip an earlier write that landed late.
    pub fn update_tree<T>(
        &self,
        doc_id: &str,
        feed: &ChangeFeed,
        modify: impl FnOnce(&mut RevTree) -> T,
    ) -> StorageResult<(u64, T)> {
        self.try_update_tree(doc_id, feed, |tree| Ok::<T, std::convert::Infallible>(modify(tree)))
            .map(|result| match result {
                Ok(written) => written,
                Err(never) => match never {},
            })
    }

    /// `update_tree` for writes that may decide, after seeing the current
    /// tree, not to write at all (e.g. `DELETE ?rev=` of a rev that's no
    /// longer a leaf). The decision is made inside the write lock, so it
    /// can't go stale before the write; on `Err` nothing is written.
    pub fn try_update_tree<T, E>(
        &self,
        doc_id: &str,
        feed: &ChangeFeed,
        modify: impl FnOnce(&mut RevTree) -> Result<T, E>,
    ) -> StorageResult<Result<(u64, T), E>> {
        let mut modify = Some(modify);
        let mut results = self.try_update_trees(&[doc_id], feed, |_, tree| (modify.take().expect("called once"))(tree))?;
        Ok(results.pop().expect("one result per doc"))
    }

    /// Several writes in one go — a `_bulk_docs` batch: `modify(i, tree)`
    /// is called for `doc_ids[i]`, in order, under one hold of the write
    /// lock, and what succeeds is written in transactions of up to
    /// `BATCH_CHUNK` docs.
    ///
    /// Not one transaction per doc: sled 0.34 pays ~0.5 ms per transaction
    /// at the next flush, and a batch is flushed before it's acknowledged,
    /// so per-doc transactions made a 2,000-doc batch take ~1.4 s (see
    /// `doc/changelog.md`). Not one for the whole batch either: a
    /// transaction holds all its writes in memory until it commits. Each
    /// doc's tree and `_changes` entry are always in the same transaction.
    /// An id may appear more than once; later entries see earlier ones.
    pub fn try_update_trees<T, E>(
        &self,
        doc_ids: &[&str],
        feed: &ChangeFeed,
        mut modify: impl FnMut(usize, &mut RevTree) -> Result<T, E>,
    ) -> StorageResult<Vec<Result<(u64, T), E>>> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Latest tree and seq of every doc this batch touched, in first-touch order.
        let mut pending: Vec<(&str, RevTree, u64)> = Vec::new();
        let mut slots: HashMap<&str, usize> = HashMap::new();
        let mut results = Vec::with_capacity(doc_ids.len());
        for (i, &doc_id) in doc_ids.iter().enumerate() {
            let slot = slots.get(doc_id).copied();
            let mut tree = match slot {
                Some(slot) => pending[slot].1.clone(),
                None => self.get_tree(doc_id)?.unwrap_or_default(),
            };
            match modify(i, &mut tree) {
                Ok(out) => {
                    tree.prune(REVS_LIMIT);
                    let seq = self.next_seq()?;
                    match slot {
                        Some(slot) => pending[slot] = (doc_id, tree, seq),
                        None => {
                            slots.insert(doc_id, pending.len());
                            pending.push((doc_id, tree, seq));
                        }
                    }
                    results.push(Ok((seq, out)));
                }
                Err(e) => results.push(Err(e)),
            }
            if pending.len() >= BATCH_CHUNK {
                self.commit_chunk(&mut pending, feed)?;
                slots.clear(); // later entries for these ids read them back from storage
            }
        }
        self.commit_chunk(&mut pending, feed)?;
        Ok(results)
    }

    fn commit_chunk(&self, pending: &mut Vec<(&str, RevTree, u64)>, feed: &ChangeFeed) -> StorageResult<()> {
        if pending.is_empty() {
            return Ok(());
        }
        self.put_trees(pending)?;
        let mut events: Vec<_> = pending.iter().map(|(id, _, seq)| ChangeEvent { seq: *seq, doc_id: id.to_string() }).collect();
        events.sort_by_key(|e| e.seq);
        for event in events {
            feed.publish(event);
        }
        pending.clear();
        Ok(())
    }

    /// `generate_id()` starts at 0, but `since=0` means "from the very
    /// start" (no prior checkpoint) — real CouchDB's own convention,
    /// where a seq token is never the same value as the "nothing seen
    /// yet" starting point. `changes_since` correctly treats `since` as
    /// exclusive (`seq > since`), so a raw 0 here would make the very
    /// first document ever written to a fresh database permanently
    /// invisible to any `_changes?since=0` poll — which is what every
    /// fresh `db.sync()` starts with. Shifting by 1 keeps persisted seq
    /// values in 1..=u64::MAX, so 0 is never a real assigned value and
    /// can safely mean only "nothing yet." Found live-testing; see
    /// `doc/changelog.md`.
    fn next_seq(&self) -> StorageResult<u64> {
        Ok(self.base.generate_id()? + 1)
    }

    /// Writes each doc's tree and its `_changes` entry in one transaction.
    /// Written separately, a crash between them could keep the tree (so
    /// `_revs_diff` says "have it" and the client never re-sends) but lose
    /// the seq entry (so `_changes` never lists it and no other client ever
    /// pulls it). Each doc's previous `_changes` entry is removed in the
    /// same transaction: one entry per doc, so `seq_log` is bounded by the
    /// doc count instead of growing with every write.
    fn put_trees(&self, writes: &[(&str, RevTree, u64)]) -> StorageResult<()> {
        let encoded = writes
            .iter()
            .map(|(doc_id, tree, seq)| {
                let stored = StoredTree::try_from(tree)
                    .map_err(|e| StorageError::Corrupt(format!("failed to encode doc body for {doc_id:?}: {e}")))?;
                let bytes = bincode_options()
                    .serialize(&stored)
                    .map_err(|e| StorageError::Corrupt(format!("failed to encode revtree for {doc_id:?}: {e}")))?;
                Ok((*doc_id, bytes, *seq))
            })
            .collect::<StorageResult<Vec<_>>>()?;
        (&self.docs, &self.seq_log, &self.seq_by_doc)
            .transaction(|(docs, seq_log, seq_by_doc)| -> Result<(), ConflictableTransactionError<()>> {
                for (doc_id, bytes, seq) in &encoded {
                    docs.insert(doc_id.as_bytes(), bytes.as_slice())?;
                    if let Some(previous) = seq_by_doc.insert(doc_id.as_bytes(), &seq.to_be_bytes())? {
                        seq_log.remove(previous.as_ref())?;
                    }
                    seq_log.insert(&seq.to_be_bytes(), doc_id.as_bytes())?;
                }
                Ok(())
            })
            .map_err(|e| match e {
                TransactionError::Storage(e) => StorageError::Sled(e),
                TransactionError::Abort(()) => StorageError::Corrupt("batch write aborted".to_string()),
            })
    }

    /// Forces everything written so far to disk. Call before acknowledging
    /// a write: sled otherwise flushes only every ~500 ms in the
    /// background, and Offlog stops this process with TerminateProcess —
    /// no shutdown hook runs — so an "ok" response could describe data that
    /// was never persisted (see doc/changelog.md, v0.1.7).
    pub async fn flush(&self) -> StorageResult<()> {
        self.base.flush_async().await?;
        Ok(())
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

/// CouchDB's database-name rule: starts with a lowercase letter, then
/// lowercase letters, digits, and `_ $ ( ) + - /`, at most 238 chars.
/// This also rules out `_`-prefixed system names (`_users`,
/// `_replicator`) and `:`, which keeps names from colliding with this
/// server's `{name}::{tree}` storage namespace.
pub fn valid_db_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 238
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '$' | '(' | ')' | '+' | '-' | '/'))
}

/// Whether `PUT /{name}` has created this database. `Db::open` creates
/// whatever it opens, so every route except `PUT /{db}` must check this
/// first.
pub fn db_exists(base: &sled::Db, name: &str) -> bool {
    let docs_tree = format!("{name}::docs");
    base.tree_names().iter().any(|n| n == docs_tree.as_bytes())
}

/// This server's identity for `GET /`'s `uuid`, generated on first start
/// and stored with the data, then reused forever. PouchDB builds its
/// replication id from it, so a uuid that changed on every start (as it
/// did before v0.1.7) gave every client a new replication id after each
/// restart: no checkpoint found, full re-sync from `since=0`.
pub fn server_uuid(root: &sled::Db) -> StorageResult<String> {
    const KEY: &[u8] = b"nyxdb::server_uuid";
    if let Some(stored) = root.get(KEY)? {
        return String::from_utf8(stored.to_vec()).map_err(|e| StorageError::Corrupt(format!("server uuid: {e}")));
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    // compare_and_swap so two starts racing on a fresh data dir agree.
    match root.compare_and_swap(KEY, None as Option<&[u8]>, Some(fresh.as_bytes()))? {
        Ok(()) => {
            root.flush()?;
            Ok(fresh)
        }
        Err(cas) => {
            let current = cas.current.ok_or_else(|| StorageError::Corrupt("server uuid vanished".into()))?;
            String::from_utf8(current.to_vec()).map_err(|e| StorageError::Corrupt(format!("server uuid: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

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
        let db = Db::open(&base, "testdb", &WriteLocks::default()).unwrap();

        db.update_tree("only-doc", &ChangeFeed::default(), |tree| {
            tree.insert_revision_chain(&["1-aaa".to_string()], false, serde_json::json!({"x": 1}))
        })
        .unwrap();

        let changes = db.changes_since(0).unwrap();
        assert_eq!(changes.len(), 1, "the first-ever write must be visible to since=0, got: {changes:?}");
        assert_eq!(changes[0].1, "only-doc");

        // Also confirm no persisted seq is ever the sentinel value 0 —
        // that's what "since=0" needs to mean exclusively "nothing yet."
        assert!(changes[0].0 > 0);
    }

    /// Appends one plain edit on top of the current winner — what the
    /// app-write path does for every `PUT`.
    fn edit(db: &Db, doc_id: &str, n: usize, body: &serde_json::Value) {
        db.update_tree(doc_id, &ChangeFeed::default(), |tree| {
            let parent = tree.winner().cloned();
            let rev = format!("{}-h{n:06}", n + 1);
            tree.nodes.insert(rev, RevNode { parent, deleted: false, body: body.clone() });
        })
        .unwrap();
    }

    /// A long-lived doc must not grow without bound (see
    /// doc/changelog.md, v0.1.7). Every node used to keep its full body
    /// forever, the whole tree was rewritten on every write, and every
    /// write appended a `_changes` entry that was never removed — so a doc
    /// edited N times cost O(N) bodies on disk, O(N) per write, and O(N)
    /// seq entries for every `since=0` scan to wade through.
    #[test]
    fn long_history_stays_bounded() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "testdb", &WriteLocks::default()).unwrap();
        let body = serde_json::json!({ "payload": "x".repeat(2048) });

        for n in 0..10 {
            edit(&db, "doc", n, &body);
        }
        let after_10 = db.docs.get("doc").unwrap().unwrap().len();
        for n in 10..1500 {
            edit(&db, "doc", n, &body);
        }
        let after_1500 = db.docs.get("doc").unwrap().unwrap().len();

        // One 2 KB body (the leaf) plus at most 1000 small rev ids — not
        // 1500 bodies (~3 MB).
        assert!(
            after_1500 < after_10 + 64 * 1024,
            "stored tree grew from {after_10} to {after_1500} bytes over 1490 edits"
        );
        let tree = db.get_tree("doc").unwrap().unwrap();
        assert!(tree.nodes.len() <= 1000, "kept {} revisions, revs_limit is 1000", tree.nodes.len());
        assert_eq!(db.seq_log.len(), 1, "one doc must have exactly one _changes entry");
        assert_eq!(db.changes_since(0).unwrap().len(), 1);
    }

    /// Databases written before v0.1.7 have one `seq_log` entry per write
    /// and no doc → seq index. Opening one must collapse that to one entry
    /// per doc (the newest), so later writes can keep it that way.
    #[test]
    fn opening_a_pre_index_database_migrates_its_seq_log() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let locks = WriteLocks::default();
        {
            let db = Db::open(&base, "legacy", &locks).unwrap();
            for (seq, id) in [(1u64, "a"), (2, "b"), (3, "a"), (4, "a"), (5, "b"), (6, "c")] {
                db.seq_log.insert(seq.to_be_bytes(), id).unwrap();
            }
        }
        let db = Db::open(&base, "legacy", &locks).unwrap();
        let mut entries = db.changes_since(0).unwrap();
        entries.sort();
        assert_eq!(entries, vec![(4, "a".to_string()), (5, "b".to_string()), (6, "c".to_string())]);

        edit(&db, "a", 10, &serde_json::json!({}));
        assert_eq!(db.seq_log.len(), 3, "a later write must replace a's entry, not add one");
    }

    #[test]
    fn server_uuid_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("nyxdb-uuid-test-{}", uuid::Uuid::new_v4()));
        let first = {
            let root = sled::open(&dir).unwrap();
            let uuid = server_uuid(&root).unwrap();
            assert_eq!(server_uuid(&root).unwrap(), uuid, "stable within one run");
            uuid
        };
        let second = {
            let root = sled::open(&dir).unwrap();
            server_uuid(&root).unwrap()
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(first, second, "the uuid must be the same after reopening the data dir");
    }

    /// Compaction deletes the bytes of attachments no revision uses any
    /// more — replaced ones, and ones only a pruned (non-leaf) revision had
    /// — and keeps every leaf's, conflict branches included.
    #[test]
    fn compaction_collects_only_unreferenced_attachments() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "gc", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();
        let stub = |data: &[u8]| {
            let digest = crate::attachments::store_raw_attachment(&db, data).unwrap();
            json!({"_attachments": {"f": {"stub": true, "digest": digest, "length": data.len()}}})
        };
        let write = |revs: &[&str], body: Value| {
            let revs: Vec<String> = revs.iter().map(|r| r.to_string()).collect();
            db.update_tree("doc", &feed, |tree| tree.insert_revision_chain(&revs, false, body)).unwrap();
        };
        let (old, kept, branch) = (stub(b"old"), stub(b"kept"), stub(b"branch"));
        write(&["1-a"], old.clone());
        write(&["2-b", "1-a"], kept.clone());
        write(&["2-c", "1-a"], branch.clone());
        stub(b"never referenced");

        assert_eq!(db.compact_attachments().unwrap(), 2);
        let digest = |body: &Value| body["_attachments"]["f"]["digest"].as_str().unwrap().to_string();
        assert!(db.get_attachment(&digest(&kept)).unwrap().is_some());
        assert!(db.get_attachment(&digest(&branch)).unwrap().is_some());
        assert!(db.get_attachment(&digest(&old)).unwrap().is_none());
        assert_eq!(db.attachments.len(), 2);
        assert_eq!(db.compact_attachments().unwrap(), 0);
    }

    /// A batch writes in one transaction but must behave like the same
    /// writes made one by one: a repeated id sees its earlier change, a
    /// failed entry writes nothing, and each doc ends with one `_changes`
    /// entry at its last write.
    #[test]
    fn a_batch_behaves_like_sequential_writes() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "batch", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();
        let ids = ["a", "b", "a", "c"];
        let results = db
            .try_update_trees(&ids, &feed, |i, tree| {
                if ids[i] == "c" {
                    return Err("rejected");
                }
                let parent = tree.winner().cloned();
                let gen = parent.as_ref().map_or(1, |p| p[..1].parse::<u64>().unwrap() + 1);
                let rev = format!("{gen}-{i}");
                tree.nodes.insert(rev.clone(), RevNode { parent, deleted: false, body: serde_json::json!({ "i": i }) });
                Ok(rev)
            })
            .unwrap();
        let revs: Vec<_> = results.iter().map(|r| r.as_ref().map(|(_, rev)| rev.clone()).map_err(|e| *e)).collect();
        assert_eq!(revs, vec![Ok("1-0".to_string()), Ok("1-1".to_string()), Ok("2-2".to_string()), Err("rejected")]);
        assert_eq!(db.get_tree("a").unwrap().unwrap().winner().map(String::as_str), Some("2-2"));
        assert!(db.get_tree("c").unwrap().is_none());
        let changes = db.changes_since(0).unwrap();
        let ids_in_changes: Vec<_> = changes.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids_in_changes, vec!["b", "a"], "one entry per doc, at its last write");
        assert_eq!(changes[1].0, results[2].as_ref().unwrap().0, "a's entry is its second write's seq");
    }

    /// A batch longer than one transaction chunk: a doc written again
    /// after its first write was committed must build on that write.
    #[test]
    fn a_batch_spanning_chunks_keeps_every_write() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "chunks", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();
        let docs = BATCH_CHUNK + 22;
        let ids: Vec<String> = (0..2 * docs).map(|i| format!("d{}", i % docs)).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        db.try_update_trees(&id_refs, &feed, |i, tree| {
            let parent = tree.winner().cloned();
            let gen = if parent.is_some() { 2 } else { 1 };
            tree.nodes.insert(format!("{gen}-{i}"), RevNode { parent, deleted: false, body: serde_json::json!({}) });
            Ok::<_, ()>(())
        })
        .unwrap();
        for id in &ids[..docs] {
            let tree = db.get_tree(id).unwrap().unwrap();
            assert!(tree.winner().unwrap().starts_with("2-"), "{id} lost its first write: {:?}", tree.leaves());
            assert_eq!(tree.leaves().len(), 1, "{id} branched");
        }
        assert_eq!(db.changes_since(0).unwrap().len(), docs);
    }
}
