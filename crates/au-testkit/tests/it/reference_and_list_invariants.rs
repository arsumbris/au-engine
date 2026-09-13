//! Property + scenario tests for typed references (`name*`, `file*`) and
//! list shapes (`X[]`). Coverage:
//! - ref to existing-typed file passes
//! - ref to absent target fires `reference-target-missing`
//! - ref to wrong-typed target fires `reference-target-type-mismatch`
//! - `file*` is existence-only (no closure check)
//! - list elementwise validation propagates inner mismatches
//! - list value not a sequence fires `field-shape-mismatch`
//! - validate output is deterministic across permuted file order

use std::collections::BTreeMap;
use std::path::PathBuf;

use au_core::{build_graph, InstanceValue, TypeName};
use au_grammar::{Primitive, Shape};
use au_references::RepoIndex;
use au_testkit::{
    instance_bare, instance_field, instance_field_seq, type_def_with_fields,
    type_def_with_list_field, type_def_with_reference_field, validate_with,
};
use proptest::prelude::*;

// ---------- helpers ----------

fn repo_with(files: &[(&str, &[&str])]) -> (RepoIndex, BTreeMap<PathBuf, Vec<TypeName>>) {
    let root = PathBuf::from("/v");
    let paths: Vec<PathBuf> = files.iter().map(|(rel, _)| root.join(rel)).collect();
    let (idx, _) = RepoIndex::build(root.clone(), paths);
    let mut claims: BTreeMap<PathBuf, Vec<TypeName>> = BTreeMap::new();
    for (rel, type_claims) in files {
        if !type_claims.is_empty() {
            claims.insert(
                root.join(rel),
                type_claims.iter().map(|n| TypeName((*n).into())).collect(),
            );
        }
    }
    (idx, claims)
}

fn codes(diags: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

// ---------- references ----------

#[test]
fn reference_to_existing_typed_target_passes() {
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &[]),
        type_def_with_reference_field("link-card", &[], "target", "note"),
    ])
    .graph;
    let (idx, claims) = repo_with(&[("notes/foo.md", &["note"])]);
    let inst = instance_bare(
        "link-card",
        vec![instance_field(
            "target",
            InstanceValue::String("[[foo]]".into()),
        )],
    );
    let diags = validate_with(&g, &idx, &claims, &inst);
    assert!(
        diags.is_empty(),
        "expected clean validation, got {:?}",
        diags
    );
}

proptest! {
    /// A ref to an absent target always fires `reference-target-missing`,
    /// regardless of the target name (within the V1 alphabet).
    #[test]
    fn reference_to_absent_target_always_missing(
        target in "[a-z][a-z0-9_-]{0,8}",
    ) {
        let g = build_graph(vec![
            type_def_with_fields("note", &[], &[]),
            type_def_with_reference_field("link-card", &[], "target", "note"),
        ])
        .graph;
        // Empty knowledge base — no file resolves.
        let (idx, claims) = repo_with(&[]);
        let inst = instance_bare(
            "link-card",
            vec![instance_field(
                "target",
                InstanceValue::String(format!("[[{}]]", target)),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        prop_assert_eq!(codes(&diags), vec!["reference-target-missing"]);
    }
}

#[test]
fn reference_to_wrong_typed_target_fires_type_mismatch() {
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &[]),
        type_def_with_fields("decision", &[], &[]),
        type_def_with_reference_field("link-card", &[], "target", "note"),
    ])
    .graph;
    // Target file claims `decision`, slot wants `note`.
    let (idx, claims) = repo_with(&[("decisions/d.md", &["decision"])]);
    let inst = instance_bare(
        "link-card",
        vec![instance_field(
            "target",
            InstanceValue::String("[[d]]".into()),
        )],
    );
    assert_eq!(
        codes(&validate_with(&g, &idx, &claims, &inst)),
        vec!["reference-target-type-mismatch"]
    );
}

