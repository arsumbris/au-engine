//! A client subscribes over the IPC socket and observes ack, initial value, and
//! change events driven by rebuilds.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name.
    let root = fs::canonicalize(dir.path()).unwrap().join("v");
    fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
    (dir, root)
}

/// A socket outside the knowledge base, so it is never seen as knowledge base content.
fn socket() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s");
    (dir, path)
}

#[test]
fn changes_channel_acks_then_fires_on_every_rebuild() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // ready at version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "changes", "id": "sub-1" }))
        .unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "changes");
    assert_eq!(ack["accepted"], true);
    assert_eq!(ack["schema_version"], 29);
    assert_eq!(ack["id"], "sub-1", "the ack echoes the request id");
    let sub_id = ack["subscription_id"].clone();

    // changes has no initial value; the next frame comes only on a rebuild.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["type"], "change_event");
    assert_eq!(ev["kind"], "knowledge-base-changed");
    assert_eq!(ev["at_version"], 2);
    assert_eq!(ev["subscription_id"], sub_id);
    // The scope hint carries the net file delta: this rebuild modified a.md.
    assert_eq!(ev["scope_hint"]["scope"], "files");
    assert_eq!(ev["scope_hint"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(ev["scope_hint"]["removed"].as_array().unwrap().len(), 0);
    let modified = ev["scope_hint"]["modified"].as_array().unwrap();
    assert_eq!(modified.len(), 1, "one modified file, got {modified:?}");
    assert!(modified[0].as_str().unwrap().ends_with("a.md"));

    // A removal shows up on the removed axis.
    fs::remove_file(root.join("a.md")).unwrap();
    engine.rebuild(); // version 3

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "knowledge-base-changed");
    assert_eq!(ev["at_version"], 3);
    let removed = ev["scope_hint"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "one removed file, got {removed:?}");
    assert!(removed[0].as_str().unwrap().ends_with("a.md"));
}

#[test]
fn type_graph_channel_delivers_initial_then_fires_on_type_def_edit() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "types" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "types");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(initial["at_version"], 1);
    let types = initial["result"].as_array().expect("types is an array");
    assert!(
        types.iter().any(|t| t["name"] == "note"),
        "initial graph carries the note type, got {types:?}"
    );

    // An instance edit advances the version but must NOT fire types.
    // A type-def edit must. Edit the type-def directly.
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n  title?: String\n",
    )
    .unwrap();
    engine.rebuild(); // version 2, type graph changed

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["type"], "change_event");
    assert_eq!(ev["kind"], "types-changed");
    assert_eq!(ev["at_version"], 2);
    // The scope hint names the changed type by its identity handle.
    assert_eq!(ev["scope_hint"]["scope"], "types");
    let changed = ev["scope_hint"]["changed"].as_array().unwrap();
    assert_eq!(changed.len(), 1, "one type changed");
    assert_eq!(changed[0]["name"], "note", "the edited type is named");
    assert!(
        changed[0]["repo"].is_string(),
        "the change entry carries the owner repo"
    );
    assert!(
        changed[0]["hash"].is_string(),
        "the change entry carries the identity hash"
    );
    assert_eq!(ev["scope_hint"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(ev["scope_hint"]["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn type_graph_change_attributes_to_the_right_identity_among_same_named_cross_repo_types() {
    // ra and rb each OWN a `widget`. The change-diff must key on identity, not
    // bare name: editing rb's widget fires an event naming rb's identity, and ra's
    // identically-named widget is not shadowed or mis-attributed (the collision
    // bug would drop or misreport the change).
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
    w("rb/.arsumbris/repo.yaml", "name: rb\n");
    w("rb/type/widget.type.yaml", "fields:\n  b: Number\n");
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "types" })).unwrap();
    let _ack = client.recv().unwrap().unwrap();
    let initial = client.recv().unwrap().unwrap();
    let widget_count = initial["result"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["name"] == "widget")
        .count();
    assert_eq!(widget_count, 2, "both repos' widget in the initial graph");

    // Edit only rb's widget.
    fs::write(
        root.join("rb/type/widget.type.yaml"),
        "fields:\n  b: Number\n  extra?: String\n",
    )
    .unwrap();
    engine.rebuild();

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "types-changed");
    let changed = ev["scope_hint"]["changed"].as_array().unwrap();
    assert_eq!(
        changed.len(),
        1,
        "only rb's widget changed, not ra's same-named one, got {changed:?}"
    );
    assert_eq!(changed[0]["name"], "widget");
    assert_eq!(
        changed[0]["repo"], "rb",
        "the change is attributed to rb's identity, not ra's"
    );
}

