//! `find_implementations`: indexed Rust trait implementations. Each impl block
//! found by syntax yields an `implements` relation with L1 header evidence;
//! semantic references at the impl's trait name (LSP or SCIP) add L3 binding
//! evidence, and LSP implementation dispatch adds L3 method evidence.
use crate::{
    engine::StoredView,
    query::{
        check_deadline, failure, filtered, public_name, relation_value, resolve, summary,
        symbol_value, test_scope,
    },
};
use anyhow::Result;
use semweft_core::*;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

const HEADER_DERIVATION: &str =
    "syntax_impl_header+name_match; trait path imports and shadowing not resolved";
const EXTERNAL_ROOTS: [&str; 3] = ["std", "core", "alloc"];

/// Rust `impl` headers name traits by path; only the final segment is matched.
fn names_trait(block: &ImplBlock, name: &str) -> bool {
    let path = block.trait_path.trim().trim_start_matches("::");
    let first = path.split("::").next().unwrap_or("").trim();
    !(path.contains("::") && EXTERNAL_ROOTS.contains(&first))
        && path.rsplit("::").next().map(str::trim) == Some(name)
}

fn blanket(block: &ImplBlock) -> bool {
    let mut base = block.self_type.trim();
    while let Some(reference) = base.strip_prefix('&') {
        base = reference.trim_start();
        if base.starts_with('\'') {
            base = base
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest)
                .trim_start();
        }
        base = base.strip_prefix("mut ").unwrap_or(base).trim_start();
    }
    block.generics.iter().any(|generic| generic == base)
}

fn semantic_evidence<'a>(relation: &'a Relation, stored: &StoredView) -> Vec<&'a Evidence> {
    if relation.origin.is_some() || !matches!(relation.freshness.as_str(), "current" | "pending") {
        return vec![];
    }
    let Some(file) = stored.view.snapshot.manifest.files.get(&relation.file) else {
        return vec![];
    };
    relation
        .evidence
        .iter()
        .filter(|evidence| evidence.source == "L3" && *evidence.file_hash == *file.file_hash)
        .collect()
}

fn definite_binding(relation: &Relation, stored: &StoredView) -> bool {
    relation.freshness == "current"
        && relation.certainty == Certainty::Definite
        && semantic_evidence(relation, stored)
            .iter()
            .any(|evidence| evidence.certainty == Certainty::Definite)
}

fn worst(a: &str, b: &str) -> String {
    let rank = |f: &str| match f {
        "stale" => 2,
        "pending" => 1,
        _ => 0,
    };
    if rank(b) > rank(a) { b } else { a }.into()
}

struct Draft {
    relation: Relation,
    block: usize,
    scope: &'static str,
    conflict: bool,
    definite: BTreeSet<String>,
    freshness: BTreeMap<String, String>,
}

impl Draft {
    fn new(
        from: &str,
        to: &str,
        file: &str,
        span: Span,
        block: &ImplBlock,
        indexer: &str,
        name_match: bool,
    ) -> Self {
        let relation_id = identity(
            "rel",
            &[
                "implements",
                from,
                to,
                file,
                &serde_json::to_string(&span).unwrap(),
            ],
        );
        let evidence = Evidence {
            evidence_id: identity("ev", &[&relation_id, "L1"]),
            source: "L1".into(),
            certainty: Certainty::Candidate,
            indexer: indexer.into(),
            derivation: HEADER_DERIVATION.into(),
            file_hash: block.file_hash.as_str().into(),
        };
        Self {
            relation: Relation {
                relation_id,
                kind: "implements".into(),
                from_symbol: from.into(),
                to_symbol: to.into(),
                file: file.into(),
                span,
                certainty: Certainty::Candidate,
                freshness: "current".into(),
                runtime: "unavailable".into(),
                evidence: if name_match { vec![evidence] } else { vec![] },
                origin: None,
            },
            block: 0,
            scope: "production",
            conflict: false,
            definite: BTreeSet::new(),
            freshness: BTreeMap::new(),
        }
    }

