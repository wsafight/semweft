use super::*;
use crate::source::{Identifier, parse_line_number};
use gpui::{HighlightStyle, StyledText};

#[derive(Default)]
pub(super) struct DefinitionState {
    generation: u64,
    location: Option<(String, usize, usize)>,
    label: String,
    busy: bool,
    result: client::Definitions,
    error: String,
}

fn syntax_color(capture: &str) -> u32 {
    match capture {
        "keyword" | "operator" => 0x8c62b5,
        "comment" => 0x909ba7,
        "string" => 0x4a8a63,
        "number" | "constant" => 0xad621f,
        "function" | "constructor" => 0x3c76a8,
        "type" => 0x94602d,
        "attribute" => 0x9b742b,
        _ => 0x3d5065,
    }
}

impl Desktop {
    pub(super) fn invalidate_definition(&mut self) {
        self.definition = DefinitionState {
            generation: self.definition.generation + 1,
            ..Default::default()
        };
    }

    pub(super) fn go_to_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match parse_line_number(self.line_query.read(cx).value(), self.source_lines.len()) {
            Ok(line) => {
                self.open_location(self.source_file.clone(), line, true, cx);
                self.page = Page::Files;
                self.focus.focus(window);
                self.message = format!("已定位到第 {line} 行");
            }
            Err(error) => {
                self.message = error.to_string();
                cx.notify();
            }
        }
    }

    pub(super) fn resolve_identifier(
        &mut self,
        line: usize,
        identifier: Identifier,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_definition();
        self.source_line = line;
        self.definition.label = identifier.label;
        self.definition.location = Some((self.source_file.clone(), line, identifier.column));
        let hash = match self.data.source_hash(&self.source_file) {
            Ok(hash) => hash,
            Err(error) => {
                self.definition.error = error.to_string();
                cx.notify();
                return;
            }
        };
        let query = client::PositionQuery {
            file: self.source_file.clone(),
            line,
            column: identifier.column,
            file_hash: hash,
            snapshot: self.data.symbols.snapshot_id.clone(),
            epoch: self.data.symbols.epochs["semantic"].as_u64().unwrap_or(0),
        };
        let generation = self.definition.generation;
        self.definition.busy = true;
        let repository = self.launch.repository.clone();
        let workspace = self.data.workspace.clone();
        let task = cx
            .background_executor()
            .spawn(async move { client::definitions(&repository, &workspace, &query) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.definition.generation != generation {
                    return;
                }
                this.definition.busy = false;
                match result {
                    Ok(result) => this.definition.result = result,
                    Err(error) => this.definition.error = format!("定义查询失败：{error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn jump_to_definition(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(candidate) = self.definition.result.symbols.get(index) else {
            return;
        };
        let location = candidate.symbol.location.clone();
        if let Err(error) = self.data.verify_location(&location) {
            self.message = format!("无法跳转：{error:#}");
            cx.notify();
            return;
        }
        // Save the call site before the destination so Back returns to the clicked token's line.
        self.open_location(self.source_file.clone(), self.source_line, true, cx);
        self.open_location(location.file, location.line, true, cx);
        self.page = Page::Files;
    }

    fn source_definition_row(&self, index: usize, cx: &Context<Self>) -> gpui::AnyElement {
        let candidate = &self.definition.result.symbols[index];
        rsx! {
            <div class="w-full min-w-0 h-[102px] px-4 py-3 border-b flex flex-col justify-center" border_color={rgb(LINE)}>
                <div class="flex items-center gap-2">
                    <div class="flex-1 min-w-0">
                        <div class="truncate font-semibold" text_size={px(12.)}>{candidate.symbol.qualified_name.clone()}</div>
                        <div base={muted(format!("{}:{} · {}", candidate.symbol.location.file, candidate.symbol.location.line, candidate.symbol.signature))} class="mt-1 truncate" text_size={px(10.)} />
                    </div>
                    <Badge label={if candidate.certainty == "definite" { "已解析" } else { "候选" }} color={if candidate.certainty == "definite" { GREEN } else { AMBER }} />
                    <Badge label={candidate.evidence.iter().filter_map(|e| e["source"].as_str()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(" / ")} />
                </div>
                <div class="flex gap-2 mt-2">
                    <ActionButton id={("definition-jump", index)} compact active onClick={cx.listener(move |this, _, _, cx| this.jump_to_definition(index, cx))}>{"跳转定义"}</ActionButton>
                    <ActionButton id={("definition-calls", index)} compact onClick={cx.listener({ let id = candidate.symbol.symbol_id.clone(); move |this, _, _, cx| this.load_navigation(id.clone(), false, cx) })}>{"调用"}</ActionButton>
                    <ActionButton id={("definition-refs", index)} compact onClick={cx.listener({ let id = candidate.symbol.symbol_id.clone(); move |this, _, _, cx| this.load_navigation(id.clone(), true, cx) })}>{"引用"}</ActionButton>
                </div>
            </div>
        }.into_any_element()
    }

    fn source_definitions(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let Some((file, line, column)) = &self.definition.location else {
            return gpui::Empty.into_any_element();
        };
        let state = &self.definition;
        let candidates = uniform_list(
            "definition-candidates",
            state.result.symbols.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| this.source_definition_row(index, cx))
                    .collect::<Vec<_>>()
            }),
        );
        let empty = !state.busy && state.error.is_empty() && state.result.symbols.is_empty();
        rsx! {
            <div class="h-[220px] flex-shrink-0 flex flex-col bg-white border-t" border_color={rgb(LINE)}>
                <div class="flex items-center justify-between px-4 py-2 border-b" border_color={rgb(LINE)}>
                    <div class="min-w-0">
                        {heading(format!("{} · 定义与候选 {}", state.label, state.result.symbols.len()))}
                        <div base={muted(format!("{file}:{line}:{column} · 候选来自名称匹配，请核查同名与绑定"))} class="mt-1" text_size={px(10.)} />
                    </div>
                    <ActionButton id="close-definitions" compact onClick={cx.listener(|this, _, _, cx| { this.invalidate_definition(); cx.notify(); })}>{"收起"}</ActionButton>
                </div>
                <div class="flex flex-col flex-1 min-h-0"
                    when={(state.busy, |panel| panel.child(rsx! { <EmptyState label="正在查询当前位置…" /> }))}
                    when={(empty, |panel| panel.child(rsx! { <EmptyState label={if state.result.occurrence.is_null() { "当前位置没有捕获的符号或引用。" } else { "已捕获此引用，但没有解析到定义；这不表示定义不存在。" }} /> }))}>
                    {...(!state.error.is_empty()).then(|| rsx! { <EmptyState warning label={state.error.clone()} /> })}
                    <div base={candidates} class="flex-1 min-h-0" />
                    {...state.result.output["truncated"].as_bool().unwrap_or(false).then(|| rsx! { <EmptyState warning label="定义结果已截断，请使用更精确的位置继续核查。" /> })}
                    {...(state.result.completeness["known_gaps"].as_array().is_some_and(|gaps| !gaps.is_empty()) && !empty).then(|| rsx! {
                        <div base={muted("当前分析存在缺口；可在变更审查中查看范围与说明。")} class="px-4 py-2" text_size={px(10.)} />
                    })}
                </div>
            </div>
        }.into_any_element()
    }

    pub(super) fn source_pane(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let bookmarked = self
            .session
            .bookmarks
            .iter()
            .any(|b| b.file == self.source_file && b.line == self.source_line);
        let source_symbol = self.source_symbol().cloned();
        let code = uniform_list("full-source-lines", self.source_code.lines.len(), cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let line = &this.source_code.lines[index];
                let identifiers = line.identifiers.clone();
                let ranges = identifiers.iter().map(|id| id.display_range.clone()).collect::<Vec<_>>();
                let styled = StyledText::new(line.text.clone()).with_highlights(line.highlights.iter().map(|(range, style)| (
                    range.clone(), HighlightStyle { color: Some(rgb(syntax_color(style)).into()), ..Default::default() }
                )));
                rsx! {
                    <div id={("code-line", index)} class="min-w-full flex items-center h-[24px] cursor-pointer" font_family="Menlo" text_size={px(12.)}
                        bg={rgb(if index + 1 == this.source_line { 0xeef6f1 } else { 0xffffff })}
                        onClick={cx.listener(move |this, _, _, cx| {
                            if this.source_line != index + 1 { this.invalidate_definition(); }
                            this.source_line = index + 1; cx.notify();
                        })}>
                        <div class="w-[52px] h-full flex-shrink-0 pr-3 text-right border-r bg-[#fafbfc]" border_color={rgb(LINE)} text_color={rgb(if index + 1 == this.source_line { GREEN } else { 0xa9b2bf })}>{(index + 1).to_string()}</div>
                        <div class="pl-4 pr-6 flex-shrink-0 whitespace-nowrap" text_color={rgb(0x3d5065)}>
                            <CodeText id={("source-token", index)} text={styled} ranges={ranges}
                                onNavigate={cx.processor(move |this, token: usize, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(identifier) = identifiers.get(token) { this.resolve_identifier(index + 1, identifier.clone(), cx); }
                                })} />
                        </div>
                    </div>
                }
            }).collect::<Vec<_>>()
        })).track_scroll(self.source_scroll.clone()).with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained);
        let filename = self
            .source_file
            .rsplit('/')
            .next()
            .unwrap_or("未打开文件")
            .to_owned();
        let parent = self
            .source_file
            .rsplit_once('/')
            .map(|(parent, _)| parent.replace('/', "  /  "))
            .unwrap_or_default();
        rsx! {
            <div class="flex flex-col flex-1 min-w-0 min-h-0 bg-white">
                <div class="h-[44px] flex-shrink-0 flex items-center gap-1 px-2 bg-[#f7f9fc] border-b" border_color={rgb(LINE)}>
                    <IconButton id="history-back" icon="arrow-left" hint="后退  ⌥←" disabledIf={self.history_position == 0} onClick={cx.listener(|this, _, _, cx| this.travel(false, cx))} />
                    <IconButton id="history-forward" icon="arrow-right" hint="前进  ⌥→" disabledIf={self.history_position + 1 >= self.history.len()} onClick={cx.listener(|this, _, _, cx| this.travel(true, cx))} />
                    <div class="ml-2 px-3 h-full max-w-[280px] min-w-0 flex items-center gap-2 bg-white border-t-2 border-[#4d9c7e]">
                        <FileBadge path={self.source_file.clone()} /><div class="truncate font-medium" text_size={px(11.)}>{filename}</div>
                    </div>
                    <div class="flex-1" />
                    <IconButton id="source-copy" icon="copy" hint="复制文件内容" disabledIf={self.source_file.is_empty()} onClick={cx.listener(|this, _, _, cx| {
                        if let Some(text) = this.data.sources.get(&this.source_file) { cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone())); }
                    })} />
                    <IconButton id="source-bookmark" icon="bookmark" hint="切换当前行书签  ⌘D" activeIf={bookmarked} disabledIf={self.source_file.is_empty()} onClick={cx.listener(|this, _, _, cx| this.toggle_bookmark(cx))} />
                    {...(self.page == Page::Files).then(|| rsx! { <IconButton id="toggle-outline" icon="panel" hint="显示或隐藏文件大纲" activeIf={self.outline_visible} onClick={cx.listener(|this, _, _, cx| { this.outline_visible = !this.outline_visible; cx.notify(); })} /> })}
                </div>
                <div class="h-[38px] px-4 flex-shrink-0 flex items-center gap-2 bg-white border-b" border_color={rgb(LINE)}>
                    <div class="flex-1 min-w-0 flex items-center gap-2" text_size={px(10.)} text_color={rgb(MUTED)}>
                        <Icon name="folder" size={12u32} /><div class="truncate">{if parent.is_empty() { "项目根目录".into() } else { parent }}</div>
                    </div>
                    {...source_symbol.map(|symbol| rsx! {
                        <div class="flex gap-1">
                            <ActionButton id="source-calls" compact onClick={cx.listener({ let id = symbol.symbol_id.clone(); move |this, _, _, cx| this.load_navigation(id.clone(), false, cx) })}>{"调用"}</ActionButton>
                            <ActionButton id="source-refs" compact onClick={cx.listener(move |this, _, _, cx| this.load_navigation(symbol.symbol_id.clone(), true, cx))}>{"引用"}</ActionButton>
                        </div>
                    })}
                    <div class="w-[48px]">{self.line_query.clone()}</div>
                    <ActionButton id="source-goto-line" compact disabledIf={self.source_file.is_empty()} onClick={cx.listener(|this, _, window, cx| this.go_to_line(window, cx))}>{"定位"}</ActionButton>
                </div>
                <div class="flex-1 min-h-0"><div base={code} class="size-full" /></div>
                {...self.source_code.notice.as_ref().map(|notice| rsx! { <EmptyState warning label={notice.clone()} /> })}
                {...self.source_file.is_empty().then(|| rsx! { <EmptyState label="该项目没有捕获的源文件；请打开项目或刷新。" /> })}
                {self.source_definitions(cx)}
            </div>
        }.into_any_element()
    }
}

