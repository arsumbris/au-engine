//! Untyped notes are first-class on the wire: their outgoing links,
//! frontmatter, typed blocks, and inbound edges all read, closing the gap
//! where reference and content reads only worked for typed instances.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// A knowledge base of untyped notes, no type vocabulary at all. `index.md` links to
/// `topic.md`, which carries a typed `^block-id` block.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("content")).unwrap();
    fs::write(
        root.join("content/index.md"),
        "---\ntitle: Index\n---\n# Index\n\nSee [[topic]] and [[missing]].\n",
    )
    .unwrap();
    fs::write(
        root.join("content/topic.md"),
        "---\ntitle: A Topic\ntags: [a, b]\n---\n# Topic\n\nprose.\n\n```yaml [:refs]\ntype: reference\nurl: x\n```\n^r1\n",
    )
    .unwrap();
    (dir, root)
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
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
    }
}

#[test]
fn note_outgoing_links_resolve() {
    let mut h = started();
    let out = h
        .client
        .query(&json!({ "read": "references_out", "path": "content/index.md" }))
        .unwrap();
    let refs = out["result"]["references_out"].as_array().unwrap();
    assert_eq!(
        refs.len(),
        2,
        "both body wikilinks surface for a note: {refs:?}"
    );

    let topic = refs.iter().find(|r| r["target"] == "topic").unwrap();
    assert!(topic["resolved"]
        .as_str()
        .unwrap()
        .ends_with("content/topic.md"));
    let missing = refs.iter().find(|r| r["target"] == "missing").unwrap();
    assert!(missing["resolved"].is_null(), "unresolved target is null");
}

#[test]
fn note_frontmatter_reads_without_a_type_key() {
    let mut h = started();
    let fm = h
        .client
        .query(&json!({ "read": "frontmatter", "path": "content/topic.md" }))
        .unwrap();
    let result = &fm["result"]["frontmatter"];
    assert!(result.is_object(), "a note's frontmatter is a map");
    assert_eq!(result["title"], "A Topic");
    assert_eq!(result["tags"], json!(["a", "b"]));
    assert!(result.get("type").is_none(), "a note carries no type claim");
}

#[test]
fn note_typed_block_resolves() {
    let mut h = started();
    let block = h
        .client
        .query(&json!({
            "read": "resolve_block_id", "target": "topic", "block_id": "r1"
        }))
        .unwrap();
    let result = &block["result"]["resolve_block_id"];
    assert!(result["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/topic.md"));
    assert_eq!(result["type_claim"], json!(["reference"]));
}

#[test]
fn a_note_contributes_backlinks_to_its_targets() {
    let mut h = started();
    let back = h
        .client
        .query(&json!({ "read": "references_in", "path": "content/topic.md" }))
        .unwrap();
    let edges = back["result"]["references_in"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        1,
        "the note's link surfaces as an inbound edge"
    );
    assert!(edges[0]["source"]
        .as_str()
        .unwrap()
        .ends_with("content/index.md"));
    assert_eq!(edges[0]["surface"], "body");
}

#[test]
fn note_content_reads_the_source() {
    let mut h = started();
    let c = h
        .client
        .query(&json!({ "read": "content", "path": "content/index.md" }))
        .unwrap();
    assert!(c["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("[[topic]]"));
}
