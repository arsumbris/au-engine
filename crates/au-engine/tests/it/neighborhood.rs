//! The `neighborhood` read over the socket: the wire envelope, the DTO shape,
//! the error frames, and the block-node addressing the walk's unit tests cannot
//! exercise end-to-end. The walk LOGIC (depth, cycles, diamonds, both,
//! truncation) is covered by `neighborhood.rs`'s module tests; here we prove the
//! contract a consumer sees.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// A single repo: a chain a → b → c (prose), a frontmatter field edge, a
/// typed block with a `^^` block-referent and a bare `^` anchor into it, and a
/// dangling link.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(root.join("type/note.type.yaml"), "fields:\n  ref?: note*\n").unwrap();
    let note = |body: &str| format!("---\ntype: note\n---\n{body}\n");
    fs::write(root.join("a.md"), note("See [[b]].")).unwrap();
    fs::write(root.join("b.md"), note("See [[c]].")).unwrap();
    fs::write(root.join("c.md"), note("A leaf.")).unwrap();
    // A frontmatter field edge fr.ref -> a, plus a dangling prose link.
    fs::write(
        root.join("fr.md"),
        "---\ntype: note\nref: \"[[a]]\"\n---\nGone: [[nowhere]].\n",
    )
    .unwrap();
    // hub.md carries a typed block `^blk`. ref.md addresses it two ways: the
    // block-referent `^^blk` (mints a block node) and a bare `^blk` anchor
    // (reaches the file node).
    fs::write(
        root.join("hub.md"),
        "---\ntype: note\n---\n# Hub\n\n```yaml [:ref]\ntype: note\n```\n^blk\n",
    )
    .unwrap();
    fs::write(
        root.join("ref.md"),
        "---\ntype: note\n---\nPull [[hub^^blk]] and jump [[hub^blk]].\n",
    )
    .unwrap();
    // A plain note: frontmatter-less, no `type:` claim, so it is a Note not an
    // Instance and its `instance` enrichment resolves to null.
    fs::write(root.join("plain.md"), "Just prose, no type.\n").unwrap();
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
fn depth_one_out_returns_the_seed_and_its_targets() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["type"], "response");
    let n = &resp["result"]["neighborhood"];
    let nodes = n["nodes"].as_array().unwrap();
    // Seed a (depth 0) plus b (depth 1).
    let names: Vec<&str> = nodes
        .iter()
        .map(|x| x["path"].as_str().unwrap().rsplit('/').next().unwrap())
        .collect();
    assert!(
        names.contains(&"a.md") && names.contains(&"b.md"),
        "{names:?}"
    );
    assert!(
        !names.contains(&"c.md"),
        "depth 1 stops before c: {names:?}"
    );
    // The seed carries its sizes; a typed note is an instance.
    let seed = nodes
        .iter()
        .find(|x| x["path"].as_str().unwrap().ends_with("a.md"))
        .unwrap();
    assert_eq!(seed["depth"], 0);
    assert_eq!(seed["file_kind"], "instance");
    assert!(
        seed["bytes"].as_u64().unwrap() > 0,
        "seed carries a byte size"
    );
    // The edge a -> b is present, kind navigational (body prose).
    let edges = n["edges"].as_array().unwrap();
    let ab = edges
        .iter()
        .find(|e| e["from"]["path"].as_str().unwrap().ends_with("a.md"))
        .unwrap();
    assert_eq!(ab["kind"], "navigational");
    assert!(ab["to"]["path"].as_str().unwrap().ends_with("b.md"));
}

#[test]
fn a_frontmatter_field_edge_is_kind_field() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "fr.md" }))
        .unwrap();
    let edges = resp["result"]["neighborhood"]["edges"].as_array().unwrap();
    let field = edges.iter().find(|e| e["kind"] == "field").unwrap();
    assert_eq!(field["surface"], "frontmatter");
    assert_eq!(field["field"], "ref");
    assert!(field["to"]["path"].as_str().unwrap().ends_with("a.md"));
    // The dangling prose link is reported with a null `to`.
    assert!(
        edges.iter().any(|e| e["to"].is_null()),
        "the dangling edge has a null target: {edges:?}"
    );
}

