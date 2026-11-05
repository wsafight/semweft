use crate::model::{Location, Relation, Symbol};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const NODE_WIDTH: f32 = 220.;
pub const NODE_HEIGHT: f32 = 76.;
const MAX_VISIBLE: usize = 160;

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub location: Location,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub candidate: bool,
    pub stale: bool,
    pub count: usize,
    pub change: i8,
}

#[derive(Clone, Default, Debug)]
pub struct Diagram {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub hidden: usize,
}

impl Diagram {
    pub fn arrange(&mut self, mode: usize, positions: &BTreeMap<String, (f32, f32)>) {
        let columns = (self.nodes.len() as f32).sqrt().ceil().max(1.) as usize;
        for (index, node) in self.nodes.iter_mut().enumerate() {
            if mode == 1 {
                let level = (node.x - 32.) / (NODE_WIDTH + 82.);
                let row = (node.y - 32.) / (NODE_HEIGHT + 30.);
                node.x = 32. + row * (NODE_WIDTH + 48.);
                node.y = 32. + level * (NODE_HEIGHT + 64.);
            } else if mode == 2 {
                node.x = 32. + (index % columns) as f32 * (NODE_WIDTH + 48.);
                node.y = 32. + (index / columns) as f32 * (NODE_HEIGHT + 48.);
            }
            if let Some(&(left, top)) = positions.get(&node.id)
                && left.is_finite()
                && top.is_finite()
            {
                node.x = left.clamp(0., 1_000_000.);
                node.y = top.clamp(0., 1_000_000.);
            }
        }
    }

