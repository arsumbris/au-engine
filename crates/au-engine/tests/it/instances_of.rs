//! The `instances_of` origin taxonomy, end to end over `build`: file, nested
//! inline record, and type-def meta instances, tagged by origin with a span and
//! an origin-specific locator.

#![cfg(unix)]

use std::fs;
use std::path::Path;

use au_engine::build;
use au_engine::wire::{introspect_instances_of, Locator, Origin, PathSeg};
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A knowledge base modelling a typed plan (nested `phase` / `action` records), a
/// file-level `note`, and a `note` type-def carrying a `display-meta` block.
fn kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: test\n");

    write(&root, "type/plan.type.yaml", "fields:\n  phases: phase[]\n");
    write(
        &root,
        "type/phase.type.yaml",
        "fields:\n  actions: action[+]\n",
    );
    write(&root, "type/action.type.yaml", "fields:\n  desc: String\n");
    write(&root, "type/action.open.type.yaml", "extends: action\n");
    write(
        &root,
        "type/display-meta.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  icon?: String\n",
    );
    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  body: String\nmeta:\n  - type: display-meta\n    icon: N\n",
    );

    // A plan with two nested open actions, the second block-id'd.
    write(
        &root,
        "plan1.md",
        "---\ntype: plan\nphases:\n  - type: phase\n    actions:\n      - type: action.open\n        desc: a\n      - ^: act2\n        type: action.open\n        desc: b\n---\n",
    );
    // A file-level note instance.
    write(&root, "note1.md", "---\ntype: note\nbody: hi\n---\n");
    (dir, root)
}

#[test]
fn nested_actions_are_enumerated_with_paths_and_block_ids() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");
    let recs = introspect_instances_of(&v, "action.open", None);

    assert_eq!(recs.len(), 2, "two open actions, got {recs:?}");
    assert!(recs.iter().all(|r| r.origin == Origin::Nested));
    assert!(recs.iter().all(|r| r.claimed && !r.inherited));
    assert!(recs.iter().all(|r| r.name == "action.open"));
    assert!(recs
        .iter()
        .all(|r| r.path.ends_with("plan1.md") && r.span.end > r.span.start));

    // The record's own body rides `fields`.
    assert_eq!(recs[0].fields.get("desc").unwrap(), "a");

    let locators: Vec<&Locator> = recs.iter().filter_map(|r| r.locator.as_ref()).collect();
    assert_eq!(
        locators[0],
        &Locator::Nested {
            field_path: vec![
                PathSeg::Field("phases".into()),
                PathSeg::Index(0),
                PathSeg::Field("actions".into()),
                PathSeg::Index(0),
            ],
            block_id: None,
        }
    );
    assert_eq!(
        locators[1],
        &Locator::Nested {
            field_path: vec![
                PathSeg::Field("phases".into()),
                PathSeg::Index(0),
                PathSeg::Field("actions".into()),
                PathSeg::Index(1),
            ],
            block_id: Some("act2".into()),
        }
    );
}

#[test]
fn a_nested_record_matches_its_parent_as_inherited() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");
    // `action.open` extends `action`, so a bare `action` query matches each open
    // action by inheritance, not claim.
    let recs = introspect_instances_of(&v, "action", None);
    assert_eq!(recs.len(), 2, "got {recs:?}");
    assert!(recs
        .iter()
        .all(|r| r.origin == Origin::Nested && !r.claimed && r.inherited && r.name == "action"));
}

#[test]
fn a_meta_block_is_enumerated_by_its_semantic_key() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");
    let recs = introspect_instances_of(&v, "display-meta", None);

    assert_eq!(recs.len(), 1, "one display-meta block, got {recs:?}");
    let r = &recs[0];
    assert_eq!(r.origin, Origin::Meta);
    assert!(r.path.ends_with("note.type.yaml"));
    assert_eq!(r.claim, vec!["display-meta".to_string()]);
    assert_eq!(r.fields.get("icon").unwrap(), "N");
    assert_eq!(
        r.locator,
        Some(Locator::Meta {
            meta_type: "display-meta".into(),
            repo: None,
        })
    );
    assert!(r.span.end > r.span.start);
}

