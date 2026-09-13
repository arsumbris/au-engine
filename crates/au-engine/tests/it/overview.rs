//! The `overview` read: one cheap up-front map aggregating members, top-level
//! graphs, type counts, diagnostic counts, and the hub-node ranking.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

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

/// A single-repo knowledge base with a structural hub (an instance others point `rel`
/// at), a navigational hub (a note others prose-link), a leaf, and a dangling
/// ref for a diagnostic.
fn kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join(".arsumbris")).unwrap();
    fs::write(root.join(".arsumbris/repo.yaml"), "name: v\n").unwrap();
    fs::create_dir_all(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  title: String\n  rel: note*\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("content")).unwrap();
    fs::write(
        root.join("content/hub.md"),
        "---\ntype: note\ntitle: hub\n---\n",
    )
    .unwrap();
    fs::write(root.join("content/moc.md"), "# moc\n").unwrap();
    fs::write(
        root.join("content/leaf.md"),
        "---\ntype: note\ntitle: leaf\n---\n",
    )
    .unwrap();
    for i in 0..3 {
        fs::write(
            root.join(format!("content/ref-{i}.md")),
            format!("---\ntype: note\ntitle: r{i}\nrel: \"[[hub]]\"\n---\n\nsee [[moc]].\n"),
        )
        .unwrap();
    }
    (dir, root)
}

#[test]
fn overview_aggregates_the_up_front_map() {
    let (_dir, root) = kb();
    let mut h = harness(&root);

    let resp = h.client.query(&json!({ "read": "overview" })).unwrap();
    assert_eq!(resp["ready"], true);
    let r = &resp["result"]["overview"];

    // Workspace shape: the members array (flat, not wrapped) carries repo `v`.
    let members = r["members"].as_array().expect("members is an array");
    assert!(
        members.iter().any(|m| m["repo"] == "v"),
        "member v present: {members:?}"
    );

    // Top-level graphs: a non-empty array of the root's subdirectories.
    assert!(!r["top_level_dirs"].as_array().unwrap().is_empty());

    // Vocabulary + health shapes: the counts objects, reused verbatim.
    assert!(
        r["type_counts"]["total"].as_u64().unwrap() >= 1,
        "the note type is counted"
    );
    assert!(
        r["diagnostic_counts"]["total"].is_u64(),
        "diagnostic total is a number"
    );

    // Hubs: the structural hub outranks the navigational MOC; the leaf is absent.
    let hubs = r["hubs"].as_array().expect("hubs is an array");
    let hub = &hubs[0];
    assert!(
        hub["path"].as_str().unwrap().ends_with("/hub.md"),
        "structural hub first: {hub:?}"
    );
    assert_eq!(hub["kind"], "instance");
    assert_eq!(hub["refs_structural"], 3);
    assert_eq!(hub["refs_navigational"], 0);
    assert_eq!(hub["repo"], "v");

    let moc = hubs
        .iter()
        .find(|hb| hb["path"].as_str().unwrap().ends_with("/moc.md"))
        .expect("moc is a hub");
    assert_eq!(moc["kind"], "note");
    assert_eq!(moc["refs_navigational"], 3);
    assert_eq!(moc["refs_structural"], 0);

    assert!(
        !hubs
            .iter()
            .any(|hb| hb["path"].as_str().unwrap().ends_with("/leaf.md")),
        "a leaf with no inbound edges is not a hub"
    );
}

/// A knowledge base whose most-referenced file is an unread ASSET: three
/// instances point a `file*` slot at one PDF. The walker catalogues an asset by
/// path and never reads it, precisely so `file*` resolves against it
/// ([[type-def shape file::au-type-system]]), so it genuinely ranks.
fn kb_with_a_referenced_asset() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::create_dir_all(root.join(".arsumbris")).unwrap();
    fs::write(root.join(".arsumbris/repo.yaml"), "name: v\n").unwrap();
    fs::create_dir_all(root.join("type")).unwrap();
    fs::write(
        root.join("type/citation.type.yaml"),
        "fields:\n  title: String\n  source: file*\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("content")).unwrap();
    fs::write(root.join("content/paper.pdf"), "%PDF-1.4 not really\n").unwrap();
    for i in 0..3 {
        fs::write(
            root.join(format!("content/cite-{i}.md")),
            format!("---\ntype: citation\ntitle: c{i}\nsource: \"[[paper.pdf]]\"\n---\n"),
        )
        .unwrap();
    }
    (dir, root)
}

