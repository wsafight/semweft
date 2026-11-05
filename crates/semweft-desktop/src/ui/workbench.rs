use super::*;

impl Desktop {
    pub(super) fn close_source_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let removed = self.session.tabs.remove(index);
        if removed.file == self.source_file && removed.side == self.source_side {
            if let Some(tab) = self
                .session
                .tabs
                .get(index.min(self.session.tabs.len().saturating_sub(1)))
                .cloned()
            {
                self.open_side(tab.file, tab.line, &tab.side, true, cx);
                return;
            }
            self.session.active = None;
            self.source_file.clear();
            self.source_lines.clear();
            self.source_code = SourceDocument::default();
            self.diff_view = diff_view::DiffView::default();
            self.invalidate_definition();
        }
        if let Err(error) = self.session.save(&self.launch.data) {
            self.message = error.to_string();
        }
        cx.notify();
    }

    pub(super) fn matches(&self, value: &str) -> bool {
        let haystack = value.to_lowercase();
        self.search_text
            .split_whitespace()
            .all(|word| haystack.contains(&word.to_lowercase()))
    }

    pub(super) fn open_location(
        &mut self,
        file: String,
        line: usize,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        self.open_side(file, line, "head", remember, cx);
    }

    pub(super) fn open_side(
        &mut self,
        file: String,
        line: usize,
        side: &str,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.data.sources_on(side).get(&file) else {
            self.message = format!("当前快照未捕获文件：{file}");
            cx.notify();
            return;
        };
        let changed_side = self.source_side != side;
        let changed = changed_side || self.source_file != file || self.source_line != line;
        if changed_side || self.source_file != file || self.source_lines.is_empty() {
            self.source_lines = source.lines().map(str::to_owned).collect();
            self.source_code = SourceDocument::new(&file, source);
        }
        self.source_side = side.into();
        if changed_side {
            self.file_tree = FileTree::new(self.data.sources_on(side).keys().cloned());
        }
        self.source_file = file;
        self.prepare_diff();
        FileTree::reveal(&self.source_file, &mut self.collapsed_folders);
        if let Some(index) = self
            .file_tree
            .visible(&self.search_text, &self.collapsed_folders)
            .iter()
            .position(|row| row.path == self.source_file)
        {
            self.file_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        self.source_line = line.max(1).min(self.source_lines.len().max(1));
        if changed {
            self.invalidate_definition();
        }
        self.source_scroll
            .scroll_to_item(self.source_line - 1, ScrollStrategy::Center);
        if let Some(row) = self.diff_view.diff.row_for_line(side, self.source_line) {
            self.diff_view.scroll_to(row);
        }
        if remember {
            let location = SourceTab {
                file: self.source_file.clone(),
                line: self.source_line,
                side: self.source_side.clone(),
            };
            if self.history.get(self.history_position) != Some(&location) {
                self.history.truncate(self.history_position + 1);
                self.history.push(location);
                if self.history.len() > 100 {
                    self.history.remove(0);
                }
                self.history_position = self.history.len() - 1;
            }
        }
        let tab = SourceTab {
            file: self.source_file.clone(),
            line: self.source_line,
            side: self.source_side.clone(),
        };
        if let Some(existing) = self
            .session
            .tabs
            .iter_mut()
            .find(|existing| existing.file == tab.file && existing.side == tab.side)
        {
            existing.line = tab.line;
        } else {
            self.session.tabs.push(tab.clone());
        }
        self.session.active = Some(tab);
        self.session.history = self.history.clone();
        self.session.history_position = self.history_position;
        self.session.outline_visible = Some(self.outline_visible);
        if let Err(error) = self.session.save(&self.launch.data) {
            self.message = format!("会话保存失败：{error}");
        }
        cx.notify();
    }

    pub(super) fn travel(&mut self, forward: bool, cx: &mut Context<Self>) {
        if forward && self.history_position + 1 < self.history.len() {
            self.history_position += 1;
        } else if !forward && self.history_position > 0 {
            self.history_position -= 1;
        } else {
            return;
        }
        if let Some(tab) = self.history.get(self.history_position).cloned() {
            self.open_side(tab.file, tab.line, &tab.side, false, cx);
            self.page = Page::Files;
        }
    }

    pub(super) fn toggle_bookmark(&mut self, cx: &mut Context<Self>) {
        if self.source_file.is_empty() {
            return;
        }
        let previous = self.session.clone();
        if let Some(index) = self.session.bookmarks.iter().position(|b| {
            b.matches_location(&self.source_file, self.source_line, &self.source_side)
        }) {
            self.session.bookmarks.remove(index);
        } else {
            self.session.bookmarks.push(Bookmark {
                file: self.source_file.clone(),
                side: self.source_side.clone(),
                line: self.source_line,
                label: self
                    .source_lines
                    .get(self.source_line - 1)
                    .map(|s| s.trim().chars().take(100).collect())
                    .unwrap_or_default(),
            });
        }
        if let Err(error) = self.session.save(&self.launch.data) {
            self.session = previous;
            self.message = format!("书签保存失败：{error}");
        } else {
            self.message = "书签已保存 · 绑定文件和行号，代码改动后请核查位置".into();
        }
        cx.notify();
    }

    pub(super) fn source_symbol(&self) -> Option<&Symbol> {
        self.data
            .symbols_on(&self.source_side)
            .symbols
            .iter()
            .filter(|s| {
                s.location.file == self.source_file
                    && s.span.start.line <= self.source_line
                    && s.span.end.line >= self.source_line
            })
            .min_by_key(|s| s.span.end.line.saturating_sub(s.span.start.line))
    }

    pub(super) fn load_navigation(
        &mut self,
        symbol: String,
        references: bool,
        cx: &mut Context<Self>,
    ) {
        self.load_side_navigation(symbol, references, self.source_side.clone(), cx);
    }

    pub(super) fn load_side_navigation(
        &mut self,
        symbol: String,
        references: bool,
        side: String,
        cx: &mut Context<Self>,
    ) {
        self.navigation_cancel.cancel();
        self.navigation_cancel = client::Cancellation::default();
        self.navigation_generation += 1;
        let generation = self.navigation_generation;
        self.navigation_busy = true;
        self.navigation_error.clear();
        self.navigation = client::Navigation::default();
        self.reset_graph();
        self.navigation_symbol = symbol.clone();
        self.navigation_side = side;
        self.references = references;
        self.restore_graph();
        self.page = Page::Graph;
        let query = client::Query {
            symbol,
            snapshot: self
                .data
                .symbols_on(&self.navigation_side)
                .snapshot_id
                .clone(),
            epoch: self.data.symbols_on(&self.navigation_side).epochs["semantic"]
                .as_u64()
                .unwrap_or(0),
            direction: ["both", "callers", "callees"][self.direction].into(),
            depth: self.graph_depth,
            references,
        };
        self.fetch_navigation(query, None, generation, cx);
    }

    fn fetch_navigation(
        &mut self,
        query: client::Query,
        cursor: Option<String>,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let cancellation = self.navigation_cancel.clone();
        let append = cursor.is_some();
        let task = cx
            .background_executor()
            .spawn(async move { client.navigate_page(&query, cursor, &cancellation) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.navigation_generation {
                    return;
                }
                this.navigation_busy = false;
                match result {
                    Ok(data) => {
                        if append {
                            this.navigation.relations.extend(data.relations);
                            let mut ids = this
                                .navigation
                                .nodes
                                .iter()
                                .map(|node| node.symbol_id.clone())
                                .collect::<BTreeSet<_>>();
                            this.navigation.nodes.extend(
                                data.nodes
                                    .into_iter()
                                    .filter(|node| ids.insert(node.symbol_id.clone())),
                            );
                            this.navigation.output = data.output;
                        } else {
                            this.navigation = data;
                        }
                    }
                    Err(e) => this.navigation_error = format!("查询失败：{e:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn more_navigation(&mut self, cx: &mut Context<Self>) {
        if self.navigation_busy {
            return;
        }
        let cursor = self.navigation.output["continuation"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
        if cursor.is_none() {
            return;
        }
        self.navigation_busy = true;
        let query = client::Query {
            symbol: self.navigation_symbol.clone(),
            snapshot: self
                .data
                .symbols_on(&self.navigation_side)
                .snapshot_id
                .clone(),
            epoch: self.data.symbols_on(&self.navigation_side).epochs["semantic"]
                .as_u64()
                .unwrap_or(0),
            direction: ["both", "callers", "callees"][self.direction].into(),
            depth: self.graph_depth,
            references: self.references,
        };
        self.fetch_navigation(query, cursor, self.navigation_generation, cx);
    }

    pub(super) fn files(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let entries = self
            .file_tree
            .visible(&self.search_text, &self.collapsed_folders);
        let count = self
            .data
            .sources_on(&self.source_side)
            .keys()
            .filter(|path| self.matches(path))
            .count();
        let list = uniform_list("project-file-list", entries.len(), cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let entry = &entries[index];
                let path = entry.path.clone();
                let directory = entry.directory;
                let selected = !directory && this.source_file == entry.path;
                rsx! {
                    <div class="w-full min-w-0 h-[30px] px-2">
                        <div key={entry.path} class="h-full w-full min-w-0 flex items-center gap-1 pr-2 rounded-md cursor-pointer"
                            pl={px(6. + entry.depth as f32 * 12.)}
                            whenClass={(selected, "bg-[#e7f2ec] text-[#216b53] font-medium")}
                            hoverClass="bg-[#eef2f6]"
                            onClick={cx.listener(move |this, _, _, cx| {
                                if directory {
                                    if this.search_text.trim().is_empty() && !this.collapsed_folders.remove(&path) { this.collapsed_folders.insert(path.clone()); }
                                    cx.notify();
                                } else { this.open_side(path.clone(), 1, &this.source_side.clone(), true, cx); }
                            })}>
                            {if directory { rsx! {
                                <div class="flex items-center gap-1 flex-shrink-0">
                                    <Icon name={if entry.expanded { "chevron-down" } else { "chevron-right" }} size={11u32} color={0x98a3b3u32} />
                                    <Icon name={if entry.expanded { "folder-open" } else { "folder" }} size={15u32} color={0x98a7bbu32} />
                                </div>
                            }.into_any_element() } else { rsx! {
                                <div class="flex items-center flex-shrink-0"><div class="w-[14px]" /><FileBadge path={entry.path.clone()} /></div>
                            }.into_any_element() }}
                            <div class="flex-1 min-w-0 truncate" text_size={px(11.)}>{entry.name.clone()}</div>
                            {...selected.then(|| rsx! { <div class="w-[4px] h-[4px] rounded-full bg-[#4c9a7b] flex-shrink-0" /> })}
                        </div>
                    </div>
                }
            }).collect::<Vec<_>>()
        })).track_scroll(self.file_scroll.clone());
        let mut outline = self
            .data
            .symbols_on(&self.source_side)
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, symbol)| symbol.location.file == self.source_file)
            .collect::<Vec<_>>();
        outline.sort_by_key(|(_, symbol)| (symbol.location.line, symbol.location.column));
        rsx! {
            <div class="flex flex-1 min-h-0 border rounded-xl overflow-hidden bg-white" border_color={rgb(LINE)}>
                <div class="flex-shrink-0 flex flex-col min-h-0 border-r bg-white" w={px(self.session.file_panel_width.unwrap_or(244.).clamp(160., 400.))} border_color={rgb(LINE)}>
                    <PanelHeader title="文件目录" count={SharedString::from(count.to_string())}>
                        <IconButton id="collapse-folders" icon="collapse" hint="收起所有目录" onClick={cx.listener(|this, _, _, cx| { this.collapsed_folders = this.file_tree.directories(); cx.notify(); })} />
                    </PanelHeader>
                    <div class="flex items-center gap-2 px-4 h-[36px] flex-shrink-0" text_size={px(10.)} text_color={rgb(MUTED)}>
                        <Icon name="folder" size={13u32} /><div class="truncate font-medium">{self.data.name.clone()}</div>
                    </div>
                    <div base={list} class="flex-1 min-h-0" />
                    {...(count == 0).then(|| rsx! { <EmptyState label="没有匹配的文件" /> })}
                    <div class="h-[34px] px-4 flex items-center gap-2 border-t flex-shrink-0" border_color={rgb(LINE)} text_size={px(10.)} text_color={rgb(MUTED)}><Icon name="search" size={12u32} />{if self.search_text.trim().is_empty() { "⌘P 搜索文件" } else { "搜索结果已展开目录" }}</div>
                </div>
                {self.pane_resize(false, cx)}
                {self.source_pane(cx)}
                {...self.outline_visible.then(|| rsx! {
                    <div class="flex min-h-0 flex-shrink-0">
                    {self.pane_resize(true, cx)}
                    <div class="flex-shrink-0 flex flex-col min-h-0 border-l bg-[#fcfcfe]" w={px(self.session.outline_width.unwrap_or(204.).clamp(140., 360.))} border_color={rgb(LINE)}>
                        <PanelHeader title="文件大纲" count={SharedString::from(outline.len().to_string())} />
                        <div id="file-outline" class="overflow-y-scroll flex-1 min-h-0 p-2">
                            {for (index, symbol) in outline {
                                <div id={("outline", index)} class="px-2 py-2 mb-1 rounded-lg cursor-pointer flex items-center gap-2" hoverClass="bg-[#eef2f6]"
                                    whenClass={(symbol.location.line == self.source_line, "bg-[#eaf2ed]")}
                                    onClick={cx.listener(move |this, _, _, cx| { let symbol = &this.data.symbols_on(&this.source_side).symbols[index]; this.open_side(symbol.location.file.clone(), symbol.location.line, &this.source_side.clone(), true, cx); })}>
                                    <SymbolBadge kind={symbol.kind.clone()} />
                                    <div class="flex-1 min-w-0">
                                        <div class="truncate" text_size={px(11.)}>{symbol.qualified_name.rsplit("::").next().unwrap_or(&symbol.qualified_name).to_owned()}</div>
                                        <div class="mt-1" text_size={px(9.)} text_color={rgb(MUTED)}>{format!("{} · L{}", symbol.kind, symbol.location.line)}</div>
                                    </div>
                                </div>
                            }}
                        </div>
                    </div>
                    </div>
                })}
            </div>
        }.into_any_element()
    }

    pub(super) fn search(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div class="flex flex-1 min-h-0 border rounded-xl overflow-hidden bg-white" border_color={rgb(LINE)}>
                <div class="w-[320px] flex-shrink-0 flex flex-col min-h-0 border-r bg-[#fcfcfe]" border_color={rgb(LINE)}>
                    <PanelHeader title="搜索结果" count={SharedString::from(self.search_total.to_string())} />
                    <div class="px-4 py-3 border-b" border_color={rgb(LINE)} text_color={rgb(MUTED)} text_size={px(10.)}>{if self.search_text.trim().is_empty() { "在顶部搜索框输入关键词".into() } else { format!("“{}” · 展示 {} 个匹配行", self.search_text, self.search_hits.len()) }}</div>
                    <div id="text-search-results" class="flex-1 min-h-0 overflow-y-scroll">
                        {for (index, hit) in self.search_hits.iter().enumerate() {
                            <div id={("text-hit", index)} class="px-4 py-3 border-b cursor-pointer" border_color={rgb(LINE)} hoverClass="bg-[#f3f6fa]"
                                whenClass={(self.source_file == hit.file && self.source_line == hit.line, "bg-[#edf5f0]")}
                                onClick={cx.listener(move |this, _, _, cx| { let hit = &this.search_hits[index]; this.open_location(hit.file.clone(), hit.line, true, cx); })}>
                                <div class="flex items-center gap-2"><FileBadge path={hit.file.clone()} /><div class="truncate flex-1" text_size={px(11.)}>{hit.file.rsplit('/').next().unwrap_or(&hit.file).to_owned()}</div><Badge label={format!("L{}", hit.line)} /></div>
                                <div class="truncate mt-1" text_size={px(9.)} text_color={rgb(MUTED)}>{hit.file.clone()}</div>
                                <div class="truncate mt-2" font_family="Menlo" text_size={px(10.)}>{hit.text.clone()}</div>
                            </div>
                        }}
                        {...(self.search_total == 0).then(|| rsx! { <EmptyState label={if self.search_text.is_empty() { "输入关键词，开始搜索项目源码" } else { "没有找到匹配的源码" }} /> })}
                    </div>
                </div>
                {self.source_pane(cx)}
            </div>
        }.into_any_element()
    }

    pub(super) fn graph(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let initial = self
            .source_symbol()
            .or_else(|| self.data.symbols.symbols.get(self.selected))
            .cloned();
        let has_more = self.navigation.output["continuation"]["next_cursor"].is_string();
        let truncated = self.navigation.output["truncated"]
            .as_bool()
            .unwrap_or(false);
        let relations = uniform_list("graph-relations", self.navigation.relations.len(), cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let relation = &this.navigation.relations[index];
                rsx! {
                    <div id={("nav-relation", index)} class="h-[44px] px-3 border-b cursor-pointer flex gap-2 items-center bg-white" border_color={rgb(LINE)} hoverClass="bg-[#f3f6fa]"
                        onClick={cx.listener(move |this, _, _, cx| { let location = this.navigation.relations[index].location.clone(); let side = this.navigation_side.clone(); if this.data.verify_location_on(&location, &side).is_ok() { this.open_side(location.file, location.line, &side, true, cx); this.page = Page::Files; } else { this.message = "关联位置与捕获源码不一致".into(); cx.notify(); } })}>
                        <Badge label={relation.kind.clone()} color={GREEN} background={MINT} />
                        <div class="flex-1 min-w-0 truncate" text_size={px(11.)}>{format!("{} → {} · {}:{}", relation.from_name, relation.to_name, relation.location.file, relation.location.line)}</div>
                        <Badge label={format!("{} · {}", relation.certainty, relation.freshness)} color={AMBER} background={0xfbf4dfu32} />
                    </div>
                }
            }).collect::<Vec<_>>()
        }));
        rsx! {
            <div class="flex flex-col flex-1 min-h-0">
                <div class="flex gap-2 items-center mb-3 flex-wrap p-3 bg-white border rounded-xl" border_color={rgb(LINE)}>
                    <ActionButton id="graph-calls" activeIf={!self.references} onClick={cx.listener(|this, _, _, cx| { if !this.navigation_symbol.is_empty() { this.load_side_navigation(this.navigation_symbol.clone(), false, this.navigation_side.clone(), cx); } })}>{"调用图"}</ActionButton>
                    <ActionButton id="graph-refs" activeIf={self.references} onClick={cx.listener(|this, _, _, cx| { if !this.navigation_symbol.is_empty() { this.load_side_navigation(this.navigation_symbol.clone(), true, this.navigation_side.clone(), cx); } })}>{"所有引用"}</ActionButton>
                    {...(!self.references).then(|| rsx! { <ActionButton id="graph-direction" onClick={cx.listener(|this, _, _, cx| { this.direction = (this.direction + 1) % 3; if !this.navigation_symbol.is_empty() { this.load_side_navigation(this.navigation_symbol.clone(), false, this.navigation_side.clone(), cx); } else { cx.notify(); } })}>{["双向", "调用方", "被调用方"][self.direction]}</ActionButton> })}
                    {...(!self.references).then(|| rsx! { <ActionButton id="graph-depth" onClick={cx.listener(|this, _, _, cx| { this.graph_depth = this.graph_depth % 5 + 1; if !this.navigation_symbol.is_empty() { this.load_side_navigation(this.navigation_symbol.clone(), false, this.navigation_side.clone(), cx); } else { cx.notify(); } })}>{format!("深度 {}", self.graph_depth)}</ActionButton> })}
                    {...initial.map(|s| rsx! { <ActionButton id="graph-current" active onClick={cx.listener(move |this, _, _, cx| this.load_navigation(s.symbol_id.clone(), this.references, cx))}>{"分析当前符号"}</ActionButton> })}
                    {...self.navigation.output["continuation"]["next_cursor"].is_string().then(|| rsx! { <ActionButton id="graph-more" disabledIf={self.navigation_busy} onClick={cx.listener(|this, _, _, cx| this.more_navigation(cx))}>{"加载下一页"}</ActionButton> })}
                    {...self.navigation_busy.then(|| rsx! { <ActionButton id="graph-cancel" onClick={cx.listener(|this, _, _, cx| { this.navigation_cancel.cancel(); this.navigation_cancel = client::Cancellation::default(); this.navigation_generation += 1; this.navigation_busy = false; this.navigation_error = "查询已取消，保留已加载结果".into(); cx.notify(); })}>{"取消查询"}</ActionButton> })}
                    {...self.navigation_busy.then(|| muted("正在查询固定快照…"))}
                </div>
                {...(!self.navigation_error.is_empty()).then(|| rsx! { <div base={muted(self.navigation_error.clone())} class="mb-3" text_color={rgb(AMBER)} /> })}
                <div base={muted(format!("已加载 {} 条关联 · {}", self.navigation.relations.len(), if has_more { "还有分页，可继续加载" } else if truncated { "分析范围已截断，请核查覆盖范围" } else { "当前查询分页完成" }))} class="mb-2" />
                {self.graph_canvas(cx)}
                <div class="h-[150px] flex-shrink-0 mt-2 border rounded-lg overflow-hidden" border_color={rgb(LINE)}>{relations}</div>
            </div>
        }.into_any_element()
    }

    pub(super) fn dependencies(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let edges = self.data.dependency_edges();
        let files = self
            .data
            .module_map
            .modules
            .iter()
            .map(|m| m.path.clone())
            .collect::<Vec<_>>();
        let rows = files
            .iter()
            .skip(self.matrix_offset)
            .take(8)
            .collect::<Vec<_>>();
        let columns = files
            .iter()
            .skip(self.matrix_col_offset)
            .take(8)
            .collect::<Vec<_>>();
        let cycles = self.data.dependency_cycles();
        rsx! {
            <div id="dependency-workbench" class="overflow-y-scroll flex-1 min-h-0">
                <div class="p-5 bg-white border rounded-xl" border_color={rgb(LINE)}>
                {heading(format!("模块依赖矩阵 · {} 个源文件", files.len()))}
                <div base={muted("行引用列 · ● 表示当前快照中的候选依赖；可分别翻动行和列。")} class="mt-2 mb-3" />
                <div class="flex gap-2 items-center mb-3">
                    <ActionButton id="dependency-graph" active onClick={cx.listener(|this, _, _, cx| this.show_dependency_graph(cx))}>{"交互依赖图"}</ActionButton>
                    <ActionButton id="matrix-row-prev" onClick={cx.listener(|this, _, _, cx| { this.matrix_offset = this.matrix_offset.saturating_sub(8); cx.notify(); })}>{"行 ←"}</ActionButton>
                    {muted(format!("{}–{}", self.matrix_offset + usize::from(!files.is_empty()), (self.matrix_offset + 8).min(files.len())))}
                    <ActionButton id="matrix-row-next" onClick={cx.listener(|this, _, _, cx| { if this.matrix_offset + 8 < this.data.module_map.modules.len() { this.matrix_offset += 8; } cx.notify(); })}>{"行 →"}</ActionButton>
                    <ActionButton id="matrix-col-prev" onClick={cx.listener(|this, _, _, cx| { this.matrix_col_offset = this.matrix_col_offset.saturating_sub(8); cx.notify(); })}>{"列 ←"}</ActionButton>
                    {muted(format!("{}–{}", self.matrix_col_offset + usize::from(!files.is_empty()), (self.matrix_col_offset + 8).min(files.len())))}
                    <ActionButton id="matrix-col-next" onClick={cx.listener(|this, _, _, cx| { if this.matrix_col_offset + 8 < this.data.module_map.modules.len() { this.matrix_col_offset += 8; } cx.notify(); })}>{"列 →"}</ActionButton>
                </div>
                <div class="border rounded-xl p-4 bg-[#fafbfe]" border_color={rgb(LINE)}>
                    <div class="flex mb-2"><div class="w-[290px] flex-shrink-0">{muted("引用方 / 被引用方")}</div>
                        {for (column, _) in columns.iter().enumerate() { <div class="w-[46px] text-center">{muted((self.matrix_col_offset + column + 1).to_string())}</div> }}
                    </div>
                    {for (index, file) in rows.iter().enumerate() {
                        <div class="flex items-center border-t py-2" border_color={rgb(LINE)}>
                            <div id={("matrix-row", index)} class="w-[290px] flex-shrink-0 truncate cursor-pointer" text_size={px(11.)}
                                onClick={cx.listener({ let file = (*file).clone(); move |this, _, _, cx| { this.open_location(file.clone(), 1, true, cx); this.page = Page::Files; } })}>{format!("{}  {}", self.matrix_offset + index + 1, file)}</div>
                            {for to in &columns {
                                <div class="w-[46px] text-center" text_color={rgb(GREEN)}>{if edges.get(file.as_str()).is_some_and(|targets| targets.contains(to.as_str())) { "●" } else { "·" }}</div>
                            }}
                        </div>
                    }}
                    {for (index, file) in columns.iter().enumerate() { <div base={muted(format!("{}  {}", self.matrix_col_offset + index + 1, file))} class="mt-2" text_size={px(10.)} /> }}
                </div>
                </div>
                <Section title={format!("候选依赖循环 · {} 组", cycles.len())}
                    note={SharedString::from("按已捕获依赖的强连通分量分组；候选绑定可能产生误报。")}>
                    {for (index, cycle) in cycles.iter().enumerate() {
                        <div class="p-3 mb-2 rounded-md border" border_color={rgb(0xe8d9ad)}>
                            <div base={heading(format!("循环组 {}", index + 1))} text_size={px(12.)} />
                            {for file in cycle {
                                <div key={format!("{index}-{file}")} class="py-2 cursor-pointer" text_color={rgb(GREEN)} text_size={px(11.)}
                                    onClick={cx.listener({ let file = file.clone(); move |this, _, _, cx| { this.open_location(file.clone(), 1, true, cx); this.page = Page::Files; } })}>{file.clone()}</div>
                            }}
                        </div>
                    }}
                    {...cycles.is_empty().then(|| rsx! { <EmptyState label="已捕获的候选依赖中没有发现循环。" /> })}
                </Section>
                <Section title="模块间依赖证据">
                    {for dep in &self.data.module_map.dependencies {
                        <div class="p-4 mb-2 border rounded-lg bg-[#fafbfc]" border_color={rgb(LINE)}>
                            <div text_size={px(12.)}>{format!("{} → {}", dep["from"].as_str().unwrap_or("?"), dep["to"].as_str().unwrap_or("?"))}</div>
                            <div base={muted(format!("{} · {}", dep["certainty"].as_str().unwrap_or("candidate"), dep["basis"].as_str().unwrap_or("")))} class="mt-2" text_size={px(10.)} />
                        </div>
                    }}
                </Section>
                <Section title="包 / Cargo 声明依赖">
                    {for dependency in &self.data.map.dependencies { <div class="p-4 mb-2 border rounded-lg bg-[#fafbfc]" border_color={rgb(LINE)}>{muted(dependency.to_string())}</div> }}
                    {...self.data.map.dependencies.is_empty().then(|| rsx! { <EmptyState label="没有捕获到包声明依赖。" /> })}
                </Section>
            </div>
        }.into_any_element()
    }

    pub(super) fn measurements(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let rows = self.data.file_metrics();
        let total_lines = rows.iter().map(|r| r.lines).sum::<usize>();
        let nonblank = rows.iter().map(|r| r.nonblank).sum::<usize>();
        let functions = rows.iter().map(|r| r.functions).sum::<usize>();
        let mut longest = self
            .data
            .symbols
            .symbols
            .iter()
            .filter(|s| s.kind == "function" && s.span.end.line >= s.span.start.line)
            .collect::<Vec<_>>();
        longest.sort_by_key(|s| std::cmp::Reverse(s.span.end.line - s.span.start.line));
        rsx! {
            <div id="measurements-scroll" class="overflow-y-scroll flex-1 min-h-0">
                <div class="flex gap-3 mb-5">
                    {for (label, value) in [("源文件", rows.len()), ("物理行", total_lines), ("非空行", nonblank), ("函数 / 方法", functions)] {
                        <StatCard label={label} value={value} />
                    }}
                </div>
                <div class="p-5 bg-white border rounded-xl" border_color={rgb(LINE)}>
                <div class="flex items-center gap-2"><Icon name="chart" color={GREEN} />{heading("文件度量与 Git 热点")}</div>
                <div base={muted(self.data.churn_scope.clone())} class="mt-2" />
                {...self.data.git_error.as_ref().map(|_| rsx! { <div base={muted("当前项目没有可读取的 Git 历史；改动量显示为 0。")} class="mt-2" text_color={rgb(AMBER)} /> })}
                <div base={muted("按历史增删行总量降序；非空行包含注释，函数数量来自语法声明。")} class="mt-2 mb-3" />
                <div class="flex p-3 rounded-lg bg-[#f3f5f9] font-medium" text_size={px(11.)}>
                    <div class="flex-1 min-w-0">{"文件"}</div>{for label in ["总行", "非空", "函数", "符号", "改动行", "提交触及"] { <div class="w-[64px] flex-shrink-0 text-right">{label}</div> }}
                </div>
                {for (index, row) in rows.iter().enumerate().filter(|(_, r)| self.matches(&r.file)) {
                    <div id={("metric-file", index)} class="flex items-center p-3 border-b cursor-pointer" text_size={px(11.)} border_color={rgb(LINE)} hoverClass="bg-[#f3f6fa]"
                        onClick={cx.listener({ let file = row.file.clone(); move |this, _, _, cx| { this.open_location(file.clone(), 1, true, cx); this.page = Page::Files; } })}>
                        <div class="flex-1 min-w-0 truncate">{row.file.clone()}</div>
                        {for value in [row.lines, row.nonblank, row.functions, row.symbols, row.churn, row.touches] { <div class="w-[64px] flex-shrink-0 text-right">{value.to_string()}</div> }}
                    </div>
                }}
                </div>
                <Section title="最长函数" note={SharedString::from("按声明跨度排序，包含注释与空行。")}>
                {for (index, symbol) in longest.into_iter().take(20).enumerate() {
                    <div id={("long-function", index)} class="p-3 border-b cursor-pointer flex justify-between" border_color={rgb(LINE)} hoverClass="bg-[#f3f6fa]"
                        onClick={cx.listener({ let file = symbol.location.file.clone(); let line = symbol.location.line; move |this, _, _, cx| { this.open_location(file.clone(), line, true, cx); this.page = Page::Files; } })}>
                        <div class="flex-1 min-w-0 flex items-center gap-3"><SymbolBadge kind={symbol.kind.clone()} /><div class="truncate" text_size={px(11.)}>{symbol.qualified_name.clone()}</div></div><Badge label={format!("{} 行", symbol.span.end.line - symbol.span.start.line + 1)} />
                    </div>
                }}
                </Section>
            </div>
        }.into_any_element()
    }

    pub(super) fn bookmarks(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div id="bookmarks-scroll" class="overflow-y-scroll flex-1 min-h-0">
                <div class="flex items-center gap-2 mb-3"><Icon name="bookmark" color={GREEN} /><div class="font-medium" text_size={px(12.)}>{format!("已收藏 {} 个位置", self.session.bookmarks.len())}</div></div>
                <div base={muted("⌘D 收藏当前源码行；书签跨重启保留。文件改动后请核查行号。")} class="mb-4" />
                {for (index, bookmark) in self.session.bookmarks.iter().enumerate().filter(|(_, b)| self.matches(&format!("{} {}", b.file, b.label))) {
                    <div id={("bookmark", index)} class="p-5 mb-3 border rounded-xl bg-white cursor-pointer" border_color={rgb(LINE)} hoverClass="bg-[#f3f6fa]"
                        onClick={cx.listener(move |this, _, _, cx| { let b = this.session.bookmarks[index].clone(); let side = b.effective_side().to_owned(); this.open_side(b.file, b.line, &side, true, cx); this.page = Page::Files; })}>
                        <div class="flex items-center gap-3"><div class="w-[32px] h-[32px] rounded-lg bg-[#eaf3ee] flex items-center justify-center"><Icon name="bookmark" color={GREEN} /></div><div class="truncate flex-1 font-medium" text_size={px(12.)}>{bookmark.file.clone()}</div><Badge label={format!("L{}", bookmark.line)} />
                            <ActionButton id={("remove-bookmark", index)} onClick={cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation(); let previous = this.session.clone(); this.session.bookmarks.remove(index);
                                if let Err(e) = this.session.save(&this.launch.data) { this.session = previous; this.message = format!("保存失败：{e}"); } cx.notify();
                            })}>{"移除"}</ActionButton>
                        </div>
                        <div class="mt-3 px-3 py-2 rounded-lg bg-[#f5f7fa] truncate" font_family="Menlo" text_size={px(11.)}>{bookmark.label.clone()}</div>
                    </div>
                }}
                {...self.session.bookmarks.is_empty().then(|| rsx! { <div class="border rounded-xl bg-white" border_color={rgb(LINE)}><EmptyState label="在源码中点击书签图标，或按 ⌘D 收藏当前位置" /></div> })}
            </div>
        }.into_any_element()
    }
}
