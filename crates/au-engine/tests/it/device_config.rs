//! The `device_config` read over the socket: the per-user device-global
//! engine-schema files (`repos.yaml`, `workspaces.yaml`) surface their path,
//! content, and field-shape diagnostics against their hardwired `au.engine.*`
//! def. These files sit OUTSIDE every knowledge base, so the diagnostics land on this
//! read, not on the workspace `diagnostics` read, and their spans point at the
//! real file. The per-user device root is the engine's INJECTED `config_dir`, a
//! tempdir, so the test never touches the developer's real `~/.arsumbris/au-engine/config`.
//!
//! See [[spec - engine-schema files - hardwired-schema files are first-class substrate nodes]]
//! and [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

struct Harness {
    _ws_dir: tempfile::TempDir,
    _cfg_dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
    config_dir: PathBuf,
}

/// A one-repo tree workspace plus an injected config dir. The knowledge base carries the
/// builtin `au-engine` graph (every build does), so `au.engine.repos` /
/// `au.engine.workspaces` resolve for the device-global field-shape check.
fn started() -> Harness {
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    crate::seed_repo(&ws);
    let p = ws.join("type/thing.type.yaml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "fields: {}\n").unwrap();

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
        config_dir,
    }
}

/// Write a device-global config file under the injected device root.
fn write_config(h: &Harness, rel: &str, content: &str) {
    let p = h.config_dir.join("au-engine").join("config").join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn device_config(h: &mut Harness) -> Value {
    let resp = h.client.query(&json!({ "read": "device_config" })).unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    resp["result"]["device_config"].clone()
}

fn codes(entry: &Value) -> Vec<String> {
    let mut out: Vec<String> = entry["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn wellformed_files_report_content_and_no_diagnostics() {
    let mut h = started();
    write_config(
        &h,
        "repos.yaml",
        "repos:\n  - name: lib\n    remote: git@github.com:o/lib.git\n    path: /kb/lib\n",
    );
    write_config(
        &h,
        "workspaces.yaml",
        "workspaces:\n  - path: /kb/main.au-workspace.yaml\n  - name: scratch\n    path: /kb/s.au-workspace.yaml\n",
    );

    let result = device_config(&mut h);
    let repos = &result["repos"];
    assert_eq!(repos["exists"], true, "{result}");
    assert!(
        repos["content"].as_str().unwrap().contains("lib"),
        "the raw content is returned: {result}"
    );
    // Field-shape clean, but an unwritten device file drifts, a nudge to
    // self-describe (`engine-schema-type-unwritten`). The floor by kind keeps it
    // fully correct regardless, so drift, never a field error.
    assert_eq!(
        codes(repos),
        vec!["engine-schema-type-unwritten".to_string()],
        "a wellformed but unwritten repos.yaml carries only the self-description drift: {result}"
    );

    let workspaces = &result["workspaces"];
    assert_eq!(workspaces["exists"], true, "{result}");
    assert_eq!(
        codes(workspaces),
        vec!["engine-schema-type-unwritten".to_string()],
        "a wellformed but unwritten workspaces.yaml carries only the self-description drift: {result}"
    );
}

#[test]
fn a_malformed_repos_entry_is_reported_with_a_real_file_span() {
    let mut h = started();
    // A registry-entry requires `path`; this one omits it.
    write_config(&h, "repos.yaml", "repos:\n  - name: lib\n    remote: r\n");

    let result = device_config(&mut h);
    let repos = &result["repos"];
    assert_eq!(repos["exists"], true, "{result}");
    assert!(
        codes(repos).contains(&"required-field-absent".to_string()),
        "a registry-entry missing `path` is a required-field violation: {result}"
    );

    // The verdict lands on the REAL file, not a synthetic `<value>` document, so
    // a config-authoring consumer can jump to the offending line.
    let diag = &repos["diagnostics"][0];
    let span_file = diag["span"]["file"].as_str().unwrap();
    assert!(
        Path::new(span_file).ends_with("repos.yaml"),
        "the diagnostic span points at the real repos.yaml: {diag}"
    );
    assert!(
        diag["span"]["line_col"].is_object(),
        "the span carries line/col into the real file: {diag}"
    );
}

#[test]
fn a_malformed_workspaces_entry_is_reported() {
    let mut h = started();
    // A workspace-entry requires `path`; this one omits it.
    write_config(&h, "workspaces.yaml", "workspaces:\n  - name: scratch\n");

    let result = device_config(&mut h);
    let workspaces = &result["workspaces"];
    assert_eq!(workspaces["exists"], true, "{result}");
    assert!(
        codes(workspaces).contains(&"required-field-absent".to_string()),
        "a workspace-entry missing `path` is a required-field violation: {result}"
    );
    // The other file is untouched and absent, so it stays a clean empty state.
    assert_eq!(result["repos"]["exists"], false, "{result}");
    assert!(codes(&result["repos"]).is_empty(), "{result}");
}

#[test]
fn absent_files_are_a_clean_empty_state() {
    let mut h = started();
    // No config files written under the injected dir.
    let result = device_config(&mut h);
    for key in ["repos", "workspaces"] {
        let entry = &result[key];
        assert_eq!(
            entry["exists"], false,
            "{key} is a legitimate empty state: {result}"
        );
        assert!(entry["content"].is_null(), "{key} has no content: {result}");
        assert!(
            codes(entry).is_empty(),
            "{key} absent fires no diagnostics: {result}"
        );
        // The resolved path is still reported, so a consumer knows where to author.
        assert!(
            entry["path"]
                .as_str()
                .unwrap()
                .ends_with(&format!("{key}.yaml")),
            "{key} reports its resolved path: {result}"
        );
    }
}

#[test]
fn a_non_utf8_file_reports_a_diagnostic_not_content() {
    let mut h = started();
    let p = h
        .config_dir
        .join("au-engine")
        .join("config")
        .join("repos.yaml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, [0xff, 0xfe, b'x']).unwrap();

    let result = device_config(&mut h);
    let repos = &result["repos"];
    assert_eq!(repos["exists"], true, "{result}");
    assert!(
        repos["content"].is_null(),
        "non-UTF-8 has no text content: {result}"
    );
    assert!(
        !repos["diagnostics"].as_array().unwrap().is_empty(),
        "a non-UTF-8 device file surfaces a diagnostic: {result}"
    );
}
