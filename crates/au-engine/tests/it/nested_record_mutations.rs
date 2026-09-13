//! The nested-record mutations over the socket: edit_record and append_record
//! through the one mediated path, byte-splices that preserve comments, CAS and
//! on_invalid honored, synchronous rebuild, fresh-state responses.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::json;

/// A plan knowledge base: `plan` holds `phases`, a `phase` holds `actions`, and `action`
/// is a sealed family whose `action.done` leaf REQUIRES `outputs`. The required
/// field gives `on_invalid: reject` a real error to trip.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/plan.type.yaml"),
        "fields:\n  phases: phase[]\n",
    )
    .unwrap();
    fs::write(
        root.join("type/phase.type.yaml"),
        "fields:\n  title?: String\n  actions?: action[]\n",
    )
    .unwrap();
    fs::write(
        root.join("type/action.type.yaml"),
        "sealed:\n  - action.open\n  - action.done\nfields:\n  description: String\n",
    )
    .unwrap();
    fs::write(root.join("type/action.open.type.yaml"), "extends: action\n").unwrap();
    fs::write(
        root.join("type/action.done.type.yaml"),
        "extends: action\nfields:\n  outputs: String[+]\n",
    )
    .unwrap();
    // A plan with one phase and one open action. The frontmatter carries a `#`
    // comment and a `#:` doc comment, the bytes a splice must preserve.
    fs::write(
        root.join("plan.md"),
        "---\ntype: plan\nphases:\n  # the first phase\n  - type: phase\n    title: first\n    actions:\n      - type: action.open\n        description: do the thing  #: the task\n---\n",
    )
    .unwrap();
    (dir, root)
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: au_engine::ServeHandle,
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

fn content(h: &mut Harness) -> String {
    h.client
        .query(&json!({ "read": "content", "path": "plan.md" }))
        .unwrap()["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn edit_record_action_done_retypes_and_inserts_outputs_preserving_comments() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions", 0],
            "patch": { "type": "action.done", "outputs": ["shipped it"] },
            "id": "m-1",
        }))
        .unwrap();
    assert_eq!(resp["type"], "response", "{resp}");
    assert_eq!(resp["id"], "m-1");
    assert_eq!(
        resp["result"]["diagnostics"].as_array().unwrap().len(),
        0,
        "action.done with outputs validates clean: {resp}"
    );
    let c = content(&mut h);

    assert!(c.contains("      - type: action.done\n"), "{c}");
    assert!(
        c.contains("        outputs:\n          - shipped it\n"),
        "{c}"
    );
    // The doc comment and the `#` comment survive the splice byte-identical.
    assert!(
        c.contains("description: do the thing  #: the task\n"),
        "{c}"
    );
    assert!(c.contains("  # the first phase\n"), "{c}");
}

#[test]
fn append_record_adds_an_action_preserving_the_existing_one() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "append_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions"],
            "value": { "type": "action.open", "description": "another thing" },
        }))
        .unwrap();
    assert_eq!(resp["type"], "response", "{resp}");

    let c = content(&mut h);
    // The new action is appended, the existing action and its doc comment stay.
    assert!(
        c.contains("description: do the thing  #: the task\n"),
        "{c}"
    );
    assert!(
        c.contains("      - type: action.open\n        description: another thing\n"),
        "{c}"
    );
    // The append lands INSIDE the frontmatter, before the closing `---`, not
    // after it (the span-over-extension bug).
    assert!(
        c.find("another thing").unwrap() < c.rfind("\n---").unwrap(),
        "the new element is inside the frontmatter: {c}"
    );
    // Exactly one diagnostic-free instance: no stray element split it.
    assert_eq!(
        resp["result"]["diagnostics"].as_array().unwrap().len(),
        0,
        "{resp}"
    );
}

#[test]
fn append_record_seeds_an_empty_actions_list() {
    let mut h = started();
    // Empty the actions first by writing a fresh plan with `actions: []`.
    h.client
        .query(&json!({
            "mutate": "write_file",
            "path": "plan.md",
            "content": "---\ntype: plan\nphases:\n  - type: phase\n    actions: []\n---\n",
        }))
        .unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "append_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions"],
            "value": { "type": "action.open", "description": "first" },
        }))
        .unwrap();
    assert_eq!(resp["type"], "response", "{resp}");
    let c = content(&mut h);
    assert!(
        c.contains("    actions:\n      - type: action.open\n        description: first\n"),
        "{c}"
    );
    assert!(!c.contains("[]"), "the empty marker is gone: {c}");
}

#[test]
fn expected_hash_guards_a_stale_edit() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions", 0],
            "patch": { "description": "changed" },
            "expected_hash": "0000000000000000",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "{resp}");
    assert!(resp["detail"]["current_hash"].is_string(), "{resp}");
    // Nothing was written.
    assert!(
        content(&mut h).contains("description: do the thing"),
        "unchanged"
    );
}

#[test]
fn on_invalid_reject_refuses_an_edit_that_adds_an_error() {
    let mut h = started();
    // Re-typing to action.done WITHOUT outputs makes the required `outputs`
    // field absent — a new error. `reject` refuses it.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions", 0],
            "patch": { "type": "action.done" },
            "on_invalid": "reject",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("required-field-absent"),
        "the reject names the new error: {resp}"
    );
    // Nothing was written: still the open action.
    assert!(content(&mut h).contains("type: action.open"), "unchanged");
}

#[test]
fn on_invalid_advise_lands_the_edit_and_surfaces_the_diagnostic() {
    let mut h = started();
    // The same edit under the default `advise` lands and surfaces the error.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_record",
            "path": "plan.md",
            "field_path": ["phases", 0, "actions", 0],
            "patch": { "type": "action.done" },
        }))
        .unwrap();
    assert_eq!(resp["type"], "response", "{resp}");
    let codes: Vec<&str> = resp["result"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"required-field-absent"), "{resp}");
    assert!(
        content(&mut h).contains("type: action.done"),
        "the edit landed"
    );
}

#[test]
fn a_stale_field_path_rejects() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_record",
            "path": "plan.md",
            "field_path": ["phases", 9],
            "patch": { "title": "x" },
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "{resp}");
}
