use semweft_query::{Engine, QueryService, config};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};

#[test]
fn public_inventory_filters_and_pins_pages_without_claiming_reachability() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"api\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn exposed() {}\npub struct Item;\npub trait Store {}\npub(crate) fn internal() {}\nfn private() {}\nmod hidden { pub fn hidden_api() {} }\n").unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let first = service
        .execute(
            "search_symbols",
            json!({"query":"*","visibility":"public","limit":1}),
            deadline(),
        )
        .unwrap();
    assert_eq!(first["api_summary"]["declaration_count"], 4);
    assert_eq!(
        first["api_summary"]["external_reachability"],
        "not_established"
    );
    fs::write(root.join("src/lib.rs"), "pub fn replacement() {}\n").unwrap();
    engine.scan(false).unwrap();
    let mut page = first.clone();
    let mut names = vec![];
    loop {
        assert_eq!(page["api_summary"], first["api_summary"]);
        assert_eq!(page["snapshot_id"], first["snapshot_id"]);
        for symbol in page["symbols"].as_array().unwrap() {
            assert_eq!(symbol["visibility"], "public");
            names.push(symbol["name"].as_str().unwrap().to_owned());
        }
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("search_symbols", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(names.len(), 4);
    assert!(names.contains(&"hidden_api".to_owned()));
    let restricted = service.execute("search_symbols", json!({"query":"*","visibility":"restricted","snapshot_id":first["snapshot_id"],"semantic_epoch":first["epochs"]["semantic"]}), deadline()).unwrap();
    assert_eq!(restricted["symbols"][0]["name"], "internal");
    assert!(
        service
            .execute(
                "search_symbols",
                json!({"query":"*","visibility":"exported"}),
                deadline()
            )
            .is_err()
    );
    engine.shutdown();
}

#[test]
fn external_candidates_expand_private_module_reexports_and_inherent_members() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"surface\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod hidden;
mod sealed { pub fn invisible() {} struct Secret; impl Secret { pub fn leak() {} } }
pub use hidden::{Item as PublicItem, Store, State, helper as run};
pub mod visible { pub use crate::hidden::*; }
#[cfg(feature="optional")]
pub mod conditional { pub fn maybe() {} }
#[cfg(test)]
mod tests { pub fn not_api() {} }
pub(crate) fn internal() {}
pub fn exposed() { fn local() {} }
"#,
    )
    .unwrap();
    fs::write(
        root.join("src/hidden.rs"),
        r#"
pub struct Item { pub value: u32, private: u32 }
impl Item { pub fn new() -> Self { Self { value: 1, private: 0 } } fn secret(&self) {} }
pub trait Store { fn save(&self); }
pub enum State { Ready, Gone }
pub fn helper() {}
fn private() {}
"#,
    )
    .unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let all = service
        .execute(
            "search_symbols",
            json!({"query":"*","api_scope":"external","limit":1000}),
            deadline(),
        )
        .unwrap();
    let symbols = all["symbols"].as_array().unwrap();
    let paths = symbols
        .iter()
        .flat_map(|symbol| symbol["api_exposure"]["paths"].as_array().unwrap())
        .map(|export| export["path"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    for path in [
        "surface::PublicItem",
        "surface::PublicItem::value",
        "surface::PublicItem::new",
        "surface::Store::save",
        "surface::State::Ready",
        "surface::run",
        "surface::visible::Item::new",
        "surface::conditional::maybe",
    ] {
        assert!(paths.contains(path), "missing {path}: {all}");
    }
    for name in [
        "invisible",
        "leak",
        "secret",
        "private",
        "internal",
        "local",
        "not_api",
    ] {
        assert!(
            !symbols.iter().any(|symbol| symbol["name"] == name),
            "unexpected {name}: {all}"
        );
    }
    assert!(!paths.contains("surface::hidden"));
    assert_eq!(all["api_summary"]["external_reachability"], "candidate");
    let conditional = symbols
        .iter()
        .find(|symbol| symbol["name"] == "maybe")
        .unwrap();
    assert_eq!(conditional["api_exposure"]["paths"][0]["conditional"], true);
    assert_eq!(
        conditional["api_exposure"]["paths"][0]["conditions"][0],
        "#[cfg(feature=\"optional\")]"
    );
    let alias = service
        .execute(
            "search_symbols",
            json!({"query":"surface::run","match":"exact","api_scope":"external"}),
            deadline(),
        )
        .unwrap();
    assert_eq!(alias["symbols"][0]["name"], "helper");
    let export = &alias["symbols"][0]["api_exposure"]["paths"][0];
    assert!(
        export["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|evidence| evidence["kind"] == "reexport" && evidence["file"] == "src/lib.rs")
    );
    assert!(
        service
            .execute(
                "search_symbols",
                json!({"query":"*","api_scope":"external","visibility":"private"}),
                deadline()
            )
            .is_err()
    );
    let mut page = service
        .execute(
            "search_symbols",
            json!({"query":"*","api_scope":"external","limit":1}),
            deadline(),
        )
        .unwrap();
    let pinned = page["snapshot_id"].clone();
    let summary = page["api_summary"].clone();
    fs::write(root.join("src/hidden.rs"), "pub fn replacement() {}\n").unwrap();
    engine.scan(false).unwrap();
    let mut count = 0;
    loop {
        assert_eq!(page["snapshot_id"], pinned);
        assert_eq!(page["api_summary"], summary);
        count += page["symbols"].as_array().unwrap().len();
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("search_symbols", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(count, symbols.len());
    engine.shutdown();
}

#[test]
fn external_diff_observes_reexport_only_changes_and_rejects_binary_api_assumptions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("source")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"),"[package]\nname=\"surface\"\nversion=\"0.1.0\"\n[lib]\nname=\"custom\"\npath=\"source/entry.rs\"\n").unwrap();
    fs::write(
        root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[index]\ndirectory=\"index\"\n",
    )
    .unwrap();
    let source = "#[path=\"types.rs\"]\nmod hidden;\npub use hidden::Item as Before;\npub use hidden::missing as Missing;\n";
    fs::write(root.join("source/entry.rs"), source).unwrap();
    fs::write(root.join("source/types.rs"), "pub struct Item;\n").unwrap();
    fs::write(
        root.join("src/main.rs"),
        "pub fn binary_only() {} fn main() {}\n",
    )
    .unwrap();
    let engine = Engine::new(config::resolve(Some(root), None).unwrap()).unwrap();
    let base = engine.scan(false).unwrap();
    let service = QueryService::new(engine.clone());
    let deadline = || Instant::now() + Duration::from_secs(10);
    let all = service
        .execute(
            "search_symbols",
            json!({"query":"*","api_scope":"external","limit":1000}),
            deadline(),
        )
        .unwrap();
    assert_eq!(all["symbols"].as_array().unwrap().len(), 1, "{all}");
    assert_eq!(
        all["symbols"][0]["api_exposure"]["paths"][0]["path"],
        "custom::Before"
    );
    assert!(
        all["api_summary"]["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap.as_str().unwrap().starts_with("reexport_unresolved:"))
    );
    fs::write(
        root.join("source/entry.rs"),
        source.replace(" as Before", " as After"),
    )
    .unwrap();
    let diff = json!({"base":{"kind":"snapshot","snapshot_id":base.snapshot.snapshot_id,"semantic_epoch":0},"head":{"kind":"workspace"}});
    let mut page = service
        .execute("impact_of", json!({"diff":diff,"limit":1}), deadline())
        .unwrap();
    let summary = page["api_surface_summary"].clone();
    assert_eq!(summary["change_count"], 2, "{page}");
    assert_eq!(page["diff_summary"]["changed_symbols"], 0, "{page}");
    let mut changes = vec![];
    loop {
        assert_eq!(page["api_surface_summary"], summary);
        changes.extend(page["api_changes"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["output"]["continuation"]["next_cursor"].as_str() else {
            break;
        };
        page = service
            .execute("impact_of", json!({"cursor":cursor}), deadline())
            .unwrap();
    }
    assert_eq!(changes.len(), 2);
    assert!(
        changes
            .iter()
            .any(|change| change["kind"] == "removed" && change["path"] == "custom::Before")
    );
    assert!(
        changes
            .iter()
            .any(|change| change["kind"] == "added" && change["path"] == "custom::After")
    );
    engine.shutdown();
}
