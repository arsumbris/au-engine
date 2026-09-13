//! A cross-repo instance `type:` claim stays `::repo`-qualified on the reads
//! that surface it: `resolved` (the top-level claim) and `resolve_block_id` (an
//! inline record's claim). The `instances` read is covered end-to-end by the
//! `cross-repo-type-claim` scenario.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// `base` owns `note` and `dm`. `app` peers `base` and holds one instance that
/// claims `note::base` at the top level and carries an inline record claiming
/// `dm::base`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/note.type.yaml", "fields:\n  detail?: any\n");
    w("base/type/dm.type.yaml", "fields: {}\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // Top-level claim `note::base`; an inline record under `detail` claims
    // `dm::base` and carries a block id.
    w(
        "app/card.md",
        "---\ntype: note::base\ndetail:\n  ^: d1\n  type: dm::base\n---\n",
    );
    (dir, root)
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
    card: String,
}

/// A slot-pinned (claim-less) cross-repo nested record's served claim is the
/// owner-qualified type, not empty. `resolve_block_id` passes `record.qualified`
/// to the wire, which is owner-qualified only because the slot pins the peer type
/// and it resolved owner-relative. Covers the read-family that surfaces
/// `record.qualified` (resolve_block_id / record_block_ids / block_ids listing):
/// the prior cross-repo test used an EXPLICIT nested `type:`, so the owner-relative
/// INFERENCE at these reads was untested.
#[test]
fn resolve_block_id_infers_the_owner_qualified_claim_for_a_slot_pinned_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    w(
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // A claim-less slot-pinned record carrying block-id c1, in a peer-claiming host.
    w(
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - ^: c1\n    salience: focal\n---\n",
    );

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let resp = client
        .query(&json!({
            "read": "resolve_block_id", "target": "extraction", "block_id": "c1",
        }))
        .unwrap();
    assert_eq!(
        resp["result"]["resolve_block_id"]["type_claim"],
        json!(["concept-candidate::base"]),
        "a slot-pinned record's served claim is inferred owner-qualified, not empty/bare: {resp:?}"
    );
}

/// The VALUE LAYER of the `instance` read resolves a slot-pinned (claim-less)
/// cross-repo nested record's OWN nested fields against the peer shape — the
/// `resolve_inline_record_fields` -> `resolved_effective_shape` path, distinct
/// from the `.qualified` claim (M1, proven above). A nested `link: file*` field
/// renders as a `reference` value ONLY because the nested record resolved its
/// slot-pinned identity (`concept-candidate::base`) owner-relative and found the
/// `file*` slot; own-graph-only the nested shape is `None`, so the same value
/// degrades to a plain `scalar` string. That flip is the non-vacuous guard: the
/// value layer here is the ONLY read whose cross-repo nested value resolution was
/// untested (`value_layer_references.rs` is single-repo).
#[test]
fn value_layer_resolves_a_slot_pinned_cross_repo_nested_records_reference_field() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    // The nested record carries a reference-typed field, so its resolved-vs-not
    // state is observable as `reference` vs `scalar` in the value layer.
    w(
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n  link?: file*\n",
    );
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("app/target.md", "a plain note\n");
    // A claim-less slot-pinned record in a peer-claiming host; its `link` field
    // holds a whole-value wikilink.
    w(
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - ^: c1\n    salience: focal\n    link: \"[[target]]\"\n---\n",
    );

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let path = root.join("app/extraction.md").display().to_string();
    let resp = client
        .query(&json!({ "read": "instance", "path": path }))
        .unwrap();

    // effective_values[concepts].containers[0].value is the inline record; its
    // `link` nested field must serve as a `reference`, not a `scalar` string.
    let evs = resp["result"]["instance"]["effective_values"]
        .as_array()
        .expect("effective_values array");
    let concepts = evs
        .iter()
        .find(|e| e["field"] == "concepts")
        .expect("concepts field present in the value layer");
    let record = &concepts["containers"][0]["value"];
    assert_eq!(
        record["kind"], "inline_record",
        "the slot-pinned element is an inline record: {resp:?}"
    );
    let link = record["fields"]
        .as_array()
        .expect("inline record fields")
        .iter()
        .find(|f| f["field"] == "link")
        .expect("the nested `link` field is present");
    assert_eq!(
        link["values"][0]["kind"], "reference",
        "the nested `link` value resolves as a reference, proving the record's \
         file* slot was found owner-relative; own-graph-only it would be a scalar: {resp:?}"
    );
    assert_eq!(
        link["values"][0]["target"], "target",
        "the resolved reference names its target: {resp:?}"
    );
}

