use semweft_core::{AnalysisView, Certainty, Position, Relation, Symbol, digest};
use semweft_query::{Engine, config, engine, frontend::Frontends};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    schema_version: String,
    annotation_method: String,
    repositories: Vec<Repository>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repository {
    id: String,
    repository: String,
    revision: String,
    language: String,
    files: BTreeMap<String, String>,
    labels: Vec<Label>,
    edit: Edit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    file: String,
    old: String,
    new: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    file: String,
    text: String,
    token: String,
    occurrence: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    id: String,
    dispatch: String,
    call: Anchor,
    target: Option<Anchor>,
    rationale: String,
}

fn corpus() -> Corpus {
    let value: Corpus = serde_json::from_str(include_str!(
        "../../../../examples/language-acceptance/corpus.json"
    ))
    .unwrap();
    assert_eq!(value.schema_version, "0.1");
    for repository in &value.repositories {
        for label in &repository.labels {
            assert!(matches!(
                label.dispatch.as_str(),
                "direct" | "virtual" | "parameter"
            ));
            assert_eq!(label.target.is_some(), label.dispatch != "parameter");
        }
    }
    value
}

fn relative(path: &str) -> &Path {
    let path = Path::new(path);
    assert!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "invalid corpus path: {path:?}"
    );
    path
}

fn position(root: &Path, anchor: &Anchor) -> Position {
    let text = fs::read_to_string(root.join(relative(&anchor.file))).unwrap();
    let matches: Vec<_> = text.match_indices(&anchor.text).collect();
    if anchor.occurrence.is_none() {
        assert_eq!(matches.len(), 1, "ambiguous anchor: {}", anchor.text);
    }
    let start = matches[anchor.occurrence.unwrap_or(0)].0;
    let token = anchor.text.find(&anchor.token).unwrap();
    let offset = start + token;
    let prefix = &text[..offset];
    Position {
        line: prefix.bytes().filter(|byte| *byte == b'\n').count() as u32 + 1,
        column: prefix.rsplit('\n').next().unwrap().len() as u32 + 1,
    }
}

fn interval(success: usize, total: usize) -> Value {
    if total == 0 {
        return Value::Null;
    }
    let count = total as f64;
    let rate = success as f64 / count;
    let squared = 1.959963984540054_f64.powi(2);
    let denominator = 1. + squared / count;
    let center = (rate + squared / (2. * count)) / denominator;
    let radius = (squared * (rate * (1. - rate) / count + squared / (4. * count * count))).sqrt()
        / denominator;
    json!({"success":success,"total":total,"rate":rate,
        "wilson_95":[(center-radius).max(0.),(center+radius).min(1.)]})
}

fn distribution(samples: &[f64]) -> Value {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |quantile: f64| {
        sorted[((sorted.len() as f64 * quantile).ceil() as usize).saturating_sub(1)]
    };
    json!({"count":sorted.len(),"p50":percentile(0.5),"p95":percentile(0.95),
        "p99":percentile(0.99),"max":sorted.last()})
}

fn score(labels: &[Value], source: &str) -> Value {
    let direct: Vec<_> = labels
        .iter()
        .filter(|label| label["dispatch"] == "direct")
        .collect();
    let mut definite = 0;
    let mut correct = 0;
    let mut recalled = 0;
    let mut candidates = 0;
    let mut candidate_errors = 0;
    let mut unresolved = 0;
    let mut unsafe_definite = 0;
    let mut bindings = 0;
    let known = labels
        .iter()
        .filter(|label| label["target"].is_object())
        .count();
    for label in labels {
        let observations = label["observations"][source].as_array().unwrap();
        unresolved += usize::from(observations.is_empty());
        let mut hit = false;
        bindings += usize::from(
            observations
                .iter()
                .any(|observation| observation["correct"] == true),
        );
        for observation in observations {
            if label["dispatch"] != "direct" {
                unsafe_definite += usize::from(observation["certainty"] == "definite");
                continue;
            }
            if observation["certainty"] == "definite" {
                definite += 1;
                correct += usize::from(observation["correct"] == true);
                hit |= observation["correct"] == true;
            } else {
                candidates += 1;
                candidate_errors += usize::from(observation["correct"] != true);
            }
        }
        recalled += usize::from(hit);
    }
    json!({"direct_labels":direct.len(),"precision":interval(correct,definite),
        "definite_recall":interval(recalled,direct.len()),
        "candidate_error_rate":interval(candidate_errors,candidates),
        "unresolved_rate":interval(unresolved,labels.len()),
        "virtual_labels":labels.iter().filter(|label|label["dispatch"] == "virtual").count(),
        "parameter_labels":labels.iter().filter(|label|label["dispatch"] == "parameter").count(),
        "declaration_binding_recall":interval(bindings,known),"unsafe_definite":unsafe_definite,
        "candidate_error_scope":"source-reviewed direct labels only; uncertain target sets are unjudged"})
}