#[test]
fn origins_filter_file_reproduces_the_file_only_stream() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");

    // A file-only query on `note` returns the file instance, tagged `file` with
    // a null locator.
    let notes = introspect_instances_of(&v, "note", Some(&[Origin::File]));
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].origin, Origin::File);
    assert!(notes[0].path.ends_with("note1.md"));
    assert!(notes[0].locator.is_none());
    assert!(notes[0].span.end > notes[0].span.start);

    // A file-only query on a nested-only type returns nothing.
    let actions = introspect_instances_of(&v, "action.open", Some(&[Origin::File]));
    assert!(actions.is_empty(), "no file-level action.open: {actions:?}");
}

#[test]
fn default_returns_all_origins_but_a_scoped_filter_narrows() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");

    // Default (no filter) still returns the nested actions.
    let all = introspect_instances_of(&v, "action.open", None);
    let nested_only = introspect_instances_of(&v, "action.open", Some(&[Origin::Nested]));
    assert_eq!(all.len(), nested_only.len());

    // Excluding nested drops them entirely.
    let no_nested = introspect_instances_of(&v, "action.open", Some(&[Origin::File, Origin::Meta]));
    assert!(no_nested.is_empty(), "got {no_nested:?}");
}

/// `base` owns `note` (with a `detail?: any` slot) and `dm`; `app` peers `base`
/// and holds a `note::base` instance carrying a nested `dm::base` record.
fn cross_repo_kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/note.type.yaml",
        "fields:\n  detail?: any\n",
    );
    write(&root, "base/type/dm.type.yaml", "fields: {}\n");
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/card.md",
        "---\ntype: note::base\ndetail:\n  ^: d1\n  type: dm::base\n---\n",
    );
    (dir, root)
}

#[test]
fn a_qualified_query_resolves_a_nested_cross_repo_record() {
    let (_dir, root) = cross_repo_kb();
    let v = build(&root, &RealFileSystem).expect("build");
    // `dm::base` scopes to the one identity `base` owns; the match is app's
    // nested record, owned-by-base, claimed via the cross-repo fold.
    let recs = introspect_instances_of(&v, "dm::base", None);
    assert_eq!(recs.len(), 1, "got {recs:?}");
    let r = &recs[0];
    assert_eq!(r.origin, Origin::Nested);
    assert_eq!(r.name, "dm");
    assert_eq!(r.type_owners, vec!["base".to_string()]);
    // `member` owns the FILE (app), distinct from `type_owners` (base owns the
    // type). The asymmetry the field exists to make legible.
    assert_eq!(r.member, "app");
    assert!(r.claimed && !r.inherited);
    assert_eq!(r.claim, vec!["dm::base".to_string()]);
    assert!(r.path.ends_with("card.md"));
    assert_eq!(
        r.locator,
        Some(Locator::Nested {
            field_path: vec![PathSeg::Field("detail".into())],
            block_id: Some("d1".into()),
        })
    );
}

/// `base` owns `research-extraction` (whose `concepts?: concept-candidate&[]`
/// slot pins the peer type `concept-candidate`) and `concept-candidate`; `app`
/// peers `base` and holds a `research-extraction::base` instance whose nested
/// candidate record omits its own `type:`, so it is slot-pinned to the peer type.
fn cross_repo_slot_pinned_kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    write(
        &root,
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // The inline candidate omits `type:`, the slot `concept-candidate&[]` pins it.
    write(
        &root,
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - salience: focal\n---\n",
    );
    (dir, root)
}

#[test]
fn a_slot_pinned_nested_cross_repo_record_is_discovered() {
    let (_dir, root) = cross_repo_slot_pinned_kb();
    let v = build(&root, &RealFileSystem).expect("build");

    // The reported bug: the slot pins the peer type, so the inline record omits
    // `type:`. It must still be discovered as a `concept-candidate::base`
    // instance, resolved owner-relative, exactly as validation resolves it.
    let demanded = introspect_instances_of(&v, "concept-candidate::base", Some(&[Origin::Nested]));
    assert_eq!(demanded.len(), 1, "demanded query, got {demanded:?}");
    let r = &demanded[0];
    assert_eq!(r.origin, Origin::Nested);
    assert_eq!(r.name, "concept-candidate");
    assert_eq!(r.type_owners, vec!["base".to_string()]);
    assert_eq!(r.member, "app");
    assert!(r.claimed && !r.inherited);
    assert_eq!(r.claim, vec!["concept-candidate::base".to_string()]);
    assert!(r.path.ends_with("extraction.md"));

    // The bare (name-conflating) query finds it too.
    let bare = introspect_instances_of(&v, "concept-candidate", Some(&[Origin::Nested]));
    assert_eq!(bare.len(), 1, "bare query, got {bare:?}");
    assert_eq!(bare[0].name, "concept-candidate");
}

