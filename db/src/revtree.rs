//! Per-document revision tree: storage, winner-picking, `_revs_diff`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub type RevId = String; // "{generation}-{hash}"

/// CouchDB's default `revs_limit`: how many revisions of history each branch
/// keeps (see `RevTree::prune`) and the most `_revisions` ever reports.
pub const REVS_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevNode {
    pub parent: Option<RevId>,
    pub deleted: bool,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevTree {
    pub nodes: HashMap<RevId, RevNode>,
}

impl RevTree {
    pub fn leaves(&self) -> Vec<&RevId> {
        let parents: std::collections::HashSet<&RevId> =
            self.nodes.values().filter_map(|n| n.parent.as_ref()).collect();
        self.nodes.keys().filter(|id| !parents.contains(id)).collect()
    }

    /// Winner-picking (CouchDB's rule): highest generation, then lexicographically
    /// highest hash as tiebreak, among non-deleted leaves (falls back to all
    /// leaves if every leaf is deleted).
    pub fn winner(&self) -> Option<&RevId> {
        let candidates: Vec<&RevId> = {
            let non_deleted: Vec<&RevId> = self
                .leaves()
                .into_iter()
                .filter(|id| !self.nodes[*id].deleted)
                .collect();
            if non_deleted.is_empty() {
                self.leaves()
            } else {
                non_deleted
            }
        };

        candidates.into_iter().max_by(|a, b| {
            let (gen_a, hash_a) = split_rev(a);
            let (gen_b, hash_b) = split_rev(b);
            gen_a.cmp(&gen_b).then_with(|| hash_a.cmp(hash_b))
        })
    }

    pub fn conflicts(&self) -> Vec<RevId> {
        let winner = self.winner().cloned();
        self.leaves()
            .into_iter()
            .filter(|id| Some((*id).clone()) != winner && !self.nodes[*id].deleted)
            .cloned()
            .collect()
    }

    /// `_revs_diff` support: of the requested rev ids, which do we NOT
    /// already have stored for this document.
    pub fn missing<'a>(&self, requested: &'a [RevId]) -> Vec<&'a RevId> {
        requested.iter().filter(|rev| !self.nodes.contains_key(*rev)).collect()
    }

    /// The revision history of `rev`: `rev` itself, then its parent, and
    /// so on back to the root, newest first — what `_revisions` and
    /// `_revs_info` are built from. Empty if `rev` isn't in the tree. A
    /// parent that's named but was never stored (a chain pushed without
    /// full ancestry) is still included — its id is known — and the walk
    /// stops there. Capped at CouchDB's default `revs_limit` (1000).
    pub fn ancestry(&self, rev: &str) -> Vec<RevId> {
        self.ancestry_limited(rev, REVS_LIMIT)
    }

    fn ancestry_limited(&self, rev: &str, limit: usize) -> Vec<RevId> {
        let mut out = Vec::new();
        let mut current = Some(rev.to_string());
        while let Some(id) = current {
            if out.len() == limit {
                break;
            }
            let node = self.nodes.get(&id);
            if node.is_none() && out.is_empty() {
                return out;
            }
            current = node.and_then(|n| n.parent.clone());
            out.push(id);
        }
        out
    }

    /// `_revisions` for `rev`: the generation it starts at, plus the
    /// hash part of every rev in its ancestry, newest first —
    /// `{"start": 3, "ids": ["c", "b", "a"]}` for `3-c`. This is what a
    /// pulling replica needs to attach an incoming revision to the
    /// history it already has instead of storing it as a new root.
    pub fn revisions(&self, rev: &str) -> Option<(u64, Vec<String>)> {
        let ancestry = self.ancestry(rev);
        let first = ancestry.first()?;
        let start = split_rev(first).0;
        Some((start, ancestry.iter().map(|r| split_rev(r).1.to_string()).collect()))
    }

    /// `latest=true` support: every leaf whose history includes `rev`
    /// (just `rev` itself if it's already a leaf), highest generation
    /// first. Empty if `rev` isn't in the tree.
    pub fn leaves_descending_from(&self, rev: &str) -> Vec<RevId> {
        if !self.nodes.contains_key(rev) {
            return Vec::new();
        }
        let mut out: Vec<RevId> =
            self.leaves().into_iter().filter(|leaf| self.ancestry(leaf).iter().any(|r| r == rev)).cloned().collect();
        out.sort_by(|a, b| {
            let (gen_a, hash_a) = split_rev(a);
            let (gen_b, hash_b) = split_rev(b);
            gen_b.cmp(&gen_a).then_with(|| hash_b.cmp(hash_a))
        });
        out
    }

    pub fn is_leaf(&self, rev: &str) -> bool {
        self.nodes.contains_key(rev) && !self.nodes.values().any(|n| n.parent.as_deref() == Some(rev))
    }

    /// Keeps a tree bounded no matter how often its doc is edited — the
    /// equivalent of CouchDB's compaction plus revs_limit stemming, done on
    /// every write:
    ///
    /// - only leaves keep a body; ancestors keep their id and parent link
    ///   (so `_revisions`/`_revs_diff` still work) but not their content;
    /// - each branch keeps its newest `revs_limit` revisions, and anything
    ///   older that no branch still reaches is dropped.
    ///
    /// Without this, a doc edited N times held N full bodies forever and
    /// every write re-serialised all of them (see doc/changelog.md, v0.1.7).
    pub fn prune(&mut self, revs_limit: usize) {
        let leaves: Vec<RevId> = self.leaves().into_iter().cloned().collect();
        let keep: HashSet<RevId> = leaves.iter().flat_map(|leaf| self.ancestry_limited(leaf, revs_limit)).collect();
        self.nodes.retain(|id, _| keep.contains(id));
        for (id, node) in self.nodes.iter_mut() {
            if !leaves.contains(id) {
                node.body = serde_json::Value::Null;
            }
        }
    }

    /// `_bulk_docs` with `new_edits:false` (real replication push, plan
    /// §3): the client dictates the exact revision id and its ancestry
    /// via `_revisions` (`{"start": N, "ids": [hash_N, hash_N-1, ...]}`,
    /// newest first) instead of us minting a new one. `chain` here is
    /// that same newest-first list of full rev ids, already resolved by
    /// the caller from `start`/`ids`.
    ///
    /// Existing nodes are never overwritten — replaying the same push
    /// twice, or receiving a chain whose tail already connects to
    /// history we have, must be idempotent and must not clobber a
    /// tombstone's `deleted` flag or an existing node's body.
    pub fn insert_revision_chain(&mut self, chain: &[RevId], leaf_deleted: bool, leaf_body: serde_json::Value) {
        for (i, rev) in chain.iter().enumerate() {
            if self.nodes.contains_key(rev) {
                continue;
            }
            let parent = chain.get(i + 1).cloned();
            let deleted = i == 0 && leaf_deleted;
            let body = if i == 0 { leaf_body.clone() } else { serde_json::json!({}) };
            self.nodes.insert(rev.clone(), RevNode { parent, deleted, body });
        }
    }
}

