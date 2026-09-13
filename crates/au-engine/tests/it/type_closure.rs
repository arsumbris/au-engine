//! The `type_closure` read: a type identity's resolved ancestor closure and
//! effective field set, with each field's declaring origin.
//!
//! Serves the three queries consumers were each re-deriving by walking `parents`
//! over the raw `types` read — effective fields, field origin, and ancestor
//! membership — from one traversal.
//!
//! The cross-repo cases are the point: a client-side walk had to re-key itself
//! by `(name, owner-repo)` to follow a `parent::repo` edge, which is exactly
//! what made those walks fragile.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::wire_fixtures::{harness, Harness};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// Two repos owning a DIVERGENT same-named type, plus a cross-repo parent and a
/// diamond.
///
/// - `base` owns `note { title }` and `tag { label }`.
/// - `app` owns its OWN `note { title, archived }` — a distinct identity under
///   the same name, which is what makes a bare lookup multi-fit.
/// - `app` owns `card`, extending the PEER `note::base`, so its closure crosses
///   the repo boundary and its inherited field's origin is owned by `base`.
/// - `app` owns `both`, extending `card` and `tag::base`, a diamond over two
///   repos.
fn cross_repo_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/note.type.yaml",
        "#: a base note\nfields:\n  title: String\n",
    );
    write(
        &root,
        "base/type/tag.type.yaml",
        "fields:\n  label: String\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // A SAME-NAMED, DIVERGENT own copy: the multi-fit case.
    write(
        &root,
        "app/type/note.type.yaml",
        "fields:\n  title: String\n  archived: Boolean\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "extends: note::base\nfields:\n  pinned: Boolean\n",
    );
    write(
        &root,
        "app/type/both.type.yaml",
        "extends:\n  - card\n  - tag::base\nfields:\n  extra: String\n",
    );

    (dir, root)
}

fn closure(h: &mut Harness, args: Value) -> Value {
    h.payload("type_closure", args)
}

fn names(entries: &Value) -> Vec<String> {
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect()
}

fn field(entries: &Value, name: &str) -> Value {
    entries
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("field {name} present: {entries}"))
        .clone()
}

/// A name owned by exactly one mounted repo answers with exactly one closure.
#[test]
fn a_unique_name_answers_one_closure() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "card" }));
    let entries = out.as_array().unwrap();
    assert_eq!(entries.len(), 1, "one identity owns `card`: {out}");
    assert_eq!(entries[0]["identity"]["name"], "card");
    assert_eq!(entries[0]["identity"]["repo"], "app");
    assert!(
        !entries[0]["identity"]["hash"].as_str().unwrap().is_empty(),
        "the identity carries its closure hash"
    );
}

/// A BARE name that conflates across repos answers one closure PER identity,
/// each owner-and-hash qualified. It does not guess a winner.
#[test]
fn a_conflating_bare_name_answers_one_closure_per_identity() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "note" }));
    let entries = out.as_array().unwrap();
    assert_eq!(entries.len(), 2, "both `note` identities answer: {out}");

    let mut repos: Vec<&str> = entries
        .iter()
        .map(|e| e["identity"]["repo"].as_str().unwrap())
        .collect();
    repos.sort();
    assert_eq!(repos, vec!["app", "base"]);

    // Distinct identities, so distinct hashes — the copies genuinely diverge.
    let hashes: Vec<&str> = entries
        .iter()
        .map(|e| e["identity"]["hash"].as_str().unwrap())
        .collect();
    assert_ne!(
        hashes[0], hashes[1],
        "a divergent same-named copy is a different identity: {out}"
    );

    // And the shapes differ, which is what the hashes are reporting.
    let app = entries
        .iter()
        .find(|e| e["identity"]["repo"] == "app")
        .unwrap();
    let base = entries
        .iter()
        .find(|e| e["identity"]["repo"] == "base")
        .unwrap();
    assert_eq!(names(&app["fields"]), vec!["archived", "title"]);
    assert_eq!(names(&base["fields"]), vec!["title"]);
}

/// A `repo` arg, or a `::repo` in the name, scopes the conflating name to one
/// identity. Both spellings mean the same thing.
#[test]
fn a_qualifier_scopes_to_one_identity() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    for args in [
        json!({ "name": "note", "repo": "base" }),
        json!({ "name": "note::base" }),
    ] {
        let out = closure(&mut h, args.clone());
        let entries = out.as_array().unwrap();
        assert_eq!(entries.len(), 1, "scoped to one ({args}): {out}");
        assert_eq!(entries[0]["identity"]["repo"], "base");
        assert_eq!(names(&entries[0]["fields"]), vec!["title"]);
    }
}

