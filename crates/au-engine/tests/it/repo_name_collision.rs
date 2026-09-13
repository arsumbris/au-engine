//! Repo names never silently merge two repos into one.
//!
//! Two co-present members stay isolated, each routed to its own per-repo graph.
//! A nested repo under a folder-repo entry that is not a declared member is
//! SKIPPED (`undeclared-nested-repo`), contributing nothing, rather than
//! dissolving its files into the parent or fighting over a duplicated name.

use std::fs;
use std::path::Path;

use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn two_co_present_members_stay_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = fs::canonicalize(dir.path()).unwrap();

    // One workspace, two co-present members, each declaring its own name. Their
    // vocabularies stay isolated, routed to their own per-repo graphs.
    crate::seed_workspace(&tmp.join("ws"), &["alpha", "beta"]);

    // alpha owns `note`.
    write(&tmp, "ws/alpha/.arsumbris/repo.yaml", "name: alpha\n");
    write(
        &tmp,
        "ws/alpha/type/note.type.yaml",
        "fields:\n  a: String\n",
    );
    write(&tmp, "ws/alpha/doc.md", "---\ntype: note\na: hi\n---\n");

    // beta owns `widget`.
    write(&tmp, "ws/beta/.arsumbris/repo.yaml", "name: beta\n");
    write(
        &tmp,
        "ws/beta/type/widget.type.yaml",
        "fields:\n  w: String\n",
    );
    write(&tmp, "ws/beta/doc.md", "---\ntype: widget\nw: hi\n---\n");

    let ws = tmp.join("ws");
    let kb = build(&ws, &RealFileSystem).expect("build");

    let alpha_doc = tmp.join("ws/alpha/doc.md");
    let beta_doc = tmp.join("ws/beta/doc.md");

    // Each member's graph holds its own type, routed by path.
    assert!(
        kb.graph_for_path(&alpha_doc)
            .names()
            .any(|n| n.as_str() == "note"),
        "alpha resolves `note` against its own graph"
    );
    assert!(
        kb.graph_for_path(&beta_doc)
            .names()
            .any(|n| n.as_str() == "widget"),
        "beta resolves `widget` against its own graph"
    );

    // Both valid claims resolve; neither dangles.
    assert!(
        kb.instances[&alpha_doc].effective_shape.is_some(),
        "alpha's note instance resolves"
    );
    assert!(
        kb.instances[&beta_doc].effective_shape.is_some(),
        "beta's widget instance resolves"
    );
    assert!(
        !kb.diagnostics()
            .any(|d| d.code.as_str() == "unknown-type-claim"),
        "no valid claim should dangle"
    );
}

#[test]
fn an_undeclared_nested_repo_is_skipped_not_absorbed() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The entry folder-repo `root` owns `note`. A nested `pkg/` is NOT a declared
    // member (and here even declares the SAME name `root`, a misconfiguration).
    // It is SKIPPED with `undeclared-nested-repo` and contributes nothing: its
    // `widget` never enters any graph, and because its marker is filtered before
    // discovery there is no `duplicate-repo-name` fight over the shared name.
    write(&root, ".arsumbris/repo.yaml", "name: root\n");
    write(&root, ".arsumbris/workspace.yaml", "edit:\n  - root\n");
    write(&root, "pkg/.arsumbris/repo.yaml", "name: root\n");
    write(&root, "type/note.type.yaml", "fields:\n  a: String\n");
    write(&root, "doc.md", "---\ntype: note\na: hi\n---\n");
    write(&root, "pkg/type/widget.type.yaml", "fields:\n  w: String\n");
    write(&root, "pkg/w.md", "---\ntype: widget\nw: hi\n---\n");

    let kb = build(&root, &RealFileSystem).expect("build");

    let root_doc = root.join("doc.md");
    let pkg_doc = root.join("pkg/w.md");

    let codes: Vec<String> = kb
        .diagnostics()
        .map(|d| d.code.as_str().to_string())
        .collect();
    assert!(
        codes.iter().any(|c| c == "undeclared-nested-repo"),
        "the undeclared nested repo is skipped with a warning, got {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "duplicate-repo-name"),
        "the skipped repo never reaches discovery, so no duplicate fight, got {codes:?}"
    );

    // The root repo keeps only its own vocabulary; `widget` was skipped entirely.
    let root_types: Vec<String> = kb
        .graph_for_path(&root_doc)
        .names()
        .map(|n| n.as_str().to_string())
        .collect();
    assert_eq!(
        root_types,
        vec!["note"],
        "root repo keeps only its own vocabulary"
    );
    assert!(kb.instances[&root_doc].effective_shape.is_some());
    // The skipped subtree contributes no instances.
    assert!(
        !kb.instances.contains_key(&pkg_doc),
        "the skipped nested subtree contributes no instances"
    );
}

