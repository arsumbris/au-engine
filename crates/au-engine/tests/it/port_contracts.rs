//! The standing contract guard: one test per consumer port, asserting the
//! daemon read it binds to returns the fields that port consumes.
//!
//! The consumer's adapters normalize names (snake_case to camelCase, `span` to
//! `range`, `fix` to `suggestedFix`) and enum values; these tests assert the
//! engine-native fields those adapters map from. A wire change that drops or
//! renames a field the consumer needs breaks a test here, before it breaks the
//! consumer.
//!
//! Ports covered: DiagnosticsPort, TypeIndexPort, BodyTemplatePort,
//! ProvenancePort, LinkGraphPort, RepoFilesPort, TopLevelGraphsPort.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

/// A knowledge base exercising every read: a type hierarchy with a body, instances with
/// field values and a typed block, a resolving body wikilink, and one instance
/// missing a required field so diagnostics are non-empty.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name.
    let root = fs::canonicalize(dir.path()).unwrap().join("v");
    fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::create_dir(root.join("content")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  description: String\n  assumptions?: assumption&[+]\nbody:\n  - section: Why\n",
    )
    .unwrap();
    fs::write(
        root.join("type/assumption.type.yaml"),
        "fields:\n  description: String\n",
    )
    .unwrap();
    fs::write(
        root.join("content/research.md"),
        "---\ntype: note\ndescription: research\n---\n# Why\n\nSee [[decision]].\n\n```yaml [:assumptions]\ntype: assumption\ndescription: stable\n```\n^a1\n",
    )
    .unwrap();
    fs::write(
        root.join("content/decision.md"),
        "---\ntype: note\ndescription: a decision\n---\n",
    )
    .unwrap();
    // Missing the required `description` — guarantees a diagnostic.
    fs::write(root.join("content/broken.md"), "---\ntype: note\n---\n").unwrap();
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

impl Harness {
    /// Issue a read and return its PAYLOAD, unwrapped through the uniform
    /// envelope as `result[verb]` — the same one-accessor pattern an SDK uses,
    /// which is the whole point of key-equals-verb: a consumer never needs
    /// per-read unwrap knowledge. See `WIRE.md`, "The result envelope".
    ///
    /// A missing key is a hard failure now that the whole catalog is enveloped:
    /// the earlier raw-`result` fallback for unswept reads is gone, so a read
    /// dropping its payload key is caught here rather than silently tolerated.
    fn read(&mut self, req: Value) -> Value {
        let resp = self.client.query(&req).unwrap();
        assert_eq!(resp["ready"], true, "read not ready: {req}");
        let verb = req["read"].as_str().expect("a read names its verb");
        resp["result"]
            .get(verb)
            .cloned()
            .unwrap_or_else(|| panic!("read '{verb}' result carries no '{verb}' key: {resp}"))
    }
}

fn is_string(v: &Value) -> bool {
    v.is_string()
}
fn is_uint(v: &Value) -> bool {
    v.as_u64().is_some()
}

#[test]
fn diagnostics_port_contract() {
    let mut h = started();
    let result = h.read(json!({ "read": "diagnostics" }));
    let diags = result.as_array().expect("diagnostics is an array");
    assert!(!diags.is_empty(), "broken.md should produce a diagnostic");
    let d = &diags[0];
    // code, severity, message, and a byte-range span — the fields
    // DiagnosticsPort maps (span -> range, fix -> suggestedFix).
    assert!(is_string(&d["code"]), "code is a kebab-case string");
    assert!(
        matches!(d["severity"].as_str(), Some("error" | "warning" | "hint")),
        "severity is a lowercase enum, got {}",
        d["severity"]
    );
    assert!(is_string(&d["message"]));
    assert!(is_string(&d["span"]["file"]), "span carries the file");
    assert!(is_uint(&d["span"]["range"]["start"]), "byte-offset start");
    assert!(is_uint(&d["span"]["range"]["end"]), "byte-offset end");
}

#[test]
fn type_index_port_contract() {
    let mut h = started();

    // listTypes <- `types`
    let types = h.read(json!({ "read": "types" }));
    let note = types
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "note")
        .expect("note type");
    assert!(is_string(&note["name"]));
    assert!(note["parents"].is_array());
    assert!(
        is_string(&note["source"]["file"]),
        "definedAt <- source.file"
    );
    let field = &note["fields"][0];
    assert!(is_string(&field["name"]));
    assert!(is_string(&field["shape"]));
    assert!(field["required"].is_boolean());
    // shape_ast <- parsed_shape, the structured AST beside the string.
    assert_eq!(
        field["shape_ast"],
        json!({ "kind": "primitive", "name": "String" }),
        "shape_ast mirrors the parsed slot shape"
    );

    // A nested shape: note's `assumptions?: assumption&[+]` is a
    // non-empty list wrapping an inline-or-reference.
    let assumptions = note["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "assumptions")
        .expect("assumptions field");
    assert_eq!(
        assumptions["shape_ast"],
        json!({
            "kind": "list",
            "min": 1,
            "inner": { "kind": "inline-or-reference", "name": "assumption" }
        }),
        "shape_ast nests faithfully, references are leaves"
    );

    // listInstancesOf <- `instances_of`: a flat match record per (instance,
    // matched identity), tagged claimed / inherited.
    let rows = h.read(json!({ "read": "instances_of", "type": "note" }));
    let row = &rows.as_array().unwrap()[0];
    assert!(is_string(&row["path"]));
    assert!(row["claim"].is_array(), "typeClaim <- claim");
    assert!(row["fields"].is_object(), "frontmatter field map");
    assert!(is_string(&row["name"]), "matched identity name");
    assert!(is_string(&row["hash"]), "matched identity hash");
    assert!(row["type_owners"].is_array(), "type owner repos");
    assert!(
        is_string(&row["member"]),
        "the instance file's owning member"
    );
    assert!(row["claimed"].is_boolean());
    assert!(row["inherited"].is_boolean());
}

