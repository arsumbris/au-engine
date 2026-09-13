//! The scope-management surface over the socket: the `ignores` read returns a
//! member's `.auignore` rules (and, with `resolve`, their boundary-level
//! effect); the `set_ignores` mutation writes the file through the governed
//! channel, validating patterns up front, committing, and re-scoping.
//!
//! See [[spec - scope management surface - an ignores read and a set_ignores config mutation]].

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// One git-backed member rooted at the knowledge base: a couple of instances, a pruned
/// `node_modules`, and an `.auignore` excluding `docs/`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("type/note.type.yaml", "fields:\n  title: String\n");
    w("keep.md", "---\ntype: note\ntitle: K\n---\n");
    w("docs/guide.md", "---\ntype: note\ntitle: G\n---\n");
    w("node_modules/pkg/index.js", "x");
    w(".arsumbris/.auignore", "docs/\n");
    // A git tree so `set_ignores` commits, the same as the mutation e2e paths.
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@t"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    (dir, root)
}

fn git(root: &std::path::Path, args: &[&str]) {
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
    engine: Engine,
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
        engine,
        _server: server,
        client,
        root,
    }
}

#[test]
fn ignores_returns_patterns_defaults_and_floor() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "ignores" })).unwrap();
    assert_eq!(resp["ready"], true);
    let members = resp["result"]["ignores"].as_array().unwrap();
    // The au-engine builtin repo (and, in a real workspace, every mounted dependency) surfaces its own ignores member here too; select the knowledge base's own member. On-demand hiding is tracked in the todo.
    let m = members
        .iter()
        .find(|m| m["root"] == h.root.display().to_string())
        .expect("the knowledge base's own member is present");
    assert_eq!(m["root"], h.root.display().to_string());
    assert_eq!(m["patterns"], json!(["docs/"]));
    assert_eq!(m["default_excludes"], json!(["node_modules", "target"]));
    assert_eq!(m["floor"], json!([".git", ".arsumbris"]));
    // No `resolve`, no boundary effect on the wire.
    assert!(m.get("resolved").is_none() || m["resolved"].is_null());
}

#[test]
fn ignores_resolve_reports_boundaries_not_contents() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "ignores", "resolve": true }))
        .unwrap();
    let m = &resp["result"]["ignores"][0];
    let resolved = &m["resolved"];
    let dirs: Vec<String> = resolved["ignored_dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    // Both the `.auignore` `docs/` and the default-excluded `node_modules` are
    // pruned boundaries; their contents are never enumerated.
    assert!(dirs.contains(&h.root.join("docs").display().to_string()));
    assert!(dirs.contains(&h.root.join("node_modules").display().to_string()));
    assert!(resolved["ignored_files"].as_array().unwrap().is_empty());
}

#[test]
fn set_ignores_writes_commits_and_rescopes() {
    let mut h = started();
    // `keep.md` and `docs/guide.md`: docs is currently excluded. Re-scope to
    // exclude `keep.md`'s sibling instead, and confirm the graph follows.
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores",
            "root": h.root.display().to_string(),
            "patterns": ["node_modules/", "keep.md"],
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "resp: {resp}");
    assert_eq!(resp["result"]["reflected"], true);
    // A commit landed (a `Mutation-Id` trailer, like any governed write).
    assert!(resp["result"]["commit"].is_string());

    // The file on disk is the new list, one pattern per line.
    let written = fs::read_to_string(h.root.join(".arsumbris/.auignore")).unwrap();
    assert_eq!(written, "node_modules/\nkeep.md\n");

    // The re-scope took effect: `keep.md` left the graph, `docs/guide.md` is
    // back (no longer excluded).
    let ig = h.client.query(&json!({ "read": "ignores" })).unwrap();
    assert_eq!(
        ig["result"]["ignores"][0]["patterns"],
        json!(["node_modules/", "keep.md"])
    );
    let resolved_frontmatter = h
        .client
        .query(&json!({ "read": "frontmatter", "path": "keep.md" }))
        .unwrap();
    // An excluded file is absent from the graph: its frontmatter read is null.
    assert_eq!(
        resolved_frontmatter["result"]["frontmatter"],
        serde_json::Value::Null
    );
    let docs_fm = h
        .client
        .query(&json!({ "read": "frontmatter", "path": "docs/guide.md" }))
        .unwrap();
    assert!(
        !docs_fm["result"]["frontmatter"].is_null(),
        "docs/guide.md back in the graph"
    );
    let _ = &h.engine;
}

#[test]
fn set_ignores_rejects_a_malformed_pattern_writing_nothing() {
    let mut h = started();
    let before = fs::read_to_string(h.root.join(".arsumbris/.auignore")).unwrap();
    // A lone backslash is not a valid gitignore glob; the governed path rejects
    // up front, unlike the file-write path's advisory degradation.
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores",
            "root": h.root.display().to_string(),
            "patterns": ["\\"],
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "resp: {resp}");
    // Nothing was written: the file is byte-for-byte unchanged.
    let after = fs::read_to_string(h.root.join(".arsumbris/.auignore")).unwrap();
    assert_eq!(before, after, "a rejected mutation writes nothing");
}

#[test]
fn set_ignores_empty_removes_the_file() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores",
            "root": h.root.display().to_string(),
            "patterns": [],
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "resp: {resp}");
    assert!(
        !h.root.join(".arsumbris/.auignore").exists(),
        "empty patterns remove the file"
    );
    // The member reverts to the default excludes: `docs/` is back in the graph.
    let docs_fm = h
        .client
        .query(&json!({ "read": "frontmatter", "path": "docs/guide.md" }))
        .unwrap();
    assert!(!docs_fm["result"]["frontmatter"].is_null());
}

#[test]
fn raw_write_file_to_auignore_still_rejects() {
    let mut h = started();
    // The `.arsumbris/` write-guard holds for the generic verbs: only
    // `set_ignores` may write the `.auignore`.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": ".arsumbris/.auignore",
            "content": "sneaky/\n",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "resp: {resp}");
}

#[test]
fn set_ignores_may_orphan_a_reference_advisory_not_rejected() {
    // The defining semantic of the config-mutation sort: newly excluding a
    // referenced file is a valid scope choice. The mutation SUCCEEDS and the
    // orphaned reference surfaces as an advisory diagnostic, never a rejection.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("type/note.type.yaml", "fields:\n  link?: note*\n");
    w("target.md", "---\ntype: note\n---\n");
    w(
        "referrer.md",
        "---\ntype: note\nlink: \"[[target]]\"\n---\n",
    );
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "t@t"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "seed"]);

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    // Exclude the reference target.
    let resp = client
        .query(&json!({
            "mutate": "set_ignores",
            "root": root.display().to_string(),
            "patterns": ["target.md"],
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "the mutation is NOT rejected: {resp}");
    assert_ne!(resp["type"], "error");

    // The orphaned reference is now an advisory diagnostic on the referrer, not a
    // blocked mutation.
    let diags = client.query(&json!({ "read": "diagnostics" })).unwrap();
    let codes: Vec<String> = diags["result"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        codes
            .iter()
            .any(|c| c == "reference-target-missing" || c == "navigational-target-not-found"),
        "expected an advisory dangling-reference diagnostic, got {codes:?}"
    );
}

#[test]
fn set_ignores_rejects_a_non_member_root() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores",
            "root": h.root.join("docs").display().to_string(),
            "patterns": ["x"],
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a subdir is not a member root: {resp}"
    );
}
