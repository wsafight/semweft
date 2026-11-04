use crate::graph::{GraphView, Interrupt};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: u64,
    pub max_nodes: usize,
    pub max_paths: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StopReason {
    PathLimit,
    DepthLimit,
    NodeLimit,
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path<Node, Edge> {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub struct Paths<Node: Ord, Edge> {
    pub paths: Vec<Path<Node, Edge>>,
    pub visited: BTreeSet<Node>,
    pub frontier: BTreeSet<Node>,
    pub reason: Option<StopReason>,
}

pub fn simple_paths_filtered<Graph: GraphView>(
    graph: &Graph,
    from: Graph::Node,
    to: Graph::Node,
    limits: Limits,
    checkpoint: impl FnMut() -> Result<(), Interrupt>,
    include: impl FnMut(&Graph::Edge) -> bool,
) -> Paths<Graph::Node, Graph::Edge> {
    let mut search = Search {
        graph,
        to,
        limits,
        checkpoint,
        include,
        active: BTreeSet::from([from.clone()]),
        path: Path {
            nodes: vec![from.clone()],
            edges: vec![],
        },
        result: Paths {
            paths: vec![],
            visited: BTreeSet::new(),
            frontier: BTreeSet::new(),
            reason: None,
        },
    };
    if limits.max_nodes == 0 {
        search.stop(StopReason::NodeLimit);
    } else {
        search.result.visited.insert(from.clone());
        search.visit(&from);
    }
    search.result
}

struct Search<'graph, Graph: GraphView, Checkpoint, Include> {
    graph: &'graph Graph,
    to: Graph::Node,
    limits: Limits,
    checkpoint: Checkpoint,
    include: Include,
    active: BTreeSet<Graph::Node>,
    path: Path<Graph::Node, Graph::Edge>,
    result: Paths<Graph::Node, Graph::Edge>,
}

impl<Graph: GraphView, Checkpoint, Include> Search<'_, Graph, Checkpoint, Include>
where
    Checkpoint: FnMut() -> Result<(), Interrupt>,
    Include: FnMut(&Graph::Edge) -> bool,
{
    fn stop(&mut self, reason: StopReason) {
        self.result.reason = self.result.reason.max(Some(reason));
        self.result.frontier.extend(self.path.nodes.iter().cloned());
    }

    fn interrupted(&mut self) -> bool {
        if let Err(interrupt) = (self.checkpoint)() {
            self.stop(match interrupt {
                Interrupt::Timeout => StopReason::Timeout,
                Interrupt::Cancelled => StopReason::Cancelled,
            });
            true
        } else {
            false
        }
    }

    fn visit(&mut self, node: &Graph::Node) -> bool {
        if self.interrupted() {
            return true;
        }
        if node == &self.to {
            if self.result.paths.len() == self.limits.max_paths {
                self.stop(StopReason::PathLimit);
                return true;
            }
            self.result.paths.push(self.path.clone());
            return false;
        }
        let graph = self.graph;
        for (edge, next) in graph.neighbors(node) {
            if self.interrupted() {
                return true;
            }
            if !(self.include)(&edge) || self.active.contains(&next) {
                continue;
            }
            if self.path.edges.len() as u64 >= self.limits.max_depth {
                self.result.reason = self.result.reason.max(Some(StopReason::DepthLimit));
                self.result.frontier.insert(node.clone());
                continue;
            }
            if !self.result.visited.contains(&next)
                && self.result.visited.len() >= self.limits.max_nodes
            {
                self.stop(StopReason::NodeLimit);
                return true;
            }
            self.result.visited.insert(next.clone());
            self.active.insert(next.clone());
            self.path.nodes.push(next.clone());
            self.path.edges.push(edge);
            if self.visit(&next) {
                return true;
            }
            self.path.edges.pop();
            self.path.nodes.pop();
            self.active.remove(&next);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Graph(BTreeMap<u8, Vec<(u8, u8)>>);
    impl GraphView for Graph {
        type Node = u8;
        type Edge = u8;

        fn neighbors(&self, node: &u8) -> impl Iterator<Item = (u8, u8)> {
            self.0.get(node).into_iter().flatten().copied()
        }
    }

    fn graph(edges: &[(u8, u8, u8)]) -> Graph {
        let mut graph = BTreeMap::<_, Vec<_>>::new();
        for &(edge, from, to) in edges {
            graph.entry(from).or_default().push((edge, to));
        }
        Graph(graph)
    }

    fn limits() -> Limits {
        Limits {
            max_depth: 10,
            max_nodes: 100,
            max_paths: 100,
        }
    }

    #[test]
    fn simple_paths_match_all_three_node_graphs_without_repeating_symbols() {
        for mask in 0..(1_u16 << 9) {
            let edges = (0..9_u8)
                .filter(|&edge| mask & (1 << edge) != 0)
                .map(|edge| (edge, edge / 3, edge % 3))
                .collect::<Vec<_>>();
            let result = simple_paths_filtered(&graph(&edges), 0, 2, limits(), || Ok(()), |_| true);
            let actual = result
                .paths
                .iter()
                .map(|path| path.edges.clone())
                .collect::<BTreeSet<_>>();
            let mut expected = BTreeSet::new();
            if mask & (1 << 2) != 0 {
                expected.insert(vec![2]);
            }
            if mask & (1 << 1) != 0 && mask & (1 << 5) != 0 {
                expected.insert(vec![1, 5]);
            }
            assert_eq!(actual, expected, "mask {mask}");
            assert_eq!(result.reason, None, "mask {mask}");
            assert!(
                result.paths.iter().all(
                    |path| path.nodes.iter().collect::<BTreeSet<_>>().len() == path.nodes.len()
                )
            );
        }
    }

    #[test]
    fn parallel_edges_are_distinct_and_path_limit_requires_actual_omitted_work() {
        let graph = graph(&[(1, 0, 1), (2, 0, 1), (3, 1, 2), (4, 2, 0)]);
        let full = simple_paths_filtered(&graph, 0, 2, limits(), || Ok(()), |_| true);
        assert_eq!(
            full.paths
                .iter()
                .map(|path| &path.edges)
                .collect::<Vec<_>>(),
            [&vec![1, 3], &vec![2, 3]]
        );
        let exact = simple_paths_filtered(
            &graph,
            0,
            2,
            Limits {
                max_paths: 2,
                ..limits()
            },
            || Ok(()),
            |_| true,
        );
        assert_eq!(exact.reason, None);
        let limited = simple_paths_filtered(
            &graph,
            0,
            2,
            Limits {
                max_paths: 1,
                ..limits()
            },
            || Ok(()),
            |_| true,
        );
        assert_eq!(limited.paths.len(), 1);
        assert_eq!(limited.reason, Some(StopReason::PathLimit));
    }

    #[test]
    fn depth_nodes_zero_length_and_unreachable_targets_have_explicit_bounds() {
        let graph = graph(&[(1, 0, 1), (2, 1, 2), (3, 1, 0)]);
        let depth = simple_paths_filtered(
            &graph,
            0,
            2,
            Limits {
                max_depth: 1,
                ..limits()
            },
            || Ok(()),
            |_| true,
        );
        assert!(depth.paths.is_empty());
        assert_eq!(depth.reason, Some(StopReason::DepthLimit));
        assert_eq!(depth.frontier, BTreeSet::from([1]));
        let nodes = simple_paths_filtered(
            &graph,
            0,
            2,
            Limits {
                max_nodes: 1,
                ..limits()
            },
            || Ok(()),
            |_| true,
        );
        assert_eq!(nodes.reason, Some(StopReason::NodeLimit));
        assert_eq!(nodes.visited, BTreeSet::from([0]));
        let same = simple_paths_filtered(&graph, 0, 0, limits(), || Ok(()), |_| true);
        assert_eq!(
            same.paths,
            [Path {
                nodes: vec![0],
                edges: vec![]
            }]
        );
        let absent = simple_paths_filtered(&graph, 2, 0, limits(), || Ok(()), |_| true);
        assert!(absent.paths.is_empty());
        assert_eq!(absent.reason, None);
    }

    #[test]
    fn interruption_retains_completed_paths_and_the_unexpanded_prefix() {
        let graph = graph(&[(1, 0, 1), (2, 0, 2), (3, 2, 1)]);
        let mut checks = 0;
        let result = simple_paths_filtered(
            &graph,
            0,
            1,
            limits(),
            || {
                checks += 1;
                if checks == 4 {
                    Err(Interrupt::Timeout)
                } else {
                    Ok(())
                }
            },
            |_| true,
        );
        assert_eq!(
            result.paths,
            [Path {
                nodes: vec![0, 1],
                edges: vec![1]
            }]
        );
        assert_eq!(result.reason, Some(StopReason::Timeout));
        assert_eq!(result.frontier, BTreeSet::from([0]));
    }

    #[test]
    fn filtered_edges_spend_no_budget_and_checkpoints_interrupt_even_excluded_edges() {
        let graph = graph(&[(1, 0, 1), (2, 0, 2), (3, 0, 3)]);
        let filtered = simple_paths_filtered(
            &graph,
            0,
            3,
            Limits {
                max_nodes: 2,
                ..limits()
            },
            || Ok(()),
            |edge| *edge == 3,
        );
        assert_eq!(filtered.reason, None);
        assert_eq!(filtered.paths[0].edges, [3]);
        for interrupt in [Interrupt::Timeout, Interrupt::Cancelled] {
            let mut checks = 0;
            let result = simple_paths_filtered(
                &graph,
                0,
                3,
                limits(),
                || {
                    checks += 1;
                    if checks == 3 { Err(interrupt) } else { Ok(()) }
                },
                |_| false,
            );
            assert_eq!(checks, 3);
            assert_eq!(result.frontier, BTreeSet::from([0]));
            assert_eq!(
                result.reason,
                Some(match interrupt {
                    Interrupt::Timeout => StopReason::Timeout,
                    Interrupt::Cancelled => StopReason::Cancelled,
                })
            );
        }
    }
}
