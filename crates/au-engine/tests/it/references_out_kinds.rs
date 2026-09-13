//! `references_out` returns EVERY outgoing edge, frontmatter and body, each
//! classified by kind.
//!
//! Before this the answer was scattered: body-only from this read, typed
//! frontmatter references only via the `instance` read, and a `[[...]]` inside
//! a frontmatter string nowhere in the forward direction at all.
//!
//! The kind is load-bearing, not cosmetic — it decides how a consumer acts on
//! the edge and how severe a break is. A `field-reference` is a structural
//! dependency whose dangling case is an error; a `navigational` link is a hint
//! whose dangling case is a warning.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::wire_fixtures::{harness, Harness};

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// One instance carrying all four edge kinds, plus a dangling one of each
/// surface so resolved-vs-dangling is observable per kind.
///
/// `note` declares:
/// - `rel: note*` — a reference-admitting slot, so a whole-value wikilink there
///   is the structural `field-reference`.
/// - `refs: note*[]` — the list form, so each element is its own edge.
/// - `see-also: String` — NOT reference-admitting, so a wikilink there is an
///   intended-but-untyped `field-string-wikilink`, which is the case that was
///   entirely absent from the forward direction.
/// - `note-on: String` — holds a link EMBEDDED in a longer string, the other
///   `field-string-wikilink` route.
/// - `via: note*` — filled from the BODY by a `[[target:via]]` contribution.
fn kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);

    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  title: String\n  rel?: note*\n  refs?: \"note*[]\"\n  \
         see-also?: String\n  note-on?: String\n  via?: note*\n",
    );
    write(&root, "content/a.md", "---\ntype: note\ntitle: a\n---\n");
    write(&root, "content/b.md", "---\ntype: note\ntitle: b\n---\n");
    write(&root, "content/moc.md", "# moc\n");

    write(
        &root,
        "content/src.md",
        concat!(
            "---\n",
            "type: note\n",
            "title: src\n",
            "rel: \"[[a]]\"\n",
            "refs:\n  - \"[[a]]\"\n  - \"[[b]]\"\n",
            "see-also: \"[[b]]\"\n",
            "note-on: \"see [[moc]] for context\"\n",
            "via:\n",
            "missing-ref: \"[[nowhere]]\"\n",
            "---\n",
            "\n",
            "Prose mentioning [[moc]].\n",
            "\n",
            "Contributed from [[b:via]].\n",
            "\n",
            "A dangling prose link [[gone]].\n",
        ),
    );

    (dir, root)
}

fn edges(h: &mut Harness) -> Value {
    h.payload("references_out", json!({ "path": "content/src.md" }))
}

/// Every edge matching a kind, as `(field, target, resolved-or-not)`.
fn of_kind(edges: &Value, kind: &str) -> Vec<(String, String, bool)> {
    edges
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .map(|e| {
            (
                e["field"].as_str().unwrap_or("").to_string(),
                e["target"].as_str().unwrap().to_string(),
                !e["resolved"].is_null(),
            )
        })
        .collect()
}

/// A frontmatter slot that ADMITS a reference yields the structural kind — the
/// edge that was previously reachable only through the `instance` read.
#[test]
fn a_reference_slot_yields_field_reference() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    let refs = of_kind(&out, "field-reference");
    assert_eq!(
        refs,
        vec![
            ("rel".to_string(), "a".to_string(), true),
            ("refs".to_string(), "a".to_string(), true),
            ("refs".to_string(), "b".to_string(), true),
        ],
        "the bare slot and BOTH list elements, each its own edge: {out}"
    );

    for e in out
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "field-reference")
    {
        assert_eq!(e["surface"], "frontmatter");
    }
}

