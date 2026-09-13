//! Path-scoped `diagnostics` reads range the served stream for one file or one
//! directory. This asserts the range path is a pure narrowing: a scoped read
//! equals the full read filtered by the same predicate, same entries, same
//! order.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _server: ServeHandle,
    client: Client,
}

fn harness(root: &std::path::Path) -> Harness {
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    engine.rebuild();
    let server = serve(engine.handle(), &socket).expect("serve");
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: tempfile::tempdir().unwrap(),
        _sock_dir: sock_dir,
        _server: server,
        client,
    }
}

/// A knowledge base with a typed `note`, and three notes across two dirs each carrying a
/// dangling `rel`, an advisory dangling-reference diagnostic apiece.
fn kb_with_diagnostics() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir_all(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  title: String\n  rel: note*\n",
    )
    .unwrap();
    for (d, n) in [("a", "n1"), ("a", "n2"), ("b", "n3")] {
        fs::create_dir_all(root.join(d)).unwrap();
        fs::write(
            root.join(format!("{d}/{n}.md")),
            format!("---\ntype: note\ntitle: t\nrel: [[gone-{n}]]\n---\n"),
        )
        .unwrap();
    }
    (dir, root)
}

fn entries(resp: &Value) -> Vec<Value> {
    // The `diagnostics` read carries its payload under its own name.
    resp["result"]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn file_of(d: &Value) -> &str {
    d["span"]["file"].as_str().unwrap()
}

#[test]
fn path_scoped_diagnostics_equals_full_filtered() {
    let (_dir, root) = kb_with_diagnostics();
    let mut h = harness(&root);

    let full = entries(&h.client.query(&json!({ "read": "diagnostics" })).unwrap());
    assert!(
        full.len() >= 3,
        "the three dangling refs should each diagnose, got {}",
        full.len()
    );

    // Per-file: the range path equals the full set filtered to that file.
    let by_file = entries(
        &h.client
            .query(&json!({ "read": "diagnostics", "path": "a/n1.md" }))
            .unwrap(),
    );
    let expected_file: Vec<Value> = full
        .iter()
        .filter(|d| file_of(d).ends_with("/a/n1.md"))
        .cloned()
        .collect();
    assert!(
        !expected_file.is_empty(),
        "a/n1.md should have a diagnostic"
    );
    assert_eq!(
        by_file, expected_file,
        "per-file range must match the filter"
    );

    // Per-directory: the prefix range equals the full set filtered to the dir.
    let by_prefix = entries(
        &h.client
            .query(&json!({ "read": "diagnostics", "path_prefix": "a" }))
            .unwrap(),
    );
    let expected_prefix: Vec<Value> = full
        .iter()
        .filter(|d| file_of(d).contains("/a/"))
        .cloned()
        .collect();
    assert_eq!(expected_prefix.len(), 2, "two notes live under a/");
    assert_eq!(
        by_prefix, expected_prefix,
        "prefix range must match the filter"
    );

    // Counts ride the same narrowing.
    let counts = h
        .client
        .query(&json!({ "read": "diagnostic_counts", "path": "a/n1.md" }))
        .unwrap();
    assert_eq!(
        counts["result"]["diagnostic_counts"]["total"]
            .as_u64()
            .unwrap() as usize,
        expected_file.len(),
        "scoped counts match the scoped set"
    );
}
