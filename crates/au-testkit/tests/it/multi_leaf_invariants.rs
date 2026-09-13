//! Property + scenario tests for the multi-leaf-in-sealed-family rule
//! (spec §5, §9.5). A sealed family is a discriminated union — an
//! instance's deduped `type:` claim must contain at most one non-sealed
//! leaf per sealed family. Fires at file-level and inline-value identity
//! claims with the same semantics; this file covers the file-level path
//! (au-testkit's `validate_simple` is file-level).

use au_core::{build_graph, TypeDef, TypeName, TypeNameClaim};
use au_diagnostics::ByteRange;
use au_testkit::{
    instance_bare, instance_list, type_def_with_fields, type_def_with_parents, validate_simple,
};

fn codes_of(diags: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

fn with_sealed(mut td: TypeDef, branches: &[&str]) -> TypeDef {
    td.sealed = branches
        .iter()
        .map(|b| TypeNameClaim::own(TypeName((*b).into()), ByteRange::new(0, 0)))
        .collect();
    td
}

/// Mirror the spec §3.3 nested-sum decision family plus a disjoint
/// sealed `source` family and a non-sealed `maturity` sibling. Used
/// across this file to exercise nested-sums, disjoint-families, and
/// sealed-vs-non-sealed cases.
fn families_graph() -> au_core::TypeGraph {
    build_graph(vec![
        with_sealed(
            type_def_with_fields("decision", &[], &[]),
            &["decision.pending", "decision.decided"],
        ),
        type_def_with_parents("decision.pending", &["decision"]),
        with_sealed(
            type_def_with_parents("decision.decided", &["decision"]),
            &["decision.decided.committed", "decision.decided.reverted"],
        ),
        type_def_with_parents("decision.decided.committed", &["decision.decided"]),
        type_def_with_parents("decision.decided.reverted", &["decision.decided"]),
        with_sealed(
            type_def_with_fields("source", &[], &[]),
            &["source.url", "source.path"],
        ),
        type_def_with_parents("source.url", &["source"]),
        type_def_with_parents("source.path", &["source"]),
        type_def_with_fields("maturity", &[], &[]),
    ])
    .graph
}

// ----- 1. Two siblings of outer sealed fire at the outer family -----

#[test]
fn outer_sealed_siblings_fire() {
    // [decision.pending, decision.decided.committed] — both descend from
    // sealed `decision`. Fire once at `decision`.
    let g = families_graph();
    let inst = instance_list(&["decision.pending", "decision.decided.committed"], vec![]);
    let diags = validate_simple(&g, &inst);
    let multi: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
        .collect();
    assert_eq!(multi.len(), 1, "got {:?}", codes_of(&diags));
    assert!(multi[0].message.contains("'decision'"));
}

// ----- 2. Inner sealed wins via leaf-set-equality suppression -----

#[test]
fn inner_sealed_siblings_fire_only_at_inner() {
    // [committed, reverted] — both bucket under `decision.decided` AND
    // `decision`. Identical leaf sets → suppress outer.
    let g = families_graph();
    let inst = instance_list(
        &["decision.decided.committed", "decision.decided.reverted"],
        vec![],
    );
    let diags = validate_simple(&g, &inst);
    let multi: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
        .collect();
    assert_eq!(multi.len(), 1, "got {:?}", codes_of(&diags));
    assert!(multi[0].message.contains("'decision.decided'"));
}

// ----- 3. Strict-superset → both outer and inner fire -----

#[test]
fn three_leaves_fire_at_both_levels() {
    // [pending, committed, reverted] — `decision` bucket has 3 leaves,
    // `decision.decided` has 2. Sets differ; suppression doesn't apply.
    let g = families_graph();
    let inst = instance_list(
        &[
            "decision.pending",
            "decision.decided.committed",
            "decision.decided.reverted",
        ],
        vec![],
    );
    let diags = validate_simple(&g, &inst);
    let multi: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
        .collect();
    assert_eq!(multi.len(), 2, "got {:?}", codes_of(&diags));
}

// ----- 4. Disjoint sealed families fire independently -----

#[test]
fn disjoint_sealed_families_fire_independently() {
    let g = families_graph();
    let inst = instance_list(
        &[
            "decision.pending",
            "source.url",
            "decision.decided.committed",
            "source.path",
        ],
        vec![],
    );
    let diags = validate_simple(&g, &inst);
    let multi: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
        .collect();
    assert_eq!(multi.len(), 2, "got {:?}", codes_of(&diags));
    let mut named: Vec<&str> = multi
        .iter()
        .map(|d| {
            if d.message.contains("family 'source'") {
                "source"
            } else {
                "decision"
            }
        })
        .collect();
    named.sort();
    assert_eq!(named, vec!["decision", "source"]);
}

// ----- 5. Negative cases — no diagnostic -----

#[test]
fn leaf_with_non_sealed_sibling_does_not_fire() {
    // [decision.pending, maturity] — only one sealed-family leaf.
    let g = families_graph();
    let inst = instance_list(&["decision.pending", "maturity"], vec![]);
    let diags = validate_simple(&g, &inst);
    assert!(
        !codes_of(&diags).contains(&"multi-leaf-in-sealed-family"),
        "got {:?}",
        codes_of(&diags)
    );
}

#[test]
fn single_leaf_claim_does_not_fire() {
    let g = families_graph();
    let inst = instance_bare("decision.decided.committed", vec![]);
    let diags = validate_simple(&g, &inst);
    assert!(
        !codes_of(&diags).contains(&"multi-leaf-in-sealed-family"),
        "got {:?}",
        codes_of(&diags)
    );
}

#[test]
fn sealed_claim_with_one_leaf_does_not_fire_multi_leaf() {
    // [decision, decision.decided.committed] — `decision` is sealed
    // (sealed-parent-claimed handles it); the multi-leaf rule sees
    // only one non-sealed leaf in the claim.
    let g = families_graph();
    let inst = instance_list(&["decision", "decision.decided.committed"], vec![]);
    let diags = validate_simple(&g, &inst);
    let codes = codes_of(&diags);
    assert!(
        codes.contains(&"sealed-parent-claimed"),
        "sealed-parent-claimed should fire: {:?}",
        codes
    );
    assert!(
        !codes.contains(&"multi-leaf-in-sealed-family"),
        "multi-leaf should not fire: {:?}",
        codes
    );
}

#[test]
fn subsumption_pair_does_not_fire_multi_leaf() {
    // [decision.decided, decision.decided.committed] — `decision.decided`
    // is sealed (covered by sealed-parent-claimed); only `committed`
    // counts as a non-sealed leaf. Multi-leaf does not fire.
    let g = families_graph();
    let inst = instance_list(&["decision.decided", "decision.decided.committed"], vec![]);
    let diags = validate_simple(&g, &inst);
    assert!(
        !codes_of(&diags).contains(&"multi-leaf-in-sealed-family"),
        "got {:?}",
        codes_of(&diags)
    );
}

// ----- 6. Mixin commutativity -----

/// `type: [a, b]` and `type: [b, a]` produce the same set of
/// multi-leaf-in-sealed-family diagnostics. Locks in claim-order
/// independence; mirrors the sealed-leaf commutativity invariant.
#[test]
fn multi_leaf_diagnostics_commute_under_claim_permutation() {
    let g = families_graph();
    let ab = validate_simple(
        &g,
        &instance_list(&["decision.pending", "decision.decided.committed"], vec![]),
    );
    let ba = validate_simple(
        &g,
        &instance_list(&["decision.decided.committed", "decision.pending"], vec![]),
    );

    // Compare on (code, message) directly. The diagnostic helper builds
    // the message from a `BTreeSet<TypeName>` of leaves, so the leaf
    // list inside the message is alphabetically sorted and
    // claim-order-independent. (code, message) is therefore an invariant
    // of the rule under permutation, and any future change to message
    // wording surfaces as a loud test failure rather than silent
    // substring-extraction drift.
    let project = |diags: &[au_diagnostics::Diagnostic]| -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .map(|d| (d.code.as_str().to_string(), d.message.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(project(&ab), project(&ba));
}

// ----- 7. Rule semantics — fires-iff-has-multi-leaf-sealed-bucket -----

/// For each candidate claim list in a fixed catalog, the rule fires iff
/// the claim list has ≥2 distinct non-sealed leaves sharing a sealed
/// ancestor. Cross-checks the implementation against an explicit
/// expectation table.
#[test]
fn rule_fires_iff_multi_leaf_in_sealed_family() {
    let g = families_graph();
    // (claim list, expected_multi_leaf_count)
    let catalog: &[(&[&str], usize)] = &[
        (&["decision.pending"], 0),
        (&["decision.decided.committed"], 0),
        (&["maturity"], 0),
        (&["decision.pending", "maturity"], 0),
        // Two leaves of outer family → 1 (outer bucket only).
        (&["decision.pending", "decision.decided.committed"], 1),
        // Two leaves of inner family → 1 (inner suppresses outer).
        (
            &["decision.decided.committed", "decision.decided.reverted"],
            1,
        ),
        // Three leaves spanning inner + outer → 2 (sets differ).
        (
            &[
                "decision.pending",
                "decision.decided.committed",
                "decision.decided.reverted",
            ],
            2,
        ),
        // Two disjoint families → 2 (independent).
        (
            &[
                "decision.pending",
                "decision.decided.committed",
                "source.url",
                "source.path",
            ],
            2,
        ),
    ];
    for (claim, expected) in catalog {
        let inst = if claim.len() == 1 {
            instance_bare(claim[0], vec![])
        } else {
            instance_list(claim, vec![])
        };
        let diags = validate_simple(&g, &inst);
        let count = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .count();
        assert_eq!(
            count, *expected,
            "claim {:?} fired {} multi-leaf diagnostics; expected {}",
            claim, count, expected
        );
    }
}
