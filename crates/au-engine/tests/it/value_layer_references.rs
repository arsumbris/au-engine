//! The value layer decides reference-ness by the SLOT, on every surface.
//!
//! A value container is a `reference` iff its slot admits one AND the value is
//! exactly one whole-value `[[...]]` — [[type reference::au-type-system]]'s validated-reference
//! definition, applied uniformly. Frontmatter used to produce no references at
//! all and the body gated on syntax, so the same target written on both
//! surfaces yielded two structurally different containers, defeated the
//! spec-mandated collapse, and emitted a spurious `field-cardinality-exceeded`.
//!
//! `backlinks` is deliberately NOT covered here: it answers the NAVIGATIONAL
//! question (any `[[...]]` anywhere is an edge), which is a different question
//! and already correct.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

struct Harness {
    _sock_dir: tempfile::TempDir,
    _server: ServeHandle,
    client: Client,
}

fn harness(root: &Path) -> Harness {
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    engine.rebuild();
    let server = serve(engine.handle(), &socket).expect("serve");
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _sock_dir: sock_dir,
        _server: server,
        client,
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A knowledge base whose `note` type carries one slot of each interesting shape, plus a
/// `paper` target to point at.
fn kb(note_fields: &str, instance: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/paper.type.yaml", "fields:\n  t: String\n");
    write(
        &root,
        "type/note.type.yaml",
        &format!("fields:\n{note_fields}"),
    );
    write(&root, "paper-a.md", "---\ntype: paper\nt: a\n---\n");
    write(&root, "paper-b.md", "---\ntype: paper\nt: b\n---\n");
    write(&root, "n.md", instance);
    (dir, root)
}

/// The `instance` read's containers for one field.
fn containers(h: &mut Harness, field: &str) -> Vec<Value> {
    let resp = h
        .client
        .query(&json!({ "read": "instance", "path": "n.md" }))
        .expect("instance read");
    let values = &resp["result"]["instance"]["effective_values"];
    let entry = values
        .as_array()
        .expect("effective_values is an array")
        .iter()
        .find(|e| e["field"] == field)
        .unwrap_or_else(|| panic!("field {field} present in {values}"));
    entry["containers"]
        .as_array()
        .expect("containers is an array")
        .clone()
}

fn diagnostic_codes(h: &mut Harness) -> Vec<String> {
    let resp = h
        .client
        .query(&json!({ "read": "diagnostics", "path": "n.md" }))
        .expect("diagnostics read");
    let arr = resp["result"]["diagnostics"]["diagnostics"]
        .as_array()
        .or_else(|| resp["result"]["diagnostics"].as_array())
        .expect("diagnostics array");
    arr.iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// A bare `T*` slot's frontmatter wikilink is a REFERENCE, not a raw string.
#[test]
fn frontmatter_reference_in_a_bare_slot_is_a_reference_container() {
    let (_dir, root) = kb(
        "  myRef: paper*\n",
        "---\ntype: note\nmyRef: \"[[paper-a]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "myRef");
    assert_eq!(cs.len(), 1, "one value: {cs:?}");
    assert_eq!(cs[0]["value"]["kind"], "reference", "got {:?}", cs[0]);
    assert_eq!(cs[0]["value"]["target"], "paper-a");
}

/// The REPORTED case. A list slot yields one container PER ELEMENT, each a
/// reference — [[type value container::au-type-system]]'s "a field's effective value is a LIST
/// of value containers". It used to be ONE container holding a sequence of raw
/// strings, which is what silently emptied a consumer's tool lists.
#[test]
fn frontmatter_reference_list_yields_one_reference_container_per_element() {
    let (_dir, root) = kb(
        "  refs: paper*[]\n",
        "---\ntype: note\nrefs:\n  - \"[[paper-a]]\"\n  - \"[[paper-b]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "refs");
    assert_eq!(cs.len(), 2, "one container per element: {cs:?}");
    let targets: Vec<&str> = cs
        .iter()
        .map(|c| {
            assert_eq!(c["value"]["kind"], "reference", "got {c:?}");
            c["value"]["target"].as_str().unwrap()
        })
        .collect();
    assert_eq!(targets, vec!["paper-a", "paper-b"]);
}

/// THE BUG. The same target on both surfaces is ONE container with TWO
/// contributions, and must NOT count twice against cardinality.
/// [[type value container::au-type-system]]: "The same value in frontmatter and body is one
/// container, two contributions. It does not count twice against cardinality."
#[test]
fn the_same_reference_on_both_surfaces_collapses_and_does_not_exceed_cardinality() {
    let (_dir, root) = kb(
        "  myRef: paper*\n",
        "---\ntype: note\nmyRef: \"[[paper-a]]\"\n---\n\nAlso see [[paper-a:myRef]].\n",
    );
    let mut h = harness(&root);

    let cs = containers(&mut h, "myRef");
    assert_eq!(cs.len(), 1, "one container, two contributions: {cs:?}");
    assert_eq!(
        cs[0]["contributions"].as_array().map(Vec::len),
        Some(2),
        "both surfaces contribute: {:?}",
        cs[0]
    );

    let codes = diagnostic_codes(&mut h);
    assert!(
        !codes.iter().any(|c| c == "field-cardinality-exceeded"),
        "naming one target on both surfaces is spec-blessed, got {codes:?}"
    );
}

/// A branded scalar collapses on its underlying representation: `length: 42` in
/// frontmatter and a body `meter(42)` are ONE value, two contributions. The
/// constructor name is a surface annotation, not part of the value for equality,
/// so a branded value never double-counts against cardinality across surfaces.
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]] value-container collapse.
#[test]
fn a_branded_scalar_collapses_a_bare_value_and_its_constructor() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/meter.type.yaml", "shape: Number\n");
    write(&root, "type/note.type.yaml", "fields:\n  length: meter\n");
    write(
        &root,
        "n.md",
        "---\ntype: note\nlength: 42\n---\n\n`[:length] meter(42)`\n",
    );
    let mut h = harness(&root);

    let cs = containers(&mut h, "length");
    assert_eq!(cs.len(), 1, "one container, two contributions: {cs:?}");
    assert_eq!(
        cs[0]["contributions"].as_array().map(Vec::len),
        Some(2),
        "both surfaces contribute: {:?}",
        cs[0]
    );
    let codes = diagnostic_codes(&mut h);
    assert!(
        !codes.iter().any(|c| c == "field-cardinality-exceeded"),
        "a branded value on both surfaces is one value, got {codes:?}"
    );
    assert!(
        !codes
            .iter()
            .any(|c| c == "field-shape-mismatch" || c == "body-slot-shape-mismatch"),
        "both surfaces are valid branded values, got {codes:?}"
    );
}

/// The reserved-primitive escape: `String("looks(foo)")` at a `<String | label>`
/// union brand slot is the plain string `looks(foo)`. It normalizes to its inner
/// scalar (`Name(v) == v`), and no diagnostic fires — `String` is an admitted
/// member. The written brand `String` rides faithfully beside the value (the
/// discriminator that forces the String branch), per the faithful-parse model.
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
#[test]
fn a_primitive_member_constructor_escapes_a_constructor_shaped_literal_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/label.type.yaml", "shape: String\n");
    write(&root, "type/stringy.type.yaml", "shape: <String | label>\n");
    write(&root, "type/note.type.yaml", "fields:\n  v: stringy\n");
    write(
        &root,
        "n.md",
        "---\ntype: note\nv: String(\"looks(foo)\")\n---\n",
    );
    let mut h = harness(&root);

    let codes = diagnostic_codes(&mut h);
    assert!(codes.is_empty(), "the escape is clean, got {codes:?}");
    let cs = containers(&mut h, "v");
    assert_eq!(cs.len(), 1, "one container: {cs:?}");
    assert_eq!(cs[0]["value"]["kind"], "scalar", "{:?}", cs[0]);
    assert_eq!(
        cs[0]["value"]["value"], "looks(foo)",
        "normalized to its inner scalar: {:?}",
        cs[0]
    );
    assert_eq!(
        cs[0]["value"]["brand"], "String",
        "the reserved-primitive escape carries its written brand faithfully: {:?}",
        cs[0]
    );
}

