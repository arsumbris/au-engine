//! The `resolve` verb: fetch the served workspace's declared dependency closure
//! into the device cache, write and commit the package lock, and rebuild so the
//! dependency mounts. A write-path verb, not a `read`.
//!
//! The cache root is injected with a tempdir so the verb is hermetic, off the
//! real `~/.arsumbris` cache. The dependency remote is a local git repo, so no
//! network is touched.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::json;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A source repo with a type-def, standing in for a dependency remote.
fn source_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.name", "Tester"]);
    git(p, &["config", "user.email", "tester@example.com"]);
    fs::create_dir_all(p.join("type")).unwrap();
    fs::write(p.join("type/thing.type.yaml"), "fields:\n  title: String\n").unwrap();
    fs::create_dir_all(p.join(".arsumbris")).unwrap();
    fs::write(p.join(".arsumbris/repo.yaml"), "name: thing-pkg\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "v1"]);
    let sha = git(p, &["rev-parse", "HEAD"]);
    (dir, sha)
}

struct Harness {
    _dir: tempfile::TempDir,
    _cache: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: au_engine::ServeHandle,
    client: Client,
    ws: PathBuf,
}

/// A workspace whose co-present primary `proj` declares one dependency by
/// explicit remote. The workspace dir is itself a git repo (the user's project),
/// so the lock can commit into it.
fn started_with_dep(remote: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(dir.path()).unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["config", "user.name", "Tester"]);
    git(&ws, &["config", "user.email", "tester@example.com"]);
    fs::create_dir_all(ws.join("proj/.arsumbris")).unwrap();
    fs::write(
        ws.join("proj/.arsumbris/repo.yaml"),
        format!("name: proj\ndeps:\n  - name: thing-pkg\n    remote: {remote}\n    ref: main\n"),
    )
    .unwrap();
    crate::seed_workspace(&ws, &["proj"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);

    let cache = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    // Point at the entry folder-repo directory: its `.arsumbris/workspace.yaml`
    // declares `proj` as an editable member whose deps resolve.
    let mut engine = Engine::new(&ws, ConfigSource::Empty);
    // Hermetic: the resolver and the build share this tempdir, off the real cache.
    engine.set_package_cache_root(cache.path().join("packages"));
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir,
        _cache: cache,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
        ws,
    }
}

#[test]
fn resolve_fetches_locks_commits_and_mounts_a_dependency() {
    let (src, sha) = source_repo();
    let mut h = started_with_dep(&src.path().to_string_lossy());

    let before = git(&h.ws, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({ "resolve": {}, "id": "x-1" }))
        .unwrap();

    assert_eq!(resp["type"], "resolved");
    assert_eq!(resp["id"], "x-1", "the resolve response echoes the id");

    // The declared dependency resolved to its sha.
    let resolved = resp["resolved"].as_array().expect("a resolved list");
    assert_eq!(resolved.len(), 1, "one dependency resolved: {resp}");
    assert_eq!(resolved[0]["name"], "thing-pkg");
    assert_eq!(resolved[0]["sha"], sha);
    assert!(
        resp["failed"].as_array().unwrap().is_empty(),
        "nothing failed: {resp}"
    );

    // proj's own lock committed into the enclosing git tree, advancing HEAD. The
    // `commits` map keys the sha by the editable repo whose lock committed.
    let commit = resp["commits"]["proj"]
        .as_str()
        .unwrap_or_else(|| panic!("a lock commit sha for proj: {resp}"));
    let head = git(&h.ws, &["rev-parse", "HEAD"]);
    assert_ne!(head, before, "the lock commit advances HEAD");
    assert_eq!(head, commit, "the response carries the lock commit");
    let committed = git(&h.ws, &["show", "HEAD:proj/.arsumbris/repo.lock"]);
    assert!(
        committed.contains(&sha),
        "the committed lock pins the resolved sha: {committed}"
    );

    // The rebuild mounted the dependency: its type-def is in the served graph.
    let types = h.client.query(&json!({ "read": "types" })).unwrap();
    assert!(
        serde_json::to_string(&types["result"]["types"])
            .unwrap()
            .contains("thing"),
        "the dependency's type-def should be mounted: {}",
        types["result"]["types"]
    );
}

#[test]
fn a_second_resolve_is_an_idempotent_no_op() {
    let (src, _sha) = source_repo();
    let mut h = started_with_dep(&src.path().to_string_lossy());

    // First resolve commits proj's lock.
    let first = h
        .client
        .query(&json!({ "resolve": {}, "id": "x-4a" }))
        .unwrap();
    assert!(
        first["commits"]["proj"].as_str().is_some(),
        "first resolve commits: {first}"
    );

    // Second resolve: the lock is unchanged, so nothing to commit. That is a
    // no-op (empty commits, empty commit_errors), not a git failure.
    let second = h
        .client
        .query(&json!({ "resolve": {}, "id": "x-4b" }))
        .unwrap();
    assert_eq!(second["type"], "resolved");
    assert!(
        second["commits"].as_object().unwrap().is_empty(),
        "an unchanged lock is nothing to commit: {second}"
    );
    assert!(
        second["commit_errors"].as_object().unwrap().is_empty(),
        "a no-op is not a commit error: {second}"
    );
}

