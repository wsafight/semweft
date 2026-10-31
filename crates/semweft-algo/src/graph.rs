//! Bounded breadth-first traversal with stable paths and explicit unfinished work.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Neighbors must be ordered by a stable edge key, then by node key.
/// Edge keys identify edges globally; the same key may be exposed at both ends
/// when traversing an undirected view of a directed graph.
pub trait GraphView {
    type Node: Clone + Ord;
    type Edge: Clone + Ord;

    fn neighbors(&self, node: &Self::Node) -> impl Iterator<Item = (Self::Edge, Self::Node)>;
}

/// An adjacency representation that enforces the ordering required by GraphView.
pub type Adjacency<N, E> = BTreeMap<N, BTreeSet<(E, N)>>;

impl<N: Clone + Ord, E: Clone + Ord> GraphView for Adjacency<N, E> {
    type Node = N;
    type Edge = E;

    fn neighbors(&self, node: &N) -> impl Iterator<Item = (E, N)> {
        self.get(node).into_iter().flatten().cloned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The root is at depth zero. A boundary node is retained in the result.
    pub max_depth: u64,
    /// Includes the root. Zero returns no nodes and leaves the root as frontier.
    pub max_nodes: usize,
}

/// The caller supplies interruption decisions; the algorithm never reads a clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    Timeout,
    Cancelled,
}

/// Declaration order is the precedence used when several bounds are reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StopReason {
    DepthLimit,
    NodeLimit,
    Timeout,
    Cancelled,
}

