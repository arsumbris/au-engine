//! The `semantic_tokens` read: a file's typed `(range, kind)` highlight
//! stream. This covers the body-surface kinds — wikilinks (resolved /
//! broken), bare `^id` block-id markers, and headings (anchors) — which
//! serve notes and typed instances alike. The value-layer kinds join as
//! the read grows.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A knowledge base whose `research.md` carries every body-surface signal: a heading,
/// a resolving wikilink, a bare `^id` marker, plus a typed fence with a
/// trailing id and a frontmatter `^:` record — the latter two are value /
/// fence layer, deferred, so they must NOT appear yet. `plain.md` is an
/// untyped note (no `type:` claim) proving notes are served.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir(root.join("content")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  description: String\n  assumptions?: assumption&[+]\n  related?: assumption&[]\n",
    )
    .unwrap();
    fs::write(
        root.join("type/assumption.type.yaml"),
        "fields:\n  description: String\n",
    )
    .unwrap();
    // A type-def exercising the shape layer: a parent claim, an enum, a
    // def-ref, and a plain reference.
    fs::write(
        root.join("type/widget.type.yaml"),
        "extends: note\nfields:\n  level: [low, high]\n  kind?: type<note>*\n  link?: assumption*\n",
    )
    .unwrap();
    fs::write(
        root.join("content/research.md"),
        "---\ntype: note\ndescription: research\nrelated:\n  - ^: rec-1\n    description: an inline assumption\n---\n# Findings\n\nSee [[decision]] and [[ghost]].\n\nA paragraph. ^wal-tradeoff\n\n```yaml [:assumptions]\ntype: assumption\ndescription: stable\n```\n^extractor-stability\n",
    )
    .unwrap();
    fs::write(
        root.join("content/decision.md"),
        "---\ntype: note\ndescription: a decision\n---\n",
    )
    .unwrap();
    // A plain-String `description` with two embedded wikilinks, one
    // resolving, one dangling — navigational, not a reference slot.
    fs::write(
        root.join("content/embed.md"),
        "---\ntype: note\ndescription: \"Runs at [[decision]] and [[ghost]].\"\n---\n",
    )
    .unwrap();
    // An untyped note — no frontmatter at all. Served like a typed instance.
    fs::write(
        root.join("content/plain.md"),
        "# Title\n\nSee [[research]].\n\nBody. ^note-mark\n",
    )
    .unwrap();
    // Local wikilinks (empty target, a locating fragment): a `^^` block-referent,
    // a bare `^` navigational marker, and a `#` anchor. All target this file, so
    // none is broken.
    fs::write(
        root.join("content/local.md"),
        "---\ntype: note\ndescription: local\nrelated:\n  - ^: here\n    description: a record\n---\n# Head\n\nPull [[^^here]], jump [[^nav]], head [[#Head]].\n\nA paragraph. ^nav\n",
    )
    .unwrap();
    // Commit-pinned wikilinks: a named pin and the empty-target commit-referent.
    // Both are inert coordinates, so neither is resolved nor broken.
    fs::write(
        root.join("content/pins.md"),
        "---\ntype: note\ndescription: pins\n---\nA named [[decision::@abc123def]] and a bare [[::@abc123def]].\n",
    )
    .unwrap();
    (dir, root)
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
    _server: ServeHandle,
    client: Client,
}

fn started() -> Harness {
    let (dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir,
        _sock_dir: sock_dir,
        _engine: engine,
        _server: server,
        client,
    }
}

fn tokens(h: &mut Harness, path: &str) -> Vec<Value> {
    let resp = h
        .client
        .query(&json!({ "read": "semantic_tokens", "path": path }))
        .unwrap();
    resp["result"]["semantic_tokens"]
        .as_array()
        .unwrap()
        .clone()
}

/// The tokens of one `kind`, in stream order.
fn of_kind<'a>(toks: &'a [Value], kind: &str) -> Vec<&'a Value> {
    toks.iter().filter(|t| t["kind"] == kind).collect()
}