/// `brand` means a brand constructor was WRITTEN. A bare value carries none; an
/// explicit `meter(5)` records `brand: "meter"`; the resolved `value` is the
/// underlying `5` either way. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
#[test]
fn a_written_brand_constructor_records_the_brand_side_channel() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/meter.type.yaml", "shape: Number\n");
    write(
        &root,
        "type/note.type.yaml",
        "fields:\n  bare: meter\n  built: meter\n",
    );
    write(
        &root,
        "n.md",
        "---\ntype: note\nbare: 5\nbuilt: meter(5)\n---\n",
    );
    let mut h = harness(&root);

    let bare = containers(&mut h, "bare");
    assert_eq!(bare[0]["value"]["value"], 5, "{:?}", bare[0]);
    assert!(
        bare[0]["value"].get("brand").is_none(),
        "a bare value carries no brand: {:?}",
        bare[0]
    );
    let built = containers(&mut h, "built");
    assert_eq!(
        built[0]["value"]["value"], 5,
        "resolved value: {:?}",
        built[0]
    );
    assert_eq!(
        built[0]["value"]["brand"], "meter",
        "the written brand is recorded: {:?}",
        built[0]
    );
}

/// At a union brand, a nominal member's constructor resolves to the underlying
/// value and records the brand as the discriminator — `label("important")` at
/// `<String | label>` is `value: "important", brand: "label"`, not the raw
/// constructor string. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
#[test]
fn a_union_nominal_member_resolves_the_value_and_records_the_brand() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/label.type.yaml", "shape: String\n");
    write(&root, "type/stringy.type.yaml", "shape: <String | label>\n");
    write(&root, "type/note.type.yaml", "fields:\n  v: stringy\n");
    write(
        &root,
        "n.md",
        "---\ntype: note\nv: label(\"important\")\n---\n",
    );
    let mut h = harness(&root);

    assert!(diagnostic_codes(&mut h).is_empty(), "clean");
    let cs = containers(&mut h, "v");
    assert_eq!(
        cs[0]["value"]["value"], "important",
        "resolved: {:?}",
        cs[0]
    );
    assert_eq!(
        cs[0]["value"]["brand"], "label",
        "discriminator: {:?}",
        cs[0]
    );
}

