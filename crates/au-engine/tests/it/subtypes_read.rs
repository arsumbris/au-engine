//! The `subtypes` read over the socket: every type-def across the workspace
//! whose closure includes a base, deduped to its owner copy (carrying meta),
//! each with the owner repo. The type-level dual of `instances_of`.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// Two repos. `base` owns `projection`. `app` imports `base` and owns `bento`
/// (type: projection::base, with its own runtime meta block) and `grid`
/// (type: bento, transitive).
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/projection.type.yaml",
        "fields:\n  title: String\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w(
        "app/type/bento.type.yaml",
        "extends: projection::base\nmeta:\n  - type: runtime-meta\n    entry: bento.js\nfields:\n  cols: Number\n",
    );
    w(
        "app/type/grid.type.yaml",
        "extends: bento\nfields:\n  rows: Number\n",
    );
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
fn subtypes_enumerates_workspace_wide_owners_with_meta() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "subtypes", "base": "projection" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    assert_eq!(resp["result"]["base"], "projection");
    let subs = resp["result"]["subtypes"].as_array().unwrap();

    // bento (direct, via the folded projection::base edge) and grid
    // (transitive), each once, base excluded. Name-sorted.
    let names: Vec<&str> = subs.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["bento", "grid"],
        "workspace-wide, base excluded; got {names:?}"
    );

    let bento = subs.iter().find(|s| s["name"] == "bento").unwrap();
    let grid = subs.iter().find(|s| s["name"] == "grid").unwrap();

    // The owner repo rides on each match.
    assert_eq!(bento["repo"], "app");
    assert_eq!(grid["repo"], "app");

    // The owner's def carries its meta_blocks.
    assert!(
        bento["meta_blocks"]
            .as_array()
            .is_some_and(|m| !m.is_empty()),
        "bento carries its runtime meta block, got {:?}",
        bento["meta_blocks"]
    );

    // The full type-def shape rides along: parents and source.
    assert_eq!(bento["parents"], json!(["projection::base"]));
    assert_eq!(grid["parents"], json!(["bento"]));
    assert!(grid["source"]["file"]
        .as_str()
        .unwrap()
        .ends_with("grid.type.yaml"));
}

#[test]
fn subtypes_of_a_leaf_base_is_empty_not_null() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "subtypes", "base": "grid" }))
        .unwrap();
    assert_eq!(resp["result"]["subtypes"].as_array().unwrap().len(), 0);
}

/// The IMPORT case (no vendored copy): `base` owns `mcp.tool`. `app` declares
/// `base` a peer and extends it via `type: mcp.tool::base` with NO local copy of
/// `mcp.tool`. A grandchild `mcp.tool.shout.loud` extends the leaf with a bare
/// own parent, so it reaches the base transitively THROUGH the folded edge.
fn import_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/mcp.tool.type.yaml", "fields:\n  name: String\n");
    // An OWNER-repo-resident subtype: base extends its own `mcp.tool` by bare
    // name. base imports nothing, so it has no resolution graph; a qualified
    // `subtypes("mcp.tool::base")` must still surface this via identity.
    w(
        "base/type/mcp.tool.local.type.yaml",
        "extends: mcp.tool\nfields:\n  tag: String\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // Peer-extended leaf, no local `mcp.tool` copy.
    w(
        "app/type/mcp.tool.shout.type.yaml",
        "extends: mcp.tool::base\nfields:\n  msg: String\n",
    );
    // Grandchild: bare own parent, reaches the base transitively via the fold.
    w(
        "app/type/mcp.tool.shout.loud.type.yaml",
        "extends: mcp.tool.shout\nfields:\n  volume: Number\n",
    );
    (dir, root)
}

