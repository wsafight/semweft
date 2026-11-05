use super::*;

#[derive(Default)]
pub(super) struct AnalysisState {
    pub cancellation: client::Cancellation,
    pub generation: u64,
    busy: bool,
    tool: usize,
    report: Value,
    error: String,
    selected: usize,
    seen: BTreeSet<String>,
    review: Value,
    evidence: String,
}

const TOOLS: [&str; 3] = ["verify_change", "suggest_tests", "arch_check"];
const FIELDS: [&str; 10] = [
    "findings",
    "test_candidates",
    "relations",
    "relation_changes.added",
    "relation_changes.removed",
    "affected_modules",
    "api_changes",
    "affected_services",
    "modules",
    "dependencies",
];

impl AnalysisState {
    fn refresh_evidence(&mut self) {
        let report = &self.report;
        let list_key = if self.tool == 1 {
            "test_candidates"
        } else {
            "findings"
        };
        let item = &report[list_key][self.selected];
        self.evidence = serde_json::to_string_pretty(&json!({"selected":item,"completeness":report["completeness"],"verification":report["verification"],"test_summary":report["test_summary"],"architecture":report["architecture"],"output":report["output"]})).unwrap_or_default();
    }

    pub(super) fn is_request(&self) -> bool {
        self.review["kind"] == "request"
    }
    pub(super) fn reset(&mut self) {
        self.cancellation.cancel();
        *self = Self {
            generation: self.generation + 1,
            ..Default::default()
        };
    }
}

impl Desktop {
    pub(super) fn install_review(&mut self, review: Value, cx: &mut Context<Self>) {
        self.analysis.reset();
        self.analysis.report = review["report"].clone();
        self.analysis.review = review;
        self.analysis.refresh_evidence();
        self.page = Page::Analysis;
        cx.notify();
    }