/// Mixin variant of `cross_repo_slot_pinned_kb` (coverage gap #1): the host claims
/// TWO peer types, `[tag::base, research-extraction::base]`, so the top-level fold
/// seeds across MULTIPLE `::repo` claims. The `concepts` slot comes from the SECOND
/// mixed-in claim, so a first-claim-only seeding would drop it; a claim-less record
/// in the slot must still pin owner-relative to `concept-candidate::base`.
fn cross_repo_mixin_host_kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    write(
        &root,
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n",
    );
    write(&root, "base/type/tag.type.yaml", "fields: {}\n");
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // A mixin host, slot-owner (`research-extraction`) listed SECOND; the candidate
    // omits `type:`, the `concepts` slot pins it.
    write(
        &root,
        "app/extraction.md",
        "---\ntype:\n  - tag::base\n  - research-extraction::base\nconcepts:\n  - salience: focal\n---\n",
    );
    (dir, root)
}

#[test]
fn a_slot_pinned_record_under_a_cross_repo_mixin_host_is_discovered() {
    let (_dir, root) = cross_repo_mixin_host_kb();
    let v = build(&root, &RealFileSystem).expect("build");

    // The host mixes two peer claims; the pinning slot comes from the second. The
    // fold must seed across BOTH `::repo` claims for the slot to resolve, so the
    // claim-less nested record pins owner-relative to `concept-candidate::base`.
    let demanded = introspect_instances_of(&v, "concept-candidate::base", Some(&[Origin::Nested]));
    assert_eq!(
        demanded.len(),
        1,
        "mixin host, demanded query, got {demanded:?}"
    );
    assert_eq!(demanded[0].name, "concept-candidate");
    assert_eq!(demanded[0].type_owners, vec!["base".to_string()]);
    assert_eq!(
        demanded[0].claim,
        vec!["concept-candidate::base".to_string()]
    );
    assert!(demanded[0].path.ends_with("extraction.md"));
}

/// Depth variant of `cross_repo_slot_pinned_kb`: `base` also owns `evidence-item`,
/// and `concept-candidate` has an `evidence?: evidence-item&[]` slot. `app`'s
/// extraction nests a claim-less `evidence-item` inside a claim-less
/// `concept-candidate`, so BOTH pins are cross-repo and slot-pinned, one inside
/// the other.
fn cross_repo_deep_slot_pinned_kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    write(
        &root,
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n  evidence?: evidence-item&[]\n",
    );
    write(
        &root,
        "base/type/evidence-item.type.yaml",
        "fields:\n  note?: String\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - salience: focal\n    evidence:\n      - note: seen here\n---\n",
    );
    (dir, root)
}

#[test]
fn a_deeper_slot_pinned_peer_record_is_discovered() {
    let (_dir, root) = cross_repo_deep_slot_pinned_kb();
    let v = build(&root, &RealFileSystem).expect("build");

    // A claim-less `evidence-item` nested inside a claim-less `concept-candidate`,
    // both slot-pinned to peer types. Full parity: it must be discovered too.
    let demanded = introspect_instances_of(&v, "evidence-item::base", Some(&[Origin::Nested]));
    assert_eq!(demanded.len(), 1, "deeper record, got {demanded:?}");
    assert_eq!(demanded[0].name, "evidence-item");
    assert_eq!(demanded[0].type_owners, vec!["base".to_string()]);
}

