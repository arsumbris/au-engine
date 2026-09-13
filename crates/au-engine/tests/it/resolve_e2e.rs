//! End-to-end resolve: a workspace with a project member plus dependency
//! members, resolved from a fixture registry and explicit remotes, mounted,
//! locked, and reproducible offline.
//!
//! The full path, nothing mocked: real git repos as remotes, a real fetch into a
//! real (temp) cache, a real lock commit, a real daemon over a real socket.
//! Every fixture is a tempdir, auto-removed on drop (including panic unwind), so
//! the test creates everything under the system temp dir and cleans up after
//! itself. The cache root and the registry remote are injected, so the test
//! touches nothing global: not the real `~/.arsumbris` cache, not the org
//! registry, not global git config (identities are set per repo).

#![cfg(unix)]

use std::fs;
use std::path::Path;
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

fn init_repo(p: &Path) {
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.name", "Tester"]);
    git(p, &["config", "user.email", "tester@example.com"]);
}

/// A git source repo declaring `name` and one type-def. Stands in for a
/// dependency remote or a located project member.
fn member_repo(name: &str, type_name: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    init_repo(p);
    fs::create_dir_all(p.join("type")).unwrap();
    fs::write(
        p.join(format!("type/{type_name}.type.yaml")),
        "fields:\n  title: String\n",
    )
    .unwrap();
    fs::create_dir_all(p.join(".arsumbris")).unwrap();
    fs::write(p.join(".arsumbris/repo.yaml"), format!("name: {name}\n")).unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "v1"]);
    let sha = git(p, &["rev-parse", "HEAD"]);
    (dir, sha)
}

/// A registry repo whose `registry.yaml` lists one name-only package.
fn registry_repo(name: &str, remote: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    init_repo(p);
    fs::write(
        p.join("registry.yaml"),
        format!("packages:\n  - name: {name}\n    remote: {remote}\n    ref: main\n"),
    )
    .unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "registry"]);
    dir
}

struct Daemon {
    _sock_dir: tempfile::TempDir,
    engine: Engine,
    server: au_engine::ServeHandle,
    client: Client,
}

/// A daemon over `ws`, with the cache root and registry remote injected so it is
/// hermetic. The daemon is built (one rebuild) and connected.
fn daemon(ws: &Path, cache_packages: &Path, registry_remote: &str) -> Daemon {
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let mut engine = Engine::new(ws, ConfigSource::Empty);
    engine.set_package_cache_root(cache_packages);
    engine.set_registry_remote(registry_remote);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Daemon {
        _sock_dir: sock_dir,
        engine,
        server,
        client,
    }
}

