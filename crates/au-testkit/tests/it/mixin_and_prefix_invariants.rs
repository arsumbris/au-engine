//! Property + scenario tests for the mixin and field-qualifier surface.
//! Covers commutativity, auto-unify equivalence, non-identical collision,
//! per-originator required-field partial-fill, mixed-bare-and-qualified-
//! field detection, and the redundant-claim warnings (`duplicate-claim`,
//! `subsumption-in-mixin`).
//!
//! Auto-unify-shape invariants use `type_def_with_fields` (`String`
//! everywhere); diverging-shape invariants use `type_def_with_enum_field`
//! to vary literals without leaving the existing builder set.

use au_core::{build_graph, FieldDecl, FieldName, InstanceValue, TypeDef};
use au_diagnostics::ByteRange;
use au_grammar::{Primitive, Shape};
use au_testkit::{
    instance_bare, instance_field, instance_list, type_def_with_enum_field, type_def_with_fields,
    type_def_with_parents, validate_simple,
};
use proptest::prelude::*;

fn codes_of(diags: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

/// A type-def with one optional `String` field — useful when we want to
/// isolate a single behavior without required-field-absent firing.
fn td_with_optional_string(name: &str, parents: &[&str], field: &str) -> TypeDef {
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field.into()),
            optional: true,
            raw_shape: "String".into(),
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(Shape::Primitive(Primitive::String)),
            doc: None,
        }],
        ..type_def_with_parents(name, parents)
    }
}

// ----- 1. Mixin commutativity -----

proptest! {
    #[test]
    fn mixin_commutativity_under_claim_permutation(
        seed in 0u32..1024,
    ) {
        // Two non-overlapping types — both required; instance provides
        // both bare. The diagnostic vector must be identical regardless
        // of claim order.
        let g = build_graph(vec![
            type_def_with_fields("a", &[], &["fa"]),
            type_def_with_fields("b", &[], &["fb"]),
        ])
        .graph;
        let fields = vec![
            instance_field("fa", InstanceValue::String(format!("a{seed}"))),
            instance_field("fb", InstanceValue::String(format!("b{seed}"))),
        ];
        let ab = validate_simple(&g, &instance_list(&["a", "b"], fields.clone()));
        let ba = validate_simple(&g, &instance_list(&["b", "a"], fields));
        let ab_codes: Vec<&str> = codes_of(&ab);
        let ba_codes: Vec<&str> = codes_of(&ba);
        prop_assert_eq!(ab_codes, ba_codes);
    }
}

// ----- 2. Auto-unify: 2-claim with token-equal ≡ 1-claim with field -----

#[test]
fn auto_unify_equivalence_two_claims_vs_single() {
    // Both `note` and `deliverable` declare `description: String`
    // (token-equal). Either:
    //  - claim both via mixin, provide bare description.
    //  - claim only `note` (single-claim), provide bare description.
    // Both should validate clean. The instance contract is the same
    // because auto-unify collapses the descriptions to one slot.
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("deliverable", &[], &["description"]),
    ])
    .graph;
    let fields = || {
        vec![instance_field(
            "description",
            InstanceValue::String("x".into()),
        )]
    };
    assert!(validate_simple(&g, &instance_bare("note", fields())).is_empty());
    assert!(validate_simple(&g, &instance_list(&["note", "deliverable"], fields())).is_empty());
}

// ----- 3. Divergent required field: required per origin; a bare use also collides -----

#[test]
fn divergent_required_field_enforces_per_origin_and_bare_collides() {
    // Two enum shapes that differ on literals — auto-unify cannot unify them,
    // so `priority` is divergent AND required at both origins.
    let g = build_graph(vec![
        type_def_with_enum_field("a", &[], "priority", &["low", "high"]),
        type_def_with_enum_field("b", &[], "priority", &["low", "moderate", "high"]),
    ])
    .graph;

    // Untouched → required is required: both unfilled origins fire, no collision.
    let untouched = validate_simple(&g, &instance_list(&["a", "b"], vec![]));
    let uc = codes_of(&untouched);
    assert_eq!(
        uc.iter().filter(|c| **c == "required-field-absent").count(),
        2,
        "both required origins must fire when untouched: {uc:?}"
    );
    assert!(
        !uc.contains(&"mixin-collision"),
        "no bare use, no collision: {uc:?}"
    );

    // Bare use → mixin-collision PLUS required (a bare value fills no origin).
    let bare = validate_simple(
        &g,
        &instance_list(
            &["a", "b"],
            vec![instance_field(
                "priority",
                InstanceValue::String("low".into()),
            )],
        ),
    );
    let bc = codes_of(&bare);
    assert!(bc.contains(&"mixin-collision"), "got {bc:?}");
    assert_eq!(
        bc.iter().filter(|c| **c == "required-field-absent").count(),
        2,
        "got {bc:?}"
    );
}

// ----- 4. Prefix vs bare equivalence (single originator + token-equal) -----