fn observations(
    root: &Path,
    labels: &[Label],
    view: &AnalysisView,
    relations: &[Relation],
) -> Vec<Value> {
    labels
        .iter()
        .map(|label| {
            let call = position(root, &label.call);
            let target = label.target.as_ref().map(|anchor| {
                let point = position(root, anchor);
                assert!(view.snapshot.syntax.symbols.iter().any(|symbol|
                    symbol.file == anchor.file && symbol.name_span.start == point),
                    "label target missing from syntax: {}", label.id);
                (anchor.file.as_str(), point)
            });
            let symbols: BTreeMap<_, _> = view
                .snapshot
                .syntax
                .symbols
                .iter()
                .map(|symbol| (symbol.symbol_id.as_str(), symbol))
                .collect();
            let mut by_source = BTreeMap::<String, Vec<Value>>::new();
            for source in ["L1", "L3"] {
                let mut seen = BTreeSet::new();
                let mut observations = vec![];
                for relation in relations.iter().filter(|relation| {
                    relation.kind == "call"
                        && relation.freshness == "current"
                        && relation.file == label.call.file
                        && relation.span.start == call
                }) {
                    let evidence: Vec<_> = relation
                        .evidence
                        .iter()
                        .filter(|evidence| evidence.source == source)
                        .collect();
                    if evidence.is_empty() {
                        continue;
                    }
                    let symbol = symbols[relation.to_symbol.as_str()];
                    let definite = evidence
                        .iter()
                        .any(|evidence| evidence.certainty == Certainty::Definite);
                    if !seen.insert((symbol.symbol_id.as_str(), definite)) {
                        continue;
                    }
                    observations.push(json!({"target":coordinate(symbol),
                        "certainty":if definite {"definite"} else {"candidate"},
                        "correct":target.as_ref().map(|(file,position)|
                            symbol.file == *file && symbol.name_span.start == *position),
                        "relation_id":relation.relation_id,"evidence":evidence}));
                }
                by_source.insert(source.into(), observations);
            }
            json!({"id":label.id,"dispatch":label.dispatch,"call":{"file":label.call.file,"position":call},
                "target":target.map(|(file,position)|json!({"file":file,"position":position})),
                "rationale":label.rationale,"observations":by_source})
        })
        .collect()
}

fn coordinate(symbol: &Symbol) -> Value {
    json!({"file":symbol.file,"position":symbol.name_span.start,"name":symbol.name})
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(180)
}

fn current(engine: &Engine) -> (Arc<AnalysisView>, Vec<Relation>) {
    engine.index(&json!({}), deadline()).unwrap();
    let (stored, _) = engine.bind(&json!({}), deadline()).unwrap();
    assert!(stored.view.pending_units.is_empty());
    let relations = engine::relations(&stored);
    (stored.view, relations)
}

fn identities(relations: &[Relation]) -> BTreeSet<String> {
    relations
        .iter()
        .filter(|relation| relation.freshness == "current")
        .map(|relation| {
            let sources: BTreeSet<_> = relation
                .evidence
                .iter()
                .map(|evidence| format!("{}:{:?}", evidence.source, evidence.certainty))
                .collect();
            serde_json::to_string(&json!({"id":relation.relation_id,
                "certainty":relation.certainty,"source_certainties":sources}))
            .unwrap()
        })
        .collect()
}

struct Session(Arc<Engine>);

