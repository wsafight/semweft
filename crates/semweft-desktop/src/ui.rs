use crate::{
    client,
    input::TextInput,
    model::{Bookmark, LocalMarks, Preview, SearchHit, Session, Symbol},
    source::SourceDocument,
};
use anyhow::{Context as _, Result, bail, ensure};
use gpui::{
    App, Application, Bounds, Context, Div, Entity, FocusHandle, Focusable, FontWeight, KeyBinding,
    Menu, MenuItem, PathPromptOptions, ScrollStrategy, SharedString, Subscription, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px,
    rgb, size, uniform_list,
};
use gpui_rsx::{rsx, rsx_fragment};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

mod components;
mod file_tree;
mod icons;
#[cfg(test)]
mod rsx_compat_tests;
mod shell;
use components::{
    ActionButton, Badge, CodeText, DetailField, EmptyState, FileBadge, Icon, IconButton,
    PanelHeader, Section, StatCard, SymbolBadge,
};
use file_tree::FileTree;

const INK: u32 = 0x283345;
const MUTED: u32 = 0x8792a2;
const LINE: u32 = 0xe7ebf1;
const GREEN: u32 = 0x267b62;
const MINT: u32 = 0xe9f4ef;
const AMBER: u32 = 0x9b742b;

actions!(
    semweft_desktop,
    [
        Quit,
        Close,
        Refresh,
        OpenWorkspace,
        FindText,
        FindFile,
        FindSymbol,
        Back,
        Forward,
        ToggleBookmark,
        FocusLine,
        GoToLine
    ]
);

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Review,
    Overview,
    Symbols,
    Dependencies,
    Files,
    Search,
    Graph,
    Metrics,
    Bookmarks,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Impacts,
    Changes,
    Gaps,
}

#[derive(Clone)]
struct Launch {
    data: PathBuf,
    repository: PathBuf,
    check_ui: bool,
    exporter: PathBuf,
    python: String,
    workspace: Option<PathBuf>,
}

