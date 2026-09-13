//! The knowledge-base-wide introspection reads: per-instance introspection and the
//! implicit-identity candidate scan, served over the wire.

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

/// Boot an engine over `root`, build once, serve, and connect a client.
fn harness(root: &std::path::Path) -> Harness {
    let dir_keep = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    engine.rebuild();
    let server = serve(engine.handle(), &socket).expect("serve");
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir_keep,
        _sock_dir: sock_dir,
        _server: server,
        client,
    }
}

fn clean_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/a.type.yaml"), "fields:\n  title: String\n").unwrap();
    fs::write(root.join("type/b.type.yaml"), "fields:\n  title: String\n").unwrap();
    // Claims `a`, carries `title`, so `b` is an implicit candidate it doesn't claim.
    fs::write(root.join("doc.md"), "---\ntype: a\ntitle: hello\n---\n").unwrap();
    (dir, root)
}

#[test]
fn instances_read_returns_per_instance_introspection() {
    let (_dir, root) = clean_kb();
    let mut h = harness(&root);

    let resp = h.client.query(&json!({ "read": "instances" })).unwrap();
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["ready"], true);
    let result = &resp["result"];
    assert_eq!(result["aborted_at_load"], false);
    // Three instances: the user's `doc.md`, the folder-repo's `.arsumbris/repo.yaml`
    // (an au.engine.repo node), and its root `README.md` (an au.engine.readme node).
    assert_eq!(result["count"], 3);

    let entries = result["instances"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        3,
        "three resolved instances, got {entries:?}"
    );
    let entry = entries
        .iter()
        .find(|e| e["file"].as_str().unwrap().ends_with("doc.md"))
        .expect("the doc.md instance");
    assert_eq!(entry["claim"][0], "a");
    assert!(
        entry["closure"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "a"),
        "closure contains the claimed type, got {:?}",
        entry["closure"]
    );
    assert!(
        entry["effective_shape"].is_array(),
        "effective_shape present"
    );
    assert!(
        entry["effective_values"].is_array(),
        "the value layer rides the introspection"
    );
}

#[test]
fn instances_read_signals_aborted_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    // A malformed type-def is a load-phase error, so the build aborts before
    // instance validation.
    fs::write(root.join("type/bad.type.yaml"), "fields: [unclosed\n").unwrap();
    fs::write(root.join("doc.md"), "---\ntype: bad\n---\n").unwrap();

    let mut h = harness(&root);
    let resp = h.client.query(&json!({ "read": "instances" })).unwrap();
    let result = &resp["result"];
    assert_eq!(
        result["aborted_at_load"], true,
        "a broken vocabulary aborts at load, got {result:?}"
    );
    assert_eq!(
        result["instances"].as_array().unwrap().len(),
        0,
        "no instances are validated against a broken graph"
    );
}

/// Three docs, each claims `a` and carries `title`, so each is an implicit
/// candidate for `b`. Gives the scan multiple files to page over.
fn multi_candidate_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/a.type.yaml"), "fields:\n  title: String\n").unwrap();
    fs::write(root.join("type/b.type.yaml"), "fields:\n  title: String\n").unwrap();
    for n in 1..=3 {
        fs::write(
            root.join(format!("doc{n}.md")),
            "---\ntype: a\ntitle: hi\n---\n",
        )
        .unwrap();
    }
    (dir, root)
}

