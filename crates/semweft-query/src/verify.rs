use serde_json::{Value, json};

const RULE_VERSION: &str = "0.1";
const RULES: [(&str, &str); 4] = [
    (
        "configuration_change_requires_review",
        "Changed build or analysis configuration requires review",
    ),
    (
        "impact_analysis_incomplete",
        "Analysis gaps, pending evidence or traversal limits prevent a complete check",
    ),
    (
        "residual_reference_requires_review",
        "Old-name references after deletion, rename or move require binding review",
    ),
    (
        "candidate_impact_requires_review",
        "Indexed impact relations require review of whether callers must change",
    ),
];

pub(crate) fn rules() -> Value {
    json!(RULES.map(|(id, description)| json!({
        "id":id,"version":RULE_VERSION,"status":"experimental",
        "severity":"warning","description":description
    })))
}

fn finding(rule: &str, message: &str, evidence: Value) -> Value {
    json!({"rule_id":rule,"rule_version":RULE_VERSION,"severity":"warning",
        "status":"insufficient_evidence","message":message,"evidence":evidence})
}

fn status(findings: &[Value]) -> &'static str {
    if findings
        .iter()
        .any(|item| item["status"] == "violations_found")
    {
        "violations_found"
    } else if findings
        .iter()
        .any(|item| item["status"] == "insufficient_evidence")
    {
        "insufficient_evidence"
    } else {
        "no_violations_found"
    }
}

pub(crate) fn report(mut impact: Value, args: &Value) -> Value {
    let mut findings = vec![];
    for file in impact["configuration_changes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        findings.push(finding(
            "configuration_change_requires_review",
            "Configuration changes can affect unchanged declarations; symbol impact is not exhaustive",
            json!({"file":file,"snapshots":impact["snapshots"]}),
        ));
    }
    let incomplete = impact["completeness"]["status"] != "within_scope"
        || impact["evidence_summary"]["basis"] != "complete"
        || impact["output"]["truncated"] == true
        || impact["fresh_requirement"]["satisfied"] == false
        || ["base", "head"]
            .iter()
            .any(|side| impact["snapshots"][side]["fresh_requirement"]["satisfied"] == false)
        || impact["completeness"]["known_gaps"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        || impact["completeness"]["pending_units"]
            .as_array()
            .is_some_and(|items| !items.is_empty());
    if incomplete {
        findings.push(finding(
            "impact_analysis_incomplete",
            "Analysis scope, freshness or traversal limits leave missing evidence",
            json!({"completeness":impact["completeness"],
                "analysis_basis":impact["evidence_summary"]["basis"],
                "fresh_requirement":impact["fresh_requirement"],"snapshots":impact["snapshots"],
                "truncation_reason":impact["output"]["truncation_reason"],"frontier":impact["frontier"]}),
        ));
    }
    for reference in impact["residual_references"]
        .as_array()
        .into_iter()
        .flatten()
    {
        findings.push(finding(
            "residual_reference_requires_review",
            "The old name remains in head; lexical binding and migration status are unresolved",
            reference.clone(),
        ));
    }
    for relation in impact["relations"].as_array().into_iter().flatten() {
        findings.push(finding(
            "candidate_impact_requires_review",
            "An indexed impact relation does not establish whether this caller requires a change",
            relation.clone(),
        ));
    }
    let rules = RULES.map(|(id, _)| {
        let matching = findings.iter().filter(|item| item["rule_id"] == id).cloned().collect::<Vec<_>>();
        json!({"id":id,"version":RULE_VERSION,"status":status(&matching),"finding_count":matching.len()})
    });
    impact["verification"] = json!({
        "status":status(&findings),"rules":rules,"not_run":[],
        "finding_count":findings.len(),
        "violation_count":findings.iter().filter(|item| item["status"] == "violations_found").count(),
        "warning_count":findings.iter().filter(|item| item["severity"] == "warning").count(),
        "scope":{"diff":args["diff"],"depth":args["depth"].as_u64().unwrap_or(3),
            "max_nodes":args["max_nodes"].as_u64().unwrap_or(1000),
            "external_consumers_included":false,
            "limitations":["built-in review rules only", "no compiler or test execution", "no behavioral correctness proof"]}
    });
    impact["findings"] = json!(findings);
    impact
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete() -> Value {
        json!({"completeness":{"status":"within_scope","known_gaps":[],"pending_units":[]},
            "evidence_summary":{"basis":"complete"},"output":{"truncated":false},
            "configuration_changes":[],"residual_references":[],"relations":[]})
    }

    #[test]
    fn complete_empty_scope_has_no_findings_and_runs_all_rules() {
        let result = report(complete(), &json!({}));
        assert_eq!(result["verification"]["status"], "no_violations_found");
        assert_eq!(result["verification"]["finding_count"], 0);
        assert_eq!(result["verification"]["rules"].as_array().unwrap().len(), 4);
        assert!(
            result["verification"]["rules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|rule| rule["status"] == "no_violations_found"
                    && rule["version"] == RULE_VERSION)
        );
    }

    #[test]
    fn review_findings_preserve_evidence_without_promoting_candidates_to_violations() {
        let mut impact = complete();
        impact["configuration_changes"] = json!(["Cargo.toml"]);
        impact["residual_references"] = json!([{"name":"old","certainty":"candidate","location":{"file":"src/lib.rs","line":3,"column":2}}]);
        impact["relations"] = json!([
            {"relation_id":"candidate","side":"base","certainty":"candidate","evidence":[{"evidence_id":"ev1","source":"L1"}]},
            {"relation_id":"bound","side":"head","certainty":"definite","evidence":[{"evidence_id":"ev2","source":"L3"}]}
        ]);
        let result = report(impact.clone(), &json!({"depth":2,"max_nodes":10}));
        assert_eq!(result["verification"]["status"], "insufficient_evidence");
        assert_eq!(result["verification"]["warning_count"], 4);
        assert_eq!(result["verification"]["violation_count"], 0);
        assert_eq!(result["verification"]["scope"]["depth"], 2);
        assert_eq!(result["findings"][0]["evidence"]["file"], "Cargo.toml");
        assert_eq!(
            result["findings"][1]["evidence"],
            impact["residual_references"][0]
        );
        assert_eq!(result["findings"][2]["evidence"], impact["relations"][0]);
        assert_eq!(result["findings"][3]["evidence"], impact["relations"][1]);
    }

    #[test]
    fn each_analysis_gap_prevents_a_clean_verdict() {
        for patch in [
            json!({"completeness":{"status":"unknown"}}),
            json!({"completeness":{"status":"within_scope","known_gaps":[{"kind":"parse_error"}]}}),
            json!({"completeness":{"status":"within_scope","pending_units":[{"unit":"src/lib.rs"}]}}),
            json!({"evidence_summary":{"basis":"within_limits"}}),
            json!({"output":{"truncated":true,"truncation_reason":"node_limit"}}),
            json!({"fresh_requirement":{"satisfied":false}}),
            json!({"snapshots":{"base":{"fresh_requirement":{"satisfied":false}}}}),
        ] {
            let mut impact = complete();
            impact
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let result = report(impact, &json!({}));
            assert_eq!(
                result["verification"]["status"], "insufficient_evidence",
                "{patch}"
            );
            assert_eq!(
                result["findings"][0]["rule_id"],
                "impact_analysis_incomplete"
            );
        }
    }

    #[test]
    fn confirmed_violations_take_precedence_over_insufficient_evidence() {
        assert_eq!(
            status(&[
                json!({"status":"insufficient_evidence"}),
                json!({"status":"violations_found"})
            ]),
            "violations_found"
        );
    }
}