#[test]
fn qualified_and_bare_validate_identically_for_single_originator() {
    // Single originator (`note` declares description). Bare and qualified
    // forms should produce equivalent diagnostic streams when both
    // satisfy the slot.
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_parents("decision", &["note"]),
        type_def_with_parents("decision.decided", &["decision"]),
    ])
    .graph;
    let bare = validate_simple(
        &g,
        &instance_bare(
            "decision.decided",
            vec![instance_field(
                "description",
                InstanceValue::String("x".into()),
            )],
        ),
    );
    let qualified = validate_simple(
        &g,
        &instance_bare(
            "decision.decided",
            vec![instance_field(
                "description{note}",
                InstanceValue::String("x".into()),
            )],
        ),
    );
    assert!(bare.is_empty());
    assert!(qualified.is_empty());
}

// ----- 5. Per-originator partial-fill -----

#[test]
fn per_originator_partial_fill_fires_for_unfilled_origin_only() {
    // Two distinct originators of `description: String` (auto-unify).
    // Only `description{note}` provided — `deliverable`'s required slot
    // stays empty. required-field-absent fires for deliverable; not note.
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("deliverable", &[], &["description"]),
    ])
    .graph;
    let inst = instance_list(
        &["note", "deliverable"],
        vec![instance_field(
            "description{note}",
            InstanceValue::String("x".into()),
        )],
    );
    let diags = validate_simple(&g, &inst);
    let absent: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "required-field-absent")
        .collect();
    assert_eq!(absent.len(), 1);
    assert!(absent[0].message.contains("'deliverable'"));
    assert!(!absent[0].message.contains("'note'"));
}

#[test]
fn bare_value_satisfies_all_origins_in_one_shot() {
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("deliverable", &[], &["description"]),
    ])
    .graph;
    let inst = instance_list(
        &["note", "deliverable"],
        vec![instance_field(
            "description",
            InstanceValue::String("x".into()),
        )],
    );
    assert!(validate_simple(&g, &inst).is_empty());
}

// ----- 6. Mixed-bare-and-qualified determinism -----

proptest! {
    #[test]
    fn mixed_bare_and_prefixed_fires_once_regardless_of_order(
        prefixed_first in any::<bool>(),
    ) {
        let g = build_graph(vec![
            type_def_with_fields("note", &[], &["description"]),
        ])
        .graph;
        let bare_field = instance_field(
            "description",
            InstanceValue::String("bare".into()),
        );
        let prefixed_field = instance_field(
            "description{note}",
            InstanceValue::String("prefixed".into()),
        );
        let fields = if prefixed_first {
            vec![prefixed_field, bare_field]
        } else {
            vec![bare_field, prefixed_field]
        };
        let diags = validate_simple(&g, &instance_bare("note", fields));
        let mixed = diags
            .iter()
            .filter(|d| d.code.as_str() == "mixed-bare-and-qualified-field")
            .count();
        prop_assert_eq!(mixed, 1);
    }
}

// ----- 7. Valid prefixes don't false-fire prefix-error codes -----

#[test]
fn valid_prefix_does_not_fire_prefix_errors() {
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_parents("decision", &["note"]),
    ])
    .graph;
    // `description{note}` and `description{decision}` are both valid
    // qualifiers for the description field on a `decision` instance.
    for key in ["description{note}", "description{decision}"] {
        let inst = instance_bare(
            "decision",
            vec![instance_field(key, InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            !codes.contains(&"qualifier-not-in-closure"),
            "false positive qualifier-not-in-closure for key {key}: {codes:?}"
        );
        assert!(
            !codes.contains(&"qualifier-does-not-declare-field"),
            "false positive qualifier-does-not-declare-field for key {key}: {codes:?}"
        );
        assert!(
            !codes.contains(&"malformed-qualifier-key"),
            "false positive malformed-qualifier-key for key {key}: {codes:?}"
        );
    }
}

// ----- 8. Redundant-claim warnings -----

#[test]
fn duplicate_claim_warning_always_fires_and_does_not_block() {
    // Single optional field so required-field-absent doesn't show up.
    let g = build_graph(vec![td_with_optional_string("note", &[], "description")]).graph;
    let inst = instance_list(&["note", "note"], vec![]);
    let diags = validate_simple(&g, &inst);
    let dups: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "duplicate-claim")
        .collect();
    assert_eq!(dups.len(), 1);
    assert_eq!(
        dups[0].severity,
        au_diagnostics::Severity::Warning,
        "expected duplicate-claim to be a warning, not an error"
    );
    // No errors — the warning shouldn't block validation.
    assert!(diags
        .iter()
        .all(|d| d.severity != au_diagnostics::Severity::Error));
}

#[test]
fn subsumption_warning_always_fires_for_subtype_pairs() {
    let g = build_graph(vec![
        td_with_optional_string("note", &[], "description"),
        type_def_with_parents("decision", &["note"]),
    ])
    .graph;
    let inst = instance_list(&["note", "decision"], vec![]);
    let diags = validate_simple(&g, &inst);
    let subs: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "subsumption-in-mixin")
        .collect();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].severity, au_diagnostics::Severity::Warning);
    // Names both: the wider redundant claim and the narrower implier.
    assert!(subs[0].message.contains("'note'"));
    assert!(subs[0].message.contains("'decision'"));
}
