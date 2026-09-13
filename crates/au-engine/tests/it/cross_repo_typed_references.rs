//! Cross-boundary typed-reference type-checking: a `[[name::repo]]` link in a
//! typed slot (`who: person::base*`) is verified by `(name, canonical-hash)`
//! identity against the target's effective type in the named repo, not just
//! existence.
//!
//! `app` imports `base`. A `person::base*` slot pointed at a `base` file is
//! satisfied only when that file's type closure includes base's `person`
//! identity.

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

/// Diagnostic `(code, severity)` pairs on one file.
fn codes_on(kb: &au_engine::KnowledgeBase, file: &Path) -> Vec<(String, String)> {
    kb.diagnostics()
        .filter(|d| d.span.file == file)
        .map(|d| (d.code.as_str().to_string(), format!("{:?}", d.severity)))
        .collect()
}

/// `base` owns `person` and `org` and a typed block. `app` imports `base` and
/// owns `card` with a `who: person::base*` slot. Each `app/card-*.md` points its
/// slot across the boundary at a different `base` target.
fn faithful_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/person.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/type/org.type.yaml",
        "fields:\n  name: String\n  sector: String\n",
    );
    write(
        &root,
        "base/type/note.type.yaml",
        "fields:\n  roster?: person&[]\nbody:\n  - section: People\n    fills: roster\n",
    );
    write(
        &root,
        "base/alice.md",
        "---\ntype: person\nname: Alice\n---\n",
    );
    write(
        &root,
        "base/acme.md",
        "---\ntype: org\nname: Acme\nsector: tech\n---\n",
    );
    // A plain note with no `type:` claim — a resolvable file that is not typed.
    write(&root, "base/plain.md", "Just prose, no frontmatter type.\n");
    // A typed block claiming `person`, addressable as `^bob`.
    write(
        &root,
        "base/people.md",
        "---\ntype: note\nroster:\n---\n\n# People\n\n```yaml [:roster]\ntype: person\nname: Bob\n```\n^bob\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // app imports base: card's who slot demands the peer type person::base.
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  who: person::base*\n",
    );

    // who resolves to a base `person` → identity matches → satisfied.
    write(
        &root,
        "app/card-ok.md",
        "---\ntype: card\nwho: \"[[alice::base]]\"\n---\n",
    );
    // who resolves to a base `org` → closure lacks `person` → mismatch.
    write(
        &root,
        "app/card-wrong.md",
        "---\ntype: card\nwho: \"[[acme::base]]\"\n---\n",
    );
    // who resolves to an untyped file → no `type:` claim → mismatch.
    write(
        &root,
        "app/card-plain.md",
        "---\ntype: card\nwho: \"[[plain::base]]\"\n---\n",
    );
    // who resolves to a `^^bob` block-referent claiming `person` → satisfied.
    write(
        &root,
        "app/card-block.md",
        "---\ntype: card\nwho: \"[[people::base^^bob]]\"\n---\n",
    );
    dir
}

#[test]
fn a_satisfying_cross_repo_typed_reference_emits_no_diagnostic() {
    let dir = faithful_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_on(&kb, &root.join("app/card-ok.md"));
    assert!(
        codes.is_empty(),
        "a `person*` slot pointed at a matching `person` is clean, got {codes:?}"
    );
}

#[test]
fn a_wrong_typed_cross_repo_reference_fires_target_type_mismatch() {
    let dir = faithful_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_on(&kb, &root.join("app/card-wrong.md"));
    assert_eq!(
        codes,
        vec![(
            "reference-target-type-mismatch".to_string(),
            "Error".to_string()
        )],
        "a `person*` slot pointed at an `org` is a cross-repo type mismatch"
    );
}

#[test]
fn an_untyped_cross_repo_target_fires_target_type_mismatch() {
    let dir = faithful_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    // Previously a silent pass: a `::repo` target with no `type:` claim was
    // never type-checked. Now it is a mismatch, like the repo-local case.
    let codes = codes_on(&kb, &root.join("app/card-plain.md"));
    assert_eq!(
        codes,
        vec![(
            "reference-target-type-mismatch".to_string(),
            "Error".to_string()
        )],
        "a `person*` slot pointed at an untyped file is a cross-repo type mismatch"
    );
}

