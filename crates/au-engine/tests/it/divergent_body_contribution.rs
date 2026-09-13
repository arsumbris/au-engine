//! A DIVERGENT field ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) filled from the markdown BODY resolves per-origin
//! through a qualified contribution (`[:field{type}]`, `[[x:field{type}]]`, or a
//! ```[:field{type}] fence`), the body twin of the frontmatter `field{type}` key.
//! A BARE body contribution to a divergent field is a `mixin-collision`.

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

/// A repo whose `c` inherits a DIVERGENT `title` from `a` (String) and `b`
/// (Number), plus the given instance body under `type: c`. `title` is OPTIONAL at
/// both origins, so the test isolates body ROUTING from the (separately tested)
/// required-per-origin rule — a required divergent field filled only from the body
/// would need a frontmatter `title{origin}:` anchor to reach `provided`.
fn build_with_entry(body: &str) -> (tempfile::TempDir, std::path::PathBuf, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &[]);
    write(&root, "type/a.type.yaml", "fields:\n  title?: String\n");
    write(&root, "type/b.type.yaml", "fields:\n  title?: Number\n");
    write(&root, "type/c.type.yaml", "extends:\n  - a\n  - b\n");
    write(&root, "entry.md", body);
    let kb = build(&root, &RealFileSystem).expect("build");
    let entry = root.join("entry.md");
    let codes = codes_on(&kb, &entry);
    (dir, entry, codes)
}

#[test]
fn qualified_inline_marker_from_body_resolves() {
    // `title{a}` binds to a's String, `title{b}` to b's Number, both filled from
    // the body — no collision, no unbound-field-binding.
    let (_d, _e, codes) =
        build_with_entry("---\ntype: c\n---\n\n`[:title{a}] hello`\n\n`[:title{b}] 42`\n");
    assert!(
        !codes.contains(&"mixin-collision".to_string())
            && !codes.contains(&"unbound-field-binding".to_string())
            && !codes.contains(&"body-slot-shape-mismatch".to_string()),
        "a fully-qualified body fill of a divergent field must resolve: {codes:?}"
    );
}

#[test]
fn qualified_wikilink_and_fence_from_body_resolve() {
    // A qualified wikilink and a qualified fence, each attributing to a distinct
    // origin of the divergent field.
    let (_d, _e, codes) = build_with_entry(
        "---\ntype: c\n---\n\n`[:title{b}] 7`\n\nSee [[a::x]] wait — `[:title{a}] hi`\n",
    );
    assert!(
        !codes.contains(&"mixin-collision".to_string())
            && !codes.contains(&"unbound-field-binding".to_string()),
        "qualified body contributions must resolve: {codes:?}"
    );
}

#[test]
fn bare_inline_marker_from_body_fires_mixin_collision() {
    // A BARE body contribution to a divergent field is ambiguous → mixin-collision.
    let (_d, _e, codes) = build_with_entry("---\ntype: c\n---\n\n`[:title] hello`\n");
    assert!(
        codes.contains(&"mixin-collision".to_string()),
        "a bare body use of a divergent field must fire mixin-collision: {codes:?}"
    );
}