#[test]
fn a_declared_but_ambiguous_name_is_not_falsely_reported_undeclared() {
    // `lib` is a DECLARED edit member, but two nested repos both claim the name,
    // so it resolves ambiguously and never mounts. The ambiguity is owned by
    // `duplicate-repo-name`; the nested roots must NOT also be reported as
    // `undeclared-nested-repo` ("lib is not a declared member" would be false,
    // since `lib` IS declared).
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    write(&root, ".arsumbris/repo.yaml", "name: root\n");
    write(
        &root,
        ".arsumbris/workspace.yaml",
        "edit:\n  - root\n  - lib\n",
    );
    write(&root, "a/lib/.arsumbris/repo.yaml", "name: lib\n");
    write(&root, "b/lib/.arsumbris/repo.yaml", "name: lib\n");

    let kb = build(&root, &RealFileSystem).expect("build");
    let codes: Vec<String> = kb
        .diagnostics()
        .map(|d| d.code.as_str().to_string())
        .collect();
    assert!(
        !codes.iter().any(|c| c == "undeclared-nested-repo"),
        "a declared-but-ambiguous name is not falsely called undeclared, got {codes:?}"
    );
    assert!(
        codes.iter().any(|c| c == "duplicate-repo-name"),
        "the same-name collision still surfaces as duplicate-repo-name, got {codes:?}"
    );
}

#[test]
fn a_nested_broken_repo_yaml_names_the_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The entry folder-repo owns `note`. A nested repo carries a `repo.yaml` with
    // an unquoted `:` inside a scalar value, which will not parse. It drops out of
    // membership, but the malformed file now NAMES itself with an Error, instead
    // of vanishing behind only `undeclared-nested-repo`.
    write(&root, ".arsumbris/repo.yaml", "name: root\n");
    write(&root, "type/note.type.yaml", "fields:\n  a: String\n");
    write(&root, "doc.md", "---\ntype: note\na: hi\n---\n");
    write(
        &root,
        "nested/.arsumbris/repo.yaml",
        "name: nested\ndescription: text: colon\n",
    );
    write(&root, "nested/x.md", "hi\n");

    let kb = build(&root, &RealFileSystem).expect("build");

    let parse_err = kb
        .diagnostics()
        .find(|d| d.code.as_str() == "repo-registry-parse-error")
        .unwrap_or_else(|| {
            let codes: Vec<_> = kb.diagnostics().map(|d| d.code.as_str()).collect();
            panic!("expected repo-registry-parse-error, got {codes:?}")
        });
    assert_eq!(format!("{:?}", parse_err.severity), "Error");
    assert_eq!(
        parse_err.span.file,
        root.join("nested/.arsumbris/repo.yaml")
    );
    assert!(
        parse_err
            .fix
            .as_ref()
            .is_some_and(|f| f.description.contains("quote")),
        "the parse error carries a quoting hint, got {:?}",
        parse_err.fix
    );
}

