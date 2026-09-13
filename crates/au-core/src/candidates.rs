//! Implicit identity candidates ([[type candidate scan::au-type-system]]).
//!
//! A type-def is a *candidate* for an instance (or an inline value) when its
//! required-field set is fully satisfied by that scope's top-level fields and
//! the type-def is not already in the scope's identity closure. Candidates are
//! advisory output — they surface as "you could claim this type" suggestions;
//! they never validate or invalidate a file.
//!
//! The top-level case ([[type candidate scan::au-type-system]]) evaluates against the instance's frontmatter.
//! The nested case ([[type candidate scan::au-type-system]]) re-runs the same rule inside each inline value,
//! scoped to that inline value's own fields and closure. Each emitted
//! `Candidate` carries a `scope` handle so a consumer can tell which level a
//! suggestion applies to.
//!
//! Detection lives in this module; ranking ([[type candidate scan::au-type-system]]) lives alongside as a free
//! function. The full scan API lands when the nested walker arrives — for
//! now this module ships the data shapes only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_grammar::{RefMode, Shape};

use crate::closure::closure_of;
use crate::graph::TypeGraph;
use crate::instance::{Instance, InstanceField, InstanceValue, TypeClaim};
use crate::typedef::{FieldName, TypeName};
use crate::validate::value_matches_simple_shape;

/// One implicit-identity candidate surfaced by the scan.
///
/// `satisfied_required` lists T's required fields, alphabetical. Every name
/// appears in the scope's top-level fields — that is the rule.
/// `also_satisfied_optional` counts T's optional fields that also happen to
/// be present; it's the [[type candidate scan::au-type-system]] tiebreaker only, so the count is sufficient.
///
/// `supersedes` is non-empty when T is a leaf under a sealed family the
/// scope already claims a different leaf in. Promoting to T would require
/// dropping each named claim first per [[type-instance type::au-type-system]]'s multi-leaf-in-sealed-family
/// rule. An empty list means the candidate can be added cleanly. The scan
/// already skips sealed parents themselves ([[type-def sealed::au-type-system]] — they're unactionable
/// as direct claims), so a candidate here is always a non-sealed terminus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub type_name: TypeName,
    pub scope: CandidateScope,
    pub satisfied_required: Vec<FieldName>,
    pub also_satisfied_optional: usize,
    pub supersedes: Vec<TypeName>,
}

/// Where in the surrounding artifact a candidate applies.
///
/// `file_path` names the instance file the scan ran against. `inline_path`
/// is a JSON Pointer (RFC 6901: <https://www.rfc-editor.org/rfc/rfc6901>)
/// addressing the scope inside that file's frontmatter:
///
/// - `""` — the file's top-level frontmatter (RFC 6901 root convention).
/// - `/rationales/0` — the first element of the `rationales` field.
/// - `/rationales/0/evidence/1` — doubly nested.
///
/// Field names containing `/` or `~` are escaped per RFC 6901
/// (`/` → `~1`, `~` → `~0`). Consumers branching on `inline_path == ""`
/// distinguish top-level from nested without a separate marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateScope {
    pub file_path: PathBuf,
    pub inline_path: String,
}

impl CandidateScope {
    /// Convenience constructor for top-level scope. `inline_path` is `""`,
    /// matching RFC 6901's root-pointer convention.
    pub fn top_level(file_path: PathBuf) -> Self {
        Self {
            file_path,
            inline_path: String::new(),
        }
    }

    pub fn is_top_level(&self) -> bool {
        self.inline_path.is_empty()
    }
}

/// Required + optional field names contributed by a type-def's closure.
///
/// Required: every field where some declaration has `optional: false`.
/// Optional: every field where every declaration has `optional: true`.
/// The two sets are disjoint by construction — a name in `required` is
/// required at the candidate scope regardless of whether some origins also
/// marked it optional, which matches the [[type-def fields collision - auto-unify and qualified field::au-type-system]] "required wins" reading of
/// per-originator optionality.
struct ClosureFieldSets {
    required: BTreeSet<FieldName>,
    optional: BTreeSet<FieldName>,
}

fn closure_field_sets(graph: &TypeGraph, name: &TypeName) -> ClosureFieldSets {
    let mut required: BTreeSet<FieldName> = BTreeSet::new();
    let mut optional: BTreeSet<FieldName> = BTreeSet::new();
    for ancestor in closure_of(graph, name) {
        let Some(td) = graph.get(&ancestor) else {
            continue;
        };
        for f in &td.fields {
            if f.optional {
                optional.insert(f.name.clone());
            } else {
                required.insert(f.name.clone());
            }
        }
    }
    // A field declared required somewhere shadows an "optional" record from
    // a parallel chain. With no-redeclare ([[type subtyping width-only::au-type-system]]) this only matters under
    // mixin, but the candidate scan walks a single T's closure where it
    // shouldn't arise — keep the prune for invariant safety.
    for name in &required {
        optional.remove(name);
    }
    ClosureFieldSets { required, optional }
}

/// Strip the [[type-def fields collision - auto-unify and qualified field::au-type-system]] field qualifier from a frontmatter key, if any.
/// `foo{note}` → `foo`; `foo` → `foo`. The candidate scan checks
/// satisfaction by underlying field name regardless of whether the author
/// opted into qualifier syntax — the qualifier is a disambiguation glyph,
/// not a different field.
pub(crate) fn base_field_name(key: &str) -> &str {
    match key.split_once('{') {
        Some((name, _qualifier)) => name,
        None => key,
    }
}

fn frontmatter_field_names(fields: &[InstanceField]) -> BTreeSet<FieldName> {
    fields
        .iter()
        .map(|f| FieldName(base_field_name(&f.key).to_string()))
        .collect()
}

/// Closure of a `TypeClaim` — every type-name reachable through `type:`
/// parent chains from any element of the claim list. Used both by the
/// top-level detector (against the instance's identity claim) and the
/// nested detector (against each inline value's own claim).
pub(crate) fn claim_closure(graph: &TypeGraph, claim: &TypeClaim) -> BTreeSet<TypeName> {
    let mut closure: BTreeSet<TypeName> = BTreeSet::new();
    for c in claim.iter() {
        // A `::repo` peer claim resolves via the cross-repo fold, not this own
        // graph; it contributes nothing to the own-graph claim closure.
        if c.is_qualified() {
            continue;
        }
        closure.extend(closure_of(graph, &c.name));
    }
    closure
}

