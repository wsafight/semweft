use crate::{
    config::{self, Resolved},
    engine::Engine,
    query::{self, QueryService},
};
use anyhow::{Context, Result, bail};
#[cfg(not(windows))]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::{
    ListenerNonblockingMode, ListenerOptions, Name, Stream, prelude::*,
};
use notify::{RecursiveMode, Watcher};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn file_signature(
    resolved: &Resolved,
    external: &[PathBuf],
) -> Result<BTreeMap<PathBuf, (u64, std::time::SystemTime)>> {
    let exclude = config::exclusions(&resolved.config.workspace.exclude)?;
    let root = resolved.root.clone();
    let cache = resolved.cache.clone();
    let index = resolved.index_dir.clone();
    let signature_roots = std::iter::once(root.clone())
        .chain(external.iter().cloned())
        .collect::<Vec<_>>();
    let mut builder = ignore::WalkBuilder::new(&root);
    for path in external {
        builder.add(path);
    }
    let walker = builder
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .git_ignore(resolved.config.workspace.respect_gitignore)
        .filter_entry(move |entry| {
            let path = entry.path();
            let relative = signature_roots
                .iter()
                .filter_map(|root| path.strip_prefix(root).ok())
                .min_by_key(|p| p.components().count())
                .unwrap_or(path);
            !matches!(
                entry.file_name().to_str(),
                Some(
                    ".git"
                        | ".semweft"
                        | "node_modules"
                        | ".next"
                        | ".nuxt"
                        | ".yarn"
                        | "dist"
                        | "coverage"
                )
            ) && !path.starts_with(&cache)
                && !path.starts_with(&index)
                && !exclude.is_match(relative)
                && !exclude.is_match(relative.join("__contents__"))
        })
        .build();
    let mut files = BTreeMap::new();
    for entry in walker {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy();
        if !entry.file_type().is_some_and(|t| t.is_file())
            || !(name.ends_with(".rs")
                || name.ends_with(".go")
                || crate::web::is_source(&name)
                || matches!(
                    name.as_ref(),
                    "Cargo.toml"
                        | "package.json"
                        | "package-lock.json"
                        | "pnpm-lock.yaml"
                        | "yarn.lock"
                        | "tsconfig.json"
                        | "jsconfig.json"
                        | "go.mod"
                        | "go.sum"
                        | "go.work"
                        | "go.work.sum"
                        | "Cargo.lock"
                        | "semweft.toml"
                        | "rust-toolchain"
                        | "rust-toolchain.toml"
                        | ".gitignore"
                        | ".ignore"
                        | "config.toml"
                ))
        {
            continue;
        }
        let meta = entry.metadata()?;
        files.insert(entry.path().to_owned(), (meta.len(), meta.modified()?));
    }
    Ok(files)
}

fn external_roots(engine: &Engine) -> Vec<PathBuf> {
    let state = engine.state.lock().unwrap();
    state
        .coordinator
        .latest()
        .and_then(|v| {
            v.snapshot
                .manifest
                .files
                .get(crate::dependencies::MAP)
                .cloned()
        })
        .and_then(|f| serde_json::from_str::<BTreeMap<String, String>>(&f.content).ok())
        .into_iter()
        .flat_map(|map| map.into_values().map(PathBuf::from))
        .filter(|p| !p.starts_with(&state.resolved.root))
        .collect()
}

pub use semweft_protocol::DaemonRequest as Request;

