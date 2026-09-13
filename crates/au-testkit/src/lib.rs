//! Property generators and named adversarial fixtures for the type system.
//!
//! Generators (type names, field names) and convenience builders for
//! `TypeDef`s. Property tests themselves live in `tests/`; this lib is a
//! toolkit, not a test runner.

use std::collections::BTreeMap;
use std::path::PathBuf;

use au_core::{
    validate, FieldDecl, FieldName, Instance, InstanceField, InstanceValue, MapRefData,
    ParentClaim, ParentClaimForm, SequenceElement, TypeClaim, TypeDef, TypeGraph, TypeName,
    TypeNameClaim, ValidateContext,
};
use au_diagnostics::{ByteRange, Diagnostic};
use au_grammar::{DefBound, Shape};
use au_references::RepoIndex;
use proptest::prelude::*;

pub mod opcat;
pub mod repogen;

// ----- validation helpers -----

/// Validate `instance` against `graph` with an empty `RepoIndex` and
/// empty `claims_by_path`. Suitable for primitive / enum / list-of-primitive
/// invariants that don't exercise reference resolution. Reference-typed
/// fields will fire `reference-target-missing` against this — use
/// [`validate_with`] for those.
pub fn validate_simple(graph: &TypeGraph, instance: &Instance) -> Vec<Diagnostic> {
    let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
    let claims = BTreeMap::new();
    validate_with(graph, &idx, &claims, instance)
}

/// Build a `ValidateContext` from the three borrowed pieces and call
/// `validate`. Reads close to the (now-private) 4-arg form: convenient
/// for tests that already produce all three components.
pub fn validate_with(
    graph: &TypeGraph,
    idx: &RepoIndex,
    claims: &BTreeMap<PathBuf, Vec<TypeName>>,
    instance: &Instance,
) -> Vec<Diagnostic> {
    let body_sources = BTreeMap::new();
    let record_targets = BTreeMap::new();
    let ref_data = MapRefData {
        claims_by_path: claims,
        body_sources: &body_sources,
        record_targets: &record_targets,
    };
    let ctx = ValidateContext {
        graph,
        repo_index: idx,
        ref_data: &ref_data,
        cross_repo: None,
        // Own-graph validation only; no cross-repo resolution graph.
        resolution: None,
        // No marker: meta-legality needs the resolution graph, which own-graph
        // validation does not build.
        meta_marker: None,
    };
    validate(&ctx, instance)
}

// ----- name generators -----

/// Strategy producing type names that match the spec regex.
pub fn arb_type_name() -> impl Strategy<Value = TypeName> {
    "[a-z][a-z0-9_-]{0,6}(\\.[a-z][a-z0-9_-]{0,6}){0,2}".prop_map(TypeName)
}

/// Strategy producing field names (no dots, no special chars).
pub fn arb_field_name() -> impl Strategy<Value = FieldName> {
    "[a-z][a-z0-9_]{0,8}".prop_map(FieldName)
}

/// Strategy producing enum literals from the V1 alphabet (lowercase subset of
/// `[A-Za-z0-9_.-]+`, no leading digit). Length 1..=8.
pub fn arb_enum_literal() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,7}".prop_map(String::from)
}

/// Strategy producing 1..=5 unique enum literals in declaration order.
/// Token-equality (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]) is order-sensitive — the dedupe ensures
/// reordered sequences are actually distinct so the equality invariant
/// has something to assert against.
pub fn arb_enum_shape() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(arb_enum_literal(), 1..=5).prop_map(|v| {
        let mut seen = std::collections::BTreeSet::new();
        v.into_iter().filter(|s| seen.insert(s.clone())).collect()
    })
}

// ----- builders -----

/// Construct a type-def with no parents, no fields, no sealed list, no meta.
pub fn empty_type_def(name: &str) -> TypeDef {
    TypeDef {
        name: TypeName(name.into()),
        source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
        source_span: ByteRange::new(0, 0),
        parent_claim: None,
        parents: vec![],
        fields: vec![],
        shape: None,
        sealed: vec![],
        declared_abstract: false,
        meta_blocks: None,
        required_meta: Vec::new(),
        body: None,
        doc: None,
        ..Default::default()
    }
}

/// Construct a type-def with a parent list.
pub fn type_def_with_parents(name: &str, parents: &[&str]) -> TypeDef {
    TypeDef {
        parent_claim: if parents.is_empty() {
            None
        } else {
            Some(ParentClaim {
                form: ParentClaimForm::List,
                value_span: ByteRange::new(0, 0),
            })
        },
        parents: parents
            .iter()
            .map(|p| TypeNameClaim::own(TypeName((*p).into()), ByteRange::new(0, 0)))
            .collect(),
        ..empty_type_def(name)
    }
}

/// Construct a type-def with parents and a field list. All fields default to
/// `String` shape (parsed) — sufficient for graph-load invariants that only
/// care about field name presence/absence.
pub fn type_def_with_fields(name: &str, parents: &[&str], fields: &[&str]) -> TypeDef {
    TypeDef {
        fields: fields
            .iter()
            .map(|n| FieldDecl {
                name: FieldName((*n).into()),
                optional: false,
                raw_shape: "String".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
                doc: None,
            })
            .collect(),
        ..type_def_with_parents(name, parents)
    }
}

