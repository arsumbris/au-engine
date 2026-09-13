//! The `register` config mutation over the socket: it writes one `{ name, path,
//! remote }` entry into the per-user registry (`repos.yaml`), a DEVICE-GLOBAL
//! file (no git commit), then rebuilds so a newly-registered scattered member
//! mounts. The registry path comes from the engine's INJECTED `config_dir`, so
//! the test never touches the developer's real `~/.arsumbris/au-engine/config`.
//!
//! See [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

fn w(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

struct Harness {
    _ws_dir: tempfile::TempDir,
    _scattered: tempfile::TempDir,
    _cfg_dir: tempfile::TempDir,
    _cache_dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
    config_dir: PathBuf,
    lib: PathBuf,
}

/// An entry folder-repo workspace declaring an editable member `main` (a
/// co-present primary) that depends on `lib`, plus a SCATTERED `lib` repo at
/// `scattered/lib`
/// (outside the entry). `lib` is not co-present and not yet registered, so it
/// starts unmounted; `register` supplies its path. The engine's config dir and
/// package cache are injected tempdirs, so both the `register` write and the
/// registry reads stay hermetic.
fn started() -> Harness {
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = fs::canonicalize(ws_dir.path()).unwrap();
    crate::seed_workspace(&ws, &["main"]);
    w(
        &ws,
        "main/.arsumbris/repo.yaml",
        "name: main\ndeps:\n  - name: lib\n",
    );
    w(&ws, "main/type/mainthing.type.yaml", "fields: {}\n");

    let scattered = tempfile::tempdir().unwrap();
    let lib = fs::canonicalize(scattered.path()).unwrap().join("lib");
    w(&lib, ".arsumbris/repo.yaml", "name: lib\n");
    w(&lib, "type/libthing.type.yaml", "fields: {}\n");

    let cfg_dir = tempfile::tempdir().unwrap();
    let config_dir = fs::canonicalize(cfg_dir.path()).unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    let mut engine = Engine::new(&ws, ConfigSource::Dir(config_dir.clone()));
    engine.set_package_cache_root(cache_dir.path().join("packages"));
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _ws_dir: ws_dir,
        _scattered: scattered,
        _cfg_dir: cfg_dir,
        _cache_dir: cache_dir,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
        config_dir,
        lib,
    }
}

fn types_json(client: &mut Client) -> String {
    serde_json::to_string(&client.query(&json!({ "read": "types" })).unwrap()).unwrap()
}

#[test]
fn register_writes_the_entry_and_mounts_the_scattered_member() {
    let mut h = started();

    // Before register: `lib` resolves nowhere, so its type is absent.
    assert!(
        !types_json(&mut h.client).contains("libthing"),
        "lib is unmounted before it is registered"
    );

    let resp = h
        .client
        .query(&json!({
            "mutate": "register",
            "name": "lib",
            "path": h.lib.to_string_lossy(),
            "id": "reg-1",
        }))
        .unwrap();
    assert_eq!(resp["type"], "registered", "{resp}");
    assert_eq!(resp["id"], "reg-1", "the response echoes the request id");
    assert_eq!(resp["name"], "lib");

    // The entry landed in the INJECTED registry, not the real `~/.arsumbris/au-engine/config`.
    let registry = h.config_dir.join("au-engine/config/repos.yaml");
    let written = fs::read_to_string(&registry).expect("repos.yaml written");
    assert!(
        written.contains("lib") && written.contains(&*h.lib.to_string_lossy()),
        "the entry names lib and its path: {written}"
    );

    // The register rebuild mounted the now-locatable scattered member, so its
    // type is in the served graph — the peer-unmounted bootstrap, closed.
    assert!(
        types_json(&mut h.client).contains("libthing"),
        "lib mounts after registration"
    );
}

#[test]
fn register_refuses_a_path_whose_repo_declares_another_name() {
    let mut h = started();

    // A repo that declares itself `other`, at lib's path's sibling.
    let other = h.lib.parent().unwrap().join("other-dir");
    w(&other, ".arsumbris/repo.yaml", "name: other\n");

    let resp = h
        .client
        .query(&json!({
            "mutate": "register",
            "name": "lib",
            "path": other.to_string_lossy(),
            "id": "reg-2",
        }))
        .unwrap();
    // A path whose repo.yaml declares another name is a dependency-identity
    // conflict: rejected, nothing written.
    assert_eq!(resp["type"], "error", "{resp}");
    assert!(
        resp["error"].as_str().unwrap().contains("other"),
        "the reject names the declared-vs-key mismatch: {resp}"
    );
    let registry = h.config_dir.join("au-engine/config/repos.yaml");
    let written = fs::read_to_string(&registry).unwrap_or_default();
    assert!(
        !written.contains("other-dir"),
        "a rejected register writes nothing: {written}"
    );
}
