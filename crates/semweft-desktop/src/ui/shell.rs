use super::*;

impl Page {
    fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Files => ("code", "源码浏览", "沿着目录与符号，探索你的代码"),
            Self::Symbols => ("symbols", "符号导航", "从声明、签名到定义，快速找到上下文"),
            Self::Search => ("search", "全文搜索", "在整个项目的源码中查找线索"),
            Self::Graph => ("graph", "调用与引用", "理解符号之间的联系与影响方向"),
            Self::Overview => ("grid", "仓库概览", "项目结构、模块与核心符号，一览可见"),
            Self::Review => ("review", "变更审查", "跟随证据，核查变更带来的影响"),
            Self::Dependencies => ("layers", "模块依赖", "从依赖矩阵与循环关系理解架构"),
            Self::Metrics => ("chart", "度量与热点", "观察代码规模、活跃文件与长函数"),
            Self::Bookmarks => ("bookmark", "我的书签", "随时回到值得关注的代码位置"),
        }
    }
}

impl Desktop {
    pub(super) fn sidebar(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let groups = [
            (
                "探索",
                vec![Page::Files, Page::Symbols, Page::Search, Page::Graph],
            ),
            (
                "分析",
                vec![
                    Page::Overview,
                    Page::Review,
                    Page::Dependencies,
                    Page::Metrics,
                ],
            ),
            ("个人", vec![Page::Bookmarks]),
        ];
        rsx! {
            <div class="w-[200px] flex-shrink-0 h-full flex flex-col bg-[#18212f] text-[#a6b0c1]">
                <div class="h-[64px] px-5 flex items-center gap-3 flex-shrink-0">
                    <div class="w-[30px] h-[30px] rounded-lg bg-[#92d4bb] flex items-center justify-center"><Icon name="logo" color={0x173e32u32} size={21u32} /></div>
                    <div class="font-semibold text-white" text_size={px(18.)}>{"semweft"}</div>
                </div>
                <div class="mx-3 mt-2 mb-3 p-3 rounded-lg bg-[#222e3e] border border-[#303d50]">
                    <div class="flex items-center gap-2">
                        <Icon name="folder" color={0x9cb3cdu32} size={15u32} />
                        <div class="font-semibold truncate text-[#e2e9f2]" text_size={px(12.)}>{self.data.name.clone()}</div>
                    </div>
                    <div class="flex items-center gap-2 mt-2" text_size={px(10.)}>
                        <Icon name="branch" color={0x75889fu32} size={12u32} />
                        <div class="truncate">{self.data.branch.clone().unwrap_or_else(|| "本地项目".into())}</div>
                    </div>
                </div>
                <div id="navigation-scroll" class="flex-1 min-h-0 overflow-y-scroll px-3">
                    {for (group, pages) in groups {
                        <div class="px-3 mt-5 mb-2 font-medium" text_color={rgb(0x63758d)} text_size={px(10.)}>{group}</div>
                        {for page in pages {
                            <div key={page.info().1} class="h-[37px] mb-1 px-3 flex items-center gap-3 rounded-lg cursor-pointer" text_size={px(12.)}
                                whenClass={(page == self.page, "bg-[#2c403e] text-[#a6e0c9] font-semibold")}
                                hoverClass="bg-[#263345]"
                                onClick={cx.listener(move |this, _, _, cx| this.change_page(page, cx))}>
                                <Icon name={page.info().0} color={if page == self.page { 0xa6e0c9u32 } else { 0x8293aau32 }} />
                                <div class="flex-1">{page.info().1}</div>
                                {...(page == Page::Review && !self.data.impact.changes.is_empty()).then(|| rsx! {
                                    <span class="px-2 rounded-md bg-[#34465b] text-[#b8c8da]" text_size={px(10.)}>{self.data.impact.changes.len().to_string()}</span>
                                })}
                            </div>
                        }}
                    }}
                </div>
                <div class="p-3 mt-3 border-t border-[#2a3546] flex-shrink-0">
                    <button id="open-workspace" class="w-full h-[34px] flex items-center gap-2 px-3 rounded-lg cursor-pointer text-[#b4c1d1]" hoverClass="bg-[#263345]" text_size={px(11.)}
                        onClick={cx.listener(|this, _, _, cx| this.open_workspace(cx))}>
                        <Icon name="plus" color={0x8fa1b8u32} size={15u32} /><div class="flex-1">{"打开工作区"}</div><span text_color={rgb(0x62758c)}>{"⌘O"}</span>
                    </button>
                    <div class="flex gap-2 items-center px-3 mt-3 mb-1" text_color={rgb(0x697e96)} text_size={px(10.)}>
                        <div class="w-[5px] h-[5px] rounded-full bg-[#78b69d]" />{"本地分析 · 代码留在本机"}
                    </div>
                </div>
            </div>
        }.into_any_element()
    }