#[test]
fn a_cross_repo_block_id_target_is_type_checked() {
    let dir = faithful_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    // `[[people::base^^bob]]` is a block-referent resolving to a typed block
    // claiming `person`, across the repo boundary — the slot is satisfied.
    let codes = codes_on(&kb, &root.join("app/card-block.md"));
    assert!(
        codes.is_empty(),
        "a `person*` slot pointed at a matching cross-repo `^block` is clean, got {codes:?}"
    );
}

/// `base` owns `person` and `org`. `app` imports `base` and owns `card` with a
/// `who?: person::base*` reference field that body wikilinks bind to via `:who`.
fn body_slot_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/person.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/type/org.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/alice.md",
        "---\ntype: person\nname: Alice\n---\n",
    );
    write(&root, "base/acme.md", "---\ntype: org\nname: Acme\n---\n");

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  who?: person::base*\n",
    );

    // A cross-repo body wikilink bound to `who`, pointed at a `base` person.
    write(
        &root,
        "app/card-ok.md",
        "---\ntype: card\nwho:\n---\n\nSee [[alice::base:who]].\n",
    );
    // …pointed at a `base` org → wrong type.
    write(
        &root,
        "app/card-wrong.md",
        "---\ntype: card\nwho:\n---\n\nSee [[acme::base:who]].\n",
    );
    dir
}

#[test]
fn a_satisfying_cross_repo_body_slot_reference_is_clean() {
    let dir = body_slot_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/card-ok.md"));
    assert!(
        codes.is_empty(),
        "a cross-repo body ref at a matching person is clean, got {codes:?}"
    );
}

#[test]
fn a_wrong_typed_cross_repo_body_slot_reference_fires_body_slot_shape_mismatch() {
    let dir = body_slot_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/card-wrong.md"));
    assert!(
        codes
            .iter()
            .any(|(c, s)| c == "body-slot-shape-mismatch" && s == "Error"),
        "a cross-repo body ref at an org does not satisfy a person* slot, got {codes:?}"
    );
}

/// Finding 2.1: a body-prose reference at a QUALIFIED demand (`who?: person::base*`).
/// `app` peers base and does NOT own `person`, so the pre-fix body path bailed on
/// the source graph lacking `person` and false-errored a valid `[[alice::base:who]]`.
fn qualified_body_slot_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/person.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/type/org.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/alice.md",
        "---\ntype: person\nname: Alice\n---\n",
    );
    write(&root, "base/acme.md", "---\ntype: org\nname: Acme\n---\n");
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  who?: person::base*\n",
    );
    write(
        &root,
        "app/card-ok.md",
        "---\ntype: card\nwho:\n---\n\nSee [[alice::base:who]].\n",
    );
    write(
        &root,
        "app/card-wrong.md",
        "---\ntype: card\nwho:\n---\n\nSee [[acme::base:who]].\n",
    );
    dir
}

#[test]
fn a_qualified_body_slot_reference_to_a_matching_peer_type_is_clean() {
    // Symptom B: valid `[[alice::base:who]]` at `person::base*` must NOT
    // false-error (the pre-fix path bailed on app not owning `person`).
    let dir = qualified_body_slot_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/card-ok.md"));
    assert!(
        !codes.iter().any(|(c, _)| c == "body-slot-shape-mismatch"),
        "a valid cross-repo body ref at the demanded peer type must not mismatch, got {codes:?}"
    );
}

#[test]
fn a_qualified_body_slot_reference_to_the_wrong_peer_type_mismatches() {
    let dir = qualified_body_slot_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/card-wrong.md"));
    assert!(
        codes
            .iter()
            .any(|(c, s)| c == "body-slot-shape-mismatch" && s == "Error"),
        "a body ref at a base org does not satisfy person::base*, got {codes:?}"
    );
}

/// Finding 2.1 symptom A: `app` owns its OWN `person` (structurally DIFFERENT from
/// base's, so a distinct identity) AND demands the peer `who?: person::base*`. A
/// body ref to the LOCAL app-person must mismatch — the pre-fix path matched by
/// bare NAME and silently accepted it.
fn shadowed_body_slot_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/person.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // A DIFFERENT shape than base's person, so a distinct `(name, closure-hash)`.
    write(
        &root,
        "app/type/person.type.yaml",
        "fields:\n  alias: String\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  who?: person::base*\n",
    );
    write(&root, "app/bob.md", "---\ntype: person\nalias: Bob\n---\n");
    write(
        &root,
        "app/card-shadow.md",
        "---\ntype: card\nwho:\n---\n\nSee [[bob:who]].\n",
    );
    dir
}

