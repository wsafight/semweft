//! Code evidence, immutable input identities and snapshot publication rules.
mod refresh;

pub use refresh::*;
// Keep existing adapters and persisted data compatible with the shared types.
pub use semweft_shared::{
    LineIndex, Position, Span, byte_offset, digest, digest_json, from_utf16, identity, to_utf16,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub file_hash: String,
    pub content: String,
}

impl SourceFile {
    pub fn new(content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            file_hash: digest(content.as_bytes()),
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub workspace_id: String,
    pub files: BTreeMap<String, SourceFile>,
    pub binary_files: BTreeMap<String, String>,
    pub analysis_versions: BTreeMap<String, String>,
    pub scope: String,
}

impl Manifest {
    pub fn input_hash(&self) -> String {
        digest(serde_json::to_vec(self).unwrap())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub symbol_id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub file: String,
    pub file_hash: String,
    pub span: Span,
    pub name_span: Span,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Direct,
    Method,
    Indirect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub callsite_id: String,
    pub caller_id: Option<String>,
    pub file: String,
    pub file_hash: String,
    pub span: Span,
    pub expression: String,
    pub kind: CallKind,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub file: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxFacts {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallSite>,
    pub diagnostics: Vec<Diagnostic>,
    /// Per-file hash of the item structure with function bodies elided.
    /// Equal skeletons mean an edit stayed inside function bodies.
    pub skeletons: BTreeMap<String, String>,
    #[serde(default)]
    pub occurrences: Vec<Occurrence>,
    #[serde(default)]
    pub declarations: BTreeMap<String, DeclarationInfo>,
    #[serde(default)]
    pub imports: BTreeMap<String, Vec<String>>,
    /// Trait implementation blocks (`impl Trait for Type`); inherent impls are
    /// not recorded. Sorted by file, position and identity.
    #[serde(default)]
    pub impls: Vec<ImplBlock>,
}

impl SyntaxFacts {
    /// Merge independently extracted file facts; callers preserve their public sorting rules.
    pub fn extend(&mut self, other: &Self) {
        self.symbols.extend_from_slice(&other.symbols);
        self.calls.extend_from_slice(&other.calls);
        self.occurrences.extend_from_slice(&other.occurrences);
        self.diagnostics.extend_from_slice(&other.diagnostics);
        self.skeletons.extend(other.skeletons.clone());
        self.declarations.extend(other.declarations.clone());
        self.imports.extend(other.imports.clone());
        self.impls.extend_from_slice(&other.impls);
    }
}

/// A syntax-level `impl Trait for Type` block. The trait path is the written
/// text; binding it to a declaration is a query-time name match (L1) unless a
/// semantic backend supplies a reference at `trait_span`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImplBlock {
    pub impl_id: String,
    pub file: String,
    pub file_hash: String,
    pub span: Span,
    /// Written trait path without generic arguments, e.g. `inventory::Deduct`.
    pub trait_path: String,
    /// Final identifier of the trait path, the occurrence bound by L3 backends.
    pub trait_span: Span,
    pub self_type: String,
    /// Type parameter names declared on the impl, excluding lifetimes and consts.
    pub generics: Vec<String>,
    pub negative: bool,
    pub unsafe_impl: bool,
    pub test_only: bool,
    /// Symbols of methods defined directly in the impl body.
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationInfo {
    pub docs: String,
    pub visibility: String,
    pub test_only: bool,
    #[serde(default)]
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub name: String,
    pub file: String,
    pub span: Span,
    pub kind: String,
    pub enclosing_symbol_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub symbol_id: String,
    pub file: String,
    pub span: Span,
    /// True only for a syntax-aligned function definition. A semantic name
    /// binding to a variable or trait method does not prove a runtime target.
    pub concrete_function: bool,
    pub derivation: TargetDerivation,
}

/// How the semantic backend reached a target. Implementation enumeration
/// lists possible dispatch targets and never proves a single runtime target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetDerivation {
    Definition,
    Implementation,
    ScipBinding,
    ScipGoStaticBinding,
}

impl TargetDerivation {
    pub fn evidence_label(self) -> &'static str {
        match self {
            Self::Definition => "lsp_definition+syntax_callsite",
            Self::Implementation => "lsp_implementation+syntax_callsite",
            Self::ScipBinding => "scip_occurrence+syntax_callsite",
            Self::ScipGoStaticBinding => "scip_occurrence+go_static_dispatch+syntax_callsite",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    Definite,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub evidence_id: String,
    pub source: String,
    pub certainty: Certainty,
    // Shared: these repeat across most evidence in a view, so each distinct
    // value is allocated once and cloned by reference count.
    pub indexer: std::sync::Arc<str>,
    pub derivation: std::sync::Arc<str>,
    pub file_hash: std::sync::Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub relation_id: String,
    pub kind: String,
    pub from_symbol: String,
    pub to_symbol: String,
    pub file: String,
    pub span: Span,
    pub certainty: Certainty,
    pub freshness: String,
    pub runtime: String,
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RelationOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationOrigin {
    pub snapshot_id: String,
    pub epochs: crate::Epochs,
    pub file_hash: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_files_hash_their_content_and_manifests_hash_every_input() {
        let file = SourceFile::new("fn a() {}");
        assert_eq!(file.file_hash, digest("fn a() {}"));
        let manifest = Manifest {
            workspace_id: "ws".into(),
            files: BTreeMap::from([("src/lib.rs".into(), file.clone())]),
            binary_files: BTreeMap::new(),
            analysis_versions: BTreeMap::from([("semantic".into(), "test@1".into())]),
            scope: "test".into(),
        };
        let hash = manifest.input_hash();
        assert_eq!(hash, manifest.input_hash());
        let mut binary = manifest.clone();
        binary.binary_files.insert("asset".into(), digest([0]));
        assert_ne!(hash, binary.input_hash());
        let mut version = manifest.clone();
        version
            .analysis_versions
            .insert("engine".into(), "2".into());
        assert_ne!(hash, version.input_hash());
    }

    #[test]
    fn target_derivation_evidence_labels() {
        assert_eq!(
            TargetDerivation::Definition.evidence_label(),
            "lsp_definition+syntax_callsite"
        );
        assert_eq!(
            TargetDerivation::Implementation.evidence_label(),
            "lsp_implementation+syntax_callsite"
        );
    }
}
