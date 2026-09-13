//! The two reference directions must agree.
//!
//! `references_out` (forward) and `references_in` (inverse) derive from ONE
//! traversal, `backlinks::walk_edges`. This asserts the property that makes
//! that worth doing: for every resolved outgoing edge A → B, B's inbound set
//! contains its mirror.
//!
//! It exists because the two used to be independent walks, and nothing checked
//! them against each other. Three divergences accumulated unnoticed:
//! - an untyped NOTE's frontmatter links were in the index, absent from the read
//! - `source_block_id` was on the inbound edge only
//! - the local form (`[[^id]]`) resolved to self inbound, reported dangling outbound
//!
//! All three would have failed these tests on the day they appeared. That is
//! the point: agreement is now checkable rather than something a reviewer has
//! to notice.
//!
//! KNOWN LIMIT, verified by breaking each fix in turn. Now that both directions
//! derive from one walk, a bug in the SHARED traversal breaks them SYMMETRICALLY
//! and the set-agreement test still passes — reverting the local-form fix leaves
//! it green. What it now guards is the PROJECTIONS (which edges each direction
//! keeps, and how it maps them), and it does catch those: dropping a note's
//! frontmatter edges from the index alone fails it immediately.
//!
//! So the two tests do different jobs and both are needed. Agreement guards the
//! projections; `the_previously_diverging_shapes_agree` pins the SPEC behaviour
//! of each shape, and it is what caught the reverted local-form fix.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::json;

use crate::wire_fixtures::{harness, Harness};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A knowledge base deliberately built from the shapes the two walkers used to disagree
/// on, plus the ordinary ones. Every file here is a source whose edges the test
/// mirrors.
fn kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);

    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  title: String\n  rel?: note*\n  refs?: \"note*[]\"\n  \
         see?: String\n  inner?: box\n",
    );
    write(&root, "type/box.type.yaml", "fields:\n  link?: note*\n");

    write(&root, "content/a.md", "---\ntype: note\ntitle: a\n---\n");
    write(&root, "content/b.md", "---\ntype: note\ntitle: b\n---\n");

    // An ordinary typed instance: reference slot, list slot, string slot, body.
    write(
        &root,
        "content/typed.md",
        concat!(
            "---\n",
            "type: note\n",
            "title: typed\n",
            "rel: \"[[a]]\"\n",
            "refs:\n  - \"[[a]]\"\n  - \"[[b]]\"\n",
            "see: \"prose [[b]] inline\"\n",
            "---\n\nbody [[a]] and [[b:rel]].\n",
        ),
    );

    // DIVERGENCE 1: an untyped note with a frontmatter link.
    write(
        &root,
        "content/note.md",
        "---\nsee: \"[[a]]\"\n---\n\nand body [[b]].\n",
    );

    // DIVERGENCE 2: an edge originating inside an inline record carrying `^:`.
    write(
        &root,
        "content/record.md",
        "---\ntype: note\ntitle: rec\ninner:\n  ^: r1\n  link: \"[[a]]\"\n---\n",
    );

    // DIVERGENCE 3: the local form, which resolves to the host file itself.
    write(
        &root,
        "content/local.md",
        "---\ntype: note\ntitle: loc\n---\n\nA para. ^p1\n\nsee [[^p1]] and [[#Nowhere]].\n",
    );

    // A malformed claim: body edges only, but they must still mirror.
    write(
        &root,
        "content/malformed.md",
        "---\ntype: []\n---\n\nsee [[a]].\n",
    );

    (dir, root)
}

fn files() -> Vec<&'static str> {
    vec![
        "content/typed.md",
        "content/note.md",
        "content/record.md",
        "content/local.md",
        "content/malformed.md",
    ]
}

/// An edge identified the same way from both directions: (source, target, span).
/// The span is what makes it an identity rather than a count — it is also the
/// field the write path rewrites bytes at, so a mismatch here is a rewrite bug.
type EdgeKey = (String, String, u64);

fn outgoing(h: &mut Harness, path: &str) -> BTreeSet<EdgeKey> {
    let src = abs(h, path);
    h.payload("references_out", json!({ "path": path }))
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| {
            let resolved = e["resolved"].as_str()?;
            Some((
                src.clone(),
                resolved.to_string(),
                e["span"]["start"].as_u64().unwrap(),
            ))
        })
        .collect()
}

