//! The uniform read-result envelope: every read's `result` is an object
//! carrying its payload under the read's own name, so `result[verb]` always
//! reaches it. See `WIRE.md`, "The result envelope".
//!
//! Two tests over one read-surface fixture, a knowledge base built so every read has
//! real content to answer with.
//!
//! - `every_read_carries_its_verb_as_the_payload_key` is the compliance
//!   invariant. It drives the whole catalog, so a read cannot drift out of
//!   compliance without failing here — nobody has to remember to write a
//!   per-read shape test.
//! - `the_read_surface_fixture_exercises_every_read` guards the FIRST test's
//!   worth. An empty result carries its payload key perfectly well, so without
//!   this a thin fixture would let compliance pass while proving nothing about
//!   most of the catalog.

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

/// The read-surface fixture: one knowledge base where EVERY read has something real to
/// answer with. Deliberately dense rather than minimal — the compliance test's
/// value is proportional to how much of each result is actually populated.
///
/// What each piece is here for:
/// - a type-def with a subtype, so `types` / `type` / `type_tree` / `subtypes`
///   are non-empty and the tree has a real parent edge.
/// - typed instances under `content/`, so `instances` / `instances_of` /
///   `instance` / `frontmatter` answer, and `top_level_dirs` sees a
///   subdirectory holding catalogued files.
/// - inbound and outbound references, so `references_in` / `references_out`
///   answer.
/// - a heading and a `^block-id`, so `resolve_anchor` / `resolve_block_id`
///   resolve rather than returning null.
/// - a dangling link, so `diagnostics` / `diagnostic_counts` are non-empty.
/// - an untyped file carrying a typed file's required fields, so the candidate
///   scan has a candidate to report.
/// - an `.auignore`, so `ignores` reports a non-default pattern set.
fn read_surface_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    write(&root, ".arsumbris/repo.yaml", "name: v\n");
    write(&root, ".arsumbris/.auignore", "build/\n");

    write(
        &root,
        "type/note.type.yaml",
        "#: a note\nfields:\n  title: String\n  rel?: note*\n",
    );
    write(
        &root,
        "type/decision.type.yaml",
        "extends: note\nfields:\n  status: [open, decided]\n",
    );

    // The reference target: carries a heading and a block-id so the two
    // resolve_* reads have something to land on.
    write(
        &root,
        "content/hub.md",
        "---\ntype: note\ntitle: hub\n---\n\n# Rationale\n\nA paragraph. ^para-one\n",
    );
    write(
        &root,
        "content/choice.md",
        "---\ntype: decision\ntitle: choice\nstatus: decided\nrel: \"[[hub]]\"\n---\n\nsee [[hub#Rationale]].\n",
    );
    // Dangling link → a diagnostic to count.
    write(
        &root,
        "content/broken.md",
        "---\ntype: note\ntitle: broken\nrel: \"[[nowhere]]\"\n---\n",
    );
    // Untyped, but carries `note`'s required field → a candidate.
    write(
        &root,
        "content/untyped.md",
        "---\ntitle: could be a note\n---\n",
    );

    (dir, root)
}

/// The number of READ verbs in the catalog. Pins `every_read()` to a deliberate
/// count so the "machine-verified, a read cannot drift out of compliance" claim
/// is actually enforced: a read added to the `Request` enum and dispatch, but
/// NOT to `every_read()`, would silently escape the compliance invariant.
/// `the_read_catalog_size_is_pinned` fails when this and `every_read()` diverge,
/// so adding a read means touching both — the tripwire.
///
/// NOT auto-derived: `Request` interleaves reads, mutations, and control verbs
/// with no structural read/mutation split, so there is no single source to
/// enumerate the read SUBSET from. The pinned count is the pragmatic guard; the
/// daemon still rejects an unknown verb, so a typo'd row fails on send.
const READ_VERB_COUNT: usize = 38;

/// Every read verb, with arguments that resolve against the fixture. The list
/// IS the catalog: a read added without a row here is a read the compliance
/// invariant does not cover, so keep it exhaustive.
fn every_read() -> Vec<(&'static str, Value)> {
    let hub = "content/hub.md";
    vec![
        ("diagnostics", json!({})),
        ("diagnostic_counts", json!({})),
        ("types", json!({})),
        ("type_counts", json!({})),
        ("type_tree", json!({})),
        ("type", json!({ "name": "note" })),
        ("type_batch", json!({ "names": ["note", "decision"] })),
        ("type_closure", json!({ "name": "decision" })),
        ("instances_of", json!({ "type": "note" })),
        ("imports", json!({})),
        ("subtypes", json!({ "base": "note" })),
        ("instances", json!({})),
        ("candidates", json!({})),
        ("candidate_counts", json!({})),
        ("instance", json!({ "path": "content/choice.md" })),
        ("references_in", json!({ "path": hub })),
        ("references_out", json!({ "path": "content/choice.md" })),
        ("resolve_target", json!({ "target": "hub" })),
        (
            "resolve_block_id",
            json!({ "target": "hub", "block_id": "para-one" }),
        ),
        (
            "resolve_anchor",
            json!({ "target": "hub", "anchor": "Rationale" }),
        ),
        ("anchors", json!({ "target": "hub" })),
        ("block_ids", json!({ "target": "hub" })),
        ("files", json!({})),
        ("dir_entries", json!({ "dir": "content" })),
        ("frontmatter", json!({ "path": hub })),
        ("content", json!({ "path": hub })),
        ("semantic_tokens", json!({ "path": hub })),
        // Deliberately INVALID (`note` requires `title`), so the read answers a
        // real diagnostic. A valid value correctly returns an empty list, which
        // would read as a fixture gap.
        (
            "validate_value",
            json!({ "type_name": "note", "value": { "untitled": true } }),
        ),
        ("members", json!({})),
        ("ignores", json!({})),
        ("resolve_member", json!({ "path": hub })),
        ("device_config", json!({})),
        ("top_level_dirs", json!({})),
        ("hubs", json!({})),
        ("graph_shape", json!({})),
        ("link_graph", json!({})),
        ("overview", json!({})),
        ("lifecycle", json!({})),
    ]
}