fn is_sorted(toks: &[Value]) -> bool {
    let starts: Vec<u64> = toks
        .iter()
        .map(|t| t["range"]["start"].as_u64().unwrap())
        .collect();
    starts.windows(2).all(|w| w[0] <= w[1])
}

#[test]
fn body_and_value_tokens_for_a_typed_instance() {
    let mut h = started();
    let toks = tokens(&mut h, "content/research.md");
    assert!(is_sorted(&toks), "tokens are sorted by range.start");

    // Body-surface kinds.
    let anchors = of_kind(&toks, "anchor");
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0]["text"], "Findings");

    let resolved = of_kind(&toks, "wikilink-resolved");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["target"], "decision");
    assert!(resolved[0]["resolved"]
        .as_str()
        .unwrap()
        .ends_with("content/decision.md"));

    let broken = of_kind(&toks, "wikilink-broken");
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0]["target"], "ghost");
    assert!(broken[0].get("resolved").is_none());

    // Type claims: the frontmatter `type: note` and the fence's `type: assumption`.
    let claims: Vec<&str> = of_kind(&toks, "type-claim")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(claims.contains(&"note"), "frontmatter claim: {claims:?}");
    assert!(claims.contains(&"assumption"), "fence claim: {claims:?}");

    // The typed fence is a `typed-block` container over field `assumptions`.
    let blocks = of_kind(&toks, "typed-block");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["field"], "assumptions");

    // Three `description` field-values, all String: frontmatter (note),
    // the nested `related` record (assumption, slot-pinned), and the fence
    // inner field (assumption). No null leaves.
    let descriptions: Vec<&Value> = of_kind(&toks, "field-value")
        .into_iter()
        .filter(|t| t["field"] == "description")
        .collect();
    assert_eq!(descriptions.len(), 3, "frontmatter, record, and fence");
    for d in &descriptions {
        assert_eq!(
            d["value_type"],
            json!({ "kind": "primitive", "name": "String" }),
            "every description resolves to String, not null"
        );
    }

    // Block-ids: the bare marker, the frontmatter `^:` record, and the
    // fence's trailing id.
    let ids: Vec<&str> = of_kind(&toks, "block-id")
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"wal-tradeoff"), "bare marker: {ids:?}");
    assert!(ids.contains(&"rec-1"), "frontmatter record id: {ids:?}");
    assert!(
        ids.contains(&"extractor-stability"),
        "trailing fence id: {ids:?}"
    );
}

#[test]
fn local_wikilinks_resolve_to_the_current_file() {
    // A local link (empty target + a locating fragment) targets the file it appears
    // in, per [[type reference::au-type-system]], so none is broken. Resolving the empty name against
    // the repo index would have marked all three broken.
    let mut h = started();
    let toks = tokens(&mut h, "content/local.md");

    let broken = of_kind(&toks, "wikilink-broken");
    assert!(
        broken.is_empty(),
        "no local link is broken, got: {broken:?}"
    );

    let resolved = of_kind(&toks, "wikilink-resolved");
    assert_eq!(resolved.len(), 3, "^^here, ^nav, and #Head");
    for t in &resolved {
        assert_eq!(t["target"], "", "a local link carries an empty target");
        assert!(
            t["resolved"]
                .as_str()
                .unwrap()
                .ends_with("content/local.md"),
            "a local link resolves to its own file"
        );
    }
}

#[test]
fn commit_pinned_wikilinks_tokenize_as_pinned_not_broken_or_resolved() {
    // An inert pin (named `[[file::@sha]]` or the empty-target commit-referent
    // `[[::@sha]]`) is a coordinate into an immutable past. It is neither resolved
    // (a pin never re-resolves live) nor broken (it is never dangling), so it emits
    // its own `wikilink-pinned` kind, mirroring `references_out`'s commit-referent.
    let mut h = started();
    let toks = tokens(&mut h, "content/pins.md");

    assert!(
        of_kind(&toks, "wikilink-broken").is_empty(),
        "a pin is never broken"
    );
    assert!(
        of_kind(&toks, "wikilink-resolved").is_empty(),
        "a pin never re-resolves live"
    );

    let pinned = of_kind(&toks, "wikilink-pinned");
    assert_eq!(pinned.len(), 2, "the named pin and the commit-referent");
    for t in &pinned {
        assert_eq!(t["commit"], "abc123def");
    }
    // The named pin keeps its target; the commit-referent's is empty.
    let targets: Vec<&str> = pinned
        .iter()
        .map(|t| t["target"].as_str().unwrap())
        .collect();
    assert!(
        targets.contains(&"decision"),
        "named pin target: {targets:?}"
    );
    assert!(
        targets.contains(&""),
        "commit-referent empty target: {targets:?}"
    );
}

