//! The reference and knowledge base reads over the socket: outgoing links, target and
//! block-id resolution, the file tree, frontmatter, content, and the top-level
//! graphs. These back `LinkGraphPort`, `RepoFilesPort`, and
//! `TopLevelGraphsPort`.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

/// A knowledge base with a `type/` vocabulary and a `content/` graph. `research.md`
/// links to `decision.md` and carries a typed `^block-id` block.
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
    fs::write(
        root.join("content/research.md"),
        "---\ntype: note\ndescription: research\nrelated:\n  - ^: rec-1\n    description: an inline assumption\n---\n# Findings\n\nSee [[decision]] for context.\n\nA navigational paragraph. ^wal-tradeoff\n\n```yaml [:assumptions]\ntype: assumption\ndescription: stable\n```\n^extractor-stability\n",
    )
    .unwrap();
    fs::write(
        root.join("content/decision.md"),
        "---\ntype: note\ndescription: a decision\n---\n",
    )
    .unwrap();
    // Two body block-id references to research.md: a bare `^` and a `^^`, the
    // references_out block-id-mode case.
    fs::write(
        root.join("content/blockrefs.md"),
        "---\ntype: note\ndescription: block refs\n---\nnav [[research^wal-tradeoff]] and pull [[research^^extractor-stability]].\n",
    )
    .unwrap();
    // A body with one commit-pinned wikilink and one plain one, the
    // references_out pin-carrying case.
    fs::write(
        root.join("content/pinned.md"),
        "---\ntype: note\ndescription: pinned refs\n---\nA pinned [[decision::@abc123def]] and a plain [[decision]].\n",
    )
    .unwrap();
    // Nested headings, one carrying a trailing `^id`, the `anchors` listing
    // case: document order, per-heading level, and the marker excluded from
    // the served text.
    fs::write(
        root.join("content/outline.md"),
        "---\ntype: note\ndescription: an outline\n---\n# Top\n\nprose\n\n## Nested ^sec-nested\n\nmore prose\n\n# Second Top\n",
    )
    .unwrap();
    // One id on two surfaces, a frontmatter record and a body marker, the
    // `block_ids` duplicate case: `resolve_block_id` reaches only the first,
    // the listing reports both.
    fs::write(
        root.join("content/dupes.md"),
        "---\ntype: note\ndescription: dupes\nrelated:\n  - ^: twice\n    description: the record\n---\nA paragraph. ^twice\n",
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

#[test]
fn outgoing_references_resolve_targets_in_source_order() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "references_out", "path": "content/research.md" }))
        .unwrap();
    let refs = resp["result"]["references_out"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "one body wikilink, got {refs:?}");
    assert_eq!(refs[0]["target"], "decision");
    assert!(refs[0]["resolved"]
        .as_str()
        .unwrap()
        .ends_with("content/decision.md"));
    assert!(refs[0]["span"]["start"].as_u64().unwrap() < refs[0]["span"]["end"].as_u64().unwrap());
}

#[test]
fn outgoing_references_carry_the_commit_pin() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "references_out", "path": "content/pinned.md" }))
        .unwrap();
    let refs = resp["result"]["references_out"].as_array().unwrap();
    assert_eq!(refs.len(), 2, "two body wikilinks, got {refs:?}");
    // Source order: the pinned link first. Its `@commit` rides the edge, so a
    // consumer reads which edges are pinned off references_out, not body_events.
    assert_eq!(refs[0]["target"], "decision");
    assert_eq!(
        refs[0]["commit"], "abc123def",
        "the pin's commit is carried: {refs:?}"
    );
    // The plain link omits `commit` (skip_serializing_if), so it reads as absent.
    assert!(
        refs[1]["commit"].is_null(),
        "an unpinned edge omits commit: {refs:?}"
    );
}