/// Emit one `Candidate` per type-def in the graph whose closure's required
/// field set is fully present in `frontmatter_names` and that is not
/// already in `closure`.
///
/// Filters in order:
///   - Sealed parents (`graph.is_sealed(name)`) skip — adding a sealed
///     name to `type:` would fire `sealed-parent-claimed` per [[type-def sealed::au-type-system]], so
///     they're unactionable as direct candidates.
///   - In-closure types skip per [[type candidate scan::au-type-system]](b).
///   - Empty-required-set types (tag types, all-optional) skip per
///     [[type candidate scan::au-type-system]]'s vacuous-match exclusion.
///   - Required-set not fully present in frontmatter skips per [[type candidate scan::au-type-system]](a).
///
/// `scope_claims` is the literal claim list at the candidate's scope —
/// `instance.type_claim`'s elements at top-level, or the inline value's
/// `type_claim` (or the slot's derived demand) for nested scopes. Used
/// to compute `Candidate.supersedes`: any of T's sealed ancestors that
/// the scope's claims already reach through a different leaf produces
/// a supersedes entry per the multi-leaf-in-sealed-family rule ([[type-instance type::au-type-system]]).
fn emit_candidates(
    graph: &TypeGraph,
    closure: &BTreeSet<TypeName>,
    scope_claims: &[TypeName],
    scope_fields: &[InstanceField],
    scope: &CandidateScope,
) -> Vec<Candidate> {
    let frontmatter_names = frontmatter_field_names(scope_fields);
    let mut value_by_name: BTreeMap<&str, &InstanceValue> = BTreeMap::new();
    for field in scope_fields {
        // Per [[type-def fields collision - auto-unify and qualified field::au-type-system]], the base field name (with any qualifier stripped)
        // is what the candidate's required-set is keyed by. Re-qualified
        // entries collide on the base name; first-wins is fine because
        // `mixed-bare-and-qualified-field` is a validation error and the
        // candidate scan stays advisory.
        value_by_name
            .entry(base_field_name(&field.key))
            .or_insert(&field.value);
    }

    let mut out = Vec::new();
    for (name, _td) in graph.iter() {
        // Non-claimable types never surface as candidates, a claim on them is
        // impossible. `is_abstract` folds in sealed, so this covers both a
        // declared-abstract base and a sealed parent. See
        // [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
        if graph.is_abstract(name) {
            continue;
        }
        if closure.contains(name) {
            continue;
        }
        let sets = closure_field_sets(graph, name);
        if sets.required.is_empty() {
            continue;
        }
        if !sets.required.is_subset(&frontmatter_names) {
            continue;
        }
        // [[type candidate scan::au-type-system]] (shape-aware satisfaction): every required field's *value*
        // must conform to T's declared shape per [[type validation::au-type-system]], not just be
        // present. An empty list against `T[+]` or a string against
        // `Number` is a non-fit, not a near-fit — partial-match
        // semantics belong to a future mode (see aspirations).
        let candidate_closure = closure_of(graph, name);
        let shape_map = build_field_shape_map(graph, &candidate_closure);
        let conforms = sets.required.iter().all(|fname| {
            let Some(shape) = shape_map.get(fname) else {
                // Required field has no parsed shape (load-time
                // diagnostic) — skip the conformance check; the
                // presence check above already covered it.
                return true;
            };
            let Some(value) = value_by_name.get(fname.as_str()) else {
                // Required field absent — the subset check above should
                // have caught this; defensive.
                return false;
            };
            candidate_value_satisfies_shape(value, shape)
        });
        if !conforms {
            continue;
        }
        let also_satisfied_optional = sets
            .optional
            .iter()
            .filter(|n| frontmatter_names.contains(*n))
            .count();
        let supersedes = find_supersedes(graph, name, scope_claims);
        out.push(Candidate {
            type_name: name.clone(),
            scope: scope.clone(),
            satisfied_required: sets.required.into_iter().collect(),
            also_satisfied_optional,
            supersedes,
        });
    }
    out
}

/// Shape-conformance check for the [[type candidate scan::au-type-system]] candidate scan. Returns `true` iff
/// `value` plausibly fits `shape` under [[type validation::au-type-system]]. Strict on primitive types,
/// enum membership, list cardinality (incl. `[+]` non-empty), and value-
/// shape category (string vs map vs sequence). Permissive on reference-
/// resolution (we don't walk the knowledge base) and on the names inside record /
/// reference / compound shapes — those would require closure + knowledge base
/// state the scan can't (yet) thread through. A reference slot accepts
/// any string here; full validation catches mismatches at the use site.
fn candidate_value_satisfies_shape(value: &InstanceValue, shape: &Shape) -> bool {
    match shape {
        Shape::Primitive(_) | Shape::Enum(_) => value_matches_simple_shape(value, shape),
        // A refined scalar is a candidate when its base primitive matches.
        Shape::Refined { base, .. } => value_matches_simple_shape(value, &Shape::Primitive(*base)),
        // An `any` slot ([[type-def shape any::au-type-system]]) or an `opaque` slot
        // ([[type-def shape opaque::au-type-system]]) imposes no type, so every value satisfies it.
        Shape::Any | Shape::Opaque => true,
        Shape::List { inner, min, max } => match value {
            InstanceValue::Sequence(elements) => {
                let len = elements.len();
                if len < *min as usize {
                    return false;
                }
                if let Some(m) = max {
                    if len > *m as usize {
                        return false;
                    }
                }
                elements
                    .iter()
                    .all(|el| candidate_value_satisfies_shape(&el.value, inner))
            }
            _ => false,
        },
        Shape::Reference(_) => matches!(value, InstanceValue::String(_)),
        // A def-reference ([[type-def shape def-ref::au-type-system]], `type<T>*` / `type*`) is a
        // wikilink string, like any reference. Full def-axis validation
        // happens at the use site.
        Shape::DefReference(_) => matches!(value, InstanceValue::String(_)),
        Shape::Record(_) => matches!(value, InstanceValue::Mapping(_)),
        Shape::InlineOrReference(_) => {
            matches!(value, InstanceValue::String(_) | InstanceValue::Mapping(_))
        }
        Shape::CompoundReference { mode, .. } => match mode {
            RefMode::Star => matches!(value, InstanceValue::String(_)),
            RefMode::Inline => {
                matches!(value, InstanceValue::String(_) | InstanceValue::Mapping(_))
            }
        },
        Shape::Union(branches) => branches
            .iter()
            .any(|b| candidate_value_satisfies_shape(value, b)),
        Shape::Intersection(branches) => branches
            .iter()
            .all(|b| candidate_value_satisfies_shape(value, b)),
        // A tuple value is a `Name(...)` constructor string, or a bare YAML
        // sequence; the scan is permissive, full validation is at the use site.
        Shape::Tuple(_) => {
            matches!(value, InstanceValue::String(_) | InstanceValue::Sequence(_))
        }
        // A commit-pinned reference ([[type-def shape suffixes::au-type-system]] `*@`) is its inner
        // reference shape plus a pin requirement; the scan is permissive on
        // resolution, so it delegates to the inner.
        Shape::Pinned(inner) => candidate_value_satisfies_shape(value, inner),
    }
}