    pub(super) fn review_action(
        &mut self,
        action: &'static str,
        item: Option<String>,
        conclusion: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.analysis.busy {
            return;
        }
        let review = &self.analysis.review;
        let mut args = if action == "create" {
            json!({"action":"create","report":self.analysis.report,"title":format!("{} · 变更审查", self.data.name)})
        } else {
            json!({"action":"decide","review_id":review["review_id"],"revision":review["revision"],"item_id":item,
                "conclusion":conclusion,"reviewer":std::env::var("USER").unwrap_or_else(|_| "local user".into()),"note":self.review_note.read(cx).value()})
        };
        if item.as_deref() == Some("source") {
            args["note"] = json!(self.source_note.read(cx).value());
            if let Some(source) = self
                .data
                .sources_on(&self.source_side)
                .get(&self.source_file)
            {
                args["location"] = json!({"side":self.source_side,"file":self.source_file,"line":self.source_line,"file_hash":semweft_shared::digest(source)});
            }
        }
        self.analysis.busy = true;
        self.analysis.error.clear();
        let client = self.client.clone();
        let cancellation = self.analysis.cancellation.clone();
        let generation = self.analysis.generation;
        let task = cx
            .background_executor()
            .spawn(async move { client.call("__review_request", args, &cancellation) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.analysis.generation != generation {
                    return;
                }
                this.analysis.busy = false;
                match result {
                    Ok(review) => {
                        this.message = format!(
                            "审查已保存 · {} · 修订 {}",
                            review["state"].as_str().unwrap_or(""),
                            review["revision"]
                        );
                        this.analysis.review = review;
                    }
                    Err(error) => {
                        this.analysis.error = format!("审查未保存：{error:#}；请刷新审查状态后重试")
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn reload_review(&mut self, cx: &mut Context<Self>) {
        if self.analysis.busy {
            return;
        }
        let Some(id) = self.analysis.review["review_id"]
            .as_str()
            .map(str::to_owned)
        else {
            return;
        };
        self.analysis.busy = true;
        let client = self.client.clone();
        let cancellation = self.analysis.cancellation.clone();
        let generation = self.analysis.generation;
        let task = cx.background_executor().spawn(async move {
            client.call(
                "get_review_result",
                json!({"review_id":id,"timeout_ms":60000}),
                &cancellation,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.analysis.generation != generation {
                    return;
                }
                this.analysis.busy = false;
                match result {
                    Ok(value) => {
                        this.analysis.review = value["review"].clone();
                        this.analysis.error.clear();
                    }
                    Err(error) => this.analysis.error = format!("状态刷新失败：{error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn check_analysis(&mut self, tool: usize, cx: &mut Context<Self>) {
        self.run_analysis(tool, false, cx);
    }
    fn run_analysis(&mut self, tool: usize, more: bool, cx: &mut Context<Self>) {
        if self.analysis.busy {
            return;
        }
        let args = if more {
            let Some(cursor) = self.analysis.report["output"]["continuation"]["next_cursor"]
                .as_str()
                .map(str::to_owned)
            else {
                return;
            };
            if !self.analysis.seen.insert(cursor.clone()) {
                self.analysis.error = "分析服务重复了游标，请重新运行检查".into();
                cx.notify();
                return;
            }
            json!({"cursor":cursor,"timeout_ms":60000})
        } else {
            let Some(diff) = self.data.fixed_diff() else {
                self.analysis.error = "此视图缺少固定 Base/Head，请刷新 Git 项目".into();
                cx.notify();
                return;
            };
            self.analysis.cancellation.cancel();
            let generation = self.analysis.generation + 1;
            self.analysis = AnalysisState {
                generation,
                tool,
                ..Default::default()
            };
            let mut args = json!({"diff":diff,"limit":100,"timeout_ms":60000});
            if tool != 2 {
                args["depth"] = json!(5);
                args["max_nodes"] = json!(5000);
            }
            args
        };
        self.analysis.busy = true;
        self.analysis.error.clear();
        let generation = self.analysis.generation;
        let client = self.client.clone();
        let cancellation = self.analysis.cancellation.clone();
        let snapshot = self.data.symbols.snapshot_id.clone();
        let epoch = self.data.symbols.epochs["semantic"].as_u64().unwrap_or(0);
        let task = cx
            .background_executor()
            .spawn(async move { client.page(TOOLS[tool], args, &snapshot, epoch, &cancellation) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.analysis.generation != generation {
                    return;
                }
                this.analysis.busy = false;
                match result {
                    Ok(page) => {
                        if more {
                            client::merge_page(&mut this.analysis.report, page, &FIELDS);
                        } else {
                            this.analysis.report = page;
                        }
                        this.analysis.refresh_evidence();
                    }
                    Err(error) => this.analysis.error = format!("检查未完成：{error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn record_review(&mut self, conclusion: &'static str, cx: &mut Context<Self>) {
        if self.analysis.review["kind"] == "request" {
            self.review_action("decide", None, conclusion, cx);
            return;
        }
        if self.analysis.busy
            || !self.analysis.report["verification"].is_object()
            || self.analysis.report["output"]["continuation"]["next_cursor"].is_string()
        {
            return;
        }
        self.analysis.busy = true;
        let client = self.client.clone();
        let cancellation = self.analysis.cancellation.clone();
        let generation = self.analysis.generation;
        let args = json!({"report":self.analysis.report,"conclusion":conclusion,"note":self.review_note.read(cx).value()});
        let task = cx
            .background_executor()
            .spawn(async move { client.call("__desktop_review", args, &cancellation) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.analysis.generation != generation {
                    return;
                }
                this.analysis.busy = false;
                match result {
                    Ok(record) => {
                        this.message = format!(
                            "人工结论已保存：{}",
                            record["review_id"].as_str().unwrap_or_default()
                        );
                        this.analysis.review = record;
                    }
                    Err(error) => this.analysis.error = format!("保存失败：{error:#}"),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn analysis_export(&self, cx: &mut Context<Self>) {
        let value = json!({"type":"semweft_desktop_analysis","tool":TOOLS[self.analysis.tool],"report":self.analysis.report,"human_review":self.analysis.review,"source_notes":self.session.source_notes});
        let prompt = cx.prompt_for_new_path(&self.data.workspace, Some("semweft-analysis.json"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = prompt.await {
                let result = serde_json::to_vec_pretty(&value)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(fs::write(&path, bytes)?));
                let _ = this.update(cx, |this, cx| {
                    this.message = match result {
                        Ok(()) => format!("已导出分析证据：{}", path.display()),
                        Err(error) => error.to_string(),
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn analysis_panel(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let report = &self.analysis.report;
        let list_key = if self.analysis.tool == 1 {
            "test_candidates"
        } else {
            "findings"
        };
        let count = report[list_key].as_array().map(Vec::len).unwrap_or(0);
        let rows = uniform_list("analysis-results", count, cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let item = &this.analysis.report[list_key][index];
                let title = if list_key == "test_candidates" { item["test"]["qualified_name"].as_str().unwrap_or("测试候选") } else { item["message"].as_str().unwrap_or("规则发现") };
                let detail = if list_key == "test_candidates" { format!("{} · {} · 未执行", item["availability"].as_str().unwrap_or(""), item["certainty"].as_str().unwrap_or("candidate")) } else { format!("{} · {} · {}", item["rule_id"].as_str().unwrap_or(""), item["severity"].as_str().unwrap_or(""), item["status"].as_str().unwrap_or("")) };
                rsx! { <div id={("analysis-row", index)} class="h-[68px] p-3 border-b cursor-pointer" border_color={rgb(LINE)} whenClass={(this.analysis.selected == index, "bg-[#edf5f0]")} hoverClass="bg-[#f3f6fa]" onClick={cx.listener(move |this, _, _, cx| { this.analysis.selected = index; this.analysis.refresh_evidence(); cx.notify(); })}>
                    <div class="truncate" text_size={px(12.)}>{title.to_owned()}</div><div base={muted(detail)} class="truncate mt-2" text_size={px(10.)} />
                </div> }
            }).collect::<Vec<_>>()
        }));
        let item = &report[list_key][self.analysis.selected];
        let location_value = if list_key == "test_candidates" {
            &item["test"]["location"]
        } else {
            &item["evidence"]["location"]
        };
        let location =
            serde_json::from_value::<crate::model::Location>(location_value.clone()).ok();
        let side = if item["availability"] == "base_only" || item["evidence"]["side"] == "base" {
            "base"
        } else {
            "head"
        };
        let partial = report["output"]["continuation"]["next_cursor"].is_string();
        let can_review =
            report["verification"].is_object() && !partial && !self.analysis.busy && !self.busy;
        let request = self.analysis.review["kind"] == "request";
        let item_id = self.analysis.review["items"]
            .as_array()
            .and_then(|items| items.get(self.analysis.selected))
            .and_then(|item| item["item_id"].as_str())
            .map(str::to_owned);
        rsx! {
            <div class="flex flex-col flex-1 min-h-0">
                <div class="p-3 bg-white border rounded-xl flex items-center gap-2 flex-wrap mb-3" border_color={rgb(LINE)}>
                    {for (index, label) in ["验证变更", "推荐测试", "架构检查"].into_iter().enumerate() { <ActionButton id={("analysis-tool", index)} activeIf={self.analysis.tool == index} disabledIf={self.analysis.busy || self.busy} onClick={cx.listener(move |this, _, _, cx| this.run_analysis(index, false, cx))}>{label}</ActionButton> }}
                    {...partial.then(|| rsx! { <ActionButton id="analysis-more" disabledIf={self.analysis.busy} onClick={cx.listener(|this, _, _, cx| this.run_analysis(this.analysis.tool, true, cx))}>{"加载下一页"}</ActionButton> })}
                    {...self.analysis.busy.then(|| rsx! { <ActionButton id="analysis-cancel" onClick={cx.listener(|this, _, _, cx| { cancel_page(&mut this.analysis.cancellation, &mut this.analysis.generation, &mut this.analysis.busy, &mut this.analysis.error, "已取消，保留已加载证据；请重新运行检查后再保存结论"); cx.notify(); })}>{"取消"}</ActionButton> })}
                    <ActionButton id="analysis-export" disabledIf={report.is_null()} onClick={cx.listener(|this, _, _, cx| this.analysis_export(cx))}>{"导出证据"}</ActionButton>
                    {muted(if self.analysis.busy { "查询固定 Base/Head…".into() } else { format!("{} · 已加载 {} 项{}", report["verification"]["status"].as_str().unwrap_or("尚未运行"), count, if partial { " · 分页未完成" } else { "" }) })}
                </div>
                {...(!self.analysis.error.is_empty()).then(|| rsx! { <div base={muted(self.analysis.error.clone())} class="mb-2" text_color={rgb(AMBER)} /> })}
                <div class="flex flex-1 min-h-0 bg-white border rounded-xl overflow-hidden" border_color={rgb(LINE)}>
                    <div class="min-w-0 min-h-0 flex flex-col border-r" w={gpui::relative(0.48)} border_color={rgb(LINE)}>{rows}{...(count == 0).then(|| rsx! { <EmptyState label="当前页没有发现或测试候选；请同时检查覆盖缺口。" /> })}</div>
                    <div id="analysis-evidence" class="flex-1 min-w-0 overflow-y-scroll p-4">
                        {heading("证据与覆盖范围")}
                        <div class="flex gap-2 my-3">
                            {...location.map(|location| rsx! { <ActionButton id="analysis-source" onClick={cx.listener(move |this, _, _, cx| { if this.data.verify_location_on(&location, side).is_ok() { this.open_side(location.file.clone(), location.line, side, true, cx); this.page = Page::Files; } else { this.message = "证据位置缺少匹配的源码身份".into(); cx.notify(); } })}>{"打开证据源码"}</ActionButton> })}
                        </div>
                        <div font_family="Menlo" text_size={px(10.)}>{for line in self.analysis.evidence.lines() { <div>{line.to_owned()}</div> }}</div>
                        {...request.then(|| rsx! { <div class="mt-4">{heading("审查意见")}
                            {for event in self.analysis.review["events"].as_array().into_iter().flatten().rev().take(30) {
                                <div class="py-2 border-b" border_color={rgb(LINE)}><div class="font-medium">{format!("{} · {} · 修订 {}", event["reviewer"].as_str().unwrap_or(""), event["conclusion"].as_str().unwrap_or(""), event["revision"])}</div><div>{event["note"].as_str().unwrap_or("").to_owned()}</div><div base={muted(event["location"].to_string())} /></div>
                            }}
                        </div> })}
                    </div>
                </div>
                <div class="p-3 mt-3 bg-white border rounded-xl" border_color={rgb(LINE)}>
                    <div class="flex items-center gap-2 mb-2">
                        <ActionButton id="review-create" disabledIf={!can_review} onClick={cx.listener(|this, _, _, cx| this.review_action("create", None, "comment", cx))}>{"创建审查请求"}</ActionButton>
                        {...request.then(|| rsx! { <ActionButton id="review-reload" disabledIf={self.analysis.busy} onClick={cx.listener(|this, _, _, cx| this.reload_review(cx))}>{"刷新审查状态"}</ActionButton> })}
                        {muted(format!("{} · 修订 {}", self.analysis.review["state"].as_str().unwrap_or("尚未创建请求"), self.analysis.review["revision"].as_u64().unwrap_or(0)))}
                    </div>
                    {...(request && list_key == "findings").then(|| rsx! { <div class="flex gap-2 mb-2">
                        {for (decision, label) in [("accept", "确认当前发现"), ("dismiss", "驳回当前发现"), ("comment", "添加条目意见")] {
                            <ActionButton id={SharedString::from(format!("finding-{decision}"))} disabledIf={!can_review || item_id.is_none()} onClick={cx.listener({ let item_id = item_id.clone(); move |this, _, _, cx| this.review_action("decide", item_id.clone(), decision, cx) })}>{label}</ActionButton>
                        }}
                    </div> })}
                    <div class="h-[28px] mb-2">{self.review_note.clone()}</div>
                    <div class="flex gap-2 items-center">
                        {for (conclusion, label) in [("approve", "人工认可"), ("reject", "需要修改"), ("comment", "记录已审阅")] { <ActionButton id={SharedString::from(format!("review-{conclusion}"))} disabledIf={!can_review} onClick={cx.listener(move |this, _, _, cx| this.record_review(conclusion, cx))}>{label}</ActionButton> }}
                        <div base={muted(self.analysis.review["review_id"].as_str().unwrap_or("").to_owned())} class="truncate flex-1" />
                    </div>
                    <div base={muted("请求需逐项处理后认可；意见绑定固定报告与修订。Agent 可读取 get_review_result，或 review wait 等待变化。人工认可不会提升分析确定性。")} class="mt-2" />
                </div>
            </div>
        }.into_any_element()
    }
}

pub(super) fn check_analysis_views(
    view: Entity<Desktop>,
    tool: usize,
    retries: usize,
    window: &mut Window,
) {
    window.on_next_frame(move |window, cx| {
        if view.read(cx).analysis.busy {
            assert!(retries < 3600, "Analysis query did not finish");
            check_analysis_views(view, tool, retries + 1, window);
            return;
        }
        if retries > 0 {
            let analysis = &view.read(cx).analysis;
            assert!(analysis.error.is_empty(), "{}", analysis.error);
            assert!(analysis.report["snapshot_id"].is_string());
        }
        if tool < 3 {
            view.update(cx, |this, cx| {
                this.page = Page::Analysis;
                this.check_analysis(tool, cx);
            });
            check_analysis_views(view, tool + 1, 1, window);
        } else {
            view.update(cx, |this, cx| {
                let symbol = this
                    .data
                    .base_symbols
                    .symbols
                    .iter()
                    .find(|symbol| symbol.kind == "function")
                    .cloned()
                    .expect("Missing Base function");
                this.open_side(
                    symbol.location.file.clone(),
                    symbol.location.line,
                    "base",
                    true,
                    cx,
                );
                assert_eq!(this.source_side, "base");
                assert_eq!(
                    this.source_code.lines.len(),
                    this.data.base_sources[&symbol.location.file]
                        .lines()
                        .count()
                );
                assert!(this.session.tabs.iter().any(|tab| tab.side == "base"));
                this.travel(false, cx);
            });
            super::explorer::check_explorer_views(view.clone(), 0, false, 0, window);
        }
    });
    window.refresh();
}
