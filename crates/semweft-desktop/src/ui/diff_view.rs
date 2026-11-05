use super::*;
use crate::{diff::LineDiff, model::SourceNote};
use gpui::{HighlightStyle, StyledText};

#[derive(Default)]
pub(super) struct DiffView {
    key: String,
    pub(super) diff: LineDiff,
    base: SourceDocument,
    head: SourceDocument,
    base_file: String,
    head_file: String,
    base_hash: String,
    head_hash: String,
    scroll: UniformListScrollHandle,
    hunk: usize,
}

impl DiffView {
    pub(super) fn scroll_to(&self, row: usize) {
        self.scroll.scroll_to_item(row, ScrollStrategy::Center);
    }
}

impl Desktop {
    pub(super) fn prepare_diff(&mut self) {
        if !self.session.compare_sources {
            return;
        }
        let other = if self.source_side == "base" {
            "head"
        } else {
            "base"
        };
        let alternatives = self
            .data
            .impact
            .changes
            .iter()
            .filter_map(|change| {
                let (source, target) = if self.source_side == "base" {
                    (&change.base_symbol, &change.head_symbol)
                } else {
                    (&change.head_symbol, &change.base_symbol)
                };
                (source.as_ref()?.location.file == self.source_file)
                    .then(|| target.as_ref().map(|symbol| symbol.location.file.clone()))
                    .flatten()
            })
            .collect::<BTreeSet<_>>();
        let other_file = if self.data.sources_on(other).contains_key(&self.source_file)
            || alternatives.len() != 1
        {
            self.source_file.clone()
        } else {
            alternatives.into_iter().next().unwrap()
        };
        let (base_file, head_file) = if self.source_side == "base" {
            (self.source_file.clone(), other_file)
        } else {
            (other_file, self.source_file.clone())
        };
        let base = self
            .data
            .base_sources
            .get(&base_file)
            .map(String::as_str)
            .unwrap_or("");
        let head = self
            .data
            .sources
            .get(&head_file)
            .map(String::as_str)
            .unwrap_or("");
        let key = format!("{}/{base_file}/{head_file}", self.data.review_key);
        if self.diff_view.key == key {
            return;
        }
        self.diff_view = DiffView {
            key,
            diff: LineDiff::build(base, head),
            base: SourceDocument::new(&base_file, base),
            head: SourceDocument::new(&head_file, head),
            base_hash: semweft_shared::digest(base),
            head_hash: semweft_shared::digest(head),
            base_file,
            head_file,
            ..Default::default()
        };
    }

    fn diff_hunk(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.diff_view.diff.hunks.len();
        if count == 0 {
            return;
        }
        self.diff_view.hunk = if forward {
            (self.diff_view.hunk + 1) % count
        } else {
            (self.diff_view.hunk + count - 1) % count
        };
        self.diff_view.scroll.scroll_to_item(
            self.diff_view.diff.hunks[self.diff_view.hunk].start,
            ScrollStrategy::Center,
        );
        cx.notify();
    }

    fn add_source_note(&mut self, cx: &mut Context<Self>) {
        let text = self.source_note.read(cx).value().trim().to_owned();
        let Some(source) = self
            .data
            .sources_on(&self.source_side)
            .get(&self.source_file)
        else {
            return;
        };
        if text.is_empty() || text.len() > 16384 || self.session.source_notes.len() >= 1000 {
            self.message = "批注不能为空，最多 16KiB；每个工作区最多保存 1000 条批注".into();
            cx.notify();
            return;
        }
        self.session.source_notes.push(SourceNote {
            file: self.source_file.clone(),
            side: self.source_side.clone(),
            file_hash: semweft_shared::digest(source),
            line: self.source_line,
            text,
        });
        match self.session.save(&self.launch.data) {
            Ok(()) => {
                self.source_note.update(cx, |input, cx| input.clear(cx));
                self.message = "批注已绑定此侧源码内容".into();
            }
            Err(error) => self.message = error.to_string(),
        }
        cx.notify();
    }