/// Identify the scope's claimed leaves that the candidate would conflict
/// with under [[type-instance type::au-type-system]]'s multi-leaf-in-sealed-family rule. For each sealed
/// type-def S in the candidate's closure, any scope-claim whose own
/// closure also contains S is a sibling-family conflict — promoting to
/// the candidate would require dropping that claim first.
///
/// Returns an alphabetical, deduplicated list. Empty when the candidate
/// has no sealed ancestors or none of the scope's claims reach those
/// sealed ancestors.
fn find_supersedes(
    graph: &TypeGraph,
    candidate: &TypeName,
    scope_claims: &[TypeName],
) -> Vec<TypeName> {
    let candidate_closure = closure_of(graph, candidate);
    let sealed_ancestors: BTreeSet<TypeName> = candidate_closure
        .iter()
        .filter(|n| graph.is_sealed(n))
        .cloned()
        .collect();
    if sealed_ancestors.is_empty() {
        return Vec::new();
    }
    let mut supersedes: BTreeSet<TypeName> = BTreeSet::new();
    for claim in scope_claims {
        // Skip the trivial case where the claim IS the candidate (can't
        // happen — the closure filter excludes that — but defensive).
        if claim == candidate {
            continue;
        }
        let claim_closure = closure_of(graph, claim);
        if claim_closure.iter().any(|n| sealed_ancestors.contains(n)) {
            supersedes.insert(claim.clone());
        }
    }
    supersedes.into_iter().collect()
}

/// Scan an instance's top-level frontmatter for [[type candidate scan::au-type-system]] identity candidates.
///
/// Returns one `Candidate` per type-def T in the graph that satisfies:
///   (a) every required field of T's closure is present as a top-level
///       frontmatter key on the instance (whether the key currently lands
///       inside the identity closure or surfaces as an extra is
///       irrelevant — both partitions count);
///   (b) T is not already in the instance's identity closure;
///   (c) T's required-field set is non-empty (tag-types and all-optional
///       types never surface — they would match every file vacuously).
///
/// Output order is alphabetical by type-name; the ranking pass ([[type candidate scan::au-type-system]])
/// sorts the list afterwards. For combined top-level + nested scanning
/// with ranking, use [`scan`].
pub fn scan_top_level(graph: &TypeGraph, instance: &Instance) -> Vec<Candidate> {
    let closure = claim_closure(graph, &instance.type_claim);
    let claims: Vec<TypeName> = instance.type_claim.iter().map(|c| c.name.clone()).collect();
    let scope = CandidateScope::top_level(instance.source_path.clone());
    emit_candidates(graph, &closure, &claims, &instance.fields, &scope)
}

/// Escape one JSON Pointer reference token per RFC 6901 §3:
/// `~` → `~0`, `/` → `~1`. The `~`-first ordering matters so the `~0`
/// introduced by the first replacement isn't re-interpreted as a token.
fn json_pointer_escape(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// Build a name → slot-shape map for the current host's effective shape.
///
/// Iterates every type-def in the closure and collects each declared
/// field's parsed shape, keyed by field name. With width-only subtyping
/// + no-redeclare ([[type subtyping width-only::au-type-system]]), each name has exactly one declaring origin
/// per closure (mixin auto-unify keeps shapes token-equal), so the
/// "first encountered" pick is canonical. Fields whose shape failed to
/// parse (Err) are skipped — they're already a load-time diagnostic and
/// can't drive slot dispatch.
pub(crate) fn build_field_shape_map<'a>(
    graph: &'a TypeGraph,
    closure: &BTreeSet<TypeName>,
) -> BTreeMap<FieldName, &'a Shape> {
    let mut out = BTreeMap::new();
    for tn in closure {
        let Some(td) = graph.get(tn) else {
            continue;
        };
        for f in &td.fields {
            if let Ok(shape) = &f.parsed_shape {
                out.entry(f.name.clone()).or_insert(shape);
            }
        }
    }
    out
}

/// The slot's demanded type-def for a Mapping-valued field, if the shape
/// names exactly one record type. Spec [[type-def shape record::au-type-system]] case 1 (`name`) and [[type-def shape record::au-type-system]]'s
/// `name&` (Shape::InlineOrReference) both nominate a single record;
/// other shapes (primitives, enums, unions, intersections, compound
/// references) either don't accept inline maps or require an explicit
/// `type:` claim ([[type-def shape record::au-type-system]] cases 3–4), so the candidate walker treats them
/// as ambiguous and emits nothing for unclaimed inline values there.
pub(crate) fn record_demanded_type(shape: &Shape) -> Option<TypeName> {
    match shape {
        Shape::Record(name) | Shape::InlineOrReference(name) => {
            Some(TypeName(name.as_str().to_string()))
        }
        _ => None,
    }
}

/// The element shape inside a `Shape::List(inner)`, used when descending
/// into a Sequence value. Other shapes don't accept sequences, so the
/// element-side slot context is undefined and we emit nothing for
/// elements there (validate fires the type mismatch separately).
/// True when a slot holds opaque content the scans must not descend into: an
/// `opaque` slot ([[type-def shape opaque::au-type-system]]). The interpreted top `any` (and
/// `any&`) is NOT opaque, its content is scanned like any other value. `any*` is
/// a plain reference, not opaque, and never reaches the Mapping/Sequence descent
/// anyway.
pub(crate) fn is_opaque_slot(shape: Option<&Shape>) -> bool {
    matches!(shape, Some(Shape::Opaque))
}

pub(crate) fn sequence_element_shape(shape: &Shape) -> Option<&Shape> {
    if let Shape::List { inner, .. } = shape {
        Some(inner)
    } else {
        None
    }
}

/// Walk an instance's inline-value tree, emitting candidates at each
/// scope as we go. Threads the "current host closure" through the
/// recursion so unclaimed inline values ([[type-def shape record::au-type-system]] case 1) resolve their
/// implied identity from the surrounding slot's demanded type.
fn walk_and_emit_inline(
    graph: &TypeGraph,
    host_closure: &BTreeSet<TypeName>,
    fields: &[InstanceField],
    path: &str,
    file_path: &Path,
    out: &mut Vec<Candidate>,
) {
    let shape_map = build_field_shape_map(graph, host_closure);
    for field in fields {
        let key_name = FieldName(base_field_name(&field.key).to_string());
        let slot_shape = shape_map.get(&key_name).copied();
        let token = json_pointer_escape(&field.key);
        descend_value(
            graph,
            slot_shape,
            &field.value,
            format!("{path}/{token}"),
            file_path,
            out,
        );
    }
}

