//! `pins` is the reverse-by-target lookup for commit-pinned references.
//!
//! An inert pin forms no inbound backlink (see `backlinks` / `references_in`),
//! so "which sources pin this name" is a fold over the retained OUTBOUND pins,
//! not a backlink question. `pins { target, source_type }` returns every pin
//! naming `target` whose source file contains an instance of `source_type`.
//!
//! The fixture mirrors au-provenance's shape: a ledger file whose pins live in
//! NESTED inline records, plus a top-level pin, plus an out-of-scope carrier of
//! a same-named pin. See [[spec - pinned references - a recorded resolved edge
//! with an immutable past and an on-demand forward trace]].

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

/// A ledger file whose reverse-read pins nest inside `span` records (au-
/// provenance's shape), a top-level pin on the same file, and a SEPARATE
/// `other` file pinning the same name `foo` — the out-of-scope carrier the
/// `source_type` scope must exclude.
///
/// - `span.read: file*@[]` — the nested pin list, inside a `spans: span[]`.
/// - `ledger.note: file*@` — a top-level frontmatter pin on the same file.
/// - `other.ref: file*@` — a pin to `foo` from an out-of-scope type.
///
/// The pinned targets need not exist as files: a pin is inert, it names a
/// coordinate into an immutable past, never a live edge. The commits are hex
/// oids so they parse as pins.
fn kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);

    write(
        &root,
        "type/span.type.yaml",
        "fields:\n  read?: \"file*@[]\"\n",
    );
    write(
        &root,
        "type/ledger.type.yaml",
        "fields:\n  spans?: \"span[]\"\n  note?: \"file*@\"\n  xref?: \"file*@\"\n",
    );
    write(
        &root,
        "type/other.type.yaml",
        "fields:\n  ref?: \"file*@\"\n",
    );

    // Two pins name `foo` (at different commits), one names `bar`, all inside a
    // nested span record; plus a top-level `note` pin naming `foo` a third time.
    // `xref` is a CROSS-REPO pin (`::base`) naming `baz`, and the body carries an
    // empty-target commit-referent — neither names foo/bar, so the counts above
    // stay intact.
    write(
        &root,
        "content/ledger-1.md",
        concat!(
            "---\n",
            "type: ledger\n",
            "spans:\n",
            "  - ^: s1\n",
            "    read:\n",
            "      - \"[[foo::@aaa1111]]\"\n",
            "      - \"[[bar::@bbb2222]]\"\n",
            "note: \"[[foo::@ccc3333]]\"\n",
            "xref: \"[[baz::base@fff6666]]\"\n",
            "---\n",
            "\n",
            "A commit-referent in prose [[::@eee5555]] names a commit, not a file.\n",
        ),
    );
    // The out-of-scope carrier: it pins `foo` too, but is not a `ledger`.
    write(
        &root,
        "content/other-1.md",
        "---\ntype: other\nref: \"[[foo::@ddd4444]]\"\n---\n",
    );

    (dir, root)
}

fn pins(h: &mut Harness, target: &str, source_type: &str) -> Vec<Value> {
    h.payload(
        "pins",
        json!({ "target": target, "source_type": source_type }),
    )
    .as_array()
    .expect("pins is an array")
    .clone()
}

/// The pinned commits in a result, sorted for a stable compare.
fn commits(recs: &[Value]) -> Vec<String> {
    let mut c: Vec<String> = recs
        .iter()
        .map(|r| r["commit"].as_str().unwrap().to_string())
        .collect();
    c.sort();
    c
}

fn by_commit<'a>(recs: &'a [Value], commit: &str) -> &'a Value {
    recs.iter()
        .find(|r| r["commit"] == commit)
        .unwrap_or_else(|| panic!("no record for commit {commit}: {recs:?}"))
}