#[test]
fn resolve_a_mixed_workspace_then_reopen_offline() {
    // Two dependency remotes, one declared by explicit remote and one resolved by
    // name through a fixture registry.
    let (explicit_src, explicit_sha) = member_repo("explicit-dep", "explicitthing");
    let (named_src, named_sha) = member_repo("named-dep", "namedthing");
    let registry = registry_repo("named-dep", &named_src.path().to_string_lossy());

    // The workspace dir is a git repo holding the manifest and (after resolve)
    // the lock. The primary `proj` is a co-present subdir declaring the two
    // dependencies in its own `repo.yaml`.
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    init_repo(&ws);
    fs::create_dir_all(ws.join("proj/type")).unwrap();
    fs::write(
        ws.join("proj/type/projthing.type.yaml"),
        "fields:\n  title: String\n",
    )
    .unwrap();
    fs::create_dir_all(ws.join("proj/.arsumbris")).unwrap();
    fs::write(
        ws.join("proj/.arsumbris/repo.yaml"),
        format!(
            "name: proj\ndeps:\n  \
             - name: explicit-dep\n    remote: {}\n    ref: main\n  \
             - name: named-dep\n",
            explicit_src.path().to_string_lossy()
        ),
    )
    .unwrap();
    crate::seed_workspace(&ws, &["proj"]);
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);

    // Hermetic cache, shared by the resolve and the later offline re-open.
    let cache = tempfile::tempdir().unwrap();
    let cache_packages = cache.path().join("packages");

    let before = git(&ws, &["rev-parse", "HEAD"]);
    // Point the daemon at the entry folder-repo directory: its
    // `.arsumbris/workspace.yaml` declares the closure to resolve.
    let mut d = daemon(&ws, &cache_packages, &registry.path().to_string_lossy());

    // --- Resolve ---
    let resp = d
        .client
        .query(&json!({ "resolve": {}, "id": "e2e" }))
        .unwrap();
    assert_eq!(resp["type"], "resolved", "{resp}");
    assert_eq!(resp["id"], "e2e");
    assert!(
        resp["failed"].as_array().unwrap().is_empty(),
        "both dependencies resolve: {resp}"
    );
    let resolved = resp["resolved"].as_array().unwrap();
    assert_eq!(resolved.len(), 2, "two dependency members resolved: {resp}");
    let by_name = |name: &str| {
        resolved
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing from {resp}"))
            .clone()
    };
    assert_eq!(by_name("explicit-dep")["sha"], explicit_sha);
    assert_eq!(by_name("named-dep")["sha"], named_sha);

    // --- editable + local + role on the members read ---
    let members = d.client.query(&json!({ "read": "members" })).unwrap();
    let mem = |name: &str| {
        members["result"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mem| mem["repo"] == name)
            .unwrap_or_else(|| panic!("{name} not a member: {}", members["result"]))
            .clone()
    };
    // proj is an `edit` member (an editable authoring surface) served from a live
    // local tree: editable AND local.
    assert_eq!(mem("proj")["role"], "edit");
    assert_eq!(mem("proj")["editable"], true);
    assert_eq!(mem("proj")["local"], true);
    // The two computed deps mount from the immutable cache snapshot: `dep` role
    // (consumed, not editable) AND not local (roots sit under the package cache).
    // This is exactly the two axes coming apart.
    assert_eq!(mem("explicit-dep")["role"], "dep");
    assert_eq!(mem("explicit-dep")["editable"], false);
    assert_eq!(mem("explicit-dep")["local"], false);
    assert_eq!(mem("named-dep")["role"], "dep");
    assert_eq!(mem("named-dep")["editable"], false);
    assert_eq!(mem("named-dep")["local"], false);

    // --- proj's own lock committed into the enclosing git tree ---
    // The dep lock moved per-repo: proj's `.arsumbris/repo.lock` pins its full
    // fetched closure, committed into the git tree that holds it (here `ws`, since
    // the primary `proj` is a co-present subdir of the workspace git repo). The
    // `commits` map keys the sha by the editable repo.
    let commit = resp["commits"]["proj"]
        .as_str()
        .unwrap_or_else(|| panic!("a lock commit sha for proj: {resp}"));
    assert_ne!(git(&ws, &["rev-parse", "HEAD"]), before, "HEAD advanced");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), commit);
    let lock = git(&ws, &["show", "HEAD:proj/.arsumbris/repo.lock"]);
    assert!(
        lock.contains(&explicit_sha) && lock.contains(&named_sha),
        "{lock}"
    );

    // --- All three types mounted in the served graph ---
    let types =
        serde_json::to_string(&d.client.query(&json!({ "read": "types" })).unwrap()).unwrap();
    for t in ["projthing", "explicitthing", "namedthing"] {
        assert!(types.contains(t), "type {t} should be mounted: {types}");
    }

    // Tear the first daemon down, then remove the remotes and the registry. A
    // re-open must not need any of them.
    drop(d.client);
    drop(d.server);
    drop(d.engine);
    drop(explicit_src);
    drop(named_src);
    drop(registry);

    // --- Offline re-open: a fresh daemon over the same ws + cache mounts the
    // dependencies from the lock + cache, no network. ---
    let mut d2 = daemon(&ws, &cache_packages, "/no/such/registry");
    let members2 = d2.client.query(&json!({ "read": "members" })).unwrap();
    let names: Vec<String> = members2["result"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|mem| mem["repo"].as_str().unwrap().to_string())
        .collect();
    for name in ["proj", "explicit-dep", "named-dep"] {
        assert!(
            names.contains(&name.to_string()),
            "{name} mounts offline: {names:?}"
        );
    }
    let types2 =
        serde_json::to_string(&d2.client.query(&json!({ "read": "types" })).unwrap()).unwrap();
    for t in ["explicitthing", "namedthing"] {
        assert!(
            types2.contains(t),
            "dependency type {t} mounts offline: {types2}"
        );
    }
    drop(d2.server);
}

