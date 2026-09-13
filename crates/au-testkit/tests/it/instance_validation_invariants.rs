//! Property tests for the instance validator: required-field detection,
//! unknown-claim detection, determinism across field-order permutation, and
//! purity (idempotence). These are invariants the validator must hold
//! regardless of input details — proptest randomizes names where applicable
//! and explicit permutation tests cover ordering.

use au_core::{build_graph, FieldName, InstanceValue};
use au_testkit::{
    arb_field_name, arb_type_name, instance_bare, instance_field, type_def_with_fields,
    validate_simple,
};
use proptest::prelude::*;

// ----- 1. Removing a required field always triggers required-field-absent -----

proptest! {
    #[test]
    fn missing_required_field_always_diagnosed(
        field_name in arb_field_name(),
    ) {
        let g = build_graph(vec![type_def_with_fields(
            "rec",
            &[],
            &[field_name.as_str()],
        )])
        .graph;
        // Instance has no fields — the required field is absent.
        let inst = instance_bare("rec", vec![]);
        let diags = validate_simple(&g, &inst);
        let hit = diags
            .iter()
            .any(|d| d.code.as_str() == "required-field-absent");
        prop_assert!(hit, "expected required-field-absent for {:?}", field_name);
    }
}

// ----- 2. Unknown type-claim always triggers unknown-type-claim -----

proptest! {
    #[test]
    fn unknown_type_claim_always_diagnosed(name in arb_type_name()) {
        // Empty graph: any claim is unknown.
        let g = build_graph(vec![]).graph;
        let inst = instance_bare(name.as_str(), vec![]);
        let diags = validate_simple(&g, &inst);
        let count = diags
            .iter()
            .filter(|d| d.code.as_str() == "unknown-type-claim")
            .count();
        // The validator returns immediately on UnknownType, so exactly one.
        prop_assert_eq!(count, 1);
    }
}

// ----- 3. Determinism across field-order permutation -----

#[test]
fn validate_output_invariant_under_field_permutation() {
    // The validator iterates EffectiveShape (BTreeMap → sorted) for required
    // checks and the instance's field Vec for per-field checks. The set of
    // diagnostics shouldn't change when we shuffle the instance's field
    // ordering — only the per-field iteration order changes, and the sort in
    // the CLI layer would normalize anyway. The validator itself is what we
    // exercise here.
    let g = build_graph(vec![type_def_with_fields("rec", &[], &["a", "b", "c"])]).graph;

    let base = vec![
        instance_field("a", InstanceValue::String("x".into())),
        instance_field("b", InstanceValue::String("y".into())),
        instance_field("c", InstanceValue::String("z".into())),
    ];

    // All 6 permutations of three fields.
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
        // Compare on the (code, message) pair sorted — the validator's own
        // ordering may shift with input order, but the SET of diagnostics
        // must be identical. Downstream sorters (CLI compute) normalize
        // ordering for snapshot stability.
        let mut codes: Vec<String> = validate_simple(&g, &inst)
            .into_iter()
            .map(|d| format!("{}|{}", d.code.as_str(), d.message))
            .collect();
        codes.sort();
        sorted_diags.push(codes);
    }

    // All permutations produce identical (sorted) diagnostic sets.
    for d in &sorted_diags[1..] {
        assert_eq!(d, &sorted_diags[0]);
    }
}

// ----- 4. Idempotence (purity) -----

#[test]
fn validate_is_idempotent_on_its_own_input() {
    let g = build_graph(vec![type_def_with_fields("rec", &[], &["a", "b"])]).graph;
    let inst = instance_bare(
        "rec",
        vec![
            instance_field("a", InstanceValue::String("x".into())),
            // Missing `b` — exercises required-field-absent so there's
            // actually output to compare.
            instance_field("extra", InstanceValue::String("z".into())),
        ],
    );

    let first = validate_simple(&g, &inst);
    let second = validate_simple(&g, &inst);
    let third = validate_simple(&g, &inst);

    // Pure function — no global state, no I/O. Three calls, three identical
    // outputs.
    assert_eq!(first, second);
    assert_eq!(second, third);
}

// ----- 5. Sanity: builders compose into something the validator handles -----

#[test]
fn builders_produce_validatable_instances() {
    // Smoke test that the testkit builders themselves don't drift from the
    // au-core API surface.
    let g = build_graph(vec![type_def_with_fields("note", &[], &["description"])]).graph;
    let inst = instance_bare(
        "note",
        vec![instance_field(
            "description",
            InstanceValue::String("hi".into()),
        )],
    );
    assert!(validate_simple(&g, &inst).is_empty());
    // Field name is the testkit's, not synthesized.
    let _typed: FieldName = FieldName("description".into());
}