/// An UNADMITTED but well-formed constructor is a faithful parse, not a raw
/// literal: `second(42)` at a `meter` slot resolves to `{ value: 42, brand:
/// "second" }` (reconstructable as `second(42)`), and the validator separately
/// fires `brand-constructor-mismatch`. Parse and admission are two layers, so the
/// value layer stays graph-free. Locks decision 2609041736 (amendment 2609051620)
/// and codereview 2609051548 finding 1.1. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
#[test]
fn an_unadmitted_constructor_is_a_faithful_parse_plus_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/meter.type.yaml", "shape: Number\n");
    write(&root, "type/second.type.yaml", "shape: Number\n");
    write(&root, "type/note.type.yaml", "fields:\n  len: meter\n");
    write(&root, "n.md", "---\ntype: note\nlen: second(42)\n---\n");
    let mut h = harness(&root);

    assert!(
        diagnostic_codes(&mut h)
            .iter()
            .any(|c| c == "brand-constructor-mismatch"),
        "the validator flags the unadmitted brand: {:?}",
        diagnostic_codes(&mut h)
    );
    let cs = containers(&mut h, "len");
    assert_eq!(
        cs[0]["value"]["value"], 42,
        "faithful inner value: {:?}",
        cs[0]
    );
    assert_eq!(
        cs[0]["value"]["brand"], "second",
        "faithful brand, reconstructable as second(42): {:?}",
        cs[0]
    );
}

/// A bare value that two indistinguishable members accept is ambiguous, the
/// author must name the branch. `<String | label>` (both `String`) with a bare
/// string fires `brand-constructor-required`, end-to-end.
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
#[test]
fn a_bare_value_at_an_overlapping_union_is_ambiguous_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/label.type.yaml", "shape: String\n");
    write(&root, "type/stringy.type.yaml", "shape: <String | label>\n");
    write(&root, "type/note.type.yaml", "fields:\n  v: stringy\n");
    write(&root, "n.md", "---\ntype: note\nv: hi\n---\n");
    let mut h = harness(&root);

    let codes = diagnostic_codes(&mut h);
    assert!(
        codes.iter().any(|c| c == "brand-constructor-required"),
        "an ambiguous bare value names its branch, got {codes:?}"
    );
}