/// A nested record with an EXPLICIT cross-repo MIXIN claim (`type: [a::base,
/// b::base]`) resolves EVERY member's fields owner-relative in the value layer.
/// This is the case a code review flagged as a possible mixin gap in
/// `owner_relative_effective_shape` (which takes the owner-relative branch only
/// for a single `Bare` claim, deferring a mixin to the fold). It is NOT a gap: an
/// inline-record `::repo` claim SEEDS the host's fold (`collect_value_seeds`
/// recurses into nested-record claims), so both mixin members are imported and
/// `resolved_effective_shape` resolves them. The `link: file*` field, declared
/// only on the SECOND mixin member, reads as a `reference` — degraded to `scalar`
/// only if that member's shape were lost. The claim-less slot-demand case (the M4
/// bug) is the only non-seeding path, and a slot demand is always a single claim.
#[test]
fn value_layer_resolves_every_member_of_a_cross_repo_mixin_nested_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/research-extraction.type.yaml",
        "fields:\n  concepts?: concept-candidate&[]\n",
    );
    w(
        "base/type/concept-candidate.type.yaml",
        "fields:\n  salience?: String\n",
    );
    // The reference-typed field lives ONLY on the second mixin member, so it is
    // observable only if that member's shape resolves.
    w("base/type/extra-tag.type.yaml", "fields:\n  link?: file*\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w("app/target.md", "a plain note\n");
    // The nested record carries an EXPLICIT mixin claim; `link` comes from
    // extra-tag::base, the second member.
    w(
        "app/extraction.md",
        "---\ntype: research-extraction::base\nconcepts:\n  - type: [concept-candidate::base, extra-tag::base]\n    salience: focal\n    link: \"[[target]]\"\n---\n",
    );

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let path = root.join("app/extraction.md").display().to_string();
    let resp = client
        .query(&json!({ "read": "instance", "path": path }))
        .unwrap();
    let evs = resp["result"]["instance"]["effective_values"]
        .as_array()
        .expect("effective_values array");
    let concepts = evs
        .iter()
        .find(|e| e["field"] == "concepts")
        .expect("concepts field present");
    let record = &concepts["containers"][0]["value"];
    let link = record["fields"]
        .as_array()
        .expect("inline record fields")
        .iter()
        .find(|f| f["field"] == "link")
        .expect("the nested `link` field is present");
    assert_eq!(
        link["values"][0]["kind"], "reference",
        "the second mixin member's `link: file*` field resolves owner-relative: {resp:?}"
    );
}

/// The `closure` field renders a cross-repo ANCESTOR owner-qualified, parity with
/// the `claim` field. `app` owns `card` extending `note::base`; an instance claims
/// bare `card`. Its closure is `{card, note::base}`: the own `card` stays bare, the
/// peer ancestor keeps its `::repo`. The pre-fix projection mapped the
/// `instance_closure` name-set, dropping the qualifier to a bare `note`.
#[test]
fn closure_renders_a_cross_repo_ancestor_qualified() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/note.type.yaml", "fields:\n  detail?: any\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // An app-owned subtype of a base type, and an instance claiming it bare.
    w("app/type/card.type.yaml", "extends: note::base\n");
    w("app/item.md", "---\ntype: card\n---\n");

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let path = root.join("app/item.md").display().to_string();
    let resp = client
        .query(&json!({ "read": "instance", "path": path }))
        .unwrap();
    assert_eq!(
        resp["result"]["instance"]["closure"],
        json!(["card", "note::base"]),
        "the own `card` stays bare, the cross-repo ancestor keeps `::base`: {resp:?}"
    );
}