#[test]
fn the_kind_filter_selects_edges_over_the_wire() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "fr.md", "kinds": ["field"] }))
        .unwrap();
    let n = &resp["result"]["neighborhood"];
    let names: Vec<&str> = n["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["path"].as_str().unwrap().rsplit('/').next().unwrap())
        .collect();
    // Only the field target `a` is reached; the dangling prose link is dropped.
    assert!(names.contains(&"a.md"), "field target reached: {names:?}");
    let edges = n["edges"].as_array().unwrap();
    assert!(
        edges.iter().all(|e| e["kind"] == "field"),
        "only field edges: {edges:?}"
    );
}

#[test]
fn kinds_is_required_past_depth_one() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "depth": 2 }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an absent kinds at depth 2 errors: {resp}"
    );
    assert!(
        resp["error"].as_str().unwrap().contains("kinds"),
        "the message names the rule: {resp}"
    );
    // With kinds supplied, depth 2 is allowed.
    let ok = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "depth": 2, "kinds": ["navigational"] }))
        .unwrap();
    assert_eq!(ok["type"], "response", "kinds present unblocks depth 2");
}

#[test]
fn an_unknown_kind_is_an_error_frame() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "kinds": ["bogus"] }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    assert!(resp["error"].as_str().unwrap().contains("bogus"), "{resp}");
}

#[test]
fn an_unknown_direction_is_a_malformed_read() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "direction": "sideways" }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an unknown direction value errors: {resp}"
    );
}

#[test]
fn an_unsupported_flag_is_rejected_not_silently_ignored() {
    let mut h = started();
    // `frontmatter` enrichment is planned but not built; `deny_unknown_fields`
    // rejects it loudly rather than silently ignoring a flag that does nothing.
    // (The built flags — content / body / instance — are exercised elsewhere.)
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "frontmatter": true }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an unsupported flag errors, not a silent no-op"
    );
}

#[test]
fn max_nodes_truncates_and_names_the_dropped() {
    let mut h = started();
    // fr -> a and fr -> nowhere(dangling); a -> b. Seed fr, depth 2, cap 2.
    let resp = h
        .client
        .query(&json!({
            "read": "neighborhood", "path": "a.md",
            "depth": 3, "kinds": ["navigational"], "max_nodes": 2
        }))
        .unwrap();
    let n = &resp["result"]["neighborhood"];
    assert_eq!(n["nodes"].as_array().unwrap().len(), 2, "seed plus one");
    assert_eq!(n["truncated"], true);
    assert_eq!(n["truncated_at_depth"], 2);
    let dropped = n["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 1, "one node named as dropped: {dropped:?}");
    assert!(dropped[0]["path"].as_str().unwrap().ends_with("c.md"));
}

#[test]
fn a_block_referent_reaches_a_block_node_a_bare_anchor_the_file() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "ref.md" }))
        .unwrap();
    let n = &resp["result"]["neighborhood"];
    let nodes = n["nodes"].as_array().unwrap();
    // The `^^blk` link mints a block node (hub.md + block_id "blk").
    let block_node = nodes
        .iter()
        .find(|x| x["block_id"] == "blk")
        .unwrap_or_else(|| panic!("a block node for ^^blk: {nodes:?}"));
    assert!(block_node["path"].as_str().unwrap().ends_with("hub.md"));
    // The bare `^blk` link reaches the FILE node (hub.md, no block_id).
    let file_node = nodes
        .iter()
        .find(|x| x["path"].as_str().unwrap().ends_with("hub.md") && x["block_id"].is_null());
    assert!(
        file_node.is_some(),
        "a bare ^ reaches the file node: {nodes:?}"
    );
    // The two are distinct nodes in one file.
    let hub_nodes = nodes
        .iter()
        .filter(|x| x["path"].as_str().unwrap().ends_with("hub.md"))
        .count();
    assert_eq!(
        hub_nodes, 2,
        "the block node and the file node are distinct"
    );
}

