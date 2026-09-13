//! Property + scenario tests for compound slot expressions (spec §4.2.4,
//! §4.2.5, §4.7). Covers Display roundtrip through `parse_shape`,
//! branch-order significance, suffix non-commutativity, and subsumption
//! parity with `subsumption-in-mixin`.

use au_core::{build_graph, run_graph_structure_checks, TypeDef, TypeName, TypeNameClaim};
use au_diagnostics::ByteRange;
use au_grammar::{parse_shape, CompoundRefOp, Primitive, RefMode, Shape};
use au_testkit::{empty_type_def, type_def_with_parents};
use proptest::prelude::*;

fn run_struct(defs: Vec<TypeDef>) -> Vec<au_diagnostics::Diagnostic> {
    let g = build_graph(defs).graph;
    run_graph_structure_checks(&g)
}

/// Build a TypeDef whose single field has the given parsed shape.
fn td_with_field(name: &str, field: &str, shape: Shape) -> TypeDef {
    use au_core::{FieldDecl, FieldName};
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field.into()),
            optional: false,
            raw_shape: shape.to_string(),
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(shape),
            doc: None,
        }],
        ..type_def_with_parents(name, &[])
    }
}

// ----- 1. Display roundtrip -----
//
// For any constructible compound shape, `parse_shape(shape.to_string())`
// must produce the same Shape. Locks the source-form contract.

prop_compose! {
    /// Generate a compound shape with 2..=3 reference branches. Names are
    /// short lowercase identifiers from `arb_type_name`'s alphabet so they
    /// re-parse cleanly.
    fn arb_simple_union()
        (names in proptest::collection::vec("[a-z][a-z0-9]{0,4}", 2..=3))
        -> Shape
    {
        Shape::Union(names.into_iter().map(|n| Shape::Reference(n.into())).collect())
    }
}

prop_compose! {
    fn arb_simple_intersection()
        (names in proptest::collection::vec("[a-z][a-z0-9]{0,4}", 2..=3))
        -> Shape
    {
        Shape::Intersection(names.into_iter().map(|n| Shape::Reference(n.into())).collect())
    }
}

prop_compose! {
    fn arb_compound_reference()
        (mode in proptest::sample::select(&[RefMode::Star, RefMode::Inline][..]),
         op in proptest::sample::select(&[CompoundRefOp::Union, CompoundRefOp::Intersection][..]),
         names in proptest::collection::vec("[a-z][a-z0-9]{0,4}", 2..=3))
        -> Shape
    {
        Shape::CompoundReference {
            mode,
            op,
            branches: names.into_iter().map(Into::into).collect(),
        }
    }
}

proptest! {
    #[test]
    fn union_round_trips_through_parse_shape(shape in arb_simple_union()) {
        // Branches are bare names (`Shape::Reference`); Display renders
        // each as `name*`; parse_shape consumes that back to the same AST.
        let rendered = shape.to_string();
        let reparsed = parse_shape(&rendered).expect("Display output must parse");
        prop_assert_eq!(shape, reparsed, "round-trip failed for {}", rendered);
    }

    #[test]
    fn intersection_round_trips_through_parse_shape(shape in arb_simple_intersection()) {
        let rendered = shape.to_string();
        let reparsed = parse_shape(&rendered).expect("Display output must parse");
        prop_assert_eq!(shape, reparsed, "round-trip failed for {}", rendered);
    }

    #[test]
    fn compound_reference_round_trips_through_parse_shape(shape in arb_compound_reference()) {
        let rendered = shape.to_string();
        let reparsed = parse_shape(&rendered).expect("Display output must parse");
        prop_assert_eq!(shape, reparsed, "round-trip failed for {}", rendered);
    }
}

// ----- 2. Branch-order significance (spec §6.2 token equality is
// structural over the branch Vec) -----

#[test]
fn union_branch_order_distinguishes_shapes() {
    let ab = parse_shape("<a* | b*>").unwrap();
    let ba = parse_shape("<b* | a*>").unwrap();
    assert_ne!(ab, ba);
}

#[test]
fn intersection_branch_order_distinguishes_shapes() {
    let ab = parse_shape("<a* & b*>").unwrap();
    let ba = parse_shape("<b* & a*>").unwrap();
    assert_ne!(ab, ba);
}

#[test]
fn compound_reference_branch_order_distinguishes_shapes() {
    let ab = parse_shape("<a* | b*>*").unwrap();
    let ba = parse_shape("<b* | a*>*").unwrap();
    assert_ne!(ab, ba);
}

// ----- 3. Suffix non-commutativity -----
//
// `<a | b>*` is a single reference whose target satisfies a-or-b.
// `<a* | b*>` is a slot-union of two distinct references. They have
// different validation semantics; the AST must distinguish them.

#[test]
fn compound_reference_distinct_from_union_of_references() {
    let inner_compound_with_outer_star = Shape::CompoundReference {
        mode: RefMode::Star,
        op: CompoundRefOp::Union,
        branches: vec!["a".into(), "b".into()],
    };
    let union_of_two_refs = Shape::Union(vec![
        Shape::Reference("a".into()),
        Shape::Reference("b".into()),
    ]);
    assert_ne!(inner_compound_with_outer_star, union_of_two_refs);
}