/// A compound slot legitimately holds BOTH kinds, so a mixed list is CORRECT
/// input, not an error to bail on. Each element is typed independently: the
/// link is a reference, the plain string stays a scalar. An all-or-nothing
/// fallback would hand back the old single-scalar shape for valid data.
#[test]
fn a_compound_slot_types_each_list_element_independently() {
    let (_dir, root) = kb(
        "  refs: \"<paper* | String>[]\"\n",
        "---\ntype: note\nrefs:\n  - \"[[paper-a]]\"\n  - just some text\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "refs");
    assert_eq!(cs.len(), 2, "one container per element: {cs:?}");
    assert_eq!(cs[0]["value"]["kind"], "reference", "the link: {:?}", cs[0]);
    assert_eq!(cs[0]["value"]["target"], "paper-a");
    assert_eq!(cs[1]["value"]["kind"], "scalar", "the string: {:?}", cs[1]);
}

/// Elements carry their OWN byte spans, so a consumer can locate each one and
/// container ordering (which sorts by span) preserves list order.
#[test]
fn list_element_containers_carry_distinct_spans_in_source_order() {
    let (_dir, root) = kb(
        "  refs: paper*[]\n",
        "---\ntype: note\nrefs:\n  - \"[[paper-a]]\"\n  - \"[[paper-b]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "refs");
    let spans: Vec<u64> = cs
        .iter()
        .map(|c| {
            c["contributions"][0]["location"]["byte_range"]["start"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(spans.len(), 2);
    assert!(spans[0] < spans[1], "source order preserved: {spans:?}");
    assert!(spans[0] > 0, "real spans, not zeroed: {spans:?}");
}

/// The NEGATIVE case that proves the gate is the SLOT, not the syntax. A
/// whole-value wikilink in a `String` slot is the string `[[paper-a]]`, per
/// [[type reference::au-type-system]] — it must NOT become a reference.
#[test]
fn a_wikilink_in_a_string_slot_stays_a_scalar() {
    let (_dir, root) = kb(
        "  label: String\n",
        "---\ntype: note\nlabel: \"[[paper-a]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "label");
    assert_eq!(cs.len(), 1);
    assert_eq!(
        cs[0]["value"]["kind"], "scalar",
        "a String slot takes the literal text: {:?}",
        cs[0]
    );
}

/// The body path is slot-gated too. A `[[x:field]]` aimed at a non-reference
/// slot is NOT a reference container — that input is already a
/// `body-slot-shape-mismatch`, so only invalid input changes shape here.
#[test]
fn a_body_contribution_into_a_non_reference_slot_is_a_scalar() {
    let (_dir, root) = kb(
        "  label: String\n",
        "---\ntype: note\nlabel:\n---\n\nSee [[paper-a:label]].\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "label");
    assert!(!cs.is_empty(), "the body contributes something");
    assert!(
        cs.iter().all(|c| c["value"]["kind"] == "scalar"),
        "a String slot cannot hold a reference: {cs:?}"
    );
}

/// The enrichment is conditional on the instance RESOLVING — no claim, no
/// shape, no slot to gate on. A consumer therefore cannot read `scalar` as
/// proof the slot is not a reference, which WIRE.md must say.
#[test]
fn an_unresolved_instance_keeps_scalar_containers() {
    let (_dir, root) = kb(
        "  myRef: paper*\n",
        "---\ntype: nonexistent-type\nmyRef: \"[[paper-a]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "myRef");
    assert_eq!(cs.len(), 1);
    assert_eq!(
        cs[0]["value"]["kind"], "scalar",
        "no shape means no slot to gate on: {:?}",
        cs[0]
    );
}

/// Two elements of one authored sequence are two value SLOTS, so an equal pair
/// does NOT collapse. Collapsing them would make a list a set and lose order:
/// `[a, b, a]` would report `a, b`, which cannot be reconstructed.
#[test]
fn duplicate_list_elements_stay_distinct_containers() {
    let (_dir, root) = kb(
        "  refs: paper*[]\n",
        "---\ntype: note\nrefs:\n  - \"[[paper-a]]\"\n  - \"[[paper-a]]\"\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "refs");
    assert_eq!(cs.len(), 2, "a list is not a set: {cs:?}");
    assert!(cs.iter().all(|c| c["value"]["target"] == "paper-a"));
}

/// Order survives a duplicate: `[a, b, a]` reports three containers in source
/// order, and a prose mention corroborates the FIRST matching slot — a
/// deterministic tie-break, since a prose mention carries no position.
#[test]
fn a_positionless_contribution_corroborates_the_first_matching_slot() {
    let (_dir, root) = kb(
        "  refs: paper*[]\n",
        "---\ntype: note\nrefs:\n  - \"[[paper-a]]\"\n  - \"[[paper-b]]\"\n  - \"[[paper-a]]\"\n---\n\nSee [[paper-a:refs]].\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "refs");
    assert_eq!(cs.len(), 3, "three slots, order preserved: {cs:?}");
    let targets: Vec<&str> = cs
        .iter()
        .map(|c| c["value"]["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, vec!["paper-a", "paper-b", "paper-a"]);
    let counts: Vec<usize> = cs
        .iter()
        .map(|c| c["contributions"].as_array().unwrap().len())
        .collect();
    assert_eq!(
        counts,
        vec![2, 1, 1],
        "the prose mention joins the FIRST slot"
    );
}

/// An EMPTY list keeps its field with ZERO containers, so "authored but empty"
/// stays distinct from "not authored" (which has no entry at all). This is what
/// makes `T[+]` checkable at the container level.
#[test]
fn an_empty_list_is_present_with_zero_containers() {
    let (_dir, root) = kb(
        "  refs: paper*[]\n  other?: String\n",
        "---\ntype: note\nrefs: []\n---\n",
    );
    let mut h = harness(&root);
    let resp = h
        .client
        .query(&json!({ "read": "instance", "path": "n.md" }))
        .expect("instance read");
    let entries = resp["result"]["instance"]["effective_values"]
        .as_array()
        .unwrap()
        .clone();
    let refs = entries.iter().find(|e| e["field"] == "refs");
    assert!(
        refs.is_some(),
        "an authored empty list keeps its field: {entries:?}"
    );
    assert_eq!(
        refs.unwrap()["containers"].as_array().map(Vec::len),
        Some(0)
    );
    assert!(
        !entries.iter().any(|e| e["field"] == "other"),
        "an UNAUTHORED field has no entry at all: {entries:?}"
    );
}

/// Slot-gating the VALUE layer must not touch the NAVIGATIONAL layer. A
/// `[[x]]` in a `String` slot is the literal string as a value, AND still a
/// live edge in the graph — [[type reference::au-type-system]]'s two orthogonal properties.
/// Guards against a future gate accidentally suppressing navigation.
#[test]
fn a_string_slot_link_is_still_a_navigational_edge() {
    let (_dir, root) = kb(
        "  label: String\n",
        "---\ntype: note\nlabel: \"[[paper-a]]\"\n---\n",
    );
    let mut h = harness(&root);
    let resp = h
        .client
        .query(&json!({ "read": "references_in", "path": "paper-a.md" }))
        .expect("backlinks read");
    let edges = resp["result"]["references_in"]["backlinks"]
        .as_array()
        .or_else(|| resp["result"]["references_in"].as_array())
        .expect("backlinks array");
    assert_eq!(
        edges.len(),
        1,
        "the frontmatter link still indexes: {edges:?}"
    );
    assert_eq!(edges[0]["slot"], "label");
    assert_eq!(edges[0]["surface"], "frontmatter");
    assert!(
        !diagnostic_codes(&mut h)
            .iter()
            .any(|c| c == "field-shape-mismatch"),
        "a wikilink-shaped String is a legal String value"
    );
}

/// EVERY list splits per element, not only reference lists. A value layer where
/// `String[]` and `paper*[]` structure differently would be a new arbitrary
/// seam replacing the one this removes.
#[test]
fn a_plain_string_list_also_yields_one_container_per_element() {
    let (_dir, root) = kb(
        "  tags: String[]\n",
        "---\ntype: note\ntags:\n  - urgent\n  - draft\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "tags");
    assert_eq!(cs.len(), 2, "one container per element: {cs:?}");
    let vals: Vec<&str> = cs
        .iter()
        .map(|c| {
            assert_eq!(c["value"]["kind"], "scalar");
            c["value"]["value"].as_str().unwrap()
        })
        .collect();
    assert_eq!(vals, vec!["urgent", "draft"]);
}

/// A record list splits into INLINE_RECORD containers, not scalars — the
/// element keeps its own kind. Record lists are the common authoring shape
/// (a plan's `actions:`), so getting this wrong would be widely visible.
#[test]
fn a_record_list_yields_inline_record_containers_per_element() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, "type/step.type.yaml", "fields:\n  what: String\n");
    write(&root, "type/note.type.yaml", "fields:\n  steps: step[]\n");
    write(
        &root,
        "n.md",
        "---\ntype: note\nsteps:\n  - what: mix\n  - what: bake\n---\n",
    );
    let mut h = harness(&root);
    let cs = containers(&mut h, "steps");
    assert_eq!(cs.len(), 2, "one container per record: {cs:?}");
    assert!(
        cs.iter().all(|c| c["value"]["kind"] == "inline_record"),
        "elements keep their own kind: {cs:?}"
    );
}