#[test]
fn outgoing_references_carry_the_block_id_mode() {
    // A reference edge's `block_id` is the `{ id, referent }` object, so a consumer
    // reads whether the link is a bare `^` (navigational) or a `^^` (block-referent).
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "references_out", "path": "content/blockrefs.md" }))
        .unwrap();
    let refs = resp["result"]["references_out"].as_array().unwrap();
    assert_eq!(refs.len(), 2, "two body wikilinks, got {refs:?}");
    // Source order: the bare `^` first.
    assert_eq!(refs[0]["block_id"]["id"], "wal-tradeoff");
    assert_eq!(
        refs[0]["block_id"]["referent"], false,
        "a bare `^` edge is navigational: {refs:?}"
    );
    assert_eq!(refs[1]["block_id"]["id"], "extractor-stability");
    assert_eq!(
        refs[1]["block_id"]["referent"], true,
        "a `^^` edge is a block-referent: {refs:?}"
    );
}

#[test]
fn resolve_target_maps_a_basename_to_a_path_and_kind() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "resolve_target", "target": "decision" }))
        .unwrap();
    assert!(resp["result"]["resolve_target"]["path"]
        .as_str()
        .unwrap()
        .ends_with("content/decision.md"));
    assert_eq!(resp["result"]["resolve_target"]["kind"], "instance");

    let resp = h
        .client
        .query(&json!({ "read": "resolve_target", "target": "nope" }))
        .unwrap();
    assert!(
        resp["result"]["resolve_target"].is_null(),
        "unresolved target is null"
    );
}

#[test]
fn resolve_block_id_returns_the_typed_block_claim_and_span() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "research",
            "block_id": "extractor-stability",
        }))
        .unwrap();
    let block = &resp["result"]["resolve_block_id"];
    assert!(block["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));
    assert_eq!(block["type_claim"], json!(["assumption"]));
    assert_eq!(block["kind"], "typed_block");
    assert!(block["span"]["end"].as_u64().unwrap() > 0);

    // A block-id that doesn't exist resolves to null.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "research",
            "block_id": "missing",
        }))
        .unwrap();
    assert!(resp["result"]["resolve_block_id"].is_null());
}

#[test]
fn resolve_block_id_reaches_an_inline_record() {
    // `^: rec-1` on a claim-less record under `related?: assumption&[]`
    // ([[type block-id::au-type-system]]) — the slot-pinned claim is the type_claim.
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "research",
            "block_id": "rec-1",
        }))
        .unwrap();
    let record = &resp["result"]["resolve_block_id"];
    assert!(record["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));
    assert_eq!(record["type_claim"], json!(["assumption"]));
    assert_eq!(record["kind"], "record");
    assert!(record["span"]["end"].as_u64().unwrap() > 0);
}

#[test]
fn resolve_block_id_reaches_a_bare_marker() {
    // A bare `^id` marker is navigationally addressable ([[type block-id::au-type-system]]);
    // the read is the one resolution surface, so it resolves with an
    // empty claim and `kind: marker` instead of returning null.
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "research",
            "block_id": "wal-tradeoff",
        }))
        .unwrap();
    let marker = &resp["result"]["resolve_block_id"];
    assert!(marker["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));
    assert_eq!(marker["type_claim"], json!([]));
    assert_eq!(marker["kind"], "marker");
    assert!(marker["span"]["end"].as_u64().unwrap() > 0);
}

#[test]
fn navigation_reads_resolve_the_local_form_against_the_origin() {
    // A LOCAL wikilink (empty target + a locating fragment: `[[^^id]]` / `[[^id]]`
    // / `[[#head]]`) targets the file it appears in, per [[type reference::au-type-system]]. The
    // navigation reads honor it via `origin`, matching the validator and the
    // backlink index — a `[[^^id]]` block-referent in a file must resolve the same
    // whether or not the target name is spelled out.
    let mut h = started();

    // `[[^^extractor-stability]]` in research.md → the typed block in research.md.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "",
            "block_id": "extractor-stability",
            "origin": "content/research.md",
        }))
        .unwrap();
    let block = &resp["result"]["resolve_block_id"];
    assert!(block["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));
    assert_eq!(block["type_claim"], json!(["assumption"]));
    assert_eq!(block["kind"], "typed_block");

    // `[[^^rec-1]]` → the inline record in the same file.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "",
            "block_id": "rec-1",
            "origin": "content/research.md",
        }))
        .unwrap();
    assert_eq!(
        resp["result"]["resolve_block_id"]["kind"],
        json!("record"),
        "a local block-referent must reach the same-file record"
    );

    // `[[#Findings]]` → the heading in the same file.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_anchor",
            "target": "",
            "anchor": "Findings",
            "origin": "content/research.md",
        }))
        .unwrap();
    assert!(resp["result"]["resolve_anchor"]["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));

    // Without an `origin` there is no file to be local to, so it resolves nothing.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "",
            "block_id": "extractor-stability",
        }))
        .unwrap();
    assert!(
        resp["result"]["resolve_block_id"].is_null(),
        "an empty target with no origin resolves nothing"
    );
}

