//! SCIP is binding evidence, never an unqualified runtime call graph.
use anyhow::{Context, Result, bail};
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, PositionEncoding, occurrence::Typed_range};
use semweft_core::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Instant};

pub const ADAPTER_VERSION: &str = "scip-adapter@0.3-go-static-dispatch";

pub fn source_manifest(snapshot: &Snapshot) -> Value {
    let mut manifest = json!({"schema_version":"0.1","workspace_id":snapshot.manifest.workspace_id,
        "analysis_config":snapshot.manifest.analysis_versions.get("analysis_config"),
        "files":snapshot.manifest.files.iter().map(|(p,f)|(p.clone(),f.file_hash.clone())).collect::<BTreeMap<_,_>>(),
        "binary_files":snapshot.manifest.binary_files});
    if let Some(environment) = snapshot
        .manifest
        .analysis_versions
        .get("go_build_environment")
    {
        manifest["go_build_environment"] = json!(environment);
    }
    manifest
}

pub fn load(path: &Path) -> Result<(Index, String)> {
    let bytes = std::fs::read(path).context("invalid_argument: cannot read SCIP index")?;
    let index =
        Index::parse_from_bytes(&bytes).context("invalid_argument: malformed SCIP index")?;
    let tool = index
        .metadata
        .as_ref()
        .and_then(|m| m.tool_info.as_ref())
        .context("invalid_argument: SCIP tool name and version are required")?;
    if tool.name.is_empty() || tool.version.is_empty() {
        bail!("invalid_argument: SCIP tool name and version are required");
    }
    let backend = format!("scip:{}@{}:{}", tool.name, tool.version, digest(&bytes));
    Ok((index, backend))
}

fn key(file: &str, symbol: &str) -> String {
    if symbol.starts_with("local ") {
        format!("{file}\0{symbol}")
    } else {
        symbol.into()
    }
}

fn position_encoding(index: &Index, document: &Document) -> Option<PositionEncoding> {
    if document.position_encoding.value() == 0
        && index
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.tool_info.as_ref())
            .is_some_and(|tool| tool.name == "scip-typescript" && tool.version == "0.6.0")
    {
        Some(PositionEncoding::UTF16CodeUnitOffsetFromLineStart)
    } else {
        document.position_encoding.enum_value().ok()
    }
}

fn span(
    occurrence: &Occurrence,
    source: &LineIndex<'_>,
    encoding: Option<PositionEncoding>,
) -> Result<Span> {
    let range = match &occurrence.typed_range {
        Some(Typed_range::SingleLineRange(r)) => {
            vec![r.line, r.start_character, r.line, r.end_character]
        }
        Some(Typed_range::MultiLineRange(r)) => {
            vec![r.start_line, r.start_character, r.end_line, r.end_character]
        }
        None => match occurrence.range.as_slice() {
            [a, b, c] => vec![*a, *b, *a, *c],
            [a, b, c, d] => vec![*a, *b, *c, *d],
            _ => bail!("invalid SCIP range length"),
        },
        _ => bail!("unsupported SCIP typed range"),
    };
    let point = |line: i32, column: i32| -> Result<Position> {
        let line = u32::try_from(line).context("negative SCIP line")?;
        let column = u32::try_from(column).context("negative SCIP column")?;
        match encoding {
            Some(PositionEncoding::UTF8CodeUnitOffsetFromLineStart) => {
                let pos = Position {
                    line: line.checked_add(1).context("line overflow")?,
                    column: column.checked_add(1).context("column overflow")?,
                };
                source.byte_offset(pos).map_err(anyhow::Error::msg)?;
                Ok(pos)
            }
            Some(PositionEncoding::UTF16CodeUnitOffsetFromLineStart) => {
                source.from_utf16(line, column).map_err(anyhow::Error::msg)
            }
            Some(PositionEncoding::UTF32CodeUnitOffsetFromLineStart) => {
                let text = source
                    .line(line.checked_add(1).context("line overflow")?)
                    .map_err(anyhow::Error::msg)?;
                let byte = text
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(text.len()))
                    .nth(column as usize)
                    .context("SCIP column out of bounds")?;
                Ok(Position {
                    line: line + 1,
                    column: u32::try_from(byte)?
                        .checked_add(1)
                        .context("column overflow")?,
                })
            }
            _ => bail!("SCIP position encoding is unspecified or unsupported"),
        }
    };
    let result = Span {
        start: point(range[0], range[1])?,
        end: point(range[2], range[3])?,
    };
    if result.start >= result.end {
        bail!("empty or reversed SCIP range");
    }
    Ok(result)
}