#[test]
fn candidates_summary_projects_names_and_paging_bounds_the_file_list() {
    let (_dir, root) = multi_candidate_kb();
    let mut h = harness(&root);

    // Summary: each file carries just the candidate type NAMES, no per-candidate
    // detail object.
    let s = h
        .client
        .query(&json!({ "read": "candidates", "summary": true }))
        .unwrap();
    assert_eq!(s["result"]["aborted_at_load"], false);
    let files = s["result"]["candidates"].as_array().unwrap();
    // Five scanned files: the three docs, the folder-repo's repo.yaml node
    // (au.engine.repo), and its root README.md node (au.engine.readme), the two
    // engine nodes carrying no candidates of their own.
    assert_eq!(
        files.len(),
        5,
        "three docs plus the repo.yaml and README nodes"
    );
    // A doc file carries candidate NAMES (bare strings, not objects) in summary.
    let doc = files
        .iter()
        .find(|f| !f["candidates"].as_array().unwrap().is_empty())
        .expect("a scanned file with candidates");
    assert!(doc["file"].is_string());
    assert!(
        doc["candidates"][0].is_string(),
        "a summary candidate is a bare name, not an object, got {}",
        doc["candidates"]
    );
    let names: Vec<&str> = doc["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"b"),
        "summary lists candidate names, got {names:?}"
    );

    // Paging over files, in the catalog's sorted order: the two engine nodes
    // (repo.yaml, then README.md) sort ahead of the three docs.
    let p1 = h
        .client
        .query(&json!({ "read": "candidates", "limit": 3 }))
        .unwrap();
    assert_eq!(p1["result"]["candidates"].as_array().unwrap().len(), 3);
    // Full detail intact on a paged non-summary entry: a scanned file with
    // candidates carries objects, not bare names.
    let detailed = p1["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| !f["candidates"].as_array().unwrap().is_empty())
        .expect("a paged file with candidates");
    assert_eq!(detailed["candidates"][0]["type_name"], "b");

    let p2 = h
        .client
        .query(&json!({ "read": "candidates", "limit": 2, "offset": 3 }))
        .unwrap();
    assert_eq!(p2["result"]["candidates"].as_array().unwrap().len(), 2);

    // Past the end is an empty page (five files, so offset 5 is the boundary).
    let p3 = h
        .client
        .query(&json!({ "read": "candidates", "limit": 2, "offset": 5 }))
        .unwrap();
    assert_eq!(p3["result"]["candidates"].as_array().unwrap().len(), 0);

    // The scan is knowledge-base-wide, so there is no `repo` scope; an unknown arg errors.
    let bad = h
        .client
        .query(&json!({ "read": "candidates", "repo": "x" }))
        .unwrap();
    assert_eq!(
        bad["type"], "error",
        "candidates rejects an unknown arg rather than ignoring it"
    );
}

/// One doc with a candidate, one without, so the counts distinguish
/// total_files from files_with_candidates.
fn mixed_candidate_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/a.type.yaml"), "fields:\n  title: String\n").unwrap();
    fs::write(root.join("type/b.type.yaml"), "fields:\n  title: String\n").unwrap();
    fs::write(root.join("type/c.type.yaml"), "fields:\n  zzz: String\n").unwrap();
    // Claims `a`, carries `title`, so it is a candidate for `b`.
    fs::write(root.join("doc1.md"), "---\ntype: a\ntitle: hi\n---\n").unwrap();
    // Claims `c`, carries `zzz`, so a/b (which need title) are not candidates.
    fs::write(root.join("doc2.md"), "---\ntype: c\nzzz: x\n---\n").unwrap();
    (dir, root)
}

#[test]
fn candidate_counts_summarize_the_scan_without_materializing_it() {
    let (_dir, root) = mixed_candidate_kb();
    let mut h = harness(&root);

    let resp = h
        .client
        .query(&json!({ "read": "candidate_counts" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let r = &resp["result"]["candidate_counts"];
    assert_eq!(r["aborted_at_load"], false);
    // Four scanned instances: the two docs, the folder-repo's repo.yaml node
    // (au.engine.repo), and its root README.md node (au.engine.readme), neither
    // engine node with candidates of its own.
    assert_eq!(
        r["total_files"], 4,
        "both docs plus the repo.yaml and README nodes"
    );
    assert_eq!(r["files_with_candidates"], 1, "only doc1 has a candidate");
    assert_eq!(
        r["by_type"],
        json!({ "b": 1 }),
        "one file could claim b, none could claim a/c"
    );

    // total_files reconciles with the candidates read's file count.
    let cands = h.client.query(&json!({ "read": "candidates" })).unwrap();
    assert_eq!(
        r["total_files"].as_u64().unwrap() as usize,
        cands["result"]["candidates"].as_array().unwrap().len()
    );

    // Takes no args; a bogus one is rejected.
    let bad = h
        .client
        .query(&json!({ "read": "candidate_counts", "summary": true }))
        .unwrap();
    assert_eq!(bad["type"], "error", "candidate_counts rejects any arg");
}

#[test]
fn candidates_read_returns_ranked_candidates() {
    let (_dir, root) = clean_kb();
    let mut h = harness(&root);

    let resp = h.client.query(&json!({ "read": "candidates" })).unwrap();
    assert_eq!(resp["ready"], true);
    let result = &resp["result"];
    assert_eq!(result["aborted_at_load"], false);

    let files = result["candidates"].as_array().unwrap();
    let doc = files
        .iter()
        .find(|f| f["file"].as_str().unwrap().ends_with("doc.md"))
        .expect("doc.md is in the scan");
    let candidates = doc["candidates"].as_array().unwrap();
    let b = candidates
        .iter()
        .find(|c| c["type_name"] == "b")
        .unwrap_or_else(|| panic!("doc.md is a candidate for b, got {candidates:?}"));
    assert!(
        b["satisfied_required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "title"),
        "b is satisfied by the title field, got {:?}",
        b["satisfied_required"]
    );
    assert_eq!(b["scope"]["inline_path"], "");
    let _: &Value = &b["also_satisfied_optional"];
}