#[test]
fn navigation_reads_do_not_resolve_a_commit_pinned_target() {
    // A commit-pinned reference (`[[file::@sha]]` / `[[::@sha]]`) is an inert tombstone
    // into an immutable past: it resolves to NO live file, matching the backlink index
    // and the outbound-pin contract (`references_out` carries `resolved: null`). Live-
    // resolving it by name would misattribute on name reuse.
    let mut h = started();

    // A named pin, even with an origin (the go-to-definition path), does not jump to
    // the live `decision.md`.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_target",
            "target": "decision::@abc123def",
            "origin": "content/pinned.md",
        }))
        .unwrap();
    assert!(
        resp["result"]["resolve_target"].is_null(),
        "a named pin is a tombstone, not a live file"
    );

    // The empty-target commit-referent likewise resolves to nothing.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_target", "target": "::@abc123def" }))
        .unwrap();
    assert!(resp["result"]["resolve_target"].is_null());

    // resolve_block_id on a pinned target is inert too — no live block lookup.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id",
            "target": "decision::@abc123def",
            "block_id": "anything",
            "origin": "content/pinned.md",
        }))
        .unwrap();
    assert!(resp["result"]["resolve_block_id"].is_null());
}

#[test]
fn resolve_block_id_reaches_a_record_in_a_nested_yaml_instance_by_bare_name() {
    // The session-log shape: a pure-YAML instance nested deep under
    // `operations/`, its list entries inline records carrying `^:` ids.
    // The bare extensionless target matches the file's stem
    // ([[type reference::au-type-system]]), so `[[s-001^e-prompt]]` resolves cross-file.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir_all(root.join("operations/sessions/2026")).unwrap();
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
        root.join("operations/sessions/2026/s-001.yaml"),
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
        .query(&json!({
            "read": "resolve_block_id",
            "target": "s-001",
            "block_id": "e-prompt",
        }))
        .unwrap();
    let record = &resp["result"]["resolve_block_id"];
    assert!(record["file_path"]
        .as_str()
        .unwrap()
        .ends_with("operations/sessions/2026/s-001.yaml"));
    assert_eq!(record["type_claim"], json!(["sessionEvent"]));
    assert_eq!(record["kind"], "record");
    assert!(record["span"]["end"].as_u64().unwrap() > 0);

    // resolve_target reaches the same file and names its kind.
    let resp = client
        .query(&json!({ "read": "resolve_target", "target": "s-001" }))
        .unwrap();
    assert!(resp["result"]["resolve_target"]["path"]
        .as_str()
        .unwrap()
        .ends_with("s-001.yaml"));
    assert_eq!(resp["result"]["resolve_target"]["kind"], "instance");
}

#[test]
fn resolve_anchor_matches_heading_text_case_insensitively() {
    // The engine owns anchor matching ([[type reference::au-type-system]]): exact
    // heading text, case-insensitive, first in document order.
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_anchor",
            "target": "research",
            "anchor": "findings",
        }))
        .unwrap();
    let heading = &resp["result"]["resolve_anchor"];
    assert!(heading["file_path"]
        .as_str()
        .unwrap()
        .ends_with("content/research.md"));
    assert!(heading["span"]["end"].as_u64().unwrap() > heading["span"]["start"].as_u64().unwrap());

    // No matching heading resolves to null.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_anchor",
            "target": "research",
            "anchor": "No Such Heading",
        }))
        .unwrap();
    assert!(resp["result"]["resolve_anchor"].is_null());
}