impl Launch {
    fn discover() -> Result<Self> {
        let resources = std::env::current_exe()?
            .parent()
            .unwrap()
            .join("../Resources");
        let config: Value = fs::read(resources.join("config.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or(Value::Null);
        let root = config["repository"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
        let mut options = Self {
            repository: root.clone(),
            check_ui: false,
            data: root.join("target/desktop-preview/data.json"),
            exporter: root.join("crates/semweft-desktop/scripts/export_preview.py"),
            python: config["python"].as_str().unwrap_or("python3").to_owned(),
            workspace: config["workspace"].as_str().map(PathBuf::from),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--check-ui" => options.check_ui = true,
                "--data" => {
                    options.data = PathBuf::from(args.next().context("--data requires a path")?)
                }
                _ if arg.starts_with("-psn_") => {}
                _ => bail!("Unknown argument: {arg}. Usage: semweft-desktop [--data preview.json]"),
            }
        }
        Ok(options)
    }
}

struct Desktop {
    launch: Launch,
    data: Preview,
    rows: Vec<crate::model::ImpactRow>,
    marks: LocalMarks,
    page: Page,
    tab: Tab,
    selected: usize,
    scope: usize,
    unread: bool,
    side: &'static str,
    busy: bool,
    message: String,
    focus: FocusHandle,
    query: Entity<TextInput>,
    _query_subscription: Subscription,
    search_text: String,
    search_hits: Vec<SearchHit>,
    search_total: usize,
    file_tree: FileTree,
    collapsed_folders: BTreeSet<String>,
    file_scroll: UniformListScrollHandle,
    outline_visible: bool,
    source_file: String,
    source_line: usize,
    source_lines: Vec<String>,
    source_code: SourceDocument,
    definition: source_view::DefinitionState,
    line_query: Entity<TextInput>,
    source_scroll: UniformListScrollHandle,
    history: Vec<(String, usize)>,
    history_position: usize,
    session: Session,
    navigation: client::Navigation,
    navigation_symbol: String,
    navigation_busy: bool,
    navigation_generation: u64,
    navigation_error: String,
    references: bool,
    direction: usize,
    graph_depth: usize,
    matrix_offset: usize,
    matrix_col_offset: usize,
}

fn muted(value: impl Into<SharedString>) -> Div {
    rsx! { <div text_color={rgb(MUTED)} text_size={px(12.)}>{value.into()}</div> }
}
fn heading(value: impl Into<SharedString>) -> Div {
    rsx! { <div font_weight={FontWeight::SEMIBOLD}>{value.into()}</div> }
}
impl Desktop {
    fn new(mut launch: Launch, data: Preview, cx: &mut Context<Self>) -> Self {
        if !data.example {
            launch.workspace = Some(data.workspace.clone());
        }
        let marks = LocalMarks::load(
            &LocalMarks::path(&launch.data, &data.review_key),
            &data.review_key,
        );
        let rows = data.rows();
        let source_file = data.sources.keys().next().cloned().unwrap_or_default();
        let file_tree = FileTree::new(data.sources.keys().cloned());
        let mut collapsed_folders = file_tree.directories();
        FileTree::reveal(&source_file, &mut collapsed_folders);
        let source_lines = data
            .sources
            .get(&source_file)
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        let source_code = SourceDocument::new(
            &source_file,
            data.sources
                .get(&source_file)
                .map(String::as_str)
                .unwrap_or_default(),
        );
        let session = Session::load(&launch.data, &data.workspace);
        let query = cx.new(|cx| TextInput::new("搜索文件、符号或源码…", cx));
        let line_query =
            cx.new(|cx| TextInput::new("行号", cx).with_key_context("SemweftInput SemweftLine"));
        let subscription = cx.observe(&query, |this, query, cx| {
            let value = query.read(cx).value().to_owned();
            if value != this.search_text {
                this.search_text = value;
                (this.search_hits, this.search_total) = this.data.text_search(&this.search_text);
                cx.notify();
            }
        });
        let history = vec![(source_file.clone(), 1)];
        Self {
            launch,
            data,
            rows,
            marks,
            page: Page::Files,
            tab: Tab::Impacts,
            selected: 0,
            scope: 0,
            unread: false,
            side: "head",
            busy: false,
            message: String::new(),
            focus: cx.focus_handle(),
            query,
            _query_subscription: subscription,
            search_text: String::new(),
            search_hits: vec![],
            search_total: 0,
            file_tree,
            collapsed_folders,
            file_scroll: UniformListScrollHandle::new(),
            outline_visible: true,
            source_file,
            source_line: 1,
            source_lines,
            source_code,
            definition: source_view::DefinitionState::default(),
            line_query,
            source_scroll: UniformListScrollHandle::new(),
            history,
            history_position: 0,
            session,
            navigation: client::Navigation::default(),
            navigation_symbol: String::new(),
            navigation_busy: false,
            navigation_generation: 0,
            navigation_error: String::new(),
            references: false,
            direction: 0,
            graph_depth: 2,
            matrix_offset: 0,
            matrix_col_offset: 0,
        }
    }

    fn change_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.selected = 0;
        self.side = "head";
        cx.notify();
    }

    fn refresh(&mut self, workspace: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.message = "正在捕获工作区与分析证据…".into();
        let options = self.launch.clone();
        let requested = workspace.or(options.workspace.clone());
        let next_workspace = requested.clone();
        let task = cx.background_executor().spawn(async move {
            let destination = options.data.with_extension("next.json");
            let mut command = Command::new(&options.python);
            command
                .arg(&options.exporter)
                .arg("--output")
                .arg(&destination);
            if let Some(path) = requested {
                command.arg("--workspace").arg(path);
            }
            let output = command.output().context("无法运行本地数据导出器")?;
            if !output.status.success() {
                bail!("{}", String::from_utf8_lossy(&output.stderr));
            }
            let data = Preview::load(&destination)?;
            fs::rename(destination, &options.data)?;
            Ok::<_, anyhow::Error>(data)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(data) => {
                        this.launch.workspace = next_workspace;
                        this.rows = data.rows();
                        this.marks = LocalMarks::load(
                            &LocalMarks::path(&this.launch.data, &data.review_key),
                            &data.review_key,
                        );
                        let same_workspace = this.data.workspace == data.workspace;
                        this.session = Session::load(&this.launch.data, &data.workspace);
                        this.file_tree = FileTree::new(data.sources.keys().cloned());
                        if !same_workspace {
                            this.collapsed_folders = this.file_tree.directories();
                        }
                        this.data = data;
                        this.navigation_generation += 1;
                        this.navigation_busy = false;
                        this.navigation = client::Navigation::default();
                        this.navigation_symbol.clear();
                        this.navigation_error.clear();
                        this.matrix_offset = 0;
                        this.matrix_col_offset = 0;
                        let file = if same_workspace
                            && this.data.sources.contains_key(&this.source_file)
                        {
                            this.source_file.clone()
                        } else {
                            this.data.sources.keys().next().cloned().unwrap_or_default()
                        };
                        let line = if same_workspace { this.source_line } else { 1 };
                        this.history.clear();
                        this.history_position = 0;
                        this.source_lines.clear();
                        this.invalidate_definition();
                        this.open_location(file, line, true, cx);
                        (this.search_hits, this.search_total) =
                            this.data.text_search(&this.search_text);
                        this.selected = 0;
                        this.message = "已捕获新的固定视图".into();
                    }
                    Err(error) => this.message = format!("刷新失败，保留原视图：{error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_workspace(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择 Rust / Go / TS-JS 项目".into()),
        });
        cx.spawn(async move |this, cx| match prompt.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.refresh(Some(path), cx));
                }
            }
            Ok(Err(error)) => {
                let _ = this.update(cx, |this, cx| {
                    this.message = error.to_string();
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    fn export(&self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_new_path(&self.data.workspace, Some("semweft-review.json"));
        let report = json!({
            "type": "semweft_local_preview_review", "schema_version": "0.1",
            "review_key": self.data.review_key, "snapshots": self.data.impact.snapshots,
            "local_marks": self.marks, "captured_view": self.data.raw,
            "scope": "Local preview marks only; no Agent submission or approval."
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = prompt.await {
                let result = serde_json::to_vec_pretty(&report)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| fs::write(path, bytes).map_err(anyhow::Error::from));
                let _ = this.update(cx, |this, cx| {
                    this.message = match result {
                        Ok(()) => "已导出快照证据与本地标记".into(),
                        Err(e) => format!("导出失败：{e}"),
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn mark(&mut self, decision: &str, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let previous = self.marks.clone();
        if self
            .marks
            .decisions
            .get(&row.key)
            .is_some_and(|d| d == decision)
        {
            self.marks.decisions.remove(&row.key);
        } else {
            self.marks
                .decisions
                .insert(row.key.clone(), decision.to_owned());
        }
        match self
            .marks
            .save(&LocalMarks::path(&self.launch.data, &self.data.review_key))
        {
            Ok(()) => self.message = "本地标记已保存 · 候选关系的确定性保持原值".into(),
            Err(error) => {
                self.marks = previous;
                self.message = format!("保存失败：{error}");
            }
        }
        cx.notify();
    }

    fn metrics(&self) -> gpui::AnyElement {
        let values = [
            ("变更符号", self.data.impact.changes.len(), "Base → Head"),
            ("影响关联", self.rows.len(), "合并两侧相同符号关联"),
            ("涉及文件", self.data.affected_files(), "候选影响分布"),
            ("本地已阅", self.marks.decisions.len(), "仅当前快照"),
        ];
        rsx! {
            <div class="flex gap-3 mb-4 flex-shrink-0">
                {for (label, value, note) in values {
                    <StatCard label={label} value={value} note={SharedString::from(note)} />
                }}
            </div>
        }
        .into_any_element()
    }

    fn notice(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div class="flex items-center gap-3 p-3 mb-4 rounded-md bg-[#fbf7ec] border flex-shrink-0" border_color={rgb(0xeee4ca)}>
                <div text_color={rgb(AMBER)} text_size={px(18.)}>{"ⓘ"}</div>
                <div class="flex-1">
                    <div class="font-semibold" text_size={px(11.)} text_color={rgb(AMBER)}>
                        {if self.data.impact_error.is_some() { "未建立 Git 双侧比较 · 当前为符号影响" } else if self.data.symbols.epochs["semantic"].as_u64().unwrap_or(0) > 0 { "已捕获语义视图 · 仍需核查候选关系与缺口" } else { "L1 候选视图 · 语义分析尚未运行" }}
                    </div>
                    <div base={muted("名称匹配产生候选；词法绑定、宏展开及外部消费者需进一步核查。")} class="mt-1" text_size={px(10.)} />
                </div>
                <button id="show-gaps" class="cursor-pointer" text_size={px(11.)} text_color={rgb(AMBER)}
                    onClick={cx.listener(|this, _, _, cx| { this.page = Page::Review; this.tab = Tab::Gaps; this.selected = 0; cx.notify(); })}>
                    {"查看缺口  →"}
                </button>
            </div>
        }.into_any_element()
    }

    fn tabs(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let tabs = [
            (Tab::Impacts, "影响范围", self.rows.len()),
            (Tab::Changes, "变更符号", self.data.impact.changes.len()),
            (Tab::Gaps, "分析缺口", self.data.gaps().len()),
        ];
        rsx! {
            <div class="flex items-center justify-between mb-3 p-3 border rounded-xl bg-white flex-shrink-0" border_color={rgb(LINE)}>
                <div class="flex gap-1">
                    {for (tab, title, count) in tabs {
                        <ActionButton id={SharedString::from(title)} activeIf={self.tab == tab}
                            onClick={cx.listener(move |this, _, _, cx| { this.tab = tab; this.selected = 0; cx.notify(); })}>
                            {format!("{title}  {count}")}
                        </ActionButton>
                    }}
                </div>
                <div class="flex gap-2">
                    {...(self.tab != Tab::Gaps).then(||
                        rsx! { <ActionButton id="scope" activeIf={self.scope > 0} onClick={cx.listener(|this, _, _, cx| { this.scope = (this.scope + 1) % 3; cx.notify(); })}>{["全部代码", "生产代码", "测试代码"][self.scope]}</ActionButton> }
                    )}
                    {...(self.tab == Tab::Impacts).then(||
                        rsx! { <ActionButton id="unread" activeIf={self.unread} onClick={cx.listener(|this, _, _, cx| { this.unread = !this.unread; cx.notify(); })}>{"仅未阅"}</ActionButton> }
                    )}
                </div>
            </div>
        }.into_any_element()
    }

    fn scope_matches(&self, scope: &str) -> bool {
        match self.scope {
            1 => scope == "production",
            2 => scope == "test",
            _ => true,
        }
    }

    fn review_list(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let items = match self.tab {
            Tab::Impacts => {
                let rows = self.rows.iter().enumerate().filter_map(|(index, row)| {
                    let r = row.preferred();
                    if !self.scope_matches(&r.scope)
                        || (self.unread && self.marks.decisions.contains_key(&row.key))
                    {
                        return None;
                    }
                    let source = r
                        .evidence
                        .first()
                        .and_then(|e| e["source"].as_str())
                        .unwrap_or("—");
                    let status = match self.marks.decisions.get(&row.key).map(String::as_str) {
                        Some("follow") => "需要跟进",
                        Some("dismiss") => "暂不关注",
                        _ if r.depth == 1 => "直接关联",
                        _ => "间接关联",
                    };
                    Some((index, r, source, status))
                });
                rsx_fragment! {
                    <>
                        {for (index, r, source, status) in rows {
                            <div key={index} class="px-4 py-4 border-b cursor-pointer flex items-center gap-3" border_color={rgb(LINE)}
                                bg={rgb(if index == self.selected { 0xeef6f1 } else { 0xffffff })} hoverClass="bg-[#f3f6fa]"
                                onClick={cx.listener(move |this, _, _, cx| { this.selected = index; this.side = "head"; cx.notify(); })}>
                                <div class="rounded-md w-[30px] h-[32px] flex items-center justify-center" text_size={px(16.)} text_color={rgb(GREEN)} bg={rgb(MINT)}>{"ƒ"}</div>
                                <div class="flex-1 min-w-0">
                                    <div class="font-semibold truncate" text_size={px(12.)}>{r.from_name.clone()}</div>
                                    <div base={muted(format!("{}:{}  →  {}", r.location.file, r.location.line, r.to_name))} class="mt-2 truncate" text_size={px(10.)} />
                                </div>
                                <div class="flex flex-col items-end gap-2">
                                    <div class="flex gap-1">
                                        <Badge label={source.to_owned()} color={MUTED} background={0xf1f3f6u32} />
                                        <Badge label={if r.certainty == "definite" { "已解析" } else { "候选" }} color={AMBER} background={0xf8f0dbu32} />
                                    </div>
                                    <div base={muted(format!("{status} · {} 层", r.depth))} text_size={px(10.)} />
                                </div>
                            </div>
                        }}
                    </>
                }
            }
            Tab::Changes => {
                let symbols = self
                    .data
                    .impact
                    .changes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, change)| {
                        change.symbol().map(|symbol| (index, symbol, &change.kind))
                    })
                    .filter(|(_, symbol, _)| self.scope_matches(&symbol.scope));
                rsx_fragment! {
                    <>
                        {for (index, symbol, kind) in symbols {
                            <div key={index} class="p-4 border-b cursor-pointer" border_color={rgb(LINE)}
                                bg={rgb(if index == self.selected { 0xeef6f1 } else { 0xffffff })} hoverClass="bg-[#f3f6fa]"
                                onClick={cx.listener(move |this, _, _, cx| { this.selected = index; cx.notify(); })}>
                                <div class="font-semibold truncate" text_size={px(12.)}>{symbol.qualified_name.clone()}</div>
                                <div base={muted(format!("{}:{} · {}", symbol.location.file, symbol.location.line, kind))} class="mt-2" />
                            </div>
                        }}
                    </>
                }
            }
            Tab::Gaps => {
                let mut kinds = std::collections::BTreeMap::<String, usize>::new();
                for gap in self.data.gaps() {
                    *kinds
                        .entry(gap["kind"].as_str().unwrap_or("unknown").to_owned())
                        .or_default() += 1;
                }
                rsx_fragment! {
                    <>
                        {for (kind, count) in kinds {
                            <div class="p-4 border-b" border_color={rgb(LINE)}>
                                <div class="flex gap-2">
                                    <div base={heading(gap_description(&kind).0)} text_size={px(12.)} /><Badge label={count.to_string()} color={AMBER} background={0xfbf7ecu32} />
                                </div>
                                <div base={muted(gap_description(&kind).1)} class="mt-3" /><div base={muted(kind)} class="mt-3" text_size={px(10.)} />
                            </div>
                        }}
                    </>
                }
            }
        };
        let count = items.len();
        rsx! {
            <div class="flex-1 min-w-0 flex flex-col min-h-0">
                <PanelHeader title="符号与关联路径" />
                <div id="review-scroll" class="overflow-y-scroll flex-1 min-h-0"
                    when={(count == 0, |list| list.child(rsx! { <EmptyState label="当前筛选没有匹配条目" /> }))}>
                    {for item in items { {item} }}
                </div>
                <div class="px-4 py-3 border-t" border_color={rgb(LINE)}>
                    <div base={muted(format!("展示 {count} 项 · 固定输入视图"))} text_size={px(10.)} />
                </div>
            </div>
        }.into_any_element()
    }

    fn side_switcher(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div class="flex gap-1">
                {for side in ["base", "head"] {
                    <ActionButton id={SharedString::from(side)} compact activeIf={self.side == side}
                        onClick={cx.listener(move |this, _, _, cx| { this.side = side; cx.notify(); })}>
                        {side.to_uppercase()}
                    </ActionButton>
                }}
            </div>
        }.into_any_element()
    }

    fn code(&self, file: &str, line: usize, cx: &Context<Self>) -> gpui::AnyElement {
        let side = if self.page == Page::Symbols {
            "head"
        } else {
            self.side
        };
        let lines = self.data.snippet(file, line, side);
        rsx! {
            <div class="mt-5">
                <div class="flex items-center justify-between mb-3">
                    {muted("捕获的源码")}
                    {if self.page == Page::Symbols { rsx! { <Badge label={"HEAD"} color={GREEN} background={MINT} /> }.into_any_element() } else { self.side_switcher(cx) }}
                </div>
                <div id="source-scroll" class="overflow-x-scroll rounded-md bg-[#f7f9fb] border py-2" border_color={rgb(LINE)}
                    when={(lines.is_empty(), |code| code.child(rsx! { <EmptyState label="此侧没有捕获的源码" /> }))}>
                    {for (number, content) in &lines {
                        <div class="flex gap-3 px-3 py-1 whitespace-nowrap" font_family="Menlo" text_size={px(10.)}
                            bg={rgb(if *number == line { 0xe6efe9 } else { 0xf7f9fb })}>
                            <div class="w-[22px] flex-shrink-0" text_color={rgb(0x9aa6af)}>{number.to_string()}</div>
                            <div text_color={rgb(0x425b6a)}>{content.clone()}</div>
                        </div>
                    }}
                </div>
                <div base={muted(format!("{file} · {}", side.to_uppercase()))} class="mt-2" text_size={px(10.)} />
            </div>
        }.into_any_element()
    }

    fn symbol_details(&self, symbol: &Symbol, side: &str, cx: &Context<Self>) -> gpui::AnyElement {
        let navigable = side == "head";
        rsx! {
            <div class="p-4">
                <div class="flex items-start gap-3 mb-4"><SymbolBadge kind={symbol.kind.clone()} /><div class="font-semibold min-w-0" text_size={px(15.)}>{symbol.qualified_name.clone()}</div></div>
                <div base={muted(format!("{}:{}", symbol.location.file, symbol.location.line))} class="mt-2" />
                <div class="bg-[#f6f8fa] p-3 rounded-md mt-5" font_family="Menlo" text_size={px(11.)}>{symbol.signature.clone()}</div>
                <div base={muted(symbol.docs.clone().unwrap_or_default())} class="mt-3" />
                <DetailField name={"类型"} value={symbol.kind.clone()} /><DetailField name={"可见性"} value={symbol.visibility.clone()} />
                <DetailField name={"作用域"} value={symbol.scope.clone()} />
                <div base={muted(format!("ID {}…", symbol.symbol_id.chars().take(22).collect::<String>()))} text_size={px(10.)} />
                <div class="flex gap-2 mt-4 flex-wrap">
                    <ActionButton id="symbol-source" disabledIf={!navigable} onClick={cx.listener({ let symbol = symbol.clone(); move |this, _, _, cx| {
                        if navigable && this.verify_symbol_location(&symbol, cx) {
                            this.open_location(symbol.location.file.clone(), symbol.location.line, true, cx);
                            this.page = Page::Files;
                        }
                    }})}>{"打开源码"}</ActionButton>
                    <ActionButton id="symbol-calls" active disabledIf={!navigable} onClick={cx.listener({ let symbol = symbol.clone(); move |this, _, _, cx| {
                        if navigable && this.verify_symbol_location(&symbol, cx) { this.load_navigation(symbol.symbol_id.clone(), false, cx); }
                    }})}>{"调用图"}</ActionButton>
                    <ActionButton id="symbol-refs" disabledIf={!navigable} onClick={cx.listener({ let symbol = symbol.clone(); move |this, _, _, cx| {
                        if navigable && this.verify_symbol_location(&symbol, cx) { this.load_navigation(symbol.symbol_id.clone(), true, cx); }
                    }})}>{"引用"}</ActionButton>
                </div>
                {...(!navigable).then(|| rsx! { <div base={muted("Base 源码见下方；完整源码、调用图和引用导航目前仅支持 Head。")} class="mt-3" /> })}
                {self.code(&symbol.location.file, symbol.location.line, cx)}
            </div>
        }.into_any_element()
    }

    fn inspector(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let body = match self.tab {
            Tab::Gaps => rsx! {
                <div class="p-5">
                    <div class="font-semibold" text_size={px(15.)}>{"证据是线索，判断属于你。"}</div>
                    <div base={muted("candidate 表示待核查的候选关系，来源等级与确定性分别展示。")} class="mt-4" />
                    <div base={muted("宏展开、动态分派和仓库外消费者均不能据此判定完整。规则核验尚未执行。")} class="mt-4" />
                    <div base={muted("需要更多确定性时，可在 CLI 显式授权并运行语义索引。")} class="mt-4" />
                </div>
            }.into_any_element(),
            Tab::Changes => self.data.impact.changes.get(self.selected).and_then(|change|
                if self.side == "base" { change.base_symbol.as_ref() } else { change.head_symbol.as_ref() })
                .map(|symbol| self.symbol_details(symbol, self.side, cx))
                .unwrap_or_else(|| rsx! {
                    <div class="p-4">{self.side_switcher(cx)}<div base={muted("此侧不存在该符号")} class="mt-4" /></div>
                }.into_any_element()),
            Tab::Impacts => if let Some(row) = self.rows.get(self.selected).filter(|row| self.scope_matches(&row.preferred().scope) && (!self.unread || !self.marks.decisions.contains_key(&row.key))) {
                if let Some(r) = row.on_side(self.side) {
                    let source = r.evidence.iter().filter_map(|e| e["source"].as_str()).collect::<Vec<_>>().join(" / ");
                    let decision = self.marks.decisions.get(&row.key).map(String::as_str);
                    rsx! {
                        <div class="p-4">
                            <div class="font-semibold" text_size={px(15.)}>{r.from_name.clone()}</div>
                            <div base={muted(format!("{}:{}:{}", r.location.file, r.location.line, r.location.column))} class="mt-2" />
                            <div base={muted("关联路径")} class="mt-5" />
                            <div class="p-3 mt-2 rounded-md border bg-[#f4f8f5]" text_size={px(12.)} border_color={rgb(0xd7e6dc)}>{self.data.route(r)}</div>
                            <div base={muted("沿索引关系可到达变更符号，仍需核查真实绑定与行为。")} class="mt-3" />
                            <div class="mt-4 border-t border-b" border_color={rgb(LINE)}>
                                <DetailField name={"来源"} value={source} /><DetailField name={"确定性"} value={r.certainty.clone()} />
                                <DetailField name={"新鲜度"} value={r.freshness.clone()} /><DetailField name={"作用域"} value={r.scope.clone()} />
                            </div>
                            {self.code(&r.location.file, r.location.line, cx)}
                            <div base={muted("本地审阅")} class="mt-6" />
                            <div class="flex gap-2 mt-3">
                                <ActionButton id="follow" activeIf={decision == Some("follow")} onClick={cx.listener(|this, _, _, cx| this.mark("follow", cx))}>{"✓  需要跟进"}</ActionButton>
                                <ActionButton id="dismiss" activeIf={decision == Some("dismiss")} onClick={cx.listener(|this, _, _, cx| this.mark("dismiss", cx))}>{"暂不关注"}</ActionButton>
                            </div>
                            <div base={muted("仅保存在本机，标记不会改变候选确定性。")} class="mt-3" text_size={px(10.)} />
                        </div>
                    }.into_any_element()
                } else { rsx! {
                    <div class="p-4">{self.side_switcher(cx)}<div base={muted("此侧未捕获该关联，未复用另一侧坐标。")} class="mt-4" /></div>
                }.into_any_element() }
            } else { rsx! { <div class="p-5">{muted("选择左侧条目查看证据")}</div> }.into_any_element() },
        };
        rsx! {
            <div class="w-[352px] flex-shrink-0 border-l flex flex-col min-h-0 bg-[#fcfcfe]" border_color={rgb(LINE)}>
                <PanelHeader title="证据详情" />
                <div id="inspector-scroll" class="flex-1 min-h-0 overflow-y-scroll">{body}</div>
            </div>
        }.into_any_element()
    }

    fn review(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div class="flex flex-col flex-1 min-h-0">
                {self.metrics()}{self.notice(cx)}{self.tabs(cx)}
                <div class="flex flex-1 min-h-0 border rounded-xl overflow-hidden bg-white" border_color={rgb(LINE)}>
                    {self.review_list(cx)}{self.inspector(cx)}
                </div>
            </div>
        }.into_any_element()
    }

    fn overview(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div id="overview-scroll" class="overflow-y-scroll flex-1 min-h-0">
                <div class="p-5 rounded-xl bg-white border mb-4" border_color={rgb(LINE)}>
                    <div class="flex items-center justify-between gap-4">
                        <div class="flex items-center gap-4 min-w-0">
                            <div class="w-[46px] h-[46px] rounded-xl bg-[#eaf3ee] flex items-center justify-center"><Icon name="folder-open" color={GREEN} size={24u32} /></div>
                            <div class="min-w-0"><div class="font-semibold truncate" text_size={px(19.)}>{self.data.name.clone()}</div><div base={muted(self.data.workspace.to_string_lossy().into_owned())} class="mt-2 truncate" text_size={px(11.)} /></div>
                        </div>
                        <Badge label={self.data.branch.clone().unwrap_or_else(|| "本地项目".into())} />
                    </div>
                </div>
                <div class="flex gap-3 mb-5">
                    <StatCard label="源文件" value={self.data.sources.len()} />
                    <StatCard label="已捕获符号" value={self.data.symbols.symbols.len()} />
                    <StatCard label="模块" value={self.data.module_map.modules.len()} />
                    <StatCard label="变更符号" value={self.data.impact.changes.len()} />
                </div>
                <div class="flex items-center gap-2 mb-3"><Icon name="layers" color={GREEN} /><div class="font-semibold" text_size={px(13.)}>{"模块与核心符号"}</div><div class="flex-1" />{muted("选择模块，查看符号")}</div>
                <div class="grid grid-cols-2 gap-3">
                    {for (index, module) in self.data.map.modules.iter().enumerate().filter(|(_, module)| self.matches(&module.path)) {
                        <div id={("module", index)} class="p-4 rounded-xl border bg-white cursor-pointer min-w-0" border_color={rgb(LINE)} hoverClass="bg-[#f8fafc]"
                            onClick={cx.listener(move |this, _, _, cx| {
                                let path = &this.data.map.modules[index].path;
                                this.selected = this.data.symbols.symbols.iter().position(|symbol| &symbol.location.file == path).unwrap_or(0);
                                this.page = Page::Symbols; cx.notify();
                            })}>
                            <div class="flex items-center gap-2 mb-3"><FileBadge path={module.path.clone()} /><div class="font-medium truncate flex-1" text_size={px(12.)}>{module.path.clone()}</div><Badge label={module.symbol_count.to_string()} /></div>
                            {for symbol in self.data.symbols.symbols.iter().filter(|symbol| symbol.location.file == module.path).take(4) {
                                <div class="flex items-center gap-2 mt-2"><SymbolBadge kind={symbol.kind.clone()} /><div class="truncate" text_size={px(11.)} text_color={rgb(MUTED)}>{symbol.qualified_name.rsplit("::").next().unwrap_or(&symbol.qualified_name).to_owned()}</div></div>
                            }}
                        </div>
                    }}
                </div>
            </div>
        }.into_any_element()
    }

    fn symbols(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let visible = self
            .data
            .symbols
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                self.matches(&format!(
                    "{} {} {}",
                    s.qualified_name, s.kind, s.location.file
                ))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let count = visible.len();
        let active = visible
            .iter()
            .copied()
            .find(|index| *index == self.selected)
            .or_else(|| visible.first().copied());
        let list = uniform_list("symbol-search-list", count, cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
            range.map(|row| {
                let index = visible[row]; let symbol = &this.data.symbols.symbols[index];
                rsx! {
                    <div id={("symbol", index)} class="w-full min-w-0 px-4 h-[64px] flex items-center gap-3 border-b cursor-pointer" border_color={rgb(LINE)}
                        bg={rgb(if Some(index) == active { 0xeef6f1 } else { 0xffffff })} hoverClass="bg-[#f3f6fa]"
                        onClick={cx.listener(move |this, _, _, cx| { this.selected = index; cx.notify(); })}>
                        <SymbolBadge kind={symbol.kind.clone()} />
                        <div class="flex-1 min-w-0">
                            <div class="font-medium truncate" text_size={px(12.)}>{symbol.qualified_name.clone()}</div>
                            <div base={muted(format!("{}:{} · {}", symbol.location.file, symbol.location.line, symbol.kind))} class="mt-1 truncate" text_size={px(10.)} />
                        </div>
                        <Icon name="chevron-right" size={12u32} color={0xc0c8d3u32} />
                    </div>
                }
            }).collect::<Vec<_>>()
        }));
        rsx! {
            <div class="flex flex-1 min-h-0 border rounded-xl overflow-hidden bg-white" border_color={rgb(LINE)}>
                <div class="flex flex-col flex-1 min-h-0 min-w-0">
                    <PanelHeader title="所有符号" count={SharedString::from(count.to_string())}><span text_size={px(10.)} text_color={rgb(MUTED)}>{"⌘T 搜索"}</span></PanelHeader>
                    <div base={list} class="flex-1 min-h-0" />
                    {...(count == 0).then(|| rsx! { <EmptyState label="没有匹配的符号" /> })}
                </div>
                <div id="symbol-detail-scroll" class="w-[360px] flex-shrink-0 border-l bg-[#fcfcfe] overflow-y-scroll" border_color={rgb(LINE)}
                    whenSome={(active.and_then(|index| self.data.symbols.symbols.get(index)), |panel, symbol| panel.child(self.symbol_details(symbol, "head", cx)))} />
            </div>
        }.into_any_element()
    }
}

impl Render for Desktop {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.page {
            Page::Review => self.review(cx),
            Page::Overview => self.overview(cx),
            Page::Symbols => self.symbols(cx),
            Page::Dependencies => self.dependencies(cx),
            Page::Files => self.files(cx),
            Page::Search => self.search(cx),
            Page::Graph => self.graph(cx),
            Page::Metrics => self.measurements(cx),
            Page::Bookmarks => self.bookmarks(cx),
        };
        rsx! {
            <div class="size-full flex flex-col bg-[#f3f5f8]" text_color={rgb(INK)} font_family=".SystemUIFont" text_size={px(13.)}
                track_focus={&self.focus}
                onAction={cx.listener(|this, _: &Refresh, _, cx| this.refresh(None, cx))}
                onAction={cx.listener(|this, _: &OpenWorkspace, _, cx| this.open_workspace(cx))}
                onAction={|_: &Close, window, _| window.remove_window()}
                onAction={cx.listener(|this, _: &FindText, window, cx| { this.page = Page::Search; this.query.focus_handle(cx).focus(window); cx.notify(); })}
                onAction={cx.listener(|this, _: &FindFile, window, cx| { this.page = Page::Files; this.query.focus_handle(cx).focus(window); cx.notify(); })}
                onAction={cx.listener(|this, _: &FindSymbol, window, cx| { this.page = Page::Symbols; this.query.focus_handle(cx).focus(window); cx.notify(); })}
                onAction={cx.listener(|this, _: &Back, _, cx| this.travel(false, cx))}
                onAction={cx.listener(|this, _: &Forward, _, cx| this.travel(true, cx))}
                onAction={cx.listener(|this, _: &ToggleBookmark, _, cx| this.toggle_bookmark(cx))}
                onAction={cx.listener(|this, _: &FocusLine, window, cx| { this.page = Page::Files; this.line_query.focus_handle(cx).focus(window); this.line_query.update(cx, |input, cx| input.select_all_value(cx)); cx.notify(); })}
                onAction={cx.listener(|this, _: &GoToLine, window, cx| this.go_to_line(window, cx))}>

                <div class="flex flex-1 min-h-0">
                    {self.sidebar(cx)}
                    <div class="flex flex-col flex-1 min-w-0">
                        {self.header(cx)}
                        <div class="p-5 flex flex-col flex-1 min-h-0">{self.page_heading()}{content}</div>
                    </div>
                </div>
                {self.statusbar()}
            </div>
        }
    }
}