/// Every pin naming the target within the scope comes back, the nested one and
/// the top-level one alike. Two pins name `foo` at different commits — both
/// return, the engine does NOT window or dedupe by name (that is the consumer's
/// job).
#[test]
fn returns_every_pin_naming_the_target_in_scope() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let recs = pins(&mut h, "foo", "ledger");

    assert_eq!(
        commits(&recs),
        vec!["aaa1111".to_string(), "ccc3333".to_string()],
        "both foo pins (nested read + top-level note), not bar, not other-1: {recs:?}"
    );

    // The nested pin carries its enclosing record id and its field.
    let nested = by_commit(&recs, "aaa1111");
    assert_eq!(nested["source_block_id"], "s1");
    assert_eq!(nested["slot"], "read");
    assert_eq!(nested["surface"], "frontmatter");
    assert_eq!(nested["target"], "foo");
    assert!(nested["source"].as_str().unwrap().ends_with("ledger-1.md"));
    assert!(
        nested.get("repo").is_none(),
        "own-repo pin carries no repo: {nested}"
    );

    // The top-level pin has a slot but no enclosing record.
    let top = by_commit(&recs, "ccc3333");
    assert_eq!(top["slot"], "note");
    assert!(
        top.get("source_block_id").is_none(),
        "a top-level pin has no enclosing record: {top}"
    );
}

/// `source_type` scopes to the FILES containing an instance of the type. The
/// nested `span` type and the file-level `ledger` type both key to the same
/// ledger file, so scoping by either yields the same pins — exactly au-
/// provenance's case, where the pin-carrier is a nested record type.
#[test]
fn a_nested_source_type_scopes_to_the_containing_file() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    let by_file = pins(&mut h, "foo", "ledger");
    let by_nested = pins(&mut h, "foo", "span");
    assert_eq!(
        commits(&by_file),
        commits(&by_nested),
        "scoping by the nested type or the file type finds the same file's pins"
    );
}

/// The `source_type` scope excludes a same-named pin held by an out-of-scope
/// file. `other-1` pins `foo` too, but is not a `ledger`.
#[test]
fn source_type_excludes_out_of_scope_pins() {
    let (_d, root) = kb();
    let mut h = harness(&root);

    let in_ledger = pins(&mut h, "foo", "ledger");
    assert!(
        !commits(&in_ledger).contains(&"ddd4444".to_string()),
        "the other-1 pin is out of scope for source_type=ledger: {in_ledger:?}"
    );

    let in_other = pins(&mut h, "foo", "other");
    assert_eq!(
        commits(&in_other),
        vec!["ddd4444".to_string()],
        "scoping by `other` returns only its own pin: {in_other:?}"
    );
}

/// The target filter is exact and per-name: `bar` returns only the `bar` pin,
/// with its nested-record provenance intact.
#[test]
fn target_filter_is_exact() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let recs = pins(&mut h, "bar", "ledger");

    assert_eq!(commits(&recs), vec!["bbb2222".to_string()], "{recs:?}");
    assert_eq!(recs[0]["source_block_id"], "s1");
    assert_eq!(recs[0]["slot"], "read");
    assert_eq!(recs[0]["target"], "bar");
}

/// A cross-repo pin (`[[baz::base@sha]]`) carries its `::repo` scope through to
/// the record. The one field the other tests only assert ABSENT (an own-repo
/// pin carries no `repo`), locked here with a positive case.
#[test]
fn a_cross_repo_pin_round_trips_its_repo() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    let recs = pins(&mut h, "baz", "ledger");

    assert_eq!(commits(&recs), vec!["fff6666".to_string()], "{recs:?}");
    assert_eq!(recs[0]["repo"], "base");
    assert_eq!(recs[0]["slot"], "xref");
    assert_eq!(recs[0]["target"], "baz");
}

/// An empty-target commit-referent (`[[::@sha]]`) names a commit, not a file, so
/// it never surfaces for a named target — guarding the `is_inert_pin` superset
/// edge, where the target filter (`w.target != target`) is what excludes it.
#[test]
fn a_commit_referent_never_surfaces_for_a_named_target() {
    let (_d, root) = kb();
    let mut h = harness(&root);
    for target in ["foo", "bar", "baz"] {
        let recs = pins(&mut h, target, "ledger");
        assert!(
            !commits(&recs).contains(&"eee5555".to_string()),
            "the commit-referent eee5555 must not appear for target {target}: {recs:?}"
        );
    }
}