/// A frontmatter slot that does NOT admit a reference yields the untyped
/// pointer kind — whole-value or embedded alike. This is the category that was
/// absent from the forward direction entirely.
#[test]
fn a_non_reference_slot_yields_field_string_wikilink() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    let untyped = of_kind(&out, "field-string-wikilink");
    assert!(
        untyped.contains(&("see-also".to_string(), "b".to_string(), true)),
        "a whole-value link in a String slot is untyped, not structural: {untyped:?}"
    );
    assert!(
        untyped.contains(&("note-on".to_string(), "moc".to_string(), true)),
        "a link embedded in a longer string is untyped too: {untyped:?}"
    );
    // An EXTRA field (outside the effective shape) has no slot to admit
    // anything, so it lands here rather than being silently dropped.
    assert!(
        untyped.contains(&("missing-ref".to_string(), "nowhere".to_string(), false)),
        "an extra field's link still surfaces, dangling: {untyped:?}"
    );
}

/// The frontmatter split is decided by the SLOT, never by the value's syntax.
/// `see-also: "[[b]]"` and `rel: "[[a]]"` are syntactically identical and
/// classify differently.
#[test]
fn the_frontmatter_split_is_slot_gated_not_syntax_gated() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    let by_field = |f: &str| -> String {
        out.as_array()
            .unwrap()
            .iter()
            .find(|e| e["field"] == f && e["surface"] == "frontmatter")
            .unwrap_or_else(|| panic!("an edge for {f}: {out}"))["kind"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(by_field("rel"), "field-reference");
    assert_eq!(
        by_field("see-also"),
        "field-string-wikilink",
        "identical syntax, different slot, different kind"
    );
}

/// A body `[[target:field]]` is both a link and a data contribution, so it
/// carries its own kind and names the field it fills.
#[test]
fn a_body_attribution_yields_contributing() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    assert_eq!(
        of_kind(&out, "contributing"),
        vec![("via".to_string(), "b".to_string(), true)],
        "the contribution names the field it supplies: {out}"
    );
}

/// A bare body link is the hint kind, and carries no field.
#[test]
fn a_bare_body_link_yields_navigational() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    let nav = of_kind(&out, "navigational");
    assert!(nav.contains(&(String::new(), "moc".to_string(), true)));
    assert!(
        nav.contains(&(String::new(), "gone".to_string(), false)),
        "a dangling prose link still surfaces: {nav:?}"
    );
    for e in out
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "navigational")
    {
        assert_eq!(e["surface"], "body");
        assert!(e["field"].is_null(), "a bare link fills no field: {e}");
    }
}

/// The read is COMPLETE: one call answers "what does this connect to", with
/// both surfaces present. That completeness is the request's actual point.
#[test]
fn one_call_returns_both_surfaces_and_all_four_kinds() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    let mut kinds: Vec<&str> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    kinds.sort();
    kinds.dedup();
    assert_eq!(
        kinds,
        vec![
            "contributing",
            "field-reference",
            "field-string-wikilink",
            "navigational"
        ],
        "all four kinds present in one answer: {out}"
    );

    let mut surfaces: Vec<&str> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["surface"].as_str().unwrap())
        .collect();
    surfaces.sort();
    surfaces.dedup();
    assert_eq!(surfaces, vec!["body", "frontmatter"]);

    // Frontmatter edges come first, then body, so the order is stable and
    // groupable without a sort.
    let first_body = out
        .as_array()
        .unwrap()
        .iter()
        .position(|e| e["surface"] == "body")
        .unwrap();
    assert!(
        out.as_array().unwrap()[..first_body]
            .iter()
            .all(|e| e["surface"] == "frontmatter"),
        "frontmatter edges precede body edges: {out}"
    );
}

/// Resolution info rides every edge regardless of kind, so a consumer never
/// needs a second read to follow one.
#[test]
fn every_edge_carries_its_resolution_info() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let out = edges(&mut h);

    for e in out.as_array().unwrap() {
        assert!(e["target"].is_string(), "a bare target: {e}");
        assert!(e["span"]["start"].is_u64(), "a byte span: {e}");
        assert!(
            e["span"]["line_col"]["start"]["line"].is_u64(),
            "a line/col rendering: {e}"
        );
        assert!(
            e["resolved"].is_string() || e["resolved"].is_null(),
            "resolved-or-dangling, never absent: {e}"
        );
    }
}

