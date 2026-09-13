//! The workspace-wide type reads over the socket: `types` and `type` with no
//! `repo` span every member deduped to owner copies, and `type_tree` serves the
//! cross-repo parent/child adjacency forest. These back a consumer's
//! "what type-defs exist across the mounted workspace" view.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// Two repos under one tree. `base` owns `thing`. `app` imports `base` and owns
/// `task` (extends the peer `thing::base`), `tag`, and `audited-task` (a mixin
/// extending both `task` and `tag`, the multi-parent case). The root carries no
/// type-defs of its own.
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
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("base/type/thing.type.yaml", "fields:\n  title: String\n");
    w(
        "app/type/task.type.yaml",
        "extends: thing::base\nfields:\n  done: Boolean\n",
    );
    w("app/type/tag.type.yaml", "fields:\n  label: String\n");
    w(
        "app/type/audited-task.type.yaml",
        "extends: [task, tag]\nfields:\n  at: String\n",
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

fn names(result: &serde_json::Value) -> Vec<String> {
    let mut v: Vec<String> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn owner(result: &serde_json::Value, name: &str) -> String {
    result
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("type {name} present"))["repo"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Names in the array's served order (name-sorted), NOT re-sorted, so paging
/// order is observable.
fn ordered_names(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn find<'a>(result: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    result
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("type {name} present"))
}

#[test]
fn workspace_types_span_members_deduped_to_owner_copies() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "types" })).unwrap();
    assert_eq!(resp["ready"], true);
    let result = &resp["result"]["types"];

    // Every owner type across base and app; `thing` is owned only by base and
    // appears once (app imports it rather than copying it).
    // The au.engine.* builtin schema types (and, in a real workspace, every mounted dependency) are surfaced here too; filtered to the knowledge base's own types. On-demand hiding is tracked in the todo.
    let mut own = names(result);
    own.retain(|n| !n.starts_with("au.engine."));
    assert_eq!(
        own,
        vec!["audited-task", "tag", "task", "thing"],
        "workspace-wide owner set"
    );

    // Each entry carries its owner repo.
    assert_eq!(owner(result, "thing"), "base", "thing owned by base");
    assert_eq!(owner(result, "task"), "app", "task owned by app");
    assert_eq!(owner(result, "tag"), "app", "tag owned by app");

    // The def payload rides beside `repo`: task's fields and parents are there.
    let task = result
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "task")
        .unwrap();
    assert_eq!(
        task["parents"],
        json!(["thing::base"]),
        "task extends the peer thing::base"
    );
    assert_eq!(task["fields"][0]["name"], "done");
}

#[test]
fn types_summary_projects_a_lightweight_entry_keeping_identity() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "types", "summary": true }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let result = &resp["result"]["types"];

    // Same owner set and name-sorted order as the full read.
    let mut own = ordered_names(result);
    own.retain(|n| !n.starts_with("au.engine."));
    assert_eq!(
        own,
        vec!["audited-task", "tag", "task", "thing"],
        "summary spans the same owner set"
    );

    let task = find(result, "task");
    // Identity + placement survive.
    assert_eq!(task["repo"], "app");
    assert_eq!(task["parents"], json!(["thing::base"]), "parents kept");
    assert!(
        !task["hash"].as_str().unwrap().is_empty(),
        "identity hash kept"
    );
    // `sealed` is always present (null here, task is not sealed), matching the
    // full view, so a consumer can rely on the key.
    assert!(task.get("sealed").is_some(), "sealed key present");
    assert!(task["sealed"].is_null(), "task is not sealed");

    // The heavy detail is dropped — this is the wire-payload win.
    for dropped in ["fields", "body", "effective_body", "meta_blocks", "source"] {
        assert!(
            task.get(dropped).is_none(),
            "summary drops {dropped}, got {task}"
        );
    }
}