fn descend_value(
    graph: &TypeGraph,
    slot_shape: Option<&Shape>,
    value: &InstanceValue,
    path: String,
    file_path: &Path,
    out: &mut Vec<Candidate>,
) {
    // An `opaque` slot ([[type-def shape opaque::au-type-system]]) holds content the scan does
    // not descend into, so no nested candidate surfaces from an uninterpreted
    // payload. `opaque[]` reaches here per element with the inner `Shape::Opaque`,
    // so elements are skipped too. The interpreted top `any` is NOT skipped, its
    // content is scanned like any other value.
    if is_opaque_slot(slot_shape) {
        return;
    }
    match value {
        InstanceValue::Mapping(inline) => {
            // Resolve the inline value's closure AND its scope claims:
            // explicit `type:` wins; otherwise fall back to the slot's
            // demanded type per [[type-def shape record::au-type-system]] case 1. Without either, we have no
            // closure to subtract from — emit nothing for this scope
            // (the typical case is a union/intersection slot where
            // validate is already firing a missing-type error).
            let (closure, claims) = if let Some(claim) = &inline.type_claim {
                let cs: Vec<TypeName> = claim.iter().map(|c| c.name.clone()).collect();
                (claim_closure(graph, claim), cs)
            } else if let Some(demanded) = slot_shape
                .and_then(record_demanded_type)
                .filter(|name| graph.contains(name))
            {
                // Claim-less slot-pinned record: its identity is the
                // slot's, not an implicit-identity question. A candidate
                // here would advertise a promotion (`type: other`) that
                // REPLACES the pin and breaks the slot — explicit claims
                // win over pinning per [[type block-id::au-type-system]]. Suppressed; the
                // scan resumes once the record carries an explicit claim.
                // Nested values still walk against the pinned shape.
                let closure = closure_of(graph, &demanded);
                walk_and_emit_inline(graph, &closure, &inline.fields, &path, file_path, out);
                return;
            } else {
                // Still recurse into the inline's fields with an empty
                // host closure — its own nested inline values may have
                // explicit `type:` claims and should still surface.
                walk_and_emit_inline(
                    graph,
                    &BTreeSet::new(),
                    &inline.fields,
                    &path,
                    file_path,
                    out,
                );
                return;
            };
            let scope = CandidateScope {
                file_path: file_path.to_path_buf(),
                inline_path: path.clone(),
            };
            out.extend(emit_candidates(
                graph,
                &closure,
                &claims,
                &inline.fields,
                &scope,
            ));
            walk_and_emit_inline(graph, &closure, &inline.fields, &path, file_path, out);
        }
        InstanceValue::Sequence(elements) => {
            let inner = slot_shape.and_then(sequence_element_shape);
            for (idx, element) in elements.iter().enumerate() {
                descend_value(
                    graph,
                    inner,
                    &element.value,
                    format!("{path}/{idx}"),
                    file_path,
                    out,
                );
            }
        }
        _ => {}
    }
}

/// Full [[type candidate scan::au-type-system]] scan: top-level ([[type candidate scan::au-type-system]]) plus every inline-value scope ([[type candidate scan::au-type-system]]),
/// aggregated into one ranked list ([[type candidate scan::au-type-system]]). Each `Candidate.scope`
/// disambiguates which level the suggestion applies to; consumers may
/// group by scope to render per-level UIs.
///
/// Unclaimed inline values (`type:` omitted) resolve their closure via
/// the surrounding slot's demanded type per [[type-def shape record::au-type-system]] case 1 — the walker
/// threads the host closure through and looks up each Mapping field's
/// declared shape to find the demand. Unions, intersections, and
/// compound-reference slots require an explicit `type:` claim ([[type-def shape record::au-type-system]]
/// cases 3–4); the walker treats them as ambiguous and emits nothing
/// for unclaimed inline values at those sites.
pub fn scan(graph: &TypeGraph, instance: &Instance) -> Vec<Candidate> {
    let mut out = scan_top_level(graph, instance);
    let host_closure = claim_closure(graph, &instance.type_claim);
    walk_and_emit_inline(
        graph,
        &host_closure,
        &instance.fields,
        "",
        &instance.source_path,
        &mut out,
    );
    rank(&mut out);
    out
}

