//! Per-document revision tree: storage, winner-picking, `_revs_diff`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type RevId = String; // "{generation}-{hash}"

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

    /// Winner-picking per plan §4.4: highest generation, then lexicographically
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
    /// already have stored for this document (plan §3).
    pub fn missing<'a>(&self, requested: &'a [RevId]) -> Vec<&'a RevId> {
        requested.iter().filter(|rev| !self.nodes.contains_key(*rev)).collect()
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
    /// hash, since generation is compared first (plan §4.4 step 3).
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

    /// Once a client explicitly DELETEs the losing branch (plan §2.3: a
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
    /// tombstone is still reported as the "winner" (fallback case in
    /// plan §4.4 step 3) so `GET` can report the doc as deleted rather
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
    /// handle correctly (plan §1.2, §2.3).
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