#[test]
fn embedded_links_in_a_string_field_value_tokenize() {
    let mut h = started();
    let toks = tokens(&mut h, "content/embed.md");
    assert!(is_sorted(&toks), "tokens sorted by range.start");

    // The embedded links become wikilink tokens, even inside a String slot.
    let resolved = of_kind(&toks, "wikilink-resolved");
    assert_eq!(resolved.len(), 1, "one resolving embedded link");
    assert_eq!(resolved[0]["target"], "decision");

    let broken = of_kind(&toks, "wikilink-broken");
    assert_eq!(broken.len(), 1, "one dangling embedded link");
    assert_eq!(broken[0]["target"], "ghost");

    // The surrounding text runs stay `field-value` String tokens on the
    // `description` field, not part of the link.
    let fv: Vec<&Value> = of_kind(&toks, "field-value")
        .into_iter()
        .filter(|t| t["field"] == "description")
        .collect();
    assert!(!fv.is_empty(), "surrounding text is field-value: {fv:?}");
    for t in &fv {
        assert_eq!(
            t["value_type"],
            json!({ "kind": "primitive", "name": "String" })
        );
    }
}

#[test]
fn field_values_carry_their_resolved_type() {
    // The spec's verification case: bool / number / enum / string, each
    // colored by its resolved type, not the grammar.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  active: Boolean\n  priority: Number\n  gate: [stub, open, closed]\n  title: String\n",
    )
    .unwrap();
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nactive: true\npriority: 1\ngate: stub\ntitle: a task\n---\n",
    )
    .unwrap();

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "semantic_tokens", "path": "task.md" }))
        .unwrap();
    let toks = resp["result"]["semantic_tokens"].as_array().unwrap();

    let type_of = |field: &str| -> Value {
        toks.iter()
            .find(|t| t["kind"] == "field-value" && t["field"] == field)
            .unwrap_or_else(|| panic!("field-value for {field}"))["value_type"]
            .clone()
    };
    assert_eq!(
        type_of("active"),
        json!({ "kind": "primitive", "name": "Boolean" })
    );
    assert_eq!(
        type_of("priority"),
        json!({ "kind": "primitive", "name": "Number" })
    );
    assert_eq!(
        type_of("gate"),
        json!({ "kind": "enum", "members": ["stub", "open", "closed"] })
    );
    assert_eq!(
        type_of("title"),
        json!({ "kind": "primitive", "name": "String" })
    );
}

#[test]
fn extra_fields_infer_their_type_from_the_yaml_literal() {
    // A field outside the type's shape has no declared type. The engine
    // infers a bare primitive from the YAML literal, which the consumer's
    // grammar cannot do (it tags every scalar the same). The token is
    // wire-identical to a declared primitive, by design.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/task.type.yaml"),
        "fields:\n  title: String\n",
    )
    .unwrap();
    fs::write(
        root.join("task.md"),
        "---\ntype: task\ntitle: a task\nurgent: true\ncount: 7\nlabel: draft\n---\n",
    )
    .unwrap();

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "semantic_tokens", "path": "task.md" }))
        .unwrap();
    let toks = resp["result"]["semantic_tokens"].as_array().unwrap();

    let type_of = |field: &str| -> Value {
        toks.iter()
            .find(|t| t["kind"] == "field-value" && t["field"] == field)
            .unwrap_or_else(|| panic!("field-value for {field}"))["value_type"]
            .clone()
    };
    // The declared field and the extras all carry a primitive value_type.
    assert_eq!(
        type_of("title"),
        json!({ "kind": "primitive", "name": "String" })
    );
    assert_eq!(
        type_of("urgent"),
        json!({ "kind": "primitive", "name": "Boolean" }),
        "extra bool literal infers Boolean"
    );
    assert_eq!(
        type_of("count"),
        json!({ "kind": "primitive", "name": "Number" }),
        "extra integer literal infers Number"
    );
    assert_eq!(
        type_of("label"),
        json!({ "kind": "primitive", "name": "String" }),
        "extra plain string infers String"
    );
}