/// Three parent families, each with two subtypes and a file instance per
/// subtype. `ruling` is SEALED (non-claimable, closed leaf set), `figure` is
/// ABSTRACT (non-claimable, open), `memo` is a PLAIN concrete parent. Every
/// subtype declares its parent via `type:`, so the parent sits in each
/// instance's ancestor closure. `memo` additionally has one instance claiming
/// the parent directly, the concrete-parent case sealed/abstract can't have.
fn family_kb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: test\n");

    // Sealed family: parent non-claimable, leaf set closed.
    write(
        &root,
        "type/ruling.type.yaml",
        "fields:\n  summary: String\nsealed:\n  - ruling.affirm\n  - ruling.reverse\n",
    );
    write(&root, "type/ruling.affirm.type.yaml", "extends: ruling\n");
    write(&root, "type/ruling.reverse.type.yaml", "extends: ruling\n");

    // Abstract family: parent non-claimable, open.
    write(
        &root,
        "type/figure.type.yaml",
        "abstract: true\nfields:\n  area: Number\n",
    );
    write(&root, "type/figure.circle.type.yaml", "extends: figure\n");
    write(&root, "type/figure.square.type.yaml", "extends: figure\n");

    // Plain family: parent concrete, directly claimable.
    write(&root, "type/memo.type.yaml", "fields:\n  body: String\n");
    write(&root, "type/memo.brief.type.yaml", "extends: memo\n");
    write(&root, "type/memo.long.type.yaml", "extends: memo\n");

    // One instance per subtype, claiming the leaf.
    write(
        &root,
        "ruling1.md",
        "---\ntype: ruling.affirm\nsummary: a\n---\n",
    );
    write(
        &root,
        "ruling2.md",
        "---\ntype: ruling.reverse\nsummary: b\n---\n",
    );
    write(
        &root,
        "figure1.md",
        "---\ntype: figure.circle\narea: 1\n---\n",
    );
    write(
        &root,
        "figure2.md",
        "---\ntype: figure.square\narea: 2\n---\n",
    );
    write(&root, "memo1.md", "---\ntype: memo.brief\nbody: a\n---\n");
    write(&root, "memo2.md", "---\ntype: memo.long\nbody: b\n---\n");
    // The concrete parent, claimed directly — legal only for the plain family.
    write(&root, "memo3.md", "---\ntype: memo\nbody: c\n---\n");

    (dir, root)
}

#[test]
fn sealed_parent_collects_its_leaf_instances_as_inherited() {
    let (_dir, root) = family_kb();
    let v = build(&root, &RealFileSystem).expect("build");
    // A sealed parent is non-claimable, so no instance claims `ruling` directly;
    // every match is a leaf instance reached by inheritance.
    let recs = introspect_instances_of(&v, "ruling", Some(&[Origin::File]));
    assert_eq!(recs.len(), 2, "two leaf instances, got {recs:?}");
    assert!(recs
        .iter()
        .all(|r| r.origin == Origin::File && r.name == "ruling" && !r.claimed && r.inherited));
    let mut paths: Vec<&str> = recs.iter().map(|r| r.path.as_str()).collect();
    paths.sort();
    assert!(paths[0].ends_with("ruling1.md") && paths[1].ends_with("ruling2.md"));
}

#[test]
fn abstract_parent_collects_its_subtype_instances_as_inherited() {
    let (_dir, root) = family_kb();
    let v = build(&root, &RealFileSystem).expect("build");
    // An abstract parent is also non-claimable; both subtype instances match by
    // inheritance, none by direct claim.
    let recs = introspect_instances_of(&v, "figure", Some(&[Origin::File]));
    assert_eq!(recs.len(), 2, "two subtype instances, got {recs:?}");
    assert!(recs
        .iter()
        .all(|r| r.origin == Origin::File && r.name == "figure" && !r.claimed && r.inherited));
    let mut paths: Vec<&str> = recs.iter().map(|r| r.path.as_str()).collect();
    paths.sort();
    assert!(paths[0].ends_with("figure1.md") && paths[1].ends_with("figure2.md"));
}

#[test]
fn plain_parent_collects_subtypes_inherited_and_its_own_claim() {
    let (_dir, root) = family_kb();
    let v = build(&root, &RealFileSystem).expect("build");
    // A concrete parent: the two subtype instances come back inherited, and the
    // instance claiming `memo` directly comes back claimed. Both flags exercised.
    let recs = introspect_instances_of(&v, "memo", Some(&[Origin::File]));
    assert_eq!(
        recs.len(),
        3,
        "two subtypes plus the direct claim, got {recs:?}"
    );
    assert!(recs
        .iter()
        .all(|r| r.origin == Origin::File && r.name == "memo"));

    let claimed: Vec<&str> = recs
        .iter()
        .filter(|r| r.claimed && !r.inherited)
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(claimed.len(), 1, "one direct claim");
    assert!(claimed[0].ends_with("memo3.md"));

    let mut inherited: Vec<&str> = recs
        .iter()
        .filter(|r| r.inherited && !r.claimed)
        .map(|r| r.path.as_str())
        .collect();
    inherited.sort();
    assert_eq!(inherited.len(), 2, "two inherited subtype instances");
    assert!(inherited[0].ends_with("memo1.md") && inherited[1].ends_with("memo2.md"));
}