// ----- 4. Subsumption fires iff a closure relation exists -----

/// Spec §3.3 nested-sum example: decision (sealed) → pending / decided
/// (sealed) → committed / reverted; maturity is a non-sealed sibling.
fn nested_decision_graph_defs() -> Vec<TypeDef> {
    fn with_sealed(mut td: TypeDef, branches: &[&str]) -> TypeDef {
        td.sealed = branches
            .iter()
            .map(|b| TypeNameClaim::own(TypeName((*b).into()), ByteRange::new(0, 0)))
            .collect();
        td
    }
    vec![
        with_sealed(
            empty_type_def("decision"),
            &["decision.pending", "decision.decided"],
        ),
        type_def_with_parents("decision.pending", &["decision"]),
        with_sealed(
            type_def_with_parents("decision.decided", &["decision"]),
            &["decision.decided.committed", "decision.decided.reverted"],
        ),
        type_def_with_parents("decision.decided.committed", &["decision.decided"]),
        type_def_with_parents("decision.decided.reverted", &["decision.decided"]),
        empty_type_def("maturity"),
    ]
}

proptest! {
    /// For any pair of names from the nested-decision graph, `subsumption-
    /// in-slot-union` fires for `<a* | b*>` iff `closure(a) ⊃ closure(b)`
    /// or vice versa. Mirrors the `fires_iff_is_sealed` discipline:
    /// the rule is a surface for the closure-of relation, nothing more.
    #[test]
    fn slot_union_subsumption_fires_iff_closure_relation(
        i in 0usize..6,
        j in 0usize..6,
    ) {
        prop_assume!(i != j);
        let names = [
            "decision",
            "decision.pending",
            "decision.decided",
            "decision.decided.committed",
            "decision.decided.reverted",
            "maturity",
        ];
        let mut defs = nested_decision_graph_defs();
        defs.push(td_with_field(
            "ev",
            "support",
            Shape::Union(vec![
                Shape::Reference(names[i].into()),
                Shape::Reference(names[j].into()),
            ]),
        ));
        let diags = run_struct(defs);
        let fired = diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union");

        // Compute expected from raw closure relations: subsumption holds
        // iff `names[i]` is an ancestor of `names[j]` or vice versa
        // (one's closure contains the other).
        let g = build_graph(nested_decision_graph_defs()).graph;
        let ci = au_core::closure_of(&g, &TypeName(names[i].into()));
        let cj = au_core::closure_of(&g, &TypeName(names[j].into()));
        let expected = ci.contains(&TypeName(names[j].into()))
            || cj.contains(&TypeName(names[i].into()));

        prop_assert_eq!(fired, expected, "names=({}, {})", names[i], names[j]);
    }
}

// ----- 5. Mixin / slot-union symmetry (spec §4.7 + §5) -----

#[test]
fn mixin_and_slot_union_subsumption_are_symmetric() {
    // For the same closure relationship between A and B, the mixin
    // claim `[A, B]` fires `subsumption-in-mixin` AND the slot
    // shape `<A* | B*>` fires `subsumption-in-slot-union`.
    // Spec §4.7 calls these symmetric — the two diagnostics are the
    // surface, the rule is the same.

    // mixin side: type-def `c` claims [decision, decision.decided] as parents
    let mixin_diags = {
        let mut defs = nested_decision_graph_defs();
        let mut child = type_def_with_parents("c", &["decision", "decision.decided"]);
        // Set parent_claim so the per-claim helper has a span to point at.
        child.parent_claim = Some(au_core::ParentClaim {
            value_span: ByteRange::new(0, 0),
            form: au_core::ParentClaimForm::List,
        });
        defs.push(child);
        let g = build_graph(defs).graph;
        au_core::run_inheritance_checks(&g)
    };
    let mixin_fired = mixin_diags
        .iter()
        .any(|d| d.code.as_str() == "subsumption-in-mixin");

    // slot-union side: a field shape `<decision* | decision.decided*>`
    let union_diags = {
        let mut defs = nested_decision_graph_defs();
        defs.push(td_with_field(
            "ev",
            "support",
            Shape::Union(vec![
                Shape::Reference("decision".into()),
                Shape::Reference("decision.decided".into()),
            ]),
        ));
        run_struct(defs)
    };
    let union_fired = union_diags
        .iter()
        .any(|d| d.code.as_str() == "subsumption-in-slot-union");

    assert_eq!(
        mixin_fired, union_fired,
        "mixin and slot-union must agree on the same closure relation"
    );
    assert!(
        mixin_fired,
        "the example pair has a real subsumption relation"
    );
}

// ----- 6. Primitive / enum branches in a Union are silent for subsumption -----

#[test]
fn primitive_branch_in_union_does_not_break_other_branch_subsumption() {
    // `<String | decision* | decision.decided*>` — String has no closure
    // to compare, but the (decision, decision.decided) pair still fires.
    let mut defs = nested_decision_graph_defs();
    defs.push(td_with_field(
        "ev",
        "v",
        Shape::Union(vec![
            Shape::Primitive(Primitive::String),
            Shape::Reference("decision".into()),
            Shape::Reference("decision.decided".into()),
        ]),
    ));
    let diags = run_struct(defs);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"),
        "primitive branch should not suppress reference-pair subsumption"
    );
}
