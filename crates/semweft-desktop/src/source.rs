use anyhow::{Context, Result};
use std::{ops::Range, path::Path, sync::OnceLock};
use tree_sitter::{Language, Parser};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const CAPTURES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constructor",
    "embedded",
    "function",
    "keyword",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "type",
    "variable",
];

#[derive(Clone, Debug)]
pub struct Identifier {
    pub display_range: Range<usize>,
    // One-based UTF-8 byte column in the captured source, before tab expansion.
    pub column: usize,
    pub label: String,
}

#[derive(Default)]
pub struct CodeLine {
    pub text: String,
    pub highlights: Vec<(Range<usize>, &'static str)>,
    pub identifiers: Vec<Identifier>,
}

#[derive(Default)]
pub struct SourceDocument {
    pub lines: Vec<CodeLine>,
    pub language: &'static str,
    pub notice: Option<String>,
}

#[derive(Clone, Copy)]
enum Syntax {
    Rust,
    Go,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Syntax {
    fn for_path(path: &str) -> Option<Self> {
        Some(match Path::new(path).extension()?.to_str()? {
            "rs" => Self::Rust,
            "go" => Self::Go,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::JavaScript => "JavaScript / JSX",
        }
    }

    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    fn configuration(self) -> Result<&'static HighlightConfiguration> {
        static RUST: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
        static GO: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
        static TS: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
        static TSX: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
        static JS: OnceLock<Result<HighlightConfiguration, String>> = OnceLock::new();
        let cache = match self {
            Self::Rust => &RUST,
            Self::Go => &GO,
            Self::TypeScript => &TS,
            Self::Tsx => &TSX,
            Self::JavaScript => &JS,
        };
        cache
            .get_or_init(|| {
                let query = match self {
                    Self::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY.to_owned(),
                    Self::Go => tree_sitter_go::HIGHLIGHTS_QUERY.to_owned(),
                    Self::TypeScript => format!(
                        "{}\n{}",
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                        tree_sitter_typescript::HIGHLIGHTS_QUERY
                    ),
                    Self::Tsx => format!(
                        "{}\n{}\n{}",
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                        tree_sitter_typescript::HIGHLIGHTS_QUERY
                    ),
                    Self::JavaScript => format!(
                        "{}\n{}",
                        tree_sitter_javascript::HIGHLIGHT_QUERY,
                        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
                    ),
                };
                HighlightConfiguration::new(self.language(), self.name(), &query, "", "")
                    .map(|mut config| {
                        config.configure(CAPTURES);
                        config
                    })
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }
}

impl SourceDocument {
    pub fn new(path: &str, source: &str) -> Self {
        let Some(syntax) = Syntax::for_path(path) else {
            return Self::plain(source, "纯文本", None);
        };
        if source.len() > 2 * 1024 * 1024 {
            return Self::plain(
                source,
                syntax.name(),
                Some("文件超过 2 MiB，保留纯文本虚拟浏览；语法高亮和标识符导航未启用。".into()),
            );
        }
        let mut maps = Vec::new();
        let mut starts = Vec::new();
        let mut offset = 0;
        let lines = source
            .lines()
            .map(|original| {
                starts.push(offset);
                // Preserve CRLF byte offsets while rendering without line terminators.
                offset += original.len();
                if source.as_bytes().get(offset) == Some(&b'\r') {
                    offset += 1;
                }
                if source.as_bytes().get(offset) == Some(&b'\n') {
                    offset += 1;
                }
                let mut text = String::new();
                let mut map = vec![0; original.len() + 1];
                for (index, character) in original.char_indices() {
                    map[index] = text.len();
                    if character == '\t' {
                        text.push_str("    ");
                    } else {
                        text.push(character);
                    }
                    map[index + character.len_utf8()] = text.len();
                }
                maps.push(map);
                CodeLine {
                    text,
                    ..Default::default()
                }
            })
            .collect();
        let mut document = Self {
            lines,
            language: "纯文本",
            notice: None,
        };
        document.language = syntax.name();
        if let Err(error) = document.analyze(syntax, source, &starts, &maps) {
            document.notice = Some(format!("语法着色失败：{error:#}"));
        }
        document
    }

    fn plain(source: &str, language: &'static str, notice: Option<String>) -> Self {
        Self {
            lines: source
                .lines()
                .map(|line| CodeLine {
                    text: line.replace('\t', "    "),
                    ..Default::default()
                })
                .collect(),
            language,
            notice,
        }
    }

    fn analyze(
        &mut self,
        syntax: Syntax,
        source: &str,
        starts: &[usize],
        maps: &[Vec<usize>],
    ) -> Result<()> {
        let mut parser = Parser::new();
        parser.set_language(&syntax.language())?;
        let tree = parser.parse(source, None).context("无法解析源码")?;
        let mut cursor = tree.walk();
        loop {
            let node = cursor.node();
            if node.child_count() == 0 && node.kind().contains("identifier") {
                let point = node.start_position();
                if let (Some(line), Some(map)) =
                    (self.lines.get_mut(point.row), maps.get(point.row))
                {
                    let end = point.column + node.byte_range().len();
                    if let (Some(&start), Some(&end_display)) =
                        (map.get(point.column), map.get(end))
                    {
                        line.identifiers.push(Identifier {
                            display_range: start..end_display,
                            column: point.column + 1,
                            label: source[node.byte_range()].to_owned(),
                        });
                    }
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return self.highlight(syntax, source, starts, maps);
                }
            }
        }
    }

    fn highlight(
        &mut self,
        syntax: Syntax,
        source: &str,
        starts: &[usize],
        maps: &[Vec<usize>],
    ) -> Result<()> {
        let mut highlighter = Highlighter::new();
        let events =
            highlighter.highlight(syntax.configuration()?, source.as_bytes(), None, |_| None)?;
        let mut stack = Vec::new();
        for event in events {
            match event? {
                HighlightEvent::HighlightStart(style) => stack.push(CAPTURES[style.0]),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(&style) = stack.last() else {
                        continue;
                    };
                    let mut row = starts
                        .partition_point(|offset| *offset <= start)
                        .saturating_sub(1);
                    while let Some(&line_start) = starts.get(row).filter(|&&offset| offset < end) {
                        let map = &maps[row];
                        let from = start.saturating_sub(line_start).min(map.len() - 1);
                        let to = end.saturating_sub(line_start).min(map.len() - 1);
                        if from < to {
                            self.lines[row].highlights.push((map[from]..map[to], style));
                        }
                        row += 1;
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn parse_line_number(text: &str, total: usize) -> Result<usize> {
    let line: usize = text.trim().parse().context("请输入正整数行号")?;
    anyhow::ensure!(line > 0 && line <= total, "行号范围为 1–{total}");
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_queries_work_for_all_supported_grammars() {
        for (path, source, keyword) in [
            ("lib.rs", "pub fn reserve() -> u32 { 7 }", "fn"),
            (
                "main.go",
                "package main\nfunc reserve() int { return 7 }",
                "func",
            ),
            (
                "lib.ts",
                "export function reserve(): number { return 7; }",
                "function",
            ),
            (
                "lib.mts",
                "export function reserve(): number { return 7; }",
                "function",
            ),
            (
                "lib.cts",
                "export function reserve(): number { return 7; }",
                "function",
            ),
            (
                "view.tsx",
                "export function reserve() { return <p>库存</p>; }",
                "function",
            ),
            (
                "lib.js",
                "export function reserve() { return 7; }",
                "function",
            ),
            (
                "view.jsx",
                "export function reserve() { return <p>库存</p>; }",
                "function",
            ),
        ] {
            let document = SourceDocument::new(path, source);
            assert!(document.notice.is_none(), "{path}: {:?}", document.notice);
            assert!(
                document
                    .lines
                    .iter()
                    .any(|line| line.identifiers.iter().any(|id| id.label == "reserve")),
                "missing identifier in {path}"
            );
            assert!(
                document.lines.iter().any(|line| line
                    .highlights
                    .iter()
                    .any(|(range, style)| *style == "keyword"
                        && &line.text[range.clone()] == keyword)),
                "missing keyword highlight in {path}"
            );
            for line in &document.lines {
                let mut end = 0;
                for (range, _) in &line.highlights {
                    assert!(range.start >= end && range.end <= line.text.len());
                    assert!(
                        line.text.is_char_boundary(range.start)
                            && line.text.is_char_boundary(range.end)
                    );
                    end = range.end;
                }
            }
        }
    }

    #[test]
    fn crlf_tabs_unicode_and_multiline_comments_keep_original_click_columns() {
        let source = "fn caller() {\r\n\t/* 中文😀\r\n\tstill comment */ let 中文 = \"😀\"; reserve();\r\n}\r\n";
        let document = SourceDocument::new("lib.rs", source);
        assert!(document.notice.is_none(), "{:?}", document.notice);
        assert_eq!(document.lines.len(), 4);
        let line = &document.lines[2];
        let identifier = line
            .identifiers
            .iter()
            .find(|id| id.label == "reserve")
            .unwrap();
        assert_eq!(&line.text[identifier.display_range.clone()], "reserve");
        assert_eq!(
            identifier.column,
            source.lines().nth(2).unwrap().find("reserve").unwrap() + 1
        );
        assert!(line.text.starts_with("    still comment */"));
        assert!(
            line.highlights
                .iter()
                .any(|(range, style)| *style == "comment"
                    && line.text[range.clone()].contains("still comment"))
        );
        assert!(
            !document.lines[1]
                .identifiers
                .iter()
                .any(|id| id.label == "中文")
        );
        assert!(document.lines.iter().all(|line| !line.text.contains('\r')));
    }

    #[test]
    fn line_navigation_rejects_missing_negative_zero_and_out_of_range_input() {
        for text in ["", "0", "-1", "1:2", "10", "184467440737095516160"] {
            assert!(parse_line_number(text, 9).is_err(), "accepted {text}");
        }
        assert_eq!(parse_line_number(" 9 ", 9).unwrap(), 9);
        assert!(parse_line_number("1", 0).is_err());
        let document = SourceDocument::new("notes.txt", "中文\t😀\r\n");
        assert_eq!(document.lines[0].text, "中文    😀");
        assert!(document.lines[0].identifiers.is_empty());
    }
}