#[test]
fn the_wire_shape_serializes_as_specified() {
    let (_dir, root) = kb();
    let v = build(&root, &RealFileSystem).expect("build");

    let nested = introspect_instances_of(&v, "action.open", Some(&[Origin::Nested]));
    let j = serde_json::to_value(&nested[0]).unwrap();
    assert_eq!(j["origin"], "nested");
    assert_eq!(j["locator"]["kind"], "nested");
    // The field_path is a mixed array of names and indices.
    assert_eq!(
        j["locator"]["field_path"],
        serde_json::json!(["phases", 0, "actions", 0])
    );
    assert!(j["span"]["start"].is_number() && j["span"]["end"].is_number());

    let meta = introspect_instances_of(&v, "display-meta", Some(&[Origin::Meta]));
    let jm = serde_json::to_value(&meta[0]).unwrap();
    assert_eq!(jm["origin"], "meta");
    assert_eq!(jm["locator"]["kind"], "meta");
    assert_eq!(jm["locator"]["meta_type"], "display-meta");

    // A file match carries a null locator.
    let file = introspect_instances_of(&v, "note", Some(&[Origin::File]));
    let jf = serde_json::to_value(&file[0]).unwrap();
    assert_eq!(jf["origin"], "file");
    assert!(jf["locator"].is_null());
}

/// `#:` docstrings surface as `doc` (record head) and `field_docs` (per field)
/// on `file`, `nested`, and `meta` matches alike, the value-surface twin of the
/// schema read's type-def docstrings.
#[test]
fn instance_docstrings_surface_on_file_nested_and_meta() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: test\n");
    write(&root, "type/step.type.yaml", "fields:\n  gate: String\n");
    write(&root, "type/flow.type.yaml", "fields:\n  steps: step[]\n");
    write(
        &root,
        "type/display-meta.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  icon?: String\n",
    );
    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  text: String\nmeta:\n  #: how it renders\n  - type: display-meta\n    icon: N        #: the glyph\n",
    );

    // A file instance with a head doc and a field doc, holding a nested step
    // record that also carries a head doc and a field doc.
    write(
        &root,
        "flow1.md",
        "---\n#: the release flow\ntype: flow\nsteps:\n  #: build the image\n  - type: step\n    gate: manual   #: reviewer confirms\n---\n",
    );
    write(
        &root,
        "note1.md",
        "---\n#: this note\ntype: note\ntext: hi        #: the text\n---\n",
    );

    let v = build(&root, &RealFileSystem).expect("build");

    // file origin: head + field doc.
    let notes = introspect_instances_of(&v, "note", Some(&[Origin::File]));
    let n = notes
        .iter()
        .find(|r| r.path.ends_with("note1.md"))
        .expect("note match");
    assert_eq!(n.doc.as_deref(), Some("this note"));
    assert_eq!(
        n.field_docs.get("text").map(String::as_str),
        Some("the text")
    );

    // nested origin: the step record's head + field doc.
    let steps = introspect_instances_of(&v, "step", Some(&[Origin::Nested]));
    assert_eq!(steps.len(), 1, "one step, got {steps:?}");
    assert_eq!(steps[0].doc.as_deref(), Some("build the image"));
    assert_eq!(
        steps[0].field_docs.get("gate").map(String::as_str),
        Some("reviewer confirms")
    );

    // meta origin: the block's head + field doc.
    let metas = introspect_instances_of(&v, "display-meta", Some(&[Origin::Meta]));
    assert_eq!(metas.len(), 1, "one meta block, got {metas:?}");
    assert_eq!(metas[0].doc.as_deref(), Some("how it renders"));
    assert_eq!(
        metas[0].field_docs.get("icon").map(String::as_str),
        Some("the glyph")
    );

    // The flow file carries a head doc but documents no top-level field, so its
    // empty `field_docs` is OMITTED from the JSON, never an empty object.
    let flow = introspect_instances_of(&v, "flow", Some(&[Origin::File]));
    assert_eq!(flow[0].doc.as_deref(), Some("the release flow"));
    let jflow = serde_json::to_value(&flow[0]).unwrap();
    assert_eq!(jflow["doc"], "the release flow");
    assert!(
        jflow.get("field_docs").is_none(),
        "empty field_docs must be omitted, got {jflow}"
    );
}
