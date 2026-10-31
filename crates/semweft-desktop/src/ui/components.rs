use super::{AMBER, GREEN, INK, LINE, MINT, MUTED};
use gpui::{
    App, ClickEvent, ElementId, FontWeight, InteractiveText, SharedString, StyledText, Window, div,
    prelude::*, px, rgb, svg,
};
use gpui_rsx::{component, rsx};
use std::ops::Range;
use std::rc::Rc;

pub(super) trait ButtonClick: Fn(&ClickEvent, &mut Window, &mut App) {}
impl<F: Fn(&ClickEvent, &mut Window, &mut App)> ButtonClick for F {}

pub(super) trait SourceClick: Fn(usize, &mut Window, &mut App) {}
impl<F: Fn(usize, &mut Window, &mut App)> SourceClick for F {}

#[component]
pub(super) fn Icon(
    name: SharedString,
    #[prop(default = MUTED)] color: u32,
    #[prop(default = 16)] size: u32,
) -> impl IntoElement {
    rsx! { <svg path={format!("icons/{name}.svg")} class="flex-shrink-0" size={px(size as f32)} text_color={rgb(color)} /> }
}

struct HoverLabel(SharedString);
impl gpui::Render for HoverLabel {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        rsx! { <div class="px-3 py-2 rounded-lg bg-[#202939] text-white" text_size={px(11.)}>{self.0.clone()}</div> }
    }
}

#[component]
pub(super) fn IconButton(
    id: ElementId,
    icon: SharedString,
    hint: SharedString,
    #[prop(default)] active: bool,
    #[prop(default)] disabled: bool,
    #[prop(callback)] on_click: Rc<dyn ButtonClick>,
) -> impl IntoElement {
    rsx! {
        <button id={id} class="w-[28px] h-[28px] flex items-center justify-center rounded-md cursor-pointer"
            whenClass={(active, "bg-[#e6f2ed]")} whenClass={(disabled, "opacity-30 cursor-default")}
            hoverClass="bg-[#e9edf3]"
            tooltip={move |_, cx| cx.new(|_| HoverLabel(hint.clone())).into()}
            onClick={move |event, window, cx| { if !disabled { on_click(event, window, cx); } }}>
            <Icon name={icon} color={if active { GREEN } else { MUTED }} size={15u32} />
        </button>
    }
}

#[component]
pub(super) fn FileBadge(path: SharedString) -> impl IntoElement {
    let (label, color) = match path.rsplit('.').next().unwrap_or("") {
        "rs" => ("rs", 0xb16b46),
        "go" => ("go", 0x2796ad),
        "ts" | "tsx" | "mts" | "cts" => ("ts", 0x397ac4),
        "js" | "jsx" | "mjs" | "cjs" => ("js", 0x9a7d22),
        _ => ("·", MUTED),
    };
    rsx! { <div class="w-[19px] h-[20px] flex-shrink-0 flex items-center justify-center font-semibold" font_family="Menlo" text_size={px(10.)} text_color={rgb(color)}>{label}</div> }
}

#[component]
pub(super) fn SymbolBadge(kind: SharedString) -> impl IntoElement {
    let (label, color, background) = match kind.as_ref() {
        "function" | "method" => ("ƒ", 0x7160b5, 0xf0edfa),
        "struct" | "class" | "interface" | "trait" | "type" => ("T", 0x3f7dab, 0xeaf2fa),
        "enum" | "variant" => ("E", 0x9f7437, 0xf9f1e3),
        "field" | "property" => ("P", 0x508375, 0xeaf3ef),
        _ => ("C", 0x7b8394, 0xeff1f5),
    };
    rsx! { <div class="w-[22px] h-[22px] rounded-md flex-shrink-0 flex items-center justify-center font-semibold" text_size={px(11.)} text_color={rgb(color)} bg={rgb(background)}>{label}</div> }
}

#[component]
pub(super) fn PanelHeader(
    title: SharedString,
    #[prop(default)] count: Option<SharedString>,
    #[children] children: Vec<gpui::AnyElement>,
) -> impl IntoElement {
    rsx! {
        <div class="h-[44px] px-4 flex-shrink-0 flex items-center gap-2 border-b bg-[#fafbfc]" border_color={rgb(LINE)}>
            <div class="font-semibold" text_size={px(11.)} text_color={rgb(0x576274)}>{title}</div>
            {...count.map(|count| rsx! { <span class="px-2 rounded-md bg-[#edf0f4]" text_size={px(10.)} text_color={rgb(MUTED)}>{count}</span> })}
            <div class="flex-1" />{...children}
        </div>
    }
}