impl std::ops::Deref for Session {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

fn settings(root: &Path, temporary: &Path, name: &str) -> config::Resolved {
    let path = temporary.join(format!("{name}.toml"));
    fs::write(
        &path,
        format!(
            "schema_version=\"0.1\"\n[workspace]\nroot={:?}\n[index]\ndirectory={:?}\n",
            root.to_string_lossy(),
            temporary.join(name).to_string_lossy()
        ),
    )
    .unwrap();
    config::resolve(Some(root), Some(&path)).unwrap()
}

fn prepare(repository: &Repository, sources: &Path, root: &Path) -> Option<Value> {
    for (path, expected) in &repository.files {
        let source = sources.join(&repository.id).join(relative(path));
        assert!(
            !fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let hash = Command::new("git")
            .args(["hash-object", "--no-filters"])
            .arg(&source)
            .output()
            .unwrap();
        assert!(hash.status.success());
        assert_eq!(
            String::from_utf8(hash.stdout).unwrap().trim(),
            expected,
            "source mismatch: {} {path}",
            repository.id
        );
        let destination = root.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(source, destination).unwrap();
    }
    let overlay = if repository.language != "go" {
        Some(json!({"compilerOptions":{"strict":true,"target":"es2018",
            "moduleResolution":"node","lib":["es2018","dom"],"skipLibCheck":true,
            "allowJs":repository.language == "javascript","checkJs":repository.language == "javascript"},
            "include":["src/**/*"]}))
    } else {
        None
    };
    if let Some(overlay) = &overlay {
        fs::write(
            root.join("tsconfig.json"),
            serde_json::to_vec_pretty(overlay).unwrap(),
        )
        .unwrap();
    }
    overlay
}

fn evaluate(repository: &Repository, sources: &Path) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let overlay = prepare(repository, sources, &root);
    let resolved = settings(&root, temp.path(), "warm");
    resolved.trust(false).unwrap();
    let engine = Session(Engine::new(resolved).unwrap());
    let start = Instant::now();
    let (baseline, relations) = current(&engine);
    let initial_ms = start.elapsed().as_secs_f64() * 1000.;
    let labels = observations(&root, &repository.labels, &baseline, &relations);
    let mut parse_samples = vec![];
    let mut file_sizes = BTreeMap::new();
    for (path, file) in &baseline.snapshot.manifest.files {
        if !(path.ends_with(".go") || path.ends_with(".ts") || path.ends_with(".js")) {
            continue;
        }
        let mut manifest = baseline.snapshot.manifest.clone();
        manifest.files.retain(|name, _| name == path);
        file_sizes.insert(
            path,
            json!({"bytes":file.content.len(),"lines":file.content.lines().count()}),
        );
        for _ in 0..10 {
            let mut parser = Frontends::new().unwrap();
            let start = Instant::now();
            parser.analyze(&manifest).unwrap();
            parse_samples.push(start.elapsed().as_secs_f64() * 1000.);
        }
    }
    let mut stages = vec![];
    for step in ["body", "build_configuration"] {
        let path = root.join(if step == "body" {
            &repository.edit.file
        } else if repository.language == "go" {
            "go.mod"
        } else {
            "tsconfig.json"
        });
        let original = fs::read_to_string(&path).unwrap_or_default();
        let edited = if step == "body" {
            assert_eq!(original.matches(&repository.edit.old).count(), 1);
            original.replacen(&repository.edit.old, &repository.edit.new, 1)
        } else if repository.language == "go" {
            changed_go_directive(&original)
        } else {
            let mut config: Value = serde_json::from_str(&original).unwrap();
            config["compilerOptions"]["strict"] = json!(false);
            serde_json::to_string_pretty(&config).unwrap()
        };
        fs::write(&path, edited).unwrap();
        let start = Instant::now();
        engine.scan(false).unwrap();
        let syntax_ms = start.elapsed().as_secs_f64() * 1000.;
        let (warm, refreshed) = current(&engine);
        let refresh_ms = start.elapsed().as_secs_f64() * 1000.;
        let cold_config = settings(&root, temp.path(), &format!("cold-{step}"));
        let cold = Session(Engine::new(cold_config).unwrap());
        let (cold_view, cold_relations) = current(&cold);
        let warm_ids = identities(&refreshed);
        let cold_ids = identities(&cold_relations);
        stages.push(json!({"edit":step,"syntax_publish_ms":syntax_ms,
            "semantic_current_ms":refresh_ms,"warm_snapshot":warm.snapshot.snapshot_id,
            "cold_snapshot":cold_view.snapshot.snapshot_id,"warm_relations":warm_ids.len(),
            "cold_relations":cold_ids.len(),"warm_only":warm_ids.difference(&cold_ids).collect::<Vec<_>>(),
            "cold_only":cold_ids.difference(&warm_ids).collect::<Vec<_>>()}));
        cold.shutdown();
    }
    engine.shutdown();
    json!({"id":repository.id,"repository":repository.repository,"revision":repository.revision,
        "language":repository.language,"projection_git_blob_ids":repository.files,
        "build_overlay":overlay,"snapshot":baseline.snapshot.snapshot_id,
        "input_identity":digest(serde_json::to_vec(&baseline.snapshot.manifest).unwrap()),
        "analysis_versions":baseline.snapshot.manifest.analysis_versions,
        "known_gaps":baseline.known_gaps,"file_sizes":file_sizes,"labels":labels,
        "metrics":{"L1":score(&labels,"L1"),"L3":score(&labels,"L3")},
        "initial_semantic_current_ms":initial_ms,"cold_file_parse_ms":distribution(&parse_samples),
        "cold_file_parse_samples_ms":parse_samples,"refresh_steps":stages})
}

fn changed_go_directive(original: &str) -> String {
    let mut lines: Vec<_> = original
        .lines()
        .filter(|line| !line.starts_with("go "))
        .collect();
    lines.push("go 1.22");
    format!("{}\n", lines.join("\n"))
}

struct WatchClient(std::process::Child);

impl Drop for WatchClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn watched_current(
    workspace: &super::Workspace,
    watcher: &mut WatchClient,
    previous: &str,
    expected: &BTreeMap<String, String>,
    started: Instant,
) -> (Value, f64, f64) {
    let mut syntax_ms = None;
    loop {
        assert!(
            watcher.0.try_wait().unwrap().is_none(),
            "watch client exited"
        );
        let status = workspace.ok(&["status"]);
        assert_ne!(status["refresh"]["state"], "failed", "{status}");
        let changed = status["snapshot_id"]
            .as_str()
            .is_some_and(|id| id != previous)
            && expected.iter().all(|(path, hash)| {
                status["manifest"]["files"].as_array().is_some_and(|files| {
                    files
                        .iter()
                        .any(|file| file["path"] == *path && file["file_hash"] == *hash)
                })
            });
        if changed {
            let elapsed = started.elapsed().as_secs_f64() * 1000.;
            let syntax = *syntax_ms.get_or_insert(elapsed);
            if status["refresh"]["state"] == "current"
                && status["epochs"]["syntax"] == status["epochs"]["semantic"]
                && status["clients"]["watch_leases"].as_u64().unwrap_or(0) > 0
            {
                return (status, syntax, elapsed);
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "watch timed out: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn selected_calls(
    workspace: &super::Workspace,
    path: &str,
    name: &str,
    snapshot: &str,
) -> BTreeSet<String> {
    let search = workspace.ok(&[
        "search",
        name,
        "--match",
        "exact",
        "--path",
        path,
        "--snapshot-id",
        snapshot,
    ]);
    let symbols = search["symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 1, "{search}");
    let calls = workspace.ok(&[
        "calls",
        symbols[0]["symbol_id"].as_str().unwrap(),
        "--direction",
        "callees",
        "--depth",
        "3",
        "--all-pages",
        "--snapshot-id",
        snapshot,
    ]);
    assert!(
        !calls["relations"].as_array().unwrap().is_empty(),
        "{calls}"
    );
    calls["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|relation| {
            let sources: BTreeSet<_> = relation["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|evidence| format!("{}:{}", evidence["source"], evidence["certainty"]))
                .collect();
            serde_json::to_string(&json!({"relation_id":relation["relation_id"],
            "certainty":relation["certainty"],"freshness":relation["freshness"],
            "source_certainties":sources}))
            .unwrap()
        })
        .collect()
}

#[test]
#[ignore = "Requires nine pinned source projections, real SCIP tools and Go; measures real CLI watchers"]
fn repository_watchers_match_cold_after_body_rename_and_build_changes() {
    let sources = PathBuf::from(
        std::env::var_os("SEMWEFT_ACCEPTANCE_ROOT").expect("set SEMWEFT_ACCEPTANCE_ROOT"),
    );
    let mut reports = vec![];
    for repository in corpus().repositories {
        eprintln!("Watcher acceptance: {}", repository.id);
        let workspace = super::Workspace::new();
        fs::remove_file(workspace.root.join("Cargo.toml")).unwrap();
        fs::remove_file(workspace.root.join("src/lib.rs")).unwrap();
        let overlay = prepare(&repository, &sources, &workspace.root);
        if repository.language == "go" {
            fs::write(
                workspace.root.join("semweft.toml"),
                "schema_version='0.1'\n[go]\ntags=[]\n",
            )
            .unwrap();
        }
        workspace.ok(&["trust", "--accept-build-execution"]);
        let log = workspace.temp.path().join("watch.log");
        let mut watcher = WatchClient(
            workspace
                .command()
                .args(["index", "--watch"])
                .stdout(std::process::Stdio::null())
                .stderr(fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let startup = Instant::now();
        loop {
            assert!(
                watcher.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            if fs::read_to_string(&log)
                .unwrap()
                .contains("Index ready; watching")
            {
                break;
            }
            assert!(
                startup.elapsed() < Duration::from_secs(180),
                "{}",
                fs::read_to_string(&log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let (mut baseline, _, _) =
            watched_current(&workspace, &mut watcher, "", &BTreeMap::new(), startup);
        let (path, original_name, rename, renamed) = match repository.id.as_str() {
            "uuid" => ("version4.go", "New", "NewUUID", "AcceptanceNewUUID"),
            "xxhash" => ("xxhash.go", "New", "NewWithSeed", "AcceptanceNewWithSeed"),
            "pflag" => ("int.go", "GetInt", "getFlagType", "acceptanceGetFlagType"),
            "resolve-uri" => (
                "src/resolve-uri.ts",
                "resolve",
                "parseUrl",
                "acceptanceParseUrl",
            ),
            "sourcemap-codec" => (
                "src/sourcemap-codec.ts",
                "decode",
                "decodeInteger",
                "acceptanceDecodeInteger",
            ),
            "trace-mapping" => (
                "src/binary-search.ts",
                "memoizedBinarySearch",
                "memoizedBinarySearch",
                "acceptanceMemoizedBinarySearch",
            ),
            "clsx" => ("src/index.js", "clsx", "toVal", "acceptanceToVal"),
            "mri" => ("src/index.js", "default", "toArr", "acceptanceToArr"),
            "klona" => ("src/index.js", "klona", "klona", "acceptanceKlona"),
            other => panic!("missing watcher edit plan for {other}"),
        };
        let mut root_name = original_name;
        let mut stages = vec![];
        for step in ["body", "rename", "build_configuration"] {
            let mut edits = BTreeMap::new();
            match step {
                "body" => {
                    let text =
                        fs::read_to_string(workspace.root.join(&repository.edit.file)).unwrap();
                    assert_eq!(text.matches(&repository.edit.old).count(), 1);
                    edits.insert(
                        repository.edit.file.clone(),
                        text.replacen(&repository.edit.old, &repository.edit.new, 1),
                    );
                }
                "rename" => {
                    for file in repository.files.keys().filter(|file| {
                        file.ends_with(".go") || file.ends_with(".ts") || file.ends_with(".js")
                    }) {
                        let text = fs::read_to_string(workspace.root.join(file)).unwrap();
                        if text.contains(rename) {
                            edits.insert(file.clone(), text.replace(rename, renamed));
                        }
                    }
                    assert!(!edits.is_empty());
                    if original_name == rename {
                        root_name = renamed;
                    }
                }
                _ if repository.language == "go" => {
                    edits.insert(
                        "semweft.toml".into(),
                        "schema_version='0.1'\n[go]\ntags=['purego']\n".into(),
                    );
                }
                _ => {
                    let mut configuration = overlay.clone().unwrap();
                    configuration["compilerOptions"]["target"] = json!("es2020");
                    edits.insert(
                        "tsconfig.json".into(),
                        serde_json::to_string_pretty(&configuration).unwrap(),
                    );
                }
            }
            let expected = edits
                .iter()
                .map(|(file, text)| (file.clone(), digest(text.as_bytes())))
                .collect();
            let started = Instant::now();
            for (file, text) in &edits {
                fs::write(workspace.root.join(file), text).unwrap();
            }
            let (observed, syntax_ms, current_ms) = watched_current(
                &workspace,
                &mut watcher,
                baseline["snapshot_id"].as_str().unwrap(),
                &expected,
                started,
            );
            let warm = selected_calls(
                &workspace,
                path,
                root_name,
                observed["snapshot_id"].as_str().unwrap(),
            );
            let cold = super::Workspace::new();
            fs::remove_file(cold.root.join("Cargo.toml")).unwrap();
            fs::remove_file(cold.root.join("src/lib.rs")).unwrap();
            for file in repository
                .files
                .keys()
                .map(String::as_str)
                .chain(["semweft.toml", "tsconfig.json"])
            {
                if workspace.root.join(file).is_file() {
                    let destination = cold.root.join(file);
                    fs::create_dir_all(destination.parent().unwrap()).unwrap();
                    fs::copy(workspace.root.join(file), destination).unwrap();
                }
            }
            cold.ok(&["trust", "--accept-build-execution"]);
            let cold_status = cold.ok(&["index"]);
            let cold_relations = selected_calls(
                &cold,
                path,
                root_name,
                cold_status["snapshot_id"].as_str().unwrap(),
            );
            stages.push(
                json!({"edit":step,"changed_hashes":expected,"root":{"file":path,"name":root_name},
                "watch_to_syntax_ms":syntax_ms,"watch_to_current_ms":current_ms,
                "warm_status":observed,"cold_snapshot":cold_status["snapshot_id"],
                "warm_relations":warm.len(),"cold_relations":cold_relations.len(),
                "warm_identities":warm,"cold_identities":cold_relations,
                "warm_only":warm.difference(&cold_relations).collect::<Vec<_>>(),
                "cold_only":cold_relations.difference(&warm).collect::<Vec<_>>() }),
            );
            baseline = observed;
        }
        let build = if repository.language == "go" {
            let output = Command::new("go")
                .current_dir(&workspace.root)
                .env("GOFLAGS", "-tags=purego")
                .env("GOPROXY", "off")
                .env("GOSUMDB", "off")
                .args(["test", "./..."])
                .output()
                .unwrap();
            Some(
                json!({"command":"GOFLAGS=-tags=purego GOPROXY=off GOSUMDB=off go test ./...", "exit_code":output.status.code(),
                "stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)}),
            )
        } else {
            None
        };
        drop(watcher);
        reports.push(
            json!({"id":repository.id,"language":repository.language,"revision":repository.revision,
            "steps":stages,"projection_build":build,"watch_log":fs::read_to_string(&log).unwrap()}),
        );
    }
    let report = json!({"schema_version":"0.1","profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "logical_cpus":std::thread::available_parallelism().map(|count|count.get()).ok(),
        "measured_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        "cargo_lock_id":digest(include_bytes!("../../../../Cargo.lock")),
        "scope":"selected root callees to depth 3, production-source projections; status-only polling during watcher measurements; 50ms polling and CLI overhead included; no full graph equivalence claim",
        "corpus_id":digest(include_bytes!("../../../../examples/language-acceptance/corpus.json")),"repositories":reports});
    if let Some(path) = std::env::var_os("SEMWEFT_WATCH_ACCEPTANCE_REPORT") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    for repository in report["repositories"].as_array().unwrap() {
        if !repository["projection_build"].is_null() {
            assert_eq!(
                repository["projection_build"]["exit_code"], 0,
                "{repository}"
            );
        }
        for step in repository["steps"].as_array().unwrap() {
            assert!(
                step["warm_only"].as_array().unwrap().is_empty()
                    && step["cold_only"].as_array().unwrap().is_empty(),
                "{repository}"
            );
        }
    }
}

#[test]
fn acceptance_statistics_keep_missing_evidence_and_wrong_candidates_visible() {
    assert!(interval(0, 0).is_null());
    assert_eq!(interval(2, 2)["rate"], 1.);
    assert!(interval(2, 2)["wilson_95"][0].as_f64().unwrap() < 0.35);
    let labels = vec![
        json!({"dispatch":"direct","target":{},"observations":{"L3":[{"certainty":"definite","correct":true},
            {"certainty":"candidate","correct":false}]}}),
        json!({"dispatch":"direct","target":{},"observations":{"L3":[]}}),
        json!({"dispatch":"parameter","target":null,"observations":{"L3":[{"certainty":"definite","correct":null}]}}),
    ];
    let result = score(&labels, "L3");
    assert_eq!(result["precision"]["rate"], 1.);
    assert_eq!(result["definite_recall"]["rate"], 0.5);
    assert_eq!(result["candidate_error_rate"]["rate"], 1.);
    assert_eq!(result["unsafe_definite"], 1);
    assert_eq!(result["unresolved_rate"]["success"], 1);
}

#[test]
fn acceptance_matches_source_coordinates_and_filters_evidence_before_scoring() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"sample\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "fn target() {}\nfn entry() { target(); }\n",
    )
    .unwrap();
    let engine = Session(Engine::new(settings(&root, temporary.path(), "syntax")).unwrap());
    engine.scan(false).unwrap();
    let (stored, _) = engine.bind(&json!({}), deadline()).unwrap();
    let label = Label {
        id: "direct".into(),
        dispatch: "direct".into(),
        call: Anchor {
            file: "src/lib.rs".into(),
            text: "target();".into(),
            token: "target".into(),
            occurrence: None,
        },
        target: Some(Anchor {
            file: "src/lib.rs".into(),
            text: "fn target()".into(),
            token: "target".into(),
            occurrence: None,
        }),
        rationale: "Named direct call to a local declaration".into(),
    };
    let mut relations = engine::relations(&stored);
    let labels = observations(
        &root,
        std::slice::from_ref(&label),
        &stored.view,
        &relations,
    );
    assert_eq!(labels[0]["observations"]["L1"].as_array().unwrap().len(), 1);
    assert_eq!(score(&labels, "L1")["candidate_error_rate"]["rate"], 0.);
    assert_eq!(score(&labels, "L3")["definite_recall"]["rate"], 0.);
    let mut evidence = relations[0].evidence[0].clone();
    evidence.source = "L3".into();
    evidence.certainty = Certainty::Definite;
    relations[0].evidence.push(evidence);
    let labels = observations(
        &root,
        std::slice::from_ref(&label),
        &stored.view,
        &relations,
    );
    assert_eq!(score(&labels, "L3")["definite_recall"]["rate"], 1.);
    assert_eq!(score(&labels, "L1")["definite_recall"]["rate"], 0.);
    for relation in &mut relations {
        relation.freshness = "stale".into();
    }
    let labels = observations(&root, &[label], &stored.view, &relations);
    assert_eq!(score(&labels, "L3")["unresolved_rate"]["rate"], 1.);
}

#[test]
#[ignore = "Requires nine pinned source projections and real SCIP tools; trusts temporary copies only"]
fn source_annotated_go_and_typescript_repositories() {
    if std::env::var_os("SEMWEFT_ACCEPTANCE_CHILD").is_none() {
        let temporary = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acceptance::source_annotated_go_and_typescript_repositories",
                "--ignored",
                "--nocapture",
            ])
            .env("SEMWEFT_ACCEPTANCE_CHILD", "1")
            .env("SEMWEFT_CACHE_DIR", temporary.path().join("cache"))
            .env("SEMWEFT_CONFIG_DIR", temporary.path().join("config"))
            .status()
            .unwrap();
        assert!(status.success(), "isolated acceptance process failed");
        return;
    }
    let corpus = corpus();
    let sources = PathBuf::from(
        std::env::var_os("SEMWEFT_ACCEPTANCE_ROOT")
            .expect("set SEMWEFT_ACCEPTANCE_ROOT to the repository source directories"),
    );
    let mut reports = vec![];
    for repository in &corpus.repositories {
        eprintln!("Acceptance: {} at {}", repository.id, repository.revision);
        reports.push(evaluate(repository, &sources));
    }
    let mut languages = BTreeMap::new();
    for language in ["go", "typescript", "javascript"] {
        let repositories: Vec<_> = reports
            .iter()
            .filter(|report| report["language"] == language)
            .collect();
        assert!(repositories.len() >= 3);
        let labels: Vec<_> = repositories
            .iter()
            .flat_map(|report| report["labels"].as_array().unwrap().iter().cloned())
            .collect();
        let samples: Vec<_> = repositories
            .iter()
            .flat_map(|report| {
                report["cold_file_parse_samples_ms"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|sample| sample.as_f64().unwrap())
            })
            .collect();
        let syntax: Vec<_> = repositories
            .iter()
            .flat_map(|report| {
                report["refresh_steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|step| step["syntax_publish_ms"].as_f64().unwrap())
            })
            .collect();
        let semantic: Vec<_> = repositories
            .iter()
            .flat_map(|report| {
                report["refresh_steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|step| step["semantic_current_ms"].as_f64().unwrap())
            })
            .collect();
        languages.insert(
            language,
            json!({"repositories":repositories.len(),
            "L1":score(&labels,"L1"),"L3":score(&labels,"L3"),
            "cold_file_parse_ms":distribution(&samples),
            "syntax_publish_ms":distribution(&syntax),
            "semantic_current_ms":distribution(&semantic)}),
        );
    }
    let report = json!({"schema_version":"0.1","annotation_method":corpus.annotation_method,
        "acceptance":"preliminary_source_reviewed_sample; independent human audit and remote CI pending",
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "logical_cpus":std::thread::available_parallelism().map(|count|count.get()).ok(),
        "corpus_id":digest(include_bytes!("../../../../examples/language-acceptance/corpus.json")),
        "cargo_lock_id":digest(include_bytes!("../../../../Cargo.lock")),
        "measured_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        "profile":if cfg!(debug_assertions) {"debug"} else {"release"},
        "os_version":Command::new("uname").args(["-s","-r"]).output().ok().map(|output|
            String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        "mac_hardware":if cfg!(target_os="macos") {Command::new("sysctl")
            .args(["-n","hw.model","hw.memsize","machdep.cpu.brand_string"])
            .output().ok().map(|output|String::from_utf8_lossy(&output.stdout).trim().to_owned())} else {None},
        "linux_cpu":fs::read_to_string("/proc/cpuinfo").ok().and_then(|text|
            text.lines().find(|line|line.starts_with("model name")).map(str::to_owned)),
        "linux_memory":fs::read_to_string("/proc/meminfo").ok().and_then(|text|
            text.lines().find(|line|line.starts_with("MemTotal:")).map(str::to_owned)),
        "consistency_basis":"relation_id, certainty and evidence source/certainty; raw producer-output hashes remain provenance and may vary between builds",
        "languages":languages,"repositories":reports});
    if let Some(path) = std::env::var_os("SEMWEFT_ACCEPTANCE_REPORT") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    for repository in report["repositories"].as_array().unwrap() {
        for step in repository["refresh_steps"].as_array().unwrap() {
            assert!(
                step["warm_only"].as_array().unwrap().is_empty()
                    && step["cold_only"].as_array().unwrap().is_empty(),
                "{} {}: {} warm-only, {} cold-only; see SEMWEFT_ACCEPTANCE_REPORT",
                repository["id"],
                step["edit"],
                step["warm_only"].as_array().unwrap().len(),
                step["cold_only"].as_array().unwrap().len()
            );
        }
    }
    for (language, metrics) in &languages {
        assert!(
            metrics["L3"]["precision"]["rate"].as_f64().unwrap_or(0.) >= 0.99,
            "{language}: {metrics}"
        );
        assert!(
            metrics["L3"]["definite_recall"]["rate"]
                .as_f64()
                .unwrap_or(0.)
                >= 0.95,
            "{language}: {metrics}"
        );
        assert_eq!(metrics["L3"]["unsafe_definite"], 0, "{language}: {metrics}");
    }
    eprintln!("Source-reviewed sample metrics: {}", report["languages"]);
}
