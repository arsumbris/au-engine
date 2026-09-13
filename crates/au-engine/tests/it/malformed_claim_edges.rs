//! A file whose `type:` claim fails to parse still carries real wikilink edges,
//! and the backlink index must hold them.
//!
//! `source_edges` requires `FileParse::Instance { instance: Some(..) }`, so a
//! malformed claim (`type: []`, a non-scalar) parsed to `instance: None` used to
//! contribute ZERO edges — including its perfectly well-formed body links.
//!
//! That is not a read gap. The backlink index is the WRITE PATH's reference map:
//! `rename`, `rename_type`, `promote`, `inline`, and `assign_block_id` all
//! rewrite referrer bytes off `Backlink.span`. An edge missing from the index is
//! an edge `rename` never rewrites, so renaming a target SILENTLY STRANDED every
//! live link from such a file — the state this repo's write path is not allowed
//! to produce.
//!
//! The mutation-level test is the point. Asserting the index alone would let the
//! bug come back through a handler that stops consulting it.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use serde_json::json;

use crate::wire_fixtures::{harness, Harness};

fn git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The structural refactors are git-only (compensation needs it), so a rename
/// test needs a committed member.
fn commit_all(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "tester@example.com"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "seed"]);
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A knowledge base where one referrer has a MALFORMED `type:` claim (`type: []`, which
/// [[spec - diagnostic codes::au-type-system^instance-claim-bad-shape]] rejects) but still holds
/// a body link to the rename target, and a second, well-formed referrer as the
/// control.
fn kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);

    write(&root, "type/note.type.yaml", "fields:\n  title: String\n");
    write(
        &root,
        "content/target.md",
        "---\ntype: note\ntitle: target\n---\n",
    );
    // The control: a well-formed referrer, whose link has always been indexed.
    write(
        &root,
        "content/ok.md",
        "---\ntype: note\ntitle: ok\n---\n\nsee [[target]].\n",
    );
    // The case: `type: []` names no type, so the claim does not parse and the
    // file becomes `Instance { instance: None }` — but its BODY is intact and
    // its link is a real edge.
    write(
        &root,
        "content/broken.md",
        "---\ntype: []\n---\n\nsee [[target]] too.\n",
    );
    (dir, root)
}

fn sources_referring_to(h: &mut Harness, path: &str) -> Vec<String> {
    let mut v: Vec<String> = h
        .payload("references_in", json!({ "path": path }))
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["source"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    v.sort();
    v
}

/// The index holds the malformed file's body edge. Without this, everything
/// downstream of the index silently loses the file.
#[test]
fn a_malformed_claim_still_contributes_its_body_edges() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    assert_eq!(
        sources_referring_to(&mut h, "content/target.md"),
        vec!["broken.md", "ok.md"],
        "a file whose `type:` does not parse still points at the target"
    );
}

/// THE test: renaming the target must rewrite the malformed file's link too.
/// Asserting the index alone would let this regress through a handler that
/// stops consulting it.
#[test]
fn rename_rewrites_a_referrer_whose_type_claim_is_malformed() {
    let (_d, root) = kb();
    commit_all(&root);
    let mut h = harness(&root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "content/target.md",
            "to": "content/renamed.md",
        }))
        .expect("rename");
    assert_eq!(resp["type"], "response", "the rename is accepted: {resp}");

    let broken = fs::read_to_string(root.join("content/broken.md")).unwrap();
    assert!(
        broken.contains("[[renamed]]"),
        "the malformed file's link was REWRITTEN, not stranded: {broken:?}"
    );
    assert!(
        !broken.contains("[[target]]"),
        "no stale link survives: {broken:?}"
    );

    // The control moved too, so the test is not passing because nothing moved.
    let ok = fs::read_to_string(root.join("content/ok.md")).unwrap();
    assert!(
        ok.contains("[[renamed]]"),
        "the control was rewritten: {ok:?}"
    );
}

/// The malformed file has no PARSED frontmatter, so it contributes no field
/// edges — only body ones. Pins the boundary, so a future fix does not start
/// inventing frontmatter edges it cannot attribute to a field.
#[test]
fn a_malformed_claim_contributes_no_frontmatter_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    write(&root, "type/note.type.yaml", "fields:\n  title: String\n");
    write(
        &root,
        "content/target.md",
        "---\ntype: note\ntitle: t\n---\n",
    );
    write(
        &root,
        "content/broken.md",
        "---\ntype: []\nsee: \"[[target]]\"\n---\n\nbody [[target]].\n",
    );
    let mut h = harness(&root);

    let edges = h.payload("references_in", json!({ "path": "content/target.md" }));
    let from_broken: Vec<&serde_json::Value> = edges
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["source"].as_str().unwrap().ends_with("broken.md"))
        .collect();

    assert_eq!(
        from_broken.len(),
        1,
        "exactly the body edge, the frontmatter is unparsed: {edges}"
    );
    assert_eq!(from_broken[0]["surface"], "body");
}
