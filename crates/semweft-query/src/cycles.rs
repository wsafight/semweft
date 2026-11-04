use crate::query;
use anyhow::Result;
use semweft_core::identity;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

pub(crate) fn report(result: &mut Value, deadline: Instant) -> Result<()> {
    let edges = result["dependencies"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    for edge in edges {
        query::check_deadline(deadline)?;
        if let (Some(from), Some(to)) = (edge["from"].as_str(), edge["to"].as_str()) {
            if from == "external dependencies" || to == "external dependencies" {
                continue;
            }
            graph.entry(from.into()).or_default().insert(to.into());
        }
    }
    let components =
        semweft_algo::components::strongly_connected(&graph, || query::check_deadline(deadline))?;
    let components = components
        .into_iter()
        .filter(|component| {
            component.len() > 1
                || graph
                    .get(&component[0])
                    .is_some_and(|neighbors| neighbors.contains(&component[0]))
        })
        .collect::<Vec<_>>();
    // One pass over the edges, bucketed by component, instead of one pass per component.
    let component_of = components
        .iter()
        .enumerate()
        .flat_map(|(index, component)| component.iter().map(move |node| (node.as_str(), index)))
        .collect::<BTreeMap<_, _>>();
    let mut evidence_by_component = vec![Vec::new(); components.len()];
    for edge in edges {
        query::check_deadline(deadline)?;
        if let (Some(from), Some(to)) = (edge["from"].as_str(), edge["to"].as_str())
            && let (Some(a), Some(b)) = (component_of.get(from), component_of.get(to))
            && a == b
        {
            evidence_by_component[*a].push(edge.clone());
        }
    }
    let mut cycles = vec![];
    for (component, evidence) in components.into_iter().zip(evidence_by_component) {
        query::check_deadline(deadline)?;
        let certainty = if evidence.iter().all(|edge| edge["certainty"] == "definite") {
            "definite"
        } else {
            "candidate"
        };
        cycles.push(json!({"cycle_id":identity("cycle", &[&result["dependency_kind"].to_string(), &serde_json::to_string(&component)?]),
            "nodes":component,"dependencies":evidence,"certainty":certainty,
            "kind":"strongly_connected_component","basis":"indexed dependency graph; not an enumeration of every simple cycle"}));
    }
    result["cycle_summary"] = json!({"component_count":cycles.len(),"graph_enumeration_complete":true,
        "dependency_kind":result["dependency_kind"],"services_included":result["dependency_kind"]=="service",
        "conclusion":"Candidate edges can produce candidate cycles; absence does not rule out unindexed dependencies"});
    result["cycles"] = json!(cycles);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn edge(from: &str, to: &str, certainty: &str) -> Value {
        json!({"from":from,"to":to,"certainty":certainty})
    }

    fn run(edges: Vec<Value>) -> Value {
        let mut result = json!({"dependency_kind":"module","dependencies":edges});
        report(&mut result, Instant::now() + Duration::from_secs(30)).unwrap();
        result
    }

    #[test]
    fn evidence_is_attributed_to_the_right_component() {
        let result = run(vec![
            edge("a", "b", "definite"),
            edge("b", "a", "definite"),
            edge("c", "d", "candidate"),
            edge("d", "c", "definite"),
            edge("a", "c", "definite"),
        ]);
        let cycles = result["cycles"].as_array().unwrap();
        assert_eq!(result["cycle_summary"]["component_count"], 2);
        assert_eq!(cycles.len(), 2);
        for cycle in cycles {
            let nodes = cycle["nodes"].as_array().unwrap();
            let evidence = cycle["dependencies"].as_array().unwrap();
            assert_eq!(evidence.len(), 2, "cross-component edge must be excluded");
            for e in evidence {
                assert!(nodes.contains(&e["from"]) && nodes.contains(&e["to"]));
            }
            let expected = if nodes.contains(&json!("a")) {
                "definite"
            } else {
                "candidate"
            };
            assert_eq!(cycle["certainty"], expected);
        }
    }

    #[test]
    fn self_loop_is_a_cycle_but_singletons_are_not() {
        let result = run(vec![edge("a", "a", "definite"), edge("b", "c", "definite")]);
        let cycles = result["cycles"].as_array().unwrap();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0]["nodes"], json!(["a"]));
        assert_eq!(cycles[0]["dependencies"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn acyclic_and_external_edges_report_no_cycles() {
        let result = run(vec![
            edge("a", "b", "definite"),
            edge("external dependencies", "a", "definite"),
            edge("b", "external dependencies", "definite"),
        ]);
        assert_eq!(result["cycles"], json!([]));
        assert_eq!(result["cycle_summary"]["component_count"], 0);
    }

    #[test]
    fn many_components_scale_linearly_in_edges() {
        let edges = (0..2000)
            .flat_map(|i| {
                let (a, b) = (format!("n{i}a"), format!("n{i}b"));
                [edge(&a, &b, "definite"), edge(&b, &a, "definite")]
            })
            .collect();
        let started = Instant::now();
        let result = run(edges);
        assert_eq!(result["cycles"].as_array().unwrap().len(), 2000);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