#[test]
fn body_template_port_contract() {
    let mut h = started();
    let note = h.read(json!({ "read": "type", "name": "note" }));
    // getBodyTemplate <- body, getEffectiveBody <- effective_body.
    let body = note["body"].as_array().expect("body is an array");
    assert_eq!(body[0]["kind"], "section");
    assert!(is_string(&body[0]["name"]));
    assert!(body[0]["optional"].is_boolean());
    assert!(
        note["effective_body"].is_array(),
        "post-splice form present"
    );
}

#[test]
fn provenance_port_contract() {
    let mut h = started();
    let r = h.read(json!({ "read": "instance", "path": "content/research.md" }));

    // getEffectiveValues <- effective_values, with full contribution provenance.
    let values = r["effective_values"].as_array().expect("effective_values");
    let desc = values
        .iter()
        .find(|e| e["field"] == "description")
        .expect("description field");
    let contribution = &desc["containers"][0]["contributions"][0];
    assert!(is_string(&contribution["surface"]), "contribution surface");
    assert!(is_string(&contribution["location"]["file"]));
    assert!(is_uint(&contribution["location"]["byte_range"]["start"]));
    assert!(contribution["section_path"].is_array());
    assert!(contribution["value"]["kind"].is_string());

    // getSectionPresence <- section_presence.
    let presence = r["section_presence"].as_array().expect("section_presence");
    let why = presence.iter().find(|s| s["name"] == "Why").unwrap();
    assert!(why["present"].is_boolean());
    assert!(is_uint(&why["depth"]));
    assert!(why["path"].is_array());

    // getBodyEvents <- body_events.
    assert!(r["body_events"].is_array(), "markdown body event stream");
}

#[test]
fn link_graph_port_contract() {
    let mut h = started();

    // getOutgoing <- references_out.
    let out = h.read(json!({ "read": "references_out", "path": "content/research.md" }));
    let link = &out.as_array().unwrap()[0];
    assert!(is_string(&link["target"]));
    assert!(link["resolved"].is_string() || link["resolved"].is_null());
    assert!(is_uint(&link["span"]["start"]) && is_uint(&link["span"]["end"]));

    // getBacklinks <- backlinks (source + span; consumer derives line/snippet).
    let back = h.read(json!({ "read": "references_in", "path": "content/decision.md" }));
    let edge = &back.as_array().unwrap()[0];
    assert!(is_string(&edge["source"]), "path <- source");
    assert!(is_uint(&edge["span_start"]) && is_uint(&edge["span_end"]));

    // resolveTarget <- resolve_target.
    let resolved = h.read(json!({ "read": "resolve_target", "target": "decision" }));
    assert!(is_string(&resolved["path"]));
    assert!(is_string(&resolved["kind"]));

    // resolveBlockId <- resolve_block_id.
    let block = h.read(json!({
        "read": "resolve_block_id", "target": "research", "block_id": "a1"
    }));
    assert!(is_string(&block["file_path"]));
    assert!(block["type_claim"].is_array());
    assert!(is_uint(&block["span"]["start"]) && is_uint(&block["span"]["end"]));
}

#[test]
fn files_port_contract() {
    let mut h = started();

    // listChildren <- children.
    let children = h.read(json!({ "read": "dir_entries", "dir": "content" }));
    let entry = &children.as_array().unwrap()[0];
    assert!(is_string(&entry["path"]));
    assert!(is_string(&entry["name"]));
    assert!(matches!(entry["kind"].as_str(), Some("file" | "directory")));

    // readFrontmatter <- frontmatter.
    let fm = h.read(json!({ "read": "frontmatter", "path": "content/research.md" }));
    assert!(fm.is_object(), "frontmatter is a JSON map");
    assert_eq!(fm["type"], "note");
}