#[test]
fn types_pages_the_name_sorted_set_by_limit_and_offset() {
    let mut h = started();

    // The au.engine.* builtin schema types (and, in a real workspace, every mounted dependency) are surfaced here too; they sort ahead of the knowledge base's own names, so page past them to exercise paging over the knowledge base's OWN name-sorted set. On-demand hiding is tracked in the todo.
    let full = h.client.query(&json!({ "read": "types" })).unwrap();
    let base = ordered_names(&full["result"]["types"])
        .iter()
        .position(|n| !n.starts_with("au.engine."))
        .unwrap();

    // First page of two, name-sorted.
    let p1 = h
        .client
        .query(&json!({ "read": "types", "limit": 2, "offset": base }))
        .unwrap();
    assert_eq!(
        ordered_names(&p1["result"]["types"]),
        vec!["audited-task", "tag"]
    );
    // Full detail is intact on a paged entry.
    assert!(
        find(&p1["result"]["types"], "tag").get("fields").is_some(),
        "paged full entries keep their detail"
    );

    // The next page.
    let p2 = h
        .client
        .query(&json!({ "read": "types", "limit": 2, "offset": base + 2 }))
        .unwrap();
    assert_eq!(ordered_names(&p2["result"]["types"]), vec!["task", "thing"]);

    // Past the end is an empty page, the done signal (a consumer pages until a
    // short page).
    let p3 = h
        .client
        .query(&json!({ "read": "types", "limit": 2, "offset": base + 4 }))
        .unwrap();
    assert_eq!(p3["result"]["types"].as_array().unwrap().len(), 0);

    // Paging composes with summary.
    let s = h
        .client
        .query(&json!({ "read": "types", "summary": true, "limit": 1, "offset": base + 1 }))
        .unwrap();
    assert_eq!(ordered_names(&s["result"]["types"]), vec!["tag"]);
    assert!(
        s["result"]["types"][0].get("fields").is_none(),
        "summary + paging stays lightweight"
    );
}

#[test]
fn types_rejects_an_unknown_arg_rather_than_silently_ignoring_it() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "types", "bogus": true }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an unknown arg is a loud error, never a silently-unfiltered set"
    );
}

#[test]
fn type_counts_totals_reconcile_with_the_types_read() {
    let mut h = started();

    // Workspace: the total matches the full read's length, by_repo histograms
    // the owner set (base owns thing; app owns task/tag/audited-task).
    let full = h.client.query(&json!({ "read": "types" })).unwrap();
    let counts = h.client.query(&json!({ "read": "type_counts" })).unwrap();
    let full_len = full["result"]["types"].as_array().unwrap().len();
    assert_eq!(
        counts["result"]["type_counts"]["total"].as_u64().unwrap() as usize,
        full_len,
        "total reconciles with the full types read"
    );
    // The au-engine builtin repo (and, in a real workspace, every mounted dependency) contributes its own by_repo entry here too; assert the knowledge base's own repos. On-demand hiding is tracked in the todo.
    assert_eq!(counts["result"]["type_counts"]["by_repo"]["app"], json!(3));
    assert_eq!(counts["result"]["type_counts"]["by_repo"]["base"], json!(1));

    // Repo-scoped: the total matches the scoped read (the member's own defs),
    // and by_repo is just the scoped holder.
    let full_app = h
        .client
        .query(&json!({ "read": "types", "repo": "app" }))
        .unwrap();
    let app_len = full_app["result"]["types"].as_array().unwrap().len() as u64;
    let counts_app = h
        .client
        .query(&json!({ "read": "type_counts", "repo": "app" }))
        .unwrap();
    assert_eq!(
        counts_app["result"]["type_counts"]["total"]
            .as_u64()
            .unwrap(),
        app_len
    );
    assert_eq!(
        counts_app["result"]["type_counts"]["by_repo"]["app"]
            .as_u64()
            .unwrap(),
        app_len
    );
    assert_eq!(
        counts_app["result"]["type_counts"]["by_repo"]
            .as_object()
            .unwrap()
            .len(),
        1,
        "a scoped count histograms only the scoped repo"
    );

    // An unknown repo is null, matching the `types` read.
    let nope = h
        .client
        .query(&json!({ "read": "type_counts", "repo": "ghost" }))
        .unwrap();
    assert_eq!(nope["ready"], true);
    assert!(
        nope["result"]["type_counts"].is_null(),
        "unknown repo → null"
    );

    // A meaningless projection/paging arg is rejected loudly.
    let bad = h
        .client
        .query(&json!({ "read": "type_counts", "summary": true }))
        .unwrap();
    assert_eq!(
        bad["type"], "error",
        "counts reject summary/limit/offset as unknown args"
    );
}

/// Two INDEPENDENT repos, each owning a same-named but DIVERGENT `widget`. Legal
/// (name uniqueness is per-repo, cross-repo names coexist), so the bare `types`
/// read conflates them by name into two entries. The identity `hash` is what
/// tells them apart inline, with no `type_sites` fan-out.
fn same_name_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["repo_a", "repo_b"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("repo_a/.arsumbris/repo.yaml", "name: repo_a\n");
    w("repo_a/type/widget.type.yaml", "fields:\n  a: String\n");
    w("repo_b/.arsumbris/repo.yaml", "name: repo_b\n");
    // Divergent: a different field, so a different closure, so a different hash.
    w("repo_b/type/widget.type.yaml", "fields:\n  b: Number\n");
    (dir, root)
}