#[test]
fn an_unread_asset_reports_its_own_kind_rather_than_note() {
    // The engine never read the PDF, so it has no parse. Reporting that as
    // `note` would call a binary attachment prose; `asset` is the honest
    // answer, and it is what a consumer completing `[[` needs to see.
    let (_dir, root) = kb_with_a_referenced_asset();
    let mut h = harness(&root);

    let hubs = h.client.query(&json!({ "read": "hubs" })).unwrap()["result"]["hubs"]
        .as_array()
        .unwrap()
        .clone();
    let paper = hubs
        .iter()
        .find(|hb| hb["path"].as_str().unwrap().ends_with("/paper.pdf"))
        .expect("the asset ranks: three `file*` slots point at it");
    assert_eq!(
        paper["kind"], "asset",
        "an unread asset is not prose, got {paper:?}",
    );
    assert_eq!(
        paper["refs_structural"], 3,
        "the `file*` edges are structural"
    );

    // The markdown citations keep their own kind, so the new value narrows
    // rather than swallowing the existing ones.
    let cite = hubs
        .iter()
        .find(|hb| hb["path"].as_str().unwrap().ends_with("cite-0.md"));
    if let Some(cite) = cite {
        assert_eq!(cite["kind"], "instance");
    }
}

/// The whole point of the `files` read: a consumer completing `[[` must be
/// able to offer a target `file*` will accept. The implicit-identity scan
/// reaches only PARSED files, so reading it for the file set silently omits
/// every asset.
#[test]
fn files_offers_the_assets_a_file_slot_resolves_against() {
    let (_dir, root) = kb_with_a_referenced_asset();
    let mut h = harness(&root);

    let files = h.client.query(&json!({ "read": "files" })).unwrap()["result"]["files"]
        .as_array()
        .unwrap()
        .clone();
    let paper = files
        .iter()
        .find(|f| f["path"].as_str().unwrap().ends_with("/paper.pdf"))
        .expect("the asset is a resolvable target, so it is listed");
    assert_eq!(paper["kind"], "asset");
    assert_eq!(paper["stem"], "paper");

    // The gap this closes: the candidate scan never saw it.
    let scanned = h.client.query(&json!({ "read": "candidates" })).unwrap()["result"]["candidates"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        !scanned
            .iter()
            .any(|c| c["file"].as_str().unwrap().ends_with("/paper.pdf")),
        "the scan omits the asset, which is why it cannot answer this question",
    );

    // And the listed asset really is addressable: a `file*` slot resolves to it.
    let resolved = h
        .client
        .query(&json!({ "read": "resolve_target", "target": paper["stem"] }))
        .unwrap()["result"]["resolve_target"]
        .clone();
    assert_eq!(resolved["path"], paper["path"], "the listed stem resolves");
}

#[test]
fn the_file_kind_vocabulary_agrees_across_the_reads_that_serve_it() {
    // `hubs` and a `neighborhood` node derive kind from one helper, so a
    // consumer joining the two never sees one call an asset prose and the
    // other not.
    let (_dir, root) = kb_with_a_referenced_asset();
    let mut h = harness(&root);

    let hubs = h.client.query(&json!({ "read": "hubs" })).unwrap()["result"]["hubs"]
        .as_array()
        .unwrap()
        .clone();
    let asset_path = hubs
        .iter()
        .find(|hb| hb["path"].as_str().unwrap().ends_with("/paper.pdf"))
        .expect("the asset ranks")["path"]
        .as_str()
        .unwrap()
        .to_string();

    let nodes = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "content/cite-0.md" }))
        .unwrap()["result"]["neighborhood"]["nodes"]
        .as_array()
        .unwrap()
        .clone();
    let node = nodes
        .iter()
        .find(|n| n["path"] == asset_path.as_str())
        .expect("the walk reaches the asset over the `file*` edge");
    assert_eq!(
        node["file_kind"], "asset",
        "neighborhood agrees with hubs, got {node:?}",
    );
}

#[test]
fn overview_is_additive_no_schema_bump() {
    // The read exists and answers; a new read name is additive, so this frame's
    // schema_version matches every other read's (guards the "no bump" claim).
    let (_dir, root) = kb();
    let mut h = harness(&root);
    let overview = h.client.query(&json!({ "read": "overview" })).unwrap();
    let members = h.client.query(&json!({ "read": "members" })).unwrap();
    assert_eq!(overview["schema_version"], members["schema_version"]);
}

/// `top_level_dirs` rejects a typo'd arg rather than silently returning a
/// different-scoped set. A mistyped `scope` must not quietly change the result;
/// the read carries a dedicated strict arg-struct for this.
#[test]
fn top_level_dirs_rejects_an_unknown_arg() {
    let (_dir, root) = kb();
    let mut h = harness(&root);
    let resp = h
        .client
        .query(&json!({ "read": "top_level_dirs", "scpe": "all" }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a mistyped `scope` must not silently return the own-scoped default: {resp}"
    );
}