/// The ancestor closure is SELF FIRST, then name-sorted, and each entry is
/// resolved to the repo that actually OWNS it — so a `parent::repo` edge
/// reports the peer, not the importing member.
#[test]
fn ancestors_are_self_first_and_owner_resolved_across_the_repo_boundary() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "card" }));
    let ancestors = &out[0]["ancestors"];

    assert_eq!(
        names(ancestors),
        vec!["card", "note"],
        "self first, then the folded parent: {ancestors}"
    );
    assert_eq!(ancestors[0]["repo"], "app", "self is owned by app");
    assert_eq!(
        ancestors[1]["repo"], "base",
        "the `note::base` parent resolves to its OWNER, not to the importer"
    );
    // Owner-resolved means the peer's identity, so the hash is base's `note`,
    // never app's divergent same-named copy.
    let base_note = closure(&mut h, json!({ "name": "note::base" }));
    assert_eq!(ancestors[1]["hash"], base_note[0]["identity"]["hash"]);
}

/// The effective field set is own fields plus every ancestor's, and each field
/// names the type-def that DECLARES it — the go-to-definition target a consumer
/// was walking parent links to compute.
#[test]
fn fields_carry_their_declaring_origin_across_repos() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "card" }));
    let fields = &out[0]["fields"];

    assert_eq!(
        names(fields),
        vec!["pinned", "title"],
        "own field plus the inherited one, name-sorted: {fields}"
    );

    // The own field originates here.
    let pinned = field(fields, "pinned");
    assert_eq!(pinned["origin"]["name"], "card");
    assert_eq!(pinned["origin"]["repo"], "app");
    assert_eq!(pinned["shape"], "Boolean");
    assert_eq!(pinned["required"], true);

    // The inherited one originates in the PEER, which is the whole point: a
    // consumer jumping to the declaration lands in `base`, not in `app`.
    let title = field(fields, "title");
    assert_eq!(title["origin"]["name"], "note");
    assert_eq!(
        title["origin"]["repo"], "base",
        "the inherited field's origin is the peer that declares it: {title}"
    );
    assert_eq!(title["shape"], "String");
}

/// A diamond over two repos: every branch's fields land, each with its own
/// origin, and every ancestor is reported once.
#[test]
fn a_cross_repo_diamond_resolves_every_branch() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "both" }));
    let entry = &out[0];

    assert_eq!(
        names(&entry["ancestors"]),
        vec!["both", "card", "note", "tag"],
        "self, then both branches and the transitive peer parent"
    );

    let fields = &entry["fields"];
    assert_eq!(names(fields), vec!["extra", "label", "pinned", "title"]);
    assert_eq!(field(fields, "extra")["origin"]["repo"], "app");
    assert_eq!(field(fields, "pinned")["origin"]["name"], "card");
    assert_eq!(field(fields, "label")["origin"]["repo"], "base");
    assert_eq!(field(fields, "title")["origin"]["repo"], "base");
}

/// The field view carries what `types` carries — including `shape_ast` and the
/// docstring — so a consumer needs no second read to render a field.
#[test]
fn fields_carry_the_full_introspection_beside_the_origin() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "note::base" }));
    let title = field(&out[0]["fields"], "title");

    assert_eq!(title["shape"], "String");
    assert_eq!(title["shape_ast"]["kind"], "primitive");
    assert_eq!(title["shape_ast"]["name"], "String");
    assert_eq!(title["required"], true);
}

/// An unknown name is an EMPTY array, not an error and not a null — the
/// multi-fit rule's zero case, reached the same way as its one and N cases.
#[test]
fn an_unknown_name_is_an_empty_array() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    assert_eq!(closure(&mut h, json!({ "name": "nope" })), json!([]));
    // A known name in the wrong repo is equally empty, never base's copy.
    assert_eq!(
        closure(&mut h, json!({ "name": "card", "repo": "base" })),
        json!([])
    );
    // An unknown repo likewise.
    assert_eq!(
        closure(&mut h, json!({ "name": "note", "repo": "nope" })),
        json!([])
    );
}

/// The closure agrees with what the validator resolves: every effective field
/// is one the `type` read's own def declares, or an ancestor's.
#[test]
fn the_closure_agrees_with_the_type_read() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let out = closure(&mut h, json!({ "name": "card" }));
    let def = h.payload("type", json!({ "name": "card" }));

    assert_eq!(out[0]["identity"]["hash"], def["hash"]);
    assert_eq!(out[0]["identity"]["repo"], def["repo"]);

    // The def's OWN fields are a subset of the effective set.
    for f in def["fields"].as_array().unwrap() {
        let name = f["name"].as_str().unwrap();
        assert_eq!(
            field(&out[0]["fields"], name)["shape"],
            f["shape"],
            "own field {name} appears in the closure with the same shape"
        );
    }
}

/// An unknown arg is rejected rather than silently ignored, matching every
/// other read's strictness.
#[test]
fn an_unknown_arg_is_rejected() {
    let (_d, root) = cross_repo_kb();
    let mut h = harness(&root);

    let resp = h
        .client
        .query(&json!({ "read": "type_closure", "name": "card", "reop": "app" }))
        .expect("query");
    assert!(
        resp["result"].get("type_closure").is_none(),
        "a typo'd arg is an error frame, never a silently unscoped answer: {resp}"
    );
}
