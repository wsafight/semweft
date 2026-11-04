use crate::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub syntax_epoch: u64,
    pub input_hash: String,
    pub manifest: Manifest,
    pub syntax: SyntaxFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epochs {
    pub syntax_epoch: u64,
    pub semantic_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisView {
    pub snapshot: Arc<Snapshot>,
    pub epochs: Epochs,
    pub relations: Vec<Relation>,
    pub pending_units: Vec<String>,
    pub known_gaps: Vec<Diagnostic>,
    /// Backend outcomes behind `relations`, kept so a later refresh can reuse
    /// results for callsites outside the invalidated scope.
    #[serde(default)]
    pub resolutions: Vec<CallResolution>,
}

impl AnalysisView {
    pub fn completeness_status(&self) -> &str {
        if self.pending_units.is_empty() && self.known_gaps.is_empty() {
            "within_scope"
        } else {
            "partial"
        }
    }
}

#[derive(Debug, Clone)]
pub struct RefreshTicket {
    snapshot_id: String,
    syntax_epoch: u64,
    input_hash: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallResolution {
    pub callsite_id: String,
    pub targets: Vec<Target>,
    pub diagnostics: Vec<Diagnostic>,
    /// The callsite is compiled out under the analyzed configuration (for
    /// example a disabled Cargo feature); its bindings stay candidates.
    pub cfg_inactive: bool,
}

#[derive(Debug, Clone)]
pub struct SemanticBatch {
    pub snapshot_id: String,
    pub syntax_epoch: u64,
    pub input_hash: String,
    pub backend: String,
    /// Exactly one outcome per syntax callsite, including unresolved outcomes.
    pub resolutions: Vec<CallResolution>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RefreshScope {
    /// Every callsite is queried again.
    Full { reason: String },
    /// Only function bodies changed; callsites elsewhere keep their outcomes.
    BodyLocal { changed_files: Vec<String> },
}

/// Which callsites a refresh must query, relative to the newest published
/// semantic view. Reused outcomes are remapped to the new snapshot and still
/// pass the same validation as freshly queried ones.
#[derive(Debug, Clone)]
pub struct RefreshPlan {
    pub scope: RefreshScope,
    pub base_syntax_epoch: Option<u64>,
    pub query: BTreeSet<String>,
    pub reused: Vec<CallResolution>,
}

/// In-memory prototype, deliberately not a production storage decision.
#[derive(Default)]
pub struct RefreshCoordinator {
    views: Vec<Arc<AnalysisView>>,
    current: Option<usize>,
    generation: u64,
    completed_epoch: u64,
    syntax_high_water: u64,
}

impl RefreshCoordinator {
    pub fn retained(&self) -> &[Arc<AnalysisView>] {
        &self.views
    }

    /// Restore immutable publications; callers validate the persisted envelope.
    pub fn restore(views: Vec<Arc<AnalysisView>>) -> Result<Self, String> {
        let mut previous_epoch = 0;
        let mut completed_epoch = 0;
        for view in &views {
            let snapshot = &view.snapshot;
            if view.epochs.syntax_epoch != snapshot.syntax_epoch
                || view.epochs.semantic_epoch > snapshot.syntax_epoch
                || snapshot.input_hash != snapshot.manifest.input_hash()
            {
                return Err("index_store_error: invalid persisted version".into());
            }
            validate_syntax(&snapshot.manifest, &snapshot.syntax)?;
            previous_epoch = previous_epoch.max(snapshot.syntax_epoch);
            completed_epoch = completed_epoch.max(view.epochs.semantic_epoch);
        }
        Ok(Self {
            current: views.len().checked_sub(1),
            views,
            generation: previous_epoch,
            completed_epoch,
            syntax_high_water: previous_epoch,
        })
    }

    /// Restore only resident roots while preserving epoch allocation across
    /// historical publications that a storage layer will load on demand.
    pub fn restore_roots(
        views: Vec<Arc<AnalysisView>>,
        syntax_high_water: u64,
        semantic_high_water: u64,
    ) -> Result<Self, String> {
        let mut restored = Self::restore(views)?;
        if restored.syntax_high_water > syntax_high_water
            || restored.completed_epoch > semantic_high_water
            || semantic_high_water > syntax_high_water
        {
            return Err("index_store_error: invalid epoch high water".into());
        }
        restored.syntax_high_water = syntax_high_water;
        restored.completed_epoch = semantic_high_water;
        restored.generation = syntax_high_water;
        Ok(restored)
    }

    /// The durable catalog owns history. Resident refresh state only needs the
    /// latest publication and the last usable semantic baseline.
    pub fn retain_refresh_roots(&mut self) {
        let latest = self.latest();
        let base = self.latest_current();
        self.views.retain(|view| {
            latest.as_ref().is_some_and(|v| Arc::ptr_eq(v, view))
                || base.as_ref().is_some_and(|v| Arc::ptr_eq(v, view))
        });
        self.current =
            latest.and_then(|latest| self.views.iter().position(|v| Arc::ptr_eq(v, &latest)));
    }

    pub fn latest(&self) -> Option<Arc<AnalysisView>> {
        self.current
            .and_then(|index| self.views.get(index))
            .cloned()
    }

    pub fn select_latest(&mut self, snapshot_id: &str, semantic_epoch: u64) -> Result<(), String> {
        self.current = Some(
            self.views
                .iter()
                .position(|v| {
                    v.snapshot.snapshot_id == snapshot_id
                        && v.epochs.semantic_epoch == semantic_epoch
                })
                .ok_or("snapshot_expired")?,
        );
        Ok(())
    }

    pub fn view(&self, snapshot_id: &str, semantic_epoch: u64) -> Option<Arc<AnalysisView>> {
        self.views
            .iter()
            .find(|v| {
                v.snapshot.snapshot_id == snapshot_id && v.epochs.semantic_epoch == semantic_epoch
            })
            .cloned()
    }

    pub fn publish_inputs(
        &mut self,
        manifest: Manifest,
        syntax: SyntaxFacts,
    ) -> Result<Arc<AnalysisView>, String> {
        for (path, file) in &manifest.files {
            if file.file_hash != digest(file.content.as_bytes()) {
                return Err(format!("content_mismatch: {path}"));
            }
        }
        let input_hash = manifest.input_hash();
        if let Some(previous) = self.latest()
            && previous.snapshot.input_hash == input_hash
        {
            if previous.snapshot.syntax != syntax {
                return Err("inconsistent_syntax_rebuild".into());
            }
            return Ok(previous);
        }
        validate_syntax(&manifest, &syntax)?;
        self.generation += 1;
        let syntax_epoch = self
            .syntax_high_water
            .checked_add(1)
            .ok_or("index_store_error: syntax epoch exhausted")?;
        self.syntax_high_water = syntax_epoch;
        let snapshot_id = identity(
            "snap",
            &[
                &manifest.workspace_id,
                &input_hash,
                &syntax_epoch.to_string(),
            ],
        );
        let blocked = manifest
            .analysis_versions
            .get("trust")
            .is_some_and(|v| v == "false");
        let pending_units = manifest
            .files
            .keys()
            .filter(|path| {
                !blocked
                    && matches!(
                        path.rsplit('.').next(),
                        Some(
                            "rs" | "go"
                                | "ts"
                                | "tsx"
                                | "js"
                                | "jsx"
                                | "mts"
                                | "cts"
                                | "mjs"
                                | "cjs"
                        )
                    )
            })
            .cloned()
            .collect();
        let known_gaps = syntax.diagnostics.clone();
        let snapshot = Arc::new(Snapshot {
            snapshot_id,
            syntax_epoch,
            input_hash,
            manifest,
            syntax,
        });
        let view = Arc::new(AnalysisView {
            snapshot,
            epochs: Epochs {
                syntax_epoch,
                semantic_epoch: self.completed_epoch,
            },
            relations: vec![],
            pending_units,
            known_gaps,
            resolutions: vec![],
        });
        self.views.push(view.clone());
        self.current = self.views.len().checked_sub(1);
        Ok(view)
    }

    /// The newest view whose semantic facts match its own snapshot.
    pub fn latest_current(&self) -> Option<Arc<AnalysisView>> {
        self.views
            .iter()
            .rev()
            .find(|v| {
                v.epochs.semantic_epoch == v.epochs.syntax_epoch
                    && !v
                        .snapshot
                        .manifest
                        .analysis_versions
                        .contains_key("revision")
            })
            .cloned()
    }

    /// Plans the refresh of the latest snapshot. Because the base is the last
    /// published semantic view, edits from superseded refreshes are included.
    pub fn plan_refresh(&self) -> Result<RefreshPlan, String> {
        let latest = self.latest().ok_or("not_indexed")?;
        let full = |base: Option<&AnalysisView>, reason: &str| RefreshPlan {
            scope: RefreshScope::Full {
                reason: reason.into(),
            },
            base_syntax_epoch: base.map(|b| b.epochs.syntax_epoch),
            query: latest
                .snapshot
                .syntax
                .calls
                .iter()
                .map(|c| c.callsite_id.clone())
                .collect(),
            reused: vec![],
        };
        let Some(base) = self.latest_current() else {
            return Ok(full(None, "no_published_semantic_view"));
        };
        let (old, new) = (&base.snapshot, &latest.snapshot);
        if old.manifest.analysis_versions != new.manifest.analysis_versions {
            return Ok(full(Some(&base), "analysis_versions_changed"));
        }
        let non_rust = |m: &Manifest| {
            (
                m.files
                    .iter()
                    .filter(|(p, _)| !p.ends_with(".rs"))
                    .map(|(p, f)| (p.clone(), f.file_hash.clone()))
                    .collect::<BTreeMap<_, _>>(),
                m.binary_files.clone(),
            )
        };
        if non_rust(&old.manifest) != non_rust(&new.manifest) {
            return Ok(full(Some(&base), "build_or_configuration_input_changed"));
        }
        let rust = |m: &Manifest| -> BTreeSet<String> {
            m.files
                .keys()
                .filter(|p| p.ends_with(".rs"))
                .cloned()
                .collect()
        };
        if rust(&old.manifest) != rust(&new.manifest) {
            return Ok(full(Some(&base), "rust_file_set_changed"));
        }
        let mut changed = vec![];
        for path in rust(&new.manifest) {
            if old.manifest.files[&path].file_hash == new.manifest.files[&path].file_hash {
                continue;
            }
            match (
                old.syntax.skeletons.get(&path),
                new.syntax.skeletons.get(&path),
            ) {
                (Some(a), Some(b)) if a == b => changed.push(path),
                (Some(_), Some(_)) => {
                    return Ok(full(Some(&base), &format!("item_skeleton_changed: {path}")));
                }
                _ => return Ok(full(Some(&base), &format!("skeleton_unavailable: {path}"))),
            }
        }
        let changed_set: BTreeSet<&str> = changed.iter().map(String::as_str).collect();
        // Declarations in changed files may have moved; map them by a key that
        // an unchanged skeleton preserves, and only when it is unambiguous.
        let key = |s: &Symbol| (s.file.clone(), s.qualified_name.clone(), s.kind.clone());
        let unique = |symbols: &[Symbol]| {
            let mut map: BTreeMap<_, Option<Symbol>> = BTreeMap::new();
            for symbol in symbols
                .iter()
                .filter(|s| changed_set.contains(s.file.as_str()))
            {
                map.entry(key(symbol))
                    .and_modify(|e| *e = None)
                    .or_insert_with(|| Some(symbol.clone()));
            }
            map
        };
        let (old_moved, new_moved) = (unique(&old.syntax.symbols), unique(&new.syntax.symbols));
        let old_symbols: BTreeMap<_, _> = old
            .syntax
            .symbols
            .iter()
            .map(|s| (&s.symbol_id, s))
            .collect();
        let old_calls: BTreeMap<_, _> = old
            .syntax
            .calls
            .iter()
            .map(|c| (&c.callsite_id, c))
            .collect();
        let outcomes: BTreeMap<_, _> = base
            .resolutions
            .iter()
            .map(|r| (&r.callsite_id, r))
            .collect();
        let remap = |target: &Target| -> Option<Target> {
            if !changed_set.contains(target.file.as_str()) {
                return Some(target.clone());
            }
            let old_symbol = old_symbols.get(&target.symbol_id)?;
            old_moved.get(&key(old_symbol))?.as_ref()?;
            let symbol = new_moved.get(&key(old_symbol))?.as_ref()?;
            Some(Target {
                symbol_id: symbol.symbol_id.clone(),
                file: symbol.file.clone(),
                span: symbol.name_span,
                ..target.clone()
            })
        };
        let mut plan = RefreshPlan {
            scope: RefreshScope::BodyLocal {
                changed_files: changed.clone(),
            },
            base_syntax_epoch: Some(base.epochs.syntax_epoch),
            query: BTreeSet::new(),
            reused: vec![],
        };
        for call in &new.syntax.calls {
            let reused = (!changed_set.contains(call.file.as_str()))
                .then(|| old_calls.get(&call.callsite_id))
                .flatten()
                .filter(|old| old.caller_id == call.caller_id && old.kind == call.kind)
                .and_then(|_| outcomes.get(&call.callsite_id))
                .and_then(|outcome| {
                    let targets = outcome
                        .targets
                        .iter()
                        .map(remap)
                        .collect::<Option<Vec<_>>>()?;
                    Some(CallResolution {
                        targets,
                        ..(*outcome).clone()
                    })
                });
            match reused {
                Some(outcome) => plan.reused.push(outcome),
                None => {
                    plan.query.insert(call.callsite_id.clone());
                }
            }
        }
        Ok(plan)
    }

    pub fn begin_refresh(&mut self) -> Result<RefreshTicket, String> {
        let view = self.latest().ok_or("not_indexed")?;
        self.generation += 1;
        Ok(RefreshTicket {
            snapshot_id: view.snapshot.snapshot_id.clone(),
            syntax_epoch: view.epochs.syntax_epoch,
            input_hash: view.snapshot.input_hash.clone(),
            generation: self.generation,
        })
    }

    /// Invalidates in-flight publication, while preserving every published view.
    pub fn cancel_refresh(&mut self) {
        self.generation += 1;
    }

    pub fn complete(
        &mut self,
        ticket: &RefreshTicket,
        batch: SemanticBatch,
    ) -> Result<Arc<AnalysisView>, String> {
        let previous = self.latest().ok_or("not_indexed")?;
        if ticket.generation != self.generation
            || ticket.snapshot_id != previous.snapshot.snapshot_id
        {
            return Err("superseded".into());
        }
        if batch.snapshot_id != ticket.snapshot_id
            || batch.syntax_epoch != ticket.syntax_epoch
            || batch.input_hash != ticket.input_hash
        {
            return Err("content_mismatch".into());
        }
        if previous
            .snapshot
            .manifest
            .analysis_versions
            .get("semantic")
            .is_some_and(|expected| expected != &batch.backend)
        {
            return Err("content_mismatch".into());
        }
        let symbols: BTreeMap<_, _> = previous
            .snapshot
            .syntax
            .symbols
            .iter()
            .map(|s| (s.symbol_id.as_str(), s))
            .collect();
        let calls: BTreeMap<_, _> = previous
            .snapshot
            .syntax
            .calls
            .iter()
            .map(|c| (c.callsite_id.clone(), c))
            .collect();
        let mut resolutions = batch.resolutions.clone();
        resolutions.sort_by(|a, b| a.callsite_id.cmp(&b.callsite_id));
        let mut seen = BTreeSet::new();
        let mut relations = BTreeMap::new();
        let mut gaps = previous.snapshot.syntax.diagnostics.clone();
        gaps.extend(batch.diagnostics);
        for resolution in batch.resolutions {
            let call = calls
                .get(&resolution.callsite_id)
                .ok_or("unknown_callsite")?;
            if !seen.insert(resolution.callsite_id) {
                return Err("duplicate_callsite".into());
            }
            gaps.extend(resolution.diagnostics);
            if resolution.targets.is_empty() {
                gaps.push(Diagnostic {
                    code: "unresolved_call".into(),
                    file: Some(call.file.clone()),
                    message: call.expression.clone(),
                });
            }
            let Some(caller) = &call.caller_id else {
                gaps.push(Diagnostic {
                    code: "unsupported_call_owner".into(),
                    file: Some(call.file.clone()),
                    message: call.expression.clone(),
                });
                continue;
            };
            let multiple = resolution.targets.len() != 1;
            for target in resolution.targets {
                let symbol = symbols
                    .get(target.symbol_id.as_str())
                    .ok_or("unknown_target")?;
                if target.file != symbol.file || target.span != symbol.name_span {
                    return Err("content_mismatch".into());
                }
                if target.concrete_function && symbol.kind != "function" {
                    return Err("invalid_target_assertion".into());
                }
                let static_binding = match call.kind {
                    CallKind::Direct => matches!(
                        target.derivation,
                        TargetDerivation::Definition | TargetDerivation::ScipBinding
                    ),
                    CallKind::Method => {
                        target.derivation == TargetDerivation::ScipGoStaticBinding
                            && call.file.ends_with(".go")
                            && target.file.ends_with(".go")
                    }
                    CallKind::Indirect => false,
                };
                let certainty = if !multiple
                    && !resolution.cfg_inactive
                    && target.concrete_function
                    && static_binding
                {
                    Certainty::Definite
                } else {
                    Certainty::Candidate
                };
                let relation_id = identity(
                    "rel",
                    &[
                        "call",
                        caller,
                        &target.symbol_id,
                        &call.file,
                        &serde_json::to_string(&call.span).unwrap(),
                    ],
                );
                let evidence = Evidence {
                    evidence_id: identity(
                        "ev",
                        &[&relation_id, &batch.backend, &ticket.input_hash],
                    ),
                    source: "L3".into(),
                    certainty: certainty.clone(),
                    indexer: batch.backend.as_str().into(),
                    derivation: target.derivation.evidence_label().into(),
                    file_hash: call.file_hash.as_str().into(),
                };
                relations.insert(
                    relation_id.clone(),
                    Relation {
                        relation_id,
                        kind: "call".into(),
                        from_symbol: caller.clone(),
                        to_symbol: target.symbol_id,
                        file: call.file.clone(),
                        span: call.span,
                        certainty,
                        freshness: "current".into(),
                        runtime: "unavailable".into(),
                        evidence: vec![evidence],
                        origin: None,
                    },
                );
            }
        }
        if seen.len() != calls.len() {
            return Err("incomplete_refresh".into());
        }
        gaps.sort();
        gaps.dedup();
        let view = Arc::new(AnalysisView {
            snapshot: previous.snapshot.clone(),
            epochs: Epochs {
                syntax_epoch: ticket.syntax_epoch,
                semantic_epoch: ticket.syntax_epoch,
            },
            relations: relations.into_values().collect(),
            pending_units: vec![],
            known_gaps: gaps,
            resolutions,
        });
        if let Some(existing) = self.view(&ticket.snapshot_id, ticket.syntax_epoch) {
            if existing != view {
                return Err("inconsistent_semantic_rebuild".into());
            }
            return Ok(existing);
        }
        self.completed_epoch = ticket.syntax_epoch;
        self.views.push(view.clone());
        self.current = self.views.len().checked_sub(1);
        Ok(view)
    }
}

fn validate_syntax(manifest: &Manifest, syntax: &SyntaxFacts) -> Result<(), String> {
    let mut symbols = BTreeMap::new();
    let mut lines = BTreeMap::new();
    for symbol in &syntax.symbols {
        if symbols.insert(&symbol.symbol_id, symbol).is_some() {
            return Err("duplicate_symbol".into());
        }
        validate_span(
            manifest,
            &mut lines,
            &symbol.file,
            &symbol.file_hash,
            symbol.span,
        )?;
        validate_span(
            manifest,
            &mut lines,
            &symbol.file,
            &symbol.file_hash,
            symbol.name_span,
        )?;
    }
    let mut calls = BTreeSet::new();
    for call in &syntax.calls {
        if !calls.insert(&call.callsite_id) {
            return Err("duplicate_callsite".into());
        }
        if call
            .caller_id
            .as_ref()
            .is_some_and(|id| !symbols.contains_key(id))
        {
            return Err("unknown_caller".into());
        }
        validate_span(manifest, &mut lines, &call.file, &call.file_hash, call.span)?;
    }
    let mut impls = BTreeSet::new();
    let mut methods = BTreeSet::new();
    for block in &syntax.impls {
        if !impls.insert(&block.impl_id) {
            return Err("duplicate_impl".into());
        }
        validate_span(
            manifest,
            &mut lines,
            &block.file,
            &block.file_hash,
            block.span,
        )?;
        validate_span(
            manifest,
            &mut lines,
            &block.file,
            &block.file_hash,
            block.trait_span,
        )?;
        if block.trait_span.start < block.span.start || block.trait_span.end > block.span.end {
            return Err("invalid_span".into());
        }
        for method in &block.methods {
            if !methods.insert(method) {
                return Err("duplicate_impl_method".into());
            }
            let symbol = symbols.get(method).ok_or("unknown_impl_method")?;
            if symbol.kind != "function"
                || symbol.file != block.file
                || symbol.file_hash != block.file_hash
                || symbol.span.start < block.span.start
                || symbol.span.end > block.span.end
                || symbol.name_span.start < symbol.span.start
                || symbol.name_span.end > symbol.span.end
            {
                return Err("invalid_impl_method".into());
            }
        }
    }
    Ok(())
}

fn validate_span<'a>(
    manifest: &'a Manifest,
    lines: &mut BTreeMap<&'a str, LineIndex<'a>>,
    path: &str,
    file_hash: &str,
    span: Span,
) -> Result<(), String> {
    let (path, file) = manifest.files.get_key_value(path).ok_or("file_not_found")?;
    if file.file_hash != file_hash {
        return Err("content_mismatch".into());
    }
    if span.start > span.end {
        return Err("invalid_span".into());
    }
    let lines = lines
        .entry(path.as_str())
        .or_insert_with(|| LineIndex::new(&file.content));
    lines.byte_offset(span.start)?;
    lines.byte_offset(span.end)?;
    Ok(())
}