/// Construct a type-def with a single required enum-shaped field. The
/// `parsed_shape` is `Shape::Enum(literals)` and the `raw_shape` is the
/// canonical `[a, b, c]` rendering — useful for invariants that exercise
/// the enum match arm in the validator.
pub fn type_def_with_enum_field(
    name: &str,
    parents: &[&str],
    field_name: &str,
    literals: &[&str],
) -> TypeDef {
    let lits: Vec<String> = literals.iter().map(|s| (*s).to_string()).collect();
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field_name.into()),
            optional: false,
            raw_shape: format!("[{}]", literals.join(", ")),
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(au_grammar::Shape::Enum(lits)),
            doc: None,
        }],
        ..type_def_with_parents(name, parents)
    }
}

/// Construct a type-def with a single required typed-reference field
/// (`field_name: target_type*`).
pub fn type_def_with_reference_field(
    name: &str,
    parents: &[&str],
    field_name: &str,
    target_type: &str,
) -> TypeDef {
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field_name.into()),
            optional: false,
            raw_shape: format!("{}*", target_type),
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(Shape::Reference(target_type.into())),
            doc: None,
        }],
        ..type_def_with_parents(name, parents)
    }
}

/// Construct a type-def with a single required list field whose inner
/// shape is the caller-supplied `Shape`. Useful for elementwise
/// validation invariants and `String[]` / `name*[]` round-trips.
pub fn type_def_with_list_field(
    name: &str,
    parents: &[&str],
    field_name: &str,
    inner: Shape,
) -> TypeDef {
    let raw = render_shape(&inner) + "[]";
    TypeDef {
        fields: vec![FieldDecl {
            name: FieldName(field_name.into()),
            optional: false,
            raw_shape: raw,
            name_span: ByteRange::new(0, 0),
            shape_span: ByteRange::new(0, 0),
            entry_span: ByteRange::new(0, 0),
            parsed_shape: Ok(Shape::List {
                inner: Box::new(inner),
                min: 0,
                max: None,
            }),
            doc: None,
        }],
        ..type_def_with_parents(name, parents)
    }
}

fn render_shape(shape: &Shape) -> String {
    match shape {
        Shape::Primitive(p) => p.as_str().to_string(),
        Shape::Any => "any".to_string(),
        Shape::Opaque => "opaque".to_string(),
        Shape::Enum(literals) => format!("[{}]", literals.join(", ")),
        Shape::Reference(name) => format!("{}*", name),
        Shape::Record(name) => name.to_string(),
        Shape::InlineOrReference(name) => format!("{}&", name),
        Shape::List { inner, min, max } => {
            format!(
                "{}{}",
                render_shape(inner),
                au_grammar::list_suffix_string(*min, *max)
            )
        }
        // A refined scalar renders via its `Display`, which this mirrors.
        Shape::Refined { .. } => shape.to_string(),
        Shape::Union(branches) => render_compound(branches, '|'),
        Shape::Intersection(branches) => render_compound(branches, '&'),
        Shape::CompoundReference { mode, op, branches } => {
            // Compound branches are flat names; render each with `*`
            // to match `Shape::Display` (which also emits the
            // `*`-per-branch form for snapshot stability). Both
            // `<a | b>*` and `<a* | b*>*` parse to the same AST since
            // bare records landed.
            let starred: Vec<String> = branches.iter().map(|n| format!("{}*", n)).collect();
            let inner = starred.join(&format!(" {} ", op.separator_char()));
            format!("<{}>{}", inner, mode.suffix_char())
        }
        // [[type-def shape def-ref::au-type-system]], `type*` / `type<T>*`. Mirrors `Shape::Display`.
        Shape::DefReference(bound) => match bound {
            None => "type*".to_string(),
            Some(DefBound::Single(name)) => format!("type<{}>*", name),
            Some(DefBound::Compound { op, branches }) => {
                let names: Vec<String> = branches.iter().map(|n| n.to_string()).collect();
                let inner = names.join(&format!(" {} ", op.separator_char()));
                format!("type<{}>*", inner)
            }
        },
        // [[type-def shape suffixes::au-type-system]], the `*@` commit-pin postfix. Mirrors `Shape::Display`.
        Shape::Pinned(inner) => format!("{}@", render_shape(inner)),
        // A tuple renders via its `Display`, which this mirrors.
        Shape::Tuple(_) => shape.to_string(),
    }
}

fn render_compound(branches: &[Shape], op: char) -> String {
    let inner = branches
        .iter()
        .map(render_shape)
        .collect::<Vec<_>>()
        .join(&format!(" {} ", op));
    format!("<{}>", inner)
}

/// Construct a type-def with a sealed branch list.
pub fn type_def_sealed(name: &str, branches: &[&str]) -> TypeDef {
    TypeDef {
        sealed: branches
            .iter()
            .map(|s| TypeNameClaim::own(TypeName((*s).into()), ByteRange::new(0, 0)))
            .collect(),
        ..empty_type_def(name)
    }
}