/// The cross-repo DIAMOND: an instance whose closure reaches two divergent
/// same-named identities (app's own `note` AND base's `note`) surfaces BOTH in
/// `closure`, not one collapsed entry. `app` owns a `note` (diverged from base's
/// by an extra field) and imports `base`; the instance mixes `type: [note,
/// note::base]`. The pre-fix `BTreeSet<TypeName>` collapsed the two to a single
/// `"note"`, unrepresentable; the folded-id projection keeps them distinct.
///
/// The reachability answer the plan flags: this instance is CLEAN (no
/// `mixin-collision` at the instance level, that rule governs type-def mixins), so
/// the collapse was a wrong answer on a valid path, not an unreachable corner.
#[test]
fn closure_renders_a_cross_repo_diamond_as_two_identities() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/note.type.yaml", "fields:\n  detail?: any\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // app's OWN `note`, diverged from base's by an extra field, so the two carry
    // distinct closure hashes and stay distinct ids.
    w(
        "app/type/note.type.yaml",
        "fields:\n  detail?: any\n  extra?: String\n",
    );
    w("app/item.md", "---\ntype: [note, note::base]\n---\n");

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let path = root.join("app/item.md").display().to_string();
    let resp = client
        .query(&json!({ "read": "instance", "path": path }))
        .unwrap();
    assert_eq!(
        resp["result"]["instance"]["closure"],
        json!(["note", "note::base"]),
        "the two divergent same-named notes both surface, not collapsed to one: {resp:?}"
    );
    assert_eq!(
        resp["result"]["instance"]["diagnostics"],
        json!([]),
        "the instance-level diamond is clean, so the collapse was wrong on a valid path: {resp:?}"
    );
}

/// An IN-SYNC same-name identity renders `closure` relative to the instance's OWN
/// repo, so it stays bare and matches a bare own `claim`. `app` defines its own
/// `note` byte-identical to base's (same `ClosureHash`, one folded node), and also
/// imports `note::base` (via `card` and a peer-claiming sibling), so the shared
/// node's retained fold origin can be the peer. An instance claiming bare own
/// `note` must still read `closure: ["note"]`, not `["note::base"]`: the render is
/// ownership-relative (own graph defines the identity), not the fold origin.
#[test]
fn closure_renders_an_in_sync_shared_identity_owner_relative() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w("base/type/note.type.yaml", "fields:\n  detail?: any\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    // app's OWN note, byte-identical to base's -> same ClosureHash -> one node.
    w("app/type/note.type.yaml", "fields:\n  detail?: any\n");
    // Import note::base into the fold two ways, so the shared node is reached from
    // the peer edge and its retained origin is the peer's (the pre-fix bug trigger).
    w("app/type/card.type.yaml", "extends: note::base\n");
    w("app/peer.md", "---\ntype: note::base\n---\n");
    // The instance under test: a bare OWN claim.
    w("app/item.md", "---\ntype: note\n---\n");

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let path = root.join("app/item.md").display().to_string();
    let resp = client
        .query(&json!({ "read": "instance", "path": path }))
        .unwrap();
    assert_eq!(
        resp["result"]["instance"]["claim"],
        json!(["note"]),
        "the bare own claim is verbatim: {resp:?}"
    );
    assert_eq!(
        resp["result"]["instance"]["closure"],
        json!(["note"]),
        "the shared in-sync identity renders bare (own-repo-relative), matching the \
         claim, NOT note::base from the fold node's retained peer origin: {resp:?}"
    );
}

fn started() -> Harness {
    let (dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    let card = root.join("app/card.md").display().to_string();
    Harness {
        _dir: dir,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
        card,
    }
}

#[test]
fn resolved_carries_the_qualified_top_level_claim() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "instance", "path": h.card.clone() }))
        .unwrap();
    assert_eq!(
        resp["result"]["instance"]["claim"],
        json!(["note::base"]),
        "the top-level `type:` claim stays qualified, not stripped to `note`"
    );

    // The inline record's claim on `record_block_ids` also stays qualified.
    let records = resp["result"]["instance"]["record_block_ids"]
        .as_array()
        .unwrap();
    let d1 = records
        .iter()
        .find(|r| r["id"] == "d1")
        .expect("the d1 record is listed");
    assert_eq!(
        d1["claims"],
        json!(["dm::base"]),
        "the record's claim stays qualified on record_block_ids"
    );
}

#[test]
fn resolve_block_id_carries_the_qualified_record_claim() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "card",
            "block_id": "d1",
        }))
        .unwrap();
    assert_eq!(
        resp["result"]["resolve_block_id"]["type_claim"],
        json!(["dm::base"]),
        "the inline record's claim stays qualified, not stripped to `dm`"
    );
}
