//! The `scope: own | all` filter on the workspace-wide type reads. `all`
//! (default) surfaces every mounted repo's vocabulary; `own` keeps only the
//! user's own editable repos (role-derived: the entry and `edit` members),
//! hiding every dependency and the compiled-in `au.engine.*` builtin.

#![cfg(unix)]

use std::fs;
use std::path::Path;

use au_engine::build;
use au_engine::wire::{
    introspect_instance_counts, introspect_list_imports, introspect_type_counts, TypeScope,
};
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// The compiled-in `au.engine.*` builtin types surface under `all` but are
/// hidden under `own` — the degenerate case of the dependency filter, since the
/// builtin is a dependency-like type source, not the user's own vocabulary.
#[test]
fn scope_own_hides_the_engine_builtin() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: proj\n");
    write(&root, "type/foo.type.yaml", "fields:\n  x: String\n");
    let kb = build(&root, &RealFileSystem).expect("build");

    let all = introspect_type_counts(&kb, None, TypeScope::all()).unwrap();
    assert!(
        all.by_repo.contains_key("au-engine"),
        "all shows the builtin: {:?}",
        all.by_repo
    );
    assert!(
        all.total >= 10,
        "all counts the 9 builtin defs plus proj's own: {}",
        all.total
    );

    let own = introspect_type_counts(&kb, None, TypeScope::new(true)).unwrap();
    assert!(
        !own.by_repo.contains_key("au-engine"),
        "own hides the builtin: {:?}",
        own.by_repo
    );
    assert_eq!(
        own.by_repo.get("proj"),
        Some(&1),
        "own keeps proj's own type"
    );
    assert_eq!(
        own.total, 1,
        "own counts only proj's own type: {:?}",
        own.by_repo
    );
}

/// A computed dependency's types are hidden under `own`, while the `edit`
/// member's are shown. Assembly mode: `app` is the edit member, `base` a
/// computed dependency via `app`'s `deps`. `base` is a co-present LOCAL working
/// tree, so this also pins the decoupling of decision 2607161333: `own` follows
/// the member ROLE, not its on-disk location — a live-tree dep is still hidden.
#[test]
fn scope_own_hides_a_dependency_repos_types() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/thing.type.yaml",
        "fields:\n  title: String\n",
    );
    // The workspace edits only `app`; `base` is pulled in solely as `app`'s
    // computed dependency, so assembly marks it a dependency, not a primary.
    crate::seed_workspace(&root, &["app"]);
    let kb = build(&root, &RealFileSystem).expect("build");

    let all = introspect_type_counts(&kb, None, TypeScope::all()).unwrap();
    assert!(
        all.by_repo.contains_key("base") && all.by_repo.contains_key("app"),
        "all shows both the primary and the dependency: {:?}",
        all.by_repo
    );

    let own = introspect_type_counts(&kb, None, TypeScope::new(true)).unwrap();
    assert!(
        own.by_repo.contains_key("app"),
        "own shows the primary: {:?}",
        own.by_repo
    );
    assert!(
        !own.by_repo.contains_key("base"),
        "own hides the computed dependency: {:?}",
        own.by_repo
    );
    assert!(
        !own.by_repo.contains_key("au-engine"),
        "own hides the builtin: {:?}",
        own.by_repo
    );
}

// Retired with decision 2607161333: `own` no longer gates on a member's on-disk
// location (root-under-cache-root). Editability is role-derived, so a member's
// vocabulary is in `own` scope by its role, never by where it resolved. The old
// test asserted the reverse (a repo under the cache root is hidden under `own`),
// which no longer holds. The role-over-location decoupling is now pinned by
// `scope_own_hides_a_dependency_repos_types` (a co-present live-tree dep hidden
// under `own`).

/// `instance_counts` is CLOSURE-INCLUSIVE: an `article` (which `extends note`)
/// counts toward both `article` and `note`, so `by_type` does not sum to
/// `total`, and a row's count equals the length of the matching `instances_of`
/// drill-in. Counts are identity-keyed and carry the owner.
#[test]
fn instance_counts_are_closure_inclusive() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: proj\n");
    write(&root, "type/note.type.yaml", "fields:\n  title: String\n");
    write(
        &root,
        "type/article.type.yaml",
        "extends: note\nfields:\n  text: String\n",
    );
    write(&root, "n.md", "---\ntype: note\ntitle: a\n---\n");
    write(
        &root,
        "a1.md",
        "---\ntype: article\ntitle: b\ntext: x\n---\n",
    );
    write(
        &root,
        "a2.md",
        "---\ntype: article\ntitle: c\ntext: y\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    let counts = introspect_instance_counts(&kb, None, TypeScope::all()).unwrap();
    // Three authored docs plus the repo's own `.arsumbris/repo.yaml`, itself an
    // `au.engine.repo` engine-schema instance.
    assert_eq!(
        counts.total, 4,
        "three docs + the repo.yaml config instance"
    );

    let by_name = |name: &str| {
        counts
            .by_type
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no count for {name}: {:?}", counts.by_type))
    };
    // The two articles ARE notes, so note = 1 + 2 = 3, article = 2. Closure-
    // inclusive counts do NOT sum to total (3 + 2 + 1 config ≠ 4).
    assert_eq!(by_name("note").count, 3, "note is closure-inclusive");
    assert_eq!(by_name("article").count, 2, "article claimed twice");
    // Identity carries the owner repo.
    assert_eq!(by_name("article").type_owners, vec!["proj".to_string()]);
    // Sorted by name.
    let names: Vec<_> = counts.by_type.iter().map(|c| c.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "by_type is name-sorted");
}

