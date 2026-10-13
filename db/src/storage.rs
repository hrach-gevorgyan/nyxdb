//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, a `(seq, doc_id)` log for `_changes` (one entry per doc,
//! its latest write), `_local` docs, and the md5 -> sha256 index of
//! attachments, whose bytes are files (`crate::blobs`).

use crate::blobs::{BlobStore, SweepStats};
use crate::changes::{ChangeEvent, ChangeFeed};
use crate::revtree::{RevId, RevNode, RevTree, REVS_LIMIT};
use bincode::Options;
use sled::transaction::{ConflictableTransactionError, TransactionError};
use sled::Transactional;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::Duration;

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
    /// Attachment files (`crate::blobs`).
    Io(std::io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Sled(e) => write!(f, "storage error: {e}"),
            StorageError::Corrupt(msg) => write!(f, "corrupt stored data: {msg}"),
            StorageError::Io(e) => write!(f, "attachment file error: {e}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

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

/// Keys of the per-database `meta` tree: live and deleted doc counts.
const LIVE: &[u8] = b"doc_count";
const DELETED: &[u8] = b"doc_del_count";

fn read_count(value: Option<sled::IVec>) -> u64 {
    value.and_then(|v| v.as_ref().try_into().ok()).map(u64::from_be_bytes).unwrap_or(0)
}

/// A winners-index value: `[deleted flag][winning rev]`.
fn encode_winner(rev: &str, deleted: bool) -> Vec<u8> {
    let mut value = Vec::with_capacity(1 + rev.len());
    value.push(deleted as u8);
    value.extend_from_slice(rev.as_bytes());
    value
}

fn decode_winner(value: &[u8]) -> StorageResult<(String, bool)> {
    let (&flag, rev) = value.split_first().ok_or_else(|| StorageError::Corrupt("empty winners entry".into()))?;
    let rev = String::from_utf8(rev.to_vec()).map_err(|e| StorageError::Corrupt(format!("winners entry: {e}")))?;
    Ok((rev, flag == 1))
}

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
    /// doc id → `[deleted flag][winning rev]`, kept in each write's
    /// transaction, so `_all_docs` and `POST _all_docs` read it instead of
    /// parsing every revision tree.
    pub(crate) winners: sled::Tree,
    /// Live and deleted doc counts (`GET /{db}`), kept in each write's
    /// transaction.
    pub(crate) meta: sled::Tree,
    /// Attachment bytes as files, one per distinct content, named by its
    /// sha256 — content-addressed, so identical content is stored once
    /// however many revisions refer to it. Document bodies hold only a
    /// stub (`{"stub":true,"digest":...,"length":...}`). See
    /// `crate::blobs` and `crate::attachments`.
    blobs: BlobStore,
    /// Where attachment bytes lived before v0.1.8 (`"sha256-<hex>"` →
    /// bytes). Emptied by `migrate_legacy_attachments`; read as a fallback
    /// until then.
    pub legacy_attachments: sled::Tree,
    /// md5 digest (`"md5-<base64>"`, what stubs expose) → content key
    /// (`"sha256-<hex>"`).
    attachment_index: sled::Tree,
}

impl Db {
    /// `att_root` is the folder holding every database's attachment folder
    /// (`<data>/att`).
    pub fn open(base: &sled::Db, att_root: &Path, name: &str, locks: &WriteLocks) -> StorageResult<Self> {
        let locks = locks.for_db(name);
        let db = Self {
            write_lock: locks.write,
            attachment_gate: locks.attachments,
            blobs: BlobStore::for_db(att_root, name),
            base: base.clone(),
            docs: base.open_tree(format!("{name}::docs"))?,
            local: base.open_tree(format!("{name}::local"))?,
            seq_log: base.open_tree(format!("{name}::seq"))?,
            seq_by_doc: base.open_tree(format!("{name}::seqidx"))?,
            winners: base.open_tree(format!("{name}::winners"))?,
            meta: base.open_tree(format!("{name}::meta"))?,
            legacy_attachments: base.open_tree(format!("{name}::attachments"))?,
            attachment_index: base.open_tree(format!("{name}::attmd5"))?,
        };
        if db.seq_by_doc.is_empty() && !db.seq_log.is_empty() {
            db.migrate_seq_index()?;
        }
        if db.meta.is_empty() && !db.docs.is_empty() {
            db.build_winners_index()?;
        }
        Ok(db)
    }

    /// One-time upgrade for databases written before v0.1.8: builds the
    /// winners index and the counts from the trees.
    fn build_winners_index(&self) -> StorageResult<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.meta.is_empty() {
            return Ok(()); // another request built it while we waited
        }
        let (mut live, mut deleted) = (0u64, 0u64);
        let mut batch = sled::Batch::default();
        for id in self.doc_ids()? {
            let Some(tree) = self.get_tree(&id)? else { continue };
            let Some(rev) = tree.winner() else { continue };
            let is_deleted = tree.nodes[rev].deleted;
            if is_deleted { deleted += 1 } else { live += 1 }
            batch.insert(id.as_bytes(), encode_winner(rev, is_deleted));
        }
        self.winners.apply_batch(batch)?;
        // Counts last: their presence marks the index complete.
        self.meta.insert(DELETED, &deleted.to_be_bytes())?;
        self.meta.insert(LIVE, &live.to_be_bytes())?;
        self.base.flush()?;
        Ok(())
    }

    /// Live and deleted doc counts.
    pub fn doc_counts(&self) -> StorageResult<(u64, u64)> {
        Ok((read_count(self.meta.get(LIVE)?), read_count(self.meta.get(DELETED)?)))
    }

    /// A doc's winning rev and whether it is deleted, from the index.
    pub fn winner(&self, doc_id: &str) -> StorageResult<Option<(String, bool)>> {
        self.winners.get(doc_id)?.map(|v| decode_winner(&v)).transpose()
    }

    /// `_all_docs` page: live docs in id order (reverse if `descending`),
    /// from `start` to `end` inclusive (in the scan's direction), after
    /// skipping `skip`, at most `limit`. Reads the winners index only and
    /// stops at `limit`. Also returns `offset`: how many live docs precede
    /// the first returned row (counted only when `start` is given).
    #[allow(clippy::type_complexity)]
    pub fn live_docs_page(
        &self,
        start: Option<&str>,
        end: Option<&str>,
        descending: bool,
        skip: usize,
        limit: usize,
    ) -> StorageResult<(Vec<(String, String)>, u64)> {
        type Entry = sled::Result<(sled::IVec, sled::IVec)>;
        let live = |entry: Entry| -> Option<StorageResult<(String, String)>> {
            match entry {
                Err(e) => Some(Err(e.into())),
                Ok((id, value)) => match decode_winner(&value) {
                    Err(e) => Some(Err(e)),
                    Ok((_, true)) => None,
                    Ok((rev, false)) => Some(Ok((String::from_utf8_lossy(&id).into_owned(), rev))),
                },
            }
        };
        let (lo, hi) = if descending { (end, start) } else { (start, end) };
        let range = match (lo, hi) {
            (Some(lo), Some(hi)) if lo > hi => return Ok((Vec::new(), 0)),
            (Some(lo), Some(hi)) => self.winners.range(lo..=hi),
            (Some(lo), None) => self.winners.range(lo..),
            (None, Some(hi)) => self.winners.range(..=hi),
            (None, None) => self.winners.iter(),
        };
        let rows: Vec<(String, String)> = if descending {
            range.rev().filter_map(live).skip(skip).take(limit).collect::<StorageResult<_>>()?
        } else {
            range.filter_map(live).skip(skip).take(limit).collect::<StorageResult<_>>()?
        };
        let before = match start {
            None => 0,
            Some(start) if descending => self.winners.range(start..).filter_map(live).filter(|r| r.as_ref().is_ok_and(|(id, _)| id.as_str() > start)).count(),
            Some(start) => self.winners.range(..start).filter_map(live).count(),
        };
        Ok((rows, (before + skip) as u64))
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
    /// written before v0.1.7, which were the content key itself) — to the
    /// content's sha256 in hex, which names its file.
    fn attachment_hex(&self, digest: &str) -> StorageResult<Option<String>> {
        let key = if digest.starts_with("md5-") {
            match self.attachment_index.get(digest)? {
                Some(key) => String::from_utf8_lossy(&key).into_owned(),
                None => return Ok(None),
            }
        } else {
            digest.to_string()
        };
        Ok(key.strip_prefix("sha256-").map(str::to_string))
    }

    pub fn get_attachment(&self, digest: &str) -> StorageResult<Option<Vec<u8>>> {
        let Some(hex) = self.attachment_hex(digest)? else { return Ok(None) };
        if let Some(bytes) = self.blobs.get(&hex)? {
            return Ok(Some(bytes));
        }
        // Not migrated yet (see `migrate_legacy_attachments`).
        Ok(self.legacy_attachments.get(format!("sha256-{hex}"))?.map(|bytes| bytes.to_vec()))
    }

    pub fn has_attachment(&self, digest: &str) -> StorageResult<bool> {
        let Some(hex) = self.attachment_hex(digest)? else { return Ok(false) };
        Ok(self.blobs.exists(&hex) || self.legacy_attachments.contains_key(format!("sha256-{hex}"))?)
    }

    /// Stores `bytes` (content key `"sha256-<hex>"`) as a file, durably,
    /// and indexes their md5 digest. Content-addressed: identical bytes are
    /// stored once. The first content to claim an md5 keeps it (a crafted
    /// md5 collision can't redirect an existing stub to other bytes).
    pub fn put_attachment(&self, key: &str, md5: &str, bytes: &[u8]) -> StorageResult<()> {
        let hex = key
            .strip_prefix("sha256-")
            .ok_or_else(|| StorageError::Corrupt(format!("attachment key is not sha256-<hex>: {key:?}")))?;
        self.blobs.put(hex, bytes)?;
        let _ = self.attachment_index.compare_and_swap(md5, None as Option<&[u8]>, Some(key.as_bytes()))?;
        Ok(())
    }

    /// Held by a write from storing (or checking) attachment bytes until
    /// its tree is committed. See `DbLocks::attachments`.
    pub fn attachment_writer(&self) -> RwLockReadGuard<'_, ()> {
        self.attachment_gate.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mark and sweep. (a) Lists every attachment referenced by any stored
    /// revision of any doc, conflict leaves included (only leaves keep
    /// bodies, `RevTree::prune`). (b) Deletes this database's attachment
    /// files that aren't on the list, and leftover temp files, but (c)
    /// never one modified less than `grace` ago: it may belong to an upload
    /// whose revision isn't committed yet. (d) The attachment gate is held
    /// exclusively throughout, so no write can add a reference between
    /// listing and deleting. md5 index entries whose bytes are gone are
    /// removed too.
    pub fn compact_attachments(&self, grace: Duration) -> StorageResult<SweepStats> {
        let _gate = self.attachment_gate.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut referenced: HashSet<String> = HashSet::new();
        for id in self.doc_ids()? {
            let Some(tree) = self.get_tree(&id)? else { continue };
            for node in tree.nodes.values() {
                let Some(attachments) = node.body.get("_attachments").and_then(|a| a.as_object()) else { continue };
                for digest in attachments.values().filter_map(|a| a.get("digest")?.as_str()) {
                    if let Some(hex) = self.attachment_hex(digest)? {
                        referenced.insert(hex);
                    }
                }
            }
        }
        let stats = self.blobs.sweep(&referenced, grace)?;
        for entry in self.attachment_index.iter() {
            let (md5, key) = entry?;
            let hex = String::from_utf8_lossy(&key).strip_prefix("sha256-").unwrap_or_default().to_string();
            if !self.blobs.exists(&hex) && !self.legacy_attachments.contains_key(&key)? {
                self.attachment_index.remove(md5)?;
            }
        }
        self.base.flush()?;
        Ok(stats)
    }

    /// Moves attachment bytes written before v0.1.8 out of sled into files.
    /// Resumable and safe to kill at any point: each sled copy is removed
    /// only after its file is durable (`BlobStore::put` fsyncs the file and
    /// its folder before returning), and a file with its final name is
    /// always complete, so a rerun just skips what's done. `progress(done,
    /// total)` is called every `every` attachments. Returns how many moved.
    pub fn migrate_legacy_attachments(&self, every: usize, mut progress: impl FnMut(usize, usize)) -> StorageResult<usize> {
        let total = self.legacy_attachments.len();
        let mut moved = 0;
        for entry in self.legacy_attachments.iter() {
            let (key, bytes) = entry?;
            let name = String::from_utf8_lossy(&key).into_owned();
            let Some(hex) = name.strip_prefix("sha256-") else {
                return Err(StorageError::Corrupt(format!("legacy attachment key {name:?}")));
            };
            self.blobs.put(hex, &bytes)?;
            self.legacy_attachments.remove(&key)?;
            moved += 1;
            if every > 0 && moved % every == 0 {
                self.base.flush()?;
                progress(moved, total);
            }
        }
        self.base.flush()?;
        Ok(moved)
    }

    /// Deletes temp files left by an earlier process killed mid-write.
    pub fn remove_stale_attachment_temps(&self) -> StorageResult<usize> {
        Ok(self.blobs.remove_stale_temps()?)
    }

    /// Deletes this database's attachment folder (`DELETE /{db}`).
    pub fn remove_attachment_files(&self) -> StorageResult<()> {
        Ok(self.blobs.remove_all()?)
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
    ///
    /// EXPECTED, DON'T "FIX": seqs (and `update_seq`) jump by roughly two
    /// million after every restart. sled hands out ids from blocks it
    /// reserves ahead on disk, and on reopen it skips past the whole last
    /// reserved block so an id is never reused after a crash. `since` is an
    /// opaque, increasing checkpoint; nothing depends on seqs being
    /// contiguous, and replacing this with a counter written on every
    /// write would cost a disk write per document.
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
    ///
    /// The doc's entry in the winners index and the live/deleted counters
    /// are updated in the same transaction (see `winner`, `doc_counts`).
    fn put_trees(&self, writes: &[(&str, RevTree, u64)]) -> StorageResult<()> {
        let encoded = writes
            .iter()
            .map(|(doc_id, tree, seq)| {
                let stored = StoredTree::try_from(tree)
                    .map_err(|e| StorageError::Corrupt(format!("failed to encode doc body for {doc_id:?}: {e}")))?;
                let bytes = bincode_options()
                    .serialize(&stored)
                    .map_err(|e| StorageError::Corrupt(format!("failed to encode revtree for {doc_id:?}: {e}")))?;
                let winner = tree.winner().map(|rev| encode_winner(rev, tree.nodes[rev].deleted));
                Ok((*doc_id, bytes, *seq, winner))
            })
            .collect::<StorageResult<Vec<_>>>()?;
        (&self.docs, &self.seq_log, &self.seq_by_doc, &self.winners, &self.meta)
            .transaction(|(docs, seq_log, seq_by_doc, winners, meta)| -> Result<(), ConflictableTransactionError<()>> {
                let (mut live, mut deleted) = (read_count(meta.get(LIVE)?), read_count(meta.get(DELETED)?));
                for (doc_id, bytes, seq, winner) in &encoded {
                    docs.insert(doc_id.as_bytes(), bytes.as_slice())?;
                    if let Some(previous) = seq_by_doc.insert(doc_id.as_bytes(), &seq.to_be_bytes())? {
                        seq_log.remove(previous.as_ref())?;
                    }
                    seq_log.insert(&seq.to_be_bytes(), doc_id.as_bytes())?;
                    let Some(winner) = winner else { continue };
                    match winners.insert(doc_id.as_bytes(), winner.as_slice())?.map(|old| old.first() == Some(&1)) {
                        Some(true) => deleted -= 1,
                        Some(false) => live -= 1,
                        None => {}
                    }
                    if winner[0] == 1 { deleted += 1 } else { live += 1 }
                }
                meta.insert(LIVE, &live.to_be_bytes())?;
                meta.insert(DELETED, &deleted.to_be_bytes())?;
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

/// A temporary folder for unit tests' attachment files, deleted on drop.
#[cfg(test)]
pub(crate) struct TestDir(pub std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new() -> Self {
        Self(std::env::temp_dir().join(format!("nyxdb-unit-{}", uuid::Uuid::new_v4())))
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub type SharedRoot = Arc<sled::Db>;

/// Opens the sled store in `dir`, or says in one line why it can't be used
/// — which `main` turns into exit code 3 ("the store can't be opened or is
/// damaged"; Offlog then moves the folder aside and starts fresh).
///
/// Two failure modes beyond sled returning an error: sled can panic on
/// damaged files, and — worse — when there is no snapshot to check
/// against (a hard kill before sled's first snapshot, which is how Offlog
/// stops NyxDB), it silently recovers a damaged `db` file as an *empty*
/// store. v0.1.7 then served that empty store under a new server id and
/// wrote over the damaged file. A store that existed before (its `conf`
/// file is there) but opens with no database and no server id is
/// therefore treated as damaged, before anything is written to it. A
/// store that really was empty loses nothing by being reset.
///
/// Not detectable: damage that cuts the log short. sled recovers the
/// writes before the damage, exactly as after a crash.
pub fn open_store(config: sled::Config, dir: &Path) -> Result<sled::Db, String> {
    let existed = dir.join("conf").is_file();
    // sled may panic on damaged files: report it as one line, not a panic.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| config.open()));
    std::panic::set_hook(previous_hook);
    let root = match opened {
        Ok(Ok(root)) => root,
        Ok(Err(e)) => return Err(format!("can't open the store in {dir:?}: {e}")),
        Err(panic) => {
            let why = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()));
            return Err(format!("the store in {dir:?} is damaged: {}", why.unwrap_or_else(|| "sled panicked while opening it".into())));
        }
    };
    let empty = root.is_empty() && root.tree_names().iter().all(|name| name.as_ref() == b"__sled__default");
    if existed && empty {
        return Err(format!("the store in {dir:?} is damaged: it existed, but no data could be read from it"));
    }
    Ok(root)
}

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
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "testdb", &WriteLocks::default()).unwrap();

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
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "testdb", &WriteLocks::default()).unwrap();
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
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let locks = WriteLocks::default();
        {
            let db = Db::open(&base, &att.0, "legacy", &locks).unwrap();
            for (seq, id) in [(1u64, "a"), (2, "b"), (3, "a"), (4, "a"), (5, "b"), (6, "c")] {
                db.seq_log.insert(seq.to_be_bytes(), id).unwrap();
            }
        }
        let db = Db::open(&base, &att.0, "legacy", &locks).unwrap();
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

    /// Compaction deletes the files of attachments no revision uses any
    /// more — replaced ones, and ones only a pruned (non-leaf) revision had
    /// — and keeps every leaf's, conflict branches included.
    #[test]
    fn compaction_collects_only_unreferenced_attachments() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "gc", &WriteLocks::default()).unwrap();
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

        let files = || std::fs::read_dir(att.0.join("gc")).unwrap().count();
        assert_eq!(files(), 4);
        assert_eq!(db.compact_attachments(Duration::ZERO).unwrap().files, 2);
        let digest = |body: &Value| body["_attachments"]["f"]["digest"].as_str().unwrap().to_string();
        assert!(db.get_attachment(&digest(&kept)).unwrap().is_some());
        assert!(db.get_attachment(&digest(&branch)).unwrap().is_some(), "a conflict leaf's attachment is kept");
        assert!(db.get_attachment(&digest(&old)).unwrap().is_none());
        assert!(!db.has_attachment(&digest(&old)).unwrap(), "its md5 index entry is gone too");
        assert_eq!(files(), 2);
        assert_eq!(db.compact_attachments(Duration::ZERO).unwrap().files, 0);
    }

    /// An attachment stored by a write whose revision isn't committed yet
    /// must survive a sweep that runs meanwhile: the sweep waits for the
    /// write (attachment gate), and the age rule protects it regardless.
    #[test]
    fn a_sweep_never_removes_an_attachment_being_uploaded() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "race", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();

        // With the age rule: stored, not referenced, newer than the grace.
        let pending = crate::attachments::store_raw_attachment(&db, b"mid-upload").unwrap();
        assert_eq!(db.compact_attachments(Duration::from_secs(3600)).unwrap().files, 0);
        assert!(db.has_attachment(&pending).unwrap());

        // With no grace at all, the gate alone: the sweep starts while the
        // write holds it, so it can only list references after the commit.
        let (stored_tx, stored_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let _gate = db.attachment_writer();
                let digest = crate::attachments::store_raw_attachment(&db, b"uploaded during the sweep").unwrap();
                stored_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                let body = json!({"_attachments": {"f": {"stub": true, "digest": digest, "length": 25}}});
                db.update_tree("doc", &feed, |tree| tree.insert_revision_chain(&["1-a".to_string()], false, body)).unwrap();
                digest
            });
            stored_rx.recv().unwrap();
            db.compact_attachments(Duration::ZERO).unwrap();
            let digest = writer.join().unwrap();
            assert!(db.has_attachment(&digest).unwrap(), "the sweep deleted an attachment committed while it waited");
        });
    }

    /// Attachments written before v0.1.8 live in sled. Moving them to files
    /// must survive an interruption at any point and never lose one.
    #[test]
    fn an_interrupted_attachment_migration_resumes_without_loss() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "old", &WriteLocks::default()).unwrap();
        let mut digests = Vec::new();
        for i in 0..50u32 {
            // What v0.1.7 stored: bytes in sled under "sha256-<hex>", md5 indexed.
            let bytes = format!("legacy attachment {i}").into_bytes();
            let key = crate::attachments::content_key_for_tests(&bytes);
            let md5 = crate::attachments::md5_digest(&bytes);
            db.legacy_attachments.insert(key.as_bytes(), bytes.clone()).unwrap();
            db.attachment_index.insert(md5.as_bytes(), key.as_bytes()).unwrap();
            digests.push((md5, bytes));
        }
        // A crash after a file was renamed into place but before its sled
        // copy was removed, and a crash mid-write (temp file left over).
        let (first_md5, first_bytes) = &digests[0];
        let first_hex = db.attachment_hex(first_md5).unwrap().unwrap();
        db.blobs.put(&first_hex, first_bytes).unwrap();
        std::fs::write(att.0.join("old").join(".tmp-interrupted"), b"half").unwrap();

        // Interrupted partway: stop after 20 (as if killed).
        let mut calls = 0;
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.migrate_legacy_attachments(10, |_, _| {
                calls += 1;
                if calls == 2 {
                    panic!("killed");
                }
            })
        }));
        assert!(interrupted.is_err());
        assert_eq!(db.legacy_attachments.len(), 30, "the first 20 moved before the kill");
        for (md5, bytes) in &digests {
            assert_eq!(db.get_attachment(md5).unwrap().as_ref(), Some(bytes), "readable mid-migration");
        }

        let moved = db.migrate_legacy_attachments(10, |_, _| {}).unwrap();
        assert_eq!(moved, 30);
        assert_eq!(db.legacy_attachments.len(), 0);
        for (md5, bytes) in &digests {
            assert_eq!(db.get_attachment(md5).unwrap().as_ref(), Some(bytes), "lost {md5}");
        }
        db.compact_attachments(Duration::ZERO).unwrap();
        let files = std::fs::read_dir(att.0.join("old")).unwrap().count();
        assert_eq!(files, 0, "unreferenced and temp files are swept once no doc uses them");
    }

    /// What the counters and the winners index must say, computed the slow
    /// way: by reading every tree.
    fn counted_from_trees(db: &Db) -> (u64, u64, Vec<(String, String, bool)>) {
        let (mut live, mut deleted, mut winners) = (0, 0, Vec::new());
        for id in db.doc_ids().unwrap() {
            let tree = db.get_tree(&id).unwrap().unwrap();
            let rev = tree.winner().unwrap().clone();
            let is_deleted = tree.nodes[&rev].deleted;
            if is_deleted { deleted += 1 } else { live += 1 }
            winners.push((id, rev, is_deleted));
        }
        (live, deleted, winners)
    }

    fn indexed(db: &Db) -> (u64, u64, Vec<(String, String, bool)>) {
        let (live, deleted) = db.doc_counts().unwrap();
        let winners = db
            .doc_ids()
            .unwrap()
            .into_iter()
            .map(|id| {
                let (rev, is_deleted) = db.winner(&id).unwrap().unwrap();
                (id, rev, is_deleted)
            })
            .collect();
        (live, deleted, winners)
    }

    /// `GET /{db}` and `_all_docs` used to parse every doc and tombstone
    /// ever written. Counts and each doc's winner are now kept up to date
    /// in the write transaction; they must match the trees after every
    /// kind of write, after a reopen, and after rebuilding them for a
    /// database written before they existed.
    #[test]
    fn counts_and_winners_follow_every_kind_of_write() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let locks = WriteLocks::default();
        let db = Db::open(&base, &att.0, "counts", &locks).unwrap();
        let feed = ChangeFeed::default();
        let chain = |revs: &[&str]| revs.iter().map(|r| r.to_string()).collect::<Vec<_>>();
        let put = |id: &str, revs: &[&str], deleted: bool| {
            db.update_tree(id, &feed, |tree| tree.insert_revision_chain(&chain(revs), deleted, json!({}))).unwrap();
        };
        assert_eq!(indexed(&db), (0, 0, vec![]));
        put("a", &["1-a"], false);
        put("b", &["1-b"], false);
        put("c", &["1-c"], false);
        put("a", &["2-a", "1-a"], false); // update
        put("b", &["2-b", "1-b"], true); // delete
        put("c", &["2-x", "1-c"], false); // conflict: two live leaves
        put("c", &["2-y", "1-c"], true); // a deleted branch: c stays live
        assert_eq!(indexed(&db), counted_from_trees(&db));
        assert_eq!(db.doc_counts().unwrap(), (2, 1));
        put("c", &["3-x", "2-x", "1-c"], true); // every leaf deleted now
        put("b", &["3-b", "2-b", "1-b"], false); // recreated
        assert_eq!(db.doc_counts().unwrap(), (2, 1));
        // A batch touching one doc twice, across a chunk boundary too.
        let ids: Vec<String> = (0..BATCH_CHUNK + 10).map(|i| format!("n{}", i % (BATCH_CHUNK - 3))).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        db.try_update_trees(&id_refs, &feed, |i, tree| {
            let parent = tree.winner().cloned();
            let gen = parent.as_ref().map_or(1, |_| 2);
            tree.nodes.insert(format!("{gen}-{i}"), RevNode { parent, deleted: i % 2 == 0, body: json!({}) });
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(indexed(&db), counted_from_trees(&db));

        drop(db);
        let db = Db::open(&base, &att.0, "counts", &locks).unwrap();
        assert_eq!(indexed(&db), counted_from_trees(&db), "kept across a reopen");

        // A database written before the index existed gets it built on open.
        let expected = counted_from_trees(&db);
        db.winners.clear().unwrap();
        db.meta.clear().unwrap();
        drop(db);
        let db = Db::open(&base, &att.0, "counts", &locks).unwrap();
        assert_eq!(indexed(&db), expected, "rebuilt for an old database");
    }

    /// A batch writes in one transaction but must behave like the same
    /// writes made one by one: a repeated id sees its earlier change, a
    /// failed entry writes nothing, and each doc ends with one `_changes`
    /// entry at its last write.
    #[test]
    fn a_batch_behaves_like_sequential_writes() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "batch", &WriteLocks::default()).unwrap();
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
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "chunks", &WriteLocks::default()).unwrap();
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