#[component]
pub(super) fn CodeText(
    id: ElementId,
    text: StyledText,
    ranges: Vec<Range<usize>>,
    #[prop(callback)] on_navigate: Rc<dyn SourceClick>,
) -> impl IntoElement {
    rsx! {
        <div base={InteractiveText::new(id, text)}
            onClick={(ranges, move |index, window, cx| on_navigate(index, window, cx))} />
    }
}

// IDs belong to the caller so instances in lists retain independent interaction state.
#[component]
pub(super) fn ActionButton(
    id: ElementId,
    #[prop(default)] active: bool,
    #[prop(default)] compact: bool,
    #[prop(default)] disabled: bool,
    #[prop(callback)] on_click: Rc<dyn ButtonClick>,
    #[children] children: Vec<gpui::AnyElement>,
) -> impl IntoElement {
    rsx! {
        <button styled id={id} class="h-[32px] px-3 flex-shrink-0 flex items-center justify-center gap-2 rounded-lg border bg-white text-[#475467] border-[#e3e7ee] font-medium cursor-pointer"
            text_size={px(if compact { 10. } else { 12. })}
            whenClass={(compact, "h-[26px] px-2")}
            whenClass={(active, "bg-[#e6f2ed] text-[#237b62] border-[#cfe4da]")}
            whenClass={(disabled, "opacity-50 cursor-default")}
            hoverClass="bg-[#f1f4f7]" activeClass="bg-[#e3e9ef]"
            onClick={move |event, window, cx| {
                if !disabled { on_click(event, window, cx); }
            }}>
            {...children}
        </button>
    }
}

#[component]
pub(super) fn Badge(
    label: SharedString,
    #[prop(default = MUTED)] color: u32,
    #[prop(default = 0xf4f6f8)] background: u32,
) -> impl IntoElement {
    rsx! {
        <span class="px-2 py-1 rounded-md flex-shrink-0 font-medium" text_size={px(10.)}
            text_color={rgb(color)} bg={rgb(background)}>{label}</span>
    }
}

#[component]
pub(super) fn DetailField(name: SharedString, value: SharedString) -> impl IntoElement {
    rsx! {
        <div class="flex justify-between gap-3 py-2" text_size={px(11.)}>
            <div class="flex-shrink-0" text_color={rgb(MUTED)}>{name}</div>
            <div class="min-w-0" text_color={rgb(INK)}>{value}</div>
        </div>
    }
}

#[component]
pub(super) fn StatCard(
    label: SharedString,
    value: usize,
    #[prop(default)] note: Option<SharedString>,
) -> impl IntoElement {
    rsx! {
        <div class="flex-1 min-w-0 p-5 border rounded-xl bg-white" border_color={rgb(LINE)}>
            <div class="flex items-center justify-between">
                <div text_color={rgb(MUTED)} text_size={px(11.)}>{label}</div>
                <div class="w-[24px] h-[24px] rounded-md flex items-center justify-center" bg={rgb(MINT)}><Icon name="chart" color={GREEN} size={13u32} /></div>
            </div>
            <div class="font-semibold mt-3" text_size={px(28.)}>{value.to_string()}</div>
            {...note.map(|text| rsx! {
                <div class="mt-1" text_color={rgb(MUTED)} text_size={px(10.)}>{text}</div>
            })}
        </div>
    }
}

#[component]
pub(super) fn EmptyState(label: SharedString, #[prop(default)] warning: bool) -> impl IntoElement {
    rsx! {
        <div class="px-6 py-8 flex flex-col items-center gap-3" text_size={px(12.)} text_color={rgb(MUTED)}
            when={(warning, |element| element.text_color(rgb(AMBER)))}>
            <div class="w-[36px] h-[36px] rounded-xl bg-[#f0f3f7] flex items-center justify-center"><Icon name={if warning { "review" } else { "search" }} size={18u32} /></div>
            <div>{label}</div>
        </div>
    }
}

#[component]
#[derive(IntoElement)]
pub(super) struct Section {
    title: SharedString,
    #[prop(default)]
    note: Option<SharedString>,
    #[children]
    children: Vec<gpui::AnyElement>,
}

impl RenderOnce for Section {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        rsx! {
            <section class="mt-5 p-5 border rounded-xl bg-white" border_color={rgb(LINE)}>
                <div class="font-semibold mb-3" text_size={px(13.)}>{self.title}</div>
                {...self.note.map(|text| rsx! {
                    <div class="mb-3" text_color={rgb(MUTED)} text_size={px(12.)}>{text}</div>
                })}
                {...self.children}
            </section>
        }
    }
}