#[test]
fn a_broken_external_dependency_names_the_malformed_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // The entry `app` depends on `base`, a co-present sibling. `base`'s repo.yaml
    // has an unquoted `:` and will not parse, so it cannot be matched by name and
    // drops out. Before, the only signal was a misleading `peer-unmounted` at
    // app's own repo.yaml (as if base were absent); now the malformed file names
    // itself with an Error, exactly once (the dep-notes and member-notes channels
    // are deduped).
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    write(&root, "app/a.md", "hi\n");
    write(
        &root,
        "base/.arsumbris/repo.yaml",
        "name: base\ndescription: x: y\n",
    );
    write(&root, "base/b.md", "hi\n");

    let kb = build(&root.join("app"), &RealFileSystem).expect("build");

    let parse_errs: Vec<_> = kb
        .diagnostics()
        .filter(|d| d.code.as_str() == "repo-registry-parse-error")
        .collect();
    assert_eq!(
        parse_errs.len(),
        1,
        "the malformed dep names itself once (deduped across channels), got {parse_errs:?}"
    );
    let d = parse_errs[0];
    assert_eq!(format!("{:?}", d.severity), "Error");
    assert_eq!(d.span.file, root.join("base/.arsumbris/repo.yaml"));
    assert!(
        d.fix
            .as_ref()
            .is_some_and(|f| f.description.contains("quote")),
        "carries the quoting hint, got {:?}",
        d.fix
    );
}

/// The messages `duplicate-repo-name` carries, one per site that can emit it.
fn duplicate_repo_name_messages(kb: &au_engine::KnowledgeBase) -> Vec<String> {
    kb.diagnostics()
        .filter(|d| d.code.as_str() == "duplicate-repo-name")
        .map(|d| d.message.clone())
        .collect()
}

/// A folder-repo entry whose declared member `lib` is claimed by two nested
/// repos, plus an `app` member depending on that same ambiguous name.
///
/// One fixture reaches BOTH build-level emitters: the member one (the manifest
/// named `lib` and it does not resolve) and the dep one (`app` declared it and
/// it does not resolve).
fn ambiguous_lib_with_a_depender(root: &Path) {
    write(root, ".arsumbris/repo.yaml", "name: root\n");
    write(
        root,
        ".arsumbris/workspace.yaml",
        "edit:\n  - root\n  - app\n  - lib\n",
    );
    write(
        root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: lib\n",
    );
    write(root, "a/lib/.arsumbris/repo.yaml", "name: lib\n");
    write(root, "b/lib/.arsumbris/repo.yaml", "name: lib\n");
}

#[test]
fn an_ambiguous_dep_is_reported_against_the_repo_that_declared_it() {
    // `app` depends on `lib`, which two co-present siblings both declare. The
    // resolver refuses to pick, and the depender must HEAR about it: without
    // this, `app`'s dep silently stays unmounted and its `::lib` types fail later
    // with no explanation of why.
    //
    // Asserts the MESSAGE, not just the code. Three sites emit
    // `duplicate-repo-name`, and the registry-level one fires for this fixture
    // regardless, so a code-only assertion stays green even if the dep-level
    // emitter disappears entirely. That is exactly the hole this closes: an
    // optimisation that resolved a dep through `RepoMap::by_name` would see the
    // first winner, never the ambiguity, and drop this diagnostic silently.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    ambiguous_lib_with_a_depender(&root);

    let kb = build(&root, &RealFileSystem).expect("build");
    let messages = duplicate_repo_name_messages(&kb);
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("dep 'lib' matches more than one co-present sibling")),
        "the depender must be told its dep is ambiguous, got {messages:?}"
    );

    // Anchored at the DECLARING repo, so a consumer can jump to the `deps:` entry
    // that caused it rather than to whichever repo happens to own the name.
    let anchored: Vec<String> = kb
        .diagnostics()
        .filter(|d| d.message.starts_with("dep 'lib' matches"))
        .map(|d| d.span.file.display().to_string())
        .collect();
    assert!(
        anchored
            .iter()
            .all(|f| f == &root.join("app/.arsumbris/repo.yaml").display().to_string()),
        "the dep note anchors at the declaring repo's registry, got {anchored:?}"
    );
}

#[test]
fn an_ambiguous_workspace_member_is_reported_at_the_manifest() {
    // The member-side sibling of the test above: the manifest named `lib` and it
    // resolves to two repos, so the workspace opens without it. Also asserted by
    // message, for the same reason: the registry-level emitter would otherwise
    // keep a code-only assertion green on its own.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    ambiguous_lib_with_a_depender(&root);

    let kb = build(&root, &RealFileSystem).expect("build");
    let messages = duplicate_repo_name_messages(&kb);
    assert!(
        messages
            .iter()
            .any(|m| m
                .starts_with("workspace member 'lib' matches more than one co-present sibling")),
        "the manifest must be told its member is ambiguous, got {messages:?}"
    );
}
