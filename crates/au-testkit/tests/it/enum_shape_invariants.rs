//! Property tests for inline closed enums (spec §4.2.2). Coverage:
//! membership, parser round-trip, token-equality, and key-order
//! determinism on enum-field instances.

use au_core::{build_graph, InstanceValue};
use au_grammar::{parse_shape, Shape};
use au_testkit::{
    arb_enum_literal, arb_enum_shape, instance_bare, instance_field, type_def_with_enum_field,
    validate_simple,
};
use proptest::prelude::*;

// ----- 1. Membership: any literal in the set passes; outside fails -----

proptest! {
    #[test]
    fn any_listed_literal_passes_validation(lits in arb_enum_shape()) {
        let lit_refs: Vec<&str> = lits.iter().map(String::as_str).collect();
        let g = build_graph(vec![type_def_with_enum_field(
            "rec", &[], "kind", &lit_refs,
        )])
        .graph;
        for lit in &lits {
            let inst = instance_bare(
                "rec",
                vec![instance_field("kind", InstanceValue::String(lit.clone()))],
            );
            let diags = validate_simple(&g, &inst);
            prop_assert!(
                diags.is_empty(),
                "expected '{}' to validate against {:?}, got {:?}",
                lit, lits, diags
            );
        }
    }

    #[test]
    fn value_outside_set_fails_with_field_shape_mismatch(
        lits in arb_enum_shape(),
        outside in arb_enum_literal(),
    ) {
        prop_assume!(!lits.contains(&outside));
        let lit_refs: Vec<&str> = lits.iter().map(String::as_str).collect();
        let g = build_graph(vec![type_def_with_enum_field(
            "rec", &[], "kind", &lit_refs,
        )])
        .graph;
        let inst = instance_bare(
            "rec",
            vec![instance_field("kind", InstanceValue::String(outside.clone()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes: Vec<&str> = diags.iter().map(|d| d.code.as_str()).collect();
        prop_assert_eq!(
            codes,
            vec!["field-shape-mismatch"],
            "expected exactly one field-shape-mismatch for '{}' against {:?}",
            outside, lits
        );
    }
}

// ----- 2. Parser round-trip: declared literals reach Shape::Enum verbatim -----

proptest! {
    #[test]
    fn parse_shape_round_trips_enum(lits in arb_enum_shape()) {
        let raw = format!("[{}]", lits.join(", "));
        let parsed = parse_shape(&raw).expect("well-formed enum should parse");
        prop_assert_eq!(parsed, Shape::Enum(lits));
    }
}

// ----- 3. Token-equality: same sequence ⇒ ==; reversed ⇒ != (spec §6.2) -----

proptest! {
    #[test]
    fn token_equality_is_reflexive(lits in arb_enum_shape()) {
        // Whitespace around commas is non-semantic — `[a,b]` and `[a, b]`
        // produce the same `Shape::Enum`.
        let dense = format!("[{}]", lits.join(","));
        let spaced = format!("[ {} ]", lits.join(" , "));
        let s1 = parse_shape(&dense).unwrap();
        let s2 = parse_shape(&spaced).unwrap();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn token_equality_is_order_sensitive(
        lits in arb_enum_shape().prop_filter(
            "need ≥ 2 elements with distinct first/last to observe inequality",
            |v| v.len() >= 2 && v.first() != v.last(),
        ),
    ) {
        let mut reversed = lits.clone();
        reversed.reverse();
        let forward = parse_shape(&format!("[{}]", lits.join(", "))).unwrap();
        let backward = parse_shape(&format!("[{}]", reversed.join(", "))).unwrap();
        prop_assert_ne!(forward, backward);
    }
}

// ----- 4. Determinism across permuted instance-field order -----

#[test]
fn enum_field_validation_invariant_under_field_permutation() {
    // Three enum-typed fields; a mix of valid + invalid values; all six
    // field-order permutations must produce the same diagnostic SET.
    let g = build_graph(vec![type_def_with_enum_field(
        "rec",
        &[],
        "kind",
        &["x", "y"],
    )])
    .graph;
    // Single field on this type-def, so we permute three fields where one is
    // the declared `kind` and the others are extras (pass silently).
    let base = vec![
        instance_field("kind", InstanceValue::String("not_a_member".into())),
        instance_field("extra1", InstanceValue::String("anything".into())),
        instance_field("extra2", InstanceValue::String("anything".into())),
    ];

    let perms: Vec<[usize; 3]> = vec![
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    let mut sorted_diags: Vec<Vec<String>> = Vec::new();
    for p in &perms {
        let fields: Vec<_> = p.iter().map(|i| base[*i].clone()).collect();
        let inst = instance_bare("rec", fields);
        let mut codes: Vec<String> = validate_simple(&g, &inst)
            .into_iter()
            .map(|d| format!("{}|{}", d.code.as_str(), d.message))
            .collect();
        codes.sort();
        sorted_diags.push(codes);
    }
    for d in &sorted_diags[1..] {
        assert_eq!(d, &sorted_diags[0]);
    }
}

// ----- 5. Smoke: builder produces a typedef whose enum field validates -----

#[test]
fn type_def_with_enum_field_builder_round_trips() {
    let g = build_graph(vec![type_def_with_enum_field(
        "task",
        &[],
        "priority",
        &["low", "moderate", "high"],
    )])
    .graph;
    let ok = instance_bare(
        "task",
        vec![instance_field(
            "priority",
            InstanceValue::String("moderate".into()),
        )],
    );
    let bad = instance_bare(
        "task",
        vec![instance_field(
            "priority",
            InstanceValue::String("urgent".into()),
        )],
    );
    assert!(validate_simple(&g, &ok).is_empty());
    let bad_diags = validate_simple(&g, &bad);
    let bad_codes: Vec<&str> = bad_diags.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(bad_codes, vec!["field-shape-mismatch"]);
}
