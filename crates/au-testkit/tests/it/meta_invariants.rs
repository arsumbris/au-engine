//! Property + scenario tests for meta sub-region body validation and the
//! `lookup_meta` consumer walk (spec §8.2, §8.3, §8.4, §8.5).
//!
//! Two surfaces:
//! - `validate_meta_bodies` — load-time per-block body validation against
//!   the named meta-type-def's effective shape. Mirrors instance
//!   validation (required-field-absent, field-shape-mismatch, sealed-leaf
//!   rule, unknown-type-claim).
//! - `lookup_meta` — canonical ancestor walk. Suppression stops it (§8.5);
//!   walks don't recurse into encountered meta-type-defs (§8.2).

use std::collections::BTreeMap;
use std::path::PathBuf;

use au_core::{
    build_graph, instance::InstanceField, lookup_meta, validate_meta_bodies, InstanceValue,
    MapRefData, MetaBlock, TypeDef, TypeGraph, TypeName, ValidateContext,
};
use au_diagnostics::{ByteRange, Diagnostic};
use au_references::RepoIndex;
use au_testkit::{
    instance_bare, instance_field, type_def_sealed, type_def_with_fields, type_def_with_parents,
    validate_simple,
};
use proptest::prelude::*;

// ----- helpers -----

fn codes_of(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

fn tn(s: &str) -> TypeName {
    TypeName(s.into())
}

/// Run meta-body validation against `graph`. Empty knowledge base + claims — these
/// tests don't exercise reference fields inside meta bodies
/// (`scenarios/corpus-meta-validation/` covers that end-to-end).
fn validate_meta_simple(graph: &TypeGraph) -> Vec<Diagnostic> {
    let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
    let claims: BTreeMap<PathBuf, Vec<TypeName>> = BTreeMap::new();
    let body_sources = BTreeMap::new();
    let record_targets = BTreeMap::new();
    let ref_data = MapRefData {
        claims_by_path: &claims,
        body_sources: &body_sources,
        record_targets: &record_targets,
    };
    let ctx = ValidateContext {
        graph,
        repo_index: &idx,
        ref_data: &ref_data,
        cross_repo: None,
        resolution: None,
        meta_marker: None,
    };
    validate_meta_bodies(&ctx)
}

/// Compose a `meta:` block list onto an existing TypeDef. Mirrors
/// `with_sealed` from `sealed_leaf_invariants.rs` — au-testkit's
/// builders don't carry meta by default; tests opt in.
fn with_meta(mut td: TypeDef, blocks: Vec<MetaBlock>) -> TypeDef {
    td.meta_blocks = Some(blocks);
    td
}

/// Mark `meta: []` (§8.5 suppression form) on a TypeDef.
fn with_suppressed_meta(mut td: TypeDef) -> TypeDef {
    td.meta_blocks = Some(vec![]);
    td
}

/// Build a meta sub-region with body fields. Spans default to zero —
/// tests match on diagnostic codes + names rather than byte ranges.
fn meta_block(type_name: &str, fields: Vec<(&str, InstanceValue)>) -> MetaBlock {
    MetaBlock {
        type_name: TypeName(type_name.into()),
        repo: None,
        type_name_span: ByteRange::new(0, 0),
        block_span: ByteRange::new(0, 0),
        fields: fields
            .into_iter()
            .map(|(k, v)| InstanceField {
                key: k.into(),
                key_span: ByteRange::new(0, 0),
                value: v,
                value_span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            })
            .collect(),
        body_span: ByteRange::new(0, 0),
        doc: None,
        field_docs: Default::default(),
    }
}

// ----- §8.5 suppression invariants -----

proptest! {
    /// For any host with `meta: []` and any meta-type-name, `lookup_meta`
    /// returns None regardless of ancestor declarations. Locks the §8.5
    /// "stop signal" semantic: walks originating at the suppressed node
    /// see no ancestor metas.
    #[test]
    fn suppression_stops_walk_for_any_meta_type_name(
        meta_name in "[a-z][a-z0-9_-]{0,6}",
    ) {
        let g = build_graph(vec![
            // Ancestor declares display-meta + runtime-meta.
            with_meta(
                type_def_with_parents("note", &[]),
                vec![meta_block("display-meta", vec![]), meta_block("runtime-meta", vec![])],
            ),
            // Host suppresses everything.
            with_suppressed_meta(type_def_with_parents("decision", &["note"])),
            type_def_with_fields("display-meta", &[], &[]),
            type_def_with_fields("runtime-meta", &[], &[]),
        ])
        .graph;
        // For ANY meta name (including ones that exist on the ancestor),
        // the suppressed host returns None.
        prop_assert!(lookup_meta(&g, None, &tn("decision"), &tn(&meta_name), None).is_none());
    }
}

#[test]
fn re_declaration_unblocks_only_for_that_name() {
    // Chain: note(display-meta + runtime-meta) → decision(meta:[]) →
    // committed(display-meta). At `committed`:
    //  - display-meta resolves to committed's own (re-declared).
    //  - runtime-meta walks up, hits decision's suppression, returns None.
    //  - Other names (never declared) return None too — walk still hits
    //    the suppression.
    let mut display_for_committed = meta_block("display-meta", vec![]);
    display_for_committed.block_span = ByteRange::new(100, 200);

    let g = build_graph(vec![
        type_def_with_fields("display-meta", &[], &[]),
        type_def_with_fields("runtime-meta", &[], &[]),
        with_meta(
            type_def_with_parents("note", &[]),
            vec![
                meta_block("display-meta", vec![]),
                meta_block("runtime-meta", vec![]),
            ],
        ),
        with_suppressed_meta(type_def_with_parents("decision", &["note"])),
        with_meta(
            type_def_with_parents("committed", &["decision"]),
            vec![display_for_committed],
        ),
    ])
    .graph;

    let display = lookup_meta(&g, None, &tn("committed"), &tn("display-meta"), None);
    assert!(
        display.is_some(),
        "committed's re-declared display-meta should resolve"
    );
    assert_eq!(
        display.unwrap().block_span.start,
        100,
        "committed's own block, not note's"
    );

    let runtime = lookup_meta(&g, None, &tn("committed"), &tn("runtime-meta"), None);
    assert!(
        runtime.is_none(),
        "runtime-meta is suppressed at decision; committed's re-declaration of display-meta doesn't lift the suppression for OTHER names"
    );
}

// ----- body validation mirrors instance validation -----

#[test]
fn meta_body_validation_mirrors_instance_validation() {
    // Lock the symmetry: for the same meta-type-def with the same body
    // content, meta-body validation and instance validation produce the
    // same diagnostic CODE set.
    //
    // Two scenarios for the symmetry lock:
    //  (a) all required fields present → both produce empty diagnostics.
    //  (b) required field missing → both produce required-field-absent.
    //
    // Equality is on the projected (sorted) code list, since the
    // diagnostic messages and spans naturally differ between the two
    // sites (host name vs instance name; sub-region span vs file-level
    // span). The structural rule must agree.
    let g = build_graph(vec![
        type_def_with_fields("display-meta", &[], &["tldr"]),
        with_meta(
            type_def_with_parents("clean-host", &[]),
            vec![meta_block(
                "display-meta",
                vec![("tldr", InstanceValue::String("a".into()))],
            )],
        ),
        with_meta(
            type_def_with_parents("missing-host", &[]),
            vec![meta_block("display-meta", vec![])],
        ),
    ])
    .graph;

    // Run meta validation once, then partition by host name (each meta
    // diag's message names its host TypeDef). Compare each site's projected
    // code set against the file-level instance validator on the same input.
    let all_meta_diags = validate_meta_simple(&g);
    let clean_meta_codes: Vec<String> = all_meta_diags
        .iter()
        .filter(|d| d.message.contains("'clean-host'"))
        .map(|d| d.code.as_str().to_string())
        .collect();
    let missing_meta_codes: Vec<String> = all_meta_diags
        .iter()
        .filter(|d| d.message.contains("'missing-host'"))
        .map(|d| d.code.as_str().to_string())
        .collect();

    let clean_instance_diags = validate_simple(
        &g,
        &instance_bare(
            "display-meta",
            vec![instance_field("tldr", InstanceValue::String("a".into()))],
        ),
    );
    let missing_instance_diags = validate_simple(&g, &instance_bare("display-meta", vec![]));
    let clean_instance_codes: Vec<String> = codes_of(&clean_instance_diags)
        .into_iter()
        .map(String::from)
        .collect();
    let missing_instance_codes: Vec<String> = codes_of(&missing_instance_diags)
        .into_iter()
        .map(String::from)
        .collect();

    assert!(clean_meta_codes.is_empty());
    assert!(clean_instance_codes.is_empty());
    assert_eq!(missing_meta_codes, missing_instance_codes);
    assert_eq!(missing_meta_codes, vec!["required-field-absent"]);
}

// ----- sealed-leaf rule at meta site -----

proptest! {
    /// For any sealed meta-type-def name, a host that declares
    /// `meta: [- type: <sealed-name>]` fires `sealed-parent-claimed`
    /// at the meta site. Universal §3.3 rule, new fire surface.
    ///
    /// The graph is fixed (small set of named type-defs); the proptest
    /// just sweeps which sealed name the host claims. Mirrors the
    /// `fires_iff_is_sealed` lock.
    #[test]
    fn sealed_meta_type_fires_sealed_parent_claimed(
        idx in 0usize..3,
    ) {
        // Three sealed meta-type-def names; pick one by idx.
        let names = ["sealed-display-meta", "sealed-runtime-meta", "sealed-agent-meta"];
        let pick = names[idx];

        let g = build_graph(vec![
            type_def_sealed("sealed-display-meta", &["sealed-display-meta.x"]),
            type_def_with_parents("sealed-display-meta.x", &["sealed-display-meta"]),
            type_def_sealed("sealed-runtime-meta", &["sealed-runtime-meta.x"]),
            type_def_with_parents("sealed-runtime-meta.x", &["sealed-runtime-meta"]),
            type_def_sealed("sealed-agent-meta", &["sealed-agent-meta.x"]),
            type_def_with_parents("sealed-agent-meta.x", &["sealed-agent-meta"]),
            with_meta(
                type_def_with_parents("host", &[]),
                vec![meta_block(pick, vec![])],
            ),
        ])
        .graph;

        let diags = validate_meta_simple(&g);
        let fired = diags.iter().any(|d| d.code.as_str() == "sealed-parent-claimed");
        prop_assert!(
            fired,
            "expected sealed-parent-claimed for sealed meta-type '{}', got {:?}",
            pick,
            codes_of(&diags)
        );
    }
}

// ----- §8.2: walks don't recurse into encountered metas -----

#[test]
fn walk_does_not_recurse_into_encountered_meta_typedefs() {
    // §8.2 lock. `display-meta` itself carries `meta: [- type: runtime-meta]`.
    // Querying the host for `runtime-meta` must NOT return display-meta's
    // runtime-meta block — the walk targets the host's TYPE chain only,
    // not the chains of meta-type-defs it encounters.
    let g = build_graph(vec![
        with_meta(
            type_def_with_fields("display-meta", &[], &[]),
            vec![meta_block("runtime-meta", vec![])],
        ),
        type_def_with_fields("runtime-meta", &[], &[]),
        with_meta(
            type_def_with_parents("decision", &[]),
            vec![meta_block("display-meta", vec![])],
        ),
    ])
    .graph;

    // The host points at display-meta — but display-meta's own runtime-meta
    // is NOT reachable from the host's query.
    assert!(
        lookup_meta(&g, None, &tn("decision"), &tn("runtime-meta"), None).is_none(),
        "walk must not recurse into encountered meta-type-defs' own metas (§8.2)"
    );
    // Sanity: display-meta is still resolvable from the host (host declares
    // it directly).
    assert!(lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None).is_some());
}