#[test]
fn files_channel_delivers_catalog_then_fires_on_new_file() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "files" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "files");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    let files = initial["result"].as_array().expect("files is an array");
    assert!(
        files
            .iter()
            .any(|f| f["path"].as_str().unwrap().ends_with("a.md")),
        "catalog lists a.md, got {files:?}"
    );

    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();
    engine.rebuild();

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "files-changed");
    assert_eq!(ev["at_version"], 2);
    // The scope hint carries the path-set delta.
    assert_eq!(ev["scope_hint"]["scope"], "files");
    let added = ev["scope_hint"]["added"].as_array().unwrap();
    assert_eq!(added.len(), 1, "one added path, got {added:?}");
    assert!(added[0].as_str().unwrap().ends_with("b.md"));
    assert_eq!(ev["scope_hint"]["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn lifecycle_channel_delivers_state_immediately_when_ready() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // already Ready before the subscribe
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "lifecycle" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "lifecycle");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(initial["at_version"], 1);
    assert_eq!(initial["result"]["lifecycle"]["ref"], "ready");
    assert_eq!(initial["result"]["lifecycle"]["engine"], "up");
}

#[test]
fn lifecycle_channel_fires_on_the_deriving_to_ready_transition() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    // No rebuild yet: the engine is Deriving when the subscribe lands.
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "lifecycle" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "lifecycle");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(initial["result"]["lifecycle"]["ref"], "deriving");

    // The first build crosses Deriving -> Ready.
    engine.rebuild();

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["type"], "change_event");
    assert_eq!(ev["kind"], "lifecycle-changed");
    assert_eq!(ev["at_version"], 1);
}

#[test]
fn diagnostics_channel_delivers_the_file_set_and_fires_when_it_changes() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // clean knowledge base at version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "diagnostics", "path": "a.md" }))
        .unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "diagnostics");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(initial["at_version"], 1);
    assert_eq!(
        initial["result"].as_array().unwrap().len(),
        0,
        "clean file has no diagnostics, got {:?}",
        initial["result"]
    );

    // Introduce a dangling reference in a.md: its diagnostic set gains an entry.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["type"], "change_event");
    assert_eq!(ev["kind"], "diagnostics-changed");
    assert_eq!(ev["at_version"], 2);
    // The scope hint names the file precisely, not the whole knowledge base.
    assert_eq!(ev["scope_hint"]["scope"], "files");
    let files = ev["scope_hint"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "one changed file, got {files:?}");
    assert!(files[0].as_str().unwrap().ends_with("a.md"));

    // Clear it again: the set changes back, so the channel fires once more.
    fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
    engine.rebuild(); // version 3

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "diagnostics-changed");
    assert_eq!(ev["at_version"], 3);
    assert!(ev["scope_hint"]["files"][0]
        .as_str()
        .unwrap()
        .ends_with("a.md"));
}

#[test]
fn whole_kb_diagnostics_channel_delivers_all_and_names_changed_files() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // clean knowledge base at version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    // No path: the whole-knowledge-base diagnostics channel.
    client.send(&json!({ "subscribe": "diagnostics" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "diagnostics");

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(initial["at_version"], 1);
    assert_eq!(
        initial["result"].as_array().unwrap().len(),
        0,
        "clean knowledge base has no diagnostics"
    );

    // A dangling reference in a.md: the whole-knowledge-base set changes, and the hint
    // names a.md as the changed file.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "diagnostics-changed");
    assert_eq!(ev["at_version"], 2);
    assert_eq!(ev["scope_hint"]["scope"], "files");
    let files = ev["scope_hint"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "only a.md changed, got {files:?}");
    assert!(files[0].as_str().unwrap().ends_with("a.md"));
}

#[test]
fn scoped_diagnostics_channel_filters_initial_value_and_change_diff() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    // a.md starts with a dangling reference: one error outside the scope.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/c.md"), "---\ntype: note\n---\n").unwrap();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "diagnostics", "path_prefix": "sub" }))
        .unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "diagnostics");

    // a.md's error sits outside the scope: the initial value is empty.
    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    assert_eq!(
        initial["result"].as_array().unwrap().len(),
        0,
        "no diagnostics under sub/, got {:?}",
        initial["result"]
    );

    // One rebuild moves the diagnostic: a.md clears, sub/c.md gains one.
    // The event's diff is scoped too — it names only the in-scope file.
    fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
    fs::write(
        root.join("sub/c.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "diagnostics-changed");
    assert_eq!(ev["at_version"], 2);
    let files = ev["scope_hint"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "only the in-scope file, got {files:?}");
    assert!(files[0].as_str().unwrap().ends_with("sub/c.md"));
}

#[test]
fn an_unknown_diagnostics_filter_arg_is_rejected_as_an_error_frame() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "diagnostics", "sevrity": "error" }))
        .unwrap();

    let frame = client.recv().unwrap().unwrap();
    assert_eq!(frame["type"], "error");
    assert!(
        frame["error"].as_str().unwrap().contains("sevrity"),
        "the error names the unknown arg, got {:?}",
        frame["error"]
    );
}

