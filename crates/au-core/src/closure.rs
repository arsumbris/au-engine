//! Closure walks and effective-shape computation.
//!
//! `closure_of(graph, name)` returns the set of type-defs reachable from
//! `name` through `type:` parent chains (the inheritance closure). The
//! validator + load checks both walk it.
//!
//! `effective_shape(graph, claim)` is the higher-level API instances care
//! about: union of fields contributed by every type-def in the closure of a
//! `TypeClaim`, with each field tagged by its originating type-def(s). Multi-
//! claim mixin (`type: [a, b, ...]`) unions the closures of each claim and
//! groups field decls by name; auto-unify ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) collapses same-named fields
//! whose `parsed_shape` is token-equal across origins, otherwise the field is
//! DIVERGENT: it stays in the effective shape as a per-origin `FieldOrigin`
//! (in `EffectiveShape.divergent`), and every use of it must be qualified
//! (`field{type}`). A bare use is the `mixin-collision` diagnostic, emitted by
//! the validator, not read from a list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_grammar::{DefBound, Shape};

use crate::graph::TypeGraph;
use crate::instance::TypeClaim;
use crate::typedef::{FieldDecl, FieldName, TypeName};

/// Compute the closure of `name`: `name` itself plus every transitive
/// ancestor reachable through `type:` parent claims. Cycle-safe via the
/// visited set; the result is a sorted `BTreeSet` for deterministic
/// iteration.
pub fn closure_of(graph: &TypeGraph, name: &TypeName) -> BTreeSet<TypeName> {
    let mut visited = BTreeSet::new();
    let mut stack = vec![name.clone()];
    while let Some(n) = stack.pop() {
        if !visited.insert(n.clone()) {
            continue;
        }
        if let Some(td) = graph.get(&n) {
            for p in &td.parents {
                // A `::repo` parent is a peer type, resolved by the cross-repo
                // fold, not present in this own graph. Skip it here; the engine
                // gates the peer reference.
                if p.is_qualified() {
                    continue;
                }
                stack.push(p.name.clone());
            }
        }
    }
    visited
}

/// Compute the REFERENCED closure of `name`: `name`, every transitive
/// ancestor (the parent axis `closure_of` walks), AND every transitively
/// field-referenced type-def. Where `closure_of` follows only `type:` parent
/// chains, this also descends each member's field shapes into the type-defs
/// they name — record, reference, inline-or-reference, list/pinned inners,
/// compound operands, and def-ref ceilings (see [`collect_referenced_type_names`]).
/// Cycle-safe via the visited set; the referenced graph can cycle (A's field
/// refs B, B's field refs A) and the walk halts on revisit. Result is a sorted
/// `BTreeSet` for deterministic iteration.
///
/// This is the closure cross-repo reference satisfaction must compare. The
/// def-local canonical hash encodes a field's type only as its rendered NAME
/// token, never the field type's content, so a same-named field type that
/// DIVERGES in content across repos is invisible to a parent-only walk.
/// Including the referenced types surfaces that divergence.
pub fn referenced_closure_of(graph: &TypeGraph, name: &TypeName) -> BTreeSet<TypeName> {
    let mut visited = BTreeSet::new();
    let mut stack = vec![name.clone()];
    while let Some(n) = stack.pop() {
        if !visited.insert(n.clone()) {
            continue;
        }
        if let Some(td) = graph.get(&n) {
            // Parent axis, same as `closure_of`. A `::repo` parent resolves via
            // the cross-repo fold, not this own graph; skip it.
            for p in &td.parents {
                if p.is_qualified() {
                    continue;
                }
                stack.push(p.name.clone());
            }
            // Field axis: every type-def a field shape names. An `Err`
            // parsed_shape references nothing checkable; the load-time
            // shape-syntax-error owns it.
            for f in &td.fields {
                if let Ok(shape) = &f.parsed_shape {
                    for ref_name in collect_referenced_type_names(shape) {
                        stack.push(TypeName(ref_name.to_string()));
                    }
                }
            }
        }
    }
    visited
}

/// The type-def names a single field [`Shape`] references — the field axis of
/// the referenced closure. Covers all three single-name forms (`name*`, bare
/// `name`, `name&`), list/pinned inners, union/intersection branches, compound
/// reference branches, and def-ref ceilings. Primitives, enums, and bare `any`
/// name no graph type-def. The reserved `file` / `any` reference sentinels are
/// returned verbatim; they resolve to no type-def, so a closure walk simply
/// drops them (symmetric across repos, never a false divergence).
///
/// Returns borrowed `&str` tied to `shape`; callers clone into owned names as
/// needed. Both the load-time `slot-references-absent-type` check and
/// [`referenced_closure_of`] descend the same set, so the two stay in lockstep.
///
/// Only OWN (unqualified) names are collected. A `::repo`-qualified shape ref
/// (`foo::repo*`) names a peer type resolved by the cross-repo fold, not this
/// own graph; collecting its base would false-fire `slot-references-absent-type`
/// and pull a phantom name into the own-graph closure. The engine gates the
/// peer reference; the fold resolves it.
pub(crate) fn collect_referenced_type_names(shape: &Shape) -> Vec<&str> {
    fn push_own<'a>(name: &'a au_grammar::QualifiedName, out: &mut Vec<&'a str>) {
        if name.repo.is_none() {
            out.push(name.as_str());
        }
    }
    fn walk<'a>(shape: &'a Shape, out: &mut Vec<&'a str>) {
        match shape {
            // All three single-name reference forms — `name*`, bare `name`,
            // and `name&` — point at the same type-def name in the graph.
            Shape::Reference(name) | Shape::Record(name) | Shape::InlineOrReference(name) => {
                push_own(name, out)
            }
            Shape::List { inner, .. } => walk(inner, out),
            // A commit-pinned reference ([[type-def shape suffixes::au-type-system]] `*@`) wraps a
            // reference; its bound type names must exist, so `T*@` / `type<absent>*@`
            // descend through the inner.
            Shape::Pinned(inner) => walk(inner, out),
            Shape::Union(branches) | Shape::Intersection(branches) => {
                for branch in branches {
                    walk(branch, out);
                }
            }
            Shape::CompoundReference { branches, .. } => {
                for name in branches {
                    push_own(name, out);
                }
            }
            // A def-reference ([[type-def shape def-ref::au-type-system]], `type<T>*`) constrains the
            // target to a def under `T`. The bound's ceiling names are type-defs
            // that must exist. The unconstrained `type*` (None) names nothing.
            Shape::DefReference(bound) => match bound {
                Some(DefBound::Single(name)) => push_own(name, out),
                Some(DefBound::Compound { branches, .. }) => {
                    for name in branches {
                        push_own(name, out);
                    }
                }
                None => {}
            },
            // A tuple's element shapes may name type-defs (records) that must
            // exist, so descend into each.
            Shape::Tuple(elements) => {
                for el in elements {
                    walk(el, out);
                }
            }
            // Bare `Shape::Any` references no graph type-def. The reference
            // form is `Shape::Reference("any")`, handled by the arm above.
            Shape::Primitive(_)
            | Shape::Enum(_)
            | Shape::Any
            | Shape::Opaque
            | Shape::Refined { .. } => {}
        }
    }
    let mut names = Vec::new();
    walk(shape, &mut names);
    names
}

