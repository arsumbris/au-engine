//! Property + scenario tests for the sealed-leaf rule (spec §3.3, §9.5).
//! Per-claim semantics: each element in an instance's `type:` list must
//! independently be a non-sealed type-def. A non-sealed sibling does not
//! excuse a sealed claim.

use au_core::{build_graph, InstanceValue, TypeDef, TypeName, TypeNameClaim};
use au_diagnostics::ByteRange;
use au_testkit::{
    instance_bare, instance_field, instance_list, type_def_with_fields, type_def_with_parents,
    validate_simple,
};
use proptest::prelude::*;

fn codes_of(diags: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

/// Compose a sealed branch list onto an existing type-def. Lets tests
/// re-use `type_def_with_fields` / `type_def_with_parents` and tack on a
/// `sealed:` clause. (au-testkit's `type_def_sealed` only builds bare
/// sealed type-defs; we need parent+field+sealed combos.)
fn with_sealed(mut td: TypeDef, branches: &[&str]) -> TypeDef {
    td.sealed = branches
        .iter()
        .map(|b| TypeNameClaim::own(TypeName((*b).into()), ByteRange::new(0, 0)))
        .collect();
    td
}

/// Build a graph mirroring the spec §3.3 nested-sum example:
///   decision (sealed)
///     ├── decision.pending
///     └── decision.decided (sealed)
///           ├── decision.decided.committed
///           └── decision.decided.reverted
///   maturity (non-sealed, has its own field)
fn nested_decision_graph() -> au_core::TypeGraph {
    build_graph(vec![
        with_sealed(
            type_def_with_fields("decision", &[], &["summary"]),
            &["decision.pending", "decision.decided"],
        ),
        type_def_with_parents("decision.pending", &["decision"]),
        with_sealed(
            type_def_with_parents("decision.decided", &["decision"]),
            &["decision.decided.committed", "decision.decided.reverted"],
        ),
        type_def_with_parents("decision.decided.committed", &["decision.decided"]),
        type_def_with_parents("decision.decided.reverted", &["decision.decided"]),
        type_def_with_fields("maturity", &[], &["level"]),
    ])
    .graph
}

// ----- 1. Sealed name claimed directly always fires -----

#[test]
fn bare_sealed_parent_always_fires() {
    let g = nested_decision_graph();
    let inst = instance_bare(
        "decision",
        vec![instance_field("summary", InstanceValue::String("x".into()))],
    );
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert!(
        codes.contains(&"sealed-parent-claimed"),
        "expected sealed-parent-claimed, got {:?}",
        codes
    );
}

#[test]
fn bare_sealed_intermediate_always_fires() {
    let g = nested_decision_graph();
    let inst = instance_bare(
        "decision.decided",
        vec![instance_field("summary", InstanceValue::String("x".into()))],
    );
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert!(
        codes.contains(&"sealed-parent-claimed"),
        "expected sealed-parent-claimed at sealed intermediate, got {:?}",
        codes
    );
}

// ----- 2. Non-sealed claim never fires this code -----

#[test]
fn non_sealed_leaf_never_fires_sealed_parent_claimed() {
    let g = nested_decision_graph();
    let inst = instance_bare(
        "decision.decided.committed",
        vec![instance_field("summary", InstanceValue::String("x".into()))],
    );
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert!(
        !codes.contains(&"sealed-parent-claimed"),
        "non-sealed claim must not fire sealed-parent-claimed; got {:?}",
        codes
    );
}

#[test]
fn non_sealed_pending_branch_never_fires_sealed_parent_claimed() {
    let g = nested_decision_graph();
    let inst = instance_bare(
        "decision.pending",
        vec![instance_field("summary", InstanceValue::String("x".into()))],
    );
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert!(
        !codes.contains(&"sealed-parent-claimed"),
        "non-sealed claim must not fire sealed-parent-claimed; got {:?}",
        codes
    );
}

// ----- 3. Non-sealed sibling does not excuse sealed claim -----

#[test]
fn mixin_with_sealed_parent_fires_for_sealed_only() {
    // type: [decision, maturity] — `decision` is sealed; `maturity` is
    // non-sealed. The whole point of the per-claim rule (spec §3.3): the
    // non-sealed sibling does NOT excuse the sealed claim.
    let g = nested_decision_graph();
    let inst = instance_list(
        &["decision", "maturity"],
        vec![
            instance_field("summary", InstanceValue::String("x".into())),
            instance_field("level", InstanceValue::String("y".into())),
        ],
    );
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert_eq!(
        codes
            .iter()
            .filter(|c| **c == "sealed-parent-claimed")
            .count(),
        1,
        "exactly one sealed-parent-claimed for `decision`; got {:?}",
        codes
    );
}

// ----- 4. Mixin commutativity -----

proptest! {
    /// `type: [a, b]` and `type: [b, a]` produce the same set of
    /// sealed-parent-claimed diagnostics. Locks in the per-claim
    /// ordering discipline.
    #[test]
    fn sealed_leaf_diagnostics_commute_under_claim_permutation(
        seed in 0u32..256,
    ) {
        let g = nested_decision_graph();
        let fields = vec![
            instance_field("summary", InstanceValue::String(format!("s{seed}"))),
            instance_field("level", InstanceValue::String(format!("l{seed}"))),
        ];
        let ab = validate_simple(&g, &instance_list(&["decision", "maturity"], fields.clone()));
        let ba = validate_simple(&g, &instance_list(&["maturity", "decision"], fields));

        // Compare the (code, name-of-sealed-claim-in-message) projection
        // — span byte ranges differ by definition under permutation.
        let project = |diags: &[au_diagnostics::Diagnostic]| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = diags
                .iter()
                .filter(|d| d.code.as_str() == "sealed-parent-claimed")
                .map(|d| (d.code.as_str().to_string(), d.message.clone()))
                .collect();
            v.sort();
            v
        };
        prop_assert_eq!(project(&ab), project(&ba));
    }
}

// ----- 5. The rule fires iff the bare claim is sealed -----

/// `sealed-parent-claimed` is a surface for `TypeGraph::is_sealed`:
/// claiming any name in the graph fires iff `is_sealed(name)` is true.
#[test]
fn rule_fires_iff_claim_is_sealed() {
    let names = [
        "decision",                   // sealed parent
        "decision.pending",           // non-sealed leaf
        "decision.decided",           // sealed intermediate
        "decision.decided.committed", // non-sealed leaf in nested sum
        "decision.decided.reverted",  // non-sealed leaf in nested sum
        "maturity",                   // non-sealed sibling
    ];
    let g = nested_decision_graph();
    for claim in names {
        let inst = instance_bare(
            claim,
            vec![
                instance_field("summary", InstanceValue::String("x".into())),
                instance_field("level", InstanceValue::String("y".into())),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let fired = diags
            .iter()
            .any(|d| d.code.as_str() == "sealed-parent-claimed");
        let expected = g.is_sealed(&TypeName(claim.into()));
        assert_eq!(
            fired, expected,
            "claim {} fired={} expected={} (is_sealed)",
            claim, fired, expected
        );
    }
}
