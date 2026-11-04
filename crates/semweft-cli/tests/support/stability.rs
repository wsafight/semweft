use super::*;
#[cfg(all(feature = "go-experimental", unix))]
use std::path::Path;
use std::time::Instant;

pub(super) struct WatchClient {
    child: Child,
    stdout: tempfile::NamedTempFile,
    stderr: tempfile::NamedTempFile,
}

pub(super) fn diagnostics(ws: &Workspace) -> String {
    fs::read_dir(ws.temp.path().join("cache/workspaces"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("daemon.log")).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

impl WatchClient {
    pub(super) fn start(ws: &Workspace) -> Self {
        let stdout = tempfile::NamedTempFile::new_in(ws.temp.path()).unwrap();
        let stderr = tempfile::NamedTempFile::new_in(ws.temp.path()).unwrap();
        let child = ws
            .command()
            .args(["index", "--syntax-only", "--watch"])
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    pub(super) fn assert_alive(&mut self, ws: &Workspace) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "watch exited: stdout={} stderr={} daemon={}",
            fs::read_to_string(self.stdout.path()).unwrap(),
            fs::read_to_string(self.stderr.path()).unwrap(),
            diagnostics(ws)
        );
    }

    pub(super) fn wait_ready(&mut self, ws: &Workspace) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.assert_alive(ws);
            let stderr = fs::read_to_string(self.stderr.path()).unwrap();
            if stderr.contains("Index ready; watching") {
                assert!(
                    ws.ok(&["status"])["clients"]["watch_leases"]
                        .as_u64()
                        .unwrap()
                        > 0
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "watch startup timed out: {stderr}; {}",
                diagnostics(ws)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn wait_for_hash(
        &mut self,
        ws: &Workspace,
        suffix: &str,
        content: Option<&str>,
    ) -> Value {
        let expected = content.map(semweft_core::digest);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.assert_alive(ws);
            let status = ws.ok(&["status"]);
            let actual = status["manifest"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|file| file["path"].as_str().unwrap().ends_with(suffix))
                .and_then(|file| file["file_hash"].as_str());
            if actual == expected.as_deref() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "watch failed to observe {suffix}: {status}; {}",
                diagnostics(ws)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for WatchClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(all(feature = "go-experimental", unix))]
pub(super) fn wait_for_file(ws: &Workspace, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "producer did not create {}: status={} daemon={}",
            path.display(),
            ws.ok(&["status"]),
            diagnostics(ws)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(feature = "go-experimental", unix))]
pub(super) struct RevokeTrustOnDrop<'workspace> {
    workspace: &'workspace Workspace,
    armed: bool,
}

#[cfg(all(feature = "go-experimental", unix))]
impl<'workspace> RevokeTrustOnDrop<'workspace> {
    pub(super) fn new(workspace: &'workspace Workspace) -> Self {
        Self {
            workspace,
            armed: true,
        }
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }
}

#[cfg(all(feature = "go-experimental", unix))]
impl Drop for RevokeTrustOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .workspace
                .command()
                .args(["trust", "--revoke"])
                .output();
        }
    }
}