fn started_import() -> Harness {
    let (dir, root) = import_fixture();
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
fn subtypes_surfaces_a_peer_extended_leaf_across_the_fold() {
    let mut h = started_import();
    let resp = h
        .client
        .query(&json!({ "read": "subtypes", "base": "mcp.tool" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let subs = resp["result"]["subtypes"].as_array().unwrap();

    // The owner-repo subtype, the peer-extended leaf, and its own-graph
    // grandchild all surface, even though the leaf's ONLY path to the base is a
    // `mcp.tool::base` parent whose local copy was never present.
    let names: Vec<&str> = subs.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["mcp.tool.local", "mcp.tool.shout", "mcp.tool.shout.loud"],
        "owner-repo subtype, peer-extended leaf, and transitive child all surface; got {names:?}"
    );
    // Each is owned by the consumer repo, and its qualified parent is verbatim.
    let shout = subs.iter().find(|s| s["name"] == "mcp.tool.shout").unwrap();
    assert_eq!(shout["repo"], "app");
    assert_eq!(shout["parents"], json!(["mcp.tool::base"]));
}

#[test]
fn subtypes_qualified_base_scopes_to_the_owner_identity_including_owner_repo_subtypes() {
    let mut h = started_import();
    let resp = h
        .client
        .query(&json!({ "read": "subtypes", "base": "mcp.tool::base" }))
        .unwrap();
    assert_eq!(resp["result"]["base"], "mcp.tool::base");
    let names: Vec<&str> = resp["result"]["subtypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    // The qualified base scopes to base's `mcp.tool` IDENTITY. That identity is
    // reached by the peer-extended leaf (via the fold), its transitive child,
    // AND base's OWN `mcp.tool.local` (base imports nothing, so this is the
    // regression the review caught). All three, none dropped.
    assert_eq!(
        names,
        vec!["mcp.tool.local", "mcp.tool.shout", "mcp.tool.shout.loud"],
        "owner-repo subtype must not be dropped by the qualified form; got {names:?}"
    );
}

/// Two INDEPENDENT repos, ra and rb, each OWN a DIVERGENT `widget` (different
/// fields, so a different identity), and each owns a subtype of its own widget.
/// A qualified `subtypes("widget::ra")` must scope to ra's identity and return
/// ONLY ra's subtype, excluding rb's same-named-but-divergent subtree, while the
/// bare `subtypes("widget")` conflates both.
fn divergent_owners_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["ra", "rb"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("ra/.arsumbris/repo.yaml", "name: ra\n");
    w("ra/type/widget.type.yaml", "fields:\n  a: String\n");
    w(
        "ra/type/widget.ra_sub.type.yaml",
        "extends: widget\nfields:\n  x: Number\n",
    );
    w("rb/.arsumbris/repo.yaml", "name: rb\n");
    // Divergent widget: a different field, so a different identity.
    w("rb/type/widget.type.yaml", "fields:\n  b: Number\n");
    w(
        "rb/type/widget.rb_sub.type.yaml",
        "extends: widget\nfields:\n  y: Boolean\n",
    );
    (dir, root)
}

fn started_from(fx: (tempfile::TempDir, PathBuf)) -> Harness {
    let (dir, root) = fx;
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

fn subtype_names(resp: &serde_json::Value) -> Vec<String> {
    resp["result"]["subtypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn subtypes_qualified_base_excludes_a_divergent_same_named_owner() {
    let mut h = started_from(divergent_owners_fixture());

    // Qualified to ra's identity: only ra's subtype, rb's divergent one excluded.
    let ra = h
        .client
        .query(&json!({ "read": "subtypes", "base": "widget::ra" }))
        .unwrap();
    assert_eq!(
        subtype_names(&ra),
        vec!["widget.ra_sub"],
        "widget::ra scopes to ra's identity, excluding rb's divergent widget subtree"
    );

    // Qualified to rb's identity: only rb's subtype.
    let rb = h
        .client
        .query(&json!({ "read": "subtypes", "base": "widget::rb" }))
        .unwrap();
    assert_eq!(subtype_names(&rb), vec!["widget.rb_sub"]);

    // Bare conflates both same-named identities by name.
    let bare = h
        .client
        .query(&json!({ "read": "subtypes", "base": "widget" }))
        .unwrap();
    let mut names = subtype_names(&bare);
    names.sort();
    assert_eq!(names, vec!["widget.ra_sub", "widget.rb_sub"]);
}

/// A def carries a bare `parent: ghost` where `ghost` is defined nowhere (a
/// diagnosed dangling parent). `subtypes("ghost")` returns empty: a name that
/// resolves to no identity has no subtypes, consistent with the identity model.
fn dangling_base_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["solo"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("solo/.arsumbris/repo.yaml", "name: solo\n");
    w("solo/type/orphan.type.yaml", "extends: ghost\nfields: {}\n");
    (dir, root)
}

#[test]
fn subtypes_of_a_dangling_bare_base_is_empty() {
    let mut h = started_from(dangling_base_fixture());
    let resp = h
        .client
        .query(&json!({ "read": "subtypes", "base": "ghost" }))
        .unwrap();
    assert_eq!(
        resp["result"]["subtypes"].as_array().unwrap().len(),
        0,
        "a base that names no real identity (only a dangling parent) has no subtypes"
    );
}