fn gap_description(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "semantic_untrusted" => (
            "尚未运行语义索引",
            "本次只使用语法候选，没有运行仓库构建脚本或过程宏。词法绑定与遮蔽尚未解析。",
        ),
        "revision_semantic_unavailable" => (
            "Base 侧无匹配语义索引",
            "使用独立的 L1 语法事实分析提交侧，不能作为完整语义影响。",
        ),
        "macro_not_expanded" => (
            "宏展开不在分析范围",
            "宏生成的引用需要结合源码与测试进一步核查。",
        ),
        "closure_owner_not_modeled" => ("闭包归属尚未建模", "闭包内的调用尚未完整归属到外围符号。"),
        "callback_invocation_not_modeled" => (
            "回调调用尚未建模",
            "函数体已有独立归属；尚未推断回调调用方及动态传递。",
        ),
        _ => (
            "分析范围缺口",
            "该项已由引擎报告。具体字段与原始证据可通过导出报告查看。",
        ),
    }
}

pub fn check_native_text(system: &gpui::TextSystem) -> Result<()> {
    let ui_font = system.resolve_font(&gpui::font(".SystemUIFont"));
    let narrow = system.typographic_bounds(ui_font, px(16.), 'i')?;
    let wide = system.typographic_bounds(ui_font, px(16.), 'W')?;
    // NoopTextSystem returns the same placeholder bounds for every glyph.
    ensure!(
        wide.size.width > narrow.size.width && wide.size.height > px(0.),
        "GPUI has no native text backend; enable its macOS font-kit feature"
    );
    let cjk_font = system.resolve_font(&gpui::font("PingFang SC"));
    let cjk = system.typographic_bounds(cjk_font, px(16.), '中')?;
    ensure!(
        cjk.size.width > px(0.) && cjk.size.height > px(0.),
        "CJK glyphs are unavailable"
    );
    Ok(())
}