#[test]
fn a_qualified_body_slot_rejects_a_same_named_local_type() {
    // Symptom A: `[[bob:who]]` where bob is app's OWN (distinct) person must not
    // satisfy a `person::base*` demand — membership is by identity, not by name.
    let dir = shadowed_body_slot_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/card-shadow.md"));
    assert!(
        codes
            .iter()
            .any(|(c, s)| c == "body-slot-shape-mismatch" && s == "Error"),
        "an app-local person must not satisfy person::base*, got {codes:?}"
    );
}

#[test]
fn a_wrong_typed_cross_repo_reference_in_a_meta_body_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/person.type.yaml",
        "fields:\n  name: String\n",
    );
    write(
        &root,
        "base/type/org.type.yaml",
        "fields:\n  name: String\n",
    );
    write(&root, "base/acme.md", "---\ntype: org\nname: Acme\n---\n");

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // A meta-type with a `person::base*` reference field, used as a meta block
    // whose value points cross-repo at a `base` org → wrong type.
    write(
        &root,
        "app/type/stamp.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  by: person::base*\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  x?: String\nmeta:\n  - type: stamp\n    by: \"[[acme::base]]\"\n",
    );

    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_on(&kb, &root.join("app/type/card.type.yaml"));
    assert!(
        codes
            .iter()
            .any(|(c, s)| c == "reference-target-type-mismatch" && s == "Error"),
        "a cross-repo meta-body ref at a wrong type is a mismatch, got {codes:?}"
    );
}

#[test]
fn a_cross_repo_reference_into_an_aborted_target_repo_does_not_misfire() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    crate::seed_workspace(&root, &["base", "app"]);

    // base's `person.type.yaml` is unparseable: `person` is absent and base's
    // type graph aborts. The root cause is base's own load error.
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(&root, "base/type/person.type.yaml", "fields: [unclosed\n");
    write(
        &root,
        "base/alice.md",
        "---\ntype: person\nname: Alice\n---\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/card.type.yaml",
        "fields:\n  who?: person::base*\n",
    );
    write(
        &root,
        "app/card.md",
        "---\ntype: card\nwho: \"[[alice::base]]\"\n---\n",
    );

    let kb = build(&root, &RealFileSystem).expect("build");

    // The clean app is not blamed for base's broken vocabulary.
    let codes = codes_on(&kb, &root.join("app/card.md"));
    assert!(
        !codes
            .iter()
            .any(|(c, _)| c == "reference-target-type-mismatch"),
        "a ref into an aborted target repo must not misfire a mismatch, got {codes:?}"
    );
}

// ----- cross-repo def-references (`type<T>*`, [[type-def shape def-ref::au-type-system]]) -----

/// `base` owns `mcp.tool` and a subtype `mcp.tool.propose`, plus an unrelated
/// `other` def and a plain note. `app` imports `base` and owns `mode` with a
/// `propose_tool: type<mcp.tool::base>*` slot, pointed across the boundary by name.
fn def_ref_cross_repo_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/mcp.tool.type.yaml",
        "fields:\n  x?: String\n",
    );
    write(
        &root,
        "base/type/mcp.tool.propose.type.yaml",
        "extends: mcp.tool\n",
    );
    write(
        &root,
        "base/type/other.type.yaml",
        "fields:\n  y?: String\n",
    );
    write(&root, "base/plain.md", "Just prose, no type.\n");

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/mode.type.yaml",
        "fields:\n  propose_tool: type<mcp.tool::base>*\n",
    );

    // by type-name across the boundary, a def under mcp.tool → satisfied.
    write(
        &root,
        "app/mode-ok.md",
        "---\ntype: mode\npropose_tool: \"[[mcp.tool.propose::base]]\"\n---\n",
    );
    // a base def outside mcp.tool's subtree → closure mismatch.
    write(
        &root,
        "app/mode-wrong.md",
        "---\ntype: mode\npropose_tool: \"[[other::base]]\"\n---\n",
    );
    // a plain note → not a type-def.
    write(
        &root,
        "app/mode-plain.md",
        "---\ntype: mode\npropose_tool: \"[[plain::base]]\"\n---\n",
    );
    dir
}