#[test]
fn resolve_pins_a_discover_member_in_the_workspace_lock_and_reopens_offline() {
    // A `discover` member resolved by NAME through a fixture registry: resolve
    // fetches it, pins it in the entry's `.arsumbris/workspace.lock` (committed
    // into the entry git tree), and it mounts reproducibly offline. The
    // daemon-wire end-to-end pair to the pkgcache unit test of the same machinery.
    let (shared_src, shared_sha) = member_repo("shared", "sharedthing");
    let registry = registry_repo("shared", &shared_src.path().to_string_lossy());

    // The entry is a folder-repo git tree: a content-free `home` repo whose
    // `.arsumbris/workspace.yaml` discovers `shared` (name-only, via the registry).
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    init_repo(&ws);
    fs::create_dir_all(ws.join(".arsumbris")).unwrap();
    fs::write(ws.join(".arsumbris/repo.yaml"), "name: home\n").unwrap();
    fs::write(
        ws.join(".arsumbris/workspace.yaml"),
        "edit:\n  - home\ndiscover:\n  - shared\n",
    )
    .unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "init"]);

    let cache = tempfile::tempdir().unwrap();
    let cache_packages = cache.path().join("packages");
    let before = git(&ws, &["rev-parse", "HEAD"]);
    let mut d = daemon(&ws, &cache_packages, &registry.path().to_string_lossy());

    // --- Resolve: fetch the discover member and pin it in the workspace lock ---
    let resp = d
        .client
        .query(&json!({ "resolve": {}, "id": "disc" }))
        .unwrap();
    assert_eq!(resp["type"], "resolved", "{resp}");
    assert!(
        resp["failed"].as_array().unwrap().is_empty(),
        "the discover member resolves: {resp}"
    );
    let resolved = resp["resolved"].as_array().unwrap();
    assert!(
        resolved
            .iter()
            .any(|r| r["name"] == "shared" && r["sha"] == shared_sha.as_str()),
        "shared resolved at its fetched sha: {resp}"
    );

    // --- The workspace.lock is written + committed into the entry git tree,
    // keyed by the entry repo (role `Entry`) ---
    let commit = resp["commits"]["home"]
        .as_str()
        .unwrap_or_else(|| panic!("a workspace.lock commit for the entry: {resp}"));
    assert_ne!(git(&ws, &["rev-parse", "HEAD"]), before, "HEAD advanced");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), commit);
    let lock = git(&ws, &["show", "HEAD:.arsumbris/workspace.lock"]);
    assert!(
        lock.contains(&shared_sha),
        "the discover closure is pinned in workspace.lock: {lock}"
    );

    // --- The discover member mounts from the cache: role `discover`, not editable,
    // not local (its root sits under the package cache) ---
    let members = d.client.query(&json!({ "read": "members" })).unwrap();
    let shared = members["result"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["repo"] == "shared")
        .unwrap_or_else(|| panic!("shared is a member: {}", members["result"]))
        .clone();
    assert_eq!(shared["role"], "discover");
    assert_eq!(shared["editable"], false);
    assert_eq!(shared["local"], false);

    // --- The discover member's type surfaces in the served graph ---
    let types =
        serde_json::to_string(&d.client.query(&json!({ "read": "types" })).unwrap()).unwrap();
    assert!(
        types.contains("sharedthing"),
        "the discover type mounts: {types}"
    );

    // Tear down, remove the remote + registry: the re-open must not need them.
    drop(d.client);
    drop(d.server);
    drop(d.engine);
    drop(shared_src);
    drop(registry);

    // --- Offline re-open: a fresh daemon mounts the discover member from the
    // workspace.lock + cache, no network ---
    let mut d2 = daemon(&ws, &cache_packages, "/no/such/registry");
    let types2 =
        serde_json::to_string(&d2.client.query(&json!({ "read": "types" })).unwrap()).unwrap();
    assert!(
        types2.contains("sharedthing"),
        "the discover type mounts offline: {types2}"
    );
    let members2 = d2.client.query(&json!({ "read": "members" })).unwrap();
    assert!(
        members2["result"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["repo"] == "shared"),
        "shared mounts offline: {}",
        members2["result"]
    );
    drop(d2.server);
}