pub fn run() -> Result<()> {
    let launch = Launch::discover()?;
    let data = Preview::load(&launch.data)
        .context("请先运行 python3 crates/semweft-desktop/scripts/preview.py 生成本地分析视图")?;
    let check_navigation = if launch.check_ui {
        data.symbols
            .symbols
            .iter()
            .find(|s| s.kind == "function")
            .map(|symbol| {
                let query = client::Query {
                    symbol: symbol.symbol_id.clone(),
                    snapshot: data.symbols.snapshot_id.clone(),
                    epoch: data.symbols.epochs["semantic"].as_u64().unwrap_or(0),
                    direction: "both".into(),
                    depth: 2,
                    references: false,
                };
                client::navigate(&launch.repository, &data.workspace, &query)
                    .map(|navigation| (symbol.symbol_id.clone(), navigation))
            })
            .transpose()?
    } else {
        None
    };
    let application = Application::new().with_assets(icons::UiAssets);
    check_native_text(&application.text_system())?;
    application.run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        crate::input::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", Close, None),
            KeyBinding::new("cmd-r", Refresh, None),
            KeyBinding::new("cmd-o", OpenWorkspace, None),
            KeyBinding::new("cmd-f", FindText, None),
            KeyBinding::new("cmd-p", FindFile, None),
            KeyBinding::new("cmd-t", FindSymbol, None),
            KeyBinding::new("alt-left", Back, None),
            KeyBinding::new("alt-right", Forward, None),
            KeyBinding::new("cmd-d", ToggleBookmark, None),
            KeyBinding::new("cmd-l", FocusLine, None),
            KeyBinding::new("enter", GoToLine, Some("SemweftLine")),
        ]);
        cx.set_menus(vec![Menu {
            name: "Semweft".into(),
            items: vec![
                MenuItem::action("打开工作区…", OpenWorkspace),
                MenuItem::action("刷新视图", Refresh),
                MenuItem::separator(),
                MenuItem::action("退出 Semweft", Quit),
            ],
        }]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1430.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(1180.), px(760.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Semweft · 代码分析工作台".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let check_ui = launch.check_ui;
                let entity = cx.new(|cx| {
                    let mut view = Desktop::new(launch, data, cx);
                    if let Some((symbol, navigation)) = check_navigation {
                        view.navigation_symbol = symbol;
                        view.navigation = navigation;
                    }
                    if check_ui && view.session.bookmarks.is_empty() && !view.source_file.is_empty()
                    {
                        view.session.bookmarks.push(Bookmark {
                            file: view.source_file.clone(),
                            line: 1,
                            label: "Native render check".into(),
                        });
                    }
                    view.focus.focus(window);
                    view
                });
                if check_ui {
                    check_views(entity.clone(), 0, window);
                }
                entity
            },
        )
        .expect("Unable to open the GPUI window");
        cx.activate(true);
    });
    Ok(())
}

