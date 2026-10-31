//! Consumer regressions for the gpui-rsx upgrade, using the desktop's real GPUI types.
use gpui::{
    AnyElement, Element, ElementId, InteractiveText, SharedString, StyledText, div, prelude::*, px,
};
use gpui_rsx::{component, rsx, rsx_fragment};

#[component]
fn NumericDefaults(
    #[prop(default = 7)] color: u32,
    #[prop(default = 9)] count: usize,
    #[prop(default = 1.5)] scale: f32,
) -> impl IntoElement {
    rsx! { <div>{format!("{color}/{count}/{scale}")}</div> }
}

#[test]
fn numeric_defaults_infer_prop_types() {
    let props = rsx! { <NumericDefaults /> };
    assert_eq!(props.color, 7);
    assert_eq!(props.count, 9);
    assert_eq!(props.scale, 1.5);
}

#[test]
fn dynamic_keys_preserve_values_and_distinguish_rows() {
    fn string_id(key: String) -> (ElementId, String) {
        let element = rsx! { <div key={key} onClick={|_, _, _| {}} /> };
        (Element::id(&element.into_element()).unwrap(), key)
    }
    fn borrowed_id(key: &str) -> ElementId {
        let element = rsx! { <div key={key} onClick={|_, _, _| {}} /> };
        Element::id(&element.into_element()).unwrap()
    }
    fn shared_id(key: SharedString) -> (ElementId, SharedString) {
        let element = rsx! { <div key={key} onClick={|_, _, _| {}} /> };
        (Element::id(&element.into_element()).unwrap(), key)
    }

    let (first, retained) = string_id("src/中文.rs".into());
    assert_eq!(retained, "src/中文.rs");
    assert_eq!(first, string_id(retained).0);
    assert_ne!(first, string_id("src/other.rs".into()).0);

    // Use a local String to ensure &str keys do not require a static lifetime.
    let local = String::from("src/中文.rs");
    assert_eq!(borrowed_id(&local), borrowed_id(&local));
    assert_ne!(borrowed_id(&local), borrowed_id("src/other.rs"));

    let (shared, retained) = shared_id(local.into());
    assert_eq!(retained.as_ref(), "src/中文.rs");
    assert_eq!(shared, shared_id(retained).0);
    assert_ne!(shared, shared_id("src/other.rs".into()).0);
}

#[test]
fn erased_fragments_accept_mixed_roots_and_loop_children() {
    let parts: Vec<AnyElement> = rsx_fragment! {
        <>
        <div />
        <div id="stateful" />
        <NumericDefaults />
        </>
    };
    assert_eq!(parts.len(), 3);
    let rows: Vec<AnyElement> = rsx_fragment! {
        <>
        {for key in ["first", "second"] {
            <div key={key} onClick={|_, _, _| {}} />
        }}
        </>
    };
    assert_eq!(rows.len(), 2);
    let _parent = rsx! { <div>{...parts}{...rows}</div> };
    let empty: Vec<AnyElement> = rsx_fragment! { <></> };
    assert!(empty.is_empty());
}

#[test]
fn spacing_uses_the_enabled_tailwind_scale() {
    let mut element = rsx! { <div class="p-4 gap-2" /> };
    assert_eq!(element.style().padding.top, Some(gpui::px(16.).into()));
    assert_eq!(element.style().gap.width, Some(gpui::px(8.).into()));
}

#[test]
fn interactive_text_accepts_two_argument_on_click_alongside_single_argument_div() {
    let ranges = vec![0..7, 8..15];
    let text = rsx! {
        <div base={InteractiveText::new("source-identifiers", StyledText::new("reserve release"))}
            onClick={(ranges, |index, _, _| { assert!(index < 2); })} />
    };
    assert_eq!(
        Element::id(&text.into_element()),
        Some("source-identifiers".into())
    );
    let ordinary = rsx! { <div id="ordinary-click" onClick={|_, _, _| {}} /> };
    assert_eq!(
        Element::id(&ordinary.into_element()),
        Some("ordinary-click".into())
    );
}