#[test]
fn both_reaches_a_referrer() {
    let mut h = started();
    // From b: out to c, and IN from a (a -> b). `both` sees both.
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "b.md", "direction": "both" }))
        .unwrap();
    let names: Vec<&str> = resp["result"]["neighborhood"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["path"].as_str().unwrap().rsplit('/').next().unwrap())
        .collect();
    assert!(
        names.contains(&"a.md"),
        "both sees the referrer a: {names:?}"
    );
    assert!(names.contains(&"c.md"), "both sees the target c: {names:?}");
}

/// A workspace where `app` (an editable member) depends on `base` (a `dep`,
/// non-editable). `app/doc.md` links a local note and a note in `base`, so the
/// walk crosses the repo boundary. Exercises `scope: own` versus `all`.
fn cross_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    // Entry `v`, editing itself and `app`; `app` depends on `base`.
    w(
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    w(
        ".arsumbris/workspace.yaml",
        "edit:\n  - v\n  - app\ntype: au.engine.workspace::au-engine\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/thing.md", "A note owned by the dependency.\n");
    w("app/local.md", "A note owned by app.\n");
    w("app/doc.md", "See [[thing::base]] and [[local]].\n");
    (dir, root)
}

/// A neighborhood walk reaching a `^^`-addressed block on a cross-repo slot-pinned
/// nested record: the block node is minted and sized.
///
/// NOTE: this is a cross-repo SMOKE test, NOT an owner-relative guard. `block_span`
/// reads only the record's `.span`, which `walk_value` emits for ANY block-id
/// record regardless of whether its claim resolves owner-relative (the node carries
/// no claim / type). So the neighborhood block surface is owner-relative-INDEPENDENT
/// — verified: this test stays green even with `record_targets_of_kb` forced to the
/// own-graph (claim-dropping) path. Kept as a cross-repo block-node smoke test.
fn cross_repo_slot_pinned_block() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w(
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    w(
        ".arsumbris/workspace.yaml",
        "edit:\n  - v\n  - app\ntype: au.engine.workspace::au-engine\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    w(
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n",
    );
    // A claim-less slot-pinned peer record carrying a block-id, in a peer-claiming host.
    w(
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - ^: c1\n    salience: focal\n---\n",
    );
    // A referrer that addresses the block with `^^`.
    w("app/ref.md", "See [[extraction^^c1]].\n");
    (dir, root)
}

#[test]
fn a_cross_repo_slot_pinned_block_node_is_sized() {
    let (_dir, root) = cross_repo_slot_pinned_block();
    let (_sd, _engine, _server, mut client) = serve_on(&root);
    let resp = client
        .query(&json!({
            "read": "neighborhood", "path": "app/ref.md", "content": true
        }))
        .unwrap();
    let block = resp["result"]["neighborhood"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["block_id"] == "c1")
        .cloned()
        .unwrap_or_else(|| panic!("a block node for ^^c1: {resp:?}"));
    assert!(
        block["path"].as_str().unwrap().ends_with("extraction.md"),
        "the block node is on the cross-repo host: {block:?}"
    );
    // Sized non-null only if block_span resolved the slot-pinned peer record
    // owner-relative through record_targets_of_kb.
    let bytes = block["bytes"].as_u64().expect("block bytes sized") as usize;
    assert!(
        bytes > 0,
        "block_span resolved the cross-repo slot-pinned record: {block:?}"
    );
}

fn serve_on(root: &std::path::Path) -> (tempfile::TempDir, Engine, ServeHandle, Client) {
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    (sock_dir, engine, server, client)
}

fn base_of(node: &serde_json::Value) -> &str {
    node["path"].as_str().unwrap().rsplit('/').next().unwrap()
}