mod source_view;
mod workbench;

// Native rendering check: no screenshots, recording or accessibility automation.
fn check_views(view: Entity<Desktop>, index: usize, window: &mut Window) {
    let pages = [
        Page::Files,
        Page::Symbols,
        Page::Search,
        Page::Dependencies,
        Page::Metrics,
        Page::Bookmarks,
        Page::Graph,
        Page::Review,
        Page::Overview,
    ];
    window.on_next_frame(move |window, cx| {
        if let Some(page) = pages.get(index) {
            if index == 0 {
                use gpui::EntityInputHandler;
                view.update(cx, |this, cx| {
                    this.query.update(cx, |input, cx| {
                        input.replace_text_in_range(None, "中文😀", window, cx);
                        assert_eq!(
                            input.selected_text_range(false, window, cx).unwrap().range,
                            4..4
                        );
                        input.replace_and_mark_text_in_range(
                            Some(0..2),
                            "库存",
                            Some(0..2),
                            window,
                            cx,
                        );
                        assert_eq!(input.value(), "库存😀");
                        assert_eq!(
                            input.selected_text_range(false, window, cx).unwrap().range,
                            0..2
                        );
                        input.unmark_text(window, cx);
                        input.clear(cx);
                    })
                });
            }
            view.update(cx, |this, cx| {
                this.page = *page;
                cx.notify();
            });
            check_views(view, index + 1, window);
        } else {
            check_variants(view, 0, window);
        }
    });
    window.refresh();
}