fn concurrent_searches(ws: &Workspace, count: usize) -> Vec<Value> {
    let children = (0..count)
        .map(|_| {
            ws.command()
                .args(["search", "stock"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "stdout={} stderr={} daemon={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                diagnostics(ws)
            );
            serde_json::from_slice(&output.stdout).unwrap()
        })
        .collect()
}

#[test]
fn simultaneous_cold_clients_share_one_private_store() {
    for _ in 0..6 {
        let ws = Workspace::new();
        let results = concurrent_searches(&ws, 12);
        assert!(
            results
                .iter()
                .all(|result| result["snapshot_id"] == results[0]["snapshot_id"])
        );
        assert_eq!(
            ws.ok(&["status"])["retained_views"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        ws.stop_and_wait();
    }
}

#[test]
fn concurrent_clients_recover_history_after_repeated_daemon_restarts() {
    let ws = Workspace::new();
    let initial = ws.ok(&["search", "stock"]);
    for revision in 2..6 {
        ws.stop_and_wait();
        fs::write(
            ws.root.join("src/lib.rs"),
            SOURCE.replace("{ 1 }", &format!("{{ {revision} }}")),
        )
        .unwrap();
        let results = concurrent_searches(&ws, 8);
        assert!(
            results
                .iter()
                .all(|result| result["snapshot_id"] == results[0]["snapshot_id"])
        );
        assert_ne!(results[0]["snapshot_id"], initial["snapshot_id"]);
        let old = ws.ok(&[
            "symbol",
            "stock",
            "--include-body",
            "--snapshot-id",
            initial["snapshot_id"].as_str().unwrap(),
        ]);
        assert!(old["symbol"]["body"].as_str().unwrap().contains("{ 1 }"));
    }
}

#[test]
fn watcher_observes_atomic_replacements_deletions_and_recreation() {
    let ws = Workspace::new();
    let mut watcher = WatchClient::start(&ws);
    watcher.wait_ready(&ws);
    for revision in 2..8 {
        let content = SOURCE.replace("{ 1 }", &format!("{{ {revision} }}"));
        fs::write(ws.root.join("src/replacement.tmp"), &content).unwrap();
        fs::rename(
            ws.root.join("src/replacement.tmp"),
            ws.root.join("src/lib.rs"),
        )
        .unwrap();
        watcher.wait_for_hash(&ws, "src/lib.rs", Some(&content));
    }
    fs::remove_file(ws.root.join("src/lib.rs")).unwrap();
    watcher.wait_for_hash(&ws, "src/lib.rs", None);
    fs::write(ws.root.join("src/lib.rs"), SOURCE).unwrap();
    watcher.wait_for_hash(&ws, "src/lib.rs", Some(SOURCE));
}

#[test]
fn multiple_watch_clients_release_their_leases_independently() {
    let ws = Workspace::new();
    let mut watchers = (0..3).map(|_| WatchClient::start(&ws)).collect::<Vec<_>>();
    for watcher in &mut watchers {
        watcher.wait_ready(&ws);
    }
    assert_eq!(ws.ok(&["status"])["clients"]["watch_leases"], 3);
    drop(watchers.pop());
    let deadline = Instant::now() + Duration::from_secs(3);
    while ws.ok(&["status"])["clients"]["watch_leases"] != 2 {
        assert!(Instant::now() < deadline, "{}", diagnostics(&ws));
    }
    let content = SOURCE.replace("{ 1 }", "{ 99 }");
    fs::write(ws.root.join("src/lib.rs"), &content).unwrap();
    for watcher in &mut watchers {
        watcher.wait_for_hash(&ws, "src/lib.rs", Some(&content));
    }
    drop(watchers);
    let deadline = Instant::now() + Duration::from_secs(3);
    while ws.ok(&["status"])["clients"]["watch_leases"] != 0 {
        assert!(Instant::now() < deadline, "{}", diagnostics(&ws));
    }
    ws.stop_and_wait();
}

#[test]
fn watch_lease_prevents_idle_shutdown() {
    let ws = Workspace::new();
    fs::write(
        ws.root.join("semweft.toml"),
        "schema_version=\"0.1\"\n[daemon]\nidle_timeout_ms=100\n",
    )
    .unwrap();
    let mut watcher = WatchClient::start(&ws);
    watcher.wait_ready(&ws);
    let initial = ws.ok(&["status"]);
    std::thread::sleep(Duration::from_millis(350));
    watcher.assert_alive(&ws);
    let current = ws.ok(&["status"]);
    assert_eq!(
        current["clients"]["daemon_pid"],
        initial["clients"]["daemon_pid"]
    );
    assert_eq!(current["clients"]["watch_leases"], 1);
}

#[test]
fn watch_startup_survives_source_edits_during_initial_index() {
    for _ in 0..4 {
        let ws = Workspace::new();
        ws.ok(&["search", "stock"]);
        let mut watcher = WatchClient::start(&ws);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ws.ok(&["status"])["clients"]["watch_leases"] == 0 {
            watcher.assert_alive(&ws);
            assert!(Instant::now() < deadline, "{}", diagnostics(&ws));
        }
        std::thread::scope(|scope| {
            let editor = scope.spawn(|| {
                for revision in 2..82 {
                    let content = SOURCE.replace("{ 1 }", &format!("{{ {revision} }}"));
                    fs::write(ws.root.join("src/edit.tmp"), content).unwrap();
                    fs::rename(ws.root.join("src/edit.tmp"), ws.root.join("src/lib.rs")).unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            watcher.wait_ready(&ws);
            editor.join().unwrap();
        });
        watcher.wait_for_hash(&ws, "src/lib.rs", Some(&SOURCE.replace("{ 1 }", "{ 81 }")));
    }
}

#[cfg(feature = "go-experimental")]
fn external_go_workspace() -> (Workspace, PathBuf) {
    let ws = Workspace::new();
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    let member = ws.temp.path().join("external");
    fs::create_dir(&member).unwrap();
    fs::write(
        member.join("go.mod"),
        "module example.com/external\ngo 1.22\n",
    )
    .unwrap();
    fs::write(
        member.join("code.go"),
        "package external\nfunc Before() {}\n",
    )
    .unwrap();
    fs::write(ws.root.join("go.work"), "go 1.22\nuse ../external\n").unwrap();
    (ws, member)
}

#[cfg(feature = "go-experimental")]
#[test]
fn external_go_watcher_tracks_replacements_deletions_and_recreation() {
    let (ws, member) = external_go_workspace();
    let mut watcher = WatchClient::start(&ws);
    watcher.wait_ready(&ws);
    for revision in 0..5 {
        let content = format!("package external\nfunc Version{revision}() {{}}\n");
        fs::write(member.join("replacement.tmp"), &content).unwrap();
        fs::rename(member.join("replacement.tmp"), member.join("code.go")).unwrap();
        watcher.wait_for_hash(&ws, "code.go", Some(&content));
    }
    fs::remove_file(member.join("code.go")).unwrap();
    watcher.wait_for_hash(&ws, "code.go", None);
    let content = "package external\nfunc Recreated() {}\n";
    fs::write(member.join("code.go"), content).unwrap();
    watcher.wait_for_hash(&ws, "code.go", Some(content));
}

#[cfg(feature = "go-experimental")]
#[test]
fn go_watcher_updates_external_membership_after_go_work_changes() {
    let (ws, member) = external_go_workspace();
    let mut watcher = WatchClient::start(&ws);
    watcher.wait_ready(&ws);
    let second = ws.temp.path().join("second");
    fs::create_dir(&second).unwrap();
    fs::write(
        second.join("go.mod"),
        "module example.com/second\ngo 1.22\n",
    )
    .unwrap();
    let content = "package second\nfunc Added() {}\n";
    fs::write(second.join("second.go"), content).unwrap();
    fs::write(ws.root.join("go.work"), "go 1.22\nuse ../second\n").unwrap();
    watcher.wait_for_hash(&ws, "second.go", Some(content));
    watcher.wait_for_hash(&ws, "code.go", None);
    fs::remove_dir_all(member).unwrap();
    let content = "package second\nfunc Edited() {}\n";
    fs::write(second.join("second.go"), content).unwrap();
    watcher.wait_for_hash(&ws, "second.go", Some(content));
}

#[cfg(all(feature = "go-experimental", unix))]
fn slow_go_producer(ws: &Workspace) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    fs::remove_file(ws.root.join("Cargo.toml")).unwrap();
    fs::remove_dir_all(ws.root.join("src")).unwrap();
    fs::write(ws.root.join("go.mod"), "module example.com/shop\ngo 1.22\n").unwrap();
    fs::write(ws.root.join("shop.go"), "package shop\nfunc Stock() {}\n").unwrap();
    let markers = ws.temp.path().join("markers");
    fs::create_dir(&markers).unwrap();
    let producer = ws.temp.path().join("producer.sh");
    fs::write(&producer, "#!/bin/sh\ncd \"$SEMWEFT_TEST_MARKERS\" || exit 1\nprintf '%s' \"$$\" > pid\n(while [ ! -f release-descendant ]; do sleep 0.01; done; touch descendant-finished) &\ntouch started\nsleep 30\ntouch finished\n").unwrap();
    fs::set_permissions(&producer, fs::Permissions::from_mode(0o700)).unwrap();
    let output = ws
        .command()
        .env("SEMWEFT_SCIP_GO", producer)
        .env("SEMWEFT_TEST_MARKERS", &markers)
        .args(["index", "--syntax-only"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    markers
}

#[cfg(all(feature = "go-experimental", unix))]
fn revoke_and_check_producer(ws: &Workspace, markers: &Path) {
    let pid = fs::read_to_string(markers.join("pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let started = Instant::now();
    ws.ok(&["trust", "--revoke"]);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{}",
        diagnostics(ws)
    );
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "producer {pid} still exists"
    );
    let status = ws.ok(&["status"]);
    assert_ne!(status["refresh"]["state"], "refreshing", "{status}");
    assert_eq!(status["trust"]["state"], "untrusted", "{status}");
    fs::write(markers.join("release-descendant"), "release").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(!markers.join("descendant-finished").exists());
    assert!(!markers.join("finished").exists());
}

#[cfg(all(feature = "go-experimental", unix))]
#[test]
fn go_producers_restart_and_cancel_their_process_trees_after_repeated_trust_changes() {
    let ws = Workspace::new();
    let markers = slow_go_producer(&ws);
    let revoke_on_failure = RevokeTrustOnDrop::new(&ws);
    for _ in 0..5 {
        ws.ok(&["trust", "--accept-build-execution"]);
        wait_for_file(&ws, &markers.join("started"));
        revoke_and_check_producer(&ws, &markers);
        for name in ["started", "pid", "release-descendant"] {
            fs::remove_file(markers.join(name)).unwrap();
        }
    }
    revoke_on_failure.disarm();
}

#[cfg(all(feature = "go-experimental", unix))]
#[test]
fn go_revocation_releases_clients_waiting_for_a_semantic_index() {
    let ws = Workspace::new();
    let markers = slow_go_producer(&ws);
    let revoke_on_failure = RevokeTrustOnDrop::new(&ws);
    ws.ok(&["trust", "--accept-build-execution"]);
    wait_for_file(&ws, &markers.join("started"));
    let client = ws
        .command()
        .arg("index")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    revoke_and_check_producer(&ws, &markers);
    let output = client.wait_with_output().unwrap();
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "trust_required", "{value}");
    revoke_on_failure.disarm();
}