#[test]
fn top_level_dirs_port_contract() {
    let mut h = started();
    let dirs = h.read(json!({ "read": "top_level_dirs" }));
    let d = &dirs.as_array().unwrap()[0];
    assert!(is_string(&d["name"]));
    // `path`, not `folder_path` — and each entry now names the MEMBER that
    // owns it, since the read spans every mounted member rather than the entry.
    assert!(is_string(&d["path"]));
    assert!(is_string(&d["repo"]), "each dir names its owning member");
}

#[test]
fn semantic_tokens_port_contract() {
    // SemanticTokensPort <- `semantic_tokens`. `research.md` carries a type
    // claim, a typed field value, a resolving wikilink, a heading, a typed
    // `[:assumptions]` fence with an inner field, and a trailing fence id —
    // every kind but the bare marker / broken link / `^:` record.
    let mut h = started();
    let toks = h.read(json!({ "read": "semantic_tokens", "path": "content/research.md" }));
    let toks = toks.as_array().expect("token array");

    // Every token: a byte-range span plus a kind discriminator.
    let mut prev = 0u64;
    for t in toks {
        let span = &t["range"];
        assert!(
            is_uint(&span["start"]) && is_uint(&span["end"]),
            "range {t}"
        );
        let start = span["start"].as_u64().unwrap();
        assert!(start >= prev, "sorted by range.start");
        prev = start;
        assert!(is_string(&t["kind"]), "kind discriminator {t}");
    }

    let one = |kind: &str| -> serde_json::Value {
        toks.iter()
            .find(|t| t["kind"] == kind)
            .unwrap_or_else(|| panic!("a {kind} token"))
            .clone()
    };

    // Each kind carries the fields the port maps.
    assert!(is_string(&one("type-claim")["name"]));
    assert!(is_string(&one("anchor")["text"]));
    assert!(is_string(&one("block-id")["id"]));

    let wl = one("wikilink-resolved");
    assert!(is_string(&wl["target"]) && is_string(&wl["resolved"]));

    let tb = one("typed-block");
    assert_eq!(tb["field"], "assumptions");

    // field-value carries the field name and a WireShape value_type.
    let fv = one("field-value");
    assert!(is_string(&fv["field"]));
    assert!(
        is_string(&fv["value_type"]["kind"]),
        "value_type is a WireShape"
    );
}

#[test]
fn neighborhood_read_contract() {
    // The neighborhood read's standing shape: a subgraph of nodes plus edges,
    // with the truncation report. research.md links decision.md, so a depth-1
    // out walk has at least one edge and two nodes.
    let mut h = started();
    let n = h.read(json!({ "read": "neighborhood", "path": "content/research.md" }));

    // The aggregate shape.
    let nodes = n["nodes"].as_array().expect("nodes array");
    let edges = n["edges"].as_array().expect("edges array");
    assert!(n["truncated"].is_boolean(), "truncated is a bool");
    assert!(n["dropped"].is_array(), "dropped is an array");
    assert!(
        nodes.len() >= 2 && !edges.is_empty(),
        "seed + target, one edge"
    );

    // Every node carries its identity, depth, kind, and the two sizes (a size is
    // a number or null, never absent).
    for node in nodes {
        assert!(is_string(&node["path"]), "node path {node}");
        assert!(is_uint(&node["depth"]), "node depth {node}");
        assert!(is_string(&node["file_kind"]), "node file_kind {node}");
        assert!(
            node["bytes"].is_u64() || node["bytes"].is_null(),
            "bytes {node}"
        );
        assert!(
            node["body_bytes"].is_u64() || node["body_bytes"].is_null(),
            "body_bytes {node}"
        );
    }

    // Every edge carries endpoints, a coarse kind, a surface, and a span.
    for edge in edges {
        assert!(is_string(&edge["from"]["path"]), "edge from.path {edge}");
        assert!(
            edge["to"].is_null() || is_string(&edge["to"]["path"]),
            "edge to is null or a node ref {edge}"
        );
        let kind = edge["kind"].as_str().expect("edge kind");
        assert!(
            matches!(kind, "navigational" | "contributing" | "field"),
            "edge kind is the coarse vocabulary, got {kind}"
        );
        assert!(is_string(&edge["surface"]), "edge surface {edge}");
        assert!(
            is_uint(&edge["span_start"]) && is_uint(&edge["span_end"]),
            "edge span {edge}"
        );
    }

    // Enrichment: content / body / instance splice onto a node when requested,
    // and the node's `bytes` is exactly the content length it costs.
    let enriched = h.read(json!({
        "read": "neighborhood", "path": "content/research.md",
        "content": true, "body": true, "instance": true
    }));
    let seed = enriched["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["path"].as_str().unwrap().ends_with("research.md"))
        .unwrap();
    assert!(is_string(&seed["content"]["text"]), "content.text {seed}");
    assert_eq!(
        seed["content"]["text"].as_str().unwrap().len(),
        seed["bytes"].as_u64().unwrap() as usize,
        "bytes equals the content it costs"
    );
    assert!(seed["body"].is_string(), "body prose {seed}");
    assert!(
        seed["instance"]["resolved"].is_boolean(),
        "instance resolved view {seed}"
    );
}
