//! The `config` read over the socket: one CONSUMER config file under the
//! scoped-config channel, `<scope>/<consumer>/config/<file>`, field-shape
//! validated against a STAMPED `type` resolved in the scope's graph. Out-of-band
//! by the verb, never a walked node, the shape `device_config` returns. An
//! unresolved type is stored-as-is with a `config-type-unresolved` advisory, and
//! a path-unsafe or reserved `consumer` / `file` is an `error` response.
//!
//! See [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

struct Harness {
    _ws_dir: tempfile::TempDir,
    _cfg_dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
    ws: PathBuf,
    config_dir: PathBuf,
}

/// A one-repo tree (entry name `v`) carrying a consumer type-def
/// `viewer-default-set` with one required field, plus an injected device config
/// dir. The type lives in the entry graph, so a repo-scope (or machine-scope)
/// config stamped with it field-shape validates.
fn started() -> Harness {
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    crate::seed_repo(&ws);
    let p = ws.join("type/viewer-default-set.type.yaml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, "fields:\n  viewer: String\n").unwrap();

    let cfg_dir = tempfile::tempdir().unwrap();
    let config_dir = fs::canonicalize(cfg_dir.path()).unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    let engine = Engine::new(&ws, ConfigSource::Dir(config_dir.clone()));
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _ws_dir: ws_dir,
        _cfg_dir: cfg_dir,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
        ws,
        config_dir,
    }
}

/// Write a repo-scope config file under `<ws>/.arsumbris/<consumer>/config/`.
fn write_repo_config(h: &Harness, consumer: &str, file: &str, content: &str) {
    let p =
        h.ws.join(".arsumbris")
            .join(consumer)
            .join("config")
            .join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// Write a machine-scope config file under `<cfg>/<consumer>/config/`.
fn write_machine_config(h: &Harness, consumer: &str, file: &str, content: &str) {
    let p = h.config_dir.join(consumer).join("config").join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read_config(h: &mut Harness, args: Value) -> Value {
    let mut req = json!({ "read": "config" });
    let obj = req.as_object_mut().unwrap();
    for (k, v) in args.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    h.client.query(&req).unwrap()
}

fn codes(view: &Value) -> Vec<String> {
    let mut out: Vec<String> = view["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn an_absent_file_reports_its_path_and_no_diagnostics() {
    let mut h = started();
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "viewer-defaults.yaml", "type": "viewer-default-set" }),
    );
    assert_eq!(resp["ready"], true, "{resp}");
    let view = &resp["result"]["config"];
    assert_eq!(view["exists"], false, "{view}");
    assert!(view["content"].is_null(), "{view}");
    assert!(codes(view).is_empty(), "{view}");
    // The path is reported so a consumer knows where to author.
    assert!(
        view["path"]
            .as_str()
            .unwrap()
            .ends_with("host-app/config/viewer-defaults.yaml"),
        "{view}"
    );
}

#[test]
fn a_resolving_type_field_shape_validates() {
    let mut h = started();
    // A self-describing file missing the required `viewer` field ->
    // required-field-absent, on the REAL file, and NO self-description drift.
    write_repo_config(
        &h,
        "host-app",
        "viewer-defaults.yaml",
        "type: viewer-default-set\nother: 1\n",
    );
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "viewer-defaults.yaml", "type": "viewer-default-set" }),
    );
    let view = &resp["result"]["config"];
    assert_eq!(view["exists"], true, "{view}");
    assert_eq!(
        codes(view),
        vec!["required-field-absent".to_string()],
        "a self-describing config missing a required field is field-shape-checked, no drift: {view}"
    );
    let span_file = view["diagnostics"][0]["span"]["file"].as_str().unwrap();
    assert!(
        span_file.ends_with("viewer-defaults.yaml"),
        "the verdict lands on the real file: {view}"
    );

    // A well-formed, self-describing one is clean.
    write_repo_config(
        &h,
        "host-app",
        "viewer-defaults.yaml",
        "type: viewer-default-set\nviewer: editor-pane\n",
    );
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "viewer-defaults.yaml", "type": "viewer-default-set" }),
    );
    let view = &resp["result"]["config"];
    assert!(
        codes(view).is_empty(),
        "a well-formed self-describing config is clean: {view}"
    );
    assert!(
        view["content"].as_str().unwrap().contains("editor-pane"),
        "{view}"
    );
}

#[test]
fn a_file_without_a_type_key_carries_the_self_description_drift() {
    let mut h = started();
    // No written `type:`: the declared type is stamped and field-shape validated
    // regardless, but the file carries a `config-type-unwritten` drift nudging it
    // to self-describe, NOT the engine-schema-flavored code.
    write_repo_config(
        &h,
        "host-app",
        "viewer-defaults.yaml",
        "viewer: editor-pane\n",
    );
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "viewer-defaults.yaml", "type": "viewer-default-set" }),
    );
    let view = &resp["result"]["config"];
    assert_eq!(
        codes(view),
        vec!["config-type-unwritten".to_string()],
        "a config without `type:` drifts (self-describe nudge), still field-shape clean: {view}"
    );
    assert_eq!(view["diagnostics"][0]["severity"], "drift", "{view}");
}