pub fn analyze(
    index: &Index,
    snapshot: &Snapshot,
    backend: &str,
    deadline: Instant,
) -> Result<(SemanticBatch, Vec<Relation>, Vec<Diagnostic>)> {
    let mut gaps = vec![];
    if let Some(metadata) = index.metadata.as_ref() {
        let tool = metadata.tool_info.as_ref();
        gaps.push(Diagnostic{code:"scip_build_context".into(),file:None,message:json!({
            "tool":tool.map(|t|&t.name),"version":tool.map(|t|&t.version),
            "arguments":tool.map(|t|&t.arguments),"project_root":metadata.project_root,
            "basis":"producer-declared metadata and verified source manifest; compiler flags and conditional activation are not reconstructed"
        }).to_string()});
    }
    let mut mapped = vec![];
    let line_indices: BTreeMap<_, _> = snapshot
        .manifest
        .files
        .iter()
        .map(|(path, file)| (path, LineIndex::new(&file.content)))
        .collect();
    let mut definitions = BTreeMap::<String, Vec<&Symbol>>::new();
    for document in &index.documents {
        check_deadline(deadline)?;
        crate::workspace::safe_relative(&document.relative_path)?;
        let Some(file) = snapshot.manifest.files.get(&document.relative_path) else {
            gaps.push(Diagnostic {
                code: "scip_document_outside_scope".into(),
                file: Some(document.relative_path.clone()),
                message: "SCIP document is outside the captured source scope".into(),
            });
            continue;
        };
        if !document.text.is_empty() && document.text != file.content {
            bail!(
                "content_mismatch: SCIP text differs for {}",
                document.relative_path
            );
        }
        let encoding = position_encoding(index, document);
        if document.position_encoding.value() == 0
            && encoding == Some(PositionEncoding::UTF16CodeUnitOffsetFromLineStart)
        {
            gaps.push(Diagnostic {
                code: "scip_position_encoding_compatibility".into(),
                file: Some(document.relative_path.clone()),
                message: "scip-typescript@0.6.0 omits document position_encoding; using its verified TypeScript UTF-16 range convention, not metadata text_document_encoding".into(),
            });
        }
        for occurrence in &document.occurrences {
            check_deadline(deadline)?;
            if occurrence.symbol.is_empty() {
                continue;
            }
            let span = match span(occurrence, &line_indices[&document.relative_path], encoding) {
                Ok(span) => span,
                Err(error) => {
                    gaps.push(Diagnostic {
                        code: "scip_position_mapping_gap".into(),
                        file: Some(document.relative_path.clone()),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let identity = key(&document.relative_path, &occurrence.symbol);
            if occurrence.symbol_roles & 1 != 0 {
                let symbols = snapshot
                    .syntax
                    .symbols
                    .iter()
                    .filter(|s| s.file == document.relative_path && s.name_span == span)
                    .collect::<Vec<_>>();
                if symbols.len() == 1 {
                    let candidates = definitions.entry(identity.clone()).or_default();
                    if !candidates.contains(&symbols[0]) {
                        candidates.push(symbols[0]);
                    }
                } else {
                    gaps.push(Diagnostic {
                        code: "scip_definition_mapping_gap".into(),
                        file: Some(document.relative_path.clone()),
                        message: format!(
                            "{} does not align with a unique captured declaration",
                            occurrence.symbol
                        ),
                    });
                }
            }
            mapped.push((document, occurrence, span, identity));
        }
    }
    let mut outcomes = BTreeMap::<String, CallResolution>::new();
    let mut references = BTreeMap::new();
    for (document, occurrence, span, identity) in mapped {
        check_deadline(deadline)?;
        if occurrence.symbol_roles & (1 | 64) != 0 {
            continue;
        }
        let Some(targets) = definitions.get(&identity) else {
            gaps.push(Diagnostic {
                code: "scip_external_or_unresolved_symbol".into(),
                file: Some(document.relative_path.clone()),
                message: format!(
                    "{} at {}:{} has no aligned definition in this source scope",
                    occurrence.symbol, span.start.line, span.start.column
                ),
            });
            continue;
        };
        let call = snapshot
            .syntax
            .calls
            .iter()
            .find(|c| c.file == document.relative_path && c.span == span);
        if let Some(call) = call {
            let outcome =
                outcomes
                    .entry(call.callsite_id.clone())
                    .or_insert_with(|| CallResolution {
                        callsite_id: call.callsite_id.clone(),
                        targets: vec![],
                        diagnostics: vec![],
                        cfg_inactive: false,
                    });
            if call.kind == CallKind::Indirect {
                continue;
            }
            for target in targets
                .iter()
                .filter(|s| matches!(s.kind.as_str(), "function" | "trait_method"))
            {
                if !outcome
                    .targets
                    .iter()
                    .any(|t| t.symbol_id == target.symbol_id)
                {
                    outcome.targets.push(Target {
                        symbol_id: target.symbol_id.clone(),
                        file: target.file.clone(),
                        span: target.name_span,
                        concrete_function: target.kind == "function",
                        derivation: if call.kind == CallKind::Method
                            && call.file.ends_with(".go")
                            && target.file.ends_with(".go")
                            && target.kind == "function"
                        {
                            TargetDerivation::ScipGoStaticBinding
                        } else {
                            TargetDerivation::ScipBinding
                        },
                    });
                }
            }
            continue;
        }
        let syntax = snapshot
            .syntax
            .occurrences
            .iter()
            .find(|o| o.file == document.relative_path && o.span == span);
        let Some(syntax) = syntax else {
            gaps.push(Diagnostic {
                code: "scip_reference_mapping_gap".into(),
                file: Some(document.relative_path.clone()),
                message: format!(
                    "{} occurrence does not align with syntax",
                    occurrence.symbol
                ),
            });
            continue;
        };
        let kind = if occurrence.symbol_roles & 2 != 0 {
            "import"
        } else if occurrence.symbol_roles & 4 != 0 {
            "write"
        } else {
            &syntax.kind
        };
        let caller = syntax
            .enclosing_symbol_id
            .clone()
            .unwrap_or_else(|| format!("file:{}", document.relative_path));
        for target in targets {
            let id = identity_relation(
                kind,
                &caller,
                &target.symbol_id,
                &document.relative_path,
                span,
            );
            let certainty = if targets.len() == 1 {
                Certainty::Definite
            } else {
                Certainty::Candidate
            };
            references.insert(
                id.clone(),
                Relation {
                    relation_id: id.clone(),
                    kind: kind.into(),
                    from_symbol: caller.clone(),
                    to_symbol: target.symbol_id.clone(),
                    file: document.relative_path.clone(),
                    span,
                    certainty: certainty.clone(),
                    freshness: "current".into(),
                    runtime: "unavailable".into(),
                    origin: None,
                    evidence: vec![Evidence {
                        evidence_id: semweft_core::identity("ev", &[&id, backend]),
                        source: "L3".into(),
                        certainty,
                        indexer: backend.into(),
                        derivation: format!(
                            "SCIP occurrence {} aligned to captured syntax",
                            occurrence.symbol
                        ),
                        file_hash: snapshot.manifest.files[&document.relative_path]
                            .file_hash
                            .clone(),
                    }],
                },
            );
        }
    }
    let resolutions = snapshot
        .syntax
        .calls
        .iter()
        .map(|call| {
            outcomes
                .remove(&call.callsite_id)
                .unwrap_or_else(|| CallResolution {
                    callsite_id: call.callsite_id.clone(),
                    targets: vec![],
                    cfg_inactive: false,
                    diagnostics: vec![Diagnostic {
                        code: "scip_unresolved_call".into(),
                        file: Some(call.file.clone()),
                        message: format!("No aligned SCIP call binding for {}", call.expression),
                    }],
                })
        })
        .collect();
    gaps.push(Diagnostic{code:"scip_scope".into(),file:None,message:"SCIP bindings aligned with captured syntax; macro expansions, external consumers and exhaustive runtime dispatch are outside this model".into()});
    Ok((
        SemanticBatch {
            snapshot_id: snapshot.snapshot_id.clone(),
            syntax_epoch: snapshot.syntax_epoch,
            input_hash: snapshot.input_hash.clone(),
            backend: backend.into(),
            resolutions,
            diagnostics: gaps.clone(),
        },
        references.into_values().collect(),
        vec![],
    ))
}

pub fn identity_relation(kind: &str, from: &str, to: &str, file: &str, span: Span) -> String {
    semweft_core::identity(
        "rel",
        &[kind, from, to, file, &serde_json::to_string(&span).unwrap()],
    )
}

fn check_deadline(deadline: Instant) -> Result<()> {
    crate::query::check_cancelled()?;
    if Instant::now() >= deadline {
        bail!("timeout: SCIP import exceeded query deadline");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Metadata, ToolInfo};

    #[test]
    fn legacy_typescript_utf16_is_version_bound_and_preserves_explicit_encoding() {
        let mut index = Index::new();
        let mut tool = ToolInfo::new();
        tool.name = "scip-typescript".into();
        tool.version = "0.6.0".into();
        let mut metadata = Metadata::new();
        metadata.tool_info = Some(tool).into();
        index.metadata = Some(metadata).into();
        let mut document = Document::new();
        let mut occurrence = Occurrence::new();
        occurrence.range = vec![0, 3, 8];
        let source = LineIndex::new("😀 stock()");
        let mapped = span(&occurrence, &source, position_encoding(&index, &document)).unwrap();
        assert_eq!(mapped.start, Position { line: 1, column: 6 });
        assert_eq!(
            mapped.end,
            Position {
                line: 1,
                column: 11
            }
        );
        document.position_encoding = PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into();
        occurrence.range = vec![0, 5, 10];
        assert_eq!(
            span(&occurrence, &source, position_encoding(&index, &document)).unwrap(),
            mapped
        );
        document.position_encoding = protobuf::EnumOrUnknown::from_i32(0);
        index
            .metadata
            .as_mut()
            .unwrap()
            .tool_info
            .as_mut()
            .unwrap()
            .version = "unverified".into();
        assert!(span(&occurrence, &source, position_encoding(&index, &document)).is_err());
        index
            .metadata
            .as_mut()
            .unwrap()
            .tool_info
            .as_mut()
            .unwrap()
            .version = "0.6.0".into();
        document.position_encoding = protobuf::EnumOrUnknown::from_i32(99);
        assert!(span(&occurrence, &source, position_encoding(&index, &document)).is_err());
    }
}
