//! Cross-repo brand slots: a brand demanded by `::repo` validates exactly like
//! an own-repo one. Nothing that works within a repo fails across repos.
//!
//! `sdk` owns brands (a nominal `icon-role` enum, a structural `evidence-kind`
//! union). `app` imports `sdk` and types slots by the peer brand's qualified
//! name. See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].

#![cfg(unix)]

use std::fs;
use std::path::Path;

use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn codes_on(kb: &au_engine::KnowledgeBase, file: &Path) -> Vec<String> {
    kb.diagnostics()
        .filter(|d| d.span.file == file)
        .map(|d| d.code.as_str().to_string())
        .collect()
}

/// `sdk` owns an `icon-role` nominal enum brand; `app` imports `sdk` and a
/// `command-meta` record types its `icon` slot by the peer brand.
fn nominal_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["sdk", "app"]);

    write(&root, "sdk/.arsumbris/repo.yaml", "name: sdk\n");
    write(
        &root,
        "sdk/type/icon-role.type.yaml",
        "#: semantic icon roles\nshape:\n  - save\n  - delete\n  - open\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: sdk\n",
    );
    write(
        &root,
        "app/type/command-meta.type.yaml",
        "fields:\n  icon: icon-role::sdk\n",
    );
    dir
}

#[test]
fn a_bare_member_validates_against_a_peer_nominal_brand() {
    let dir = nominal_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/save.md",
        "---\ntype: command-meta\nicon: save\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/save.md")).is_empty(),
        "a bare member of a peer enum brand validates: {:?}",
        codes_on(&kb, &root.join("app/save.md"))
    );
}

#[test]
fn a_qualified_constructor_validates_against_a_peer_nominal_brand() {
    let dir = nominal_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/open.md",
        "---\ntype: command-meta\nicon: icon-role::sdk(open)\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/open.md")).is_empty(),
        "a qualified constructor for a peer enum brand validates: {:?}",
        codes_on(&kb, &root.join("app/open.md"))
    );
}

#[test]
fn a_non_member_fails_a_peer_nominal_brand() {
    let dir = nominal_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/bad.md",
        "---\ntype: command-meta\nicon: nope\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/bad.md"))
            .iter()
            .any(|c| c == "field-shape-mismatch"),
        "a non-member of a peer enum brand is a mismatch, exactly as in-repo"
    );
}

#[test]
fn a_foreign_constructor_fails_a_peer_nominal_brand() {
    let dir = nominal_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/foreign.md",
        "---\ntype: command-meta\nicon: role(save)\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/foreign.md"))
            .iter()
            .any(|c| c == "brand-constructor-mismatch"),
        "a constructor naming a foreign brand fails, exactly as in-repo"
    );
}

#[test]
fn a_star_on_a_peer_nominal_brand_fires_brand_not_referenceable() {
    // A nominal peer brand is inline-only cross-repo, exactly as in-repo.
    let dir = nominal_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/type/command-meta.type.yaml",
        "fields:\n  icon: icon-role::sdk*\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/type/command-meta.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "brand-not-referenceable"),
        "a nominal peer brand cannot take '*' cross-repo: {codes:?}"
    );
}

/// `sdk` owns a `paper` / `observation` record pair and a structural
/// `evidence-kind` union over them, plus a `paper` instance to point at. `app`
/// imports `sdk` and types a `claim` slot by the peer union, inline and by
/// reference.
fn structural_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["sdk", "app"]);

    write(&root, "sdk/.arsumbris/repo.yaml", "name: sdk\n");
    write(&root, "sdk/type/paper.type.yaml", "fields:\n  t: String\n");
    write(
        &root,
        "sdk/type/observation.type.yaml",
        "fields:\n  t: String\n",
    );
    write(
        &root,
        "sdk/type/evidence-kind.type.yaml",
        "shape: <paper | observation>\n",
    );
    write(&root, "sdk/paper-a.md", "---\ntype: paper\nt: a\n---\n");

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: sdk\n",
    );
    write(
        &root,
        "app/type/claim.type.yaml",
        "fields:\n  inline_ev: evidence-kind::sdk\n  ref_ev: evidence-kind::sdk*\n",
    );
    dir
}

#[test]
fn an_inline_record_discriminates_a_peer_union_brand() {
    let dir = structural_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/c.md",
        "---\ntype: claim\ninline_ev:\n  type: paper::sdk\n  t: x\nref_ev: \"[[paper-a::sdk]]\"\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/c.md")).is_empty(),
        "a peer union brand accepts an inline member and a reference to a member: {:?}",
        codes_on(&kb, &root.join("app/c.md"))
    );
}