/// Every type-def name a shape references, INCLUDING `::repo`-qualified peer
/// refs, each as `(base, repo)`. Unlike [`collect_referenced_type_names`], which
/// collects own names only for the closure hash, this keeps qualified refs so a
/// cross-repo graph projection can resolve a peer field target. Order is shape
/// order; duplicates are kept (the caller dedups per its needs).
fn collect_referenced_qualified_names(shape: &Shape) -> Vec<(&str, Option<&str>)> {
    fn push<'a>(name: &'a au_grammar::QualifiedName, out: &mut Vec<(&'a str, Option<&'a str>)>) {
        out.push((name.as_str(), name.repo.as_deref()));
    }
    fn walk<'a>(shape: &'a Shape, out: &mut Vec<(&'a str, Option<&'a str>)>) {
        match shape {
            Shape::Reference(name) | Shape::Record(name) | Shape::InlineOrReference(name) => {
                push(name, out)
            }
            Shape::List { inner, .. } => walk(inner, out),
            Shape::Pinned(inner) => walk(inner, out),
            Shape::Union(branches) | Shape::Intersection(branches) => {
                for branch in branches {
                    walk(branch, out);
                }
            }
            Shape::CompoundReference { branches, .. } => {
                for name in branches {
                    push(name, out);
                }
            }
            Shape::DefReference(bound) => match bound {
                Some(DefBound::Single(name)) => push(name, out),
                Some(DefBound::Compound { branches, .. }) => {
                    for name in branches {
                        push(name, out);
                    }
                }
                None => {}
            },
            Shape::Tuple(elements) => {
                for el in elements {
                    walk(el, out);
                }
            }
            Shape::Primitive(_)
            | Shape::Enum(_)
            | Shape::Any
            | Shape::Opaque
            | Shape::Refined { .. } => {}
        }
    }
    let mut names = Vec::new();
    walk(shape, &mut names);
    names
}

/// The `(base, repo)` type-def references of a def's FIELD SHAPES, the
/// `field-type` edges of the type-graph projection. Includes `::repo` peer
/// refs, so a cross-repo projection resolves them; a shape that failed to parse
/// contributes nothing. Order is field-declaration order with per-field
/// duplicates preserved, so the caller controls dedup and multiplicity.
pub fn field_type_refs(td: &crate::typedef::TypeDef) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for f in &td.fields {
        if let Ok(shape) = &f.parsed_shape {
            for (base, repo) in collect_referenced_qualified_names(shape) {
                out.push((base.to_string(), repo.map(str::to_string)));
            }
        }
    }
    out
}

/// The identity of a field origin: its authored form, `name` for an own def or
/// `name::repo` for a folded peer. This is the map key for `FieldOrigin.origins`.
///
/// LOAD-BEARING as a newtype, not a bare `TypeName`: a divergent field can reach
/// two DISTINCT identities of one type name (own `note` plus peer `note::base`,
/// or two peers), which must key apart. A `TypeName` key would collapse them and
/// silently drop one origin. The bare half lives in `OriginInfo::type_name` for
/// callers doing own-graph lookups (closure hashing, `provided` slots).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OriginId(pub String);

impl OriginId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One declaration contributed by a single type-def in the closure.
///
/// The originating type-def is the one whose own `fields:` list literally
/// includes this name. With width-only subtyping ([[type subtyping width-only::au-type-system]]) and the no-redeclare
/// rule, each chain has at most one origin per field name; mixin can produce
/// multiple chains and therefore multiple origins for the same field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginInfo {
    pub decl: FieldDecl,
    pub origin_path: PathBuf,
    /// The origin's BARE type-def name. The authored form (with any `::repo`)
    /// is the `OriginId` key in `FieldOrigin.origins`; this is the bare half,
    /// for callers that resolve against the own graph (closure hashing) or key
    /// `provided` slots by bare name.
    pub type_name: TypeName,
}

/// One entry in `EffectiveShape.fields`. Records every contributing origin
/// (length 1 for single-claim or 1-element-list; ≥ 2 when mixin auto-unify
/// kicks in). Per-origin `decl.optional` is preserved so the per-originator
/// required-field check ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) sees each origin's own optional bit, not a
/// collapsed canonical one.
///
/// Map key (`OriginId`, the authored `name` / `name::repo`) is the originating
/// type-def's identity, so two same-named cross-repo origins stay distinct.
/// `BTreeMap` ordering gives deterministic canonical-origin selection across runs.
///
/// `origins` is sealed at the crate boundary so the "non-empty" invariant
/// upheld by `effective_shape` can't be broken from outside au-core;
/// `canonical()` and its delegates rely on it. External readers iterate
/// via [`FieldOrigin::origins`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOrigin {
    pub(crate) origins: BTreeMap<OriginId, OriginInfo>,
}