#[test]
fn link_graph_channel_delivers_the_payload_then_node_and_edge_deltas() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "link_graph" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "link_graph");

    // The initial value is the full payload; a.md is a content node.
    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    let nodes = initial["result"]["nodes"].as_array().unwrap();
    assert!(
        nodes
            .iter()
            .any(|n| n["path"].as_str().unwrap().ends_with("/a.md")),
        "a.md is a node in the initial payload, got {nodes:?}"
    );

    // One rebuild adds b.md and a prose link a → b.
    fs::write(root.join("b.md"), "# b\n").unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\n---\n\nsee [[b]].\n").unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "link-graph-changed");
    assert_eq!(ev["at_version"], 2);
    let hint = &ev["scope_hint"];
    // The new node b.md appears in nodes_added.
    let added: Vec<&str> = hint["nodes_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["path"].as_str().unwrap())
        .collect();
    assert!(
        added.iter().any(|p| p.ends_with("/b.md")),
        "b.md is an added node, got {added:?}"
    );
    // The new edge a → b appears in edges_added.
    let edges = hint["edges_added"].as_array().unwrap();
    assert!(
        edges.iter().any(|e| {
            e["from"].as_str().unwrap().ends_with("/a.md")
                && e["to"].as_str().unwrap().ends_with("/b.md")
        }),
        "the a → b edge is added, got {edges:?}"
    );
    assert!(hint["edges_removed"].as_array().unwrap().is_empty());
}

/// The sorted top-level keys of a JSON object frame.
fn keys(frame: &serde_json::Value) -> Vec<String> {
    let mut ks: Vec<String> = frame
        .as_object()
        .expect("frame is an object")
        .keys()
        .cloned()
        .collect();
    ks.sort();
    ks
}

#[test]
fn subscription_frame_envelopes_stay_stable() {
    // A drift guard: the exact field set of each subscription frame type is the
    // wire contract consumers bind to. A dropped or renamed field breaks here
    // before it breaks a consumer.
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "types" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(
        keys(&ack),
        [
            "accepted",
            "channel",
            "schema_version",
            "subscription_id",
            "type"
        ]
    );

    let initial = client.recv().unwrap().unwrap();
    assert_eq!(
        keys(&initial),
        [
            "at_version",
            "result",
            "schema_version",
            "subscription_id",
            "type"
        ]
    );

    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n  title?: String\n",
    )
    .unwrap();
    engine.rebuild();

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(
        keys(&ev),
        [
            "at_version",
            "kind",
            "schema_version",
            "scope_hint",
            "subscription_id",
            "type"
        ]
    );
}

#[test]
fn an_unknown_channel_is_rejected_as_an_error_frame() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "nonsense" })).unwrap();

    let frame = client.recv().unwrap().unwrap();
    assert_eq!(frame["type"], "error");
    assert!(frame["error"].is_string());
}

#[test]
fn type_graph_channel_delivers_the_payload_then_a_subtype_delta() {
    let (_dir, root) = fixture();
    let (_sock_dir, socket_path) = socket();
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // version 1
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client.send(&json!({ "subscribe": "type_graph" })).unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "type_graph");

    // The initial value is the full schema-graph payload; the `note` type-def is
    // a node.
    let initial = client.recv().unwrap().unwrap();
    assert_eq!(initial["type"], "initial_value");
    let nodes = initial["result"]["nodes"].as_array().unwrap();
    assert!(
        nodes
            .iter()
            .any(|n| n["path"].as_str().unwrap().ends_with("/note.type.yaml")),
        "note is a node in the initial payload, got {nodes:?}"
    );

    // One rebuild adds `special`, a subtype of `note`.
    fs::write(root.join("type/special.type.yaml"), "extends: note\n").unwrap();
    engine.rebuild(); // version 2

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "type-graph-changed");
    assert_eq!(ev["at_version"], 2);
    let hint = &ev["scope_hint"];
    // The new type-def node appears in nodes_added.
    let added: Vec<&str> = hint["nodes_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["path"].as_str().unwrap())
        .collect();
    assert!(
        added.iter().any(|p| p.ends_with("/special.type.yaml")),
        "special is an added node, got {added:?}"
    );
    // The new subtype edge special → note appears in edges_added.
    let edges = hint["edges_added"].as_array().unwrap();
    assert!(
        edges.iter().any(|e| {
            e["from"].as_str().unwrap().ends_with("/special.type.yaml")
                && e["to"].as_str().unwrap().ends_with("/note.type.yaml")
                && e["relation"] == "subtype"
        }),
        "the special → note subtype edge is added, got {edges:?}"
    );
}
