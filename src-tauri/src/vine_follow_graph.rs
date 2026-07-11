//! ZEB-671: bounded transitive-follow reachability for Vines Discover.
//!
//! Pure graph math — no cache access, no IO. `VineFeedCache` owns the
//! verified follow lists and calls [`compute_reach`] whenever the graph
//! inputs change (a follow-list sample admitted, or the local follow
//! set mutated).
//!
//! Model (per the drawn Discover experience): degree 1 = the user's own
//! LOCAL follows (ground truth — never the own published echo), shown in
//! the Followed feed and excluded here. Degree 2 = who my follows
//! follow; degree 3 = one hop further. "No algorithm, just your social
//! graph" — nothing beyond depth 3, no ranking.

use std::collections::HashMap;

/// Maximum graph distance surfaced in Discover.
pub const MAX_DEGREE: u8 = 3;

/// Ceiling on BFS work: total nodes expanded across the whole traversal.
/// At 1000-entry list caps a hostile graph could otherwise reach
/// 1000² = 1M nodes at depth 3; the cap bounds latency deterministically
/// (sorted iteration means the SAME nodes survive the cap every run).
pub const MAX_BFS_VISITED: usize = 50_000;

/// How a Discover creator was reached: graph distance (2..=MAX_DEGREE)
/// and the follow chain that reached them. `via[0]` is one of MY
/// follows; later entries are intermediate hops. The creator themselves
/// is NOT included — `degree == via.len() + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub degree: u8,
    pub via: Vec<String>,
}

/// Compute 2°..=`MAX_DEGREE`° reachability from `me` over published
/// follow lists. `neighbors(addr)` returns the verified follow list an
/// owner published, if cached.
///
/// Deterministic: frontier and neighbor iteration are sorted, so ties
/// (two equal-length paths to one creator) always resolve to the
/// lexicographically smallest chain, and the visited cap always trims
/// the same nodes. `me` and degree-1 creators never appear in the
/// output.
pub fn compute_reach<'a, F>(me: &str, my_follows: &[String], neighbors: F) -> HashMap<String, Reach>
where
    F: Fn(&str) -> Option<&'a [String]>,
{
    let mut reach: HashMap<String, Reach> = HashMap::new();

    let mut frontier: Vec<String> = my_follows.to_vec();
    frontier.sort();
    frontier.dedup();
    // First-hop set doubles as the exclusion set for degree-1 creators.
    // Owned (not borrowed from `frontier`) — the frontier is replaced
    // each depth.
    let degree_one: std::collections::HashSet<String> = frontier.iter().cloned().collect();

    // Path to each frontier node (excluding the node itself).
    let mut paths: HashMap<String, Vec<String>> =
        frontier.iter().map(|f| (f.clone(), Vec::new())).collect();

    let mut visited = frontier.len();

    for depth in 2..=MAX_DEGREE {
        let mut next_frontier: Vec<String> = Vec::new();
        for node in &frontier {
            let Some(follows) = neighbors(node) else {
                continue;
            };
            let mut sorted: Vec<&String> = follows.iter().collect();
            sorted.sort();
            for candidate in sorted {
                if visited >= MAX_BFS_VISITED {
                    return reach;
                }
                if candidate == me
                    || degree_one.contains(candidate)
                    || reach.contains_key(candidate)
                {
                    continue;
                }
                visited += 1;
                let mut via = paths.get(node).cloned().unwrap_or_default();
                via.push(node.clone());
                if depth < MAX_DEGREE {
                    paths.insert(candidate.clone(), via.clone());
                    next_frontier.push(candidate.clone());
                }
                reach.insert(candidate.clone(), Reach { degree: depth, via });
            }
        }
        next_frontier.sort();
        frontier = next_frontier;
    }

    reach
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lists(edges: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(owner, follows)| {
                (
                    owner.to_string(),
                    follows.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    fn reach_with(
        me: &str,
        my_follows: &[&str],
        edges: &[(&str, &[&str])],
    ) -> HashMap<String, Reach> {
        let lists = lists(edges);
        let follows: Vec<String> = my_follows.iter().map(|s| s.to_string()).collect();
        compute_reach(me, &follows, |owner| lists.get(owner).map(|v| v.as_slice()))
    }

    #[test]
    fn assigns_second_and_third_degree() {
        // me → devin → ravi → ada
        let reach = reach_with(
            "me",
            &["devin"],
            &[("devin", &["ravi"]), ("ravi", &["ada"])],
        );
        assert_eq!(
            reach.get("ravi"),
            Some(&Reach {
                degree: 2,
                via: vec!["devin".into()]
            })
        );
        assert_eq!(
            reach.get("ada"),
            Some(&Reach {
                degree: 3,
                via: vec!["devin".into(), "ravi".into()]
            })
        );
    }

    #[test]
    fn excludes_me_and_first_degree() {
        // Cycles back to me and to a direct follow must not surface.
        let reach = reach_with(
            "me",
            &["devin", "mara"],
            &[("devin", &["me", "mara", "ravi"]), ("ravi", &["devin"])],
        );
        assert!(!reach.contains_key("me"));
        assert!(!reach.contains_key("devin"));
        assert!(!reach.contains_key("mara"));
        assert_eq!(reach.get("ravi").unwrap().degree, 2);
    }

    #[test]
    fn depth_bound_stops_at_max_degree() {
        // me → a → b → c → d: d is 4° and must not appear.
        let reach = reach_with("me", &["a"], &[("a", &["b"]), ("b", &["c"]), ("c", &["d"])]);
        assert_eq!(reach.get("b").unwrap().degree, 2);
        assert_eq!(reach.get("c").unwrap().degree, 3);
        assert!(!reach.contains_key("d"));
    }

    #[test]
    fn shortest_path_wins_in_diamond() {
        // ada is reachable at 2° via mara AND at 3° via devin→ravi:
        // the 2° path must win.
        let reach = reach_with(
            "me",
            &["devin", "mara"],
            &[("devin", &["ravi"]), ("ravi", &["ada"]), ("mara", &["ada"])],
        );
        assert_eq!(
            reach.get("ada"),
            Some(&Reach {
                degree: 2,
                via: vec!["mara".into()]
            })
        );
    }

    #[test]
    fn equal_length_tie_breaks_lexicographically() {
        // Both follows reach zoe at 2°; the smaller address wins the via.
        let reach = reach_with(
            "me",
            &["bob", "amy"],
            &[("bob", &["zoe"]), ("amy", &["zoe"])],
        );
        assert_eq!(reach.get("zoe").unwrap().via, vec!["amy".to_string()]);
    }

    #[test]
    fn empty_inputs_produce_empty_reach() {
        assert!(reach_with("me", &[], &[]).is_empty());
        assert!(reach_with("me", &["devin"], &[]).is_empty());
    }

    #[test]
    fn visited_cap_bounds_traversal() {
        // One follow lists MAX_BFS_VISITED+10 candidates; the traversal
        // must stop at the cap (minus the frontier seed) without panic.
        let big: Vec<String> = (0..MAX_BFS_VISITED + 10)
            .map(|i| format!("n{i:06}"))
            .collect();
        let lists: HashMap<String, Vec<String>> =
            [("devin".to_string(), big)].into_iter().collect();
        let follows = vec!["devin".to_string()];
        let reach = compute_reach("me", &follows, |owner| {
            lists.get(owner).map(|v| v.as_slice())
        });
        assert!(reach.len() <= MAX_BFS_VISITED);
        assert!(!reach.is_empty());
    }
}
