use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Location {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub file_hash: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Symbol {
    pub symbol_id: String,
    pub qualified_name: String,
    pub signature: String,
    pub docs: Option<String>,
    pub kind: String,
    pub scope: String,
    pub visibility: String,
    pub location: Location,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SourceSpan {
    pub start: Location,
    pub end: Location,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Relation {
    pub from: String,
    pub to: String,
    pub relation_id: String,
    pub from_name: String,
    pub to_name: String,
    pub kind: String,
    pub scope: String,
    pub certainty: String,
    pub freshness: String,
    pub side: String,
    pub depth: usize,
    pub location: Location,
    pub evidence: Vec<Value>,
    pub path: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ImpactRow {
    pub key: String,
    pub sides: Vec<Relation>,
}

impl ImpactRow {
    pub fn preferred(&self) -> &Relation {
        self.on_side("head").unwrap_or(&self.sides[0])
    }

    pub fn on_side(&self, side: &str) -> Option<&Relation> {
        self.sides
            .iter()
            .find(|r| r.side == side || (side == "head" && r.side.is_empty()))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Change {
    pub kind: String,
    pub base_symbol: Option<Symbol>,
    pub head_symbol: Option<Symbol>,
}

impl Change {
    pub fn symbol(&self) -> Option<&Symbol> {
        self.head_symbol.as_ref().or(self.base_symbol.as_ref())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Impact {
    pub relations: Vec<Relation>,
    pub changes: Vec<Change>,
    pub snapshots: Value,
    pub completeness: Value,
    pub output: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Symbols {
    pub symbols: Vec<Symbol>,
    pub snapshot_id: String,
    pub epochs: Value,
    pub output: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Module {
    pub path: String,
    pub symbol_count: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Structure {
    pub modules: Vec<Module>,
    pub dependencies: Vec<Value>,
    pub output: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Preview {
    pub name: String,
    pub example: bool,
    pub workspace: PathBuf,
    pub generated_at: String,
    pub review_key: String,
    pub branch: Option<String>,
    pub symbols: Symbols,
    pub map: Structure,
    pub module_map: Structure,
    pub churn: BTreeMap<String, Churn>,
    pub churn_scope: String,
    pub git_error: Option<String>,
    pub impact: Impact,
    pub impact_error: Option<String>,
    pub sources: BTreeMap<String, String>,
    pub base_sources: BTreeMap<String, String>,
    #[serde(skip)]
    pub raw: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Churn {
    pub added: usize,
    pub deleted: usize,
    pub touches: usize,
}

pub struct SearchHit {
    pub file: String,
    pub line: usize,
    pub text: String,
}
pub struct FileMetric {
    pub file: String,
    pub lines: usize,
    pub nonblank: usize,
    pub functions: usize,
    pub symbols: usize,
    pub churn: usize,
    pub touches: usize,
}

impl Preview {
    pub fn source_hash(&self, file: &str) -> Result<String> {
        let source = self.sources.get(file).context("当前快照没有捕获对应源码")?;
        Ok(semweft_shared::digest(source))
    }

    pub fn verify_location(&self, location: &Location) -> Result<()> {
        ensure!(!location.file_hash.is_empty(), "定义缺少文件内容哈希");
        ensure!(
            self.source_hash(&location.file)? == location.file_hash,
            "定义与当前捕获源码内容不同，请刷新后重查"
        );
        let source = &self.sources[&location.file];
        let position = semweft_shared::Position {
            line: u32::try_from(location.line).context("定义行号溢出")?,
            column: u32::try_from(location.column).context("定义列号溢出")?,
        };
        semweft_shared::byte_offset(source, position)
            .map_err(|error| anyhow::anyhow!("定义坐标不是有效的 UTF-8 字节位置：{error}"))?;
        Ok(())
    }

    pub fn text_search(&self, query: &str) -> (Vec<SearchHit>, usize) {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return (vec![], 0);
        }
        let mut hits = vec![];
        let mut total = 0;
        for (file, source) in &self.sources {
            for (line, text) in source.lines().enumerate() {
                if text.to_lowercase().contains(&query) {
                    total += 1;
                    if hits.len() < 500 {
                        hits.push(SearchHit {
                            file: file.clone(),
                            line: line + 1,
                            text: text.trim().to_owned(),
                        });
                    }
                }
            }
        }
        (hits, total)
    }
    pub fn file_metrics(&self) -> Vec<FileMetric> {
        let mut rows = self
            .sources
            .iter()
            .map(|(file, source)| {
                let symbols = self
                    .symbols
                    .symbols
                    .iter()
                    .filter(|s| &s.location.file == file)
                    .collect::<Vec<_>>();
                let churn = self.churn.get(file).cloned().unwrap_or_default();
                FileMetric {
                    file: file.clone(),
                    lines: source.lines().count(),
                    nonblank: source.lines().filter(|s| !s.trim().is_empty()).count(),
                    functions: symbols
                        .iter()
                        .filter(|s| matches!(s.kind.as_str(), "function" | "trait_method"))
                        .count(),
                    symbols: symbols.len(),
                    churn: churn.added + churn.deleted,
                    touches: churn.touches,
                }
            })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            b.churn
                .cmp(&a.churn)
                .then(b.lines.cmp(&a.lines))
                .then(a.file.cmp(&b.file))
        });
        rows
    }
    pub fn dependency_edges(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut edges = BTreeMap::<String, BTreeSet<String>>::new();
        for dep in &self.module_map.dependencies {
            if let (Some(from), Some(to)) = (dep["from"].as_str(), dep["to"].as_str()) {
                edges.entry(from.into()).or_default().insert(to.into());
                edges.entry(to.into()).or_default();
            }
        }
        edges
    }
    pub fn dependency_cycles(&self) -> Vec<Vec<String>> {
        // Iterative Kosaraju avoids a stack overflow on deeply nested projects.
        let graph = self.dependency_edges();
        let mut reversed = BTreeMap::<String, BTreeSet<String>>::new();
        for (from, targets) in &graph {
            for to in targets {
                reversed.entry(to.clone()).or_default().insert(from.clone());
            }
        }
        let mut visited = BTreeSet::new();
        let mut order = vec![];
        for root in graph.keys() {
            let mut stack = vec![(root.clone(), false)];
            while let Some((node, done)) = stack.pop() {
                if done {
                    order.push(node);
                    continue;
                }
                if !visited.insert(node.clone()) {
                    continue;
                }
                stack.push((node.clone(), true));
                for next in graph.get(&node).into_iter().flatten() {
                    if !visited.contains(next) {
                        stack.push((next.clone(), false));
                    }
                }
            }
        }
        visited.clear();
        let mut cycles = vec![];
        for root in order.into_iter().rev() {
            let mut component = vec![];
            let mut stack = vec![root];
            while let Some(node) = stack.pop() {
                if !visited.insert(node.clone()) {
                    continue;
                }
                component.push(node.clone());
                stack.extend(reversed.get(&node).into_iter().flatten().cloned());
            }
            if component.len() > 1
                || component
                    .first()
                    .is_some_and(|n| graph.get(n).is_some_and(|e| e.contains(n)))
            {
                component.sort();
                cycles.push(component);
            }
        }
        cycles.sort();
        cycles
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Bookmark {
    pub file: String,
    pub line: usize,
    pub label: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub workspace: PathBuf,
    pub bookmarks: Vec<Bookmark>,
}
impl Session {
    pub fn path(data: &Path, workspace: &Path) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        workspace.hash(&mut hasher);
        let key = format!("{:016x}", hasher.finish());
        data.with_file_name("sessions")
            .join(key)
            .join("session.json")
    }
    pub fn load(data: &Path, workspace: &Path) -> Self {
        fs::read(Self::path(data, workspace))
            .ok()
            .and_then(|v| serde_json::from_slice::<Self>(&v).ok())
            .filter(|s| s.workspace == workspace)
            .unwrap_or_else(|| Self {
                workspace: workspace.into(),
                ..Self::default()
            })
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        let path = Self::path(data, &self.workspace);
        fs::create_dir_all(path.parent().unwrap())?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

impl Preview {
    pub fn load(path: &Path) -> Result<Self> {
        let raw: Value =
            serde_json::from_slice(&fs::read(path).context("Cannot read preview JSON")?)?;
        let mut data: Self = serde_json::from_value(raw.clone())?;
        ensure!(
            !data.name.is_empty() && !data.review_key.is_empty(),
            "Invalid preview payload"
        );
        ensure!(
            data.review_key.len() == 64 && data.review_key.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid snapshot review key"
        );
        data.raw = raw;
        Ok(data)
    }

    pub fn rows(&self) -> Vec<ImpactRow> {
        let mut groups: BTreeMap<String, Vec<Relation>> = BTreeMap::new();
        for r in &self.impact.relations {
            // Group by symbol pair; retain every occurrence and each side's
            // coordinates rather than transplanting coordinates across sides.
            let key = serde_json::to_string(&(&r.from_name, &r.to_name, &r.location.file, &r.kind))
                .unwrap();
            groups.entry(key).or_default().push(r.clone());
        }
        let mut rows: Vec<_> = groups
            .into_iter()
            .map(|(key, sides)| ImpactRow { key, sides })
            .collect();
        rows.sort_by_key(|r| {
            let r = r.preferred();
            (
                r.scope == "test",
                r.depth,
                r.location.file.clone(),
                r.location.line,
            )
        });
        rows
    }

    pub fn gaps(&self) -> Vec<&Value> {
        self.impact.completeness["known_gaps"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    pub fn affected_files(&self) -> usize {
        self.impact
            .relations
            .iter()
            .map(|r| &r.location.file)
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub fn snippet(&self, file: &str, line: usize, side: &str) -> Vec<(usize, String)> {
        let sources = if side == "base" {
            &self.base_sources
        } else {
            &self.sources
        };
        sources
            .get(file)
            .map(|s| {
                s.lines()
                    .enumerate()
                    .skip(line.saturating_sub(4))
                    .take(13)
                    .map(|(i, s)| (i + 1, s.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn route(&self, relation: &Relation) -> String {
        let mut names = vec![relation.from_name.clone()];
        for id in &relation.path {
            if let Some(edge) = self
                .impact
                .relations
                .iter()
                .find(|r| &r.relation_id == id && r.side == relation.side)
                && names.last() != Some(&edge.to_name)
            {
                names.push(edge.to_name.clone());
            }
        }
        if names.len() == 1 {
            names.push(relation.to_name.clone());
        }
        names.join("  →  ")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalMarks {
    pub review_key: String,
    pub decisions: BTreeMap<String, String>,
}

impl LocalMarks {
    pub fn path(data_path: &Path, key: &str) -> PathBuf {
        data_path
            .with_file_name("local-marks")
            .join(format!("{key}.json"))
    }

    pub fn load(path: &Path, key: &str) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|marks| marks.review_key == key)
            .unwrap_or_else(|| Self {
                review_key: key.to_owned(),
                ..Self::default()
            })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_keeps_side_specific_coordinates_and_certainty() {
        let data: Preview = serde_json::from_value(serde_json::json!({"impact":{"relations":[
            {"from_name":"a", "to_name":"b", "side":"base", "certainty":"candidate", "location":{"file":"a.rs", "line":4}},
            {"from_name":"a", "to_name":"b", "side":"head", "certainty":"definite", "location":{"file":"a.rs", "line":9}}
        ]}})).unwrap();
        let rows = data.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].on_side("base").unwrap().location.line, 4);
        assert_eq!(rows[0].preferred().location.line, 9);
        assert_eq!(rows[0].on_side("base").unwrap().certainty, "candidate");
    }

    #[test]
    fn missing_side_does_not_reuse_other_side_location() {
        let row = ImpactRow {
            key: "x".into(),
            sides: vec![Relation {
                side: "base".into(),
                ..Relation::default()
            }],
        };
        assert!(row.on_side("head").is_none());
    }

    #[test]
    fn snippets_are_captured_and_keep_original_line_numbers() {
        let data = Preview {
            sources: BTreeMap::from([("a.rs".into(), "one\ntwo\nthree\nfour\nfive\nsix".into())]),
            ..Preview::default()
        };
        assert_eq!(data.snippet("a.rs", 5, "head")[0], (2, "two".into()));
        assert!(data.snippet("a.rs", 5, "base").is_empty());
    }

    #[test]
    fn diff_symbol_accepts_null_documentation_from_cli() {
        let change: Change = serde_json::from_value(serde_json::json!({
            "kind": "modified",
            "head_symbol": { "qualified_name": "inventory::reserve", "docs": null }
        }))
        .unwrap();
        assert_eq!(
            change.symbol().unwrap().qualified_name,
            "inventory::reserve"
        );
        assert!(change.symbol().unwrap().docs.is_none());
    }
}

#[cfg(test)]
mod workbench_tests {
    use super::*;
    #[test]
    fn definition_locations_require_matching_contents_and_utf8_boundaries() {
        let mut data = Preview {
            sources: BTreeMap::from([(
                "src/lib.rs".into(),
                "// 中文😀\r\nfn reserve() {}\r\n".into(),
            )]),
            ..Preview::default()
        };
        let mut location = Location {
            file: "src/lib.rs".into(),
            line: 2,
            column: 4,
            file_hash: data.source_hash("src/lib.rs").unwrap(),
        };
        assert!(data.verify_location(&location).is_ok());
        location.line = 1;
        location.column = 5; // Inside the UTF-8 bytes of 中.
        assert!(data.verify_location(&location).is_err());
        location.line = 2;
        location.column = 4;
        data.sources
            .get_mut("src/lib.rs")
            .unwrap()
            .push_str("// edited\n");
        assert!(data.verify_location(&location).is_err());
        location.file = "outside.rs".into();
        assert!(data.verify_location(&location).is_err());
    }

    #[test]
    fn definition_coordinates_match_engine_rules_for_crlf_and_final_empty_lines() {
        let data = Preview {
            sources: BTreeMap::from([("src/lib.rs".into(), "a\r\n".into())]),
            ..Preview::default()
        };
        let mut location = Location {
            file: "src/lib.rs".into(),
            line: 1,
            column: 3,
            file_hash: data.source_hash("src/lib.rs").unwrap(),
        };
        assert!(data.verify_location(&location).is_ok());
        location.line = 2;
        location.column = 1;
        assert!(data.verify_location(&location).is_ok());
        location.column = 2;
        assert!(data.verify_location(&location).is_err());
        location.line = 0;
        assert!(data.verify_location(&location).is_err());
        location.line = usize::MAX;
        assert!(data.verify_location(&location).is_err());
    }
    #[test]
    fn dependency_components_do_not_report_one_way_links_as_cycles() {
        let mut data = Preview::default();
        data.module_map.dependencies = vec![
            serde_json::json!({"from":"a.rs","to":"b.rs"}),
            serde_json::json!({"from":"b.rs","to":"a.rs"}),
            serde_json::json!({"from":"b.rs","to":"c.rs"}),
            serde_json::json!({"from":"self.rs","to":"self.rs"}),
        ];
        assert_eq!(
            data.dependency_cycles(),
            vec![
                vec!["a.rs".to_owned(), "b.rs".to_owned()],
                vec!["self.rs".to_owned()]
            ]
        );
    }
    #[test]
    fn text_search_preserves_unicode_and_file_line_coordinates() {
        let data = Preview {
            sources: BTreeMap::from([(
                "src/a.ts".into(),
                "// 中文😀\nexport function Reserve() {}\n".into(),
            )]),
            ..Preview::default()
        };
        let (hits, total) = data.text_search("reserve");
        assert_eq!(total, 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].file, "src/a.ts");
        assert_eq!(data.text_search("中文😀").0[0].line, 1);
        assert_eq!(data.file_metrics()[0].lines, 2);
        assert_eq!(data.text_search("").1, 0);
    }
    #[test]
    fn search_display_limit_keeps_the_total_visible() {
        let data = Preview {
            sources: BTreeMap::from([("big.go".into(), "// hit\n".repeat(700))]),
            ..Preview::default()
        };
        let (hits, total) = data.text_search("hit");
        assert_eq!(hits.len(), 500);
        assert_eq!(total, 700);
    }
}
