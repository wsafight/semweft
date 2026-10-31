use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Node {
    path: String,
    name: String,
    children: BTreeMap<String, Node>,
    file: bool,
}

#[derive(Default)]
pub(super) struct FileTree {
    root: Node,
}

#[derive(Clone, Debug)]
pub(super) struct TreeEntry {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub directory: bool,
    pub expanded: bool,
}

impl FileTree {
    pub fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut tree = Self::default();
        for path in paths {
            let mut node = &mut tree.root;
            let mut prefix = String::new();
            for part in path.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                node = node.children.entry(part.into()).or_insert_with(|| Node {
                    path: prefix.clone(),
                    name: part.into(),
                    ..Default::default()
                });
            }
            node.file = true;
        }
        tree
    }

    pub fn directories(&self) -> BTreeSet<String> {
        fn visit(node: &Node, paths: &mut BTreeSet<String>) {
            for child in node.children.values().filter(|n| !n.file) {
                paths.insert(child.path.clone());
                visit(child, paths);
            }
        }
        let mut paths = BTreeSet::new();
        visit(&self.root, &mut paths);
        paths
    }

    // Filtering retains ancestors and reveals matches inside collapsed folders.
    pub fn visible(&self, query: &str, collapsed: &BTreeSet<String>) -> Vec<TreeEntry> {
        fn visit(
            node: &Node,
            depth: usize,
            words: &[String],
            collapsed: &BTreeSet<String>,
        ) -> Vec<TreeEntry> {
            if node.file {
                let path = node.path.to_lowercase();
                return if words.iter().all(|word| path.contains(word)) {
                    vec![TreeEntry {
                        path: node.path.clone(),
                        name: node.name.clone(),
                        depth,
                        directory: false,
                        expanded: false,
                    }]
                } else {
                    vec![]
                };
            }
            let mut children = node.children.values().collect::<Vec<_>>();
            children.sort_by_key(|child| (child.file, child.name.as_str()));
            let descendants = children
                .into_iter()
                .flat_map(|child| visit(child, depth + 1, words, collapsed))
                .collect::<Vec<_>>();
            if descendants.is_empty() {
                return vec![];
            }
            let expanded = !words.is_empty() || !collapsed.contains(&node.path);
            let mut entries = vec![TreeEntry {
                path: node.path.clone(),
                name: node.name.clone(),
                depth,
                directory: true,
                expanded,
            }];
            if expanded {
                entries.extend(descendants);
            }
            entries
        }
        let words = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut children = self.root.children.values().collect::<Vec<_>>();
        children.sort_by_key(|child| (child.file, child.name.as_str()));
        children
            .into_iter()
            .flat_map(|node| visit(node, 0, &words, collapsed))
            .collect()
    }

    pub fn reveal(path: &str, collapsed: &mut BTreeSet<String>) {
        for (index, _) in path.match_indices('/') {
            collapsed.remove(&path[..index]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> FileTree {
        FileTree::new(
            [
                "src/z.rs",
                "src/nested/lib.rs",
                "src/nested/use.ts",
                "main.go",
            ]
            .map(str::to_owned),
        )
    }

    #[test]
    fn directories_sort_before_files_and_collapse_hides_descendants() {
        let rows = tree().visible("", &BTreeSet::from(["src/nested".into()]));
        assert_eq!(
            rows.iter()
                .map(|n| (n.path.as_str(), n.depth))
                .collect::<Vec<_>>(),
            [
                ("src", 0),
                ("src/nested", 1),
                ("src/z.rs", 1),
                ("main.go", 0)
            ]
        );
    }

    #[test]
    fn filter_reveals_only_matching_files_and_their_ancestors() {
        let tree = tree();
        let rows = tree.visible("SRC lib.RS", &tree.directories());
        assert_eq!(
            rows.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(),
            ["src", "src/nested", "src/nested/lib.rs"]
        );
        assert!(rows[0].expanded && rows[1].expanded);
        assert!(tree.visible("missing", &BTreeSet::new()).is_empty());
    }

    #[test]
    fn opening_a_file_reveals_ancestors_without_expanding_other_folders() {
        let mut collapsed = BTreeSet::from(["src".into(), "src/nested".into(), "other".into()]);
        FileTree::reveal("src/nested/lib.rs", &mut collapsed);
        assert_eq!(collapsed, BTreeSet::from(["other".into()]));
    }
}
