//! The hardwired `au.engine.*` schema defs are owned by a compiled-in
//! `au-engine` repo identity, resolvable from any knowledge base via `::au-engine`
//! without declaring it as a peer — a universal peer, the engine's slice of
//! the layered-ownership pattern.

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

fn codes_for(kb: &au_engine::KnowledgeBase, file: &Path) -> Vec<String> {
    kb.diagnostics()
        .filter(|d| d.span.file == file)
        .map(|d| d.code.as_str().to_string())
        .collect()
}

/// A knowledge base repo that never declares `au-engine` as a peer can still name an
/// engine-schema type via `::au-engine`: the builtin repo is present in every
/// build, so the gate resolves it, and its graph holds the hardwired def.
#[test]
fn a_kb_resolves_a_hardwired_type_via_the_au_engine_peer() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // No `deps:` — `::au-engine` is a universal peer, not a declared one.
    write(&root, ".arsumbris/repo.yaml", "name: myrepo\n");
    // A def extending a hardwired engine type by its qualified name.
    write(
        &root,
        "type/thing.type.yaml",
        "extends: au.engine.repo::au-engine\nfields:\n  extra: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_for(&kb, &root.join("type/thing.type.yaml"));
    // The peer resolves (present) and holds `au.engine.repo`: no gate diagnostic.
    assert!(
        !codes.iter().any(|c| c == "type-repo-unknown"),
        "au-engine should be a known peer; codes: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "peer-type-not-found"),
        "au.engine.repo should resolve in the builtin peer; codes: {codes:?}"
    );
}

/// The builtin peer is present, but its graph holds ONLY the hardwired set:
/// a name the engine does not define resolves the repo yet misses the type,
/// `peer-type-not-found`, never `type-repo-unknown`. This proves the graph is
/// really the hardwired defs, not an empty stand-in.
#[test]
fn an_undefined_engine_name_is_peer_type_not_found_not_repo_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: myrepo\n");
    write(
        &root,
        "type/badref.type.yaml",
        "extends: au.engine.nope::au-engine\nfields:\n  extra: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes = codes_for(&kb, &root.join("type/badref.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "peer-type-not-found"),
        "an undefined au.engine.* name should be peer-type-not-found; codes: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "type-repo-unknown"),
        "the au-engine repo itself is known; codes: {codes:?}"
    );
}

/// The in-repo engine files are typed by kind (no written `type:`), so they are
/// first-class instance nodes queryable via `instances_of`.
#[test]
fn engine_schema_files_are_typed_instance_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, "proj/.arsumbris/repo.yaml", "name: proj\n");
    write(&root, "proj/type/thing.type.yaml", "fields:\n  x: String\n");
    write(&root, "proj/.arsumbris/workspace.yaml", "edit:\n  - proj\n");
    let kb = build(&root.join("proj"), &RealFileSystem).expect("build");

    let repos = au_engine::wire::introspect_instances_of(
        &kb,
        "au.engine.repo",
        Some(&[au_engine::wire::Origin::File]),
    );
    assert!(
        repos
            .iter()
            .any(|m| m.path.ends_with("proj/.arsumbris/repo.yaml")),
        "repo.yaml is an au.engine.repo instance node: {:?}",
        repos.iter().map(|m| m.path.clone()).collect::<Vec<_>>()
    );
    let wss = au_engine::wire::introspect_instances_of(
        &kb,
        "au.engine.workspace",
        Some(&[au_engine::wire::Origin::File]),
    );
    assert!(
        wss.iter()
            .any(|m| m.path.ends_with(".arsumbris/workspace.yaml")),
        "the workspace file is an au.engine.workspace instance node: {:?}",
        wss.iter().map(|m| m.path.clone()).collect::<Vec<_>>()
    );
}

/// A knowledge base def named like a hardwired engine type COEXISTS with the builtin as a
/// distinct identity: `engine-name-live-shadow` notes the shared name, and there
/// is NO `duplicate-type-def` (the builtin lives in the `au-engine` repo, the
/// knowledge base def in its own), so the knowledge base's def is never annihilated.
#[test]
fn a_repo_def_named_like_a_hardwired_type_is_live_shadow_and_coexists() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    write(
        &root,
        "type/au.engine.repo.type.yaml",
        "fields:\n  x: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("type/au.engine.repo.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "engine-name-live-shadow"),
        "a knowledge base def sharing a hardwired name is live-shadow; codes: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "duplicate-type-def"),
        "coexistence, not annihilation: the two are distinct identities; codes: {codes:?}"
    );
}

/// A shadow whose fields DIVERGE from the engine's copy escalates to
/// `engine-name-shadow-drift` (drift) alongside the base `live-shadow` note, the
/// `(name, closure-hash)` comparison against the hardwired identity.
#[test]
fn a_diverged_shadow_also_fires_shadow_drift() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    // `au.engine.repo` with different fields than the engine's copy → diverged.
    write(
        &root,
        "type/au.engine.repo.type.yaml",
        "fields:\n  x: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("type/au.engine.repo.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "engine-name-live-shadow"),
        "the base coexistence note still fires; codes: {codes:?}"
    );
    assert!(
        codes.iter().any(|c| c == "engine-name-shadow-drift"),
        "a diverged shadow escalates to shadow-drift; codes: {codes:?}"
    );
}