    fn adopt(&mut self, source: &Relation, stored: &StoredView, prefix: &str, label: Option<&str>) {
        for evidence in semantic_evidence(source, stored)
            .into_iter()
            .filter(|evidence| label.is_none_or(|label| evidence.derivation.starts_with(label)))
        {
            let mut copy = evidence.clone();
            copy.evidence_id = identity(
                "ev",
                &[
                    &self.relation.relation_id,
                    prefix,
                    &source.relation_id,
                    &evidence.evidence_id,
                ],
            );
            copy.derivation = format!("{prefix}:{}", evidence.derivation).into();
            self.freshness
                .insert(copy.evidence_id.clone(), source.freshness.clone());
            if label.is_none()
                && source.certainty == Certainty::Definite
                && evidence.certainty == Certainty::Definite
            {
                self.definite.insert(copy.evidence_id.clone());
            }
            if self
                .relation
                .evidence
                .iter()
                .all(|evidence| evidence.evidence_id != copy.evidence_id)
            {
                self.relation.evidence.push(copy);
            }
        }
    }

    fn select(mut self, args: &Value) -> Option<Self> {
        let mut filters = args.clone();
        filters.as_object_mut().unwrap().remove("certainty");
        filters.as_object_mut().unwrap().remove("freshness");
        self.relation = filtered(vec![self.relation], &filters).pop()?;
        if self.relation.evidence.is_empty() {
            return None;
        }
        self.relation.certainty = if !self.conflict
            && self
                .relation
                .evidence
                .iter()
                .any(|evidence| self.definite.contains(&evidence.evidence_id))
        {
            Certainty::Definite
        } else {
            Certainty::Candidate
        };
        self.relation.freshness =
            self.relation
                .evidence
                .iter()
                .fold("current".to_owned(), |freshness, evidence| {
                    worst(
                        &freshness,
                        self.freshness
                            .get(&evidence.evidence_id)
                            .map_or("current", String::as_str),
                    )
                });
        if args["certainty"]
            .as_array()
            .is_some_and(|values| !values.contains(&json!(self.relation.certainty)))
            || args["freshness"]
                .as_array()
                .is_some_and(|values| !values.contains(&json!(self.relation.freshness)))
        {
            return None;
        }
        Some(self)
    }

    fn metadata(&self, block: &ImplBlock) -> Value {
        json!({"impl_id":block.impl_id,"trait_path":block.trait_path,"self_type":block.self_type,
            "location":{"file":block.file,"line":block.span.start.line,"column":block.span.start.column,"file_hash":block.file_hash},
            "scope":self.scope,"certainty":self.relation.certainty,"freshness":self.relation.freshness,
            "evidence":self.relation.evidence})
    }
}

fn gap(result: &mut Value, gap: Value) {
    result["completeness"]["known_gaps"]
        .as_array_mut()
        .unwrap()
        .push(gap);
    result["completeness"]["status"] = json!("partial");
}

#[cfg(test)]
#[path = "implementations_tests.rs"]
mod semantic_tests;

