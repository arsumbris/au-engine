//! The `type_graph` read over the socket: the schema graph as a node+edge
//! payload, the type-side sibling of `link_graph`. `subtype` + `field-type` by
//! default, `instance-of` + `meta` opt-in. Edges carry a `count` (a field-type
//! pair reached through several fields folds to one weighted edge).

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// One repo `v`. `thing` is a base; `note` extends it (a subtype edge); `book`
/// references `note` through TWO fields (a field-type edge weighted 2); `n1`
/// claims `note` (an instance-of edge). `tagged` carries a `runtime-meta` block
/// (a meta edge).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &[]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("type/thing.type.yaml", "fields:\n  name: String\n");
    w(
        "type/note.type.yaml",
        "extends: thing\nfields:\n  body: String\n",
    );
    w(
        "type/book.type.yaml",
        "fields:\n  author: note*\n  editor: note*\n",
    );
    w(
        "type/runtime-meta.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  entry: String\n",
    );
    w(
        "type/tagged.type.yaml",
        "meta:\n  - type: runtime-meta\n    entry: t.js\nfields:\n  x: Number\n",
    );
    w("n1.md", "---\ntype: note\nname: x\nbody: y\n---\n");
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

/// Edges as `(from-basename, to-basename, relation, count)`, sorted, so an
/// assertion is path-prefix-independent.
fn edges(result: &Value) -> Vec<(String, String, String, u64)> {
    let base = |p: &str| p.rsplit('/').next().unwrap().to_string();
    let mut out: Vec<_> = result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                base(e["from"].as_str().unwrap()),
                base(e["to"].as_str().unwrap()),
                e["relation"].as_str().unwrap().to_string(),
                e["count"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// Node basenames, sorted.
fn nodes(result: &Value) -> Vec<String> {
    let mut out: Vec<String> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            n["path"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn default_edges_are_the_subtype_and_field_type_backbone() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "type_graph" })).unwrap();
    assert_eq!(resp["ready"], true);
    let r = &resp["result"]["type_graph"];
    assert_eq!(r["scope"], "own");

    // Only type-defs are nodes by default; no instance node (instance-of is off).
    assert_eq!(
        nodes(r),
        vec![
            "book.type.yaml",
            "note.type.yaml",
            "runtime-meta.type.yaml",
            "tagged.type.yaml",
            "thing.type.yaml"
        ]
    );

    // subtype note->thing (count 1); field-type book->note WEIGHTED 2 (author +
    // editor); no meta edge (opt-in), no instance-of edge (opt-in).
    assert_eq!(
        edges(r),
        vec![
            (
                "book.type.yaml".into(),
                "note.type.yaml".into(),
                "field-type".into(),
                2
            ),
            (
                "note.type.yaml".into(),
                "thing.type.yaml".into(),
                "subtype".into(),
                1
            ),
        ]
    );
}

#[test]
fn instance_of_is_opt_in_and_adds_the_claiming_instance_as_a_node() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "type_graph", "edges": ["instance-of"] }))
        .unwrap();
    let r = &resp["result"]["type_graph"];

    // Only the instance-of edge is requested, so no subtype / field-type edge.
    assert_eq!(
        edges(r),
        vec![(
            "n1.md".into(),
            "note.type.yaml".into(),
            "instance-of".into(),
            1
        )]
    );
    // The claiming instance joins the node set.
    assert!(nodes(r).contains(&"n1.md".to_string()));
    let n1 = resp["result"]["type_graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["path"].as_str().unwrap().ends_with("n1.md"))
        .unwrap();
    assert_eq!(n1["kind"], "instance");
    assert_eq!(n1["repo"], "v");
}

#[test]
fn meta_edges_are_opt_in() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "type_graph", "edges": ["meta"] }))
        .unwrap();
    let r = &resp["result"]["type_graph"];
    assert_eq!(
        edges(r),
        vec![(
            "tagged.type.yaml".into(),
            "runtime-meta.type.yaml".into(),
            "meta".into(),
            1
        )]
    );
}

#[test]
fn an_unknown_edge_class_is_a_malformed_read_error() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "type_graph", "edges": ["bogus"] }))
        .unwrap();
    assert_eq!(resp["type"], "error", "unknown edge class rejects: {resp}");
}

/// A cross-repo `extends: thing::base` renders a `subtype` edge from the leaf's
/// def file to the PEER owner's def file. The edge resolver maps a `::repo`
/// parent to the named peer's graph (`graph_for_repo(dep)`), so the drawable
/// spans the repo boundary. Own-graph-only the `::base` parent resolves against
/// `app`'s graph (no `thing`), the edge drops — the non-vacuous guard for the
/// `M-graphedge` name->file resolution.
#[test]
fn a_cross_repo_parent_renders_a_subtype_edge_to_the_peer_def() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/thing.type.yaml", "fields:\n  name: String\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w(
        "app/type/note.type.yaml",
        "extends: thing::base\nfields:\n  body: String\n",
    );

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    // scope: all, so both repos' type-defs are in the node universe.
    let resp = client
        .query(&json!({ "read": "type_graph", "scope": "all" }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let r = &resp["result"]["type_graph"];
    assert!(
        edges(r).contains(&(
            "note.type.yaml".to_string(),
            "thing.type.yaml".to_string(),
            "subtype".to_string(),
            1
        )),
        "the cross-repo `extends: thing::base` edge resolves to base's def file: {:?}",
        edges(r)
    );
}