/// Rank candidates in place per [[type candidate scan::au-type-system]]: required-field-set size primary,
/// also-satisfied-optional-field count secondary, both descending. Stable
/// tertiary on `(type_name, scope.inline_path)` ascending so output is
/// reproducible across runs and the same scope's candidates stay grouped
/// when their primary/secondary keys tie.
///
/// Specificity reads as "more required fields means a tighter match." Every
/// surfaced candidate satisfies *all* of its required fields by
/// construction (`scan_top_level` filters non-satisfied), so the
/// discriminator is the *size* of that set, not how many of its members
/// are present.
pub fn rank(candidates: &mut Vec<Candidate>) {
    candidates.sort_by(|a, b| {
        b.satisfied_required
            .len()
            .cmp(&a.satisfied_required.len())
            .then_with(|| b.also_satisfied_optional.cmp(&a.also_satisfied_optional))
            .then_with(|| a.type_name.cmp(&b.type_name))
            .then_with(|| a.scope.inline_path.cmp(&b.scope.inline_path))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::instance::{Instance, InstanceField, InstanceValue, TypeClaim};
    use crate::typedef::{FieldDecl, TypeDef, TypeNameClaim};
    use au_diagnostics::ByteRange;
    use au_grammar::{Primitive, Shape};
    use std::path::PathBuf;

    /// Build a minimal type-def. Each field is `(name, optional)` — all
    /// fields share `String` shape since most candidate tests only look at
    /// field names and the required/optional bit. Use `td_typed` when a
    /// specific slot shape matters (record-typed slots, lists, unions).
    fn td(name: &str, parents: &[&str], fields: &[(&str, bool)]) -> TypeDef {
        td_typed(
            name,
            parents,
            &fields
                .iter()
                .map(|(n, o)| (*n, *o, Shape::Primitive(Primitive::String)))
                .collect::<Vec<_>>(),
        )
    }

    fn td_typed(name: &str, parents: &[&str], fields: &[(&str, bool, Shape)]) -> TypeDef {
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim: None,
            parents: parents
                .iter()
                .map(|p| TypeNameClaim::own(TypeName((*p).into()), ByteRange::new(0, 0)))
                .collect(),
            fields: fields
                .iter()
                .map(|(fname, optional, shape)| FieldDecl {
                    name: FieldName((*fname).into()),
                    optional: *optional,
                    raw_shape: format!("{shape}"),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(shape.clone()),
                    doc: None,
                })
                .collect(),
            sealed: vec![],
            declared_abstract: false,
            meta_blocks: None,
            required_meta: Vec::new(),
            body: None,
            doc: None,
            ..Default::default()
        }
    }

    fn bare_claim(name: &str) -> TypeClaim {
        TypeClaim::Bare(TypeNameClaim::own(
            TypeName(name.into()),
            ByteRange::new(0, 0),
        ))
    }

    fn instance(claim: TypeClaim, field_keys: &[&str]) -> Instance {
        Instance {
            source_path: PathBuf::from("/v/inst.md"),
            source_span: ByteRange::new(0, 0),
            type_claim: claim,
            fields: field_keys
                .iter()
                .map(|k| InstanceField {
                    key: (*k).into(),
                    key_span: ByteRange::new(0, 0),
                    value: InstanceValue::String("v".into()),
                    value_span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                })
                .collect(),
            doc: None,
            field_docs: Default::default(),
        }
    }

    fn names(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.type_name.as_str()).collect()
    }

    #[test]
    fn orthogonal_overlap_surfaces_summary() {
        // `summary` requires `description` — same as `note`. Pre-amendment
        // this was implicitly excluded because there were no extras driving
        // the scan; the [[type candidate scan::au-type-system]] rewrite surfaces it.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("summary", &[], &[("description", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description"]);
        assert_eq!(names(&scan_top_level(&g, &inst)), vec!["summary"]);
    }

    #[test]
    fn closure_member_does_not_surface() {
        // The instance already claims `note`; `note` must not surface as a
        // candidate even though it would technically match.
        let g = build_graph(vec![td("note", &[], &[("description", false)])]).graph;
        let inst = instance(bare_claim("note"), &["description"]);
        assert!(scan_top_level(&g, &inst).is_empty());
    }

    #[test]
    fn ancestor_does_not_surface() {
        // `decision` extends `note`; an instance claiming `decision` has
        // `note` in its closure — `note` must not surface.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("decision", &["note"], &[]),
        ])
        .graph;
        let inst = instance(bare_claim("decision"), &["description"]);
        assert!(scan_top_level(&g, &inst).is_empty());
    }

    #[test]
    fn extras_satisfy_surfaces_deliverable() {
        // Instance claims `note`, has extras `audience` and `due`. Type
        // `deliverable` requires both `description` and `audience` —
        // satisfied via the extra. `due` is unaccounted for; it stays an
        // extra and is irrelevant to the candidate match.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td(
                "deliverable",
                &[],
                &[("description", false), ("audience", false)],
            ),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description", "audience", "due"]);
        assert_eq!(names(&scan_top_level(&g, &inst)), vec!["deliverable"]);
    }

    #[test]
    fn tag_type_never_surfaces() {
        // `tag` has zero fields. It would match every instance vacuously —
        // [[type candidate scan::au-type-system]] explicitly excludes it.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("tag", &[], &[]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description"]);
        assert!(scan_top_level(&g, &inst).is_empty());
    }

    #[test]
    fn all_optional_never_surfaces() {
        // `hint` has only an optional field. An empty required-set means
        // it would match every file vacuously — same exclusion rule.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("hint", &[], &[("tldr", true)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description", "tldr"]);
        assert!(scan_top_level(&g, &inst).is_empty());
    }

    #[test]
    fn shape_mismatched_value_does_not_surface() {
        // [[type candidate scan::au-type-system]] (shape-aware): instance has `count` as a string, but the
        // candidate's `count` slot demands `Number`. Presence alone
        // doesn't qualify; value must conform.
        let g = build_graph(vec![
            td_typed(
                "note",
                &[],
                &[("description", false, Shape::Primitive(Primitive::String))],
            ),
            td_typed(
                "metric",
                &[],
                &[
                    ("description", false, Shape::Primitive(Primitive::String)),
                    ("count", false, Shape::Primitive(Primitive::Number)),
                ],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("note"),
            vec![
                field("description", InstanceValue::String("a".into())),
                field("count", InstanceValue::String("not-a-number".into())),
            ],
        );
        assert!(
            scan_top_level(&g, &inst).is_empty(),
            "metric must NOT surface — `count` value doesn't conform to Number"
        );
    }

    #[test]
    fn non_empty_list_with_empty_value_does_not_surface() {
        // [[type candidate scan::au-type-system]] (shape-aware) + [[type-def shape suffixes::au-type-system]]: candidate requires `tags: String[+]`
        // (non-empty). Instance has `tags: []`. Empty list is a non-fit
        // on that field, not a near-fit — candidate does NOT surface.
        let g = build_graph(vec![
            td_typed(
                "note",
                &[],
                &[("description", false, Shape::Primitive(Primitive::String))],
            ),
            td_typed(
                "tagged",
                &[],
                &[
                    ("description", false, Shape::Primitive(Primitive::String)),
                    (
                        "tags",
                        false,
                        Shape::List {
                            inner: Box::new(Shape::Primitive(Primitive::String)),
                            min: 1,
                            max: None,
                        },
                    ),
                ],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("note"),
            vec![
                field("description", InstanceValue::String("a".into())),
                field("tags", seq(vec![])),
            ],
        );
        assert!(
            scan_top_level(&g, &inst).is_empty(),
            "tagged must NOT surface — empty list doesn't satisfy String[+]"
        );
    }

    #[test]
    fn non_empty_list_with_value_surfaces() {
        // Mirror of the above — non-empty list satisfies String[+] and
        // the candidate surfaces.
        let g = build_graph(vec![
            td_typed(
                "note",
                &[],
                &[("description", false, Shape::Primitive(Primitive::String))],
            ),
            td_typed(
                "tagged",
                &[],
                &[
                    ("description", false, Shape::Primitive(Primitive::String)),
                    (
                        "tags",
                        false,
                        Shape::List {
                            inner: Box::new(Shape::Primitive(Primitive::String)),
                            min: 1,
                            max: None,
                        },
                    ),
                ],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("note"),
            vec![
                field("description", InstanceValue::String("a".into())),
                field("tags", seq(vec![InstanceValue::String("t1".into())])),
            ],
        );
        assert_eq!(names(&scan_top_level(&g, &inst)), vec!["tagged"]);
    }

    #[test]
    fn enum_non_member_value_does_not_surface() {
        // Candidate requires `status: [made, superseded]`. Instance has
        // `status: "draft"` — not a member. Candidate does NOT surface.
        let g = build_graph(vec![
            td_typed(
                "note",
                &[],
                &[("description", false, Shape::Primitive(Primitive::String))],
            ),
            td_typed(
                "decided",
                &[],
                &[
                    ("description", false, Shape::Primitive(Primitive::String)),
                    (
                        "status",
                        false,
                        Shape::Enum(vec!["made".into(), "superseded".into()]),
                    ),
                ],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("note"),
            vec![
                field("description", InstanceValue::String("a".into())),
                field("status", InstanceValue::String("draft".into())),
            ],
        );
        assert!(
            scan_top_level(&g, &inst).is_empty(),
            "decided must NOT surface — 'draft' is not in the enum"
        );
    }

    #[test]
    fn required_absent_does_not_surface() {
        // `deliverable` requires `audience`. Instance has only
        // `description`. No candidate.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td(
                "deliverable",
                &[],
                &[("description", false), ("audience", false)],
            ),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description"]);
        assert!(scan_top_level(&g, &inst).is_empty());
    }

    #[test]
    fn qualified_field_satisfies_candidate() {
        // [[type-def fields collision - auto-unify and qualified field::au-type-system]] qualifier syntax: `description{note}` is the same field as
        // `description` for candidate purposes. The qualifier disambiguates
        // semantic identity, not field identity.
        let g = build_graph(vec![
            // Two types with same-named field create the [[type-def fields collision - auto-unify and qualified field::au-type-system]] collision
            // setting under which the user typed the qualifier in the first
            // place. The candidate scan doesn't need either of them in
            // closure — just a target that wants `description`.
            td("note", &[], &[("description", false)]),
            td("summary", &[], &[("description", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description{note}"]);
        assert_eq!(names(&scan_top_level(&g, &inst)), vec!["summary"]);
    }

    #[test]
    fn ranking_by_required_size_then_optional_count() {
        // Three candidates:
        //   - `summary` requires 1 (`description`); 0 optional satisfied
        //   - `deliverable` requires 2 (`description`, `audience`); 0 opt
        //   - `summary-extra` requires 1; 1 optional satisfied (`tldr`)
        // Order: deliverable (size 2), summary-extra (size 1 + 1 opt),
        // summary (size 1 + 0 opt).
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("summary", &[], &[("description", false)]),
            td(
                "summary-extra",
                &[],
                &[("description", false), ("tldr", true)],
            ),
            td(
                "deliverable",
                &[],
                &[("description", false), ("audience", false)],
            ),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description", "audience", "tldr"]);
        let mut cands = scan_top_level(&g, &inst);
        rank(&mut cands);
        assert_eq!(
            names(&cands),
            vec!["deliverable", "summary-extra", "summary"]
        );
    }

    #[test]
    fn determinism_across_calls() {
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("summary", &[], &[("description", false)]),
            td(
                "deliverable",
                &[],
                &[("description", false), ("audience", false)],
            ),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description", "audience"]);
        let a = scan_top_level(&g, &inst);
        let b = scan_top_level(&g, &inst);
        assert_eq!(a, b);
    }

    #[test]
    fn scope_is_top_level() {
        // Every emitted scope must report top-level (inline_path == "")
        // and match the instance's source_path.
        let g = build_graph(vec![
            td("note", &[], &[("description", false)]),
            td("summary", &[], &[("description", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["description"]);
        let cands = scan_top_level(&g, &inst);
        assert_eq!(cands.len(), 1);
        assert!(cands[0].scope.is_top_level());
        assert_eq!(cands[0].scope.file_path, inst.source_path);
    }

    // ---- Nested ([[type candidate scan::au-type-system]]) ------------------------------------------------

    use crate::instance::{InlineValue, SequenceElement};

    fn field(key: &str, value: InstanceValue) -> InstanceField {
        InstanceField {
            key: key.into(),
            key_span: ByteRange::new(0, 0),
            value,
            value_span: ByteRange::new(0, 0),
            nav_links: Vec::new(),
        }
    }

    fn inline(claim: Option<&str>, fields: Vec<InstanceField>) -> InlineValue {
        InlineValue {
            type_claim: claim.map(bare_claim),
            block_id: None,
            fields,
            doc: None,
            field_docs: Default::default(),
        }
    }

    fn seq(values: Vec<InstanceValue>) -> InstanceValue {
        InstanceValue::Sequence(
            values
                .into_iter()
                .map(|v| SequenceElement {
                    value: v,
                    span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                })
                .collect(),
        )
    }

    fn instance_with(claim: TypeClaim, fields: Vec<InstanceField>) -> Instance {
        Instance {
            source_path: PathBuf::from("/v/inst.md"),
            source_span: ByteRange::new(0, 0),
            type_claim: claim,
            fields,
            doc: None,
            field_docs: Default::default(),
        }
    }

    fn by_scope(candidates: &[Candidate]) -> Vec<(&str, &str)> {
        candidates
            .iter()
            .map(|c| (c.type_name.as_str(), c.scope.inline_path.as_str()))
            .collect()
    }

    #[test]
    fn opaque_slot_is_not_descended_by_candidate_scan() {
        // [[type-def shape opaque::au-type-system]]: an `opaque` slot holds uninterpreted content.
        // The same inline value that surfaces a candidate at a `rationale` record
        // slot (see `nested_inline_surfaces_at_inline_scope`) surfaces nothing at
        // an `opaque` slot — the scan does not descend into the uninterpreted value.
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td_typed("host", &[], &[("payload", false, Shape::Opaque)]),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "payload",
                InstanceValue::Mapping(inline(
                    Some("rationale"),
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("evidence", InstanceValue::String("e".into())),
                    ],
                )),
            )],
        );
        assert_eq!(by_scope(&scan(&g, &inst)), Vec::<(&str, &str)>::new());
    }

    #[test]
    fn any_slot_is_descended_by_candidate_scan() {
        // [[type-def shape any::au-type-system]]: the interpreted top IS descended. The same
        // inline value that surfaces nothing at an `opaque` slot surfaces a
        // candidate at an `any` slot, the interpreted-vs-uninterpreted contrast.
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td_typed("host", &[], &[("payload", false, Shape::Any)]),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "payload",
                InstanceValue::Mapping(inline(
                    Some("rationale"),
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("evidence", InstanceValue::String("e".into())),
                    ],
                )),
            )],
        );
        // The inline value claims `rationale` and also carries `evidence`, so
        // `rationale-with-evidence` surfaces at the nested scope.
        assert_eq!(
            by_scope(&scan(&g, &inst)),
            vec![("rationale-with-evidence", "/payload")]
        );
    }

    #[test]
    fn nested_inline_surfaces_at_inline_scope() {
        // Host claims `decision-record` whose `rationale` slot holds an
        // inline `rationale` (description). The inline value also has
        // `evidence` — `rationale-with-evidence` requires both, so it
        // surfaces as a candidate at the inline scope.
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td(
                "decision-record",
                &[],
                &[("title", false), ("rationale", false)],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![
                field("title", InstanceValue::String("t".into())),
                field(
                    "rationale",
                    InstanceValue::Mapping(inline(
                        Some("rationale"),
                        vec![
                            field("description", InstanceValue::String("d".into())),
                            field("evidence", InstanceValue::String("e".into())),
                        ],
                    )),
                ),
            ],
        );
        let cands = scan(&g, &inst);
        assert_eq!(
            by_scope(&cands),
            vec![("rationale-with-evidence", "/rationale")]
        );
    }

    #[test]
    fn nested_inline_inside_sequence_uses_index_in_pointer() {
        // The slot holds a list of inline rationales; the second entry has
        // an extra `evidence` field satisfying `rationale-with-evidence`.
        // Pointer: `/rationales/1`.
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td(
                "decision-record",
                &[],
                &[("title", false), ("rationales", false)],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![
                field("title", InstanceValue::String("t".into())),
                field(
                    "rationales",
                    seq(vec![
                        InstanceValue::Mapping(inline(
                            Some("rationale"),
                            vec![field("description", InstanceValue::String("plain".into()))],
                        )),
                        InstanceValue::Mapping(inline(
                            Some("rationale"),
                            vec![
                                field("description", InstanceValue::String("evidenced".into())),
                                field("evidence", InstanceValue::String("e".into())),
                            ],
                        )),
                    ]),
                ),
            ],
        );
        let cands = scan(&g, &inst);
        assert_eq!(
            by_scope(&cands),
            vec![("rationale-with-evidence", "/rationales/1")]
        );
    }

    #[test]
    fn doubly_nested_inline_pointer_reflects_depth() {
        // rationale → evidence (inline, in a list) → extras satisfy a
        // richer evidence type. Pointer: `/rationale/evidence/0`.
        let g = build_graph(vec![
            td("evidence-item", &[], &[("kind", false), ("source", false)]),
            td(
                "evidence-with-citation",
                &[],
                &[("kind", false), ("source", false), ("citation", false)],
            ),
            td(
                "rationale",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td(
                "decision-record",
                &[],
                &[("title", false), ("rationale", false)],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![
                field("title", InstanceValue::String("t".into())),
                field(
                    "rationale",
                    InstanceValue::Mapping(inline(
                        Some("rationale"),
                        vec![
                            field("description", InstanceValue::String("d".into())),
                            field(
                                "evidence",
                                seq(vec![InstanceValue::Mapping(inline(
                                    Some("evidence-item"),
                                    vec![
                                        field("kind", InstanceValue::String("paper".into())),
                                        field("source", InstanceValue::String("[[a]]".into())),
                                        field("citation", InstanceValue::String("c".into())),
                                    ],
                                ))]),
                            ),
                        ],
                    )),
                ),
            ],
        );
        let cands = scan(&g, &inst);
        assert_eq!(
            by_scope(&cands),
            vec![("evidence-with-citation", "/rationale/evidence/0")]
        );
    }

    #[test]
    fn mixed_top_level_and_nested_candidates() {
        // Top-level frontmatter (`description`, `rationale`) uniquely
        // satisfies `summary` (requires `description`). The inline
        // rationale's frontmatter (`text`, `evidence`) uniquely satisfies
        // `rationale-with-evidence`. Field names deliberately disjoint
        // across scopes so each candidate surfaces exactly once at its
        // expected scope. Ranking puts the larger required-set first.
        let g = build_graph(vec![
            td("summary", &[], &[("description", false)]),
            td("rationale", &[], &[("text", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("text", false), ("evidence", false)],
            ),
            td(
                "decision-record",
                &[],
                &[("description", false), ("rationale", false)],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![
                field("description", InstanceValue::String("d".into())),
                field(
                    "rationale",
                    InstanceValue::Mapping(inline(
                        Some("rationale"),
                        vec![
                            field("text", InstanceValue::String("rt".into())),
                            field("evidence", InstanceValue::String("e".into())),
                        ],
                    )),
                ),
            ],
        );
        let cands = scan(&g, &inst);
        // `rationale-with-evidence` (size 2) ranks before `summary` (size 1).
        assert_eq!(
            by_scope(&cands),
            vec![("rationale-with-evidence", "/rationale"), ("summary", ""),]
        );
    }

    #[test]
    fn unclaimed_inline_value_resolves_via_slot_demand() {
        // [[type candidate scan::au-type-system]]: a claim-less record at a single
        // non-sealed record slot is SKIPPED. Its identity is the slot's;
        // a candidate would advertise an explicit claim that replaces
        // the pin and breaks the slot. The scan resumes once the record
        // carries an explicit `type:` (see the explicit-claim test
        // below).
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td_typed(
                "decision-record",
                &[],
                &[("rationale", false, Shape::Record("rationale".into()))],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    None, // type: omitted — identity is the slot's pin
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("evidence", InstanceValue::String("e".into())),
                    ],
                )),
            )],
        );
        assert_eq!(by_scope(&scan(&g, &inst)), Vec::<(&str, &str)>::new());
    }

    #[test]
    fn explicitly_claimed_record_at_a_pinned_slot_still_scans() {
        // An explicit `type:` opts the record back into the scan —
        // promotion there is a safe mixin extension, not a pin
        // replacement.
        let g = build_graph(vec![
            td("rationale", &[], &[("description", false)]),
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td_typed(
                "decision-record",
                &[],
                &[("rationale", false, Shape::Record("rationale".into()))],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("decision-record"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    Some("rationale"), // explicit claim, scan resumes
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("evidence", InstanceValue::String("e".into())),
                    ],
                )),
            )],
        );
        assert_eq!(
            by_scope(&scan(&g, &inst)),
            vec![("rationale-with-evidence", "/rationale")]
        );
    }

    #[test]
    fn unclaimed_inline_in_sequence_resolves_via_list_inner() {
        // Claim-less pinned records inside a list slot are skipped the
        // same way the singleton case is — the canvas pattern
        // (`nodes: node&[]` with `^:` ids) stays candidate-quiet.
        let g = build_graph(vec![
            td("evidence-item", &[], &[("kind", false)]),
            td(
                "evidence-with-source",
                &[],
                &[("kind", false), ("source", false)],
            ),
            td_typed(
                "rationale",
                &[],
                &[(
                    "evidence",
                    false,
                    Shape::List {
                        inner: Box::new(Shape::Record("evidence-item".into())),
                        min: 0,
                        max: None,
                    },
                )],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("rationale"),
            vec![field(
                "evidence",
                seq(vec![InstanceValue::Mapping(inline(
                    None, // type: omitted — derived from List<Record(evidence-item)>
                    vec![
                        field("kind", InstanceValue::String("paper".into())),
                        field("source", InstanceValue::String("s".into())),
                    ],
                ))]),
            )],
        );
        assert_eq!(by_scope(&scan(&g, &inst)), Vec::<(&str, &str)>::new());
    }

    // ---- Sealed-family filtering ([[type-def sealed::au-type-system]], [[type-instance type::au-type-system]]) ---------------------------

    fn td_sealed(name: &str, parents: &[&str], sealed: &[&str]) -> TypeDef {
        let mut t = td(name, parents, &[]);
        t.sealed = sealed
            .iter()
            .map(|s| TypeNameClaim::own(TypeName((*s).into()), ByteRange::new(0, 0)))
            .collect();
        t
    }

    #[test]
    fn sealed_parents_never_surface_as_candidates() {
        // A sealed type-def cannot be claimed directly per [[type-def sealed::au-type-system]]
        // (`sealed-parent-claimed`). Surfacing it as a candidate would
        // be an unactionable suggestion. The scan skips sealed names
        // entirely; only non-sealed leaves remain.
        let g = build_graph(vec![
            td_sealed("decision", &[], &["decision.pending", "decision.decided"]),
            td("decision.pending", &["decision"], &[("status", false)]),
            td("decision.decided", &["decision"], &[("status", false)]),
            // The instance type — also requires `status`, but in closure already.
            td("note", &[], &[("status", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["status"]);
        let cands = scan(&g, &inst);
        // `decision` is sealed → skipped. The two leaves surface (neither
        // sealed, neither in closure). `note` is in closure.
        let mut got: Vec<&str> = cands.iter().map(|c| c.type_name.as_str()).collect();
        got.sort();
        assert_eq!(got, vec!["decision.decided", "decision.pending"]);
        for c in &cands {
            assert!(
                c.supersedes.is_empty(),
                "no claims to supersede on a clean note"
            );
        }
    }

    #[test]
    fn abstract_types_never_surface_as_candidates() {
        // A declared-abstract type is non-claimable per
        // [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]],
        // so it is never an actionable candidate. Its concrete subtype still surfaces.
        let mut pane = td("pane", &[], &[("title", false)]);
        pane.declared_abstract = true;
        let g = build_graph(vec![
            pane,
            td("pane.split", &["pane"], &[]),
            td("note", &[], &[("title", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("note"), &["title"]);
        let cands = scan(&g, &inst);
        let got: Vec<&str> = cands.iter().map(|c| c.type_name.as_str()).collect();
        assert!(
            !got.contains(&"pane"),
            "abstract pane must not surface: {got:?}"
        );
        assert!(
            got.contains(&"pane.split"),
            "concrete subtype surfaces: {got:?}"
        );
    }

    #[test]
    fn sibling_leaf_surfaces_with_supersedes_marker() {
        // Instance claims `decision.pending`; `decision.decided` is also
        // a leaf of sealed `decision`. The scan surfaces it as a
        // candidate, with `supersedes: [decision.pending]` so consumers
        // know the suggested promotion requires dropping the current
        // leaf first (per [[type-instance type::au-type-system]]'s multi-leaf-in-sealed-family rule).
        let g = build_graph(vec![
            td_sealed("decision", &[], &["decision.pending", "decision.decided"]),
            td("decision.pending", &["decision"], &[("status", false)]),
            td("decision.decided", &["decision"], &[("status", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("decision.pending"), &["status"]);
        let cands = scan(&g, &inst);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].type_name.as_str(), "decision.decided");
        assert_eq!(
            cands[0]
                .supersedes
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>(),
            vec!["decision.pending"]
        );
    }

    #[test]
    fn candidate_in_disjoint_family_does_not_mark_supersedes() {
        // Instance claims `decision.pending` AND has fields satisfying
        // `tag-extra` (a non-sealed type in a different lineage).
        // `tag-extra` surfaces with empty supersedes — no sealed-family
        // conflict.
        let g = build_graph(vec![
            td_sealed("decision", &[], &["decision.pending"]),
            td("decision.pending", &["decision"], &[("status", false)]),
            td("tag-extra", &[], &[("status", false), ("audience", false)]),
        ])
        .graph;
        let inst = instance(bare_claim("decision.pending"), &["status", "audience"]);
        let cands = scan(&g, &inst);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].type_name.as_str(), "tag-extra");
        assert!(cands[0].supersedes.is_empty());
    }

    #[test]
    fn unclaimed_inline_at_union_slot_emits_nothing() {
        // [[type-def shape record::au-type-system]] cases 3–4: union / intersection / compound-reference
        // slots require an explicit `type:` claim — omitting it is a
        // validation error. The candidate walker stays advisory and
        // simply emits nothing at unresolvable inline scopes (so it
        // doesn't surface a type whose closure the inline might
        // actually be excluding once it gets a `type:`).
        let g = build_graph(vec![
            td("rationale-a", &[], &[("description", false)]),
            td(
                "rationale-b",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            // Would surface as candidate if closure were empty.
            td(
                "rationale-with-evidence",
                &[],
                &[("description", false), ("evidence", false)],
            ),
            td_typed(
                "host",
                &[],
                &[(
                    "rationale",
                    false,
                    Shape::Union(vec![
                        Shape::Record("rationale-a".into()),
                        Shape::Record("rationale-b".into()),
                    ]),
                )],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    None,
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("evidence", InstanceValue::String("e".into())),
                    ],
                )),
            )],
        );
        assert!(
            scan(&g, &inst).is_empty(),
            "candidate scan must stay quiet at an unresolvable inline scope; got {:?}",
            scan(&g, &inst)
        );
    }

    #[test]
    fn unclaimed_inline_at_missing_record_slot_emits_nothing() {
        // Slot demands `Record("rationale")` but `rationale` is not in
        // the type graph. Without a guard, `closure_of` returns
        // `{rationale}` — a name not in the graph — and `emit_candidates`'
        // "skip if in closure" filter trivially admits every other type.
        // Validate already fires a missing-type error at the slot; the
        // candidate scan must stay silent at this scope.
        let g = build_graph(vec![
            td("summary", &[], &[("description", false)]),
            td(
                "deliverable",
                &[],
                &[("description", false), ("audience", false)],
            ),
            td_typed(
                "host",
                &[],
                &[(
                    "rationale",
                    false,
                    Shape::Record("rationale".into()), // `rationale` is intentionally absent
                )],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    None,
                    vec![
                        field("description", InstanceValue::String("d".into())),
                        field("audience", InstanceValue::String("a".into())),
                    ],
                )),
            )],
        );
        let cands = scan(&g, &inst);
        assert!(
            cands.iter().all(|c| c.scope.inline_path != "/rationale"),
            "expected no candidates at /rationale (missing demanded type), got {:?}",
            cands
        );
    }

    #[test]
    fn typed_inline_nested_in_missing_record_slot_still_surfaces() {
        // Even when the host slot demands a missing type, a nested
        // inline that supplies its own `type:` must still surface
        // candidates for its scope — the missing-type guard only
        // affects the immediate scope, not the recursion.
        let g = build_graph(vec![
            td("evidence", &[], &[("kind", false)]),
            td(
                "evidence-with-source",
                &[],
                &[("kind", false), ("source", false)],
            ),
            td_typed(
                "host",
                &[],
                &[(
                    "rationale",
                    false,
                    Shape::Record("rationale".into()), // missing from graph
                )],
            ),
        ])
        .graph;
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    None,
                    vec![field(
                        "evidence",
                        InstanceValue::Mapping(inline(
                            Some("evidence"),
                            vec![
                                field("kind", InstanceValue::String("paper".into())),
                                field("source", InstanceValue::String("s".into())),
                            ],
                        )),
                    )],
                )),
            )],
        );
        assert_eq!(
            by_scope(&scan(&g, &inst)),
            vec![("evidence-with-source", "/rationale/evidence")]
        );
    }

    #[test]
    fn json_pointer_escaping_for_special_chars() {
        // RFC 6901 §3: `~` → `~0`, `/` → `~1`. Field names with these
        // characters are rare but must round-trip safely. `~`-first
        // ordering prevents the `~0` from being mis-encoded.
        let g = build_graph(vec![
            td("rationale", &[], &[("note", false)]),
            td("annotated", &[], &[("note", false), ("tag", false)]),
            td("host", &[], &[("rationale", false)]),
        ])
        .graph;
        // Field key contains both `~` and `/` to test the order:
        // `a~b/c` → `a~0b~1c`.
        let inst = instance_with(
            bare_claim("host"),
            vec![field(
                "rationale",
                InstanceValue::Mapping(inline(
                    Some("rationale"),
                    vec![
                        field("note", InstanceValue::String("n".into())),
                        field(
                            "a~b/c",
                            InstanceValue::Mapping(inline(
                                Some("rationale"),
                                vec![
                                    field("note", InstanceValue::String("n2".into())),
                                    field("tag", InstanceValue::String("t".into())),
                                ],
                            )),
                        ),
                    ],
                )),
            )],
        );
        let cands = scan(&g, &inst);
        // `annotated` should surface at the inner scope `/rationale/a~0b~1c`.
        assert_eq!(by_scope(&cands), vec![("annotated", "/rationale/a~0b~1c")]);
    }

    #[test]
    fn deep_recursion_terminates() {
        // Three levels of mapping nesting plus a sequence rung; no
        // matching candidate anywhere, but the walker must complete.
        let g = build_graph(vec![td("rationale", &[], &[("description", false)])]).graph;
        let inst = instance_with(
            bare_claim("rationale"),
            vec![
                field("description", InstanceValue::String("d".into())),
                field(
                    "a",
                    InstanceValue::Mapping(inline(
                        Some("rationale"),
                        vec![
                            field("description", InstanceValue::String("d2".into())),
                            field(
                                "b",
                                seq(vec![InstanceValue::Mapping(inline(
                                    Some("rationale"),
                                    vec![
                                        field("description", InstanceValue::String("d3".into())),
                                        field(
                                            "c",
                                            InstanceValue::Mapping(inline(
                                                Some("rationale"),
                                                vec![field(
                                                    "description",
                                                    InstanceValue::String("d4".into()),
                                                )],
                                            )),
                                        ),
                                    ],
                                ))]),
                            ),
                        ],
                    )),
                ),
            ],
        );
        let cands = scan(&g, &inst);
        assert!(cands.is_empty());
    }
}
