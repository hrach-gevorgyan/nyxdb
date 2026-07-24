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
}
