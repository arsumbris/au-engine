//! The `set_config` mutation over the socket: write one CONSUMER config file
//! under the scoped-config channel. Repo scope constructs
//! `<member>/.arsumbris/<consumer>/config/<file>` directly (a guard bypass, like
//! `set_ignores`), injects `type:` so the file self-describes, honors an
//! `expected_hash` compare-and-set, and commits per mutation through the saga.
//!
//! See [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// One git-backed member (entry name `v`) carrying a consumer type-def
/// `viewer-default-set` with one required field, so a written config validates.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    let p = root.join("type/viewer-default-set.type.yaml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "fields:\n  viewer: String\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@t"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    (dir, root)
}

fn git(root: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
    root: PathBuf,
}

fn started() -> Harness {
    let (dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
        root,
    }
}

fn config_file(h: &Harness, consumer: &str, file: &str) -> PathBuf {
    h.root
        .join(".arsumbris")
        .join(consumer)
        .join("config")
        .join(file)
}

#[test]
fn set_config_writes_commits_and_injects_type() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config",
            "scope": "repo",
            "consumer": "host-app",
            "file": "viewer-defaults.yaml",
            "type": "viewer-default-set",
            "content": "viewer: editor-pane\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp}");
    // A commit landed, and the written hash is surfaced for the next CAS.
    assert!(resp["result"]["commit"].is_string(), "{resp}");
    assert!(resp["result"]["hash"].is_string(), "{resp}");

    // The file on disk self-describes: the declared `type:` was injected.
    let written = fs::read_to_string(config_file(&h, "host-app", "viewer-defaults.yaml")).unwrap();
    assert_eq!(
        written, "type: viewer-default-set\nviewer: editor-pane\n",
        "{written:?}"
    );

    // Reading it back is clean (self-describing, required field present).
    let read = h
        .client
        .query(&json!({
            "read": "config", "scope": "repo", "consumer": "host-app",
            "file": "viewer-defaults.yaml", "type": "viewer-default-set",
        }))
        .unwrap();
    let view = &read["result"]["config"];
    assert_eq!(view["exists"], true, "{view}");
    assert!(
        view["diagnostics"].as_array().unwrap().is_empty(),
        "a governed write self-describes and validates clean: {view}"
    );
}

#[test]
fn set_config_honors_a_written_type_and_does_not_double_inject() {
    let mut h = started();
    // Content already self-describing: its own `type:` is honored, not re-injected.
    h.client
        .query(&json!({
            "mutate": "set_config",
            "scope": "repo",
            "consumer": "host-app",
            "file": "v.yaml",
            "type": "viewer-default-set",
            "content": "type: viewer-default-set\nviewer: editor-pane\n",
        }))
        .unwrap();
    let written = fs::read_to_string(config_file(&h, "host-app", "v.yaml")).unwrap();
    assert_eq!(
        written, "type: viewer-default-set\nviewer: editor-pane\n",
        "a written type: is honored, not double-injected: {written:?}"
    );
}

#[test]
fn set_config_expected_hash_compare_and_set() {
    let mut h = started();
    // First write creates the file and returns its hash.
    let first = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set", "content": "viewer: a\n",
        }))
        .unwrap();
    let hash = first["result"]["hash"].as_str().unwrap().to_string();

    // A wrong expected_hash rejects, nothing written.
    let bad = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set", "content": "viewer: b\n",
            "expected_hash": "deadbeef",
        }))
        .unwrap();
    assert_eq!(bad["type"], "error", "a stale expected_hash rejects: {bad}");
    let on_disk = fs::read_to_string(config_file(&h, "host-app", "v.yaml")).unwrap();
    assert!(
        on_disk.contains("viewer: a"),
        "nothing written on reject: {on_disk:?}"
    );

    // The correct expected_hash succeeds.
    let good = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set", "content": "viewer: b\n",
            "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(good["ready"], true, "the matching hash succeeds: {good}");
    let on_disk = fs::read_to_string(config_file(&h, "host-app", "v.yaml")).unwrap();
    assert!(
        on_disk.contains("viewer: b"),
        "the matching write landed: {on_disk:?}"
    );
}

#[test]
fn set_config_edit_changes_one_key_preserving_comments_and_siblings() {
    let mut h = started();
    // Seed a self-describing config with a top-level scalar, a comment, and a
    // sibling to leave untouched.
    h.client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set",
            "content": "viewer: old  # keep this comment\ntheme: dark\n",
        }))
        .unwrap();

    // Edit only `viewer` (empty field_path = the top-level record).
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set",
            "edit": { "field_path": [], "patch": { "viewer": "new" } },
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp}");
    assert!(
        resp["result"]["commit"].is_string(),
        "the edit commits: {resp}"
    );

    let written = fs::read_to_string(config_file(&h, "host-app", "v.yaml")).unwrap();
    // One key changed; the comment and the sibling survive byte-for-byte.
    assert_eq!(
        written, "type: viewer-default-set\nviewer: new  # keep this comment\ntheme: dark\n",
        "the splice changed one key, preserving the comment and sibling: {written:?}"
    );
}

#[test]
fn set_config_edit_on_an_absent_file_rejects() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "missing.yaml", "type": "viewer-default-set",
            "edit": { "field_path": [], "patch": { "viewer": "x" } },
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "editing an absent file rejects: {resp}"
    );
}

#[test]
fn set_config_rejects_both_content_and_edit() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set",
            "content": "viewer: a\n",
            "edit": { "field_path": [], "patch": { "viewer": "b" } },
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "content AND edit rejects: {resp}");
}

#[test]
fn set_config_rejects_a_reserved_or_traversal_segment() {
    let mut h = started();
    for consumer in ["au-engine", "Au-Engine", ".."] {
        let resp = h
            .client
            .query(&json!({
                "mutate": "set_config", "scope": "repo", "consumer": consumer,
                "file": "v.yaml", "type": "viewer-default-set", "content": "x: 1\n",
            }))
            .unwrap();
        assert_eq!(
            resp["type"], "error",
            "consumer `{consumer}` must reject: {resp}"
        );
    }
}

#[test]
fn set_config_rejects_an_unknown_repo_scope_root() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "repo", "consumer": "host-app",
            "file": "v.yaml", "type": "viewer-default-set", "content": "x: 1\n",
            "root": "/no/such/member",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an unknown member root rejects: {resp}"
    );
}