pub(crate) fn find(
    result: &mut Value,
    stored: &StoredView,
    args: &Value,
    deadline: Instant,
) -> Result<()> {
    let target = resolve(stored, args["symbol"].as_str().unwrap())?;
    let syntax = &stored.view.snapshot.syntax;
    let trait_symbol = match target.kind.as_str() {
        "trait" => target,
        "trait_method" => syntax
            .symbols
            .iter()
            .find(|symbol| {
                symbol.kind == "trait"
                    && symbol.file == target.file
                    && target.qualified_name
                        == format!("{}::{}", symbol.qualified_name, target.name)
            })
            .ok_or_else(|| {
                failure(
                    "symbol_not_found",
                    "trait method owner is not indexed",
                    json!({"symbol":target.symbol_id}),
                )
            })?,
        kind => {
            return Err(failure(
                "invalid_argument",
                "find_implementations expects a trait or trait method",
                json!({"symbol":target.symbol_id,"kind":kind,"qualified_name":public_name(target)}),
            ));
        }
    };
    let method_mode = target.kind == "trait_method";
    let indexer = stored
        .view
        .snapshot
        .manifest
        .analysis_versions
        .get("syntax")
        .cloned()
        .unwrap_or_else(crate::frontend::version);
    let index = stored.index();
    let same_name = syntax
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == "trait" && symbol.name == trait_symbol.name)
        .count();
    if same_name > 1 {
        gap(
            result,
            json!({"kind":"trait_name_ambiguous","file":trait_symbol.file,"reason":format!("{same_name} workspace traits are named {}; without L3 binding each matching impl header is a candidate for all of them",trait_symbol.name)}),
        );
    }
    let has_default = !target.signature.trim_end().ends_with(';');
    let dispatch_label = TargetDerivation::Implementation.evidence_label();
    let mut drafts = vec![];
    let mut inherited = vec![];
    let mut inherited_blankets = vec![];
    let mut excluded = vec![];
    for (position, block) in syntax.impls.iter().enumerate() {
        check_deadline(deadline)?;
        if block.negative {
            continue;
        }
        let name_match = names_trait(block, &trait_symbol.name);
        let at_header = index.relations_at(&block.file, block.trait_span);
        let header: Vec<&Relation> = at_header
            .iter()
            .filter(|relation| {
                relation.kind == "type"
                    && !semantic_evidence(relation, stored).is_empty()
                    && index
                        .symbol(&relation.to_symbol)
                        .is_some_and(|symbol| symbol.kind == "trait")
            })
            .collect();
        let bound_here: Vec<&Relation> = header
            .iter()
            .copied()
            .filter(|relation| relation.to_symbol == trait_symbol.symbol_id)
            .collect();
        if !name_match && bound_here.is_empty() {
            continue;
        }
        let competing: Vec<&Relation> = header
            .iter()
            .copied()
            .filter(|relation| relation.to_symbol != trait_symbol.symbol_id)
            .collect();
        let definite_targets: BTreeSet<&str> = header
            .iter()
            .copied()
            .filter(|relation| definite_binding(relation, stored))
            .map(|relation| relation.to_symbol.as_str())
            .collect();
        let block_scope = if block.test_only {
            "test"
        } else {
            "production"
        };
        if bound_here.is_empty() && definite_targets.len() == 1 {
            let other = competing
                .iter()
                .copied()
                .find(|relation| definite_binding(relation, stored))
                .unwrap();
            let mut draft = Draft::new(
                &block.impl_id,
                &trait_symbol.symbol_id,
                &block.file,
                block.trait_span,
                block,
                &indexer,
                false,
            );
            draft.scope = block_scope;
            for relation in competing
                .iter()
                .copied()
                .filter(|relation| relation.to_symbol == other.to_symbol)
            {
                draft.adopt(relation, stored, "impl_header_binding", None);
            }
            if let Some(draft) = draft.select(args) {
                let mut value = draft.metadata(block);
                value["bound_to"] = json!(other.to_symbol);
                value["bound_to_name"] = json!(index.symbol(&other.to_symbol).map(public_name));
                value["reason"] =
                    json!("definite semantic evidence binds this impl header to a different trait");
                excluded.push(value);
            }
            continue;
        }
        let conflict = !competing.is_empty();
        if conflict {
            gap(
                result,
                json!({"kind":"impl_header_binding_ambiguous","file":block.file,
                "impl_id":block.impl_id,"reason":"competing or conflicting trait bindings leave this implementation a candidate"}),
            );
        }
        let methods: Vec<&Symbol> = block
            .methods
            .iter()
            .filter_map(|id| index.symbol(id))
            .collect();
        let mut draft = if method_mode {
            let Some(method) = methods.iter().find(|method| method.name == target.name) else {
                let mut draft = Draft::new(
                    &block.impl_id,
                    &target.symbol_id,
                    &block.file,
                    block.trait_span,
                    block,
                    &indexer,
                    name_match,
                );
                draft.scope = block_scope;
                draft.conflict = conflict;
                for relation in &bound_here {
                    draft.adopt(relation, stored, "impl_header_binding", None);
                }
                let Some(draft) = draft.select(args) else {
                    continue;
                };
                if has_default {
                    inherited.push(draft.metadata(block));
                    if blanket(block) {
                        inherited_blankets.push(block);
                    }
                } else {
                    gap(
                        result,
                        json!({"kind":"impl_method_not_found","file":block.file,"reason":format!("impl {} for {} at line {} declares no {} and the trait method has no default body; it may be macro-generated or the impl may not compile",block.trait_path,block.self_type,block.span.start.line,target.name)}),
                    );
                }
                continue;
            };
            let mut draft = Draft::new(
                &method.symbol_id,
                &target.symbol_id,
                &method.file,
                method.name_span,
                block,
                &indexer,
                name_match,
            );
            draft.scope = if block.test_only {
                "test"
            } else {
                test_scope(method, stored)
            };
            for call in &index.adjacent(&method.symbol_id, semweft_store::Direction::Incoming) {
                check_deadline(deadline)?;
                if call.kind != "call" {
                    continue;
                }
                let at_call = index.relations_at(&call.file, call.span);
                let witnesses: Vec<&Relation> = at_call
                    .iter()
                    .filter(|relation| {
                        relation.kind == "call"
                            && relation.from_symbol == call.from_symbol
                            && relation.to_symbol == target.symbol_id
                    })
                    .collect();
                let mut dispatch = call.clone();
                dispatch.evidence = semantic_evidence(call, stored)
                    .into_iter()
                    .filter(|evidence| {
                        evidence.derivation.starts_with(dispatch_label)
                            && witnesses.iter().any(|witness| {
                                semantic_evidence(witness, stored)
                                    .iter()
                                    .any(|binding| binding.indexer == evidence.indexer)
                            })
                    })
                    .cloned()
                    .collect();
                if dispatch.evidence.is_empty() {
                    continue;
                }
                for witness in witnesses {
                    if semantic_evidence(witness, stored).iter().any(|binding| {
                        dispatch
                            .evidence
                            .iter()
                            .any(|evidence| evidence.indexer == binding.indexer)
                    }) {
                        dispatch.freshness = worst(&dispatch.freshness, &witness.freshness);
                    }
                }
                draft.adopt(&dispatch, stored, "impl_dispatch", Some(dispatch_label));
            }
            draft
        } else {
            let mut draft = Draft::new(
                &block.impl_id,
                &trait_symbol.symbol_id,
                &block.file,
                block.trait_span,
                block,
                &indexer,
                name_match,
            );
            draft.scope = block_scope;
            draft
        };
        for relation in &bound_here {
            draft.adopt(relation, stored, "impl_header_binding", None);
        }
        draft.conflict = conflict;
        draft.block = position;
        drafts.push(draft);
    }
    let mut kept: Vec<Draft> = drafts
        .into_iter()
        .filter_map(|draft| draft.select(args))
        .collect();
    for values in [&mut inherited, &mut excluded] {
        values.sort_by(|left, right| {
            (
                left["location"]["file"].as_str(),
                left["location"]["line"].as_u64(),
                left["impl_id"].as_str(),
            )
                .cmp(&(
                    right["location"]["file"].as_str(),
                    right["location"]["line"].as_u64(),
                    right["impl_id"].as_str(),
                ))
        });
    }
    kept.sort_by(|left, right| {
        (
            &left.relation.file,
            left.relation.span.start,
            &left.relation.relation_id,
        )
            .cmp(&(
                &right.relation.file,
                right.relation.span.start,
                &right.relation.relation_id,
            ))
    });
    let values: Vec<Value> = kept
        .iter()
        .map(|draft| {
            let block = &syntax.impls[draft.block];
            let mut value = relation_value(&draft.relation, stored);
            value["scope"] = json!(draft.scope);
            if !method_mode {
                value["from_name"] = json!(format!("impl {} for {}", block.trait_path, block.self_type));
            }
            value["implementation"] = json!({"impl_id":block.impl_id,"trait_path":block.trait_path,"self_type":block.self_type,
                "generics":block.generics,"unsafe":block.unsafe_impl,"blanket":blanket(block),
                "location":{"file":block.file,"line":block.span.start.line,"column":block.span.start.column,"file_hash":block.file_hash},
                "span":block.span,
                "methods":block.methods.iter().filter_map(|id|index.symbol(id)).map(|method|json!({"symbol_id":method.symbol_id,"name":method.name,"line":method.name_span.start.line,"scope":test_scope(method,stored)})).collect::<Vec<_>>()});
            value
        })
        .collect();
    let relations: Vec<Relation> = kept.iter().map(|draft| draft.relation.clone()).collect();
    let tests = kept.iter().filter(|draft| draft.scope == "test").count();
    let semantic = kept
        .iter()
        .filter(|draft| {
            draft
                .relation
                .evidence
                .iter()
                .any(|evidence| evidence.source == "L3")
        })
        .count();
    let blankets: Vec<&ImplBlock> = kept
        .iter()
        .map(|draft| &syntax.impls[draft.block])
        .filter(|block| blanket(block))
        .collect();
    for block in &blankets {
        gap(
            result,
            json!({"kind":"blanket_impl_coverage_unknown","file":block.file,"reason":format!("impl<{}> {} for {} covers types that are not enumerated",block.generics.join(", "),block.trait_path,block.self_type)}),
        );
    }
    gap(
        result,
        json!({"kind":"external_implementations_not_indexed","reason":"implementations in dependencies, downstream crates and unindexed workspaces are outside the analysis boundary"}),
    );
    gap(
        result,
        json!({"kind":"generated_impls_not_extracted","reason":"derive, procedural and declarative macro impls are not expanded; only written impl blocks are listed"}),
    );
    let missing_semantic = kept.len() - semantic
        + inherited
            .iter()
            .filter(|value| {
                value["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|evidence| evidence["source"] != "L3")
            })
            .count();
    for block in inherited_blankets {
        gap(
            result,
            json!({"kind":"blanket_impl_coverage_unknown","file":block.file,
                    "reason":"inherited default belongs to a blanket implementation whose covered types are not enumerated"}),
        );
    }
    if missing_semantic > 0 {
        gap(
            result,
            json!({"kind":"implementation_semantic_evidence_missing","count":missing_semantic,"reason":"some explicit implementations or inherited defaults rely only on syntax header name matching; no published semantic binding covers their trait path"}),
        );
    }
    result["target"] = symbol_value(target, stored, false, true);
    result["trait"] = symbol_value(trait_symbol, stored, false, false);
    result["mode"] = json!(if method_mode { "trait_method" } else { "trait" });
    result["implementation_counts"] = json!({"total":kept.len(),"production":kept.len()-tests,"test":tests,
        "with_semantic_evidence":semantic,"blanket":blankets.len(),"inherited_defaults":inherited.len(),"excluded":excluded.len()});
    result["inherited_defaults"] = json!(inherited);
    result["excluded_candidates"] = json!(excluded);
    result["relations"] = json!(values);
    result["evidence_summary"] = summary(&relations, "complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::StoredView;
    use semweft_lang_rust::RustParser;
    use std::{collections::BTreeMap, time::Duration};

    const LIB: &str = "pub trait Deduct {\n    fn deduct(&self, qty: u32) -> u32;\n    fn name(&self) -> &str { \"d\" }\n}\npub struct Stock;\npub struct Other;\nimpl Deduct for Stock {\n    fn deduct(&self, qty: u32) -> u32 { qty }\n}\nimpl crate::Deduct for Other {\n    fn deduct(&self, qty: u32) -> u32 { qty }\n    fn name(&self) -> &str { \"o\" }\n}\nimpl<T: Clone> Deduct for T {\n    fn deduct(&self, qty: u32) -> u32 { qty }\n}\nimpl std::fmt::Display for Stock {\n    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}\n#[cfg(test)]\nmod tests {\n    struct Fake;\n    impl super::Deduct for Fake {\n        fn deduct(&self, qty: u32) -> u32 { qty }\n    }\n}\n";

    fn view(references: Vec<Relation>) -> StoredView {
        let manifest = Manifest {
            workspace_id: "ws".into(),
            files: BTreeMap::from([("src/lib.rs".into(), SourceFile::new(LIB))]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::new(),
            scope: "test".into(),
        };
        let syntax = RustParser::new().unwrap().analyze(&manifest).unwrap();
        let view = RefreshCoordinator::default()
            .publish_inputs(manifest, syntax)
            .unwrap();
        StoredView::new(view, references, vec![])
    }

    fn run(stored: &StoredView, args: Value) -> Result<Value> {
        let mut result = crate::query::envelope(stored, json!({}));
        find(
            &mut result,
            stored,
            &args,
            Instant::now() + Duration::from_secs(5),
        )?;
        Ok(result)
    }

    fn l3_reference(block: &ImplBlock, to: &str, certainty: Certainty) -> Relation {
        Relation {
            relation_id: format!("ref-{}-{to}", block.impl_id),
            kind: "type".into(),
            from_symbol: format!("file:{}", block.file),
            to_symbol: to.into(),
            file: block.file.clone(),
            span: block.trait_span,
            certainty: certainty.clone(),
            freshness: "current".into(),
            runtime: "unavailable".into(),
            evidence: vec![Evidence {
                evidence_id: format!("ev-{}", block.impl_id),
                source: "L3".into(),
                certainty,
                indexer: "scip-test".into(),
                derivation: "SCIP occurrence aligned to captured syntax".into(),
                file_hash: block.file_hash.as_str().into(),
            }],
            origin: None,
        }
    }

    #[test]
    fn trait_lists_written_impls_with_scope_blanket_and_boundary_gaps() {
        let stored = view(vec![]);
        let result = run(&stored, json!({"symbol":"Deduct"})).unwrap();
        assert_eq!(result["mode"], "trait");
        let relations = result["relations"].as_array().unwrap();
        let names: Vec<_> = relations
            .iter()
            .map(|relation| relation["from_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "impl Deduct for Stock",
                "impl crate::Deduct for Other",
                "impl Deduct for T",
                "impl super::Deduct for Fake"
            ]
        );
        assert!(
            relations
                .iter()
                .all(|relation| relation["kind"] == "implements"
                    && relation["certainty"] == "candidate"
                    && relation["evidence"][0]["source"] == "L1")
        );
        assert_eq!(relations[3]["scope"], "test");
        assert_eq!(relations[2]["implementation"]["blanket"], true);
        let counts = &result["implementation_counts"];
        assert_eq!(counts["total"], 4);
        assert_eq!(counts["test"], 1);
        assert_eq!(counts["with_semantic_evidence"], 0);
        let gaps: Vec<_> = result["completeness"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| gap["kind"].as_str().unwrap())
            .collect();
        for kind in [
            "external_implementations_not_indexed",
            "blanket_impl_coverage_unknown",
            "implementation_semantic_evidence_missing",
        ] {
            assert!(gaps.contains(&kind), "{gaps:?}");
        }
        assert_eq!(result["completeness"]["status"], "partial");
        assert_eq!(result["evidence_summary"]["relation_count"], 4);
        let ids: std::collections::BTreeSet<_> = relations
            .iter()
            .map(|relation| relation["relation_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 4, "relation IDs are distinct");
    }

    #[test]
    fn trait_method_mode_separates_overrides_from_inherited_defaults() {
        let stored = view(vec![]);
        let deduct = run(&stored, json!({"symbol":"Deduct::deduct"})).unwrap();
        assert_eq!(deduct["mode"], "trait_method");
        assert_eq!(deduct["relations"].as_array().unwrap().len(), 4);
        assert_eq!(
            deduct["relations"][0]["from_name"],
            "<Deduct for Stock>::deduct"
        );
        assert_eq!(deduct["inherited_defaults"], json!([]));
        let name = run(&stored, json!({"symbol":"Deduct::name"})).unwrap();
        let relations = name["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["implementation"]["self_type"], "Other");
        assert_eq!(name["inherited_defaults"].as_array().unwrap().len(), 3);
        assert_eq!(name["implementation_counts"]["inherited_defaults"], 3);
    }

    #[test]
    fn semantic_header_bindings_upgrade_exclude_and_filter_candidates() {
        let plain = view(vec![]);
        let impls = &plain.view.snapshot.syntax.impls;
        let deduct = resolve(&plain, "Deduct").unwrap().symbol_id.clone();
        let stock = resolve(&plain, "Stock").unwrap().symbol_id.clone();
        let refs = vec![
            l3_reference(&impls[0], &deduct, Certainty::Definite),
            // A semantic backend says this header names a different trait.
            l3_reference(&impls[1], &stock, Certainty::Definite),
        ];
        let stored = view(refs);
        let index_trait = |id: &str| stored.index().symbol(id).map(|symbol| symbol.kind.clone());
        assert_eq!(index_trait(&stock).as_deref(), Some("struct"));
        let result = run(&stored, json!({"symbol":"Deduct"})).unwrap();
        let first = &result["relations"][0];
        assert_eq!(first["certainty"], "definite");
        assert!(
            first["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|evidence| {
                    evidence["source"] == "L3"
                        && evidence["derivation"]
                            .as_str()
                            .unwrap()
                            .starts_with("impl_header_binding:")
                })
        );
        assert_eq!(result["implementation_counts"]["with_semantic_evidence"], 1);
        // A non-trait target is not a competing trait binding.
        assert_eq!(result["implementation_counts"]["excluded"], 0);
        let only_l3 = run(&stored, json!({"symbol":"Deduct","source":["L3"]})).unwrap();
        assert_eq!(only_l3["relations"].as_array().unwrap().len(), 1);
        let tests_path = run(&stored, json!({"symbol":"Deduct","certainty":["definite"]})).unwrap();
        assert_eq!(tests_path["relations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn non_trait_targets_are_rejected() {
        let stored = view(vec![]);
        let error = run(&stored, json!({"symbol":"Stock"})).unwrap_err();
        assert_eq!(
            crate::query::error_value(&error)["error"]["code"],
            "invalid_argument"
        );
    }

    #[test]
    fn external_paths_and_negative_impls_do_not_match_workspace_traits() {
        let block = |path: &str, negative: bool| ImplBlock {
            impl_id: "i".into(),
            file: "f".into(),
            file_hash: "h".into(),
            span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            trait_path: path.into(),
            trait_span: Span {
                start: Position { line: 1, column: 1 },
                end: Position { line: 1, column: 2 },
            },
            self_type: "S".into(),
            generics: vec![],
            negative,
            unsafe_impl: false,
            test_only: false,
            methods: vec![],
        };
        assert!(names_trait(&block("Display", false), "Display"));
        assert!(names_trait(&block("core", false), "core"));
        assert!(names_trait(&block("crate::fmt::Display", false), "Display"));
        assert!(!names_trait(&block("std::fmt::Display", false), "Display"));
        assert!(!names_trait(
            &block("::core::fmt::Display", false),
            "Display"
        ));
        assert!(!names_trait(&block("DisplayExt", false), "Display"));
    }
}