// ----- adversarial fixtures -----

/// Generate a ring of `n` type-defs: `t_0 → t_{n-1} → t_{n-2} → … → t_1 → t_0`.
/// A single cycle of length `n`, used to exercise cycle-detection termination.
///
/// # Panics
/// `n` must be at least 2 (a single-element ring is `self_cycle`).
pub fn cycle_of_size(n: usize) -> Vec<TypeDef> {
    assert!(
        n >= 2,
        "cycle_of_size requires n >= 2; use self_cycle for n == 1"
    );
    let names: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
    let mut defs = Vec::with_capacity(n);
    for i in 0..n {
        let parent = if i == 0 { &names[n - 1] } else { &names[i - 1] };
        defs.push(type_def_with_parents(&names[i], &[parent.as_str()]));
    }
    defs
}

/// Self-loop: a single type-def whose `type:` parent claim names itself.
pub fn self_cycle(name: &str) -> Vec<TypeDef> {
    vec![type_def_with_parents(name, &[name])]
}

// ----- instance builders -----

/// Construct an `Instance` with a bare-scalar `type:` claim and the given
/// fields. Spans are zero — the validator's invariants don't depend on
/// span values, only on identity and value classification.
/// Re-stamp every field value and list element with a DISTINCT byte span, the way
/// a real parsed file does.
///
/// The instance-surface value model is keyed by byte span, so all-zero synthetic
/// spans collide in the span→node map and the model lookup misses. A reference /
/// brand value verdict then has nowhere to read its node (the re-parse fallback
/// was deleted with the value-model migration), so a valid `[[target]]` would
/// misvalidate. Distinct spans make a synthetic instance validate through the
/// model exactly like a real file, see
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
fn stamp_distinct_spans(fields: &mut [InstanceField]) {
    let mut next = 1usize;
    for f in fields.iter_mut() {
        f.value_span = ByteRange::new(next, next + 1);
        next += 2;
        if let InstanceValue::Sequence(elems) = &mut f.value {
            for e in elems.iter_mut() {
                e.span = ByteRange::new(next, next + 1);
                next += 2;
            }
        }
    }
}

pub fn instance_bare(claim: &str, mut fields: Vec<InstanceField>) -> Instance {
    stamp_distinct_spans(&mut fields);
    Instance {
        source_path: PathBuf::from(format!("/v/{claim}-instance.md")),
        source_span: ByteRange::new(0, 0),
        type_claim: TypeClaim::Bare(TypeNameClaim::own(
            TypeName(claim.into()),
            ByteRange::new(0, 0),
        )),
        fields,
        doc: None,
        field_docs: Default::default(),
    }
}

/// Construct an `Instance` with a `type: [a, b, ...]` list claim. Useful for
/// covering deferred-mixin and 1-element-list paths.
pub fn instance_list(claims: &[&str], mut fields: Vec<InstanceField>) -> Instance {
    stamp_distinct_spans(&mut fields);
    Instance {
        source_path: PathBuf::from("/v/listed-instance.md"),
        source_span: ByteRange::new(0, 0),
        type_claim: TypeClaim::List {
            items: claims
                .iter()
                .map(|c| TypeNameClaim::own(TypeName((*c).into()), ByteRange::new(0, 0)))
                .collect(),
            value_span: ByteRange::new(0, 0),
        },
        fields,
        doc: None,
        field_docs: Default::default(),
    }
}

/// Construct an `InstanceField` with the given key and value. Spans zeroed.
pub fn instance_field(key: &str, value: InstanceValue) -> InstanceField {
    InstanceField {
        key: key.into(),
        key_span: ByteRange::new(0, 0),
        value,
        value_span: ByteRange::new(0, 0),
        nav_links: Vec::new(),
    }
}

/// Construct an `InstanceField` whose value is a YAML sequence wrapping
/// the supplied scalar values. Element spans are zero — invariants
/// usually only care about value identity, not span positions.
pub fn instance_field_seq(key: &str, elements: Vec<InstanceValue>) -> InstanceField {
    let seq = InstanceValue::Sequence(
        elements
            .into_iter()
            .map(|v| SequenceElement {
                value: v,
                span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            })
            .collect(),
    );
    instance_field(key, seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_core::is_valid_type_name;

    proptest! {
        #[test]
        fn arb_type_name_satisfies_regex(name in arb_type_name()) {
            prop_assert!(is_valid_type_name(name.as_str()));
        }

        #[test]
        fn arb_field_name_is_alphanumeric_underscore(name in arb_field_name()) {
            let s = name.as_str();
            prop_assert!(!s.is_empty());
            prop_assert!(s.chars().next().unwrap().is_ascii_lowercase());
            for c in s.chars() {
                prop_assert!(c.is_ascii_alphanumeric() || c == '_');
            }
        }
    }

    #[test]
    fn cycle_of_size_actually_cycles() {
        let c = cycle_of_size(5);
        // Last node's parent is the second-to-last (index n-2).
        // First node's parent is the last (index n-1).
        assert_eq!(c[0].parents[0].name.as_str(), "t4");
        assert_eq!(c[4].parents[0].name.as_str(), "t3");
    }
}