#[test]
fn anchors_lists_every_heading_in_document_order_with_its_level() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "anchors", "target": "outline" }))
        .unwrap();
    let anchors = resp["result"]["anchors"].as_array().unwrap();

    let seen: Vec<(&str, u64)> = anchors
        .iter()
        .map(|a| (a["text"].as_str().unwrap(), a["level"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        seen,
        vec![("Top", 1), ("Nested", 2), ("Second Top", 1)],
        "document order, with each heading's own level",
    );

    // Spans ascend with document order, and each covers real bytes.
    let starts: Vec<u64> = anchors
        .iter()
        .map(|a| a["span"]["start"].as_u64().unwrap())
        .collect();
    assert!(
        starts.windows(2).all(|w| w[0] < w[1]),
        "spans ascend: {starts:?}",
    );
    for a in anchors {
        assert!(a["span"]["end"].as_u64().unwrap() > a["span"]["start"].as_u64().unwrap());
    }
}

#[test]
fn an_anchors_entry_excludes_the_trailing_marker_and_round_trips() {
    // `## Nested ^sec-nested` is addressable as `#Nested`, not
    // `#Nested ^sec-nested` — the served text is what resolve_anchor
    // matches, so a consumer inserts it verbatim ([[type block-id::au-type-system]]).
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "anchors", "target": "outline" }))
        .unwrap();
    let nested = resp["result"]["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["level"] == 2)
        .expect("the nested heading");
    let text = nested["text"].as_str().unwrap().to_string();
    assert_eq!(text, "Nested", "the `^sec-nested` marker is not part of it");

    // The listing and the singular agree: what `anchors` serves resolves.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_anchor", "target": "outline", "anchor": text }))
        .unwrap();
    let resolved = &resp["result"]["resolve_anchor"];
    assert!(!resolved.is_null(), "served text must round-trip");
    assert_eq!(resolved["span"]["start"], nested["span"]["start"]);
}

#[test]
fn anchors_separates_an_unresolved_target_from_a_file_with_no_headings() {
    let mut h = started();

    // A resolved file carrying no heading answers an EMPTY array: it exists,
    // it simply has none.
    let resp = h
        .client
        .query(&json!({ "read": "anchors", "target": "decision" }))
        .unwrap();
    let anchors = resp["result"]["anchors"].as_array().unwrap();
    assert!(anchors.is_empty(), "no headings, got {anchors:?}");

    // An unresolved target answers null, the unresolved-lookup signal.
    let resp = h
        .client
        .query(&json!({ "read": "anchors", "target": "no-such-file" }))
        .unwrap();
    assert!(
        resp["result"]["anchors"].is_null(),
        "an absent target is null, never an empty array",
    );
}

#[test]
fn block_ids_lists_every_surface_in_document_order() {
    // research.md carries all three: a frontmatter `^:` record, a bare body
    // marker, and a typed `[:field]` fence id ([[type block-id::au-type-system]]).
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "block_ids", "target": "research" }))
        .unwrap();
    let ids = resp["result"]["block_ids"].as_array().unwrap();

    let seen: Vec<(&str, &str)> = ids
        .iter()
        .map(|b| (b["id"].as_str().unwrap(), b["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("rec-1", "record"),
            ("wal-tradeoff", "marker"),
            ("extractor-stability", "typed_block"),
        ],
        "frontmatter record first, then body in document order",
    );

    // Only the typed block carries a claim; a marker is navigational.
    let typed = ids.iter().find(|b| b["kind"] == "typed_block").unwrap();
    assert_eq!(typed["type_claim"], json!(["assumption"]));
    let marker = ids.iter().find(|b| b["kind"] == "marker").unwrap();
    assert_eq!(
        marker["type_claim"],
        json!([]),
        "a marker is navigational, never a typed-reference target",
    );
}