/// An IN-SYNC shadow (fields identical to the engine's copy, so equal
/// closure-hash) fires only the base `live-shadow` note, never `shadow-drift`.
#[test]
fn an_in_sync_shadow_fires_no_drift() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    // `au.engine.workspace`'s hardwired shape is exactly `edit?` / `discover?` /
    // `disabled?`. An in-sync shadow must mirror all three.
    write(
        &root,
        "type/au.engine.workspace.type.yaml",
        "fields:\n  edit?: String[]\n  discover?: String[]\n  disabled?: String[]\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("type/au.engine.workspace.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "engine-name-live-shadow"),
        "the coexistence note fires for an in-sync shadow too; codes: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "engine-name-shadow-drift"),
        "an in-sync shadow does NOT drift; codes: {codes:?}"
    );
}

/// A knowledge base def in the reserved `au.engine.*` namespace the engine does not
/// hardwire carries the forward-reservation hint.
#[test]
fn a_repo_def_in_the_reserved_namespace_is_forward_reserved() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    write(
        &root,
        "type/au.engine.widget.type.yaml",
        "fields:\n  x: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("type/au.engine.widget.type.yaml"));
    assert!(
        codes.iter().any(|c| c == "engine-name-forward-reserved"),
        "a reserved-but-unhardwired name is forward-reserved; codes: {codes:?}"
    );
}

/// The engine-written `.arsumbris/repo.lock` is a typed instance node too: read
/// past the walk floor by targeted path and catalogued as `au.engine.repo-lock`,
/// so it is `instances_of`-queryable.
#[test]
fn the_arsumbris_locks_are_typed_instance_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, "proj/.arsumbris/repo.yaml", "name: proj\n");
    write(&root, "proj/type/thing.type.yaml", "fields:\n  x: String\n");
    // A well-formed engine-written lock self-describes with its written type, so
    // it neither drifts (`engine-schema-type-unwritten`) nor errors.
    write(
        &root,
        "proj/.arsumbris/repo.lock",
        "type: au.engine.repo-lock::au-engine\npackages:\n  - name: library\n    remote: git@x\n    sha: 9f3a1c2e\n",
    );
    let kb = build(&root.join("proj"), &RealFileSystem).expect("build");

    let repo_locks = au_engine::wire::introspect_instances_of(
        &kb,
        "au.engine.repo-lock",
        Some(&[au_engine::wire::Origin::File]),
    );
    assert!(
        repo_locks
            .iter()
            .any(|m| m.path.ends_with("proj/.arsumbris/repo.lock")),
        "repo.lock is an au.engine.repo-lock node: {:?}",
        repo_locks
            .iter()
            .map(|m| m.path.clone())
            .collect::<Vec<_>>()
    );
    // Well-formed locks validate clean against their defs.
    assert!(
        codes_for(&kb, &root.join("proj/.arsumbris/repo.lock")).is_empty(),
        "a well-formed repo.lock has no diagnostics"
    );
}

/// A corrupt lock is VALIDATED against its def like any node: a locked-package
/// missing its required `sha` is flagged, so a hand-broken lock surfaces a typed
/// diagnostic instead of a silent bad node.
#[test]
fn a_corrupt_repo_lock_is_validated_against_its_def() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, "proj/.arsumbris/repo.yaml", "name: proj\n");
    write(&root, "proj/type/thing.type.yaml", "fields:\n  x: String\n");
    // A package entry omits the required `sha` (au.engine.locked-package).
    write(
        &root,
        "proj/.arsumbris/repo.lock",
        "packages:\n  - name: library\n    remote: git@x\n",
    );
    let kb = build(&root.join("proj"), &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("proj/.arsumbris/repo.lock"));
    assert!(
        codes
            .iter()
            .any(|c| c == "required-field-absent" || c == "embedded-record-validation-failure"),
        "a locked-package missing its sha is flagged against au.engine.locked-package: {codes:?}"
    );
}

/// The engine files are VALIDATED against their def via the normal fold: a
/// well-formed one is clean, and a malformed nested record is flagged.
#[test]
fn a_repo_yaml_dep_missing_its_name_is_validated_against_au_engine_dep() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // The repo is valid (has name), but a dep entry omits its required `name`.
    // The `deps` field is `au.engine.dep[]`, so the inline record validates
    // against au.engine.dep (folded from au-engine) — proving the engine file
    // is a validated node, not just a catalogued blob.
    write(
        &root,
        "proj/.arsumbris/repo.yaml",
        "name: proj\ndeps:\n  - remote: git@x\n",
    );
    write(&root, "proj/type/thing.type.yaml", "fields:\n  x: String\n");
    let kb = build(&root.join("proj"), &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join("proj/.arsumbris/repo.yaml"));
    assert!(
        codes
            .iter()
            .any(|c| c == "required-field-absent" || c == "embedded-record-validation-failure"),
        "a dep missing its required name is flagged against au.engine.dep: {codes:?}"
    );
}