impl FieldOrigin {
    /// Iterate `(origin_id, origin_info)` pairs in origin-identity order.
    pub fn origins(&self) -> impl Iterator<Item = (&OriginId, &OriginInfo)> + '_ {
        self.origins.iter()
    }

    /// The lex-smallest origin (by `OriginId`) and its info. Used for
    /// diagnostics that need a single canonical site when per-origin
    /// distinction isn't relevant. A DIVERGENT field has no single canonical
    /// shape — callers must gate on `is_divergent()` before trusting
    /// `canonical_decl()`, and take a per-origin path otherwise.
    pub fn canonical(&self) -> (&OriginId, &OriginInfo) {
        self.origins
            .iter()
            .next()
            .expect("FieldOrigin invariant: origins is non-empty")
    }

    /// Canonical decl for shape validation. Valid to use only when the field is
    /// NOT divergent (token-equal across all origins, auto-unify rule), so any
    /// one works; `canonical()` picks the lex-smallest origin's decl for
    /// determinism. On a divergent field this is an arbitrary origin's shape —
    /// gate on `is_divergent()` first.
    pub fn canonical_decl(&self) -> &FieldDecl {
        &self.canonical().1.decl
    }

    /// Canonical origin's BARE name (lex-smallest origin by `OriginId`).
    pub fn canonical_origin(&self) -> &TypeName {
        &self.canonical().1.type_name
    }

    /// Canonical origin's source path.
    pub fn canonical_origin_path(&self) -> &Path {
        &self.canonical().1.origin_path
    }

    /// True iff at least one origin marks the field as required. Required
    /// at any origin makes the field required for the whole effective
    /// shape — auto-unify only collapses *shapes*, not optional-ness.
    pub fn is_required(&self) -> bool {
        self.origins.values().any(|info| !info.decl.optional)
    }

    /// True iff the origins do NOT all share a token-equal shape — a divergent
    /// field ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). A single-origin field is never divergent. A
    /// divergent field has no all-satisfying bare value: every use must be
    /// qualified, and a bare use is a `mixin-collision`.
    pub fn is_divergent(&self) -> bool {
        let mut iter = self.origins.values();
        let Some(first) = iter.next() else {
            return false;
        };
        !iter.all(|info| shape_token_equal(&info.decl, &first.decl))
    }
}

/// The fields an instance of a given `TypeClaim` is contractually expected
/// to honor, split into resolved and divergent. Built by walking each claim's
/// closure and grouping field decls by name.
///
/// `fields` holds the resolved entries — single-origin or auto-unified.
/// `divergent` holds every same-named field whose origins disagree on shape,
/// as a per-origin `FieldOrigin` keyed by identity ([[type-def fields collision - auto-unify and qualified field::au-type-system]]): the field
/// stays in the effective shape (every use must be qualified), it is not
/// dropped. `iter()` / `get()` expose `fields`; `divergent()` / `get_divergent()`
/// expose the divergent set. The validator emits `mixin-collision` on a bare use
/// of a divergent field and enforces its origins' required bits per-origin.
/// `instance_closure` is the union of `closure_of(c)` across every claim,
/// used by the [[type-def fields collision - auto-unify and qualified field::au-type-system]] qualifier resolver to validate `field{T}` keys.
///
/// Keep every field deterministically ordered (`BTreeMap` / `BTreeSet`, never a
/// `HashMap` / `HashSet`): the scale fuzzer `Debug`-diffs this shape to assert
/// incremental-vs-full identity, so a non-ordered container here would make that
/// diff nondeterministic (a spurious flake, or a masked real divergence).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveShape {
    fields: BTreeMap<FieldName, FieldOrigin>,
    divergent: BTreeMap<FieldName, FieldOrigin>,
    instance_closure: BTreeSet<TypeName>,
}

impl EffectiveShape {
    /// Look up a resolved field by name. Returns `None` for a divergent field
    /// (present in `divergent`, not here) as well as an absent one.
    pub fn get(&self, name: &FieldName) -> Option<&FieldOrigin> {
        self.fields.get(name)
    }

    /// Iterate `(name, origin)` pairs over resolved fields in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&FieldName, &FieldOrigin)> {
        self.fields.iter()
    }

    /// Look up a divergent field by name. The per-origin `FieldOrigin` a
    /// qualified use resolves against; `None` for a resolved or absent field.
    pub fn get_divergent(&self, name: &FieldName) -> Option<&FieldOrigin> {
        self.divergent.get(name)
    }

    /// Iterate `(name, origin)` pairs over divergent fields in name order.
    pub fn divergent(&self) -> impl Iterator<Item = (&FieldName, &FieldOrigin)> {
        self.divergent.iter()
    }

    /// Number of resolved (non-divergent) fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// True if no resolved fields are contributed.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Re-qualify every field declaration's shape to `repo` (see
    /// [`FieldDecl::qualified_to`]). Used on the OWNER's effective shape when a
    /// delegated inline value (`foo::repo&` with no inline `type:`) is validated
    /// from a CONSUMER repo: the owner's own field names are bare in the owner's
    /// graph, but from the consumer they name the owner's types, so their shapes
    /// must resolve against `repo`. `instance_closure` (type-name identity, not a
    /// resolvable shape) is unaffected; only field SHAPES carry resolvable names.
    pub fn qualified_to(&self, repo: &str) -> EffectiveShape {
        let requalify_decl = |d: &FieldDecl| d.qualified_to(repo);
        let requalify_map = |src: &BTreeMap<FieldName, FieldOrigin>| {
            src.iter()
                .map(|(name, fo)| {
                    let origins = fo
                        .origins
                        .iter()
                        .map(|(id, info)| {
                            (
                                id.clone(),
                                OriginInfo {
                                    decl: requalify_decl(&info.decl),
                                    origin_path: info.origin_path.clone(),
                                    type_name: info.type_name.clone(),
                                },
                            )
                        })
                        .collect();
                    (name.clone(), FieldOrigin { origins })
                })
                .collect()
        };
        EffectiveShape {
            fields: requalify_map(&self.fields),
            divergent: requalify_map(&self.divergent),
            instance_closure: self.instance_closure.clone(),
        }
    }

    /// Union of `closure_of(c)` across every claim. Includes the claims
    /// themselves and all transitive ancestors. Used by the [[type-def fields collision - auto-unify and qualified field::au-type-system]] qualifier
    /// resolver to verify `T` in `field{T}` is reachable from the instance.
    pub fn instance_closure(&self) -> &BTreeSet<TypeName> {
        &self.instance_closure
    }
}

/// Why `effective_shape` couldn't produce an `EffectiveShape`. Divergent fields
/// are NOT errors here — they stay in the returned shape's `divergent` set so the
/// validator can emit `mixin-collision` on a bare use while still validating the
/// rest of the instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveShapeError {
    /// A claimed type name is not present in the type-graph.
    UnknownType(TypeName),
}