#[test]
fn resolve_reports_a_failed_dependency_with_a_code() {
    // A dependency whose remote does not exist: resolution fails loudly, named.
    let mut h = started_with_dep("/no/such/remote/repo.git");

    let resp = h
        .client
        .query(&json!({ "resolve": {}, "id": "x-3" }))
        .unwrap();

    assert_eq!(resp["type"], "resolved");
    assert!(
        resp["resolved"].as_array().unwrap().is_empty(),
        "nothing resolved: {resp}"
    );
    let failed = resp["failed"].as_array().expect("a failed list");
    assert_eq!(failed.len(), 1, "the dependency failed: {resp}");
    assert_eq!(failed[0]["name"], "thing-pkg");
    assert_eq!(
        failed[0]["code"], "dependency-resolution-failed",
        "the failure carries the named code: {resp}"
    );
    assert!(
        failed[0]["reason"].as_str().is_some(),
        "a reason is given: {resp}"
    );
    // Nothing resolved, so no lock commit.
    assert!(
        resp["commits"].as_object().unwrap().is_empty(),
        "no commit when nothing resolved: {resp}"
    );
}

#[test]
fn resolve_in_tree_mode_resolves_and_reopens_offline() {
    // An entry folder-repo whose `.arsumbris/workspace.yaml` declares one editable
    // member `proj`. resolve fetches each member's declared deps and writes that
    // member's OWN .arsumbris/repo.lock, so it resolves and re-opens reproducibly.
    let (src, sha) = source_repo();

    let dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(dir.path()).unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["config", "user.name", "Tester"]);
    git(&ws, &["config", "user.email", "tester@example.com"]);
    // A co-present repo `proj/` declaring one dependency by explicit remote. It
    // carries a type-def so the tree walk discovers it (the walk skips
    // `.arsumbris/`, so a content-less repo would be invisible).
    fs::create_dir_all(ws.join("proj/.arsumbris")).unwrap();
    fs::create_dir_all(ws.join("proj/type")).unwrap();
    fs::write(
        ws.join("proj/type/projthing.type.yaml"),
        "fields:\n  title: String\n",
    )
    .unwrap();
    fs::write(
        ws.join("proj/.arsumbris/repo.yaml"),
        format!(
            "name: proj\ndeps:\n  - name: thing-pkg\n    remote: {}\n    ref: main\n",
            src.path().to_string_lossy()
        ),
    )
    .unwrap();
    crate::seed_workspace(&ws, &["proj"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);

    let cache = tempfile::tempdir().unwrap();
    let cache_packages = cache.path().join("packages");
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    // Point at the entry folder-repo directory: its `.arsumbris/workspace.yaml`
    // declares `proj` as an editable member.
    let mut engine = Engine::new(&ws, ConfigSource::Empty);
    engine.set_package_cache_root(cache_packages.clone());
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    // --- Resolve in tree mode ---
    let resp = client
        .query(&json!({ "resolve": {}, "id": "x-tree" }))
        .unwrap();
    assert_eq!(
        resp["type"], "resolved",
        "tree-mode resolve now works: {resp}"
    );
    assert_eq!(resp["id"], "x-tree", "the resolve response echoes the id");
    let resolved = resp["resolved"].as_array().expect("a resolved list");
    assert_eq!(
        resolved.len(),
        1,
        "the co-present repo's dep resolved: {resp}"
    );
    assert_eq!(resolved[0]["name"], "thing-pkg");
    assert_eq!(resolved[0]["sha"], sha);

    // proj's OWN lock committed into the enclosing git tree.
    let commit = resp["commits"]["proj"]
        .as_str()
        .unwrap_or_else(|| panic!("a lock commit sha for proj: {resp}"));
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), commit);
    let committed = git(&ws, &["show", "HEAD:proj/.arsumbris/repo.lock"]);
    assert!(
        committed.contains(&sha),
        "proj's per-repo lock pins the resolved sha: {committed}"
    );

    // Tear the daemon down, remove the remote. A tree-mode re-open must not need it.
    drop(client);
    drop(server);
    drop(engine);
    drop(src);

    // --- Offline re-open in tree mode: the dep mounts from proj's lock + cache ---
    let socket2 = sock_dir.path().join("s2");
    let mut engine2 = Engine::new(&ws, ConfigSource::Empty);
    engine2.set_package_cache_root(cache_packages);
    let server2 = serve(engine2.handle(), &socket2).expect("serve");
    engine2.rebuild();
    let mut client2 = Client::connect(&socket2).expect("connect");
    let types = client2.query(&json!({ "read": "types" })).unwrap();
    assert!(
        serde_json::to_string(&types["result"]["types"])
            .unwrap()
            .contains("thing"),
        "the dependency's type-def mounts offline in tree mode: {}",
        types["result"]["types"]
    );
    drop(server2);
}
