//! Per-repo type resolution over a co-present multi-repo tree.
//!
//! Two repos in one workspace, each with its own vendored vocabulary. A repo
//! resolves type claims against its own graph; a type a repo did not vendor
//! dangles, even when a sibling repo defines it. One broken repo does not
//! poison a clean sibling.

use std::fs;
use std::path::Path;

use au_engine::{build, BuildOutcome};
use au_parser::RealFileSystem;

/// Write a file, creating parent dirs.
fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn type_names(graph: &au_engine::RepoGraphs, repo: &str) -> Vec<String> {
    let name = au_engine::RepoName(repo.to_string());
    graph
        .of(&name)
        .names()
        .map(|n| n.as_str().to_string())
        .collect()
}

#[test]
fn each_repo_resolves_its_own_vendored_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // Two repos in one tree: the root repo owns `note`, the nested `pkg` repo
    // owns `widget`. Neither vendors the other's type.
    write(&root, ".arsumbris/repo.yaml", "name: root\n");
    write(&root, "pkg/.arsumbris/repo.yaml", "name: pkg\n");
    // The entry composes both member repos: the content-bearing entry `root`
    // (it must list itself in `edit`) and the nested `pkg`.
    write(
        &root,
        ".arsumbris/workspace.yaml",
        "edit:\n  - root\n  - pkg\n",
    );
    write(&root, "type/note.type.yaml", "fields:\n  a: String\n");
    write(&root, "pkg/type/widget.type.yaml", "fields:\n  w: String\n");

    // Root instance claims `note` (vendored locally) → resolves.
    write(&root, "doc.md", "---\ntype: note\na: hello\n---\n");
    // Pkg instance claims `widget` (vendored locally) → resolves.
    write(&root, "pkg/w.md", "---\ntype: widget\nw: hi\n---\n");
    // Pkg instance claims `note`, which pkg did NOT vendor → dangles, even
    // though the root repo defines it. No cross-repo fallback.
    write(&root, "pkg/borrows.md", "---\ntype: note\na: hi\n---\n");

    let kb = build(&root, &RealFileSystem).expect("build");

    // Each repo's graph holds only its own defs.
    assert_eq!(type_names(&kb.graphs, "root"), vec!["note"]);
    assert_eq!(type_names(&kb.graphs, "pkg"), vec!["widget"]);

    // Routing by path: an instance resolves against its repo's graph.
    let pkg_borrows = root.join("pkg/borrows.md");
    assert!(
        !kb.graph_for_path(&pkg_borrows)
            .names()
            .any(|n| n.as_str() == "note"),
        "pkg's graph does not see the root repo's `note`"
    );

    // The un-vendored claim dangles: an unknown-type-claim on the pkg file,
    // and no resolved shape.
    assert!(
        kb.diagnostics()
            .any(|d| { d.code.as_str() == "unknown-type-claim" && d.span.file == pkg_borrows }),
        "pkg/borrows.md claims a type it did not vendor"
    );
    assert!(
        kb.instances[&pkg_borrows].effective_shape.is_none(),
        "the dangling claim has no effective shape"
    );

    // The locally-vendored claims resolve.
    assert!(
        kb.instances[&root.join("doc.md")].effective_shape.is_some(),
        "root's note instance resolves against the root graph"
    );
    assert!(
        kb.instances[&root.join("pkg/w.md")]
            .effective_shape
            .is_some(),
        "pkg's widget instance resolves against the pkg graph"
    );

    // No cross-repo poisoning: the un-vendored `note` claim in pkg does not
    // make `doc.md` (which legitimately claims `note`) dangle.
    assert!(
        !kb.diagnostics().any(|d| {
            d.code.as_str() == "unknown-type-claim" && d.span.file == root.join("doc.md")
        }),
        "the root's note claim is fine"
    );
}

#[test]
fn a_broken_repo_does_not_abort_a_clean_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The root repo's vocabulary is broken (unparseable type-def). The pkg
    // repo is clean.
    write(&root, ".arsumbris/repo.yaml", "name: root\n");
    write(&root, "pkg/.arsumbris/repo.yaml", "name: pkg\n");
    // The entry composes both member repos: the content-bearing entry `root`
    // (it must list itself in `edit`) and the nested `pkg`.
    write(
        &root,
        ".arsumbris/workspace.yaml",
        "edit:\n  - root\n  - pkg\n",
    );
    write(&root, "type/bad.type.yaml", "fields: [unclosed\n");
    write(&root, "pkg/type/widget.type.yaml", "fields:\n  w: String\n");

    write(&root, "doc.md", "---\ntype: bad\n---\n");
    write(&root, "pkg/w.md", "---\ntype: widget\nw: hi\n---\n");

    let kb = build(&root, &RealFileSystem).expect("build");

    // The broken repo aborts; the clean sibling completes.
    assert_eq!(
        kb.outcome_for_path(&root.join("doc.md")),
        BuildOutcome::AbortedAtGraph,
        "the root repo's broken vocabulary aborts its own validation"
    );
    assert_eq!(
        kb.outcome_for_path(&root.join("pkg/w.md")),
        BuildOutcome::Complete,
        "the clean pkg repo still completes"
    );

    // The clean sibling's instance is validated and resolves.
    assert!(
        kb.instances[&root.join("pkg/w.md")]
            .effective_shape
            .is_some(),
        "pkg's instance resolves despite the root repo being broken"
    );
    // The broken repo's instance was not validated.
    assert!(
        !kb.instances.contains_key(&root.join("doc.md")),
        "the broken repo's instance is skipped"
    );
}