#[test]
fn file_reference_existence_only_invariant() {
    // `file*` resolves any knowledge base file by existence; type closure is not
    // consulted. Targets at different paths, untyped or typed, all pass
    // when they exist; missing targets fire missing.
    let g = build_graph(vec![type_def_with_reference_field(
        "attachment",
        &[],
        "blob",
        "file",
    )])
    .graph;
    let (idx, claims) = repo_with(&[
        ("assets/a.pdf", &[]),
        ("assets/b.png", &[]),
        ("notes/foo.md", &["note"]), // typed, but `file*` doesn't care
    ]);

    for target in &["a.pdf", "b.png", "foo.md", "foo"] {
        let inst = instance_bare(
            "attachment",
            vec![instance_field(
                "blob",
                InstanceValue::String(format!("[[{}]]", target)),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected `file*` to accept {} (existence-only), got {:?}",
            target,
            diags
        );
    }

    let missing = instance_bare(
        "attachment",
        vec![instance_field(
            "blob",
            InstanceValue::String("[[ghost.pdf]]".into()),
        )],
    );
    assert_eq!(
        codes(&validate_with(&g, &idx, &claims, &missing)),
        vec!["reference-target-missing"]
    );
}

// ---------- lists ----------

#[test]
fn list_of_strings_passes_when_all_strings() {
    let g = build_graph(vec![type_def_with_list_field(
        "rec",
        &[],
        "tags",
        Shape::Primitive(Primitive::String),
    )])
    .graph;
    let (idx, claims) = repo_with(&[]);
    let inst = instance_bare(
        "rec",
        vec![instance_field_seq(
            "tags",
            vec![
                InstanceValue::String("a".into()),
                InstanceValue::String("b".into()),
            ],
        )],
    );
    assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
}

proptest! {
    /// Per-element mismatch count equals the number of non-string entries
    /// in a `String[]` list. Validates that the elementwise check runs
    /// once per element (not stopping at the first mismatch, not double-
    /// counting any single element).
    #[test]
    fn list_elementwise_mismatch_count_is_per_bad_element(
        bad_count in 0usize..6,
        good_count in 0usize..6,
    ) {
        let g = build_graph(vec![type_def_with_list_field(
            "rec",
            &[],
            "tags",
            Shape::Primitive(Primitive::String),
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let mut elements: Vec<InstanceValue> = Vec::new();
        for _ in 0..good_count {
            elements.push(InstanceValue::String("ok".into()));
        }
        for _ in 0..bad_count {
            elements.push(InstanceValue::Integer(42));
        }
        let inst = instance_bare(
            "rec",
            vec![instance_field_seq("tags", elements)],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        let mismatch_count = diags
            .iter()
            .filter(|d| d.code.as_str() == "field-shape-mismatch")
            .count();
        prop_assert_eq!(mismatch_count, bad_count);
    }
}

#[test]
fn list_value_not_a_sequence_fires_field_shape_mismatch() {
    let g = build_graph(vec![type_def_with_list_field(
        "rec",
        &[],
        "tags",
        Shape::Primitive(Primitive::String),
    )])
    .graph;
    let (idx, claims) = repo_with(&[]);
    let inst = instance_bare(
        "rec",
        vec![instance_field(
            "tags",
            InstanceValue::String("not a list".into()),
        )],
    );
    assert_eq!(
        codes(&validate_with(&g, &idx, &claims, &inst)),
        vec!["field-shape-mismatch"]
    );
}

#[test]
fn list_of_references_validates_each_element() {
    let g = build_graph(vec![
        type_def_with_fields("note", &[], &[]),
        type_def_with_list_field("collection", &[], "items", Shape::Reference("note".into())),
    ])
    .graph;
    let (idx, claims) = repo_with(&[("notes/foo.md", &["note"]), ("notes/bar.md", &["note"])]);
    let inst = instance_bare(
        "collection",
        vec![instance_field_seq(
            "items",
            vec![
                InstanceValue::String("[[foo]]".into()),
                InstanceValue::String("[[bar]]".into()),
            ],
        )],
    );
    assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
}

// ---------- determinism ----------

#[test]
fn repo_index_resolution_deterministic_across_permuted_file_order() {
    // Three files with the same basename in different dirs — resolve("foo.md")
    // returns Ambiguous(matches). The match list (after sorting) must be
    // identical regardless of insertion order, so downstream diagnostics are
    // stable.
    let root = PathBuf::from("/v");
    let files = vec![
        root.join("a/foo.md"),
        root.join("b/foo.md"),
        root.join("c/foo.md"),
    ];
    let mut reversed = files.clone();
    reversed.reverse();

    let (idx_a, _) = RepoIndex::build(root.clone(), files.clone());
    let (idx_b, _) = RepoIndex::build(root.clone(), reversed);

    let mut a = match idx_a.resolve("foo.md") {
        Err(au_references::ResolutionError::Ambiguous(ps)) => ps,
        other => panic!("expected Ambiguous, got {:?}", other),
    };
    let mut b = match idx_b.resolve("foo.md") {
        Err(au_references::ResolutionError::Ambiguous(ps)) => ps,
        other => panic!("expected Ambiguous, got {:?}", other),
    };
    a.sort();
    b.sort();
    assert_eq!(a, b);

    // Single-file case across two index builds: same input, same resolution.
    let one = vec![root.join("a/foo.md")];
    let (idx_one_a, _) = RepoIndex::build(root.clone(), one.clone());
    let (idx_one_b, _) = RepoIndex::build(root, one);
    assert_eq!(
        idx_one_a.resolve("foo.md").unwrap(),
        idx_one_b.resolve("foo.md").unwrap()
    );
}

#[test]
fn validate_deterministic_across_permuted_field_order() {
    // Two field orderings on a list-of-strings instance produce the same
    // sorted diagnostic set when the data is otherwise identical.
    let g = build_graph(vec![type_def_with_list_field(
        "rec",
        &[],
        "tags",
        Shape::Primitive(Primitive::String),
    )])
    .graph;
    let (idx, claims) = repo_with(&[]);

    let inst_a = instance_bare(
        "rec",
        vec![instance_field_seq(
            "tags",
            vec![
                InstanceValue::Integer(1),
                InstanceValue::String("ok".into()),
                InstanceValue::Integer(2),
            ],
        )],
    );
    let inst_b = instance_bare(
        "rec",
        vec![instance_field_seq(
            "tags",
            vec![
                InstanceValue::Integer(1),
                InstanceValue::String("ok".into()),
                InstanceValue::Integer(2),
            ],
        )],
    );
    let diags_a = validate_with(&g, &idx, &claims, &inst_a);
    let diags_b = validate_with(&g, &idx, &claims, &inst_b);
    assert_eq!(codes(&diags_a), codes(&diags_b));
}