fn name(resolved: &Resolved) -> Result<Name<'static>> {
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        Ok(format!(
            "semweft-{}-{}",
            semweft_core::digest(user),
            resolved.workspace_id
        )
        .to_ns_name::<GenericNamespaced>()?
        .into_owned())
    }
    #[cfg(not(windows))]
    {
        let endpoint = endpoint(resolved);
        Ok(endpoint.to_fs_name::<GenericFilePath>()?.into_owned())
    }
}
#[cfg(not(windows))]
fn endpoint(resolved: &Resolved) -> PathBuf {
    let path = resolved.cache.join("daemon.sock");
    if path.as_os_str().len() < 100 {
        return path;
    }
    let user = std::env::var("USER").unwrap_or_default();
    PathBuf::from("/tmp")
        .join(format!("semweft-{}", &semweft_core::digest(user)[..16]))
        .join(format!("{}.sock", &resolved.workspace_id[..35]))
}
fn connect(resolved: &Resolved) -> Result<Stream> {
    Stream::connect(name(resolved)?).context("daemon_unavailable: cannot connect to shared daemon")
}
fn write_request(
    stream: &mut Stream,
    resolved: &Resolved,
    method: &str,
    args: Value,
) -> Result<()> {
    let request = Request {
        protocol: semweft_protocol::DAEMON_PROTOCOL.into(),
        workspace_id: resolved.workspace_id.clone(),
        config_hash: resolved.analysis_hash(),
        method: method.into(),
        args,
        request_id: None,
    };
    serde_json::to_writer(&mut *stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
fn read_response(stream: Stream) -> Result<(Stream, Value)> {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .context("daemon_unavailable: daemon disconnected")?;
    if line.is_empty() {
        bail!("daemon_unavailable: daemon closed connection");
    }
    let value =
        serde_json::from_str(&line).context("daemon_incompatible: invalid daemon response")?;
    Ok((stream.into_inner(), value))
}

pub fn ensure(resolved: &Resolved) -> Result<()> {
    if connect(resolved).is_ok() {
        return Ok(());
    }
    config::private_dir(&resolved.cache)?;
    let startup = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(resolved.cache.join("startup.lock"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match startup.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if connect(resolved).is_ok() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("daemon_unavailable: concurrent daemon startup timed out");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => bail!("daemon_unavailable: startup lock failed: {e}"),
        }
    }
    if connect(resolved).is_ok() {
        return Ok(());
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__daemon").arg("--workspace").arg(&resolved.root);
    if let Some(path) = &resolved.config_path {
        cmd.arg("--config").arg(path);
    }
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(resolved.cache.join("daemon.log"))?;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008 | 0x00000200);
    }
    let mut child = cmd
        .spawn()
        .context("daemon_unavailable: failed to spawn shared daemon")?;
    loop {
        if connect(resolved).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon_unavailable: daemon startup failed ({status}); inspect {}",
                resolved.cache.join("daemon.log").display()
            );
        }
        if Instant::now() >= deadline {
            bail!("daemon_unavailable: daemon startup timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn call(resolved: &Resolved, method: &str, args: Value) -> Result<Value> {
    ensure(resolved)?;
    let mut stream = connect(resolved)?;
    write_request(&mut stream, resolved, method, args)?;
    Ok(read_response(stream)?.1)
}
pub fn call_with_id(
    resolved: &Resolved,
    lease: &Mutex<Lease>,
    method: &str,
    args: Value,
    id: &str,
) -> Result<Value> {
    let mut stream = {
        let mut lease = lease.lock().unwrap();
        lease.reconnect(resolved)?;
        // Do not start another daemon here: this request must use the instance
        // on which this MCP session has just registered its watch lease.
        connect(resolved)?
    };
    let request = Request {
        protocol: semweft_protocol::DAEMON_PROTOCOL.into(),
        workspace_id: resolved.workspace_id.clone(),
        config_hash: resolved.analysis_hash(),
        method: method.into(),
        args,
        request_id: Some(id.into()),
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(read_response(stream)?.1)
}
pub struct Lease {
    stream: Stream,
}
impl Lease {
    pub fn reconnect(&mut self, resolved: &Resolved) -> Result<()> {
        self.stream.set_nonblocking(true)?;
        let status = self.stream.read(&mut [0]);
        self.stream.set_nonblocking(false)?;
        if !matches!(status, Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock) {
            *self = lease(resolved)?;
        }
        Ok(())
    }
}
pub fn lease(resolved: &Resolved) -> Result<Lease> {
    ensure(resolved)?;
    let mut stream = connect(resolved)?;
    write_request(&mut stream, resolved, "__lease", json!({}))?;
    let (stream, value) = read_response(stream)?;
    if value.get("error").is_some() {
        bail!("daemon_unavailable: lease rejected: {value}");
    }
    Ok(Lease { stream })
}
pub fn stop(resolved: &Resolved) -> Result<Value> {
    if connect(resolved).is_err() {
        return Ok(json!({"schema_version":"0.1","stopped":true}));
    }
    call(resolved, "__stop", json!({}))
}

struct Scanning(Arc<AtomicBool>);
impl Drop for Scanning {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
fn start_watcher(
    resolved: &Resolved,
    dirty: Arc<AtomicBool>,
    engine: Arc<Engine>,
) -> Result<Box<dyn Watcher>> {
    let handler = |dirty: Arc<AtomicBool>| {
        let cache = resolved.cache.clone();
        let index = resolved.index_dir.clone();
        let engine = engine.clone();
        move |event: notify::Result<notify::Event>| {
            if event.is_err() {
                engine.invalidate_inputs(&[], true);
                dirty.store(true, Ordering::SeqCst);
            }
            if let Ok(event) = event
                && (event.need_rescan() || !event.kind.is_access())
                && (event.need_rescan()
                    || event
                        .paths
                        .iter()
                        .any(|p| !p.starts_with(&cache) && !p.starts_with(&index)))
            {
                engine.invalidate_inputs(&event.paths, event.need_rescan());
                dirty.store(true, Ordering::SeqCst);
            }
        }
    };
    let native = notify::recommended_watcher(handler(dirty.clone())).and_then(|mut native| {
        native.watch(&resolved.root, RecursiveMode::Recursive)?;
        Ok(native)
    });
    let watcher: Box<dyn Watcher> = match native {
        Ok(native) => Box::new(native),
        _ => {
            eprintln!("Native file events unavailable; using notify content polling (500ms)");
            let mut polling = notify::PollWatcher::new(
                handler(dirty.clone()),
                notify::Config::default()
                    .with_poll_interval(Duration::from_millis(500))
                    .with_compare_contents(false),
            )?;
            polling.watch(&resolved.root, RecursiveMode::Recursive)?;
            Box::new(polling)
        }
    };
    Ok(watcher)
}

pub fn serve(resolved: Resolved) -> Result<()> {
    config::private_dir(&resolved.cache)?;
    config::private_dir(&resolved.index_dir)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(resolved.cache.join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(resolved.index_dir.join("writer.lock"))?;
    writer
        .try_lock()
        .map_err(|e| anyhow::anyhow!("config_conflict: index directory has another writer: {e}"))?;
    #[cfg(not(windows))]
    {
        let endpoint = endpoint(&resolved);
        config::private_dir(endpoint.parent().unwrap())?;
        if endpoint.exists() {
            fs::remove_file(&endpoint)?;
        }
        config::atomic_write(
            &resolved.cache.join("endpoint.json"),
            &serde_json::to_vec(
                &json!({"endpoint":endpoint,"protocol":"0.1","workspace_id":resolved.workspace_id}),
            )?,
        )?;
    }
    let listener = ListenerOptions::new()
        .name(name(&resolved)?)
        .nonblocking(ListenerNonblockingMode::Accept)
        .create_sync()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(endpoint(&resolved), fs::Permissions::from_mode(0o600))?;
    }
    let engine = Engine::new(resolved.clone())?;
    let service = Arc::new(QueryService::new(engine.clone()));
    let active = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(AtomicUsize::new(0));
    let stopping = Arc::new(AtomicBool::new(false));
    let dirty = Arc::new(AtomicBool::new(false));
    let scanning = Arc::new(AtomicBool::new(false));
    let mut watcher: Option<Box<dyn Watcher>> = None;
    let mut idle_since = Instant::now();
    let mut last_scan = Instant::now();
    let mut last_file_check = Instant::now();
    let mut last_reconcile = Instant::now();
    let mut signature = BTreeMap::new();
    loop {
        match listener.accept() {
            Ok(stream) => {
                active.fetch_add(1, Ordering::SeqCst);
                idle_since = Instant::now();
                let guard = Active(active.clone());
                let service = service.clone();
                let leases = leases.clone();
                let stopping = stopping.clone();
                let resolved = resolved.clone();
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(error) = handle(stream, &resolved, service, leases, stopping) {
                        eprintln!("IPC: {error:#}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        let indexed = engine.state.lock().unwrap().coordinator.latest().is_some();
        if leases.load(Ordering::SeqCst) > 0 && indexed && watcher.is_none() {
            let current = engine.state.lock().unwrap().resolved.clone();
            watcher = Some(start_watcher(&current, dirty.clone(), engine.clone())?);
            signature = file_signature(&current, &external_roots(&engine))?;
            dirty.store(true, Ordering::SeqCst);
        } else if leases.load(Ordering::SeqCst) == 0 && watcher.is_some() {
            watcher = None;
            signature.clear();
        }
        if watcher.is_some() && last_file_check.elapsed() > Duration::from_millis(500) {
            // Recover from dropped/coalesced native notifications without hashing build outputs.
            let current = engine.state.lock().unwrap().resolved.clone();
            match file_signature(&current, &external_roots(&engine)) {
                Ok(next) => {
                    if next != signature {
                        dirty.store(true, Ordering::SeqCst);
                        signature = next;
                    }
                }
                Err(error) => eprintln!("file metadata check: {error:#}"),
            }
            last_file_check = Instant::now();
        }
        if watcher.is_some() && last_reconcile.elapsed() >= Duration::from_secs(5) {
            engine.invalidate_inputs(&[], true);
            dirty.store(true, Ordering::SeqCst);
            last_reconcile = Instant::now();
        }
        if leases.load(Ordering::SeqCst) > 0
            && last_scan.elapsed() > Duration::from_millis(100)
            && !scanning.load(Ordering::SeqCst)
            && dirty.swap(false, Ordering::SeqCst)
        {
            let indexed = engine.state.lock().unwrap().coordinator.latest().is_some();
            if indexed {
                scanning.store(true, Ordering::SeqCst);
                let engine = engine.clone();
                let guard = Scanning(scanning.clone());
                let dirty = dirty.clone();
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(error) = engine.scan_incremental() {
                        eprintln!("index refresh: {error:#}");
                        dirty.store(true, Ordering::SeqCst);
                    }
                });
            }
            last_scan = Instant::now();
        }
        let (busy, idle_timeout) = {
            let state = engine.state.lock().unwrap();
            (
                state.refreshing,
                state.resolved.config.daemon.idle_timeout_ms,
            )
        };
        let busy = busy || scanning.load(Ordering::SeqCst);
        if active.load(Ordering::SeqCst) > 0 || busy {
            idle_since = Instant::now();
        }
        if (stopping.load(Ordering::SeqCst) && active.load(Ordering::SeqCst) == 0 && !busy)
            || (active.load(Ordering::SeqCst) == 0
                && !busy
                && idle_since.elapsed() > Duration::from_millis(idle_timeout))
        {
            engine.shutdown();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
fn handle(
    stream: Stream,
    resolved: &Resolved,
    service: Arc<QueryService>,
    leases: Arc<AtomicUsize>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    // macOS accepted descriptors may inherit the listener's O_NONBLOCK flag.
    stream.set_nonblocking(false)?;
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line)?;
    if line.is_empty() {
        return Ok(());
    }
    let request: Request =
        serde_json::from_str(&line).context("daemon_incompatible: invalid request")?;
    let deadline =
        Instant::now() + Duration::from_millis(request.args["timeout_ms"].as_u64().unwrap_or(5000));
    let result = (|| -> Result<Value> {
        if request.protocol != semweft_protocol::DAEMON_PROTOCOL
            || request.workspace_id != resolved.workspace_id
        {
            bail!("daemon_incompatible: protocol or workspace mismatch");
        }
        let updated =
            crate::config::resolve(Some(&resolved.root), resolved.config_path.as_deref())?;
        if request.config_hash != updated.analysis_hash() {
            bail!(
                "config_conflict: client analysis configuration differs from current workspace configuration"
            );
        }
        match request.method.as_str() {
            "__cancel" => {
                let id = request.args["id"]
                    .as_str()
                    .context("invalid_argument: cancellation requires id")?;
                service.cancel(id);
                Ok(json!({"schema_version":"0.1","cancelled":true}))
            }
            "__trust_changed" => {
                service.engine.trust_changed()?;
                Ok(json!({"schema_version":"0.1","updated":true}))
            }
            "__snapshots" => service.engine.snapshots(&request.args),
            "__lease" => Ok(json!({"schema_version":"0.1","connected":true})),
            "__stop" => {
                if leases.load(Ordering::SeqCst) > 0 {
                    bail!("config_conflict: active MCP or watch clients prevent shutdown");
                }
                stopping.store(true, Ordering::SeqCst);
                Ok(json!({"schema_version":"0.1","stopped":true}))
            }
            method => service.execute_registered(
                request.request_id.as_deref(),
                method,
                request.args,
                deadline,
            ),
        }
    })();
    let mut value = match result {
        Ok(value) => value,
        Err(error) => query::error_value(&error),
    };
    if request.method == "index_status" {
        value["clients"] =
            json!({"watch_leases":leases.load(Ordering::SeqCst),"daemon_pid":std::process::id()});
    }
    // Register before acknowledging, so a recovered bridge cannot query a
    // daemon which still reports zero clients.
    let _lease = if request.method == "__lease" && value.get("error").is_none() {
        leases.fetch_add(1, Ordering::SeqCst);
        Some(Active(leases))
    } else {
        None
    };
    serde_json::to_writer(stream.get_mut(), &value)?;
    stream.get_mut().write_all(b"\n")?;
    stream.get_mut().flush()?;
    if request.method == "__lease" && value.get("error").is_none() {
        line.clear();
        let _ = stream.read_line(&mut line);
    }
    Ok(())
}