fn check_variants(view: Entity<Desktop>, index: usize, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        view.update(cx, |this, cx| {
            match index {
                0..=2 => {
                    this.page = Page::Review;
                    this.tab = [Tab::Impacts, Tab::Changes, Tab::Gaps][index];
                }
                3 => {
                    this.page = Page::Graph;
                    this.references = true;
                }
                4 => {
                    this.navigation_busy = true;
                }
                5 => {
                    this.navigation_busy = false;
                    this.navigation.relations.clear();
                    this.navigation_error = "Native check: error state".into();
                }
                6..=7 => {
                    this.page = Page::Search;
                    this.search_text = if index == 6 {
                        "fn"
                    } else {
                        "__semweft_no_match_7c5e3a__"
                    }
                    .into();
                    (this.search_hits, this.search_total) =
                        this.data.text_search(&this.search_text);
                }
                8 => {
                    this.page = Page::Bookmarks;
                    this.session.bookmarks.clear();
                }
                9 => {
                    this.page = Page::Files;
                    this.source_file.clear();
                    this.source_lines.clear();
                    this.source_code = SourceDocument::default();
                    this.busy = true;
                }
                10..=11 => {
                    this.busy = false;
                    this.page = Page::Review;
                    this.tab = Tab::Changes;
                    this.side = if index == 10 { "base" } else { "head" };
                    this.selected = this
                        .data
                        .impact
                        .changes
                        .iter()
                        .position(|change| change.base_symbol.is_some())
                        .unwrap_or(0);
                }
                _ => {
                    this.focus.focus(window);
                }
            }
            cx.notify();
        });
        if index < 12 {
            check_variants(view, index + 1, window);
        } else {
            check_shortcuts(view, 0, window);
        }
    });
    window.refresh();
}