#[test]
fn block_ids_agrees_with_resolve_block_id_on_every_entry() {
    // The listing is the plural of the singular, so each entry must carry
    // what resolving that same id returns.
    let mut h = started();
    let listed = h
        .client
        .query(&json!({ "read": "block_ids", "target": "research" }))
        .unwrap()["result"]["block_ids"]
        .as_array()
        .unwrap()
        .clone();

    for entry in &listed {
        let id = entry["id"].as_str().unwrap();
        let resolved = h
            .client
            .query(&json!({
                "read": "resolve_block_id",
                "target": "research",
                "block_id": id,
            }))
            .unwrap()["result"]["resolve_block_id"]
            .clone();
        assert!(!resolved.is_null(), "listed id `{id}` must resolve");
        assert_eq!(resolved["kind"], entry["kind"], "kind disagrees for `{id}`");
        assert_eq!(
            resolved["type_claim"], entry["type_claim"],
            "type_claim disagrees for `{id}`",
        );
        assert_eq!(
            resolved["span"]["start"], entry["span"]["start"],
            "span disagrees for `{id}`",
        );
    }
}

#[test]
fn block_ids_lists_a_duplicate_id_once_per_occurrence() {
    // `[[file^twice]]` resolves to the first occurrence only, but the file
    // carries two. A listing that dropped the second would hide exactly what
    // `block-id-duplicate` reports.
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "block_ids", "target": "dupes" }))
        .unwrap();
    let ids = resp["result"]["block_ids"].as_array().unwrap();

    let twice: Vec<&serde_json::Value> = ids.iter().filter(|b| b["id"] == "twice").collect();
    assert_eq!(twice.len(), 2, "both occurrences listed, got {ids:?}",);
    assert_eq!(twice[0]["kind"], "record", "frontmatter comes first");
    assert_eq!(twice[1]["kind"], "marker");
    assert!(
        twice[0]["span"]["start"].as_u64().unwrap() < twice[1]["span"]["start"].as_u64().unwrap(),
        "document order across the two surfaces",
    );

    // The engine still diagnoses the collision, so the listing and the
    // diagnostic agree about the file.
    let diags = h
        .client
        .query(&json!({ "read": "diagnostics", "path": "content/dupes.md" }))
        .unwrap()["result"]["diagnostics"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        diags.iter().any(|d| d["code"] == "block-id-duplicate"),
        "block-id-duplicate fires alongside, got {diags:?}",
    );
}

#[test]
fn block_ids_separates_an_unresolved_target_from_a_file_with_none() {
    let mut h = started();

    let resp = h
        .client
        .query(&json!({ "read": "block_ids", "target": "decision" }))
        .unwrap();
    assert!(
        resp["result"]["block_ids"].as_array().unwrap().is_empty(),
        "a resolved file with no ids is an empty array",
    );

    let resp = h
        .client
        .query(&json!({ "read": "block_ids", "target": "no-such-file" }))
        .unwrap();
    assert!(
        resp["result"]["block_ids"].is_null(),
        "an absent target is null, never an empty array",
    );
}

#[test]
fn block_ids_reaches_records_in_a_pure_yaml_instance() {
    // A pure-YAML instance has no markdown body, so it carries no headings —
    // but its frontmatter records are still addressable.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
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
        root.join("s-001.yaml"),
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
        .query(&json!({ "read": "block_ids", "target": "s-001" }))
        .unwrap();
    let ids = resp["result"]["block_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1, "the one record id, got {ids:?}");
    assert_eq!(ids[0]["id"], "e-prompt");
    assert_eq!(ids[0]["kind"], "record");
    assert_eq!(ids[0]["type_claim"], json!(["sessionEvent"]));

    // The same file has no headings, so `anchors` is empty rather than null.
    let resp = client
        .query(&json!({ "read": "anchors", "target": "s-001" }))
        .unwrap();
    assert!(resp["result"]["anchors"].as_array().unwrap().is_empty());
}

#[test]
fn children_lists_files_and_dirs_excluding_hidden() {
    let mut h = started();

    // Root children: the two graph directories.
    let resp = h
        .client
        .query(&json!({ "read": "dir_entries", "dir": "" }))
        .unwrap();
    let entries = resp["result"]["dir_entries"].as_array().unwrap();
    let dirs: Vec<&str> = entries
        .iter()
        .filter(|e| e["kind"] == "directory")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        dirs.contains(&"content") && dirs.contains(&"type"),
        "got {dirs:?}"
    );

    // content/ children: the two instance files.
    let resp = h
        .client
        .query(&json!({ "read": "dir_entries", "dir": "content" }))
        .unwrap();
    let names: Vec<&str> = resp["result"]["dir_entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"research.md") && names.contains(&"decision.md"),
        "got {names:?}"
    );
}