    pub(super) fn header(&self, cx: &Context<Self>) -> gpui::AnyElement {
        rsx! {
            <div class="h-[56px] px-5 flex-shrink-0 flex items-center justify-between gap-4 border-b bg-white" border_color={rgb(LINE)}>
                <div class="flex gap-2 items-center min-w-0" text_size={px(11.)}>
                    <Icon name="folder" size={14u32} />
                    <div class="truncate" text_color={rgb(MUTED)}>{self.data.name.clone()}</div>
                    <Icon name="chevron-right" color={0xb4bdcbu32} size={11u32} />
                    <div class="font-medium flex-shrink-0">{self.page.info().1}</div>
                </div>
                <div class="flex gap-3 items-center flex-shrink-0">
                    <div class="flex gap-2 items-center w-[280px]">
                        <Icon name="search" color={0x9aa6b7u32} size={15u32} />
                        <div class="flex-1 min-w-0">{self.query.clone()}</div>
                        {...(!self.search_text.is_empty()).then(|| rsx! { <IconButton id="clear-query" icon="close" hint="清除搜索" onClick={cx.listener(|this, _, _, cx| this.query.update(cx, |q, cx| q.clear(cx)))} /> })}
                    </div>
                    <div class="w-[1px] h-[20px]" bg={rgb(LINE)} />
                    <IconButton id="refresh" icon="refresh" hint="刷新项目  ⌘R" disabledIf={self.busy} onClick={cx.listener(|this, _, _, cx| this.refresh(None, cx))} />
                    <ActionButton id="export" onClick={cx.listener(|this, _, _, cx| this.export(cx))}><Icon name="upload" color={0x627087u32} size={14u32} />{"导出报告"}</ActionButton>
                </div>
            </div>
        }.into_any_element()
    }

    pub(super) fn page_heading(&self) -> gpui::AnyElement {
        let (icon, title, description) = self.page.info();
        rsx! {
            <div class="h-[52px] mb-4 flex items-center justify-between flex-shrink-0">
                <div class="flex items-center gap-3">
                    <div class="w-[38px] h-[38px] rounded-xl bg-white border flex items-center justify-center" border_color={rgb(LINE)}><Icon name={icon} color={GREEN} size={19u32} /></div>
                    <div>
                        <div class="font-semibold" text_size={px(20.)}>{title}</div>
                        <div class="mt-1" text_color={rgb(MUTED)} text_size={px(11.)}>{description}</div>
                    </div>
                </div>
                <div class="flex items-center gap-2" text_size={px(10.)} text_color={rgb(MUTED)}>
                    <div class="w-[6px] h-[6px] rounded-full" bg={rgb(if self.busy { AMBER } else { GREEN })} />
                    {if self.busy { "正在更新项目…" } else if self.data.example { "示例项目" } else { "本地项目" }}
                </div>
            </div>
        }.into_any_element()
    }

    pub(super) fn statusbar(&self) -> gpui::AnyElement {
        let truncated = [
            &self.data.impact.output,
            &self.data.symbols.output,
            &self.data.map.output,
            &self.data.module_map.output,
        ]
        .iter()
        .any(|value| value["truncated"].as_bool().unwrap_or(false));
        rsx! {
            <div class="h-[26px] px-4 flex-shrink-0 flex items-center gap-4 border-t bg-white" border_color={rgb(LINE)} text_size={px(10.)} text_color={rgb(MUTED)}>
                <div class="flex items-center gap-2 flex-shrink-0"><Icon name="check" size={11u32} color={GREEN} />{if self.busy { "更新中" } else { "本地引擎就绪" }}</div>
                <div class="truncate flex-1 min-w-0">{if self.message.is_empty() { format!("{} 个文件 · {} 个符号{}", self.data.sources.len(), self.data.symbols.symbols.len(), if truncated { " · 部分结果已截断" } else { "" }) } else { self.message.clone() }}</div>
                <div class="flex-shrink-0">{if matches!(self.page, Page::Files | Page::Search) { format!("{}    Ln {}    UTF-8", self.source_code.language, self.source_line) } else { "固定快照 · 刷新以更新".into() }}</div>
            </div>
        }.into_any_element()
    }
}