    pub fn build<'a>(
        root: &str,
        symbols: impl Iterator<Item = &'a Symbol>,
        relations: &[Relation],
        grouped: bool,
        collapsed: &BTreeSet<String>,
    ) -> Self {
        let symbols = symbols
            .map(|symbol| (symbol.symbol_id.as_str(), symbol))
            .collect::<BTreeMap<_, _>>();
        let mut nodes = BTreeMap::new();
        let mut add = |id: &str, fallback: &str| {
            let symbol = symbols.get(id);
            let location = symbol
                .map(|symbol| symbol.location.clone())
                .unwrap_or_default();
            let key = if grouped && !location.file.is_empty() {
                location.file.clone()
            } else {
                id.into()
            };
            nodes.entry(key.clone()).or_insert_with(|| Node {
                id: key.clone(),
                label: if grouped && !location.file.is_empty() {
                    location.file.clone()
                } else {
                    symbol
                        .map(|symbol| symbol.qualified_name.clone())
                        .unwrap_or_else(|| {
                            if fallback.is_empty() {
                                id.into()
                            } else {
                                fallback.into()
                            }
                        })
                },
                location,
                x: 0.,
                y: 0.,
            });
            key
        };
        let root = add(root, root);
        let pairs = relations
            .iter()
            .map(|relation| {
                (
                    add(&relation.from, &relation.from_name),
                    add(&relation.to, &relation.to_name),
                    relation,
                )
            })
            .collect::<Vec<_>>();
        let mut adjacency = BTreeMap::<String, BTreeSet<String>>::new();
        let mut directed = BTreeSet::new();
        for (from, to, _) in &pairs {
            directed.insert((from.as_str(), to.as_str()));
            adjacency
                .entry(from.clone())
                .or_default()
                .insert(to.clone());
            adjacency
                .entry(to.clone())
                .or_default()
                .insert(from.clone());
        }
        let mut levels = BTreeMap::from([(root.clone(), 0i32)]);
        let mut queue = VecDeque::from([root.clone()]);
        while let Some(id) = queue.pop_front() {
            if collapsed.contains(&id) {
                continue;
            }
            for neighbor in adjacency.get(&id).into_iter().flatten() {
                if levels.contains_key(neighbor) || levels.len() >= MAX_VISIBLE {
                    continue;
                }
                let incoming = directed.contains(&(neighbor.as_str(), id.as_str()));
                let current = levels[&id];
                let level = if current < 0 || (current == 0 && incoming) {
                    current - 1
                } else {
                    current + 1
                };
                levels.insert(neighbor.clone(), level);
                queue.push_back(neighbor.clone());
            }
        }
        let minimum = levels.values().copied().min().unwrap_or(0);
        let mut rows = BTreeMap::<i32, usize>::new();
        let hidden = nodes.len().saturating_sub(levels.len());
        let mut ordered = levels.into_iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
        let nodes = ordered
            .into_iter()
            .map(|(id, level)| {
                let mut node = nodes.remove(&id).unwrap();
                let row = rows.entry(level).or_default();
                node.x = 32. + (level - minimum) as f32 * (NODE_WIDTH + 82.);
                node.y = 32. + *row as f32 * (NODE_HEIGHT + 30.);
                *row += 1;
                node
            })
            .collect::<Vec<_>>();
        let indices = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        let mut edges = BTreeMap::<(usize, usize), Edge>::new();
        for (from, to, relation) in pairs {
            let (Some(&from), Some(&to)) = (indices.get(from.as_str()), indices.get(to.as_str()))
            else {
                continue;
            };
            let edge = edges.entry((from, to)).or_insert(Edge {
                from,
                to,
                candidate: false,
                stale: false,
                count: 0,
                change: 0,
            });
            edge.candidate |= relation.certainty != "definite";
            edge.stale |= !matches!(relation.freshness.as_str(), "fresh" | "current" | "");
            edge.count += 1;
        }
        Self {
            nodes,
            edges: edges.into_values().collect(),
            hidden,
        }
    }

    pub fn size(&self) -> (f32, f32) {
        (
            self.nodes
                .iter()
                .map(|node| node.x + NODE_WIDTH + 32.)
                .fold(640., f32::max),
            self.nodes
                .iter()
                .map(|node| node.y + NODE_HEIGHT + 32.)
                .fold(360., f32::max),
        )
    }

    pub fn svg(&self) -> String {
        fn escape(value: &str) -> String {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        }
        let (width, height) = self.size();
        let mut result = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><defs><marker id="arrow" markerWidth="10" markerHeight="10" refX="8" refY="3" orient="auto"><path d="M0,0 L0,6 L8,3 z" fill="#6c9181"/></marker></defs><rect width="100%" height="100%" fill="#f6f8fb"/>"##
        );
        for edge in &self.edges {
            let from = &self.nodes[edge.from];
            let to = &self.nodes[edge.to];
            let color = if edge.change == 1 {
                "#267b62"
            } else if edge.change == -1 {
                "#c45b50"
            } else if edge.candidate || edge.stale {
                "#b18736"
            } else {
                "#6c9181"
            };
            let backwards = to.x < from.x;
            let start_x = if backwards {
                from.x
            } else {
                from.x + NODE_WIDTH
            };
            let end_x = if backwards { to.x + NODE_WIDTH } else { to.x };
            let start_y = from.y + NODE_HEIGHT / 2.;
            let end_y = to.y + NODE_HEIGHT / 2.;
            let middle = (start_x + end_x) / 2.;
            let curve = if edge.from == edge.to {
                format!(
                    "M{start_x} {start_y} C{} {},{} {},{end_x} {end_y}",
                    start_x + 45.,
                    start_y - 85.,
                    end_x - 45.,
                    end_y - 85.
                )
            } else {
                format!("M{start_x} {start_y} C{middle} {start_y},{middle} {end_y},{end_x} {end_y}")
            };
            result.push_str(&format!(r##"<path d="{curve}" fill="none" stroke="{color}" stroke-dasharray="{}" marker-end="url(#arrow)"/>"##, if edge.candidate || edge.stale { "5 4" } else { "none" }));
        }
        for node in &self.nodes {
            result.push_str(&format!(r##"<rect x="{}" y="{}" width="{NODE_WIDTH}" height="{NODE_HEIGHT}" rx="8" fill="white" stroke="#b6cfc1"/><text x="{}" y="{}" font-family="sans-serif" font-size="11">{}</text><text x="{}" y="{}" font-family="sans-serif" font-size="9" fill="#7b8798">{}</text>"##, node.x, node.y, node.x + 10., node.y + 26., escape(&node.label.chars().take(32).collect::<String>()), node.x + 10., node.y + 51., escape(&format!("{}:{}", node.location.file, node.location.line).chars().take(36).collect::<String>())));
        }
        result.push_str("</svg>");
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn relation(from: &str, to: &str) -> Relation {
        Relation {
            from: from.into(),
            to: to.into(),
            certainty: "candidate".into(),
            ..Default::default()
        }
    }

    #[test]
    fn alternative_layouts_and_manual_positions_preserve_graph_identity() {
        let diagram = Diagram::build(
            "root",
            std::iter::empty(),
            &[relation("root", "child"), relation("child", "leaf")],
            false,
            &BTreeSet::new(),
        );
        for mode in [0, 1, 2] {
            let mut placed = diagram.clone();
            placed.arrange(
                mode,
                &BTreeMap::from([
                    ("child".into(), (451., 207.)),
                    ("leaf".into(), (f32::NAN, 10.)),
                ]),
            );
            assert_eq!(
                placed.nodes.iter().map(|node| &node.id).collect::<Vec<_>>(),
                diagram
                    .nodes
                    .iter()
                    .map(|node| &node.id)
                    .collect::<Vec<_>>()
            );
            assert_eq!(placed.edges.len(), diagram.edges.len());
            assert!(
                placed
                    .nodes
                    .iter()
                    .all(|node| node.x.is_finite() && node.y.is_finite())
            );
            let child = placed.nodes.iter().find(|node| node.id == "child").unwrap();
            assert_eq!((child.x, child.y), (451., 207.));
        }
    }

    #[test]
    fn cycles_and_multiple_paths_remain_finite_and_collapsible() {
        let relations = vec![
            relation("root", "child"),
            relation("child", "leaf"),
            relation("leaf", "root"),
        ];
        let diagram = Diagram::build(
            "root",
            std::iter::empty(),
            &relations,
            false,
            &BTreeSet::new(),
        );
        assert_eq!(diagram.nodes.len(), 3);
        assert_eq!(diagram.edges.len(), 3);
        let collapsed = Diagram::build(
            "root",
            std::iter::empty(),
            &relations,
            false,
            &BTreeSet::from(["root".into()]),
        );
        assert_eq!(collapsed.nodes.len(), 1);
        assert_eq!(collapsed.hidden, 2);
        assert!(diagram.edges.iter().all(|edge| edge.candidate));
    }

    #[test]
    fn file_grouping_aggregates_without_promoting_certainty_and_svg_escapes() {
        let symbols = [
            Symbol {
                symbol_id: "a".into(),
                qualified_name: "<&".into(),
                location: Location {
                    file: "same.rs".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Symbol {
                symbol_id: "b".into(),
                location: Location {
                    file: "same.rs".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let diagram = Diagram::build(
            "a",
            symbols.iter(),
            &[relation("a", "b"), relation("b", "a")],
            true,
            &BTreeSet::new(),
        );
        assert_eq!(diagram.nodes.len(), 1);
        assert_eq!(diagram.edges[0].count, 2);
        assert!(diagram.edges[0].candidate);
        let ungrouped = Diagram::build("a", symbols.iter(), &[], false, &BTreeSet::new());
        assert!(ungrouped.svg().contains("&lt;&amp;"));
    }

    #[test]
    fn large_graph_is_bounded_and_reports_hidden_nodes() {
        let relations = (0..1000)
            .map(|index| relation("root", &index.to_string()))
            .collect::<Vec<_>>();
        let diagram = Diagram::build(
            "root",
            std::iter::empty(),
            &relations,
            false,
            &BTreeSet::new(),
        );
        assert_eq!(diagram.nodes.len(), MAX_VISIBLE);
        assert_eq!(diagram.hidden, 1001 - MAX_VISIBLE);
    }
}