fn split_rev(rev: &str) -> (u64, &str) {
    match rev.split_once('-') {
        Some((gen, hash)) => (gen.parse().unwrap_or(0), hash),
        None => (0, rev),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(parent: Option<&str>) -> RevNode {
        RevNode { parent: parent.map(String::from), deleted: false, body: serde_json::json!({}) }
    }

    /// 1-a ── 2-b ── 3-c, plus a sibling branch 1-a ── 2-x.
    fn branched_tree() -> RevTree {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-a".into(), node(None));
        tree.nodes.insert("2-b".into(), node(Some("1-a")));
        tree.nodes.insert("3-c".into(), node(Some("2-b")));
        tree.nodes.insert("2-x".into(), node(Some("1-a")));
        tree
    }

    fn with_body(parent: Option<&str>, v: &str) -> RevNode {
        RevNode { parent: parent.map(String::from), deleted: false, body: serde_json::json!({ "v": v }) }
    }

    /// Like CouchDB after compaction: only leaves keep a body. Ancestors
    /// keep their id and parent link (history still works) but not their
    /// content, and are never readable.
    #[test]
    fn prune_keeps_bodies_only_on_leaves() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-a".into(), with_body(None, "a"));
        tree.nodes.insert("2-b".into(), with_body(Some("1-a"), "b"));
        tree.nodes.insert("3-c".into(), with_body(Some("2-b"), "c"));
        tree.nodes.insert("2-x".into(), with_body(Some("1-a"), "x"));
        tree.prune(1000);
        assert_eq!(tree.nodes["3-c"].body, serde_json::json!({ "v": "c" }));
        assert_eq!(tree.nodes["2-x"].body, serde_json::json!({ "v": "x" }));
        assert_eq!(tree.nodes["2-b"].body, serde_json::Value::Null);
        assert_eq!(tree.nodes["1-a"].body, serde_json::Value::Null);
        assert!(tree.is_leaf("3-c") && tree.is_leaf("2-x"));
        assert!(!tree.is_leaf("2-b") && !tree.is_leaf("1-a"));
        assert_eq!(tree.ancestry("3-c"), vec!["3-c", "2-b", "1-a"]);
    }

    /// Stemming to `revs_limit`: each branch keeps its newest `limit`
    /// revisions; anything older that no branch still reaches is dropped.
    #[test]
    fn prune_stems_every_branch_to_the_limit() {
        let mut tree = RevTree::default();
        let mut parent: Option<String> = None;
        for g in 1..=12 {
            let rev = format!("{g}-m");
            tree.nodes.insert(rev.clone(), node(parent.as_deref()));
            parent = Some(rev);
        }
        // A short branch off generation 9.
        tree.nodes.insert("10-s".into(), node(Some("9-m")));
        tree.prune(5);
        // 12-m keeps 12..8; 10-s keeps 10-s, 9..6. Nothing reaches 5..1.
        let mut kept: Vec<&str> = tree.nodes.keys().map(String::as_str).collect();
        kept.sort();
        assert_eq!(kept, vec!["10-m", "10-s", "11-m", "12-m", "6-m", "7-m", "8-m", "9-m"]);
        assert_eq!(&tree.ancestry("10-s")[..5], ["10-s", "9-m", "8-m", "7-m", "6-m"]);
        assert_eq!(tree.leaves().len(), 2);
        assert_eq!(tree.winner(), Some(&"12-m".to_string()));
    }

    #[test]
    fn ancestry_walks_back_from_the_requested_rev_newest_first() {
        let tree = branched_tree();
        assert_eq!(tree.ancestry("3-c"), vec!["3-c", "2-b", "1-a"]);
        assert_eq!(tree.ancestry("2-x"), vec!["2-x", "1-a"]);
        assert_eq!(tree.ancestry("1-a"), vec!["1-a"]);
        assert!(tree.ancestry("9-nope").is_empty());
    }

    /// A chain pushed without its full ancestry still names its parent
    /// even when that parent's node was never stored — the walk keeps
    /// the id it knows and stops there, rather than inventing history.
    #[test]
    fn ancestry_stops_at_a_known_but_unstored_parent() {
        let mut tree = RevTree::default();
        tree.nodes.insert("5-e".into(), node(Some("4-d")));
        assert_eq!(tree.ancestry("5-e"), vec!["5-e", "4-d"]);
    }

    #[test]
    fn revisions_are_start_plus_hash_parts_newest_first() {
        let tree = branched_tree();
        assert_eq!(tree.revisions("3-c"), Some((3, vec!["c".to_string(), "b".to_string(), "a".to_string()])));
        assert_eq!(tree.revisions("2-x"), Some((2, vec!["x".to_string(), "a".to_string()])));
        assert_eq!(tree.revisions("9-nope"), None);
    }

    #[test]
    fn leaves_descending_from_an_ancestor_include_every_branch() {
        let tree = branched_tree();
        assert_eq!(tree.leaves_descending_from("1-a"), vec!["3-c", "2-x"]);
        assert_eq!(tree.leaves_descending_from("2-b"), vec!["3-c"]);
        assert_eq!(tree.leaves_descending_from("3-c"), vec!["3-c"]);
        assert!(tree.leaves_descending_from("9-nope").is_empty());
    }

    #[test]
    fn single_linear_history_winner_is_the_leaf() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-bbb".into(), node(Some("1-aaa")));
        assert_eq!(tree.winner(), Some(&"2-bbb".to_string()));
    }

    #[test]
    fn conflict_picks_highest_generation_then_hash() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-bbb".into(), node(Some("1-aaa")));
        tree.nodes.insert("3-ccc".into(), node(Some("2-bbb")));
        tree.nodes.insert("3-ddd".into(), node(Some("2-bbb")));
        assert_eq!(tree.winner(), Some(&"3-ddd".to_string()));
        assert_eq!(tree.conflicts(), vec!["3-ccc".to_string()]);
    }

    /// Two branches of unequal depth: the deeper one wins regardless of
    /// hash, since generation is compared first.
    #[test]
    fn unequal_depth_branches_deeper_generation_wins_even_with_lower_hash() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-zzz".into(), node(Some("1-aaa"))); // branch A, stops here
        tree.nodes.insert("2-bbb".into(), node(Some("1-aaa"))); // branch B, continues
        tree.nodes.insert("3-aaa".into(), node(Some("2-bbb")));
        assert_eq!(tree.winner(), Some(&"3-aaa".to_string()));
        assert_eq!(tree.conflicts(), vec!["2-zzz".to_string()]);
    }

    /// Once a client explicitly DELETEs the losing branch (a
    /// new deleted revision on top of it, the branch is never pruned),
    /// that branch drops out of `_conflicts` even though the node stays
    /// in the tree.
    #[test]
    fn resolved_conflict_deleted_branch_excluded_from_conflicts_but_not_pruned() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-bbb".into(), node(Some("1-aaa")));
        tree.nodes.insert("3-ccc".into(), node(Some("2-bbb"))); // loser
        tree.nodes.insert("3-ddd".into(), node(Some("2-bbb"))); // winner
        let mut deleted_tombstone = node(Some("3-ccc"));
        deleted_tombstone.deleted = true;
        tree.nodes.insert("4-eee".into(), deleted_tombstone);

        assert_eq!(tree.winner(), Some(&"3-ddd".to_string()));
        assert!(tree.conflicts().is_empty());
        // History is preserved, not pruned.
        assert!(tree.nodes.contains_key("3-ccc"));
        assert!(tree.nodes.contains_key("4-eee"));
    }

    /// Deleting a document's only branch leaves every leaf deleted; the
    /// tombstone is still reported as the "winner" (the fallback
    /// case of the winner rule) so `GET` can report the doc as deleted rather
    /// than silently 404ing as if it never existed.
    #[test]
    fn fully_deleted_document_winner_falls_back_to_tombstone() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        let mut tombstone = node(Some("1-aaa"));
        tombstone.deleted = true;
        tree.nodes.insert("2-bbb".into(), tombstone);

        assert_eq!(tree.winner(), Some(&"2-bbb".to_string()));
        assert!(tree.nodes[tree.winner().unwrap()].deleted);
    }

    /// Recreating a previously-deleted document continues the same branch
    /// (new generation on top of the tombstone), per real CouchDB
    /// behavior — it does not reset to generation 1.
    #[test]
    fn recreating_a_deleted_document_continues_the_branch_and_undeletes() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        let mut tombstone = node(Some("1-aaa"));
        tombstone.deleted = true;
        tree.nodes.insert("2-bbb".into(), tombstone);
        tree.nodes.insert("3-ccc".into(), node(Some("2-bbb"))); // recreated, not deleted

        assert_eq!(tree.winner(), Some(&"3-ccc".to_string()));
        assert!(!tree.nodes[tree.winner().unwrap()].deleted);
        assert!(tree.conflicts().is_empty());
    }

    /// Deep multi-generation branches on both sides of a conflict: the
    /// winner-picking must still walk to the true leaves, not just
    /// compare direct children of the fork point.
    #[test]
    fn deep_branches_winner_and_conflicts_still_correct() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-aaa".into(), node(Some("1-aaa")));
        // Branch A: forks at gen 2, runs to gen 5.
        tree.nodes.insert("3-aaa".into(), node(Some("2-aaa")));
        tree.nodes.insert("4-aaa".into(), node(Some("3-aaa")));
        tree.nodes.insert("5-aaa".into(), node(Some("4-aaa")));
        // Branch B: forks at gen 2, runs to gen 4 (shorter -> loses).
        tree.nodes.insert("3-bbb".into(), node(Some("2-aaa")));
        tree.nodes.insert("4-bbb".into(), node(Some("3-bbb")));

        assert_eq!(tree.winner(), Some(&"5-aaa".to_string()));
        assert_eq!(tree.conflicts(), vec!["4-bbb".to_string()]);
    }

    #[test]
    fn missing_reports_only_revs_not_already_stored() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.nodes.insert("2-bbb".into(), node(Some("1-aaa")));

        let requested = vec!["1-aaa".to_string(), "2-bbb".to_string(), "2-ccc".to_string()];
        assert_eq!(tree.missing(&requested), vec![&"2-ccc".to_string()]);
    }

    #[test]
    fn missing_against_unknown_document_reports_everything_requested() {
        let tree = RevTree::default();
        let requested = vec!["1-aaa".to_string(), "1-bbb".to_string()];
        assert_eq!(tree.missing(&requested), vec![&"1-aaa".to_string(), &"1-bbb".to_string()]);
    }

    #[test]
    fn insert_revision_chain_builds_full_ancestry_from_scratch() {
        let mut tree = RevTree::default();
        // Newest-first, as _revisions sends it.
        let chain = vec!["3-ccc".to_string(), "2-bbb".to_string(), "1-aaa".to_string()];
        tree.insert_revision_chain(&chain, false, serde_json::json!({"foo": "bar"}));

        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(tree.nodes["3-ccc"].parent, Some("2-bbb".to_string()));
        assert_eq!(tree.nodes["2-bbb"].parent, Some("1-aaa".to_string()));
        assert_eq!(tree.nodes["1-aaa"].parent, None);
        assert_eq!(tree.winner(), Some(&"3-ccc".to_string()));
        assert_eq!(tree.nodes["3-ccc"].body, serde_json::json!({"foo": "bar"}));
    }

    #[test]
    fn insert_revision_chain_connects_to_existing_history_without_duplicating() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        // Push a chain whose tail (1-aaa) we already have.
        let chain = vec!["2-bbb".to_string(), "1-aaa".to_string()];
        tree.insert_revision_chain(&chain, false, serde_json::json!({"foo": "bar"}));

        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.nodes["2-bbb"].parent, Some("1-aaa".to_string()));
        assert_eq!(tree.winner(), Some(&"2-bbb".to_string()));
    }

    #[test]
    fn insert_revision_chain_is_idempotent_and_never_clobbers_existing_nodes() {
        let mut tree = RevTree::default();
        let chain = vec!["2-bbb".to_string(), "1-aaa".to_string()];
        tree.insert_revision_chain(&chain, false, serde_json::json!({"foo": "bar"}));
        // Replay the exact same push (e.g. a retried replication batch).
        tree.insert_revision_chain(&chain, false, serde_json::json!({"foo": "bar"}));
        assert_eq!(tree.nodes.len(), 2);

        // A later push claiming 1-aaa is a fresh, non-deleted root must not
        // resurrect or alter a tombstone we already stored for it.
        let mut tombstoned = RevTree::default();
        let mut tombstone = node(None);
        tombstone.deleted = true;
        tombstoned.nodes.insert("1-aaa".into(), tombstone);
        tombstoned.insert_revision_chain(&["1-aaa".to_string()], false, serde_json::json!({"resurrected": true}));
        assert!(tombstoned.nodes["1-aaa"].deleted);
    }

    /// A diverging chain pushed via `new_edits:false` (two devices editing
    /// the same rev independently) must create a real conflict, not
    /// silently overwrite — this is the actual case replication exists to
    /// handle correctly.
    #[test]
    fn insert_revision_chain_diverging_from_shared_ancestor_creates_conflict() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.insert_revision_chain(
            &["2-bbb".to_string(), "1-aaa".to_string()],
            false,
            serde_json::json!({"device": "A"}),
        );
        tree.insert_revision_chain(
            &["2-ccc".to_string(), "1-aaa".to_string()],
            false,
            serde_json::json!({"device": "B"}),
        );

        assert_eq!(tree.nodes.len(), 3);
        let mut leaves: Vec<&String> = tree.leaves();
        leaves.sort();
        assert_eq!(leaves, vec![&"2-bbb".to_string(), &"2-ccc".to_string()]);
        assert_eq!(tree.conflicts().len(), 1);
    }

    #[test]
    fn insert_revision_chain_marks_leaf_deleted() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-aaa".into(), node(None));
        tree.insert_revision_chain(&["2-bbb".to_string(), "1-aaa".to_string()], true, serde_json::json!({}));
        assert!(tree.nodes["2-bbb"].deleted);
    }

    // Ported from PouchDB's own integration test suite
    // (tests/integration/test.conflicts.js, "Conflict resolution 1-5"),
    // which encodes real CouchDB winner-picking behavior. Kept as
    // separate cases with the exact revision ids PouchDB uses, rather
    // than folded into the tests above, so a future diff against the
    // upstream suite stays easy to spot.

    /// PouchDB: "Conflict resolution 1" — three same-generation leaves
    /// with no deletions; highest hash wins lexicographically.
    #[test]
    fn pouchdb_conflict_resolution_1_same_generation_lexicographic_tiebreak() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-a".into(), node(None));
        tree.nodes.insert("1-b".into(), node(None));
        tree.nodes.insert("1-1".into(), node(None));
        assert_eq!(tree.winner(), Some(&"1-b".to_string()));
    }

    /// PouchDB: "Conflict resolution 2" — higher generation wins even
    /// with a lexicographically smaller hash.
    #[test]
    fn pouchdb_conflict_resolution_2_generation_beats_hash() {
        let mut tree = RevTree::default();
        tree.nodes.insert("2-a".into(), node(None));
        tree.nodes.insert("1-b".into(), node(None));
        assert_eq!(tree.winner(), Some(&"2-a".to_string()));
    }

    /// PouchDB: "Conflict resolution 3" — generation must compare as an
    /// integer, not as a string: `"10-a"` sorts before `"2-b"` as text,
    /// but generation 10 beats generation 2 numerically. This is the
    /// specific bug class this test guards against.
    #[test]
    fn pouchdb_conflict_resolution_3_generation_compares_numerically_not_lexically() {
        let mut tree = RevTree::default();
        tree.nodes.insert("10-a".into(), node(None));
        tree.nodes.insert("2-b".into(), node(None));
        assert_eq!(tree.winner(), Some(&"10-a".to_string()));
    }

    /// PouchDB: "Conflict resolution 4" — a deleted branch (`1-a1` →
    /// `2-a2` → `3-a3`, tombstoned) loses to a shorter but non-deleted
    /// branch (`1-b1`), regardless of the deleted branch's higher
    /// generation.
    #[test]
    fn pouchdb_conflict_resolution_4_deleted_branch_loses_to_shorter_live_branch() {
        let mut tree = RevTree::default();
        tree.nodes.insert("1-a1".into(), node(None));
        tree.nodes.insert("2-a2".into(), node(Some("1-a1")));
        let mut tombstone = node(Some("2-a2"));
        tombstone.deleted = true;
        tree.nodes.insert("3-a3".into(), tombstone);
        tree.nodes.insert("1-b1".into(), node(None));
        assert_eq!(tree.winner(), Some(&"1-b1".to_string()));
    }

    /// PouchDB: "Conflict resolution 5" — a single non-deleted leaf
    /// (`2-a2`) wins over two other deleted leaves, even ones with
    /// otherwise-competitive generation/hash.
    #[test]
    fn pouchdb_conflict_resolution_5_single_live_leaf_beats_deleted_leaves() {
        let mut tree = RevTree::default();
        tree.nodes.insert("2-a2".into(), node(None));
        let mut deleted_b = node(None);
        deleted_b.deleted = true;
        tree.nodes.insert("1-b1".into(), deleted_b);
        let mut deleted_c = node(None);
        deleted_c.deleted = true;
        tree.nodes.insert("1-c1".into(), deleted_c);
        assert_eq!(tree.winner(), Some(&"2-a2".to_string()));
    }
}