#[test]
fn a_satisfying_cross_repo_def_reference_resolves_by_type_name_and_is_clean() {
    let dir = def_ref_cross_repo_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_on(&kb, &root.join("app/mode-ok.md"));
    assert!(
        codes.is_empty(),
        "a `type<mcp.tool>*` slot pointed by name at a base def under mcp.tool is clean, got {codes:?}"
    );

    // The cross-repo def-ref is a real backlink edge on the base def file,
    // resolved through the same name-aware per-repo index.
    let to_def = kb
        .backlinks
        .iter()
        .find(|(p, _)| {
            p.file_name()
                .map(|n| n == "mcp.tool.propose.type.yaml")
                .unwrap_or(false)
        })
        .map(|(_, edges)| edges.as_slice())
        .unwrap_or(&[]);
    assert!(
        to_def.iter().any(|e| e
            .source
            .file_name()
            .map(|n| n == "mode-ok.md")
            .unwrap_or(false)),
        "the base def has an inbound edge from app/mode-ok.md, got {to_def:?}"
    );
}

#[test]
fn a_cross_repo_def_reference_outside_the_bound_fires_closure_mismatch() {
    let dir = def_ref_cross_repo_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_on(&kb, &root.join("app/mode-wrong.md"));
    assert!(
        codes.iter().any(|(c, _)| c == "def-ref-closure-mismatch"),
        "a base def outside mcp.tool's subtree is a closure mismatch, got {codes:?}"
    );
}

#[test]
fn a_cross_repo_def_reference_to_a_note_fires_target_not_a_type_def() {
    let dir = def_ref_cross_repo_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_on(&kb, &root.join("app/mode-plain.md"));
    assert!(
        codes
            .iter()
            .any(|(c, _)| c == "def-ref-target-not-a-type-def"),
        "a plain note across the boundary is not a type-def, got {codes:?}"
    );
}

/// `base` owns `research-extraction` (whose `concepts?: concept-candidate&[]`
/// slot pins the peer type), `concept-candidate`, and an unrelated `other-thing`.
/// `app` imports `base`, holds an extraction with a slot-pinned candidate carrying
/// `^: c1`, and two citations referencing that block with `^^`: one whose slot
/// demands `concept-candidate::base*` (satisfied), one demanding `other-thing::base*`
/// (a mismatch). The `^^` target's identity must resolve owner-relative, so the
/// satisfied case type-checks cleanly AND the mismatch is actually caught — a
/// slot-pinned peer record with no resolved identity would silently skip both.
fn cross_repo_block_referent_fixture() -> tempfile::TempDir {
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
        "base/type/other-thing.type.yaml",
        "fields:\n  x?: String\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/citation.type.yaml",
        "fields:\n  evidence: concept-candidate::base*\n",
    );
    write(
        &root,
        "app/type/misfiled.type.yaml",
        "fields:\n  evidence: other-thing::base*\n",
    );
    // The candidate omits `type:`, slot-pinned; it carries a `^: c1` block-id.
    write(
        &root,
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - ^: c1\n    salience: focal\n---\n",
    );
    // `^^` block-referent, satisfied: c1 is a concept-candidate.
    write(
        &root,
        "app/cite.md",
        "---\ntype: citation\nevidence: \"[[extraction^^c1]]\"\n---\n",
    );
    // `^^` block-referent, mismatch: c1 is not an other-thing.
    write(
        &root,
        "app/misfile.md",
        "---\ntype: misfiled\nevidence: \"[[extraction^^c1]]\"\n---\n",
    );
    dir
}

#[test]
fn a_block_referent_into_a_slot_pinned_peer_record_type_checks() {
    let dir = cross_repo_block_referent_fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    // Satisfied: c1 resolves owner-relative to `concept-candidate::base`, which
    // satisfies the `concept-candidate::base*` slot. No reference error.
    let ok = codes_on(&kb, &root.join("app/cite.md"));
    assert!(
        ok.iter().all(|(_, sev)| sev != "Error"),
        "the satisfied block-referent must not error, got {ok:?}"
    );

    // Mismatch: c1 is a concept-candidate, not an other-thing. The type check
    // must actually RUN (a claim-less record would silently skip it, so a wrong
    // reference would pass unchecked — the type-safety loss this fixes).
    let bad = codes_on(&kb, &root.join("app/misfile.md"));
    assert!(
        bad.iter()
            .any(|(c, _)| c == "reference-target-type-mismatch"),
        "a wrong-typed block-referent must be caught, got {bad:?}"
    );
}
