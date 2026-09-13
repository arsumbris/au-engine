//! Repo README obligations: presence, self-declaration, placement, and scope.
//!
//! `seed_repo` writes a conformant README, so these fixtures write their own
//! `.arsumbris/repo.yaml` (and READMEs) directly to exercise the missing /
//! undeclared / misplaced / exempt cases.

use std::fs;
use std::path::Path;

use au_parser::RealFileSystem;

fn codes(root: &Path) -> Vec<String> {
    let kb = au_engine::build(root, &RealFileSystem).expect("build");
    kb.diagnostics().map(|d| d.code.0.to_string()).collect()
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

const CONFORMANT_README: &str =
    "---\ntype: au.engine.readme::au-engine\ntldr: t\n---\n\n# Repo Overview\n\n## What this is\n\nt\n\n## How to use this\n\nt\n\n## How to extend this\n\nt\n";

#[test]
fn an_entry_repo_with_no_readme_warns() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    // No README.md at the root.
    assert!(
        codes(&root).contains(&"repo-missing-readme".to_string()),
        "an editable repo without a root README warns, got {:?}",
        codes(&root)
    );
}

#[test]
fn a_conformant_readme_clears_the_obligation() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    write(&root, "README.md", CONFORMANT_README);
    let cs = codes(&root);
    assert!(
        !cs.iter()
            .any(|c| c.starts_with("repo-missing-readme") || c.starts_with("readme-")),
        "a well-formed README fires no README diagnostic, got {cs:?}"
    );
}

#[test]
fn a_readme_without_a_tldr_field_fires_required_field_absent() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    // Self-declares the type and carries every section, but omits the required
    // `tldr` field, so it validates like any instance missing a required field.
    write(
        &root,
        "README.md",
        "---\ntype: au.engine.readme::au-engine\n---\n\n# Repo Overview\n\n## What this is\n\nt\n\n## How to use this\n\nt\n\n## How to extend this\n\nt\n",
    );
    let cs = codes(&root);
    assert!(
        cs.contains(&"required-field-absent".to_string()),
        "a README missing the required tldr field fires required-field-absent, got {cs:?}"
    );
}

#[test]
fn a_root_readme_without_a_type_claim_warns_undeclared() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    // A plain README, no frontmatter type.
    write(&root, "README.md", "# What this is\n\nt\n");
    let cs = codes(&root);
    assert!(
        cs.contains(&"readme-type-undeclared".to_string()),
        "a README that does not self-declare its type warns, got {cs:?}"
    );
    assert!(
        !cs.contains(&"repo-missing-readme".to_string()),
        "the README is present, so the missing-readme obligation is met, got {cs:?}"
    );
}

#[test]
fn a_readme_claim_off_the_root_warns_misplaced() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    write(&root, "README.md", CONFORMANT_README);
    // A second file claiming the readme type, off the root, conformant body so
    // only the placement check fires.
    write(&root, "docs/overview.md", CONFORMANT_README);
    let cs = codes(&root);
    assert!(
        cs.contains(&"readme-misplaced".to_string()),
        "a readme claim that is not the root README warns, got {cs:?}"
    );
}

#[test]
fn a_readme_is_queryable_via_instances_of() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: v\ntype: au.engine.repo::au-engine\n",
    );
    write(&root, "README.md", CONFORMANT_README);
    let kb = au_engine::build(&root, &RealFileSystem).expect("build");
    // The README is a first-class node, so it is queryable like any instance; no
    // dedicated read is needed.
    let recs = au_engine::wire::introspect_instances_of(&kb, "au.engine.readme", None);
    assert!(
        recs.iter().any(|r| r.path.ends_with("README.md")),
        "the README is queryable via instances_of(au.engine.readme), got {:?}",
        recs.iter().map(|r| r.path.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn a_non_editable_member_without_a_readme_is_exempt() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    // Entry composes a `discover` member; a discover member is consumed, not
    // authored, so it is exempt from the README obligation.
    write(
        &root,
        ".arsumbris/repo.yaml",
        "name: home\ntype: au.engine.repo::au-engine\n",
    );
    write(
        &root,
        ".arsumbris/workspace.yaml",
        "edit:\n  - home\ndiscover:\n  - sub\ntype: au.engine.workspace::au-engine\n",
    );
    write(&root, "README.md", CONFORMANT_README);
    // The discover member, no README of its own.
    write(
        &root,
        "sub/.arsumbris/repo.yaml",
        "name: sub\ntype: au.engine.repo::au-engine\n",
    );
    let cs = codes(&root);
    assert!(
        !cs.contains(&"repo-missing-readme".to_string()),
        "a discover member is exempt from the README obligation, got {cs:?}"
    );
}
