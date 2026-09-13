//! Property tests for graph-load invariants: determinism, cycle-detection
//! termination, sealed reachability, redeclare detection.

use au_core::{build_graph, run_graph_structure_checks, run_inheritance_checks, TypeDef};
use au_testkit::{
    cycle_of_size, empty_type_def, self_cycle, type_def_sealed, type_def_with_fields,
    type_def_with_parents,
};
use proptest::prelude::*;

// ----- 1. Graph-load determinism -----
//
// Building the same Vec<TypeDef> twice produces graphs that iterate in the
// same order and report the same diagnostics.

proptest! {
    #[test]
    fn graph_load_is_deterministic(
        names in proptest::collection::vec("[a-z]{1,5}", 1..10)
    ) {
        // Dedupe to avoid duplicate-type-def diagnostics dominating the run.
        let mut deduped: Vec<String> = names.into_iter().collect();
        deduped.sort();
        deduped.dedup();

        let defs_a: Vec<TypeDef> = deduped.iter().map(|n| empty_type_def(n)).collect();
        let defs_b: Vec<TypeDef> = deduped.iter().map(|n| empty_type_def(n)).collect();

        let g_a = build_graph(defs_a).graph;
        let g_b = build_graph(defs_b).graph;

        let names_a: Vec<&str> = g_a.names().map(|n| n.as_str()).collect();
        let names_b: Vec<&str> = g_b.names().map(|n| n.as_str()).collect();
        prop_assert_eq!(names_a, names_b);
    }
}

// ----- 2. Cycle detection terminates on adversarial inputs -----
//
// Even pathological cycle shapes must not panic, infinite-loop, or stack
// overflow. The budget is "returns within reasonable wall time."

#[test]
fn cycle_detection_terminates_on_self_loop() {
    let g = build_graph(self_cycle("a")).graph;
    let diags = run_graph_structure_checks(&g);
    assert!(diags
        .iter()
        .any(|d| d.code.as_str() == "cycle-in-type-chain"));
}

#[test]
fn cycle_detection_terminates_on_large_cycle() {
    let g = build_graph(cycle_of_size(200)).graph;
    let diags = run_graph_structure_checks(&g);
    assert!(diags
        .iter()
        .any(|d| d.code.as_str() == "cycle-in-type-chain"));
}

proptest! {
    #[test]
    fn cycle_detection_terminates_on_random_adjacency(
        edges in proptest::collection::vec(
            (0u8..10u8, 0u8..10u8),
            0..40
        )
    ) {
        // Build a graph where each edge is parent → child. This produces
        // arbitrary directed graphs; cycles, self-loops, and disconnected
        // components are all fair game.
        let mut by_name: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (a, b) in edges {
            let child = format!("t{a}");
            let parent = format!("t{b}");
            by_name.entry(child).or_default().push(parent);
        }
        let defs: Vec<TypeDef> = by_name
            .iter()
            .map(|(child, parents)| {
                let parent_strs: Vec<&str> = parents.iter().map(|s| s.as_str()).collect();
                type_def_with_parents(child, &parent_strs)
            })
            .collect();
        let g = build_graph(defs).graph;
        // The very property is that this returns at all.
        let _ = run_graph_structure_checks(&g);
    }
}

// ----- 3. Sealed-reachability invariant -----
//
// If T's `type:` chain reaches sealed parent A, T must be transitively under
// one of A's listed branches. Otherwise sealed-no-surprise-children fires.

#[test]
fn sealed_listed_descendant_passes() {
    let defs = vec![
        type_def_sealed("source", &["source.url", "source.path"]),
        type_def_with_parents("source.url", &["source"]),
        type_def_with_parents("source.path", &["source"]),
    ];
    let g = build_graph(defs).graph;
    let diags = run_inheritance_checks(&g);
    assert!(!diags
        .iter()
        .any(|d| d.code.as_str() == "sealed-no-surprise-children"));
}

#[test]
fn sealed_unlisted_descendant_is_flagged() {
    let defs = vec![
        type_def_sealed("source", &["source.url", "source.path"]),
        type_def_with_parents("source.malformed", &["source"]),
    ];
    let g = build_graph(defs).graph;
    let diags = run_inheritance_checks(&g);
    assert!(diags
        .iter()
        .any(|d| d.code.as_str() == "sealed-no-surprise-children"));
}

#[test]
fn sealed_transitively_listed_descendant_passes() {
    // source sealed: [source.url, source.path]
    // source.url has its own subtype source.url.canonical — that's reachable
    // through the listed `source.url` branch.
    let defs = vec![
        type_def_sealed("source", &["source.url", "source.path"]),
        type_def_with_parents("source.url", &["source"]),
        type_def_with_parents("source.url.canonical", &["source.url"]),
    ];
    let g = build_graph(defs).graph;
    let diags = run_inheritance_checks(&g);
    assert!(!diags
        .iter()
        .any(|d| d.code.as_str() == "sealed-no-surprise-children"));
}

// ----- 4. Redeclare detection invariant -----
//
// If a subtype names a field an ancestor already carries (transitively),
// field-redeclaration fires. Otherwise it doesn't.

#[test]
fn direct_redeclare_is_flagged() {
    let defs = vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("decision", &["note"], &["description"]),
    ];
    let g = build_graph(defs).graph;
    let diags = run_inheritance_checks(&g);
    assert!(diags
        .iter()
        .any(|d| d.code.as_str() == "field-redeclaration"));
}

#[test]
fn distinct_fields_pass() {
    let defs = vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("decision", &["note"], &["status"]),
    ];
    let g = build_graph(defs).graph;
    let diags = run_inheritance_checks(&g);
    assert!(!diags
        .iter()
        .any(|d| d.code.as_str() == "field-redeclaration"));
}

proptest! {
    /// For any subtype claiming a field name that exists in any of its
    /// ancestors' field lists, the redeclare diagnostic must fire.
    #[test]
    fn redeclare_invariant_holds_under_random_chains(
        depth in 2usize..6,
        clash_field in "[a-z]{3,8}"
    ) {
        // Build a linear chain: t0 → t1 → ... → t(depth-1).
        // Place the clashing field at t0 and at t(depth-1). Expect the diag.
        let names: Vec<String> = (0..depth).map(|i| format!("t{i}")).collect();
        let mut defs = vec![type_def_with_fields(&names[0], &[], &[clash_field.as_str()])];
        for i in 1..(depth - 1) {
            defs.push(type_def_with_parents(&names[i], &[names[i - 1].as_str()]));
        }
        // Last type re-declares the same field → redeclare must fire.
        let last = depth - 1;
        defs.push(type_def_with_fields(
            &names[last],
            &[names[last - 1].as_str()],
            &[clash_field.as_str()],
        ));

        let g = build_graph(defs).graph;
        let diags = run_inheritance_checks(&g);
        let hit = diags
            .iter()
            .any(|d| d.code.as_str() == "field-redeclaration");
        prop_assert!(hit);
    }
}
