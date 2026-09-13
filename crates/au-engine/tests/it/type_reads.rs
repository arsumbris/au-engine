//! The type-system reads over the socket: the type graph, one type by name,
//! instances of a type by closure membership, and the resolved value layer.
//! These back `TypeIndexPort`, `BodyTemplatePort`, and `ProvenancePort`.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A small hierarchy: `task` inherits `thing` and declares a body. Two
/// instances, one of each type, so closure membership is observable.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/thing.type.yaml"),
        "fields:\n  title: String\n",
    )
    .unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "extends: thing\nfields:\n  priority: [low, high]\nbody:\n  - section: Why\n",
    )
    .unwrap();
    fs::write(
        root.join("a.md"),
        "---\ntype: task\ntitle: A\npriority: low\n---\n# Why\nbecause\n",
    )
    .unwrap();
    fs::write(root.join("b.md"), "---\ntype: thing\ntitle: B\n---\n").unwrap();
    (dir, root)
}

/// The guards a test must hold alive: the tempdirs, the engine (its handle
/// backs the server), and the server (dropping it removes the socket).
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
fn types_read_lists_the_graph_with_parents_and_fields() {
    let mut h = started();
    let client = &mut h.client;
    let resp = client.query(&json!({ "read": "types" })).unwrap();
    assert_eq!(resp["ready"], true);
    let types = resp["result"]["types"].as_array().unwrap();

    let task = types.iter().find(|t| t["name"] == "task").unwrap();
    assert_eq!(task["parents"], json!(["thing"]), "task inherits thing");
    let field_names: Vec<&str> = task["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(field_names.contains(&"priority"), "task declares priority");

    let thing = types.iter().find(|t| t["name"] == "thing").unwrap();
    assert!(thing["parents"].as_array().unwrap().is_empty());
}

#[test]
fn type_by_name_carries_body_template_and_resolves_absent_to_null() {
    let mut h = started();
    let client = &mut h.client;

    let resp = client
        .query(&json!({ "read": "type", "name": "task" }))
        .unwrap();
    let task = &resp["result"]["type"];
    assert_eq!(task["name"], "task");
    assert_eq!(task["parents"], json!(["thing"]));
    // Source-form body: one `section: Why`. effective_body is the post-splice
    // form (no `use:` here, so it mirrors the source body).
    assert_eq!(task["body"][0]["kind"], "section");
    assert_eq!(task["body"][0]["name"], "Why");
    assert_eq!(task["effective_body"][0]["name"], "Why");

    let resp = client
        .query(&json!({ "read": "type", "name": "nope" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    assert!(
        resp["result"]["type"].is_null(),
        "absent type resolves to null, got {:?}",
        resp["result"]["type"]
    );
}

#[test]
fn instances_of_filters_by_closure_membership_with_field_values() {
    let mut h = started();
    let client = &mut h.client;

    // `thing` is in both closures: a.md (task → thing) and b.md (thing).
    let resp = client
        .query(&json!({ "read": "instances_of", "type": "thing" }))
        .unwrap();
    let rows = resp["result"]["instances_of"].as_array().unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert_eq!(
        rows.len(),
        2,
        "both instances are in thing's closure: {paths:?}"
    );

    let a = rows
        .iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("a.md"))
        .unwrap();
    assert_eq!(
        a["fields"]["title"], "A",
        "frontmatter field value surfaces"
    );
    assert_eq!(a["fields"]["priority"], "low");
    assert!(
        a["fields"].get("type").is_none(),
        "the type claim is not a field value"
    );

    // `task` is in only a.md's closure.
    let resp = client
        .query(&json!({ "read": "instances_of", "type": "task" }))
        .unwrap();
    let rows = resp["result"]["instances_of"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["path"].as_str().unwrap().ends_with("a.md"));
}

/// `member` is always present, and the `instance` / `body` booleans splice the
/// adjacent per-match facts server-side, each keyed by the match's file.
#[test]
fn instances_of_carries_member_and_the_enrichment_facts() {
    let mut h = started();
    let client = &mut h.client;

    // Baseline: no flags, so no `instance` / `body` keys, and `member` present.
    // This is a single-repo knowledge base, so the member is its root repo.
    let resp = client
        .query(&json!({ "read": "instances_of", "type": "task" }))
        .unwrap();
    let row = &resp["result"]["instances_of"][0];
    assert!(row["member"].is_string(), "member is always present: {row}");
    assert!(
        row.get("instance").is_none() && row.get("body").is_none(),
        "no flags → byte-identical base record: {row}"
    );

    // instance + body. `instance` is the `instance` read's payload; `body` is the
    // prose after the frontmatter.
    let resp = client
        .query(&json!({
            "read": "instances_of", "type": "task",
            "instance": true, "body": true
        }))
        .unwrap();
    let row = &resp["result"]["instances_of"][0];

    // `instance` is the `instance` read's payload (the resolved view).
    assert_eq!(
        row["instance"]["resolved"], true,
        "the instance fact carries the resolved view: {row}"
    );
    assert_eq!(row["instance"]["claim"], json!(["task"]));

    // `body` is the prose after the frontmatter — NOT the frontmatter itself.
    let body = row["body"].as_str().expect("body is a string");
    assert!(
        body.contains("# Why") && body.contains("because"),
        "the body fact carries the prose: {body:?}"
    );
    assert!(
        !body.contains("title: A"),
        "the body fact excludes the frontmatter: {body:?}"
    );

    // Cross-check: the `instance` fact equals the standalone read byte-for-byte,
    // and the `body` fact is the body half of the standalone `content` read.
    let standalone_instance = client
        .query(&json!({ "read": "instance", "path": "a.md" }))
        .unwrap();
    assert_eq!(
        row["instance"], standalone_instance["result"]["instance"],
        "the instance fact equals the standalone instance read"
    );
    let standalone_content = client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    let whole = standalone_content["result"]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        whole.ends_with(body) && whole.contains("title: A"),
        "the body is the tail of content.text, past the frontmatter: {whole:?}"
    );
}

/// Each fact is opt-in independently: `body` without `instance` splices only the
/// body, and vice versa.
#[test]
fn instances_of_facts_are_independently_opt_in() {
    let mut h = started();
    let client = &mut h.client;

    let resp = client
        .query(&json!({ "read": "instances_of", "type": "task", "body": true }))
        .unwrap();
    let row = &resp["result"]["instances_of"][0];
    assert!(
        row.get("instance").is_none(),
        "body alone: no instance: {row}"
    );
    assert!(row["body"].is_string(), "body alone: body present: {row}");

    let resp = client
        .query(&json!({ "read": "instances_of", "type": "task", "instance": true }))
        .unwrap();
    let row = &resp["result"]["instances_of"][0];
    assert!(row.get("body").is_none(), "instance alone: no body: {row}");
    assert_eq!(row["instance"]["resolved"], true, "instance alone: {row}");
}

/// A typo'd arg is an `error` frame, not a silently-ignored key that answers
/// with the flag absent. `instances_of` carries a dedicated strict arg-struct
/// for this, the pattern `overview` / `hubs` / `type_closure` use.
#[test]
fn instances_of_rejects_an_unknown_arg() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "instances_of", "type": "task", "bdy": true }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a mistyped `body` must not silently answer with the fact absent: {resp}"
    );
}

#[test]
fn resolved_read_carries_the_full_value_layer() {
    let mut h = started();
    let client = &mut h.client;
    let resp = client
        .query(&json!({ "read": "instance", "path": "a.md" }))
        .unwrap();
    let result = &resp["result"]["instance"];
    assert_eq!(result["resolved"], true);
    assert_eq!(result["claim"][0], "task");

    // Effective values carry every frontmatter field with provenance.
    let values = result["effective_values"].as_array().unwrap();
    let field = |name: &str| -> &Value {
        values
            .iter()
            .find(|e| e["field"] == name)
            .unwrap_or_else(|| panic!("effective_values has {name}, got {values:?}"))
    };
    assert_eq!(field("title")["containers"][0]["value"]["kind"], "scalar");
    assert_eq!(field("priority")["containers"][0]["value"]["value"], "low");

    // The declared `Why` section is present in a.md's body.
    let presence = result["section_presence"].as_array().unwrap();
    let why = presence.iter().find(|s| s["name"] == "Why").unwrap();
    assert_eq!(why["present"], true);

    // Body events are emitted for the markdown instance.
    assert!(result["body_events"].is_array(), "markdown body has events");
}