#[test]
fn types_carries_a_hash_that_distinguishes_same_named_cross_repo_identities() {
    let (dir, root) = same_name_fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");
    let _keep = (dir, sock_dir, engine, server);

    let resp = client.query(&json!({ "read": "types" })).unwrap();
    let widgets: Vec<&serde_json::Value> = resp["result"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["name"] == "widget")
        .collect();

    // Both owners surface (conflated by name), each with its repo and a hash.
    assert_eq!(
        widgets.len(),
        2,
        "both repos' widget surface, conflated by name"
    );
    let repos: std::collections::BTreeSet<&str> = widgets
        .iter()
        .map(|w| w["repo"].as_str().unwrap())
        .collect();
    assert_eq!(
        repos,
        ["repo_a", "repo_b"].into_iter().collect(),
        "one widget per repo"
    );
    let hashes: std::collections::BTreeSet<&str> = widgets
        .iter()
        .map(|w| w["hash"].as_str().unwrap())
        .collect();
    assert!(
        hashes.iter().all(|h| !h.is_empty()),
        "every widget carries a non-empty identity hash"
    );
    assert_eq!(
        hashes.len(),
        2,
        "the divergent same-named identities carry DISTINCT hashes, so a conflated result is disambiguable inline"
    );
}

#[test]
fn workspace_type_resolves_a_member_defined_name_to_its_owner() {
    let mut h = started();

    // No repo arg: `task` resolves to its owner in app, no repo hint needed.
    let task = h
        .client
        .query(&json!({ "read": "type", "name": "task" }))
        .unwrap();
    assert_eq!(task["result"]["type"]["name"], "task");
    assert_eq!(task["result"]["type"]["repo"], "app");
    assert_eq!(task["result"]["type"]["parents"], json!(["thing::base"]));

    // `thing` resolves to base, its sole owner.
    let thing = h
        .client
        .query(&json!({ "read": "type", "name": "thing" }))
        .unwrap();
    assert_eq!(
        thing["result"]["type"]["repo"], "base",
        "thing is owned by base"
    );

    // An unknown name is null.
    let nope = h
        .client
        .query(&json!({ "read": "type", "name": "nope" }))
        .unwrap();
    assert_eq!(nope["ready"], true);
    assert!(nope["result"]["type"].is_null());
}