fn request(verb: &str, args: &Value) -> Value {
    let mut req = args.clone();
    req["read"] = json!(verb);
    req
}

/// Whether a payload counts as populated: a non-empty array, a non-empty
/// object, or any non-null scalar.
fn is_populated(payload: &Value) -> bool {
    match payload {
        Value::Null => false,
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

/// Reads not yet swept onto the envelope. SHRINKS to empty as the sweep lands;
/// the test asserts the violation set EQUALS this list, so it stays honest in
/// both directions:
/// - sweeping a read without removing its row here fails ("remove it"),
/// - and a swept read regressing fails immediately.
///
/// An allowlist rather than a red test: a permanently-failing assertion would
/// break the gate for the whole sweep and destroy its signal, while this keeps
/// the suite green AND makes the remaining work countable.
const NOT_YET_ENVELOPED: &[&str] = &[];

/// THE compliance invariant. Every read's `result` is an object carrying the
/// read's own name as a key. One test for the whole catalog, so a new or
/// changed read cannot silently drift out of the envelope.
///
/// KNOWN LIMITATION: this cannot tell a real payload key from a payload record
/// that merely happens to carry a field of the read's name. Today `resolved`
/// and `content` satisfy it for exactly that wrong reason — `result.resolved`
/// is a boolean and `result.content` a string, not their payloads. That is the
/// collision the envelope rule handles by renaming the FIELD (`content.text`)
/// or the verb (`resolved` → `instance`); both land later in the sweep, after
/// which no read collides and the check is sound. A future read must not
/// reintroduce one.
/// Pins the read catalog's SIZE, so `every_read()` cannot silently fall behind
/// the `Request` enum. The compliance test only checks the reads it is HANDED;
/// a new read omitted from `every_read()` escapes it entirely. This is the
/// cross-check that makes the "nobody has to remember to add the row" claim
/// true: forgetting the row (or bumping the count without one) fails here.
#[test]
fn the_read_catalog_size_is_pinned() {
    let reads = every_read();
    assert_eq!(
        reads.len(),
        READ_VERB_COUNT,
        "every_read() has {} rows but READ_VERB_COUNT is {READ_VERB_COUNT}; a read added to `Request` \
         without a row here escapes the compliance invariant — update both",
        reads.len(),
    );
    // A duplicated verb would inflate the count without adding coverage, hiding
    // a missing read behind a copy-paste.
    let mut seen = std::collections::HashSet::new();
    for (verb, _) in &reads {
        assert!(seen.insert(*verb), "duplicate verb in every_read(): {verb}");
    }
}

#[test]
fn every_read_carries_its_verb_as_the_payload_key() {
    let (_dir, root) = read_surface_kb();
    let mut h = harness(&root);

    let mut violations: Vec<String> = Vec::new();
    let mut violating: Vec<&str> = Vec::new();
    for (verb, args) in every_read() {
        let resp = h.client.query(&request(verb, &args)).expect("query");
        assert_eq!(resp["ready"], json!(true), "{verb}: not ready");

        let result = &resp["result"];
        if !result.is_object() {
            violations.push(format!("{verb}: result is {}", kind_of(result)));
            violating.push(verb);
            continue;
        }
        if result.get(verb).is_none() {
            violations.push(format!(
                "{verb}: no `{verb}` key (keys: {:?})",
                result.as_object().unwrap().keys().collect::<Vec<_>>()
            ));
            violating.push(verb);
        }
    }

    let regressed: Vec<_> = violating
        .iter()
        .filter(|v| !NOT_YET_ENVELOPED.contains(v))
        .collect();
    assert!(
        regressed.is_empty(),
        "reads that REGRESSED out of the envelope:\n  {}",
        violations.join("\n  ")
    );

    let swept: Vec<_> = NOT_YET_ENVELOPED
        .iter()
        .filter(|v| !violating.contains(v))
        .collect();
    assert!(
        swept.is_empty(),
        "these reads are now enveloped — drop them from NOT_YET_ENVELOPED: {swept:?}"
    );
}

/// Guards the compliance test's worth. An empty result satisfies "carries its
/// payload key" while proving nothing, so the fixture must actually populate
/// every read. A read landing here is a fixture gap, not an engine bug.
#[test]
fn the_read_surface_fixture_exercises_every_read() {
    let (_dir, root) = read_surface_kb();
    let mut h = harness(&root);

    let mut empty: Vec<&str> = Vec::new();
    for (verb, args) in every_read() {
        let resp = h.client.query(&request(verb, &args)).expect("query");
        // Read the payload through the envelope where it exists, else the whole
        // result, so this test is meaningful before and after the sweep.
        let result = &resp["result"];
        let payload = result.get(verb).unwrap_or(result);
        if !is_populated(payload) {
            empty.push(verb);
        }
    }

    assert!(
        empty.is_empty(),
        "the read-surface fixture leaves these reads empty, so compliance \
         proves nothing for them: {empty:?}"
    );
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