/// Compute the effective shape for a `TypeClaim`. Bare scalars and
/// 1-element list claims walk a single closure (fields appear with
/// exactly one origin, no collisions possible). Multi-claim lists union
/// every claim's closure into a deduped set, group field decls by name,
/// and either auto-unify (token-equal `parsed_shape` across origins → one
/// resolved `FieldOrigin` in `fields`) or keep a divergent per-origin
/// `FieldOrigin` in `divergent` (non-token-equal). An empty list is a parse-time
/// error and never reaches this function (see `parse_instance` / `parse_type_def`).
pub fn effective_shape(
    graph: &TypeGraph,
    claim: &TypeClaim,
) -> Result<EffectiveShape, EffectiveShapeError> {
    // Verify each claimed name is in the graph; bail on the first missing.
    // Empty list is structurally rejected at parse time, so `claim.iter()`
    // is non-empty here. A `::repo`-qualified claim is a peer type resolved by
    // the cross-repo fold, absent from this own graph by design, so it is
    // skipped here (the engine gates its peer reference) rather than reported
    // as an unknown type.
    for c in claim.iter() {
        if c.is_qualified() {
            continue;
        }
        if !graph.contains(&c.name) {
            return Err(EffectiveShapeError::UnknownType(c.name.clone()));
        }
    }

    // Union of closures across every claim. BTreeSet auto-dedupes and
    // gives deterministic iteration regardless of claim order. Qualified
    // claims contribute nothing to the own-graph closure until the fold.
    let mut instance_closure: BTreeSet<TypeName> = BTreeSet::new();
    for c in claim.iter() {
        if c.is_qualified() {
            continue;
        }
        instance_closure.extend(closure_of(graph, &c.name));
    }

    // Group every (origin, decl) by field name. A name with multiple
    // entries is either auto-unified (all decls token-equal) or a
    // collision. The `BTreeMap` keying gives sorted iteration so collision
    // diagnostics are deterministic across permuted claim orders.
    let mut by_name: BTreeMap<FieldName, BTreeMap<OriginId, OriginInfo>> = BTreeMap::new();
    for type_name in &instance_closure {
        let Some(td) = graph.get(type_name) else {
            continue;
        };
        for f in &td.fields {
            by_name.entry(f.name.clone()).or_default().insert(
                // Own graph: names are bare, so the OriginId equals the bare name.
                OriginId(type_name.as_str().to_string()),
                OriginInfo {
                    decl: f.clone(),
                    origin_path: td.source_path.clone(),
                    type_name: type_name.clone(),
                },
            );
        }
    }

    Ok(group_fields(instance_closure, by_name))
}

/// Group `(origin_id, decl)` contributions by field name into the effective
/// shape: each name either auto-unifies (its origins' shapes token-equal) into a
/// resolved `FieldOrigin` in `fields`, or is DIVERGENT (non-token-equal) and
/// stays as a per-origin `FieldOrigin` in `divergent` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) — kept,
/// never dropped. Shared by the own-graph [`effective_shape`] and the
/// resolution-graph [`effective_shape_resolved`], so the two produce identical
/// grouping from identical contributions.
fn group_fields(
    instance_closure: BTreeSet<TypeName>,
    by_name: BTreeMap<FieldName, BTreeMap<OriginId, OriginInfo>>,
) -> EffectiveShape {
    let mut fields: BTreeMap<FieldName, FieldOrigin> = BTreeMap::new();
    let mut divergent: BTreeMap<FieldName, FieldOrigin> = BTreeMap::new();

    for (field_name, origins) in by_name {
        if shapes_token_equal_across_origins(&origins) {
            fields.insert(field_name, FieldOrigin { origins });
        } else {
            divergent.insert(field_name, FieldOrigin { origins });
        }
    }

    EffectiveShape {
        fields,
        divergent,
        instance_closure,
    }
}

/// The parent-closure `TypeId`s a claim reaches over a resolution graph, the
/// closure-walk half of [`effective_shape_resolved`] exposed for the cross-repo
/// reference seam.
///
/// Each claim resolves via the resolution edge, then its parent-`TypeId` closure
/// is gathered by the same walk `effective_shape_resolved` uses. A claim that
/// does not resolve (an unresolvable peer the gate owns, or a bare name absent
/// here) contributes nothing. No `own_graph` is needed for the id set: the fold
/// seeds every own def, so a bare own claim resolves through the edge like any
/// other. Used to answer a qualified DEMAND (`foo::repo*`): the demanded `TypeId`
/// is a member of the target's folded closure iff the target satisfies it.
pub fn folded_closure_ids(
    rg: &crate::resolution::ResolutionGraph,
    claim: &TypeClaim,
) -> BTreeSet<crate::resolution::TypeId> {
    use crate::resolution::TypeId;

    let mut closure: BTreeSet<TypeId> = BTreeSet::new();
    for c in claim.iter() {
        let Some(entry) = rg.resolve_authored(&c.name, c.repo.as_deref()) else {
            continue;
        };
        let mut stack = vec![entry.clone()];
        while let Some(tid) = stack.pop() {
            if !closure.insert(tid.clone()) {
                continue;
            }
            if let Some(node) = rg.get(&tid) {
                for p in &node.parents {
                    stack.push(p.clone());
                }
            }
        }
    }
    closure
}