/// An engine-schema file with no written `type:` still validates (the kind
/// assigns the floor) but drifts: `engine-schema-type-unwritten`, a nudge to
/// self-describe. Advisory, never blocks.
#[test]
fn an_engine_schema_file_without_a_written_type_drifts() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: myrepo\n");
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join(".arsumbris/repo.yaml"));
    assert!(
        codes.iter().any(|c| c == "engine-schema-type-unwritten"),
        "an unwritten engine-schema type drifts: {codes:?}"
    );
}

/// A written floor type self-describes and is clean: no drift, no floor-omitted.
#[test]
fn a_written_floor_type_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "type: au.engine.repo::au-engine\nname: myrepo\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join(".arsumbris/repo.yaml"));
    assert!(
        !codes.iter().any(|c| c == "engine-schema-type-unwritten"),
        "a written type does not drift: {codes:?}"
    );
    assert!(
        !codes
            .iter()
            .any(|c| c == "engine-schema-type-floor-omitted"),
        "the written floor is present: {codes:?}"
    );
}

/// A written `type:` whose closure omits the kind's floor is an error: the
/// claim names the wrong kind (a `repo.yaml` claiming `au.engine.workspace`).
#[test]
fn a_wrong_floor_written_type_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "type: au.engine.workspace::au-engine\nname: myrepo\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join(".arsumbris/repo.yaml"));
    assert!(
        codes
            .iter()
            .any(|c| c == "engine-schema-type-floor-omitted"),
        "a wrong-floor written claim errors: {codes:?}"
    );
}

/// A written claim that mixes the floor with a consumer type is clean on the
/// floor axis: the extra type's fields are open-world extras, the engine reads
/// only the floor's data. Claim-extensibility on the engine's own files.
#[test]
fn a_mixin_written_type_including_the_floor_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "type/my-config.type.yaml",
        "fields:\n  flavor?: String\n",
    );
    write(
        &root,
        ".arsumbris/repo.yaml",
        "type:\n  - au.engine.repo::au-engine\n  - my-config\nname: myrepo\nflavor: spicy\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join(".arsumbris/repo.yaml"));
    assert!(
        !codes
            .iter()
            .any(|c| c == "engine-schema-type-floor-omitted"),
        "a mixin including the floor is clean: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "engine-schema-type-unwritten"),
        "a written mixin does not drift: {codes:?}"
    );
}

/// The candidate scan applies to an engine-schema file like any instance: a
/// knowledge base type whose required fields the file already carries surfaces as a real
/// candidate. Once a file is claim-authorable it is a real instance the scan can
/// offer claims to, no file-kind exclusion.
#[test]
fn the_candidate_scan_offers_a_real_candidate_to_an_engine_schema_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: myrepo\n");
    // A knowledge base type whose required `name` the repo.yaml already carries.
    write(
        &root,
        "type/my-marker.type.yaml",
        "fields:\n  name: String\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let au_engine::wire::WireCandidateFiles::Summary(files) =
        au_engine::wire::introspect_kb_candidates_paged(&kb, 0, None, true)
    else {
        panic!("summary requested");
    };
    let repo_yaml = root.join(".arsumbris/repo.yaml");
    let entry = files
        .iter()
        .find(|f| repo_yaml.ends_with(&f.file) || f.file.ends_with("repo.yaml"))
        .expect("the repo.yaml node is scanned");
    assert!(
        entry.candidates.iter().any(|c| c == "my-marker"),
        "the scan offers a real candidate to the engine-schema file: {:?}",
        entry.candidates
    );
}

/// A mixed-in extra type validates its OWN required fields like any instance
/// claim: the engine reads the floor's data, the extra is open-world, but its
/// required field is still checked. Claim-extensibility on the engine's files.
#[test]
fn a_mixed_in_extra_types_required_field_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "type/my-config.type.yaml",
        "fields:\n  flavor: String\n",
    );
    // Claims the floor plus `my-config`, but omits my-config's required `flavor`.
    write(
        &root,
        ".arsumbris/repo.yaml",
        "type:\n  - au.engine.repo::au-engine\n  - my-config\nname: myrepo\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");
    let codes = codes_for(&kb, &root.join(".arsumbris/repo.yaml"));
    assert!(
        codes.iter().any(|c| c == "required-field-absent"),
        "the extra type's required field is validated like any claim: {codes:?}"
    );
    assert!(
        !codes
            .iter()
            .any(|c| c == "engine-schema-type-floor-omitted"),
        "the floor is present in the mixin, no floor-omitted: {codes:?}"
    );
}