#[test]
fn typed_fence_inner_fields_type_by_the_fence_type() {
    // A `[:meta]` fence fills a `detail&` slot, so its inner fields type
    // against `detail` — the inner-field-by-type parse path, full parity
    // with frontmatter. The fence body is parsed with file-absolute spans.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir(root.join("content")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  meta?: detail&\n",
    )
    .unwrap();
    fs::write(
        root.join("type/detail.type.yaml"),
        "fields:\n  active: Boolean\n  weight: Number\n",
    )
    .unwrap();
    fs::write(
        root.join("content/n.md"),
        "---\ntype: note\n---\n# H\n\n```yaml [:meta]\ntype: detail\nactive: true\nweight: 5\n```\n",
    )
    .unwrap();

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "semantic_tokens", "path": "content/n.md" }))
        .unwrap();
    let toks = resp["result"]["semantic_tokens"].as_array().unwrap();
    assert!(is_sorted(toks), "sorted");

    let blocks = of_kind(toks, "typed-block");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["field"], "meta");

    assert!(of_kind(toks, "type-claim")
        .iter()
        .any(|t| t["name"] == "detail"));

    let type_of = |field: &str| -> Value {
        toks.iter()
            .find(|t| t["kind"] == "field-value" && t["field"] == field)
            .unwrap_or_else(|| panic!("field-value for {field}"))["value_type"]
            .clone()
    };
    // The inner fields resolve to detail's declared types, not inferred.
    assert_eq!(
        type_of("active"),
        json!({ "kind": "primitive", "name": "Boolean" })
    );
    assert_eq!(
        type_of("weight"),
        json!({ "kind": "primitive", "name": "Number" })
    );

    // The typed-block span encloses the inner field-value spans.
    let block = &blocks[0]["range"];
    let (bs, be) = (
        block["start"].as_u64().unwrap(),
        block["end"].as_u64().unwrap(),
    );
    for f in ["active", "weight"] {
        let t = toks
            .iter()
            .find(|t| t["kind"] == "field-value" && t["field"] == f)
            .unwrap();
        let s = t["range"]["start"].as_u64().unwrap();
        assert!(s > bs && s < be, "{f} leaf inside the fence container");
    }
}

#[test]
fn a_pure_yaml_instance_tokenizes_without_a_body() {
    // A pure-YAML instance has no markdown body, so the value-layer kinds
    // must come through on their own. The session-log shape.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir(root.join("ops")).unwrap();
    fs::write(
        root.join("type/session-log.type.yaml"),
        "fields:\n  session: String\n  events?: sessionEvent[]\n",
    )
    .unwrap();
    fs::write(
        root.join("type/sessionEvent.type.yaml"),
        "fields:\n  at: String\n",
    )
    .unwrap();
    fs::write(
        root.join("ops/s-001.yaml"),
        "type: session-log\nsession: \"s-001\"\nevents:\n  - ^: e-prompt\n    type: sessionEvent\n    at: \"2026-06-07T01:14:23Z\"\n",
    )
    .unwrap();

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client
        .query(&json!({ "read": "semantic_tokens", "path": "ops/s-001.yaml" }))
        .unwrap();
    let toks = resp["result"]["semantic_tokens"].as_array().unwrap();
    assert!(is_sorted(toks), "sorted");

    // The top-level and nested type claims, the record id, the scalar values.
    let claims: Vec<&str> = of_kind(toks, "type-claim")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(claims.contains(&"session-log"));
    assert!(
        claims.contains(&"sessionEvent"),
        "nested record claim: {claims:?}"
    );

    let ids: Vec<&str> = of_kind(toks, "block-id")
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["e-prompt"], "the inline record's ^: id");

    let session = of_kind(toks, "field-value")
        .into_iter()
        .find(|t| t["field"] == "session")
        .expect("session field-value");
    assert_eq!(
        session["value_type"],
        json!({ "kind": "primitive", "name": "String" })
    );

    // The nested record's `at` field types from sessionEvent, not null.
    let at = of_kind(toks, "field-value")
        .into_iter()
        .find(|t| t["field"] == "at")
        .expect("nested at field-value");
    assert_eq!(
        at["value_type"],
        json!({ "kind": "primitive", "name": "String" })
    );
}

