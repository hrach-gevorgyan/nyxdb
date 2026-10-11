//! sled-backed storage: one KV entry per document holding its serialized
//! revision tree, plus an append-only `(seq, doc_id)` log for `_changes`.
//! See plan §4.3, §5.

use crate::changes::{ChangeEvent, ChangeFeed};
use crate::revtree::{RevId, RevNode, RevTree, REVS_LIMIT};
use bincode::Options;
use sled::transaction::{ConflictableTransactionError, TransactionError};
use sled::Transactional;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

/// One write lock per database, shared across requests (a `Db` is opened
/// fresh per request, so the lock has to live somewhere longer-lived —
/// `AppState`). See `Db::update_tree` for why it exists.
#[derive(Clone, Default)]
pub struct WriteLocks(Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>);

impl WriteLocks {
    fn for_db(&self, name: &str) -> Arc<Mutex<()>> {
        let mut locks = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(name.to_string()).or_default().clone()
    }
}

#[derive(Clone)]
pub struct Db {
    write_lock: Arc<Mutex<()>>,
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
        let db = Self {
            write_lock: locks.for_db(name),
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
        let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut tree = self.get_tree(doc_id)?.unwrap_or_default();
        let out = modify(&mut tree);
        tree.prune(REVS_LIMIT);
        let seq = self.put_tree(doc_id, &tree)?;
        feed.publish(ChangeEvent { seq, doc_id: doc_id.to_string() });
        Ok((seq, out))
    }

    fn put_tree(&self, doc_id: &str, tree: &RevTree) -> StorageResult<u64> {
        let stored = StoredTree::try_from(tree)
            .map_err(|e| StorageError::Corrupt(format!("failed to encode doc body for {doc_id:?}: {e}")))?;
        let bytes = bincode_options()
            .serialize(&stored)
            .map_err(|e| StorageError::Corrupt(format!("failed to encode revtree for {doc_id:?}: {e}")))?;
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
        // The tree and its `_changes` entry land in one transaction. Written
        // separately, a crash between them could keep the tree (so
        // `_revs_diff` says "have it" and the client never re-sends) but
        // lose the seq entry (so `_changes` never lists it and no other
        // client ever pulls it). The doc's previous `_changes` entry is
        // removed in the same transaction: one entry per doc, so `seq_log`
        // is bounded by the doc count instead of growing with every write.
        (&self.docs, &self.seq_log, &self.seq_by_doc)
            .transaction(|(docs, seq_log, seq_by_doc)| -> Result<(), ConflictableTransactionError<()>> {
                docs.insert(doc_id.as_bytes(), bytes.as_slice())?;
                if let Some(previous) = seq_by_doc.insert(doc_id.as_bytes(), &seq.to_be_bytes())? {
                    seq_log.remove(previous.as_ref())?;
                }
                seq_log.insert(&seq.to_be_bytes(), doc_id.as_bytes())?;
                Ok(())
            })
            .map_err(|e| match e {
                TransactionError::Storage(e) => StorageError::Sled(e),
                TransactionError::Abort(()) => StorageError::Corrupt(format!("write of {doc_id:?} aborted")),
            })?;
        Ok(seq)
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
}
