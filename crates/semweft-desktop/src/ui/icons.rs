use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

// Embedded, monochrome vectors keep every screen on the same icon grid.
const ICONS: &[(&str, &str)] = &[
    (
        "code",
        r#"<path d="m8 7-5 5 5 5m8-10 5 5-5 5m-3-13-2 16"/>"#,
    ),
    (
        "folder",
        r#"<path d="M3 7V5a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/>"#,
    ),
    (
        "folder-open",
        r#"<path d="M3 11V5a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v3M3 11h18l-2 10H5L3 11Z"/>"#,
    ),
    (
        "file",
        r#"<path d="M14 3H5v18h14V8l-5-5Zm0 0v5h5M8 12h8m-8 4h5"/>"#,
    ),
    (
        "search",
        r#"<circle cx="10.5" cy="10.5" r="6.5"/><path d="m16 16 5 5"/>"#,
    ),
    (
        "symbols",
        r#"<path d="M8 3H6a2 2 0 0 0-2 2v4l-2 3 2 3v4a2 2 0 0 0 2 2h2m8-18h2a2 2 0 0 1 2 2v4l2 3-2 3v4a2 2 0 0 1-2 2h-2M10 12h4"/>"#,
    ),
    (
        "graph",
        r#"<rect x="9" y="2" width="6" height="6" rx="1.5"/><rect x="2" y="16" width="6" height="6" rx="1.5"/><rect x="16" y="16" width="6" height="6" rx="1.5"/><path d="M12 8v4M5 16v-4h14v4"/>"#,
    ),
    (
        "grid",
        r#"<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>"#,
    ),
    (
        "review",
        r#"<path d="M14 3H5v18h14V8l-5-5Zm0 0v5h5m-11 7 3 3 5-6"/>"#,
    ),
    (
        "layers",
        r#"<path d="m12 2 10 5-10 5L2 7l10-5ZM2 12l10 5 10-5M2 17l10 5 10-5"/>"#,
    ),
    ("chart", r#"<path d="M4 3v18h17M9 16v-4m5 4V7m5 9v-6"/>"#),
    ("bookmark", r#"<path d="M6 3h12v18l-6-4-6 4V3Z"/>"#),
    (
        "branch",
        r#"<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="5" r="2"/><path d="M6 7v10m12-10a10 10 0 0 1-10 9H6"/>"#,
    ),
    (
        "refresh",
        r#"<path d="M20 7a9 9 0 0 0-15-2L2 8m0-5v5h5m-3 9a9 9 0 0 0 15 2l3-3m0 5v-5h-5"/>"#,
    ),
    (
        "upload",
        r#"<path d="M12 16V3m-5 5 5-5 5 5M3 15v6h18v-6"/>"#,
    ),
    ("arrow-left", r#"<path d="m12 5-7 7 7 7M5 12h15"/>"#),
    ("arrow-right", r#"<path d="m12 5 7 7-7 7M4 12h15"/>"#),
    ("chevron-right", r#"<path d="m9 5 7 7-7 7"/>"#),
    ("chevron-down", r#"<path d="m5 9 7 7 7-7"/>"#),
    ("close", r#"<path d="m6 6 12 12M6 18 18 6"/>"#),
    ("plus", r#"<path d="M12 4v16M4 12h16"/>"#),
    (
        "copy",
        r#"<rect x="8" y="8" width="13" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>"#,
    ),
    (
        "panel",
        r#"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M15 3v18m3-13h0m0 4h0m0 4h0"/>"#,
    ),
    (
        "collapse",
        r#"<path d="m8 3 4 4 4-4M4 12h16m-12 9 4-4 4 4"/>"#,
    ),
    ("check", r#"<path d="m4 12 5 5L20 6"/>"#),
    (
        "logo",
        r#"<path d="m4 7 5-3 6 3 5-3M4 12l5-3 6 3 5-3M4 17l5-3 6 3 5-3M4 22l5-3 6 3 5-3"/>"#,
    ),
];

pub(super) struct UiAssets;

impl AssetSource for UiAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let name = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"));
        Ok(ICONS.iter().find(|(key, _)| Some(*key) == name).map(|(_, body)| {
            Cow::Owned(format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">{body}</svg>"#).into_bytes())
        }))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(name, _)| format!("icons/{name}.svg"))
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