#[test]
fn untyped_notes_are_served() {
    let mut h = started();
    let toks = tokens(&mut h, "content/plain.md");
    let kinds: Vec<&str> = toks.iter().map(|t| t["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        vec!["anchor", "wikilink-resolved", "block-id"],
        "a note with no type: claim still tokenizes its body"
    );
    assert_eq!(toks[0]["text"], "Title");
    assert_eq!(toks[1]["target"], "research");
    assert_eq!(toks[2]["id"], "note-mark");
}

#[test]
fn a_non_file_path_is_null() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "semantic_tokens", "path": "content/nope.md" }))
        .unwrap();
    assert!(
        resp["result"]["semantic_tokens"].is_null(),
        "an unread path returns null"
    );
}

fn names(toks: &[Value], kind: &str, field: &str) -> Vec<String> {
    of_kind(toks, kind)
        .iter()
        .map(|t| t[field].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn type_def_emits_the_shape_layer() {
    let mut h = started();
    let toks = tokens(&mut h, "type/widget.type.yaml");
    assert!(is_sorted(&toks), "tokens sorted by start: {toks:#?}");

    // The `type: note` parent claim is a type-claim, like an instance's.
    assert_eq!(names(&toks, "type-claim", "name"), vec!["note"]);

    // One field-shape container per declared field, carrying the WireShape.
    let shapes = of_kind(&toks, "field-shape");
    assert_eq!(shapes.len(), 3, "one per field: {toks:#?}");
    let level = shapes.iter().find(|t| t["field"] == "level").unwrap();
    assert_eq!(level["value_type"]["kind"], "enum");
    let kind = shapes.iter().find(|t| t["field"] == "kind").unwrap();
    assert_eq!(kind["value_type"]["kind"], "def-reference");
    assert_eq!(kind["value_type"]["bound"]["name"], "note");

    // enum literals are enum-member leaves, in order.
    assert_eq!(names(&toks, "enum-member", "value"), vec!["low", "high"]);

    // type-def name references are type-ref leaves: the def-ref bound `note`
    // and the plain reference `assumption`.
    let refs = names(&toks, "type-ref", "name");
    assert!(refs.contains(&"note".to_string()), "{refs:?}");
    assert!(refs.contains(&"assumption".to_string()), "{refs:?}");

    // the `type` keyword of the def-ref is a shape-builtin leaf.
    assert!(
        names(&toks, "shape-builtin", "name").contains(&"type".to_string()),
        "{toks:#?}"
    );
}

#[test]
fn shape_leaves_nest_inside_their_field_shape_container() {
    let mut h = started();
    let toks = tokens(&mut h, "type/widget.type.yaml");
    let kind = of_kind(&toks, "field-shape")
        .into_iter()
        .find(|t| t["field"] == "kind")
        .unwrap()
        .clone();
    let (cs, ce) = (
        kind["range"]["start"].as_u64().unwrap(),
        kind["range"]["end"].as_u64().unwrap(),
    );
    // The def-ref `type<note>*` leaves (the `type` builtin and the `note`
    // ref) sit within the container span.
    for t in toks.iter().filter(|t| {
        (t["kind"] == "type-ref" && t["name"] == "note")
            || (t["kind"] == "shape-builtin" && t["name"] == "type")
    }) {
        let (s, e) = (
            t["range"]["start"].as_u64().unwrap(),
            t["range"]["end"].as_u64().unwrap(),
        );
        assert!(
            cs <= s && e <= ce,
            "leaf {t:?} not within container {cs}..{ce}"
        );
    }
}
