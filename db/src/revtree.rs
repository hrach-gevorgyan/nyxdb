//! Per-document revision tree: storage, winner-picking, `_revs_diff`.
//! See rust-couchdb-clone-plan.md §4.

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
}
