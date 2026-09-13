//! The `preview_mutation` read over the socket: simulate a deterministic
//! mutation over an overlay of the current snapshot and report its product, the
//! would-be file's type identities and diagnostics plus the per-file blast
//! radius, without writing disk or committing.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A knowledge base with:
/// - `task`: requires `title: String`, and a `body:` template with a required
///   `Details` section, so a preview exercises WHOLE-file body typing.
/// - `ref`: a `target: task*` reference field, so a delete of a referent dangles
///   its referrer.
/// - `hub.md`: a valid `task`. `referrer.md`: a `ref` pointing at `hub`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  title: String\nbody:\n  - section: Details\n",
    )
    .unwrap();
    fs::write(
        root.join("type/ref.type.yaml"),
        "fields:\n  target: task*\n",
    )
    .unwrap();
    fs::write(
        root.join("hub.md"),
        "---\ntype: task\ntitle: Hub\n---\n# Details\n\nbody.\n",
    )
    .unwrap();
    fs::write(
        root.join("referrer.md"),
        "---\ntype: ref\ntarget: \"[[hub]]\"\n---\n",
    )
    .unwrap();
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

/// The `preview_mutation` payload for a request.
fn preview(h: &mut Harness, req: Value) -> Value {
    let mut req = req;
    req["read"] = json!("preview_mutation");
    let resp = h.client.query(&req).unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    resp["result"]["preview_mutation"].clone()
}

