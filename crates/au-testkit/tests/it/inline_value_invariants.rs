//! Property + scenario tests for inline values (spec §4.4, §4.5, §4.6).
//! Covers the four §4.5 disambiguation cases, nested inline recursion,
//! and the §4.6 wikilink-pattern routing in primitive-vs-reference
//! unions.

use au_core::{
    build_graph, FieldDecl, FieldName, InlineValue, InstanceField, InstanceValue, TypeClaim,
    TypeDef, TypeGraph, TypeName, TypeNameClaim,
};
use au_diagnostics::ByteRange;
use au_grammar::{Primitive, Shape};
use au_testkit::{
    instance_bare, instance_field, instance_list, type_def_with_fields, type_def_with_parents,
    validate_simple,
};
use proptest::prelude::*;
use std::path::PathBuf;

fn codes_of(diags: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

/// One required primitive field on a type-def. Mirrors
/// `type_def_with_fields` but lets the caller pick the shape directly.
fn td_one_field(name: &str, parents: &[&str], field: &str, shape: Shape) -> TypeDef {
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field.into()),
            optional: false,
            raw_shape: "".into(),
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(shape),
            doc: None,
        }],
        ..type_def_with_parents(name, parents)
    }
}

fn bare(name: &str) -> TypeClaim {
    TypeClaim::Bare(TypeNameClaim::own(
        TypeName(name.into()),
        ByteRange::new(0, 0),
    ))
}

fn list(names: &[&str]) -> TypeClaim {
    TypeClaim::List {
        items: names
            .iter()
            .map(|n| TypeNameClaim::own(TypeName((*n).into()), ByteRange::new(0, 0)))
            .collect(),
        value_span: ByteRange::new(0, 0),
    }
}

fn inline_field(key: &str, claim: Option<TypeClaim>, fields: Vec<InstanceField>) -> InstanceField {
    instance_field(
        key,
        InstanceValue::Mapping(InlineValue {
            type_claim: claim,
            block_id: None,
            fields,
            doc: None,
            field_docs: Default::default(),
        }),
    )
}

// ----- §4.5 case 1 — redundancy: declared exact = omitted -----

#[test]
fn case_1_redundant_declared_type_matches_omitted() {
    // For a non-sealed record slot, an inline value with declared
    // `type:` exactly matching the slot's demand produces the same
    // diagnostics as one with `type:` omitted.
    let g = build_graph(vec![
        type_def_with_fields("rationale", &[], &["description"]),
        td_one_field("host", &[], "body", Shape::Record("rationale".into())),
    ])
    .graph;
    let omitted = instance_bare(
        "host",
        vec![inline_field(
            "body",
            None,
            vec![instance_field(
                "description",
                InstanceValue::String("ok".into()),
            )],
        )],
    );
    let declared = instance_bare(
        "host",
        vec![inline_field(
            "body",
            Some(bare("rationale")),
            vec![instance_field(
                "description",
                InstanceValue::String("ok".into()),
            )],
        )],
    );
    assert_eq!(
        codes_of(&validate_simple(&g, &omitted)),
        codes_of(&validate_simple(&g, &declared))
    );
}

// ----- §4.5 case 2 — sealed-leaf rule mirrors file-level -----

fn sealed_decision_graph_with_record_slot() -> TypeGraph {
    build_graph(vec![
        {
            let mut td = type_def_with_fields("decision", &[], &["summary"]);
            td.sealed = vec![
                TypeNameClaim::own(TypeName("decision.pending".into()), ByteRange::new(0, 0)),
                TypeNameClaim::own(TypeName("decision.decided".into()), ByteRange::new(0, 0)),
            ];
            td
        },
        type_def_with_parents("decision.pending", &["decision"]),
        type_def_with_parents("decision.decided", &["decision"]),
        td_one_field("host", &[], "choice", Shape::Record("decision".into())),
    ])
    .graph
}

#[test]
fn sealed_inline_rule_fires_on_same_names_as_file_level() {
    // For each name in the graph, claiming it inline at a sealed slot
    // fires `sealed-parent-claimed` iff a file-level claim of the same
    // name fires it.
    let g = sealed_decision_graph_with_record_slot();
    for name in ["decision", "decision.pending", "decision.decided"] {
        let inline_inst = instance_bare(
            "host",
            vec![inline_field(
                "choice",
                Some(bare(name)),
                vec![instance_field("summary", InstanceValue::String("x".into()))],
            )],
        );
        let file_inst = instance_bare(
            name,
            vec![instance_field("summary", InstanceValue::String("x".into()))],
        );
        let inline_fired =
            codes_of(&validate_simple(&g, &inline_inst)).contains(&"sealed-parent-claimed");
        let file_fired =
            codes_of(&validate_simple(&g, &file_inst)).contains(&"sealed-parent-claimed");
        assert_eq!(
            inline_fired, file_fired,
            "sealed-parent-claimed should mirror file-level for '{name}': inline={inline_fired}, file={file_fired}"
        );
    }
}

// ----- §4.5 case 4 — mixin equivalence inline ↔ file -----