#[test]
fn scope_all_crosses_into_a_dependency() {
    let (_dir, root) = cross_repo();
    let (_sock, _engine, _server, mut client) = serve_on(&root);
    let resp = client
        .query(&json!({ "read": "neighborhood", "path": "app/doc.md" }))
        .unwrap();
    let nodes = resp["result"]["neighborhood"]["nodes"].as_array().unwrap();
    let names: Vec<&str> = nodes.iter().map(base_of).collect();
    // The default `all` includes the dependency's note, repo-tagged.
    assert!(
        names.contains(&"thing.md"),
        "scope all crosses into base: {names:?}"
    );
    let thing = nodes.iter().find(|n| base_of(n) == "thing.md").unwrap();
    assert_eq!(thing["repo"], "base", "the dep node carries its repo");
}

#[test]
fn scope_own_prunes_the_dependency_but_reports_the_crossing_edge() {
    let (_dir, root) = cross_repo();
    let (_sock, _engine, _server, mut client) = serve_on(&root);
    let resp = client
        .query(&json!({ "read": "neighborhood", "path": "app/doc.md", "scope": "own" }))
        .unwrap();
    let n = &resp["result"]["neighborhood"];
    let nodes = n["nodes"].as_array().unwrap();
    let names: Vec<&str> = nodes.iter().map(base_of).collect();
    // The dependency's note is pruned; the own-repo note stays.
    assert!(
        !names.contains(&"thing.md"),
        "scope own prunes the dep node: {names:?}"
    );
    assert!(names.contains(&"local.md"), "the own note stays: {names:?}");
    // The crossing edge is STILL reported, its `to` naming the pruned peer and
    // carrying the peer repo — the boundary is visible.
    let edges = n["edges"].as_array().unwrap();
    let crossing = edges
        .iter()
        .find(|e| {
            e["to"]["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("thing.md"))
        })
        .unwrap_or_else(|| panic!("the crossing edge is reported: {edges:?}"));
    assert_eq!(
        crossing["repo"], "base",
        "the crossing edge names the peer repo"
    );
    // A scope-pruned target is NOT a truncation: it is policy, not budget.
    assert_eq!(n["truncated"], false, "scope pruning is not truncation");
    assert!(
        n["dropped"].as_array().unwrap().is_empty(),
        "a scope-pruned peer is not in dropped: {}",
        n["dropped"]
    );
}

#[test]
fn enrichment_splices_content_body_and_instance() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "neighborhood", "path": "a.md",
            "content": true, "body": true, "instance": true
        }))
        .unwrap();
    let seed = resp["result"]["neighborhood"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["path"].as_str().unwrap().ends_with("a.md"))
        .unwrap();
    // content is the whole-file read; its text length equals the node `bytes`.
    let text = seed["content"]["text"].as_str().unwrap();
    assert_eq!(
        text.len(),
        seed["bytes"].as_u64().unwrap() as usize,
        "content.text length equals the node's bytes"
    );
    assert!(
        seed["content"]["hash"].is_string(),
        "content carries a hash"
    );
    // body is the prose; its length equals the node `body_bytes`.
    let body = seed["body"].as_str().unwrap();
    assert_eq!(
        body.len(),
        seed["body_bytes"].as_u64().unwrap() as usize,
        "body length equals the node's body_bytes"
    );
    // instance is the resolved view for a typed instance.
    assert_eq!(seed["instance"]["resolved"], true, "a typed note resolves");
}

#[test]
fn instance_is_null_for_a_plain_note() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "plain.md", "instance": true }))
        .unwrap();
    let seed = &resp["result"]["neighborhood"]["nodes"][0];
    assert_eq!(
        seed["file_kind"], "note",
        "a frontmatter-less file is a note"
    );
    // The `instance` KEY is present (the flag was set) but null: a note has no
    // resolved view. The finding behind the deferred `frontmatter` flag.
    assert!(
        seed.as_object().unwrap().contains_key("instance"),
        "the key is present"
    );
    assert!(
        seed["instance"].is_null(),
        "a note's instance is null: {seed}"
    );
}