/// An UNRESOLVED instance has no effective shape, so the engine cannot say
/// which frontmatter kind an edge is — and says so, rather than asserting one.
///
/// `field-string-wikilink` is documented as a POSITIVE claim that the slot does
/// not admit a reference, so reporting it here would be a lie, not a shrug.
#[test]
fn an_unresolved_instance_reports_unknown_not_a_false_kind() {
    let (_d, root) = kb();
    write(
        &root,
        "content/unresolved.md",
        "---\ntype: nosuchtype\nrel: \"[[a]]\"\n---\n",
    );
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "content/unresolved.md" }));
    assert_eq!(
        of_kind(&out, "unknown"),
        vec![("rel".to_string(), "a".to_string(), true)],
        "the edge surfaces, honestly unclassified: {out}"
    );
    assert_eq!(of_kind(&out, "field-reference"), vec![], "{out}");
    assert_eq!(
        of_kind(&out, "field-string-wikilink"),
        vec![],
        "no false claim that the slot rejects references: {out}"
    );
}

/// `unknown` is NARROW. Even with no shape, a body edge is settled by the
/// link's syntax and an EMBEDDED frontmatter link is settled at the value
/// level, so neither degrades.
#[test]
fn unknown_does_not_swallow_the_cases_that_stay_decidable() {
    let (_d, root) = kb();
    write(
        &root,
        "content/unresolved2.md",
        "---\ntype: nosuchtype\nnote-on: \"see [[moc]] here\"\n---\n\nprose [[a]] and [[b:via]].\n",
    );
    let mut h = harness(&root);

    let out = h.payload(
        "references_out",
        json!({ "path": "content/unresolved2.md" }),
    );
    assert_eq!(
        of_kind(&out, "field-string-wikilink"),
        vec![("note-on".to_string(), "moc".to_string(), true)],
        "an embedded link needs no shape to classify: {out}"
    );
    assert_eq!(of_kind(&out, "navigational").len(), 1, "{out}");
    assert_eq!(of_kind(&out, "contributing").len(), 1, "{out}");
    assert_eq!(
        of_kind(&out, "unknown"),
        vec![],
        "nothing else degrades: {out}"
    );
}

/// A commit-only reference names a COMMIT, not a file: it carries its own kind,
/// `resolved` is null, `commit` is set, and it must NOT read as a broken link.
/// Both the this-repo (`[[::@sha]]`) and peer (`[[::repo@sha]]`) forms, on both
/// surfaces.
#[test]
fn a_commit_only_reference_yields_commit_referent() {
    let (_d, root) = kb();
    // `commit-of` is an EXTRA field (outside note's shape) holding a this-repo
    // commit-referent; the body holds a peer one. A commit-referent is settled
    // by its own syntax, so the slot never matters.
    write(
        &root,
        "content/span.md",
        "---\ntype: note\ntitle: span\ncommit-of: \"[[::@a1b2c3d]]\"\n---\n\nProduced [[::peer@a1b2c3d]].\n",
    );
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "content/span.md" }));
    let referents: Vec<&Value> = out
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "commit-referent")
        .collect();
    assert_eq!(referents.len(), 2, "both commit-referents surface: {out}");
    for e in &referents {
        assert!(
            e["resolved"].is_null(),
            "a commit-referent resolves to no file: {e}"
        );
        assert_eq!(
            e["target"], "",
            "a commit-referent has an empty target: {e}"
        );
        assert!(e["commit"].is_string(), "the commit rides the edge: {e}");
    }
    // The peer form keeps its `::repo` qualifier; the this-repo form omits it.
    assert!(
        referents.iter().any(|e| e["repo"] == "peer"),
        "the peer form keeps its ::repo: {out}"
    );
    assert!(
        referents.iter().any(|e| e["repo"].is_null()),
        "the this-repo form has no repo: {out}"
    );
}