// Runs only for --check-ui. Exercise the async consumer and the rendered panel,
// then dispatch the line-navigation action through the native action tree.
pub(super) fn check_source_navigation(
    view: Entity<Desktop>,
    step: usize,
    retries: usize,
    window: &mut Window,
) {
    window.on_next_frame(move |window, cx| {
        match step {
            0 => {
                view.update(cx, |this, cx| {
                    let symbol = this.data.symbols.symbols.iter().find(|symbol| symbol.kind == "function" && this.data.verify_location(&symbol.location).is_ok()).cloned().expect("No captured function for source navigation check");
                    this.open_location(symbol.location.file, symbol.location.line, true, cx);
                    let identifier = this.source_code.lines[symbol.location.line - 1].identifiers.iter().find(|id| id.column == symbol.location.column).cloned().expect("Function declaration is not clickable");
                    this.resolve_identifier(symbol.location.line, identifier, cx);
                });
            }
            1 => {
                if view.read(cx).definition.busy {
                    assert!(retries < 3600, "Definition query did not finish");
                    check_source_navigation(view, step, retries + 1, window);
                    return;
                }
                let state = &view.read(cx).definition;
                assert!(state.error.is_empty(), "{}", state.error);
                assert!(!state.result.symbols.is_empty(), "Missing definition candidates");
                assert!(state.result.symbols.iter().all(|candidate| candidate.certainty == "definite"));
                // Keep the result panel on screen for a frame before navigating.
            }
            2 => {
                view.update(cx, |this, cx| this.jump_to_definition(0, cx));
                window.dispatch_action(Box::new(FocusLine), cx);
            }
            3 => {
                use gpui::EntityInputHandler;
                assert!(view.read(cx).line_query.focus_handle(cx).is_focused(window));
                let input = view.read(cx).line_query.clone();
                input.update(cx, |input, cx| input.replace_text_in_range(None, "1", window, cx));
                window.dispatch_action(Box::new(GoToLine), cx);
            }
            _ => {
                assert_eq!(view.read(cx).source_line, 1);
                assert!(view.read(cx).focus.is_focused(window));
                println!("Native UI check: 9 pages and conditional states including Base/Head rendered; RSX actions, CJK/emoji IME, async definitions, definition jump and line navigation verified");
                cx.quit();
                return;
            }
        }
        check_source_navigation(view, step + 1, 0, window);
    });
    window.refresh();
}