#[test]
fn type_batch_names_resolves_order_matched_with_nulls() {
    let mut h = started();

    // A batch of names returns an array, one slot per requested name, in order,
    // null where a name does not resolve.
    let resp = h
        .client
        .query(&json!({ "read": "type_batch", "names": ["task", "nope", "tag"] }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let arr = resp["result"]["type_batch"].as_array().unwrap();
    assert_eq!(arr.len(), 3, "order-matched, one slot per requested name");
    assert_eq!(arr[0]["name"], "task");
    assert_eq!(arr[0]["repo"], "app");
    assert!(!arr[0]["hash"].as_str().unwrap().is_empty(), "hash present");
    assert!(arr[0]["fields"].is_array(), "each hit is the full def");
    assert!(arr[1].is_null(), "an unresolved name is null in place");
    assert_eq!(arr[2]["name"], "tag");

    // A `::repo`-qualified name resolves per identity inside the batch.
    let q = h
        .client
        .query(&json!({ "read": "type_batch", "names": ["thing::base"] }))
        .unwrap();
    assert_eq!(q["result"]["type_batch"][0]["repo"], "base");
    assert_eq!(q["result"]["type_batch"][0]["name"], "thing");

    // The single form is unchanged: one object, not an array.
    let single = h
        .client
        .query(&json!({ "read": "type", "name": "task" }))
        .unwrap();
    assert_eq!(single["result"]["type"]["name"], "task");
    assert!(
        single["result"]["type"].is_object(),
        "single form stays a lone object"
    );

    // Neither `name` nor `names` is a loud error, not a silent null — a type
    // read must name a target.
    let neither = h.client.query(&json!({ "read": "type" })).unwrap();
    assert_eq!(neither["type"], "error", "a type read must name a target");

    // Both selectors at once is rejected, not silently resolved as one — WIRE
    // says exactly one.
    let both = h
        .client
        .query(&json!({ "read": "type_batch", "name": "task", "names": ["tag"] }))
        .unwrap();
    assert_eq!(
        both["type"], "error",
        "exactly one of name / names, not both"
    );

    // An unknown arg is rejected, matching the sibling reads' strictness (not
    // silently ignored, which would degrade a typo'd `names` to a single).
    let bogus = h
        .client
        .query(&json!({ "read": "type", "name": "task", "bogus": 1 }))
        .unwrap();
    assert_eq!(
        bogus["type"], "error",
        "an unknown arg on `type` is rejected"
    );
}

#[test]
fn type_tree_is_a_cross_repo_adjacency_forest_handling_multi_parent() {
    let mut h = started();
    let resp = h.client.query(&json!({ "read": "type_tree" })).unwrap();
    assert_eq!(resp["ready"], true);
    let tree = &resp["result"]["type_tree"];

    // Roots are the parentless types, name-sorted: `tag` and `thing`.
    // The au.engine.* builtin schema types (and, in a real workspace, every mounted dependency) are surfaced here too as their own parentless roots; filtered to the knowledge base's own types. On-demand hiding is tracked in the todo.
    let own_roots: Vec<String> = tree["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .filter(|n| !n.starts_with("au.engine."))
        .collect();
    assert_eq!(
        own_roots,
        vec!["tag", "thing"],
        "roots are the in-set-parentless nodes"
    );

    let node = |name: &str| -> &serde_json::Value {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == name)
            .unwrap_or_else(|| panic!("node {name}"))
    };

    // The cross-repo edge: base's `thing` parents app's `task`.
    assert_eq!(node("thing")["repo"], "base");
    assert_eq!(node("thing")["children"], json!(["task"]));
    assert_eq!(node("task")["repo"], "app");

    // The multi-parent node appears ONCE with both parents, referenced by each
    // parent's children.
    let audited = node("audited-task");
    assert_eq!(
        audited["parents"],
        json!(["task", "tag"]),
        "both declared parents, verbatim"
    );
    assert_eq!(audited["children"], json!([]));
    assert_eq!(node("task")["children"], json!(["audited-task"]));
    assert_eq!(node("tag")["children"], json!(["audited-task"]));

    // Exactly four of the knowledge base's own nodes: no duplication of the multi-parent
    // node. The au.engine.* builtin schema nodes are surfaced alongside them.
    let own_nodes = tree["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| !n["name"].as_str().unwrap().starts_with("au.engine."))
        .count();
    assert_eq!(own_nodes, 4);
}

/// The IMPORT case (no vendored copy): `app` extends `base`'s `thing` via
/// `type: thing::base` with no local copy of `thing`. The served parent carries
/// the `::repo` verbatim, and the cross-repo child edge still forms because the
/// tree matches on the base name.
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
    w("base/type/thing.type.yaml", "fields:\n  title: String\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // Peer-extended, no local `thing` copy: `type: thing::base`.
    w(
        "app/type/leaf.type.yaml",
        "extends: thing::base\nfields:\n  extra: String\n",
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
fn type_tree_keeps_the_cross_repo_edge_for_a_peer_extended_leaf() {
    let mut h = started_import();
    let resp = h.client.query(&json!({ "read": "type_tree" })).unwrap();
    assert_eq!(resp["ready"], true);
    let tree = &resp["result"]["type_tree"];

    let node = |name: &str| -> &serde_json::Value {
        tree["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == name)
            .unwrap_or_else(|| panic!("node {name}"))
    };

    // The parent is served verbatim with its `::repo` qualifier.
    assert_eq!(
        node("leaf")["parents"],
        json!(["thing::base"]),
        "the qualified parent reaches the wire unstripped"
    );
    // The cross-repo child edge survives the qualifier: base's `thing` parents
    // app's `leaf`, matched on the base name.
    assert_eq!(node("thing")["repo"], "base");
    assert_eq!(node("thing")["children"], json!(["leaf"]));
    assert_eq!(node("leaf")["repo"], "app");
    // `thing` is the sole root; `leaf` has an in-set parent so it is not a root.
    // The au.engine.* builtin schema types are surfaced here too as their own
    // roots; filtered to the knowledge base's own types. On-demand hiding is tracked in
    // the todo.
    let own_roots: Vec<String> = tree["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .filter(|n| !n.starts_with("au.engine."))
        .collect();
    assert_eq!(own_roots, vec!["thing"]);
}