/// A NAMED-target pin (`[[file::@sha]]`) keeps its ordinary kind but carries
/// `commit` with `resolved: null`. The contract: a commit-bearing edge is an
/// inert pin, never dangling, so a consumer reads `commit`, not the kind, before
/// calling a null `resolved` broken. Distinct from the empty-target
/// `commit-referent`, which gets its own kind.
#[test]
fn a_named_target_pin_carries_commit_and_is_not_a_commit_referent() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    // `snap: file*@` is a legal pinned slot. The target exists live, but the pin
    // is inert either way, so `resolved` is null regardless.
    write(&root, "type/log.type.yaml", "fields:\n  snap: file*@\n");
    write(
        &root,
        "target.md",
        "---\ntype: log\nsnap: \"[[target::@a1b2c3d]]\"\n---\n",
    );
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "target.md" }));
    let edges = out.as_array().unwrap();
    let snap = edges
        .iter()
        .find(|e| e["commit"] == "a1b2c3d")
        .unwrap_or_else(|| panic!("the pinned edge is missing: {out}"));

    assert!(
        snap["resolved"].is_null(),
        "a pin resolves to no live file: {snap}"
    );
    assert_eq!(
        snap["target"], "target",
        "the named target rides the edge: {snap}"
    );
    assert_ne!(
        snap["kind"], "commit-referent",
        "a named pin keeps its structural kind, only the empty-target form is a commit-referent: {snap}"
    );
}

/// A plain note (no `type:`) still reports its body edges, so the read is not
/// silently empty for the untyped half of a knowledge base.
#[test]
fn a_plain_note_reports_its_body_edges() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "content/moc.md" }));
    assert_eq!(out, json!([]), "this note links nowhere");

    write(&root, "content/moc2.md", "# moc2\n\nsee [[a]].\n");
    let mut h2 = harness(&root);
    let out = h2.payload("references_out", json!({ "path": "content/moc2.md" }));
    assert_eq!(of_kind(&out, "navigational").len(), 1, "{out}");
}

/// A plain note's WHOLE-VALUE frontmatter wikilink is `field-string-wikilink`,
/// not `unknown`. A note has no `type:` claim, so it is DEFINITIVELY untyped —
/// no slot can admit a reference — which is knowledge, not uncertainty. `unknown`
/// is reserved for a typed instance whose slot genuinely cannot be decided (an
/// unresolved claim, an aborted repo). A MOC-style `up: "[[parent]]"` note is a
/// very common input, so mislabelling it `unknown` would be a frequent lie.
#[test]
fn a_notes_whole_value_frontmatter_link_is_untyped_not_unknown() {
    let (_d, root) = kb();
    // A note: frontmatter, but no `type:`. `up` / `related` hold whole-value
    // wikilinks; `note-on` embeds one in a longer string.
    write(
        &root,
        "content/moc3.md",
        "---\nup: \"[[a]]\"\nrelated: \"[[b]]\"\nnote-on: \"see [[a]] too\"\n---\n\n# moc3\n",
    );
    let mut h = harness(&root);

    let out = h.payload("references_out", json!({ "path": "content/moc3.md" }));
    assert_eq!(
        of_kind(&out, "unknown"),
        vec![],
        "a note is definitively untyped, never uncertain: {out}"
    );
    let untyped = of_kind(&out, "field-string-wikilink");
    assert!(
        untyped.contains(&("up".to_string(), "a".to_string(), true)),
        "a whole-value frontmatter link on a note is an untyped pointer: {out}"
    );
    assert!(
        untyped.contains(&("related".to_string(), "b".to_string(), true)),
        "{out}"
    );
    assert!(
        untyped.contains(&("note-on".to_string(), "a".to_string(), true)),
        "the embedded link is untyped too: {out}"
    );
    assert_eq!(
        of_kind(&out, "field-reference"),
        vec![],
        "a note has no reference slots: {out}"
    );
}
