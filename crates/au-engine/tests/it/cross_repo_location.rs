//! Cross-repo location LOAD checks: a subtype's own `location` resolved over the
//! fold, not the own graph. The per-instance side already folds; this
//! guards the type-def-half load checks, which resolve a cross-repo-inherited
//! field over the fold rather than false-firing against the own graph.
//!
//! `base` owns `note { slug: String }`; `app` imports it and declares
//! `child extends note::base` with a `location.name` over the inherited `slug`.
//! See codereview - 2609050156 - location constraints branch, one cross-repo load-check gap.

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

/// `app`'s `child` extends the peer `note` and names its stem from `note`'s
/// inherited `slug`. The name-field-safety check must see `slug` through the fold,
/// or it false-fires `location-bad-shape` (an error that aborts `app`).
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/note.type.yaml",
        "fields:\n  slug: String\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(
        &root,
        "app/type/child.type.yaml",
        "extends: note::base\nlocation:\n  name: \"${.slug}\"\n",
    );
    dir
}

#[test]
fn subtype_location_name_over_a_cross_repo_inherited_field_is_clean() {
    let dir = fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");
    let child = root.join("app/type/child.type.yaml");
    let codes = codes_on(&kb, &child);
    assert!(
        !codes.iter().any(|c| c == "location-bad-shape"),
        "an inherited peer field in a subtype's `location.name` must resolve over the fold, got {codes:?}"
    );
}

#[test]
fn a_child_instance_renders_the_inherited_field_for_placement() {
    let dir = fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    // Correctly named: stem "hello" == rendered `${.slug}` = "hello".
    write(
        &root,
        "app/hello.md",
        "---\ntype: child\nslug: hello\n---\n",
    );
    // Misnamed: stem "wrong" != rendered "hello" → a soft mismatch.
    write(
        &root,
        "app/wrong.md",
        "---\ntype: child\nslug: hello\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    assert!(
        codes_on(&kb, &root.join("app/hello.md")).is_empty(),
        "a correctly-named child instance is clean: {:?}",
        codes_on(&kb, &root.join("app/hello.md"))
    );
    assert_eq!(
        codes_on(&kb, &root.join("app/wrong.md")),
        vec!["location-mismatch"],
        "a misnamed child instance renders the inherited field and mismatches"
    );
}
