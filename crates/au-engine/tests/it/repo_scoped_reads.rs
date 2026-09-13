//! Repo-scoped type reads over the socket: `types`, `type`, and
//! `validate_value` take an optional `repo`, resolving against that repo's
//! graph instead of the root's. An unknown repo is null for the type reads and
//! an unresolved claim for the value read.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// Two repos in one tree, plus an empty root. `base` owns `note { title }`;
/// `app` owns its OWN wider `note { title, archived }` (a distinct same-named
/// identity, not base's) and `task`. The root carries no type-defs of its own;
/// the no-scope read is workspace-wide across base and app.
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
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("base/type/note.type.yaml", "fields:\n  title: String\n");
    w(
        "app/type/note.type.yaml",
        "fields:\n  title: String\n  archived: Boolean\n",
    );
    w("app/type/task.type.yaml", "fields:\n  done: Boolean\n");
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

fn type_names(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn field_names(type_def: &serde_json::Value) -> Vec<String> {
    type_def["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn types_scoped_to_a_repo_returns_that_repos_graph() {
    let mut h = started();
    let c = &mut h.client;

    // base resolves only its own `note`.
    let base = c
        .query(&json!({ "read": "types", "repo": "base" }))
        .unwrap();
    assert_eq!(type_names(&base["result"]["types"]), vec!["note"]);

    // app resolves its own `note` plus its own `task`.
    let app = c.query(&json!({ "read": "types", "repo": "app" })).unwrap();
    let mut app_names = type_names(&app["result"]["types"]);
    app_names.sort();
    assert_eq!(app_names, vec!["note", "task"]);

    // The no-scope read is workspace-wide: every type-def across all members.
    // base and app each own a DISTINCT `note` identity (different shapes), so
    // both coexist here — same name, two identities, not deduped to one owner.
    let ws = c.query(&json!({ "read": "types" })).unwrap();
    let mut ws_entries: Vec<(String, String)> = ws["result"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["repo"].as_str().unwrap().to_string(),
            )
        })
        // The au.engine.* builtin schema types (and, in a real workspace, every mounted dependency) are surfaced here too; filtered to the knowledge base's own types. On-demand hiding is tracked in the todo.
        .filter(|(n, _)| !n.starts_with("au.engine."))
        .collect();
    ws_entries.sort();
    assert_eq!(
        ws_entries,
        vec![
            ("note".to_string(), "app".to_string()),
            ("note".to_string(), "base".to_string()),
            ("task".to_string(), "app".to_string()),
        ],
        "workspace-wide: base's note and app's note coexist as distinct identities"
    );

    // An unknown repo is null, not the workspace graph and not an error.
    let nope = c
        .query(&json!({ "read": "types", "repo": "nope" }))
        .unwrap();
    assert_eq!(nope["ready"], true);
    assert!(nope["result"]["types"].is_null(), "unknown repo → null");
}

#[test]
fn type_by_name_resolves_in_the_named_repo() {
    let mut h = started();
    let c = &mut h.client;

    // The same name resolves to a different shape per repo.
    let base = c
        .query(&json!({ "read": "type", "name": "note::base" }))
        .unwrap();
    assert_eq!(field_names(&base["result"]["type"]), vec!["title"]);

    let app = c
        .query(&json!({ "read": "type", "name": "note::app" }))
        .unwrap();
    let mut app_fields = field_names(&app["result"]["type"]);
    app_fields.sort();
    assert_eq!(app_fields, vec!["archived", "title"]);

    // A type the repo does not own is null in that scope.
    let missing = c
        .query(&json!({ "read": "type", "name": "task::base" }))
        .unwrap();
    assert!(missing["result"]["type"].is_null(), "base has no task");

    // An unknown repo is null.
    let nope = c
        .query(&json!({ "read": "type", "name": "note::nope" }))
        .unwrap();
    assert!(nope["result"]["type"].is_null(), "unknown repo → null");
}

#[test]
fn validate_value_uses_the_named_repos_graph() {
    let mut h = started();
    let c = &mut h.client;

    // Flatten diagnostics across the verdict list. A scoped call has one fit,
    // and an unknown repo has the one null-identity verdict, so the codes are
    // the same set the pre-multi-fit bare array carried.
    let codes = |resp: &serde_json::Value| -> Vec<String> {
        resp["result"]["validate_value"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|v| v["diagnostics"].as_array().unwrap().clone())
            .map(|d| d["code"].as_str().unwrap().to_string())
            .collect()
    };

    // `{ title }` is complete against base's `note`.
    let base = c
        .query(&json!({
            "read": "validate_value", "type_name": "note", "repo": "base",
            "value": { "title": "x" }
        }))
        .unwrap();
    assert!(
        codes(&base).is_empty(),
        "clean against base's note, got {:?}",
        base["result"]
    );

    // The same value is missing `archived` against app's wider `note`.
    let app = c
        .query(&json!({
            "read": "validate_value", "type_name": "note", "repo": "app",
            "value": { "title": "x" }
        }))
        .unwrap();
    assert!(
        codes(&app).contains(&"required-field-absent".to_string()),
        "app's note requires archived, got {:?}",
        app["result"]
    );

    // An unknown repo leaves the type unresolvable: the same verdict an
    // absent type gets.
    let nope = c
        .query(&json!({
            "read": "validate_value", "type_name": "note", "repo": "nope",
            "value": { "title": "x" }
        }))
        .unwrap();
    assert!(
        codes(&nope).contains(&"unknown-type-claim".to_string()),
        "unknown repo → unknown-type-claim, got {:?}",
        nope["result"]
    );
}

/// Guard: in a single-repo knowledge base the sole member is the root, so the
/// workspace-wide no-`repo` read is exactly that one graph.
#[test]
fn single_repo_kb_workspace_read_is_the_sole_member() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir_all(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  title: String\n",
    )
    .unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let resp = client.query(&json!({ "read": "types" })).unwrap();
    let mut names = type_names(&resp["result"]["types"]);
    names.retain(|n| !n.starts_with("au.engine."));
    assert_eq!(names, vec!["note"]);
}