/// Effective shape for a `TypeClaim` over a cross-repo RESOLUTION graph, the
/// import-aware sibling of [`effective_shape`]. Where `effective_shape` walks a
/// single name-keyed [`TypeGraph`] and DEFERS a `::repo` claim, this resolves
/// each claim (bare or `::repo`) to a folded node via the resolution edge, walks
/// its parent-`TypeId` closure, and gathers fields through the same
/// [`group_fields`] core. So `type: foo::repo`, and an own subtype whose parent
/// is `parent::repo`, both gather the folded peer fields.
///
/// `own_graph` is R's own graph, used only to tell an absent OWN claim (an
/// `UnknownType` error, mirroring the own-graph path) from an unresolvable
/// qualified claim (deferred, the engine's gate owns its diagnostic).
///
/// Origins are keyed by authored `OriginId`. A mixin reaching two same-named but
/// DIVERGED identities (the cross-repo diamond) keeps both as a divergent
/// `FieldOrigin` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]); a token-equal same-name group collapses to
/// one representative, so a purely-resolved shape matches the own-graph path.
pub fn effective_shape_resolved(
    rg: &crate::resolution::ResolutionGraph,
    own_graph: &TypeGraph,
    claim: &TypeClaim,
) -> Result<EffectiveShape, EffectiveShapeError> {
    // A bare OWN claim absent everywhere is an `UnknownType` error, mirroring the
    // own-graph path; an unresolvable qualified claim defers (the engine's gate
    // owns its diagnostic). The folded closure itself is the shared walk below.
    for c in claim.iter() {
        if c.repo.is_none()
            && rg.resolve_authored(&c.name, None).is_none()
            && !own_graph.contains(&c.name)
        {
            return Err(EffectiveShapeError::UnknownType(c.name.clone()));
        }
    }

    let closure = folded_closure_ids(rg, claim);

    // Gather per-field contributions grouped by BARE name, each entry carrying its
    // authored `OriginId`: a mixin can reach two distinct `TypeId`s sharing a name
    // (own+peer, or two peers), and a bare-name-only map would silently overwrite
    // one. The inner vec holds every same-named identity's decl so a divergence
    // stays visible.
    let mut instance_closure: BTreeSet<TypeName> = BTreeSet::new();
    let mut by_name_ids: BTreeMap<FieldName, BTreeMap<TypeName, Vec<(OriginId, OriginInfo)>>> =
        BTreeMap::new();
    for tid in &closure {
        let Some(node) = rg.get(tid) else { continue };
        instance_closure.insert(tid.name.clone());
        let origin_id = match node.origin.as_deref() {
            Some(repo) => OriginId(format!("{}::{}", tid.name.as_str(), repo)),
            None => OriginId(tid.name.as_str().to_string()),
        };
        for f in &node.fields {
            by_name_ids
                .entry(f.name.clone())
                .or_default()
                .entry(tid.name.clone())
                .or_default()
                .push((
                    origin_id.clone(),
                    OriginInfo {
                        decl: f.clone(),
                        origin_path: node.source_path.clone(),
                        type_name: tid.name.clone(),
                    },
                ));
        }
    }

    // Resolve each same-named identity group per field. A group whose decls are
    // not all token-equal is the cross-repo diamond: its identities become a
    // DIVERGENT `FieldOrigin` (kept, keyed by authored `OriginId` so `note` and
    // `note::base` stay distinct). A singleton or token-equal group collapses to
    // one representative (lex-min authored form) so the `group_fields` core sees
    // the same contributions it always has, handling cross-name divergence +
    // auto-unify.
    let mut by_name: BTreeMap<FieldName, BTreeMap<OriginId, OriginInfo>> = BTreeMap::new();
    let mut same_name_divergent: BTreeMap<FieldName, FieldOrigin> = BTreeMap::new();
    for (field_name, name_groups) in by_name_ids {
        let mut resolved: BTreeMap<OriginId, OriginInfo> = BTreeMap::new();
        let mut divergent_origins: BTreeMap<OriginId, OriginInfo> = BTreeMap::new();
        for (_bare, entries) in name_groups {
            let diverges = entries.len() >= 2 && {
                let first = &entries[0].1.decl;
                !entries
                    .iter()
                    .all(|(_, o)| shape_token_equal(&o.decl, first))
            };
            if diverges {
                divergent_origins.extend(entries);
                continue;
            }
            // Singleton or token-equal: one representative, lex-min authored form.
            let rep = entries.into_iter().min_by(|a, b| a.0.cmp(&b.0)).unwrap();
            resolved.insert(rep.0, rep.1);
        }
        if !divergent_origins.is_empty() {
            // The field is divergent. Fold the NON-diverging origins (a same-named
            // token-equal group, or a differently-named origin like a mixed-in
            // `deliverable.title`) into the divergent set too, so EVERY origin of
            // the field is present and independently required ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
            // Dropping them would silently lose an origin's `required-field-absent`.
            // Keys are disjoint (distinct authored `OriginId`s), so the merge is safe.
            divergent_origins.extend(resolved);
            same_name_divergent.insert(
                field_name,
                FieldOrigin {
                    origins: divergent_origins,
                },
            );
        } else {
            by_name.insert(field_name, resolved);
        }
    }

    let mut shape = group_fields(instance_closure, by_name);
    // Fold the same-name divergent fields into `divergent`; `group_fields` never
    // produced them (they were excluded from `by_name`), so the maps are disjoint.
    shape.divergent.extend(same_name_divergent);
    Ok(shape)
}

/// Auto-unify check (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). Single-origin trivially passes. For `Ok`
/// shapes, equality is structural (`Shape` derives `PartialEq`). For `Err`
/// shapes (deferred grammar features, syntax errors), equality is on
/// `raw_shape` — the parser is deterministic, so identical source text
/// produces equivalent Errs even though their carrier diagnostics carry
/// distinct spans.
fn shapes_token_equal_across_origins(origins: &BTreeMap<OriginId, OriginInfo>) -> bool {
    let mut iter = origins.values();
    let Some(first) = iter.next() else {
        return true;
    };
    iter.all(|info| shape_token_equal(&info.decl, &first.decl))
}

fn shape_token_equal(a: &FieldDecl, b: &FieldDecl) -> bool {
    match (&a.parsed_shape, &b.parsed_shape) {
        (Ok(sa), Ok(sb)) => sa == sb,
        (Err(_), Err(_)) => a.raw_shape == b.raw_shape,
        _ => false,
    }
}

/// One origin in a qualifier's closure that declares the qualified field,
/// gathered from either the own graph or the folded resolution graph for
/// [`resolve_qualifier_candidates`].
pub(crate) struct QualifierCandidate {
    pub origin: OriginId,
    pub path: PathBuf,
    pub decl: FieldDecl,
}

/// Whether a `field{type}` qualifier resolves to ONE declaration.
pub(crate) enum QualifierResolution {
    /// A single shape is reached — one declaring origin, or several that are
    /// token-equal (an auto-unified field). The chosen candidate.
    Unique(QualifierCandidate),
    /// The qualifier's closure declares the field at two or more NON-token-equal
    /// origins: a divergent field reached through a non-declaring descendant, so
    /// which shape the value checks against is an arbitrary choice. Carries one
    /// representative origin per distinct shape, to steer the author to name a
    /// declaring origin directly ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
    Ambiguous(Vec<OriginId>),
    /// No type in the qualifier's closure declares the field.
    Absent,
}