#[test]
fn the_flags_are_independent() {
    let mut h = started();
    // body only: no content / instance keys appear.
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "body": true }))
        .unwrap();
    let seed = &resp["result"]["neighborhood"]["nodes"][0];
    let obj = seed.as_object().unwrap();
    assert!(obj.contains_key("body"), "body requested");
    assert!(
        !obj.contains_key("content"),
        "content not requested, key absent"
    );
    assert!(
        !obj.contains_key("instance"),
        "instance not requested, key absent"
    );
}

#[test]
fn the_unenriched_walk_is_the_summary_form() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md" }))
        .unwrap();
    let seed = &resp["result"]["neighborhood"]["nodes"][0];
    let obj = seed.as_object().unwrap();
    // No enrichment keys, but the sizes are always present.
    assert!(
        !obj.contains_key("content") && !obj.contains_key("body") && !obj.contains_key("instance")
    );
    assert!(
        obj.contains_key("bytes") && obj.contains_key("body_bytes"),
        "sizes ride the summary form"
    );
}

#[test]
fn a_block_node_is_sized_and_enriched_by_its_span() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "neighborhood", "path": "ref.md",
            "content": true, "body": true, "instance": true
        }))
        .unwrap();
    let block = resp["result"]["neighborhood"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["block_id"] == "blk")
        .cloned()
        .expect("the ^^blk block node");
    // Sizes are the block's SPAN length, no longer null.
    let bytes = block["bytes"].as_u64().expect("block bytes") as usize;
    assert!(bytes > 0, "a resolved block carries a span size");
    assert_eq!(
        block["body_bytes"], block["bytes"],
        "a block's body equals its span"
    );
    // content / body are the block's slice; the size a consumer costed matches.
    let text = block["content"]["text"]
        .as_str()
        .expect("block content text");
    assert_eq!(
        text.len(),
        bytes,
        "content slice length equals the block bytes"
    );
    assert!(
        text.contains("[:ref]"),
        "the slice is the block's fence: {text:?}"
    );
    assert_eq!(
        block["body"], block["content"]["text"],
        "block body equals its content"
    );
    // A block is not a file, so its content carries no hash / commit.
    assert!(block["content"]["hash"].is_null(), "a block has no hash");
    assert!(
        block["content"]["commit"].is_null(),
        "a block has no commit"
    );
    // A block has no standalone resolved instance.
    assert!(
        block["instance"].is_null(),
        "a block node's instance is null"
    );
}

#[test]
fn a_bare_anchor_file_node_is_unaffected_by_block_sizing() {
    let mut h = started();
    // ref.md also has [[hub^blk]] (bare ^), reaching the hub FILE node. It sizes
    // as a whole file, distinct from the block node's span sizing.
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "ref.md", "content": true }))
        .unwrap();
    let nodes = resp["result"]["neighborhood"]["nodes"].as_array().unwrap();
    let file_node = nodes
        .iter()
        .find(|n| n["path"].as_str().unwrap().ends_with("hub.md") && n["block_id"].is_null())
        .expect("the hub file node");
    let block_node = nodes.iter().find(|n| n["block_id"] == "blk").unwrap();
    // The file node's content is the WHOLE file (with hash), larger than the block slice.
    assert!(
        file_node["content"]["hash"].is_string(),
        "the file node has a hash"
    );
    assert!(
        file_node["bytes"].as_u64().unwrap() > block_node["bytes"].as_u64().unwrap(),
        "the whole file is larger than the block span"
    );
}

#[test]
fn max_nodes_zero_is_rejected() {
    let mut h = started();
    // The seed always counts, so the floor is 1; 0 would return the seed and set
    // truncated, a contradiction. Rejected as an error frame.
    let resp = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "max_nodes": 0 }))
        .unwrap();
    assert_eq!(resp["type"], "error", "max_nodes 0 is rejected: {resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("max_nodes"),
        "{resp}"
    );
    // max_nodes 1 is the floor: the seed alone.
    let ok = h
        .client
        .query(&json!({ "read": "neighborhood", "path": "a.md", "max_nodes": 1 }))
        .unwrap();
    assert_eq!(
        ok["result"]["neighborhood"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