impl From<Interrupt> for StopReason {
    fn from(value: Interrupt) -> Self {
        match value {
            Interrupt::Timeout => Self::Timeout,
            Interrupt::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeVisit<E> {
    pub depth: u64,
    /// Ordered from the root outward. Ties between shortest paths use edge keys.
    pub path: Vec<E>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Traversal<N: Ord, E: Ord> {
    pub nodes: BTreeSet<N>,
    pub edges: BTreeMap<E, EdgeVisit<E>>,
    /// Nodes whose adjacent edges remain unexplored, including queued nodes
    /// after interruption. This is a boundary description, not a resume token.
    pub frontier: BTreeSet<N>,
    pub reason: Option<StopReason>,
}

/// Visit every reachable edge within the limits, preserving parallel edges and
/// cycles while expanding each node once. The checkpoint runs before each node
/// and edge so even a high-degree node can be interrupted. Already visited
/// endpoints do not consume node budget; every returned edge has both endpoints
/// in `nodes`. Results and path ties are stable for a fixed graph and checkpoint.
pub fn breadth_first<G: GraphView>(
    graph: &G,
    root: G::Node,
    limits: Limits,
    checkpoint: impl FnMut() -> Result<(), Interrupt>,
) -> Traversal<G::Node, G::Edge> {
    breadth_first_filtered(graph, root, limits, checkpoint, |_| true)
}

/// Filter edges after the interruption check, so skipping a large number of
/// excluded neighbors cannot prevent cancellation. Only included edges consume
/// traversal limits or contribute to the frontier.
pub fn breadth_first_filtered<G: GraphView>(
    graph: &G,
    root: G::Node,
    limits: Limits,
    mut checkpoint: impl FnMut() -> Result<(), Interrupt>,
    mut include: impl FnMut(&G::Edge) -> bool,
) -> Traversal<G::Node, G::Edge> {
    let mut result = Traversal {
        nodes: BTreeSet::new(),
        edges: BTreeMap::new(),
        frontier: BTreeSet::new(),
        reason: None,
    };
    if limits.max_nodes == 0 {
        result.frontier.insert(root);
        result.reason = Some(StopReason::NodeLimit);
        return result;
    }
    result.nodes.insert(root.clone());
    let mut queue = VecDeque::from([(root, 0, Vec::new())]);
    'nodes: while let Some((node, depth, path)) = queue.pop_front() {
        if let Err(interrupt) = checkpoint() {
            result.reason = result.reason.max(Some(interrupt.into()));
            result.frontier.insert(node);
            break;
        }
        for (edge, next) in graph.neighbors(&node) {
            if let Err(interrupt) = checkpoint() {
                result.reason = result.reason.max(Some(interrupt.into()));
                result.frontier.insert(node.clone());
                break 'nodes;
            }
            if !include(&edge) {
                continue;
            }
            if depth >= limits.max_depth {
                if !result.edges.contains_key(&edge) {
                    result.frontier.insert(node.clone());
                    result.reason = result.reason.max(Some(StopReason::DepthLimit));
                }
                continue;
            }
            if !result.nodes.contains(&next) && result.nodes.len() >= limits.max_nodes {
                result.frontier.insert(node.clone());
                result.reason = result.reason.max(Some(StopReason::NodeLimit));
                continue;
            }
            let mut next_path = path.clone();
            next_path.push(edge.clone());
            result.edges.entry(edge).or_insert_with(|| EdgeVisit {
                depth: depth + 1,
                path: next_path.clone(),
            });
            if result.nodes.insert(next.clone()) {
                queue.push_back((next, depth + 1, next_path));
            }
        }
    }
    result
        .frontier
        .extend(queue.into_iter().map(|(node, _, _)| node));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(u8, u8, u8)]) -> Adjacency<u8, u8> {
        let mut graph = Adjacency::new();
        for &(edge, from, to) in edges {
            graph.entry(from).or_default().insert((edge, to));
        }
        graph
    }

    fn walk(graph: &Adjacency<u8, u8>, depth: u64, nodes: usize) -> Traversal<u8, u8> {
        breadth_first(
            graph,
            0,
            Limits {
                max_depth: depth,
                max_nodes: nodes,
            },
            || Ok(()),
        )
    }

    #[test]
    fn insertion_order_does_not_change_diamond_paths_or_parallel_edges() {
        let edges = [
            (40, 2, 3),
            (20, 0, 2),
            (30, 1, 3),
            (10, 0, 1),
            (11, 0, 1),
            (50, 3, 4),
        ];
        let a = walk(&graph(&edges), 10, 10);
        let b = walk(&graph(&edges.into_iter().rev().collect::<Vec<_>>()), 10, 10);
        assert_eq!(a, b);
        assert_eq!(a.edges.len(), 6);
        assert_eq!(a.edges[&50].path, [10, 30, 50]);
        assert_eq!(a.edges[&50].depth, 3);
        assert!(a.frontier.is_empty());
        assert_eq!(a.reason, None);
    }

    #[test]
    fn cycles_self_loops_and_reversed_views_do_not_spend_extra_node_budget() {
        let graph = graph(&[(1, 0, 0), (2, 0, 1), (2, 1, 0), (3, 1, 0)]);
        let result = walk(&graph, 10, 2);
        assert_eq!(result.nodes, BTreeSet::from([0, 1]));
        assert_eq!(result.edges.len(), 3);
        assert_eq!(result.edges[&3].path, [2, 3]);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn limits_report_actual_unexplored_work_and_keep_the_stronger_reason() {
        let graph = graph(&[(1, 0, 1), (2, 0, 2), (3, 1, 3)]);
        let result = walk(&graph, 1, 2);
        assert_eq!(result.nodes, BTreeSet::from([0, 1]));
        assert_eq!(result.edges.keys().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(result.frontier, BTreeSet::from([0, 1]));
        assert_eq!(result.reason, Some(StopReason::NodeLimit));
        assert_eq!(walk(&graph, 0, 10).reason, Some(StopReason::DepthLimit));
        let zero = walk(&graph, 10, 0);
        assert!(zero.nodes.is_empty());
        assert!(zero.edges.is_empty());
        assert_eq!(zero.frontier, BTreeSet::from([0]));
        assert_eq!(zero.reason, Some(StopReason::NodeLimit));
        assert_eq!(walk(&Adjacency::new(), 0, 1).reason, None);
        assert_eq!(
            walk(&self::graph(&[(1, 0, 1), (1, 1, 0)]), 1, 2).reason,
            None
        );
    }

    #[test]
    fn interruption_keeps_current_and_queued_nodes_for_both_failure_modes() {
        let graph = graph(&[(1, 0, 1), (2, 0, 2), (3, 0, 3)]);
        for interrupt in [Interrupt::Timeout, Interrupt::Cancelled] {
            let mut checks = 0;
            let result = breadth_first(
                &graph,
                0,
                Limits {
                    max_depth: 5,
                    max_nodes: 10,
                },
                || {
                    checks += 1;
                    if checks == 4 { Err(interrupt) } else { Ok(()) }
                },
            );
            assert_eq!(result.nodes, BTreeSet::from([0, 1, 2]));
            assert_eq!(result.edges.len(), 2);
            assert_eq!(result.frontier, BTreeSet::from([0, 1, 2]));
            assert_eq!(result.reason, Some(interrupt.into()));
            let immediate = breadth_first(
                &graph,
                0,
                Limits {
                    max_depth: 5,
                    max_nodes: 10,
                },
                || Err(interrupt),
            );
            assert!(immediate.edges.is_empty());
            assert_eq!(immediate.frontier, BTreeSet::from([0]));
        }
    }

    #[test]
    fn excluded_edges_use_no_budget_and_still_reach_interruption_checkpoints() {
        let graph = graph(&[(1, 0, 1), (2, 0, 2), (3, 0, 3)]);
        let result = breadth_first_filtered(
            &graph,
            0,
            Limits {
                max_depth: 1,
                max_nodes: 2,
            },
            || Ok(()),
            |edge| *edge == 3,
        );
        assert_eq!(result.nodes, BTreeSet::from([0, 3]));
        assert_eq!(result.reason, None);
        let excluded = breadth_first_filtered(
            &graph,
            0,
            Limits {
                max_depth: 0,
                max_nodes: 1,
            },
            || Ok(()),
            |_| false,
        );
        assert!(excluded.frontier.is_empty());
        assert_eq!(excluded.reason, None);
        let mut checks = 0;
        let interrupted = breadth_first_filtered(
            &graph,
            0,
            Limits {
                max_depth: 1,
                max_nodes: 1,
            },
            || {
                checks += 1;
                if checks == 3 {
                    Err(Interrupt::Cancelled)
                } else {
                    Ok(())
                }
            },
            |_| false,
        );
        assert_eq!(checks, 3);
        assert_eq!(interrupted.reason, Some(StopReason::Cancelled));
        assert_eq!(interrupted.frontier, BTreeSet::from([0]));
    }

    #[test]
    fn bounded_reachability_matches_transitive_closure_on_all_three_node_graphs() {
        for mask in 0..(1_u16 << 9) {
            let mut edges = vec![];
            let mut reachable = [[false; 3]; 3];
            for edge in 0..9_u8 {
                if mask & (1 << edge) != 0 {
                    edges.push((edge, edge / 3, edge % 3));
                    reachable[edge as usize / 3][edge as usize % 3] = true;
                }
            }
            for (i, row) in reachable.iter_mut().enumerate() {
                row[i] = true;
            }
            for via in 0..3 {
                for from in 0..3 {
                    for to in 0..3 {
                        reachable[from][to] |= reachable[from][via] && reachable[via][to];
                    }
                }
            }
            let result = walk(&graph(&edges), 3, 3);
            let expected = (0..3_u8)
                .filter(|&n| reachable[0][n as usize])
                .collect::<BTreeSet<_>>();
            assert_eq!(result.nodes, expected, "mask {mask}");
            assert_eq!(result.reason, None, "mask {mask}");
            assert_eq!(
                result.edges.len(),
                edges
                    .iter()
                    .filter(|(_, from, _)| reachable[0][*from as usize])
                    .count()
            );
        }
    }
}