#[test]
fn an_unresolved_type_is_stored_with_the_advisory() {
    let mut h = started();
    write_repo_config(&h, "host-app", "x.yaml", "anything: here\n");
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "x.yaml", "type": "no-such-type" }),
    );
    let view = &resp["result"]["config"];
    assert_eq!(view["exists"], true, "{view}");
    // Stored-as-is: content returned, a single advisory, never unknown-type-claim.
    assert_eq!(
        codes(view),
        vec!["config-type-unresolved".to_string()],
        "an unresolved type stores with the advisory, never the hard unknown-type-claim: {view}"
    );
    assert_eq!(view["diagnostics"][0]["severity"], "warning", "{view}");
    assert!(
        view["content"].as_str().unwrap().contains("anything"),
        "the value is stored and returned regardless: {view}"
    );
}

#[test]
fn a_malformed_file_surfaces_a_parse_error_even_when_the_type_is_unresolved() {
    let mut h = started();
    // Malformed YAML AND an unresolved type: the structural parse error still
    // surfaces alongside the store-as-is advisory, not hidden behind it.
    write_repo_config(&h, "host-app", "x.yaml", "viewer: [unterminated\n");
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "x.yaml", "type": "no-such-type" }),
    );
    let view = &resp["result"]["config"];
    let cs = codes(view);
    assert!(
        cs.contains(&"config-type-unresolved".to_string()),
        "the advisory is present: {view}"
    );
    assert!(
        cs.iter().any(|c| c != "config-type-unresolved"),
        "a structural (parse) diagnostic surfaces too, not just the advisory: {view}"
    );
}

#[test]
fn machine_scope_reads_under_the_device_root() {
    let mut h = started();
    // The entry graph (`v`) owns viewer-default-set, and machine scope resolves in
    // the entry graph, so a machine-scope config stamped with it validates.
    write_machine_config(
        &h,
        "host-app",
        "viewer-defaults.yaml",
        "type: viewer-default-set\nviewer: editor-pane\n",
    );
    let resp = read_config(
        &mut h,
        json!({ "scope": "machine", "consumer": "host-app", "file": "viewer-defaults.yaml", "type": "viewer-default-set" }),
    );
    let view = &resp["result"]["config"];
    assert_eq!(view["exists"], true, "{view}");
    assert!(codes(view).is_empty(), "clean machine-scope config: {view}");
    assert!(
        view["path"]
            .as_str()
            .unwrap()
            .ends_with("host-app/config/viewer-defaults.yaml"),
        "the machine path resolves under the device root: {view}"
    );
}

#[test]
fn machine_scope_set_config_writes_no_commit_and_injects_type() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config", "scope": "machine", "consumer": "host-app",
            "file": "viewer-defaults.yaml", "type": "viewer-default-set",
            "content": "viewer: editor-pane\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp}");
    // Device-global: no commit.
    assert!(
        resp["result"]["commit"].is_null(),
        "machine scope does not commit: {resp}"
    );
    assert!(resp["result"]["hash"].is_string(), "{resp}");

    // The file self-describes under the injected device root.
    let p = h
        .config_dir
        .join("host-app")
        .join("config")
        .join("viewer-defaults.yaml");
    let written = fs::read_to_string(&p).unwrap();
    assert_eq!(
        written, "type: viewer-default-set\nviewer: editor-pane\n",
        "{written:?}"
    );

    // A machine-scope read returns it clean.
    let read = h
        .client
        .query(&json!({
            "read": "config", "scope": "machine", "consumer": "host-app",
            "file": "viewer-defaults.yaml", "type": "viewer-default-set",
        }))
        .unwrap();
    assert!(
        read["result"]["config"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{read}"
    );
}

#[test]
fn path_safety_and_the_reserved_segment_reject() {
    let mut h = started();
    // A traversal `consumer`, the reserved `au-engine` owner segment, and a
    // CASE-VARIANT of it (which aliases the same file on a case-insensitive
    // filesystem) are all an `error` response, nothing read.
    for consumer in ["..", "au-engine", "Au-Engine", "AU-ENGINE"] {
        let resp = read_config(
            &mut h,
            json!({ "scope": "machine", "consumer": consumer, "file": "x.yaml", "type": "t" }),
        );
        assert_eq!(
            resp["type"], "error",
            "consumer `{consumer}` must reject: {resp}"
        );
    }
    // A `file` carrying a separator likewise.
    let resp = read_config(
        &mut h,
        json!({ "scope": "machine", "consumer": "host-app", "file": "a/b.yaml", "type": "t" }),
    );
    assert_eq!(
        resp["type"], "error",
        "a separator in file must reject: {resp}"
    );
}

#[test]
fn an_unknown_repo_scope_root_rejects() {
    let mut h = started();
    let resp = read_config(
        &mut h,
        json!({ "scope": "repo", "consumer": "host-app", "file": "x.yaml", "type": "t", "root": "/no/such/member" }),
    );
    assert_eq!(
        resp["type"], "error",
        "a repo-scope root naming no member is an error: {resp}"
    );
}