#[test]
fn a_star_on_a_peer_all_record_union_brand_is_referenceable() {
    // Every member of the peer union is a record, so `evidence-kind::sdk*` is
    // fine — no referenceability complaint on the type-def.
    let dir = structural_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/type/claim.type.yaml"));
    assert!(
        !codes.iter().any(|c| c == "brand-not-referenceable"),
        "an all-record peer union brand takes '*': {codes:?}"
    );
}

#[test]
fn a_star_on_a_mixed_member_peer_union_brand_fires_brand_not_referenceable() {
    // A peer union with a primitive member is inline-only cross-repo, exactly as
    // in-repo: `evidence-kind::sdk*` is brand-not-referenceable.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["sdk", "app"]);
    write(&root, "sdk/.arsumbris/repo.yaml", "name: sdk\n");
    write(
        &root,
        "sdk/type/evidence.type.yaml",
        "fields:\n  t: String\n",
    );
    write(
        &root,
        "sdk/type/evidence-kind.type.yaml",
        "shape: <evidence | String>\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: sdk\n",
    );
    write(
        &root,
        "app/type/claim.type.yaml",
        "fields:\n  e: evidence-kind::sdk*\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/type/claim.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "brand-not-referenceable"),
        "a mixed-member peer union brand cannot take '*' cross-repo: {codes:?}"
    );
}

#[test]
fn a_union_with_a_peer_nominal_member_accepts_that_members_constructor() {
    // code-review 3.4: `<meter::units | paper>` where `meter` is a peer nominal
    // brand. A `meter::units(42)` value must validate (the peer member is
    // resolved through the peer graph), exactly as an own `<meter | second>`
    // accepts `meter(42)`. Before the fix, the peer member was mis-classified as
    // a record and the value wrongly rejected.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["units", "app"]);
    write(&root, "units/.arsumbris/repo.yaml", "name: units\n");
    write(&root, "units/type/meter.type.yaml", "shape: Number\n");
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: units\n",
    );
    write(&root, "app/type/paper.type.yaml", "fields:\n  t: String\n");
    write(
        &root,
        "app/type/evidence.type.yaml",
        "shape: <meter::units | paper>\n",
    );
    write(
        &root,
        "app/type/claim.type.yaml",
        "fields:\n  e: evidence\n",
    );
    write(
        &root,
        "app/c.md",
        "---\ntype: claim\ne: \"meter::units(42)\"\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/c.md")).is_empty(),
        "a peer nominal member's constructor validates in a union: {:?}",
        codes_on(&kb, &root.join("app/c.md"))
    );
}

#[test]
fn a_typeless_inline_at_a_peer_union_brand_fires_brand_constructor_required() {
    let dir = structural_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    // inline_ev with no `type:` cannot pick a peer member; ref_ev is fine.
    write(
        &root,
        "app/c.md",
        "---\ntype: claim\ninline_ev:\n  t: x\nref_ev: \"[[paper-a::sdk]]\"\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/c.md"))
            .iter()
            .any(|c| c == "brand-constructor-required"),
        "a type:-less inline at a peer union brand needs a discriminator, as in-repo: {:?}",
        codes_on(&kb, &root.join("app/c.md"))
    );
}

/// `sdk` owns a `<String | label>` union brand where `label` is a scalar `String`
/// brand — the representationally-overlapping case, cross-repo. `app` types a
/// slot by the peer union.
fn overlap_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["sdk", "app"]);

    write(&root, "sdk/.arsumbris/repo.yaml", "name: sdk\n");
    write(&root, "sdk/type/label.type.yaml", "shape: String\n");
    write(
        &root,
        "sdk/type/stringy.type.yaml",
        "shape: <String | label>\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: sdk\n",
    );
    write(
        &root,
        "app/type/note.type.yaml",
        "fields:\n  v: stringy::sdk\n",
    );
    dir
}

#[test]
fn a_primitive_member_constructor_escapes_at_a_peer_union_brand() {
    // `String("looks(foo)")` at a peer `<String | label>` slot forces the plain
    // String branch — the literal escape works cross-repo.
    let dir = overlap_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/n.md",
        "---\ntype: note\nv: String(\"looks(foo)\")\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/n.md")).is_empty(),
        "the escape works cross-repo: {:?}",
        codes_on(&kb, &root.join("app/n.md"))
    );
}

#[test]
fn a_bare_value_at_a_peer_overlapping_union_is_ambiguous() {
    // A bare string at a peer `<String | label>` slot is ambiguous — the overlap
    // predicate classifies the peer members against the peer graph.
    let dir = overlap_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, "app/n.md", "---\ntype: note\nv: hi\n---\n");
    let kb = build(&root, &RealFileSystem).expect("build");
    assert!(
        codes_on(&kb, &root.join("app/n.md")).contains(&"brand-constructor-required".to_string()),
        "an overlapping bare value is ambiguous cross-repo: {:?}",
        codes_on(&kb, &root.join("app/n.md"))
    );
}