#[test]
fn frontmatter_and_content_read_a_file() {
    let mut h = started();

    let resp = h
        .client
        .query(&json!({ "read": "frontmatter", "path": "content/research.md" }))
        .unwrap();
    assert_eq!(resp["result"]["frontmatter"]["type"], "note");
    assert_eq!(resp["result"]["frontmatter"]["description"], "research");

    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "content/research.md" }))
        .unwrap();
    let text = resp["result"]["content"]["text"].as_str().unwrap();
    assert!(text.contains("# Findings"), "content is the source text");
    assert!(text.starts_with("---"), "content includes the frontmatter");
}

#[test]
fn top_level_graphs_lists_root_subdirectories() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({ "read": "top_level_dirs" }))
        .unwrap();
    let names: Vec<&str> = resp["result"]["top_level_dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["content", "type"],
        "graphs are the root subdirs"
    );
}

/// `references_in.kind` classifies each inbound edge into the three
/// shape-independent classes, derived from `surface` and `slot`:
/// - `field` — a frontmatter-surface edge.
/// - `contributing` — a body `[[target:field]]` data contribution.
/// - `navigational` — a body prose link.
#[test]
fn inbound_edges_carry_the_three_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  ref?: note*\n  mention?: note*\n",
    )
    .unwrap();
    // The target every other file points at, three different ways.
    fs::write(root.join("hub.md"), "---\ntype: note\n---\n# Hub\n").unwrap();
    // A frontmatter field edge → `field`.
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nref: \"[[hub]]\"\n---\n",
    )
    .unwrap();
    // A body `[[hub:mention]]` contribution → `contributing`.
    fs::write(
        root.join("b.md"),
        "---\ntype: note\nmention:\n---\nSee [[hub:mention]] here.\n",
    )
    .unwrap();
    // A body prose link → `navigational`.
    fs::write(
        root.join("c.md"),
        "---\ntype: note\n---\nA prose [[hub]] link.\n",
    )
    .unwrap();

    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let mut client = Client::connect(&socket).expect("connect");

    let resp = client
        .query(&json!({ "read": "references_in", "path": "hub.md" }))
        .unwrap();
    let refs = resp["result"]["references_in"].as_array().unwrap();

    // One inbound edge per source, each with its kind and matching surface.
    let by_kind = |k: &str| {
        refs.iter()
            .find(|r| r["kind"] == k)
            .unwrap_or_else(|| panic!("a {k} inbound edge is present, got {refs:?}"))
    };

    let field = by_kind("field");
    assert_eq!(
        field["surface"], "frontmatter",
        "field is a frontmatter edge"
    );
    assert_eq!(field["slot"], "ref", "the frontmatter field key");

    let contributing = by_kind("contributing");
    assert_eq!(
        contributing["surface"], "body",
        "contributing is a body edge"
    );
    assert_eq!(contributing["slot"], "mention", "the `:field` attribution");

    let navigational = by_kind("navigational");
    assert_eq!(
        navigational["surface"], "body",
        "navigational is a body edge"
    );
    assert!(
        navigational["slot"].is_null(),
        "a prose link carries no slot: {navigational:?}"
    );

    // The derivation never disagrees with the surface / slot it is computed from.
    for r in refs {
        let expected = match (r["surface"].as_str().unwrap(), r["slot"].is_null()) {
            ("frontmatter", _) => "field",
            ("body", false) => "contributing",
            ("body", true) => "navigational",
            other => panic!("unexpected surface/slot {other:?}"),
        };
        assert_eq!(r["kind"], expected, "kind agrees with surface+slot: {r:?}");
    }
}
