//! Reference resolution is repo-local: an unqualified wikilink resolves only
//! within its own repo's files. A link to a name that lives only in a sibling
//! repo dangles, and forms no backlink edge across the boundary.

use std::fs;
use std::path::Path;

use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Two repos in one tree. `base` holds `recovery.md`; `app` does not. Both an
/// in-repo and a cross-repo body link to `[[recovery]]` exist.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(&root, "app/.arsumbris/repo.yaml", "name: app\n");
    write(&root, "base/recovery.md", "Recovery notes.\n");
    // Same-repo link: resolves to base/recovery.md.
    write(&root, "base/intro.md", "See [[recovery]].\n");
    // Cross-repo link: recovery is not in app, so it must dangle.
    write(&root, "app/sleep.md", "See [[recovery]].\n");
    dir
}

#[test]
fn an_unqualified_link_resolves_only_within_its_own_repo() {
    let dir = fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let recovery = root.join("base/recovery.md");

    // base's index finds recovery; app's index does not.
    assert_eq!(
        kb.index_for_path(&root.join("base/intro.md"))
            .resolve("recovery")
            .ok(),
        Some(recovery.clone()),
        "a same-repo link resolves"
    );
    assert!(
        kb.index_for_path(&root.join("app/sleep.md"))
            .resolve("recovery")
            .is_err(),
        "a cross-repo link does not resolve repo-local"
    );
}

#[test]
fn a_cross_repo_link_forms_no_backlink_edge() {
    let dir = fixture();
    let root = fs::canonicalize(dir.path()).unwrap();
    let kb = build(&root, &RealFileSystem).expect("build");

    let recovery = root.join("base/recovery.md");
    let sources: Vec<_> = kb
        .backlinks(&recovery)
        .iter()
        .map(|b| b.source.clone())
        .collect();

    assert!(
        sources.contains(&root.join("base/intro.md")),
        "the same-repo link is an inbound edge, got {sources:?}"
    );
    assert!(
        !sources.contains(&root.join("app/sleep.md")),
        "the cross-repo link forms no edge, got {sources:?}"
    );
}

/// A `::repo`-qualified link is not flagged by the repo-local checks: it is
/// cross-repo, deferred to the engine's cross-repo layer. (Resolution and the
/// reference-repo-* diagnostics land in a later action; here it must simply not
/// mis-fire as a repo-local dangling/missing reference.)
#[test]
fn a_repo_qualified_link_is_not_a_repo_local_dangle() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(&root, "base/recovery.md", "Recovery.\n");
    // A typed instance with a file-reference field, so the reference is
    // validated (a note's body links are not).
    write(&root, "app/type/card.type.yaml", "fields:\n  link: file*\n");
    // `recovery` is not in app; the ::repo link must not be flagged repo-local.
    write(
        &root,
        "app/ok.md",
        "---\ntype: card\nlink: \"[[recovery::base]]\"\n---\n",
    );
    // The unqualified link to the same missing-local name still errors.
    write(
        &root,
        "app/bad.md",
        "---\ntype: card\nlink: \"[[recovery]]\"\n---\n",
    );
    let kb = build(&root, &RealFileSystem).expect("build");

    let codes_for = |rel: &str| -> Vec<String> {
        let p = root.join(rel);
        kb.diagnostics()
            .filter(|d| d.span.file == p)
            .map(|d| d.code.as_str().to_string())
            .collect()
    };

    assert!(
        codes_for("app/ok.md").is_empty(),
        "a ::repo reference is not flagged repo-local, got {:?}",
        codes_for("app/ok.md")
    );
    // The unqualified one is still a repo-local dangle (sanity: the guard is
    // specific to ::repo, not a blanket skip).
    assert!(
        codes_for("app/bad.md").contains(&"reference-target-missing".to_string()),
        "the unqualified missing reference still errors, got {:?}",
        codes_for("app/bad.md")
    );
}

/// A single-repo knowledge base is unchanged: one repo means one index covering every
/// file, so resolution is identical to the pre-refactor whole-knowledge-base index.
#[test]
fn single_repo_resolution_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    write(&root, "recovery.md", "Recovery.\n");
    write(&root, "intro.md", "See [[recovery]].\n");
    let kb = build(&root, &RealFileSystem).expect("build");

    assert_eq!(
        kb.index_for_path(&root.join("intro.md"))
            .resolve("recovery")
            .ok(),
        Some(root.join("recovery.md"))
    );
    assert!(kb
        .backlinks(&root.join("recovery.md"))
        .iter()
        .any(|b| b.source == root.join("intro.md")));
}