#[test]
fn intersection_inline_mixin_validates_iff_file_mixin_validates() {
    // For an intersection slot `<A & B>` with declared `type: [a, b]`:
    // the inline value validates iff a file-level instance of `[a, b]`
    // would (mod span byte ranges).
    let g = build_graph(vec![
        type_def_with_fields("rationale", &[], &["description"]),
        type_def_with_fields("thesis", &[], &["claim"]),
        td_one_field(
            "host",
            &[],
            "body",
            Shape::Intersection(vec![
                Shape::Record("rationale".into()),
                Shape::Record("thesis".into()),
            ]),
        ),
    ])
    .graph;

    let inline_inst = instance_bare(
        "host",
        vec![inline_field(
            "body",
            Some(list(&["rationale", "thesis"])),
            vec![
                instance_field("description", InstanceValue::String("d".into())),
                instance_field("claim", InstanceValue::String("c".into())),
            ],
        )],
    );
    // File-level mixin instance of the same two types.
    let file_inst = instance_list(
        &["rationale", "thesis"],
        vec![
            instance_field("description", InstanceValue::String("d".into())),
            instance_field("claim", InstanceValue::String("c".into())),
        ],
    );
    let inline_diags = validate_simple(&g, &inline_inst);
    let file_diags = validate_simple(&g, &file_inst);
    let inline_codes = codes_of(&inline_diags);
    let file_codes = codes_of(&file_diags);
    assert_eq!(
        inline_codes, file_codes,
        "inline mixin and file-level mixin should validate equivalently"
    );
    assert!(
        inline_codes.is_empty(),
        "happy-path mixin should produce no diagnostics; got {:?}",
        inline_codes
    );
}

// ----- §4.6 — dispatch is value-shape-only, branch-order-invariant -----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any `<String | T*>` union (in either branch order), a value
    /// matching the wikilink pattern routes to the reference branch.
    /// Lock: dispatch depends solely on value shape, not branch order.
    #[test]
    fn wikilink_dispatch_is_branch_order_invariant(
        string_first in proptest::bool::ANY,
    ) {
        let g = build_graph(vec![
            type_def_with_fields("rationale", &[], &[]),
            td_one_field(
                "free",
                &[],
                "v",
                if string_first {
                    Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                    ])
                } else {
                    Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Primitive(Primitive::String),
                    ])
                },
            ),
        ])
        .graph;
        // Wikilink to a missing target — both orders surface
        // `reference-target-missing` (the precheck commits to the
        // reference path regardless of branch order).
        let inst = instance_bare(
            "free",
            vec![instance_field(
                "v",
                InstanceValue::String("[[missing]]".into()),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        prop_assert_eq!(codes, vec!["reference-target-missing"]);
    }
}

// ----- nested-inline equivalence -----

#[test]
fn nested_inline_validates_when_each_level_validates() {
    // Outer record-slot demand `rationale`; rationale has an
    // `evidence: evidence-item` field; evidence-item has `source:
    // String`. Two-deep inline: host { body: { evidence: {...} } }.
    let g = build_graph(vec![
        type_def_with_fields("evidence-item", &[], &["source"]),
        td_one_field(
            "rationale",
            &[],
            "evidence",
            Shape::Record("evidence-item".into()),
        ),
        td_one_field("host", &[], "body", Shape::Record("rationale".into())),
    ])
    .graph;
    let happy = instance_bare(
        "host",
        vec![inline_field(
            "body",
            None,
            vec![inline_field(
                "evidence",
                None,
                vec![instance_field("source", InstanceValue::String("s".into()))],
            )],
        )],
    );
    assert!(validate_simple(&g, &happy).is_empty());

    // Inner level missing required field → diagnostic at inner level,
    // outer level otherwise valid.
    let bad = instance_bare(
        "host",
        vec![inline_field(
            "body",
            None,
            vec![inline_field("evidence", None, vec![])],
        )],
    );
    assert_eq!(
        codes_of(&validate_simple(&g, &bad)),
        vec!["required-field-absent"]
    );
}

// ----- inline-or-reference (`&`) accepts both forms -----

#[test]
fn inline_or_reference_slot_accepts_both_forms() {
    // `rationale&` slot — inline map and wikilink string both validate
    // (when targets resolve correctly). Locks the dual-dispatch
    // contract.
    let g = build_graph(vec![
        type_def_with_fields("rationale", &[], &[]),
        td_one_field(
            "host",
            &[],
            "body",
            Shape::InlineOrReference("rationale".into()),
        ),
    ])
    .graph;
    let (idx, _) =
        au_references::RepoIndex::build(PathBuf::from("/v"), vec![PathBuf::from("/v/rat.md")]);
    let mut claims_by_path = std::collections::BTreeMap::new();
    claims_by_path.insert(
        PathBuf::from("/v/rat.md"),
        vec![TypeName("rationale".into())],
    );
    let inline_inst = instance_bare(
        "host",
        vec![inline_field("body", Some(bare("rationale")), vec![])],
    );
    let wikilink_inst = instance_bare(
        "host",
        vec![instance_field(
            "body",
            InstanceValue::String("[[rat]]".into()),
        )],
    );
    let body_sources = std::collections::BTreeMap::new();
    let record_targets = std::collections::BTreeMap::new();
    let ref_data = au_core::MapRefData {
        claims_by_path: &claims_by_path,
        body_sources: &body_sources,
        record_targets: &record_targets,
    };
    let ctx = au_core::ValidateContext {
        graph: &g,
        repo_index: &idx,
        ref_data: &ref_data,
        cross_repo: None,
        resolution: None,
        meta_marker: None,
    };
    assert!(au_core::validate(&ctx, &inline_inst).is_empty());
    assert!(au_core::validate(&ctx, &wikilink_inst).is_empty());
}
