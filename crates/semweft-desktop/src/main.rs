mod client;
mod diff;
mod graph;
mod model;
mod source;

#[cfg(target_os = "macos")]
mod input;
#[cfg(target_os = "macos")]
mod ui;

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    if command.as_deref() == Some("--check-service") {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--check-service requires JSON path"))?;
        let data = model::Preview::load(std::path::Path::new(&path))?;
        return client::check_service(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
            &data,
        );
    }
    if command.as_deref() == Some("--capture") {
        let workspace = std::path::PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("--capture requires workspace and JSON output"))?,
        )
        .canonicalize()?;
        let destination = std::path::PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("--capture requires JSON output"))?,
        );
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let captured = client::Client::new(&repository, &workspace)
            .capture(false, &client::Cancellation::default())?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, serde_json::to_vec(&captured.raw)?)?;
        println!(
            "Captured {} files, {} symbols, {} Base files",
            captured.sources.len(),
            captured.symbols.symbols.len(),
            captured.base_sources.len()
        );
        return Ok(());
    }
    if command.as_deref() == Some("--check-text") {
        ui::check_native_text(&gpui::Application::new().text_system())?;
        println!("Native macOS text backend: system and CJK glyph bounds verified");
        return Ok(());
    }
    if command.as_deref() == Some("--check-source") {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--check-source requires a JSON path"))?;
        let data = model::Preview::load(std::path::Path::new(&path))?;
        let location = if let Some(position) = args.next() {
            let parts = position.rsplitn(3, ':').collect::<Vec<_>>();
            anyhow::ensure!(parts.len() == 3, "expected file:line:column");
            model::Location {
                file: parts[2].into(),
                line: parts[1].parse()?,
                column: parts[0].parse()?,
                file_hash: data.source_hash(parts[2])?,
            }
        } else {
            data.symbols
                .symbols
                .iter()
                .find(|s| s.kind == "function" && data.sources.contains_key(&s.location.file))
                .ok_or_else(|| anyhow::anyhow!("No captured function"))?
                .location
                .clone()
        };
        data.verify_location(&location)?;
        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let mut query = client::PositionQuery {
            file: location.file,
            line: location.line,
            column: location.column,
            file_hash: location.file_hash,
            snapshot: data.symbols.snapshot_id.clone(),
            epoch: data.symbols.epochs["semantic"].as_u64().unwrap_or(0),
        };
        let document = source::SourceDocument::new(&query.file, &data.sources[&query.file]);
        anyhow::ensure!(document.notice.is_none(), "{:?}", document.notice);
        let result = client::definitions(repository, &data.workspace, &query)?;
        anyhow::ensure!(
            !result.symbols.is_empty(),
            "Expected at least one definition candidate"
        );
        for definition in &result.symbols {
            data.verify_location(&definition.symbol.location)?;
        }
        println!(
            "{}:{}:{}: {} definition candidates; {} syntax-highlighted lines",
            query.file,
            query.line,
            query.column,
            result.symbols.len(),
            document.lines.len()
        );
        query.file_hash = "0".repeat(64);
        let error = client::definitions(repository, &data.workspace, &query)
            .err()
            .ok_or_else(|| anyhow::anyhow!("Engine accepted incorrect file hash"))?;
        anyhow::ensure!(error.to_string().contains("content_mismatch"), "{error:#}");
        println!("Fixed snapshot and content mismatch rejection verified");
        return Ok(());
    }
    if command.as_deref() == Some("--check-navigation") {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--check-navigation requires a JSON path"))?;
        let data = model::Preview::load(std::path::Path::new(&path))?;
        let requested = args.next();
        let selected = if let Some(name) = requested.as_deref() {
            Some(
                data.symbols
                    .symbols
                    .iter()
                    .find(|s| {
                        s.symbol_id == name || s.qualified_name.rsplit("::").next() == Some(name)
                    })
                    .ok_or_else(|| anyhow::anyhow!("symbol not found: {name}"))?,
            )
        } else {
            data.symbols
                .symbols
                .iter()
                .find(|s| s.qualified_name.ends_with("order::create"))
                .or_else(|| data.symbols.symbols.iter().find(|s| s.kind == "function"))
        };
        if let Some(symbol) = selected {
            for references in [false, true] {
                let query = client::Query {
                    symbol: symbol.symbol_id.clone(),
                    snapshot: data.symbols.snapshot_id.clone(),
                    epoch: data.symbols.epochs["semantic"].as_u64().unwrap_or(0),
                    direction: "both".into(),
                    depth: 2,
                    references,
                };
                let result = client::navigate(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap(),
                    &data.workspace,
                    &query,
                )?;
                println!(
                    "{}: {} {} relations",
                    symbol.qualified_name,
                    result.relations.len(),
                    if references { "reference" } else { "call" }
                );
            }
        }
        return Ok(());
    }
    if command.as_deref() == Some("--check-data") {
        let path = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--check-data requires a JSON path"))?;
        let data = model::Preview::load(std::path::Path::new(&path))?;
        println!(
            "{}: {} symbols, {} changes, {} grouped impacts, {} gaps",
            data.name,
            data.symbols.symbols.len(),
            data.impact.changes.len(),
            data.rows().len(),
            data.gaps().len()
        );
        return Ok(());
    }
    ui::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The GPUI desktop preview currently supports macOS.");
    std::process::exit(1);
}
