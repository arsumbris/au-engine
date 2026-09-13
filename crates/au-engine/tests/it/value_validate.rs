//! The `validate_value` read over the socket: a transient JSON value gets the
//! same verdict the engine would give the value as a file's frontmatter.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A knowledge base with one type-def: `widget` requires `name: String`, allows an
/// optional `size: Number`. `content/probe.md` claims `widget` but omits the
/// required `name`, so the file path and the value path can be compared.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir(root.join("content")).unwrap();
    fs::write(
        root.join("type/widget.type.yaml"),
        "fields:\n  name: String\n  size?: Number\n",
    )
    .unwrap();
    fs::write(root.join("content/probe.md"), "---\ntype: widget\n---\n").unwrap();
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

/// The verdict list: one entry per mounted identity the name denotes.
fn verdicts(h: &mut Harness, type_name: &str, value: Value) -> Vec<Value> {
    let resp = h
        .client
        .query(&json!({ "read": "validate_value", "type_name": type_name, "value": value }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    resp["result"]["validate_value"].as_array().unwrap().clone()
}

/// Every diagnostic across every verdict. The single-identity fixture below has
/// exactly one fit, so this is that fit's verdict — and for an unknown name it
/// is the null-identity verdict, which is the point: folding the list never
/// silently loses the miss.
fn validate_value(h: &mut Harness, type_name: &str, value: Value) -> Vec<Value> {
    verdicts(h, type_name, value)
        .iter()
        .flat_map(|v| v["diagnostics"].as_array().unwrap().clone())
        .collect()
}

fn codes(diags: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = diags
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn a_conforming_value_has_no_diagnostics() {
    let mut h = started();
    let diags = validate_value(&mut h, "widget", json!({ "name": "ok", "size": 3 }));
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_missing_required_field_is_reported() {
    let mut h = started();
    let diags = validate_value(&mut h, "widget", json!({ "size": 3 }));
    assert!(
        codes(&diags).contains(&"required-field-absent".to_string()),
        "{diags:?}"
    );
}

#[test]
fn a_field_shape_mismatch_is_reported() {
    let mut h = started();
    let diags = validate_value(&mut h, "widget", json!({ "name": 5 }));
    assert!(
        codes(&diags).contains(&"field-shape-mismatch".to_string()),
        "{diags:?}"
    );
}

#[test]
fn an_extra_field_passes_silently() {
    let mut h = started();
    let diags = validate_value(
        &mut h,
        "widget",
        json!({ "name": "ok", "extra": "anything" }),
    );
    assert!(diags.is_empty(), "extras pass silently: {diags:?}");
}

fn undeclared(v: &Value) -> Vec<String> {
    v["undeclared_fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn an_extra_field_is_listed_in_undeclared_fields() {
    let mut h = started();
    // The extra passes with no diagnostic, but the response surfaces it so a
    // caller can catch a typo that quietly validated.
    let vs = verdicts(
        &mut h,
        "widget",
        json!({ "name": "ok", "extra": "anything" }),
    );
    assert_eq!(vs.len(), 1, "{vs:?}");
    assert_eq!(undeclared(&vs[0]), vec!["extra".to_string()], "{vs:?}");
}

#[test]
fn a_conforming_value_has_no_undeclared_fields() {
    let mut h = started();
    let vs = verdicts(&mut h, "widget", json!({ "name": "ok", "size": 3 }));
    assert!(undeclared(&vs[0]).is_empty(), "{vs:?}");
}

#[test]
fn a_matching_type_claim_is_not_undeclared() {
    let mut h = started();
    // The `type` claim is not a field, so it never appears in undeclared_fields.
    let vs = verdicts(&mut h, "widget", json!({ "type": "widget", "name": "ok" }));
    assert!(undeclared(&vs[0]).is_empty(), "{vs:?}");
}

#[test]
fn a_matching_type_claim_in_the_value_is_tolerated() {
    let mut h = started();
    // The natural thing to validate is the on-disk instance shape, which
    // carries its own `type:`. A matching claim must not manufacture a spurious
    // `duplicate-key 'type'` ahead of the real diagnostic.
    let diags = validate_value(&mut h, "widget", json!({ "type": "widget", "size": 3 }));
    let cs = codes(&diags);
    assert!(
        !cs.contains(&"duplicate-key-in-mapping".to_string()),
        "matching type claim must be tolerated: {diags:?}"
    );
    assert!(
        cs.contains(&"required-field-absent".to_string()),
        "the real diagnostic still surfaces first: {diags:?}"
    );
}

#[test]
fn an_unknown_type_name_is_an_unresolved_claim() {
    let mut h = started();
    let diags = validate_value(&mut h, "nope", json!({}));
    assert!(
        codes(&diags).contains(&"unknown-type-claim".to_string()),
        "{diags:?}"
    );
}

#[test]
fn a_non_object_value_is_not_a_mapping() {
    let mut h = started();
    let diags = validate_value(&mut h, "widget", json!(42));
    assert!(
        codes(&diags).contains(&"instance-not-a-mapping".to_string()),
        "{diags:?}"
    );
}

#[test]
fn diagnostics_carry_synthetic_line_col() {
    let mut h = started();
    let diags = validate_value(&mut h, "widget", json!({ "size": 3 }));
    let d = &diags[0];
    assert!(d["span"]["line_col"].is_object(), "{d:?}");
}

/// The value verdict equals the file verdict. `content/probe.md` is a real
/// file claiming `widget` with no `name`; validating the same value returns
/// the same diagnostic codes and messages, modulo file path and span.
#[test]
fn the_value_verdict_matches_the_file_verdict() {
    let mut h = started();

    let file_resp = h
        .client
        .query(&json!({ "read": "diagnostics", "path": "content/probe.md" }))
        .unwrap();
    let file_diags = file_resp["result"]["diagnostics"]
        .as_array()
        .unwrap()
        .clone();

    let value_diags = validate_value(&mut h, "widget", json!({}));

    let pairs = |diags: &[Value]| {
        let mut v: Vec<(String, String)> = diags
            .iter()
            .map(|d| {
                (
                    d["code"].as_str().unwrap().to_string(),
                    d["message"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        v.sort();
        v
    };

    assert!(
        !file_diags.is_empty(),
        "the probe file should itself be missing a required field"
    );
    assert_eq!(
        pairs(&file_diags),
        pairs(&value_diags),
        "value verdict must match file verdict (code + message)"
    );
}

/// A two-repo tree: `base` owns `person` and `org`; `app` imports `base` and
/// owns `card` with a `who?: person::base*` cross-repo reference field.
fn cross_repo_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, c: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/person.type.yaml", "fields:\n  name: String\n");
    w("base/type/org.type.yaml", "fields:\n  name: String\n");
    w("base/alice.md", "---\ntype: person\nname: Alice\n---\n");
    w("base/acme.md", "---\ntype: org\nname: Acme\n---\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w(
        "app/type/card.type.yaml",
        "fields:\n  who?: person::base*\n",
    );
    (dir, root)
}

fn started_on(dir: tempfile::TempDir, root: PathBuf) -> Harness {
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

fn verdicts_scoped(h: &mut Harness, type_name: &str, repo: &str, value: Value) -> Vec<Value> {
    let resp = h
        .client
        .query(&json!({ "read": "validate_value", "type_name": type_name, "repo": repo, "value": value }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    resp["result"]["validate_value"].as_array().unwrap().clone()
}

fn validate_value_scoped(h: &mut Harness, type_name: &str, repo: &str, value: Value) -> Vec<Value> {
    verdicts_scoped(h, type_name, repo, value)
        .iter()
        .flat_map(|v| v["diagnostics"].as_array().unwrap().clone())
        .collect()
}

#[test]
fn a_wrong_typed_cross_repo_reference_in_a_value_is_reported() {
    let (dir, root) = cross_repo_fixture();
    let mut h = started_on(dir, root);
    // `who` (person*) pointed cross-repo at a `base` org → mismatch.
    let diags = validate_value_scoped(&mut h, "card", "app", json!({ "who": "[[acme::base]]" }));
    assert!(
        codes(&diags).contains(&"reference-target-type-mismatch".to_string()),
        "a cross-repo value ref at a wrong type is a mismatch, got {diags:?}"
    );
}

#[test]
fn a_satisfying_cross_repo_reference_in_a_value_is_clean() {
    let (dir, root) = cross_repo_fixture();
    let mut h = started_on(dir, root);
    let diags = validate_value_scoped(&mut h, "card", "app", json!({ "who": "[[alice::base]]" }));
    assert!(
        diags.is_empty(),
        "a matching cross-repo value ref is clean, got {diags:?}"
    );
}

// --- multi-fit -----------------------------------------------------------
//
// A bare name conflates across mounted repos, so `validate_value` answers one
// verdict PER owning identity rather than guessing a winner.

/// Two repos owning a DIVERGENT `note`: `base` requires `title`, `app` requires
/// `heading`. The same value therefore passes one and fails the other, so a
/// per-identity verdict is observably different from a single merged one.
fn divergent_same_name_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, c: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/note.type.yaml", "fields:\n  title: String\n");
    w("app/.arsumbris/repo.yaml", "name: app\n");
    w("app/type/note.type.yaml", "fields:\n  heading: String\n");
    (dir, root)
}

fn identity_repos(verdicts: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = verdicts
        .iter()
        .map(|x| x["identity"]["repo"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn a_bare_name_owned_by_two_repos_returns_one_verdict_per_identity() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    let vs = verdicts(&mut h, "note", json!({ "title": "t" }));

    assert_eq!(identity_repos(&vs), vec!["app", "base"], "{vs:?}");
    let hashes: Vec<&str> = vs
        .iter()
        .map(|v| v["identity"]["hash"].as_str().unwrap())
        .collect();
    assert_ne!(
        hashes[0], hashes[1],
        "divergent same-named defs are distinct identities: {vs:?}"
    );
}

/// The load-bearing one: each verdict is validated against ITS OWN identity's
/// shape. A merged or first-wins answer cannot produce this split.
#[test]
fn each_identitys_verdict_reflects_that_identitys_shape() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    let vs = verdicts(&mut h, "note", json!({ "title": "t" }));

    let by_repo = |repo: &str| -> Vec<String> {
        let v = vs
            .iter()
            .find(|v| v["identity"]["repo"] == repo)
            .unwrap_or_else(|| panic!("no verdict for {repo}: {vs:?}"));
        codes(v["diagnostics"].as_array().unwrap())
    };

    assert!(
        by_repo("base").is_empty(),
        "base's note requires title, which the value supplies: {vs:?}"
    );
    assert!(
        by_repo("app").contains(&"required-field-absent".to_string()),
        "app's note requires heading, which the value omits: {vs:?}"
    );
}

#[test]
fn a_repo_arg_narrows_to_that_identity() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    let vs = verdicts_scoped(&mut h, "note", "base", json!({ "title": "t" }));
    assert_eq!(identity_repos(&vs), vec!["base"], "{vs:?}");
}

#[test]
fn a_repo_qualifier_in_the_name_narrows_and_beats_the_repo_arg() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    // The qualifier says base, the arg says app. The qualifier wins, so the
    // contradiction is not silently resolved in the arg's favour.
    let vs = verdicts_scoped(&mut h, "note::base", "app", json!({ "title": "t" }));
    assert_eq!(identity_repos(&vs), vec!["base"], "{vs:?}");
    assert!(
        vs[0]["diagnostics"].as_array().unwrap().is_empty(),
        "scoped to base, the value conforms: {vs:?}"
    );
}

// --- the unknown name fails CLOSED ---------------------------------------

/// An unknown name yields ONE null-identity verdict carrying the diagnostic a
/// FILE claiming an absent type gets — not an empty list.
///
/// NON-VACUITY: the assertion that matters is the DIAGNOSTIC, not the null. An
/// empty result would also have a null-free list, so this test is what pins
/// fail-closed: a consumer folding `diagnostics` across the verdicts sees an
/// error, rather than reading zero errors as a clean bill of health.
#[test]
fn an_unknown_name_yields_a_null_identity_verdict_not_an_empty_list() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    let vs = verdicts(&mut h, "nope", json!({ "title": "t" }));

    assert_eq!(vs.len(), 1, "one verdict for the miss: {vs:?}");
    assert!(vs[0]["identity"].is_null(), "no identity to name: {vs:?}");
    assert!(
        codes(vs[0]["diagnostics"].as_array().unwrap()).contains(&"unknown-type-claim".to_string()),
        "the miss is a finding, not an absence: {vs:?}"
    );
}

/// A name mounted SOMEWHERE but not in the requested repo is the same miss:
/// scoped to a repo that does not own it, the verdict is the unknown claim,
/// never another repo's shape silently standing in.
#[test]
fn a_name_absent_from_the_requested_repo_is_a_null_identity_verdict() {
    let (dir, root) = cross_repo_fixture();
    let mut h = started_on(dir, root);
    // `person` is owned by base, not app.
    let vs = verdicts_scoped(&mut h, "person", "app", json!({ "name": "x" }));

    assert_eq!(vs.len(), 1, "{vs:?}");
    assert!(vs[0]["identity"].is_null(), "{vs:?}");
    assert!(
        codes(vs[0]["diagnostics"].as_array().unwrap()).contains(&"unknown-type-claim".to_string()),
        "{vs:?}"
    );
}

/// An unknown name must not swallow the value's own structural problems: a
/// consumer fixing the type name would otherwise hit the second error only on
/// the next round trip.
#[test]
fn an_unknown_name_still_reports_the_values_structural_diagnostics() {
    let (dir, root) = divergent_same_name_fixture();
    let mut h = started_on(dir, root);
    let vs = verdicts(&mut h, "nope", json!(42));

    assert!(vs[0]["identity"].is_null(), "{vs:?}");
    assert!(
        codes(vs[0]["diagnostics"].as_array().unwrap())
            .contains(&"instance-not-a-mapping".to_string()),
        "the malformed value is reported alongside the unknown name: {vs:?}"
    );
}
