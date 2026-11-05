use super::*;

const TOOLS: [(&str, &str); 7] = [
    ("find_paths", "关系路径"),
    ("find_implementations", "Trait 实现"),
    ("search_symbols", "公开 API"),
    ("impact_of", "变更风险"),
    ("contract_impact", "Protobuf 契约"),
    ("repo_map", "服务依赖与循环"),
    ("agent_preflight", "修改前检查"),
];
const FIELDS: [&str; 18] = [
    "paths",
    "relations",
    "symbols",
    "modules",
    "dependencies",
    "entries",
    "cycles",
    "contract_changes",
    "contract_impacts",
    "findings",
    "changes",
    "relation_changes.added",
    "relation_changes.removed",
    "affected_modules",
    "affected_services",
    "api_changes",
    "selected_symbols",
    "protected_regions",
];

#[derive(Default)]
pub(super) struct Explorer {
    tool: usize,
    scope: usize,
    busy: bool,
    generation: u64,
    pub(super) cancellation: client::Cancellation,
    report: Value,
    rows: Vec<(String, Value)>,
    selected: usize,
    error: String,
    error_code: String,
    seen: BTreeSet<String>,
    risk_inputs: Option<Value>,
    risk_path: String,
    side: String,
    evidence: String,
}

impl Explorer {
    fn refresh_evidence(&mut self) {
        let report = &self.report;
        let selected = self
            .rows
            .get(self.selected)
            .map(|(_, value)| value)
            .unwrap_or(&Value::Null);
        self.evidence = serde_json::to_string_pretty(&json!({"selected":selected,"risk_summary":report["risk_summary"],"api_summary":report["api_summary"],"contract_summary":report["contract_summary"],"implementation_counts":report["implementation_counts"],"guard":report["guard"],"completeness":report["completeness"],"output":report["output"]})).unwrap_or_default();
    }

    pub(super) fn reset(&mut self) {
        self.cancellation.cancel();
        *self = Self {
            generation: self.generation + 1,
            ..Default::default()
        };
    }
}

fn query_binding(
    tool: usize,
    symbols: &crate::model::Symbols,
    diff: Option<Value>,
) -> Result<Value> {
    let mut args = json!({"limit":100,"timeout_ms":60000});
    if matches!(tool, 3 | 4) {
        args["diff"] = diff.context("此项目没有固定的 Base/Head 变更视图")?;
    } else {
        args["snapshot_id"] = json!(symbols.snapshot_id);
        args["semantic_epoch"] = symbols.epochs["semantic"].clone();
    }
    Ok(args)
}

