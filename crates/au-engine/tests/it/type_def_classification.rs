//! Type-def classification: the `.type.yaml` suffix is the sole marker, and a
//! type-def outside a `type/` directory is an advisory `type-def-outside-type-dir`
//! warning, never a classification change.

use std::fs;
use std::path::Path;

use au_parser::RealFileSystem;

use crate::seed_repo;

fn codes(root: &Path) -> Vec<String> {
    let kb = au_engine::build(root, &RealFileSystem).expect("build");
    kb.diagnostics().map(|d| d.code.0.to_string()).collect()
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn a_type_def_under_type_dir_fires_no_location_warning() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    seed_repo(&root);
    write(&root, "type/note.type.yaml", "fields:\n  title: String\n");
    // Nesting under `type/` is fine too.
    write(&root, "type/meta/extra.type.yaml", "fields: {}\n");
    let cs = codes(&root);
    assert!(
        !cs.iter().any(|c| c == "type-def-outside-type-dir"),
        "a type-def under `type/` fires no location warning, got {cs:?}"
    );
}

#[test]
fn a_type_def_outside_type_dir_warns_but_still_classifies() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    seed_repo(&root);
    // A valid type-def by suffix, but sitting under `notes/`, not `type/`.
    write(&root, "notes/note.type.yaml", "fields:\n  title: String\n");
    // An instance claiming it, to prove the def still entered the graph.
    write(&root, "n.md", "---\ntype: note\ntitle: hello\n---\n");
    let cs = codes(&root);
    assert!(
        cs.iter().any(|c| c == "type-def-outside-type-dir"),
        "a type-def outside `type/` warns, got {cs:?}"
    );
    // Still a real type-def: the instance claim resolves, so no unknown-type-claim.
    assert!(
        !cs.iter().any(|c| c == "unknown-type-claim"),
        "the misplaced file still classifies as a type-def, got {cs:?}"
    );
}

#[test]
fn a_prose_readme_under_type_dir_is_a_note_not_a_broken_type_def() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    seed_repo(&root);
    // A prose doc under `type/`, the dogfood case. A trailing colon used
    // to be force-parsed as YAML and became a hard `yaml-parse-error`.
    write(
        &root,
        "type/README.md",
        "# The type directory\n\nThis documents the convention:\n\nType-defs live here.\n",
    );
    let cs = codes(&root);
    assert!(
        !cs.iter().any(|c| c == "yaml-parse-error"),
        "a prose README under `type/` is not force-parsed as YAML, got {cs:?}"
    );
    // It is not a type-def, so it never trips the location advisory either.
    assert!(
        !cs.iter().any(|c| c == "type-def-outside-type-dir"),
        "a non-`.type.yaml` file is not a type-def, so no location warning, got {cs:?}"
    );
}
