use std::collections::{BTreeMap, BTreeSet};

pub fn strongly_connected<Node: Ord + Clone, Error>(
    adjacency: &BTreeMap<Node, BTreeSet<Node>>,
    mut check: impl FnMut() -> Result<(), Error>,
) -> Result<Vec<Vec<Node>>, Error> {
    let mut reverse = BTreeMap::<Node, BTreeSet<Node>>::new();
    for (from, neighbors) in adjacency {
        check()?;
        reverse.entry(from.clone()).or_default();
        for to in neighbors {
            check()?;
            reverse.entry(to.clone()).or_default().insert(from.clone());
        }
    }
    let mut visited = BTreeSet::new();
    let mut order = vec![];
    for root in reverse.keys() {
        let mut stack = vec![(root.clone(), false)];
        while let Some((node, exiting)) = stack.pop() {
            check()?;
            if exiting {
                order.push(node);
            } else if visited.insert(node.clone()) {
                stack.push((node.clone(), true));
                for next in adjacency.get(&node).into_iter().flatten().rev() {
                    check()?;
                    if !visited.contains(next) {
                        stack.push((next.clone(), false));
                    }
                }
            }
        }
    }
    visited.clear();
    let mut components = vec![];
    for root in order.into_iter().rev() {
        if visited.contains(&root) {
            continue;
        }
        let mut stack = vec![root];
        let mut component = vec![];
        while let Some(node) = stack.pop() {
            check()?;
            if visited.insert(node.clone()) {
                stack.extend(reverse.get(&node).into_iter().flatten().cloned());
                component.push(node);
            }
        }
        component.sort();
        components.push(component);
    }
    components.sort();
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_deterministic_and_include_isolated_nodes() {
        let graph = BTreeMap::from([
            ("a", BTreeSet::from(["b"])),
            ("b", BTreeSet::from(["a", "c"])),
            ("c", BTreeSet::from(["c"])),
            ("d", BTreeSet::new()),
        ]);
        let result = strongly_connected(&graph, || Ok::<_, ()>(())).unwrap();
        assert_eq!(result, vec![vec!["a", "b"], vec!["c"], vec!["d"]]);
        assert!(strongly_connected(&graph, || Err::<(), _>("cancelled")).is_err());
    }

    #[test]
    fn deep_graph_does_not_require_recursive_stack() {
        let graph = (0..10_000)
            .map(|node| (node, BTreeSet::from([node + 1])))
            .collect();
        assert_eq!(
            strongly_connected(&graph, || Ok::<_, ()>(()))
                .unwrap()
                .len(),
            10_001
        );
    }
}
