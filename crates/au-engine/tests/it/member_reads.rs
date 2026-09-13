//! The workspace member-topology reads over the socket: `members` enumerates
//! the declared members with their absolute roots and scattered-vs-subdir flag,
//! `resolve_member` maps a path to the member that owns it.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// Two repos nested under one root: `base` and `app`, each declaring itself.
/// Both are subdirs of the root, so neither is scattered.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    crate::seed_workspace(&root, &["base", "app"]);
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("app/.arsumbris/repo.yaml", "name: app\n");
    w("base/type/note.type.yaml", "fields:\n  title: String\n");
    w("app/a.md", "x");
    (dir, root)
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

#[test]
fn members_lists_declared_members_with_absolute_roots() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "members" })).unwrap();
    assert_eq!(resp["ready"], true);
    let members = resp["result"]["members"].as_array().unwrap();

    // The two declared members are present, sorted by name, with absolute roots.
    let by_name: std::collections::BTreeMap<&str, &serde_json::Value> = members
        .iter()
        .map(|m| (m["repo"].as_str().unwrap(), m))
        .collect();
    let app = by_name.get("app").expect("app member present");
    let base = by_name.get("base").expect("base member present");
    assert_eq!(app["root"], h.root.join("app").display().to_string());
    assert_eq!(base["root"], h.root.join("base").display().to_string());
    // Both are subdirs of the workspace root, so neither is scattered.
    assert_eq!(app["scattered"], false);
    assert_eq!(base["scattered"], false);
    // Declared `edit` members, so editable local authoring surfaces.
    assert_eq!(app["role"], "edit");
    assert_eq!(base["role"], "edit");
    assert_eq!(app["editable"], true);
    assert_eq!(base["editable"], true);
    assert_eq!(app["local"], true);
    assert_eq!(base["local"], true);

    // Stable order: sorted by name.
    let names: Vec<&str> = members
        .iter()
        .map(|m| m["repo"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "members are name-sorted");
}

/// The monorepo case: one git working tree at the workspace root covering both
/// members, neither of which has a `.git` of its own.
///
/// This is the fact a consumer cannot derive. Checking `<member>/.git` reports
/// both as untracked and badges them red, while the engine will happily refactor
/// across them: the covering tree is an ANCESTOR of each member's root, and the
/// two members share it, which is what says "these commit together".
#[test]
fn members_report_the_enclosing_working_tree_that_covers_them() {
    let (dir, root) = fixture();
    // One tree at the root, no `.git` inside either member.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let resp = client.query(&json!({ "read": "members" })).unwrap();
    let members = resp["result"]["members"].as_array().unwrap();
    let expected = root.display().to_string();
    for m in members {
        let name = m["repo"].as_str().unwrap();
        assert_eq!(m["git"]["tracked"], true, "{name} is covered by the tree");
        assert_eq!(
            m["git"]["root"], expected,
            "{name} must report the ENCLOSING tree, not its own directory"
        );
    }

    // For the NESTED members the covering tree is an ancestor, not their own
    // root. That is the case a consumer's own `<member>/.git` check gets wrong.
    // The entry member sits AT the tree root, so the two coincide there, which is
    // why this is asserted per member rather than across all of them.
    for name in ["base", "app"] {
        let m = members
            .iter()
            .find(|m| m["repo"] == name)
            .unwrap_or_else(|| panic!("{name} present"));
        assert_ne!(
            m["git"]["root"], m["root"],
            "{name}: the covering tree is an ancestor, not the member root"
        );
    }
    drop(dir);
}

/// A member no working tree covers reports `tracked: false` and a null root, the
/// genuine non-git case a refactor still refuses.
#[test]
fn members_report_an_uncovered_member_as_untracked() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "members" })).unwrap();
    let members = resp["result"]["members"].as_array().unwrap();
    for m in members {
        let name = m["repo"].as_str().unwrap();
        assert_eq!(m["git"]["tracked"], false, "{name} has no covering tree");
        assert!(
            m["git"]["root"].is_null(),
            "{name}: an untracked member reports a null root"
        );
    }
}

#[test]
fn resolve_member_maps_a_path_to_its_owning_member() {
    let mut h = started();

    // A path inside a member resolves to that member, by its root.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_member", "path": "app/a.md" }))
        .unwrap();
    assert_eq!(resp["result"]["resolve_member"]["repo"], "app");
    assert_eq!(
        resp["result"]["resolve_member"]["root"],
        h.root.join("app").display().to_string()
    );
    assert_eq!(resp["result"]["resolve_member"]["role"], "edit");
    assert_eq!(resp["result"]["resolve_member"]["editable"], true);
    assert_eq!(resp["result"]["resolve_member"]["local"], true);

    let resp = h
        .client
        .query(&json!({ "read": "resolve_member", "path": "base/type/note.type.yaml" }))
        .unwrap();
    assert_eq!(resp["result"]["resolve_member"]["repo"], "base");

    // A path under no declared member resolves to null.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_member", "path": "/etc/passwd" }))
        .unwrap();
    assert_eq!(resp["result"]["resolve_member"], serde_json::Value::Null);
}