fn codes(diags: &Value) -> Vec<String> {
    let mut out: Vec<String> = diags
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

fn identity_names(target: &Value) -> Vec<String> {
    let mut out: Vec<String> = target["identities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn a_valid_write_reports_its_identity_and_no_diagnostics() {
    let mut h = started();
    let p = preview(
        &mut h,
        json!({
            "op": "write_file",
            "path": "new.md",
            "content": "---\ntype: task\ntitle: New\n---\n# Details\n\nbody.\n",
        }),
    );
    let target = &p["target"];
    assert_eq!(identity_names(target), vec!["task"], "{p:?}");
    assert!(
        target["diagnostics"].as_array().unwrap().is_empty(),
        "a valid task previews clean: {p:?}"
    );
    assert!(target["hash"].is_string(), "the would-be hash is present");
    assert!(p["blast_radius"].as_array().unwrap().is_empty(), "{p:?}");
}

#[test]
fn a_preview_writes_no_disk_and_advances_no_version() {
    let mut h = started();
    let before = h.client.query(&json!({ "read": "lifecycle" })).unwrap()["version"].clone();
    preview(
        &mut h,
        json!({
            "op": "write_file",
            "path": "ghost.md",
            "content": "---\ntype: task\ntitle: Ghost\n---\n# Details\n\nx.\n",
        }),
    );
    // The file never landed: a content read finds nothing.
    let content = h
        .client
        .query(&json!({ "read": "content", "path": "ghost.md" }))
        .unwrap();
    assert!(
        content["result"]["content"].is_null(),
        "preview must not write disk: {content:?}"
    );
    // The version did not advance: a preview is a read, not a mutation.
    let after = h.client.query(&json!({ "read": "lifecycle" })).unwrap()["version"].clone();
    assert_eq!(before, after, "a preview advances no version");
}

#[test]
fn a_claim_over_junk_resolves_the_identity_and_reports_the_failures() {
    let mut h = started();
    // Claims `task` but omits the required title AND the required Details section:
    // the identity still resolves, and WHOLE-file validation reports both.
    let p = preview(
        &mut h,
        json!({
            "op": "write_file",
            "path": "junk.md",
            "content": "---\ntype: task\n---\nnot the details section\n",
        }),
    );
    let target = &p["target"];
    assert_eq!(
        identity_names(target),
        vec!["task"],
        "the claim resolves even over junk: {p:?}"
    );
    let cs = codes(&target["diagnostics"]);
    assert!(
        cs.contains(&"required-field-absent".to_string()),
        "missing title: {p:?}"
    );
    assert!(
        cs.contains(&"body-section-missing".to_string()),
        "body typing runs, so the missing Details section is reported: {p:?}"
    );
}

#[test]
fn a_write_with_no_type_claim_has_no_identities() {
    let mut h = started();
    let p = preview(
        &mut h,
        json!({
            "op": "write_file",
            "path": "note.md",
            "content": "# just a note\n",
        }),
    );
    assert!(
        p["target"]["identities"].as_array().unwrap().is_empty(),
        "a plain note claims no type: {p:?}"
    );
}

#[test]
fn an_edit_of_an_absent_file_rejects() {
    let mut h = started();
    let p = preview(
        &mut h,
        json!({
            "op": "edit_file",
            "path": "absent.md",
            "old_string": "x",
            "new_string": "y",
        }),
    );
    assert!(
        p["reject"]["message"]
            .as_str()
            .unwrap()
            .contains("nothing to edit"),
        "an edit of an absent file is a reject: {p:?}"
    );
}

#[test]
fn an_edit_previews_the_resulting_file() {
    let mut h = started();
    // Edit hub's title; the product is computed from the current file plus the
    // fragment, no caller reconstruction. It stays a valid task.
    let p = preview(
        &mut h,
        json!({
            "op": "edit_file",
            "path": "hub.md",
            "old_string": "title: Hub",
            "new_string": "title: Renamed",
        }),
    );
    let target = &p["target"];
    assert_eq!(identity_names(target), vec!["task"], "{p:?}");
    assert!(
        target["diagnostics"].as_array().unwrap().is_empty(),
        "the edited task is still valid: {p:?}"
    );
}

#[test]
fn a_delete_dangles_its_referrer_in_the_blast_radius() {
    let mut h = started();
    // Deleting hub.md leaves referrer.md's `target: [[hub]]` dangling.
    let p = preview(&mut h, json!({ "op": "delete_file", "path": "hub.md" }));
    // The target of a delete carries no identity, the file is gone.
    assert!(
        p["target"]["identities"].as_array().unwrap().is_empty(),
        "a delete's target has no identity: {p:?}"
    );
    // The referrer surfaces in the blast radius with the dangling-reference code.
    let blast = p["blast_radius"].as_array().unwrap();
    let referrer = blast
        .iter()
        .find(|e| e["path"].as_str().unwrap().ends_with("referrer.md"))
        .unwrap_or_else(|| panic!("referrer should be in the blast radius: {p:?}"));
    assert!(
        codes(&referrer["diagnostics"]).contains(&"reference-target-missing".to_string()),
        "the delete dangles the referrer: {p:?}"
    );
}

#[test]
fn a_type_def_edit_surfaces_a_broken_dependent_in_the_blast_radius() {
    let mut h = started();
    // Add a new required field to `task`: the existing hub.md (a task) now misses
    // it. A type-def edit goes through the whole-knowledge-base build path.
    let p = preview(
        &mut h,
        json!({
            "op": "write_file",
            "path": "type/task.type.yaml",
            "content": "fields:\n  title: String\n  owner: String\nbody:\n  - section: Details\n",
        }),
    );
    let blast = p["blast_radius"].as_array().unwrap();
    let hub = blast
        .iter()
        .find(|e| e["path"].as_str().unwrap().ends_with("hub.md"))
        .unwrap_or_else(|| panic!("hub should be broken by the new required field: {p:?}"));
    assert!(
        codes(&hub["diagnostics"]).contains(&"required-field-absent".to_string()),
        "the new required field breaks the existing task: {p:?}"
    );
}

/// The target verdict equals the `diagnostics` read of the same file after the
/// real write commits: a preview cannot disagree with what the write lands.
#[test]
fn the_preview_verdict_matches_the_post_write_verdict() {
    let mut h = started();
    let content = "---\ntype: task\n---\nno details here\n";

    let previewed = preview(
        &mut h,
        json!({ "op": "write_file", "path": "parity.md", "content": content }),
    );
    let preview_codes = codes(&previewed["target"]["diagnostics"]);

    // Now really write it, then read the file's diagnostics.
    let w = h
        .client
        .query(&json!({ "mutate": "write_file", "path": "parity.md", "content": content }))
        .unwrap();
    assert_eq!(w["type"], "response", "{w:?}");
    let real = h
        .client
        .query(&json!({ "read": "diagnostics", "path": "parity.md" }))
        .unwrap();
    let real_codes = codes(&real["result"]["diagnostics"]);

    assert!(
        !real_codes.is_empty(),
        "the file itself is missing a required field and section: {real:?}"
    );
    assert_eq!(
        preview_codes, real_codes,
        "the preview verdict must equal the post-write verdict"
    );
}

/// A repo whose `.auignore` excludes `docs/`, so files there are absent from the
/// graph with zero diagnostics. The regression fixture for the preview scope-leak.
fn scoped_started() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::write(root.join(".arsumbris/.auignore"), "docs/\n").unwrap();
    fs::create_dir(root.join("docs")).unwrap();

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

/// A preview of a write to an auignored (out-of-scope) target must carry zero
/// diagnostics, exactly as the committed write does. Before the scope filter was
/// applied in the preview path, the recompute parsed the would-be file and leaked
/// a `yaml-parse-error` the walk suppresses.
#[test]
fn a_preview_of_an_auignored_target_leaks_no_diagnostics() {
    let mut h = scoped_started();
    // Invalid YAML frontmatter: an unquoted `: ` colon-space, the exact shape that
    // trips `yaml-parse-error`. Under `docs/`, so the walk excludes it.
    let content = "---\ntldr: text: colon\n---\nbody\n";

    let previewed = preview(
        &mut h,
        json!({ "op": "write_file", "path": "docs/leak.md", "content": content }),
    );
    let target = &previewed["target"];
    assert!(
        target["diagnostics"].as_array().unwrap().is_empty(),
        "an auignored target previews clean, no scope leak: {previewed:?}"
    );
    assert!(
        target["identities"].as_array().unwrap().is_empty(),
        "an out-of-scope file claims no graph identity: {previewed:?}"
    );
    assert!(
        previewed["blast_radius"].as_array().unwrap().is_empty(),
        "an out-of-scope file has no blast radius: {previewed:?}"
    );

    // Parity: really write it, then read the file's diagnostics. The committed
    // write lands zero, so the preview must too.
    let w = h
        .client
        .query(&json!({ "mutate": "write_file", "path": "docs/leak.md", "content": content }))
        .unwrap();
    assert_eq!(w["type"], "response", "{w:?}");
    let real = h
        .client
        .query(&json!({ "read": "diagnostics", "path": "docs/leak.md" }))
        .unwrap();
    let real_codes = codes(&real["result"]["diagnostics"]);
    assert!(
        real_codes.is_empty(),
        "the committed write lands zero diagnostics for an auignored file: {real:?}"
    );
}
