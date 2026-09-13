//! An imported (`::repo`) instance's body/value scan reads the FOLDED effective
//! shape, not the empty own-graph shape. So an `any` field on a peer-owned type
//! suppresses navigational `[[wikilink]]` scanning the same whether the type is
//! resolved by import or by a local copy — the own-graph-vs-fold class.
//!
//! Regression for the false `navigational-target-not-found` that fired on an
//! imported instance's `any`-stored `[[...]]` text.

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

fn nav_codes_on(kb: &au_engine::KnowledgeBase, file: &Path) -> Vec<String> {
    kb.diagnostics()
        .filter(|d| d.span.file == file)
        .filter(|d| d.code.as_str() == "navigational-target-not-found")
        .map(|d| format!("{}", d.message))
        .collect()
}

/// `base` owns `log` with an `opaque`-typed `payload` and a `String`-typed
/// `blurb`. `app` peers `base` and holds one instance claiming `log::base`. The
/// `opaque` payload stores text with embedded `[[...]]`; the `blurb` string
/// stores one embedded `[[missing]]`.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);

    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/log.type.yaml",
        "fields:\n  payload?: opaque\n  blurb?: String\n",
    );

    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // Imported claim. `payload` (opaque) holds two embedded links that MUST NOT
    // be scanned; `blurb` (String) holds one embedded link that MUST be scanned.
    write(
        &root,
        "app/entry.md",
        "---\ntype: log::base\npayload: \"stored [[type-def]] and [[type-instance]] text\"\nblurb: \"see [[missing]] here\"\n---\n",
    );
    dir
}

#[test]
fn an_imported_instances_opaque_field_suppresses_navigational_scanning() {
    let dir = fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let nav = nav_codes_on(&kb, &root.join("app/entry.md"));
    // The `opaque` payload contributes nothing; only the `String` blurb's one
    // embedded link is scanned. Without the fold the own-graph shape was empty,
    // so all three embedded links fired.
    assert_eq!(
        nav.len(),
        1,
        "the opaque field is suppressed and the String field is still scanned, got {nav:?}"
    );
    assert!(
        nav[0].contains("missing"),
        "the one surviving warning is the String field's dangling link, got {nav:?}"
    );
}