/// Coverage gap #3: `instance_counts` tallies a cross-repo slot-pinned NESTED
/// record against its owner-relative identity. The tally walks
/// `enumerate_nested_records` and credits each via `nested_closure_tids` over the
/// owner repo, so `concept-candidate`, present ONLY as a claim-less nested record
/// in a peer-claiming host, must count once and carry the owner `base`.
#[test]
fn instance_counts_tally_a_cross_repo_slot_pinned_nested_record() {
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
    write(
        &root,
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - salience: focal\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    let counts = introspect_instance_counts(&kb, None, TypeScope::all()).unwrap();
    let cc = counts
        .by_type
        .iter()
        .find(|c| c.name == "concept-candidate")
        .unwrap_or_else(|| panic!("no concept-candidate count: {:?}", counts.by_type));
    assert_eq!(
        cc.count, 1,
        "the cross-repo slot-pinned nested record is tallied once"
    );
    assert_eq!(
        cc.type_owners,
        vec!["base".to_string()],
        "tallied against the owner-relative identity"
    );
}

/// `instance_counts` scopes by the instance SITE's repo: `own` counts only
/// instances authored in the user's editable repos, hiding a dependency's
/// instances; a named `repo` pins that member; an unknown `repo` is null.
#[test]
fn instance_counts_scope_by_site_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/task.type.yaml",
        "fields:\n  done: Boolean\n",
    );
    write(&root, "app/t.md", "---\ntype: task\ndone: false\n---\n");
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/thing.type.yaml",
        "fields:\n  title: String\n",
    );
    write(&root, "base/th.md", "---\ntype: thing\ntitle: z\n---\n");
    crate::seed_workspace(&root, &["app"]);
    let kb = build(&root, &RealFileSystem).expect("build");

    let has = |v: &au_engine::wire::InstanceCountsView, name: &str| {
        v.by_type.iter().any(|c| c.name == name)
    };

    // `all` counts both members' instances (site-repo scoping is on WHERE the
    // instance lives, not its type's owner).
    let all = introspect_instance_counts(&kb, None, TypeScope::all()).unwrap();
    assert!(has(&all, "task") && has(&all, "thing"), "all: both members");

    // `own` keeps the editable `app`, hides the dependency `base`.
    let own = introspect_instance_counts(&kb, None, TypeScope::new(true)).unwrap();
    assert!(
        has(&own, "task"),
        "own keeps the app instance: {:?}",
        own.by_type
    );
    assert!(
        !has(&own, "thing"),
        "own hides the base instance: {:?}",
        own.by_type
    );

    // A named repo pins to that member's own instances.
    let pinned = introspect_instance_counts(&kb, Some("base"), TypeScope::all()).unwrap();
    assert!(has(&pinned, "thing"), "base pins to its own instance");
    assert!(!has(&pinned, "task"), "base does not count app's instance");

    // `scope` still applies UNDER a pin, matching `type_counts`: pinning the
    // dependency `base` under `own` counts nothing (a dependency is not yours).
    let pinned_own = introspect_instance_counts(&kb, Some("base"), TypeScope::new(true)).unwrap();
    assert!(
        pinned_own.by_type.is_empty() && pinned_own.total == 0,
        "repo=base + own is empty, base is a dependency: {:?}",
        pinned_own.by_type
    );
    // Pinning the editable `app` under `own` still counts it.
    let app_own = introspect_instance_counts(&kb, Some("app"), TypeScope::new(true)).unwrap();
    assert!(
        has(&app_own, "task"),
        "repo=app + own keeps the editable member"
    );

    assert!(
        introspect_instance_counts(&kb, Some("nope"), TypeScope::all()).is_none(),
        "an unknown repo is the null signal"
    );
}

/// `list_imports` under `own` drops the automatic `au.engine.*` builtin fold
/// (every repo imports it), keeping only the user's own authored imports.
#[test]
fn scope_own_hides_the_engine_fold_from_list_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: proj\n");
    write(&root, "type/foo.type.yaml", "fields:\n  x: String\n");
    let kb = build(&root, &RealFileSystem).expect("build");

    let all = introspect_list_imports(&kb, TypeScope::all());
    assert!(
        all.iter().any(|i| i.owner == "au-engine"),
        "all lists the builtin fold import: {:?}",
        all.iter()
            .map(|i| (&i.importer, &i.name, &i.owner))
            .collect::<Vec<_>>()
    );

    let own = introspect_list_imports(&kb, TypeScope::new(true));
    assert!(
        own.iter().all(|i| i.owner != "au-engine"),
        "own drops the builtin fold import: {:?}",
        own.iter()
            .map(|i| (&i.importer, &i.name, &i.owner))
            .collect::<Vec<_>>()
    );
}