impl Desktop {
    pub(super) fn query_explorer(&mut self, more: bool, cx: &mut Context<Self>) {
        if self.explorer.busy || self.busy {
            return;
        }
        let tool = self.explorer.tool;
        let args = if more {
            let Some(cursor) = self.explorer.report["output"]["continuation"]["next_cursor"]
                .as_str()
                .map(str::to_owned)
            else {
                return;
            };
            if !self.explorer.seen.insert(cursor.clone()) {
                self.explorer.error = "分页游标重复，请重新运行".into();
                cx.notify();
                return;
            }
            json!({"cursor":cursor,"timeout_ms":60000})
        } else {
            let side = if matches!(tool, 3 | 4) {
                "head".into()
            } else {
                self.source_side.clone()
            };
            let symbols = self.data.symbols_on(&side);
            let mut args = match query_binding(tool, symbols, self.data.fixed_diff()) {
                Ok(args) => args,
                Err(error) => {
                    self.explorer.error = error.to_string();
                    cx.notify();
                    return;
                }
            };
            let input = self.explore_from.read(cx).value().trim();
            let target = self.explore_to.read(cx).value().trim();
            match tool {
                0 => {
                    if input.is_empty() || target.is_empty() {
                        self.explorer.error = "请填写起点和终点的符号、文件路径或服务名".into();
                        cx.notify();
                        return;
                    }
                    args["from"] = json!(input);
                    args["to"] = json!(target);
                    args["scope"] = json!(["symbol", "module", "service"][self.explorer.scope]);
                    args["max_depth"] = json!(8);
                    args["max_paths"] = json!(100);
                    args["max_nodes"] = json!(5000);
                }
                1 => {
                    if input.is_empty() {
                        self.explorer.error = "请输入 Rust trait 或 trait 方法".into();
                        cx.notify();
                        return;
                    }
                    args["symbol"] = json!(input);
                }
                2 => {
                    args["query"] = json!(if input.is_empty() { "::" } else { input });
                    args["api_scope"] = json!("external");
                }
                3 | 4 => {
                    args["depth"] = json!(5);
                    args["max_nodes"] = json!(5000);
                    if tool == 3
                        && let Some(risk) = &self.explorer.risk_inputs
                    {
                        args["risk_inputs"] = risk.clone();
                    }
                }
                5 => {
                    args["dependencies"] = json!("service");
                    args["cycles"] = json!(true);
                    args["depth"] = json!(10);
                    args["max_symbols_per_module"] = json!(0);
                }
                _ => {
                    if input.is_empty() {
                        args["files"] = json!([self.source_file]);
                    } else {
                        args["symbols"] = json!([input]);
                    }
                    args["depth"] = json!(5);
                    args["max_nodes"] = json!(5000);
                }
            }
            self.explorer.cancellation.cancel();
            self.explorer.cancellation = client::Cancellation::default();
            self.explorer.generation += 1;
            self.explorer.seen.clear();
            self.explorer.selected = 0;
            self.explorer.side = side;
            args
        };
        self.explorer.busy = true;
        self.explorer.error.clear();
        self.explorer.error_code.clear();
        let generation = self.explorer.generation;
        let client = self.client.clone();
        let cancellation = self.explorer.cancellation.clone();
        let binding = self.data.symbols_on(&self.explorer.side);
        let snapshot = binding.snapshot_id.clone();
        let epoch = binding.epochs["semantic"].as_u64().unwrap_or(0);
        let task = cx.background_executor().spawn(async move {
            client.page(TOOLS[tool].0, args, &snapshot, epoch, &cancellation)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.explorer.generation != generation {
                    return;
                }
                this.explorer.busy = false;
                match result {
                    Ok(page) => {
                        if more {
                            client::merge_page(&mut this.explorer.report, page, &FIELDS);
                        } else {
                            this.explorer.report = page;
                        }
                        this.explorer.rows = FIELDS
                            .iter()
                            .flat_map(|field| {
                                client::page_values(&this.explorer.report, field)
                                    .map(move |value| ((*field).to_owned(), value.clone()))
                            })
                            .collect();
                        this.explorer.refresh_evidence();
                    }
                    Err(error) => {
                        this.explorer.error_code =
                            serde_json::from_str::<Value>(&error.to_string())
                                .ok()
                                .and_then(|value| value["code"].as_str().map(str::to_owned))
                                .unwrap_or_default();
                        this.explorer.error = format!("查询未完成：{error:#}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn explorer_risk_inputs(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择源码绑定的风险观测 JSON".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await
                && let Some(path) = paths.first()
            {
                let result = fs::metadata(path)
                    .and_then(|metadata| {
                        if metadata.len() > 32 * 1024 * 1024 {
                            return Err(std::io::Error::other("风险输入超过 32MiB"));
                        }
                        fs::read(path)
                    })
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?));
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(value) => {
                            this.explorer.risk_inputs = Some(if value.get("head").is_some() {
                                value
                            } else {
                                json!({"head":value})
                            });
                            this.explorer.risk_path = path.display().to_string();
                            this.explorer.error.clear();
                        }
                        Err(error) => this.explorer.error = format!("观测读取失败：{error:#}"),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn explorer_export(&self, cx: &mut Context<Self>) {
        let value = json!({"type":"semweft_desktop_query","tool":TOOLS[self.explorer.tool].0,"report":self.explorer.report});
        let prompt = cx.prompt_for_new_path(&self.data.workspace, Some("semweft-query.json"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = prompt.await {
                let result = serde_json::to_vec_pretty(&value)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(fs::write(&path, bytes)?));
                let _ = this.update(cx, |this, cx| {
                    this.message = match result {
                        Ok(()) => format!("已导出 {}", path.display()),
                        Err(error) => error.to_string(),
                    };
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn explorer_panel(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let report = &self.explorer.report;
        let selected = self
            .explorer
            .rows
            .get(self.explorer.selected)
            .map(|(_, value)| value)
            .unwrap_or(&Value::Null);
        let locations = locations(selected, &self.explorer.side);
        let rows = uniform_list("explorer-results", self.explorer.rows.len(), cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let (field, item) = &this.explorer.rows[index];
                let title = ["qualified_name", "name", "message", "path", "kind"].into_iter().find_map(|key| item[key].as_str()).map(str::to_owned)
                    .unwrap_or_else(|| format!("{} → {}", item["from"].as_str().unwrap_or("关系"), item["to"].as_str().unwrap_or("详情")));
                rsx! { <div id={("explorer-result", index)} class="h-[68px] p-3 border-b cursor-pointer" border_color={rgb(LINE)} whenClass={(index == this.explorer.selected, "bg-[#e9f4ef]")} hoverClass="bg-[#f7f9fc]"
                    onClick={cx.listener(move |this, _, _, cx| { this.explorer.selected = index; this.explorer.refresh_evidence(); cx.notify(); })}>
                    <div class="truncate font-medium">{title}</div><div base={muted(format!("{} · {}", field, item["certainty"].as_str().unwrap_or("按证据核查")))} class="mt-1 truncate" />
                </div> }.into_any_element()
            }).collect::<Vec<_>>()
        }));
        rsx! { <div class="flex flex-col flex-1 min-h-0">
            <div class="flex flex-wrap gap-2 mb-2">{for (index, (_, label)) in TOOLS.into_iter().enumerate() { <ActionButton id={("explorer-tool", index)} compact activeIf={index == self.explorer.tool} disabledIf={self.explorer.busy} onClick={cx.listener(move |this, _, _, cx| { this.explorer.tool = index; this.explorer.report = Value::Null; this.explorer.rows.clear(); this.explorer.error.clear(); this.explorer.refresh_evidence(); cx.notify(); })}>{label}</ActionButton> }}</div>
            <div class="flex gap-2 items-center p-3 bg-white border rounded-xl mb-2" border_color={rgb(LINE)}>
                <div class="flex-1 h-[28px]">{self.explore_from.clone()}</div>
                {...(self.explorer.tool == 0).then(|| rsx! { <div class="flex-1 h-[28px]">{self.explore_to.clone()}</div> })}
                <ActionButton id="explorer-run" disabledIf={self.explorer.busy || self.busy} onClick={cx.listener(|this, _, _, cx| this.query_explorer(false, cx))}>{"运行查询"}</ActionButton>
                <ActionButton id="explorer-current" compact disabledIf={self.explorer.busy} onClick={cx.listener(|this, _, _, cx| { let name = this.source_symbol().map(|symbol| symbol.symbol_id.clone()).unwrap_or_else(|| this.source_file.clone()); this.explore_from.update(cx, |input, cx| input.set_value(name, cx)); })}>{"填入当前符号"}</ActionButton>
            </div>
            {...(self.explorer.tool == 0).then(|| rsx! { <div class="flex gap-2 mb-2">{for (index, label) in ["符号", "文件模块", "显式服务"].into_iter().enumerate() { <ActionButton id={("explorer-scope", index)} compact activeIf={self.explorer.scope == index} disabledIf={self.explorer.busy} onClick={cx.listener(move |this, _, _, cx| { this.explorer.scope = index; cx.notify(); })}>{label}</ActionButton> }}</div> })}
            {...(self.explorer.tool == 3).then(|| rsx! { <div class="flex gap-2 items-center mb-2"><ActionButton id="risk-import" compact onClick={cx.listener(|this, _, _, cx| this.explorer_risk_inputs(cx))}>{"导入覆盖/热点观测"}</ActionButton><ActionButton id="risk-clear" compact onClick={cx.listener(|this, _, _, cx| { this.explorer.risk_inputs = None; this.explorer.risk_path.clear(); cx.notify(); })}>{"清除观测"}</ActionButton><div base={muted(self.explorer.risk_path.clone())} class="truncate flex-1" /></div> })}
            {...(!self.explorer.error.is_empty()).then(|| rsx! { <EmptyState warning label={self.explorer.error.clone()} /> })}
            <div class="flex gap-2 items-center mb-2">
                {muted(format!("{} · {} 项 · {}", if self.explorer.side == "base" { "BASE" } else { "HEAD" }, self.explorer.rows.len(), if self.explorer.busy { "查询中" } else { "固定快照" }))}
                {...report["output"]["continuation"]["next_cursor"].is_string().then(|| rsx! { <ActionButton id="explorer-more" compact disabledIf={self.explorer.busy} onClick={cx.listener(|this, _, _, cx| this.query_explorer(true, cx))}>{"加载下一页"}</ActionButton> })}
                {...self.explorer.busy.then(|| rsx! { <ActionButton id="explorer-cancel" compact onClick={cx.listener(|this, _, _, cx| { this.explorer.cancellation.cancel(); this.explorer.generation += 1; this.explorer.busy = false; this.explorer.error = "已取消；可重新运行".into(); cx.notify(); })}>{"取消"}</ActionButton> })}
                <ActionButton id="explorer-export" compact disabledIf={report.is_null()} onClick={cx.listener(|this, _, _, cx| this.explorer_export(cx))}>{"导出证据"}</ActionButton>
            </div>
            {...report["risk_summary"].is_object().then(|| rsx! { <div class="flex gap-3 mb-2">{...report["risk_summary"]["score"].as_u64().map(|score| rsx! { <StatCard label="风险分数" value={score as usize} /> })}<div base={muted(format!("{} · {} · 缺失观测不代表低风险", report["risk_summary"]["level"].as_str().unwrap_or("未评分"), report["risk_summary"]["confidence"].as_str().unwrap_or("未知置信度")))} /></div> })}
            <div class="flex flex-1 min-h-0 border bg-white rounded-xl overflow-hidden" border_color={rgb(LINE)}>
                <div class="flex flex-col min-h-0 border-r" w={gpui::relative(0.43)} border_color={rgb(LINE)}><div base={rows} class="flex-1 min-h-0" />{...(self.explorer.rows.is_empty()).then(|| rsx! { <EmptyState label="尚无结果；零结果请结合分析范围和缺口核查。" /> })}</div>
                <div id="explorer-evidence" class="flex-1 min-w-0 overflow-y-scroll p-3">
                    {heading("结果、证据与分析范围")}
                    <div class="flex flex-wrap gap-2 my-2">{for (index, (side, location)) in locations.into_iter().enumerate() { <ActionButton id={("explorer-source", index)} compact disabledIf={self.data.verify_location_on(&location, &side).is_err()} onClick={cx.listener({ let location = location.clone(); let side = side.clone(); move |this, _, _, cx| { this.open_side(location.file.clone(), location.line, &side, true, cx); this.page = Page::Files; } })}>{format!("{} {}:{}", side.to_uppercase(), location.file, location.line)}</ActionButton> }}</div>
                    <div font_family="Menlo" text_size={px(10.)}>{for line in self.explorer.evidence.lines() { <div>{line.to_owned()}</div> }}</div>
                </div>
            </div>
        </div> }.into_any_element()
    }
}

fn locations(value: &Value, default_side: &str) -> Vec<(String, crate::model::Location)> {
    fn visit(
        value: &Value,
        side: &str,
        depth: usize,
        found: &mut Vec<(String, crate::model::Location)>,
    ) {
        if depth > 8 || found.len() >= 24 {
            return;
        }
        let side = value["side"]
            .as_str()
            .filter(|side| matches!(*side, "base" | "head"))
            .unwrap_or(side);
        if let Ok(location) =
            <crate::model::Location as serde::Deserialize>::deserialize(value)
            && !location.file.is_empty()
            && location.line > 0
            && !location.file_hash.is_empty()
            && !found.iter().any(|(existing_side, existing)| {
                existing_side == side
                    && existing.file == location.file
                    && existing.line == location.line
                    && existing.file_hash == location.file_hash
            })
        {
            found.push((side.into(), location));
        }
        match value {
            Value::Array(values) => {
                for child in values {
                    visit(child, side, depth + 1, found);
                }
            }
            Value::Object(values) => {
                for (key, child) in values {
                    visit(
                        child,
                        if key == "base_symbol" {
                            "base"
                        } else if key == "head_symbol" {
                            "head"
                        } else {
                            side
                        },
                        depth + 1,
                        found,
                    );
                }
            }
            _ => {}
        }
    }
    let mut found = vec![];
    visit(value, default_side, 0, &mut found);
    found
}

pub(super) fn check_explorer_views(
    view: Entity<Desktop>,
    tool: usize,
    waiting: bool,
    retries: usize,
    window: &mut Window,
) {
    window.on_next_frame(move |window, cx| {
        if waiting {
            let state = &view.read(cx).explorer;
            if state.busy { assert!(retries < 7200, "Explorer query did not finish"); check_explorer_views(view, tool, true, retries + 1, window); return; }
            let no_trait = !view.read(cx).data.symbols_on(&state.side).symbols.iter().any(|symbol| symbol.kind == "trait");
            if tool == 1 && no_trait { assert_eq!(state.error_code, "symbol_not_found", "{}", state.error); }
            else if tool == 5 && state.error_code == "capability_unsupported" { assert!(state.error.contains("service dependencies require configured services"), "{}", state.error); }
            else { assert!(state.error.is_empty(), "{}: {}", TOOLS[tool].0, state.error); assert!(state.report["snapshot_id"].is_string()); }
            check_explorer_views(view, tool + 1, false, 0, window);
        } else if tool < TOOLS.len() {
            view.update(cx, |this, cx| {
                this.page = Page::Explorer;
                this.explorer.tool = tool;
                let input = if tool == 0 { this.data.symbols_on(&this.source_side).symbols.iter().find(|symbol| symbol.kind == "function").map(|symbol| symbol.symbol_id.clone()).unwrap_or_default() }
                    else if tool == 1 { this.data.symbols_on(&this.source_side).symbols.iter().find(|symbol| symbol.kind == "trait").map(|symbol| symbol.symbol_id.clone()).unwrap_or_else(|| "__native_missing_trait__".into()) } else { String::new() };
                this.explore_from.update(cx, |field, cx| field.set_value(input.clone(), cx));
                this.explore_to.update(cx, |field, cx| field.set_value(input, cx));
                this.query_explorer(false, cx);
            });
            check_explorer_views(view, tool, true, 0, window);
        } else {
            println!("Native UI check: 11 pages, aligned diff, graph layouts/minimap, seven explorer tools, Base/Head navigation, IME and async analysis verified");
            if view.read(cx).launch.keep_open {
                view.update(cx, |this, cx| {
                    this.page = Page::Overview;
                    this.explorer.error.clear();
                    this.explorer.error_code.clear();
                    cx.notify();
                });
            } else {
                cx.quit();
            }
        }
    });
    window.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_queries_use_only_their_two_snapshot_selectors() {
        let symbols = crate::model::Symbols {
            snapshot_id: "head".into(),
            epochs: json!({"semantic":4}),
            ..Default::default()
        };
        let diff = json!({"base":{"snapshot_id":"base"},"head":{"snapshot_id":"head"}});
        for tool in 0..TOOLS.len() {
            let args = query_binding(tool, &symbols, Some(diff.clone())).unwrap();
            if matches!(tool, 3 | 4) {
                assert_eq!(args["diff"], diff);
                assert!(args.get("snapshot_id").is_none());
                assert!(args.get("semantic_epoch").is_none());
                assert!(query_binding(tool, &symbols, None).is_err());
            } else {
                assert_eq!(args["snapshot_id"], "head");
                assert_eq!(args["semantic_epoch"], 4);
                assert!(args.get("diff").is_none());
            }
        }
    }
}