fn inbound(h: &mut Harness, path: &str) -> BTreeSet<EdgeKey> {
    let target = abs(h, path);
    h.payload("references_in", json!({ "path": path }))
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["source"].as_str().unwrap().to_string(),
                target.clone(),
                e["span_start"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Resolve a repo-relative path to the absolute form both reads report.
fn abs(h: &mut Harness, path: &str) -> String {
    h.payload("resolve_member", json!({ "path": path }))["root"]
        .as_str()
        .map(|root| format!("{root}/{path}"))
        .expect("the member root")
}

/// THE invariant: every resolved outgoing edge appears in its target's inbound
/// set, and every inbound edge appears in its source's outgoing set. Both
/// directions, so neither can quietly hold an edge the other lacks.
#[test]
fn every_resolved_edge_appears_in_both_directions() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    let mut out_all: BTreeSet<EdgeKey> = BTreeSet::new();
    for f in files() {
        out_all.extend(outgoing(&mut h, f));
    }
    assert!(
        !out_all.is_empty(),
        "the fixture produces edges, else this test proves nothing"
    );

    let mut in_all: BTreeSet<EdgeKey> = BTreeSet::new();
    for f in files().into_iter().chain(["content/a.md", "content/b.md"]) {
        in_all.extend(inbound(&mut h, f));
    }

    let only_out: Vec<&EdgeKey> = out_all.difference(&in_all).collect();
    assert!(
        only_out.is_empty(),
        "edges the FORWARD read reports but the index lacks \
         (the write path would not rewrite these): {only_out:#?}"
    );

    let only_in: Vec<&EdgeKey> = in_all.difference(&out_all).collect();
    assert!(
        only_in.is_empty(),
        "edges the INDEX holds but the forward read omits \
         (a consumer asking 'what do I point at' would miss these): {only_in:#?}"
    );
}

/// The three specific shapes that used to diverge, each pinned by name so a
/// regression says WHICH one broke rather than just "the sets differ".
#[test]
fn the_previously_diverging_shapes_agree() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    // 1. An untyped note's FRONTMATTER link is in both directions.
    let note_out = outgoing(&mut h, "content/note.md");
    let a_in = inbound(&mut h, "content/a.md");
    assert!(
        note_out.iter().any(|e| a_in.contains(e)),
        "a note's frontmatter edge is in both directions: out={note_out:#?}"
    );

    // 2. An edge inside an inline record carries its `^:` id forward.
    let rec = h.payload("references_out", json!({ "path": "content/record.md" }));
    let edge = &rec.as_array().unwrap()[0];
    assert_eq!(
        edge["source_block_id"], "r1",
        "the outgoing edge names the record it came from: {rec}"
    );
    let a_edges = h.payload("references_in", json!({ "path": "content/a.md" }));
    let mirror = a_edges
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source"].as_str().unwrap().ends_with("record.md"))
        .expect("the inbound mirror");
    assert_eq!(
        mirror["source_block_id"], edge["source_block_id"],
        "both directions name the same record"
    );

    // 3. The local form resolves to the host file, never dangling.
    let loc = h.payload("references_out", json!({ "path": "content/local.md" }));
    for e in loc.as_array().unwrap() {
        assert!(
            e["resolved"].is_string(),
            "a local-form link resolves to its own file, it is not dangling: {e}"
        );
        assert!(
            e["resolved"].as_str().unwrap().ends_with("local.md"),
            "and it resolves to THIS file: {e}"
        );
    }
}

/// A dangling edge is forward-only BY DESIGN, and this pins that asymmetry as
/// intentional rather than the kind the test above forbids.
///
/// The index drives the write path: `rename` rewrites referrer bytes off each
/// edge's span, so an unresolved edge must not enter it. The read reports it
/// because naming a broken link is half its job.
#[test]
fn a_dangling_edge_is_forward_only_on_purpose() {
    let (_d, root) = kb();
    write(
        &root,
        "content/dangling.md",
        "---\ntype: note\ntitle: d\nrel: \"[[nowhere]]\"\n---\n\nand [[alsogone]].\n",
    );
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "content/dangling.md" }));
    let dangling: Vec<&serde_json::Value> = out
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["resolved"].is_null())
        .collect();
    assert_eq!(
        dangling.len(),
        2,
        "both broken links surface forward: {out}"
    );

    // And they are absent from the mirror set, which is why the agreement test
    // above filters on `resolved`.
    assert!(
        outgoing(&mut h, "content/dangling.md").is_empty(),
        "no RESOLVED edge, so nothing to mirror"
    );
}