/// Group declaring candidates by token-equal shape and decide whether a
/// qualifier resolves to one declaration. The single shared home for the
/// question across surfaces (frontmatter/inline key, body contribution) and
/// across graphs (own, folded resolution), so they cannot disagree on when a
/// qualifier is ambiguous. Candidates are passed in the caller's deterministic
/// closure order, so the `Unique` pick is stable.
pub(crate) fn resolve_qualifier_candidates(
    candidates: Vec<QualifierCandidate>,
) -> QualifierResolution {
    if candidates.is_empty() {
        return QualifierResolution::Absent;
    }
    // One representative per distinct (token-equal) shape, first-seen order. A
    // divergent field reached through a descendant yields two or more.
    let mut reps: Vec<&QualifierCandidate> = Vec::new();
    for c in &candidates {
        if !reps.iter().any(|r| shape_token_equal(&r.decl, &c.decl)) {
            reps.push(c);
        }
    }
    if reps.len() > 1 {
        return QualifierResolution::Ambiguous(reps.iter().map(|r| r.origin.clone()).collect());
    }
    // A single shape (possibly reached via several token-equal origins): the
    // first candidate in closure order.
    QualifierResolution::Unique(candidates.into_iter().next().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::instance::TypeClaim;
    use crate::typedef::{FieldDecl, FieldName, TypeDef, TypeName, TypeNameClaim};
    use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span};
    use au_grammar::{DefBound, Primitive, Shape};
    use std::path::PathBuf;

    fn td(name: &str, parents: &[&str], fields: &[&str]) -> TypeDef {
        td_with_shapes(
            name,
            parents,
            &fields
                .iter()
                .map(|n| (*n, Shape::Primitive(Primitive::String)))
                .collect::<Vec<_>>(),
        )
    }

    fn td_with_shapes(name: &str, parents: &[&str], fields: &[(&str, Shape)]) -> TypeDef {
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
                .map(|(n, shape)| FieldDecl {
                    name: FieldName((*n).into()),
                    optional: false,
                    raw_shape: "String".into(),
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

    fn bare(name: &str) -> TypeClaim {
        TypeClaim::Bare(TypeNameClaim::own(
            TypeName(name.into()),
            ByteRange::new(0, 0),
        ))
    }

    fn list(names: &[&str]) -> TypeClaim {
        TypeClaim::List {
            items: names
                .iter()
                .map(|n| TypeNameClaim::own(TypeName((*n).into()), ByteRange::new(0, 0)))
                .collect(),
            value_span: ByteRange::new(0, 0),
        }
    }

    #[test]
    fn effective_shape_defers_a_qualified_claim() {
        // A `::repo` claim is skipped, not reported as UnknownType — the
        // cross-repo fold resolves it. An instance claiming only a peer type
        // yields an empty own-graph shape here rather than an error.
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let claim = TypeClaim::Bare(TypeNameClaim::parse("absent::peer", ByteRange::new(0, 0)));
        let shape = effective_shape(&g, &claim);
        assert!(
            shape.is_ok(),
            "a qualified claim should defer, not error: {shape:?}"
        );
    }

    #[test]
    fn closure_skips_a_qualified_parent() {
        // A type-def with a `::repo` parent: the own-graph closure does not
        // chase the peer parent, so the closure is just the type itself.
        let mut child = td("child", &[], &[]);
        child.parents = vec![TypeNameClaim::parse("base::peer", ByteRange::new(0, 0))];
        let g = build_graph(vec![child]).graph;
        let c = closure_of(&g, &TypeName("child".into()));
        assert_eq!(c.len(), 1);
        assert!(c.contains(&TypeName("child".into())));
    }

    #[test]
    fn closure_of_root_is_just_self() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let c = closure_of(&g, &TypeName("note".into()));
        assert_eq!(c.len(), 1);
        assert!(c.contains(&TypeName("note".into())));
    }

    #[test]
    fn closure_of_walks_transitive_parents() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("decision", &["note"], &[]),
            td("decision.decided", &["decision"], &[]),
        ])
        .graph;
        let c = closure_of(&g, &TypeName("decision.decided".into()));
        let names: Vec<_> = c.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["decision", "decision.decided", "note"]);
    }

    #[test]
    fn closure_of_terminates_on_self_loop() {
        let g = build_graph(vec![td("loop", &["loop"], &[])]).graph;
        let c = closure_of(&g, &TypeName("loop".into()));
        // Doesn't blow up; the visited-set guard halts the walk.
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn effective_shape_unions_own_and_ancestor_fields() {
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("decision", &["note"], &["decided_by"]),
            td("decision.decided", &["decision"], &["decided_at"]),
        ])
        .graph;
        let shape = effective_shape(&g, &bare("decision.decided")).unwrap();
        assert_eq!(shape.len(), 3);

        // Each field tagged by its originating type-def (single origin per
        // field in single-claim closure walks).
        let by_origin: Vec<(&str, &str)> = shape
            .iter()
            .map(|(k, v)| (k.as_str(), v.canonical_origin().as_str()))
            .collect();
        assert_eq!(
            by_origin,
            vec![
                ("decided_at", "decision.decided"),
                ("decided_by", "decision"),
                ("description", "note"),
            ]
        );
    }

    #[test]
    fn effective_shape_for_one_element_list_matches_bare() {
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("decision", &["note"], &["decided_by"]),
        ])
        .graph;
        let bare_shape = effective_shape(&g, &bare("decision")).unwrap();
        let list_shape = effective_shape(&g, &list(&["decision"])).unwrap();
        assert_eq!(bare_shape, list_shape);
    }

    #[test]
    fn effective_shape_returns_unknown_type_for_absent_claim() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let err = effective_shape(&g, &bare("missing")).unwrap_err();
        assert_eq!(
            err,
            EffectiveShapeError::UnknownType(TypeName("missing".into()))
        );
    }

    #[test]
    fn effective_shape_get_finds_field_by_name() {
        let g = build_graph(vec![td("note", &[], &["description"])]).graph;
        let shape = effective_shape(&g, &bare("note")).unwrap();
        let origin = shape.get(&FieldName("description".into())).unwrap();
        assert_eq!(origin.canonical_origin().as_str(), "note");
        assert_eq!(origin.canonical_decl().name.as_str(), "description");
        assert_eq!(origin.origins.len(), 1);
    }

    #[test]
    fn effective_shape_for_tag_type_is_empty() {
        let g = build_graph(vec![td("tag", &[], &[])]).graph;
        let shape = effective_shape(&g, &bare("tag")).unwrap();
        assert!(shape.is_empty());
    }

    // ---------- multi-claim mixin ([[type closure::au-type-system]], [[type-def fields collision - auto-unify and qualified field::au-type-system]]) ----------

    #[test]
    fn mixin_unions_disjoint_claims() {
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("deliverable", &[], &["audience"]),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        assert_eq!(shape.len(), 2);
        assert!(shape.divergent().next().is_none());
        let by_origin: Vec<(&str, &str)> = shape
            .iter()
            .map(|(k, v)| (k.as_str(), v.canonical_origin().as_str()))
            .collect();
        assert_eq!(
            by_origin,
            vec![("audience", "deliverable"), ("description", "note")]
        );
    }

    #[test]
    fn mixin_auto_unifies_token_equal_shapes() {
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("deliverable", &[], &["description"]),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        assert_eq!(shape.len(), 1);
        assert!(shape.divergent().next().is_none());
        let entry = shape.get(&FieldName("description".into())).unwrap();
        assert_eq!(entry.origins.len(), 2);
        let names: Vec<&str> = entry.origins.keys().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["deliverable", "note"]);
    }

    /// Build a TypeDef whose single field has an `Err` parsed_shape carrying
    /// a synthetic diagnostic. Spans are derived from `name` so two type-defs
    /// produce diagnostics that compare unequal under derived `PartialEq` —
    /// the exact case the auto-unify equality used to mishandle.
    fn td_with_unparsed_field(name: &str, field: &str, raw_shape: &str) -> TypeDef {
        let path = PathBuf::from(format!("/v/{name}.type.yaml"));
        let span = Span::new(path.clone(), ByteRange::new(name.len(), name.len() + 1));
        let diag = Diagnostic {
            code: DiagnosticCode::from_static("not-yet-implemented-shape-feature"),
            severity: Severity::Error,
            span,
            message: format!("shape '{}' is not yet implemented", raw_shape),
            related: vec![],
            fix: None,
        };
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: path,
            source_span: ByteRange::new(0, 0),
            parent_claim: None,
            parents: vec![],
            fields: vec![FieldDecl {
                name: FieldName(field.into()),
                optional: false,
                raw_shape: raw_shape.into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(name.len(), name.len() + 1),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Err(diag),
                doc: None,
            }],
            sealed: vec![],
            declared_abstract: false,
            meta_blocks: None,
            required_meta: Vec::new(),
            body: None,
            doc: None,
            ..Default::default()
        }
    }

    #[test]
    fn mixin_auto_unifies_unparsed_shapes_with_same_raw_text() {
        // Two parents both declare `summary: rationale` (a deferred bare-name
        // shape — Err parsed_shape). Same source text under the same parser
        // produces equivalent Errs, so auto-unify must collapse them. Pre-fix
        // this fired `mixin-collision` because derived Diagnostic equality
        // includes per-decl spans that differ across files.
        let g = build_graph(vec![
            td_with_unparsed_field("note", "summary", "rationale"),
            td_with_unparsed_field("deliverable", "summary", "rationale"),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        assert!(
            shape.divergent().next().is_none(),
            "identical raw_shape across origins must auto-unify, got divergent: {:?}",
            shape
                .divergent()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
        );
        let entry = shape.get(&FieldName("summary".into())).unwrap();
        assert_eq!(entry.origins.len(), 2);
    }

    #[test]
    fn mixin_collides_when_unparsed_shapes_have_different_raw_text() {
        let g = build_graph(vec![
            td_with_unparsed_field("note", "summary", "rationale"),
            td_with_unparsed_field("deliverable", "summary", "<X | Y>"),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        assert!(shape.get(&FieldName("summary".into())).is_none());
        assert_eq!(shape.divergent().count(), 1);
    }

    #[test]
    fn mixin_collides_when_one_origin_parses_and_the_other_does_not() {
        let g = build_graph(vec![
            td_with_shapes(
                "note",
                &[],
                &[("summary", Shape::Primitive(Primitive::String))],
            ),
            td_with_unparsed_field("deliverable", "summary", "rationale"),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        assert!(shape.get(&FieldName("summary".into())).is_none());
        assert_eq!(shape.divergent().count(), 1);
    }

    #[test]
    fn mixin_non_token_equal_produces_collision() {
        let g = build_graph(vec![
            td_with_shapes(
                "tag-strict",
                &[],
                &[("priority", Shape::Enum(vec!["low".into(), "high".into()]))],
            ),
            td_with_shapes(
                "tag-loose",
                &[],
                &[(
                    "priority",
                    Shape::Enum(vec!["low".into(), "moderate".into(), "high".into()]),
                )],
            ),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["tag-strict", "tag-loose"])).unwrap();
        // Field absent from resolved fields...
        assert!(shape.get(&FieldName("priority".into())).is_none());
        // ...present in the divergent set instead, with both origins listed.
        assert_eq!(shape.divergent().count(), 1);
        let (field_name, fo) = shape.divergent().next().unwrap();
        assert_eq!(field_name.as_str(), "priority");
        let names: Vec<&str> = fo.origins().map(|(id, _)| id.as_str()).collect();
        assert_eq!(names, vec!["tag-loose", "tag-strict"]); // sorted by origin
    }

    #[test]
    fn divergent_field_is_kept_with_per_origin_decls() {
        // A non-token-equal field stays in the effective shape as a per-origin
        // `FieldOrigin` in `divergent` — not dropped. `get()` (resolved only)
        // misses it, `get_divergent()` finds it, `is_divergent()` is true, and
        // each origin keeps its OWN shape.
        let g = build_graph(vec![
            td_with_shapes(
                "note",
                &[],
                &[("title", Shape::Primitive(Primitive::String))],
            ),
            td_with_shapes(
                "deliverable",
                &[],
                &[("title", Shape::Primitive(Primitive::Number))],
            ),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        let title = FieldName("title".into());
        // Absent from the resolved set, present in the divergent set.
        assert!(shape.get(&title).is_none());
        assert!(!shape.iter().any(|(n, _)| n == &title));
        let fo = shape
            .get_divergent(&title)
            .expect("divergent field is kept");
        assert!(fo.is_divergent());
        // Two origins, keyed by identity (bare names in the own graph), each
        // carrying its own shape.
        let ids: Vec<&str> = fo.origins().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["deliverable", "note"]);
        let deliverable_shape = &fo
            .origins
            .get(&OriginId("deliverable".into()))
            .unwrap()
            .decl;
        let note_shape = &fo.origins.get(&OriginId("note".into())).unwrap().decl;
        assert_eq!(
            deliverable_shape.parsed_shape,
            Ok(Shape::Primitive(Primitive::Number))
        );
        assert_eq!(
            note_shape.parsed_shape,
            Ok(Shape::Primitive(Primitive::String))
        );
        // The divergent iterator surfaces it too.
        let divergent: Vec<&str> = shape.divergent().map(|(n, _)| n.as_str()).collect();
        assert_eq!(divergent, vec!["title"]);
    }

    #[test]
    fn auto_unified_field_is_not_divergent() {
        // Token-equal origins land in `fields`, `is_divergent()` false.
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("deliverable", &[], &["description"]),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        let entry = shape.get(&FieldName("description".into())).unwrap();
        assert!(!entry.is_divergent());
        assert!(shape.divergent().next().is_none());
    }

    #[test]
    fn diamond_inheritance_yields_single_origin() {
        // base declares F; reachable via a-chain and b-chain. `F` originates
        // on `base`; auto-unify trivially collapses to one origin.
        let g = build_graph(vec![
            td("base", &[], &["myField"]),
            td("a", &["base"], &[]),
            td("b", &["base"], &[]),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["a", "b"])).unwrap();
        assert_eq!(shape.len(), 1);
        let entry = shape.get(&FieldName("myField".into())).unwrap();
        assert_eq!(entry.origins.len(), 1);
        assert_eq!(entry.canonical_origin().as_str(), "base");
        assert!(shape.divergent().next().is_none());
    }

    #[test]
    fn mixin_claim_order_is_commutative() {
        let g = build_graph(vec![
            td("note", &[], &["description"]),
            td("deliverable", &[], &["audience"]),
        ])
        .graph;
        let ab = effective_shape(&g, &list(&["note", "deliverable"])).unwrap();
        let ba = effective_shape(&g, &list(&["deliverable", "note"])).unwrap();
        assert_eq!(ab, ba);
    }

    #[test]
    fn instance_closure_exposes_full_union() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("decision", &["note"], &[]),
            td("maturity", &[], &[]),
        ])
        .graph;
        let shape = effective_shape(&g, &list(&["decision", "maturity"])).unwrap();
        let names: Vec<&str> = shape
            .instance_closure()
            .iter()
            .map(|n| n.as_str())
            .collect();
        // Closure of `decision` walks through `note`; closure of `maturity`
        // is just itself; union sorted by name.
        assert_eq!(names, vec!["decision", "maturity", "note"]);
    }

    #[test]
    fn is_required_holds_iff_any_origin_requires() {
        // Both origins required → required.
        let g = build_graph(vec![td("a", &[], &["f"]), td("b", &[], &["f"])]).graph;
        let shape = effective_shape(&g, &list(&["a", "b"])).unwrap();
        assert!(shape.get(&FieldName("f".into())).unwrap().is_required());
    }

    // ---------- referenced closure (parents + field-referenced types) ----------

    #[test]
    fn referenced_closure_includes_transitive_field_types() {
        // A -b-> B -c-> C, all via record fields. The referenced closure
        // follows field edges transitively where `closure_of` (parents only)
        // stops at A.
        let g = build_graph(vec![
            td_with_shapes("A", &[], &[("b", Shape::Record("B".into()))]),
            td_with_shapes("B", &[], &[("c", Shape::Record("C".into()))]),
            td("C", &[], &[]),
        ])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        let names: Vec<_> = rc.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
        // `closure_of` (parent-only) sees just A — the divergence this fix
        // closes lives entirely on the field axis.
        assert_eq!(closure_of(&g, &TypeName("A".into())).len(), 1);
    }

    #[test]
    fn referenced_closure_is_cycle_safe_via_reference_fields() {
        // A -*-> B -*-> C -*-> A, a reference-field cycle (legal: think
        // `manager: Person*`). The visited set halts the walk; it returns the
        // full mutually-reachable set rather than looping.
        let g = build_graph(vec![
            td_with_shapes("A", &[], &[("b", Shape::Reference("B".into()))]),
            td_with_shapes("B", &[], &[("c", Shape::Reference("C".into()))]),
            td_with_shapes("C", &[], &[("a", Shape::Reference("A".into()))]),
        ])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        let names: Vec<_> = rc.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn referenced_closure_is_cycle_safe_via_record_fields() {
        // Mutual record cycle A <-> B. Terminates, both included.
        let g = build_graph(vec![
            td_with_shapes("A", &[], &[("b", Shape::Record("B".into()))]),
            td_with_shapes("B", &[], &[("a", Shape::Record("A".into()))]),
        ])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        let names: Vec<_> = rc.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
    }

    #[test]
    fn referenced_closure_handles_self_referential_field() {
        // A field referencing its own type — a single-node cycle. Terminates.
        let g = build_graph(vec![td_with_shapes(
            "A",
            &[],
            &[("me", Shape::Reference("A".into()))],
        )])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        assert_eq!(rc.len(), 1);
        assert!(rc.contains(&TypeName("A".into())));
    }

    #[test]
    fn referenced_closure_combines_parent_and_field_axes() {
        // A: parent P, field b: B. P: field q: Q. The closure must reach the
        // parent (P), the parent's own field types (Q), and A's field type (B).
        let g = build_graph(vec![
            td_with_shapes("A", &["P"], &[("b", Shape::Record("B".into()))]),
            td_with_shapes("P", &[], &[("q", Shape::Record("Q".into()))]),
            td("B", &[], &[]),
            td("Q", &[], &[]),
        ])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        let names: Vec<_> = rc.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "P", "Q"]);
    }

    #[test]
    fn referenced_closure_walks_every_field_shape_form() {
        // List, union, def-ref, and pinned-reference forms all reach their
        // named type-defs, in lockstep with `collect_referenced_type_names`.
        let g = build_graph(vec![
            td_with_shapes(
                "A",
                &[],
                &[
                    (
                        "l",
                        Shape::List {
                            inner: Box::new(Shape::Reference("L".into())),
                            min: 0,
                            max: None,
                        },
                    ),
                    (
                        "u",
                        Shape::Union(vec![Shape::Record("U1".into()), Shape::Record("U2".into())]),
                    ),
                    ("d", Shape::DefReference(Some(DefBound::Single("D".into())))),
                    ("p", Shape::Pinned(Box::new(Shape::Reference("P".into())))),
                ],
            ),
            td("L", &[], &[]),
            td("U1", &[], &[]),
            td("U2", &[], &[]),
            td("D", &[], &[]),
            td("P", &[], &[]),
        ])
        .graph;
        let rc = referenced_closure_of(&g, &TypeName("A".into()));
        let names: Vec<_> = rc.iter().map(|n| n.as_str()).collect();
        assert_eq!(names, vec!["A", "D", "L", "P", "U1", "U2"]);
    }
}
