//! End-to-end coverage of the required-subtype-meta obligation
//! ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]):
//! a base declares `required: X` in its `meta:`, every concrete subtype must
//! carry a satisfying meta block, checked over the full build.

use std::fs;
use std::path::Path;

use au_diagnostics::Severity;
use au_engine::build;
use au_parser::RealFileSystem;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// One repo `r`: a meta type `pm` (mixes the engine marker), a subtype `pm.plus`,
/// an abstract base obligating `pm`, and a spread of concrete subtypes.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: r\n");
    // A meta type and a subtype of it (satisfies by closure).
    write(
        &root,
        "type/pm.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  text?: String\n",
    );
    write(&root, "type/pm.plus.type.yaml", "extends: pm\nfields: {}\n");
    // An abstract base obligating every concrete subtype to carry `pm`.
    write(
        &root,
        "type/base.type.yaml",
        "abstract: true\nmeta:\n  - required: pm\n",
    );
    // Concrete subtypes: missing / carrying / carrying-a-subtype / emptied.
    write(
        &root,
        "type/tool.missing.type.yaml",
        "extends: base\nfields: {}\n",
    );
    write(
        &root,
        "type/tool.carries.type.yaml",
        "extends: base\nmeta:\n  - type: pm\n    text: hi\n",
    );
    write(
        &root,
        "type/tool.plus.type.yaml",
        "extends: base\nmeta:\n  - type: pm.plus\n",
    );
    write(
        &root,
        "type/tool.empty.type.yaml",
        "extends: base\nmeta: []\n",
    );
    // A concrete base that obligates itself (reflexive) and does not carry `pm`.
    write(&root, "type/cbase.type.yaml", "meta:\n  - required: pm\n");
    (dir, root)
}

fn fires_missing(kb: &au_engine::KnowledgeBase, type_file: &Path) -> bool {
    kb.diagnostics()
        .any(|d| d.span.file == type_file && d.code.as_str() == "subtype-missing-required-meta")
}

#[test]
fn the_obligation_fires_and_clears_across_the_family() {
    let (_dir, root) = fixture();
    let v = build(&root, &RealFileSystem).expect("build");

    // A concrete subtype with no satisfying block is flagged (warning).
    assert!(
        fires_missing(&v, &root.join("type/tool.missing.type.yaml")),
        "a concrete subtype missing the required meta fires"
    );
    let d = v
        .diagnostics()
        .find(|d| {
            d.span.file == root.join("type/tool.missing.type.yaml")
                && d.code.as_str() == "subtype-missing-required-meta"
        })
        .unwrap();
    assert_eq!(d.severity, Severity::Warning);
    assert!(
        !d.related.is_empty(),
        "related points at the obligating base"
    );

    // Carrying the meta, or a subtype of it, clears the obligation.
    assert!(!fires_missing(
        &v,
        &root.join("type/tool.carries.type.yaml")
    ));
    assert!(!fires_missing(&v, &root.join("type/tool.plus.type.yaml")));

    // `meta: []` exempts nothing.
    assert!(fires_missing(&v, &root.join("type/tool.empty.type.yaml")));

    // The abstract base is exempt (not instance-claimable).
    assert!(!fires_missing(&v, &root.join("type/base.type.yaml")));

    // A concrete declaring base is on its own hook (reflexive).
    assert!(fires_missing(&v, &root.join("type/cbase.type.yaml")));

    // The read surfaces the same computation without parsing diagnostics.
    let missing = au_engine::wire::introspect_type_by_name(&v, "tool.missing").unwrap();
    assert_eq!(missing.def.unmet_required_meta, vec!["pm".to_string()]);
    assert!(!missing.def.is_abstract);
    let carries = au_engine::wire::introspect_type_by_name(&v, "tool.carries").unwrap();
    assert!(carries.def.unmet_required_meta.is_empty());
    // The base exposes its declared obligation and its abstract marker.
    let base = au_engine::wire::introspect_type_by_name(&v, "base").unwrap();
    assert_eq!(base.def.required_meta, vec!["pm".to_string()]);
    assert!(base.def.is_abstract);
    assert!(
        base.def.unmet_required_meta.is_empty(),
        "an abstract base is exempt, so nothing unmet"
    );
}

/// A base in `base` obligates `pm`; a concrete subtype in `app` extending the
/// peer base inherits the obligation, and satisfies it with a peer meta block.
fn cross_repo_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    write(&root, "base/.arsumbris/repo.yaml", "name: base\n");
    write(
        &root,
        "base/type/pm.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  text?: String\n",
    );
    write(
        &root,
        "base/type/tool.type.yaml",
        "abstract: true\nmeta:\n  - required: pm\n",
    );
    write(
        &root,
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // Concrete subtype of the peer base, no block → inherits the obligation.
    write(
        &root,
        "app/type/mytool.type.yaml",
        "extends: tool::base\nfields: {}\n",
    );
    // Concrete subtype carrying the peer meta → satisfied across the boundary.
    write(
        &root,
        "app/type/mytool.ok.type.yaml",
        "extends: tool::base\nmeta:\n  - type: pm::base\n",
    );
    (dir, root)
}

#[test]
fn a_cross_repo_base_obligation_is_inherited_and_satisfiable() {
    let (_dir, root) = cross_repo_fixture();
    let v = build(&root, &RealFileSystem).expect("build");
    assert!(
        fires_missing(&v, &root.join("app/type/mytool.type.yaml")),
        "a concrete subtype of a peer base inherits its obligation"
    );
    assert!(
        !fires_missing(&v, &root.join("app/type/mytool.ok.type.yaml")),
        "a peer meta block satisfies the inherited obligation across the boundary"
    );
}