// Dispatch through GPUI's rendered action tree, including bubbling from the input.
fn check_shortcuts(view: Entity<Desktop>, step: usize, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        match step {
            0 => window.dispatch_action(Box::new(FindText), cx),
            1 => {
                use gpui::EntityInputHandler;
                assert!(
                    view.read(cx).page == Page::Search,
                    "FindText did not reach the RSX listener"
                );
                let query = view.read(cx).query.clone();
                assert!(query.focus_handle(cx).is_focused(window));
                query.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "中文😀", window, cx)
                });
                window.dispatch_action(Box::new(crate::input::SelectAll), cx);
            }
            2 => {
                use gpui::EntityInputHandler;
                view.read(cx).query.clone().update(cx, |input, cx| {
                    assert_eq!(
                        input.selected_text_range(false, window, cx).unwrap().range,
                        0..4
                    );
                });
                window.dispatch_action(Box::new(crate::input::Backspace), cx);
            }
            3 => {
                assert!(
                    view.read(cx).query.read(cx).value().is_empty(),
                    "Input action was not delivered"
                );
                window.dispatch_action(Box::new(FindSymbol), cx);
            }
            4 => {
                assert!(
                    view.read(cx).page == Page::Symbols,
                    "FindSymbol did not bubble from the input"
                );
                window.dispatch_action(Box::new(FindFile), cx);
            }
            _ => {
                assert!(
                    view.read(cx).page == Page::Files,
                    "FindFile did not reach the RSX listener"
                );
                source_view::check_source_navigation(view, 0, 0, window);
                return;
            }
        }
        check_shortcuts(view, step + 1, window);
    });
    window.refresh();
}