    pub(super) fn aligned_diff(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let count = self.diff_view.diff.hunks.len();
        let rows = uniform_list("aligned-diff-lines", self.diff_view.diff.rows.len(), cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let row = &this.diff_view.diff.rows[index];
                rsx! { <div class="h-[24px] flex w-full" font_family="Menlo" text_size={px(11.)}>
                    {for (side, line_index) in [("base", row.base), ("head", row.head)] {
                        <div id={SharedString::from(format!("diff-{side}-{index}"))} class="flex flex-1 min-w-0 h-full border-r overflow-hidden cursor-pointer" border_color={rgb(LINE)}
                            bg={rgb(if line_index.is_none() { 0xf4f5f7 } else if row.changed { if side == "base" { 0xffeeec } else { 0xeaf6ed } } else { 0xffffff })}
                            onClick={cx.listener(move |this, _, _, cx| { if let Some(line) = line_index { let file = if side == "base" { this.diff_view.base_file.clone() } else { this.diff_view.head_file.clone() }; this.open_side(file, line + 1, side, true, cx); } })}>
                            <div class="w-[44px] flex-shrink-0 pr-2 text-right" text_color={rgb(MUTED)}>{line_index.map(|line| (line + 1).to_string()).unwrap_or_default()}</div>
                            <div class="whitespace-nowrap pl-2">{line_index.and_then(|line| (if side == "base" { &this.diff_view.base } else { &this.diff_view.head }).lines.get(line)).map(|line| StyledText::new(line.text.clone()).with_highlights(line.highlights.iter().map(|(range, style)| (range.clone(), HighlightStyle { color: Some(rgb(source_view::syntax_color(style)).into()), ..Default::default() })))).map(IntoElement::into_any_element).unwrap_or_else(|| gpui::Empty.into_any_element())}</div>
                        </div>
                    }}
                </div> }.into_any_element()
            }).collect::<Vec<_>>()
        })).track_scroll(self.diff_view.scroll.clone());
        let notes = self
            .session
            .source_notes
            .iter()
            .enumerate()
            .filter(|(_, note)| {
                if note.side == "base" {
                    note.file == self.diff_view.base_file
                        && note.file_hash == self.diff_view.base_hash
                } else {
                    note.file == self.diff_view.head_file
                        && note.file_hash == self.diff_view.head_hash
                }
            })
            .collect::<Vec<_>>();
        rsx! { <div class="flex flex-col flex-1 min-h-0">
            <div class="h-[34px] flex items-center gap-2 px-2 border-b bg-[#fafbfc]" border_color={rgb(LINE)}>
                <ActionButton id="diff-prev" compact disabledIf={count == 0} onClick={cx.listener(|this, _, _, cx| this.diff_hunk(false, cx))}>{"上一变更"}</ActionButton>
                <ActionButton id="diff-next" compact disabledIf={count == 0} onClick={cx.listener(|this, _, _, cx| this.diff_hunk(true, cx))}>{"下一变更"}</ActionButton>
                {muted(format!("{} 个变更块{}", count, if self.diff_view.diff.coarse { " · 大替换区块未细分" } else { "" }))}
            </div>
            <div class="flex h-[26px] flex-shrink-0 bg-[#f7f9fc]" text_size={px(10.)}>
                <div class="flex-1 px-2 truncate">{format!("BASE · {}", self.diff_view.base_file)}</div><div class="flex-1 px-2 truncate">{format!("HEAD · {}", self.diff_view.head_file)}</div>
            </div>
            <div base={rows} class="flex-1 min-h-0" />
            <div class="flex gap-2 p-2 border-t" border_color={rgb(LINE)}><div class="flex-1 h-[28px]">{self.source_note.clone()}</div><ActionButton id="source-add-note" compact onClick={cx.listener(|this, _, _, cx| this.add_source_note(cx))}>{format!("本地批注 {}:{}", self.source_side.to_uppercase(), self.source_line)}</ActionButton>
                {...(self.analysis.is_request()).then(|| rsx! { <ActionButton id="source-review-note" compact disabledIf={self.source_note.read(cx).value().trim().is_empty()} onClick={cx.listener(|this, _, _, cx| this.review_action("decide", Some("source".into()), "comment", cx))}>{"提交审查意见"}</ActionButton> })}
            </div>
            <div id="diff-notes" class="max-h-[100px] overflow-y-scroll px-2" text_size={px(10.)}>
                {for (index, note) in notes { <div class="flex gap-2 py-1"><div class="flex-1">{format!("{}:{} · {}", note.side.to_uppercase(), note.line, note.text)}</div><IconButton id={SharedString::from(format!("source-note-{index}"))} icon="close" hint="删除批注" onClick={cx.listener(move |this, _, _, cx| { this.session.source_notes.remove(index); if let Err(error) = this.session.save(&this.launch.data) { this.message = error.to_string(); } cx.notify(); })} /></div> }}
            </div>
        </div> }.into_any_element()
    }
}
