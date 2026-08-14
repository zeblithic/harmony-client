//! Mutable, shareable network-reachability predicate over node ids.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// node_id -> island (group) index.
type IslandMap = HashMap<[u8; 32], usize>;

/// Mutable, shareable network-reachability predicate over node ids.
/// A `None` split map means fully connected.
#[derive(Clone)]
pub(crate) struct Partition {
    // node_id -> group index; `None` == fully connected.
    split: Arc<RwLock<Option<IslandMap>>>,
}

impl Partition {
    pub(crate) fn fully_connected() -> Self {
        Self {
            split: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) fn set_split(&self, groups: Vec<Vec<[u8; 32]>>) {
        let mut map = HashMap::new();
        for (gi, group) in groups.iter().enumerate() {
            for id in group {
                map.insert(*id, gi);
            }
        }
        *self.split.write().expect("partition lock") = Some(map);
    }

    pub(crate) fn heal(&self) {
        *self.split.write().expect("partition lock") = None;
    }

    pub(crate) fn same_side(&self, a: [u8; 32], b: [u8; 32]) -> bool {
        if a == b {
            return true;
        }
        let guard = self.split.read().expect("partition lock");
        match guard.as_ref() {
            None => true,
            Some(map) => match (map.get(&a), map.get(&b)) {
                (Some(ga), Some(gb)) => ga == gb,
                // An id not placed in any group is isolated from everyone but itself.
                _ => false,
            },
        }
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;

    fn id(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn fully_connected_all_same_side() {
        let p = Partition::fully_connected();
        assert!(p.same_side(id(1), id(2)));
        assert!(p.same_side(id(1), id(1)));
    }

    #[test]
    fn split_isolates_across_groups() {
        let p = Partition::fully_connected();
        p.set_split(vec![vec![id(1), id(2), id(3)], vec![id(4), id(5), id(6)]]);
        assert!(p.same_side(id(1), id(2)), "same group -> reachable");
        assert!(!p.same_side(id(1), id(4)), "cross group -> partitioned");
        assert!(p.same_side(id(4), id(4)), "self is always same-side");
        p.heal();
        assert!(p.same_side(id(1), id(4)), "heal restores reachability");
    }
}
