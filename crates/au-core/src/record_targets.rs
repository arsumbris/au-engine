//! Per-file index of addressable inline records ([[type block-id::au-type-system]]):
//! `^:` id → the record's effective claim names.
//!
//! Computed once per instance before validation. Reference resolution
//! looks block-ids up here first (frontmatter precedes the body in
//! document order), then falls back to body blocks.
//!
//! The effective claim is the explicit inline `type:` when present,
//! else the type the slot pins ([[type-def shape record::au-type-system]]): a bare
//! `Record` or `InlineOrReference` slot names exactly one type-def.
//! Union / intersection / sealed slots demand an explicit claim — a
//! record without one there is already `inline-value-missing-type`, so
//! it indexes with no claims and the typed check skips it (no
//! double-fire).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use au_diagnostics::ByteRange;
use au_grammar::{QualifiedName, RefMode, Shape};

use crate::closure::{effective_shape, effective_shape_resolved, EffectiveShape};
use crate::graph::TypeGraph;
use crate::instance::{InlineValue, Instance, InstanceField, InstanceValue, TypeClaim};
use crate::resolution::{fold_extending, PeerGraphResolver, ResolutionGraph};
use crate::typedef::{FieldName, TypeName, TypeNameClaim};

/// The resolution context threaded through one record-targets walk.
///
/// Bundles the four values every walk step needs — the source repo's own
/// [`TypeGraph`], its optional cross-repo fold, the optional peer-graph resolver,
/// and the source repo name — so a walk passes one `&ResolveCtx` instead of a
/// four-value tuple. The resolver-less single-repo fallback is then one check on
/// the context rather than a repeated `(resolution, resolver)` match.
///
/// It also carries a per-walk memo of extended folds ([`ResolveCtx::extended_fold`]),
/// so sibling nested records sharing a `::repo` seed set re-use one fold rather
/// than recomputing it per record.
struct ResolveCtx<'a> {
    graph: &'a TypeGraph,
    resolution: Option<&'a ResolutionGraph>,
    resolver: Option<&'a dyn PeerGraphResolver>,
    own_repo: &'a str,
    /// Memo of [`fold_extending`] results keyed by SORTED seed-set, per walk.
    ///
    /// Sibling nested records in one slot carry the same `::repo` seeds (e.g. 508
    /// `concept-candidate::base` records in one file), and `fold_extending` clones
    /// two `BTreeMap`s per call, so without this the identical fold is recomputed
    /// once per record — O(records) folds collapse to O(distinct seed sets). The
    /// fold is pure over `(base, own_repo, seeds, resolver)`, all fixed within a
    /// walk, so the cache is sound; it is per-walk, so it never outlives the
    /// inputs it is keyed against. See [[codereview - 2609101214 - cross-repo nested discovery parity, healthy with one efficiency cliff]].
    fold_cache: RefCell<BTreeMap<Vec<(TypeName, String)>, Rc<ResolutionGraph>>>,
}

impl<'a> ResolveCtx<'a> {
    fn new(
        graph: &'a TypeGraph,
        resolution: Option<&'a ResolutionGraph>,
        resolver: Option<&'a dyn PeerGraphResolver>,
        own_repo: &'a str,
    ) -> Self {
        ResolveCtx {
            graph,
            resolution,
            resolver,
            own_repo,
            fold_cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// The resolution graph extended with `seeds`, folded once per distinct seed
    /// set within this walk. Requires a base fold and a resolver; the caller
    /// handles the resolver-less / seedless fallback.
    fn extended_fold(
        &self,
        base: &ResolutionGraph,
        resolver: &dyn PeerGraphResolver,
        seeds: &[(TypeName, String)],
    ) -> Rc<ResolutionGraph> {
        // Sorted + deduped key, so the same seed set in any order hits; the fold
        // itself drains a set, so order never affects the result.
        let mut key = seeds.to_vec();
        key.sort();
        key.dedup();
        if let Some(hit) = self.fold_cache.borrow().get(&key) {
            return hit.clone();
        }
        let extended = Rc::new(fold_extending(base, self.own_repo, seeds, resolver));
        self.fold_cache.borrow_mut().insert(key, extended.clone());
        extended
    }
}

/// `^:` id → the record carrying it.
/// First occurrence wins, matching resolve-first semantics —
/// `block-id-duplicate` surfaces the collision separately.
pub type RecordTargets = BTreeMap<String, RecordTarget>;

/// One addressable inline record: its effective claim names plus the
/// file-absolute span of the `^:` value.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordTarget {
    /// The bare claim names, for the own-graph / same-repo closure check.
    pub claims: Vec<TypeName>,
    /// The claim in QUALIFIED form (each `::repo` preserved), for a cross-repo
    /// qualified DEMAND that folds the record's own claim over its repo's
    /// resolution graph.
    /// The explicit inline `type:` when present, else the slot-pinned type.
    pub qualified: Vec<TypeNameClaim>,
    pub span: ByteRange,
}

pub fn collect_record_targets(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    resolver: Option<&dyn PeerGraphResolver>,
    own_repo: &str,
    instance: &Instance,
) -> RecordTargets {
    let mut out = RecordTargets::new();
    for (id, target) in
        collect_record_target_occurrences(graph, resolution, resolver, own_repo, instance)
    {
        // First occurrence wins, the resolve-first semantics. The walk emits
        // in document order, so the first entry per id is the first in the
        // file.
        out.entry(id).or_insert(target);
    }
    out
}

/// Every addressable inline record in document order, duplicate `^:` ids
/// INCLUDED.
///
/// [`collect_record_targets`] is this folded into a first-wins map, which is
/// what resolution wants: `[[file^id]]` reaches exactly one record. A LISTING
/// wants the opposite, every occurrence, so a consumer sees what the file
/// actually carries and `block-id-duplicate` has something to agree with. One
/// walk behind both, so the two views cannot disagree about what a record is.
pub fn collect_record_target_occurrences(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    resolver: Option<&dyn PeerGraphResolver>,
    own_repo: &str,
    instance: &Instance,
) -> Vec<(String, RecordTarget)> {
    let ctx = ResolveCtx::new(graph, resolution, resolver, own_repo);
    let mut out = Vec::new();
    let shape = resolved_shape(&ctx, &instance.type_claim);
    for field in &instance.fields {
        let slot = shape.as_ref().and_then(|s| field_shape(s, &field.key));
        walk_value(&ctx, &field.value, slot, &mut out);
    }
    out
}

/// An instance-or-record claim's effective shape, resolved over the fold when a
/// resolution graph is present, else the own graph. The mirror of the validator's
/// `effective_shape_for` seam: an importing claim (`foo::repo`) resolves its peer
/// fields (re-qualified to the owner repo) rather than yielding an own-only shape.
fn resolved_shape(ctx: &ResolveCtx, claim: &crate::instance::TypeClaim) -> Option<EffectiveShape> {
    match ctx.resolution {
        Some(rg) => effective_shape_resolved(rg, ctx.graph, claim).ok(),
        None => effective_shape(ctx.graph, claim).ok(),
    }
}

/// A nested record's effective shape, resolved owner-relative.
///
/// When the record claims a peer type (`foo::repo`, whether slot-pinned or
/// explicit), its own field shapes live in the owner repo, absent from the source
/// fold (the fold follows the claim / parent axis, not field references). So the
/// source fold is extended on demand with the record's `::repo` claims, then the
/// shape resolves over the extended fold, where the peer node's fields are
/// re-qualified to the owner. This is what lets a peer record nested inside
/// another peer record be discovered. A bare / own claim, or the resolver-less
/// case (single-repo, au-core's own tests), falls back to [`resolved_shape`].
fn nested_effective_shape(ctx: &ResolveCtx, qualified: &[TypeNameClaim]) -> Option<EffectiveShape> {
    if qualified.is_empty() {
        return None;
    }
    let claim = claim_from_qualified(qualified);
    let seeds: Vec<(TypeName, String)> = qualified
        .iter()
        .filter_map(|c| c.repo.as_ref().map(|r| (c.name.clone(), r.clone())))
        .collect();
    match (ctx.resolution, ctx.resolver) {
        (Some(base), Some(res)) if !seeds.is_empty() => {
            // Extended once per distinct seed set within this walk, see
            // [`ResolveCtx::extended_fold`].
            let extended = ctx.extended_fold(base, res, &seeds);
            effective_shape_resolved(&extended, ctx.graph, &claim).ok()
        }
        _ => resolved_shape(ctx, &claim),
    }
}

/// A [`TypeClaim`] over a record's qualified claim names, `::repo` preserved, for
/// the nested effective-shape walk. A single claim is `Bare`, a mixin a `List`.
fn claim_from_qualified(qualified: &[TypeNameClaim]) -> TypeClaim {
    if qualified.len() == 1 {
        TypeClaim::Bare(qualified[0].clone())
    } else {
        TypeClaim::List {
            items: qualified.to_vec(),
            value_span: ByteRange::new(0, 0),
        }
    }
}

/// A field's canonical declared shape, by bare key. Qualified keys
/// (`T:field`) are not resolved here — a record under one pins via its
/// explicit claim only.
fn field_shape<'a>(shape: &'a EffectiveShape, key: &str) -> Option<&'a Shape> {
    shape
        .get(&FieldName(key.to_string()))
        .and_then(|origin| origin.canonical_decl().parsed_shape.as_ref().ok())
}

fn walk_value(
    ctx: &ResolveCtx,
    value: &InstanceValue,
    slot: Option<&Shape>,
    out: &mut Vec<(String, RecordTarget)>,
) {
    match value {
        InstanceValue::Sequence(elements) => {
            let inner = slot.and_then(|s| match s {
                Shape::List { inner, .. } => Some(inner.as_ref()),
                _ => None,
            });
            for el in elements {
                walk_value(ctx, &el.value, inner, out);
            }
        }
        InstanceValue::Mapping(inline) => {
            let (claims, qualified): (Vec<TypeName>, Vec<TypeNameClaim>) =
                if let Some(claim) = &inline.type_claim {
                    (
                        claim.iter().map(|c| c.name.clone()).collect(),
                        claim.iter().cloned().collect(),
                    )
                } else if let Some(qn) = slot.and_then(pinned_qualified) {
                    let tnc = pinned_claim(qn);
                    (vec![tnc.name.clone()], vec![tnc])
                } else {
                    (Vec::new(), Vec::new())
                };
            if let Some(decl) = &inline.block_id {
                out.push((
                    decl.id.clone(),
                    RecordTarget {
                        claims: claims.clone(),
                        qualified: qualified.clone(),
                        span: decl.value_span,
                    },
                ));
            }
            // Nested records validate against the claimed type's shape, resolved
            // owner-relative so a peer record nested inside another peer type is
            // seen (parity with `enum_walk`).
            let nested_shape = nested_effective_shape(ctx, &qualified);
            for field in &inline.fields {
                let nested_slot = nested_shape
                    .as_ref()
                    .and_then(|s| field_shape(s, &field.key));
                walk_value(ctx, &field.value, nested_slot, out);
            }
        }
        _ => {}
    }
}

/// A segment of a [`NestedRecord`]'s path from the file root: a field name or a
/// list index. The structured form, so a consumer can reason over it and it can
/// later serve as a mutation locator, see
/// [[spec - instances-of read - flat match records tagged by identity and claimed-or-inherited]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Field(String),
    Index(usize),
}

/// One nested inline record found inside an instance, at any depth, whether or
/// not it carries a `^` block-id.
///
/// The enumeration counterpart to [`collect_record_targets`], which indexes only
/// block-id-bearing records: this emits EVERY claimed inline record with a
/// structured path back to it, for the `instances_of` origin-taxonomy read.
#[derive(Debug, Clone, PartialEq)]
pub struct NestedRecord {
    /// The record's effective claim, bare names, for the own-graph closure check.
    pub claims: Vec<TypeName>,
    /// The effective claim in QUALIFIED form (`::repo` preserved), for a
    /// cross-repo qualified demand.
    pub qualified: Vec<TypeNameClaim>,
    /// Structured path from the file root: field names and list indices.
    pub field_path: Vec<PathSegment>,
    /// The record's value span.
    pub span: ByteRange,
    /// The `^` block-id when the record carries one, else `None`.
    pub block_id: Option<String>,
    /// The record's own body fields, so a consumer sees the record's values.
    pub fields: Vec<InstanceField>,
    /// The record's `#:` head docstring, `None` when absent. See [[type docstring::au-type-system]].
    pub doc: Option<String>,
    /// Per-field `#:` docstrings, keyed by field key, documented fields only.
    pub field_docs: std::collections::BTreeMap<String, String>,
}

/// Every claimed nested inline record inside `instance`, at any depth.
///
/// Walks the instance's field values by the same parallel value/effective-shape
/// descent as [`collect_record_targets`], emitting a [`NestedRecord`] for each
/// inline mapping that resolves a claim (an explicit `type:` or a slot-pinned
/// type). A mapping without a claim is not a typed instance, so it is skipped,
/// but the walk still descends through it to reach deeper records. The top-level
/// instance itself is NOT emitted, it is the `file` origin of the read.
pub fn enumerate_nested_records(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    resolver: Option<&dyn PeerGraphResolver>,
    own_repo: &str,
    instance: &Instance,
) -> Vec<NestedRecord> {
    let ctx = ResolveCtx::new(graph, resolution, resolver, own_repo);
    let mut out = Vec::new();
    // Resolution-aware, so a file claiming a peer type (`type: foo::repo`) resolves
    // its effective shape over the fold, where the peer node's field shapes are
    // re-qualified to the owner repo. A slot-pinned inline record then pins to the
    // owner-qualified type, resolved owner-relative like validation, not dropped.
    let shape = resolved_shape(&ctx, &instance.type_claim);
    let mut path = Vec::new();
    for field in &instance.fields {
        let slot = shape.as_ref().and_then(|s| field_shape(s, &field.key));
        path.push(PathSegment::Field(field.key.clone()));
        enum_walk(
            &ctx,
            &field.value,
            field.value_span,
            slot,
            &mut path,
            &mut out,
        );
        path.pop();
    }
    out
}

fn enum_walk(
    ctx: &ResolveCtx,
    value: &InstanceValue,
    value_span: ByteRange,
    slot: Option<&Shape>,
    path: &mut Vec<PathSegment>,
    out: &mut Vec<NestedRecord>,
) {
    match value {
        InstanceValue::Sequence(elements) => {
            let inner = slot.and_then(|s| match s {
                Shape::List { inner, .. } => Some(inner.as_ref()),
                _ => None,
            });
            for (i, el) in elements.iter().enumerate() {
                path.push(PathSegment::Index(i));
                enum_walk(ctx, &el.value, el.span, inner, path, out);
                path.pop();
            }
        }
        InstanceValue::Mapping(inline) => {
            let (claims, qualified): (Vec<TypeName>, Vec<TypeNameClaim>) =
                if let Some(claim) = &inline.type_claim {
                    (
                        claim.iter().map(|c| c.name.clone()).collect(),
                        claim.iter().cloned().collect(),
                    )
                } else if let Some(qn) = slot.and_then(pinned_qualified) {
                    let tnc = pinned_claim(qn);
                    (vec![tnc.name.clone()], vec![tnc])
                } else {
                    (Vec::new(), Vec::new())
                };
            if !claims.is_empty() {
                out.push(NestedRecord {
                    claims: claims.clone(),
                    qualified: qualified.clone(),
                    field_path: path.clone(),
                    span: value_span,
                    block_id: inline.block_id.as_ref().map(|d| d.id.clone()),
                    fields: inline.fields.clone(),
                    doc: inline.doc.clone(),
                    field_docs: inline.field_docs.clone(),
                });
            }
            // Nested records validate against the claimed type's shape; descend
            // even through a claim-less mapping to reach an explicitly-typed
            // record deeper in. Owner-relative: a record claiming a peer type
            // resolves ITS shape over the owner repo, so a peer type nested inside
            // another peer type is seen, not dropped.
            let nested_shape = nested_effective_shape(ctx, &qualified);
            for field in &inline.fields {
                let nested_slot = nested_shape
                    .as_ref()
                    .and_then(|s| field_shape(s, &field.key));
                path.push(PathSegment::Field(field.key.clone()));
                enum_walk(ctx, &field.value, field.value_span, nested_slot, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// The declared shape governing the value whose value-span is `span`.
///
/// Resolves the slot by the same parallel value/effective-shape descent
/// [`collect_record_targets`] uses, then returns the shape that introduced the
/// value at `span` — a record mapping, a scalar reference, any value-kind.
/// `None` when no declared shape governs the slot (an extra field) or no value
/// matches the span, so the caller imposes no constraint.
///
/// The shared slot-resolution primitive behind the promote / inline host-slot
/// guards (each checks the resolved shape admits the form it writes) and the
/// inline `file*`-referrer guard (which matches `Shape::Reference("file")`).
pub fn slot_shape_at(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    resolver: Option<&dyn PeerGraphResolver>,
    own_repo: &str,
    instance: &Instance,
    span: ByteRange,
) -> Option<Shape> {
    fn walk(
        ctx: &ResolveCtx,
        value: &InstanceValue,
        value_span: ByteRange,
        slot: Option<&Shape>,
        span: ByteRange,
        out: &mut Option<Option<Shape>>,
    ) {
        if out.is_some() {
            return;
        }
        if value_span == span {
            *out = Some(slot.cloned());
            return;
        }
        match value {
            InstanceValue::Sequence(elements) => {
                let inner = slot.and_then(|s| match s {
                    Shape::List { inner, .. } => Some(inner.as_ref()),
                    _ => None,
                });
                for el in elements {
                    walk(ctx, &el.value, el.span, inner, span, out);
                }
            }
            InstanceValue::Mapping(inline) => {
                // The nested record's claim, `::repo` preserved (an explicit claim,
                // else the slot's pin), so its shape resolves owner-relative — a
                // record inside a cross-repo-claiming host's slot is seen, parity
                // with `enum_walk` / `walk_value`.
                let qualified: Vec<TypeNameClaim> = if let Some(claim) = &inline.type_claim {
                    claim.iter().cloned().collect()
                } else if let Some(qn) = slot.and_then(pinned_qualified) {
                    vec![pinned_claim(qn)]
                } else {
                    Vec::new()
                };
                let nested = nested_effective_shape(ctx, &qualified);
                for field in &inline.fields {
                    let nested_slot = nested.as_ref().and_then(|s| field_shape(s, &field.key));
                    walk(ctx, &field.value, field.value_span, nested_slot, span, out);
                }
            }
            _ => {}
        }
    }

    let ctx = ResolveCtx::new(graph, resolution, resolver, own_repo);
    // The host's top-level shape resolved over the fold, so a file claiming a peer
    // type (`type: holder::base`) sees its peer-typed slots rather than yielding no
    // shape — the guard's own-graph-only gap this closes.
    let top = resolved_shape(&ctx, &instance.type_claim)?;
    let mut out: Option<Option<Shape>> = None;
    for field in &instance.fields {
        walk(
            &ctx,
            &field.value,
            field.value_span,
            field_shape(&top, &field.key),
            span,
            &mut out,
        );
    }
    out.flatten()
}

/// Whether the inline record whose value-span is `span` sits in a slot that
/// admits a reference value.
///
/// `Some(true)` for an inline-or-reference (`&`) slot, single-name or compound.
/// `Some(false)` for a bare inline-only slot (`Record`, `any`, a non-`&`
/// compound) — it holds the record now but cannot hold the `[[newFile]]`
/// reference a promote would write in its place. `None` when no declared shape
/// governs the slot (an extra field) or no record matches the span, so the
/// caller imposes no constraint.
pub fn record_slot_admits_reference_at(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    resolver: Option<&dyn PeerGraphResolver>,
    own_repo: &str,
    instance: &Instance,
    span: ByteRange,
) -> Option<bool> {
    slot_shape_at(graph, resolution, resolver, own_repo, instance, span)
        .map(|s| shape_is_inline_or_reference(&s))
}

/// A slot that accepts both an inline record and a reference value: the `&`
/// inline-or-reference forms, single-name or the inline compound. A list wrapper
/// is unwrapped first.
///
/// The sole shape where both promote (record → reference) and inline (reference
/// → record) are well-defined, so it gates both directions: promote rejects a
/// bare inline-only slot, inline rejects a reference-only (`*`) slot.
pub fn shape_is_inline_or_reference(shape: &Shape) -> bool {
    let base = match shape {
        Shape::List { inner, .. } => inner.as_ref(),
        other => other,
    };
    matches!(
        base,
        Shape::InlineOrReference(_)
            | Shape::CompoundReference {
                mode: RefMode::Inline,
                ..
            }
    )
}

/// The pinned slot type in QUALIFIED form, so a slot-pinned record (no explicit
/// `type:`) keeps the slot's `::repo` for the cross-repo fold.
fn pinned_qualified(shape: &Shape) -> Option<&QualifiedName> {
    match shape {
        Shape::Record(name) | Shape::InlineOrReference(name) => Some(name),
        _ => None,
    }
}

/// A `TypeNameClaim` for a slot-pinned record's identity, preserving the slot's
/// `::repo` qualifier.
fn pinned_claim(qn: &QualifiedName) -> TypeNameClaim {
    let raw = match &qn.repo {
        Some(r) => format!("{}::{}", qn.as_str(), r),
        None => qn.as_str().to_string(),
    };
    TypeNameClaim::parse(&raw, ByteRange::new(0, 0))
}

/// The value-kind of a located field, so an `edit_record` replace can preserve
/// it. `Other` is a parse the engine does not model as one of the four (a
/// `NotYetSupported` value); a kind-preserving replace over it is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Scalar,
    Null,
    Sequence,
    Mapping,
    Other,
}

/// One authored field in a located record, with the spans a splice needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpan {
    pub key: String,
    pub key_span: ByteRange,
    pub value_span: ByteRange,
    pub kind: ValueKind,
}

/// A located record: the inline mapping (or the file instance) a `field_path`
/// addresses, with every span an `edit_record` splice needs. The insert anchor
/// (the record's last authored line) is computed by the write layer from
/// `type_span`, `block_id_span`, and the field value spans, since it needs the
/// raw bytes to find the line end and indent.
#[derive(Debug, Clone, PartialEq)]
pub struct LocatedRecord {
    /// The record's own span: the inline mapping, or the file instance's source span.
    pub span: ByteRange,
    /// The `type:` claim span, present on a file instance and on a claim-bearing inline record.
    pub type_span: Option<ByteRange>,
    /// The `^:` block-id span (key start through value end), when the record carries one.
    pub block_id_span: Option<ByteRange>,
    /// The authored fields, in document order. Empty for a bare `type:`-only record.
    pub fields: Vec<FieldSpan>,
}

/// A located sequence: the block sequence a `field_path` addresses, each
/// element's span, for an `append_record` splice.
#[derive(Debug, Clone, PartialEq)]
pub struct LocatedSequence {
    pub span: ByteRange,
    pub elements: Vec<ByteRange>,
}

/// The node a `field_path` addresses: a record to patch, or a sequence to grow.
#[derive(Debug, Clone, PartialEq)]
pub enum Located {
    Record(LocatedRecord),
    Sequence(LocatedSequence),
}

/// Resolve a `field_path` to the record or sequence it addresses, the inverse of
/// [`enumerate_nested_records`]'s path emission. Descend by field name and list
/// index to the addressed node, carrying the byte spans a mutation splice needs.
/// Pure, no shape resolution.
///
/// An empty `field_path` addresses the file-level instance (its frontmatter
/// fields), the write convention. A path resolving to no node, or to a scalar
/// leaf (neither a record nor a sequence), returns `None`.
pub fn locate_field_path(instance: &Instance, field_path: &[PathSegment]) -> Option<Located> {
    let Some((head, rest)) = field_path.split_first() else {
        // The whole file instance is the addressed record.
        return Some(Located::Record(record_from_instance(instance)));
    };
    // The instance root is a mapping, so only a field descent applies.
    let PathSegment::Field(name) = head else {
        return None;
    };
    let field = instance.fields.iter().find(|f| &f.key == name)?;
    locate_descend(&field.value, field.value_span, rest)
}

fn locate_descend(value: &InstanceValue, span: ByteRange, path: &[PathSegment]) -> Option<Located> {
    let Some((head, rest)) = path.split_first() else {
        return match value {
            InstanceValue::Mapping(inline) => {
                Some(Located::Record(record_from_inline(inline, span)))
            }
            InstanceValue::Sequence(elements) => Some(Located::Sequence(LocatedSequence {
                span,
                elements: elements.iter().map(|e| e.span).collect(),
            })),
            _ => None,
        };
    };
    match (head, value) {
        (PathSegment::Field(name), InstanceValue::Mapping(inline)) => {
            let field = inline.fields.iter().find(|f| &f.key == name)?;
            locate_descend(&field.value, field.value_span, rest)
        }
        (PathSegment::Index(i), InstanceValue::Sequence(elements)) => {
            let el = elements.get(*i)?;
            locate_descend(&el.value, el.span, rest)
        }
        _ => None,
    }
}

fn record_from_instance(instance: &Instance) -> LocatedRecord {
    LocatedRecord {
        span: instance.source_span,
        type_span: Some(instance.type_claim.span()),
        block_id_span: None,
        fields: instance.fields.iter().map(field_span).collect(),
    }
}

fn record_from_inline(inline: &InlineValue, span: ByteRange) -> LocatedRecord {
    LocatedRecord {
        span,
        type_span: inline.type_claim.as_ref().map(|c| c.span()),
        block_id_span: inline
            .block_id
            .as_ref()
            .map(|b| ByteRange::new(b.key_span.start, b.value_span.end)),
        fields: inline.fields.iter().map(field_span).collect(),
    }
}

fn field_span(f: &InstanceField) -> FieldSpan {
    FieldSpan {
        key: f.key.clone(),
        key_span: f.key_span,
        value_span: f.value_span,
        kind: value_kind(&f.value),
    }
}

fn value_kind(v: &InstanceValue) -> ValueKind {
    match v {
        InstanceValue::String(_)
        | InstanceValue::Integer(_)
        | InstanceValue::Float(_)
        | InstanceValue::Boolean(_) => ValueKind::Scalar,
        InstanceValue::Null => ValueKind::Null,
        InstanceValue::Sequence(_) => ValueKind::Sequence,
        InstanceValue::Mapping(_) => ValueKind::Mapping,
        InstanceValue::NotYetSupported => ValueKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use au_parser::yaml::parse;
    use std::path::Path;

    fn graph_for(defs: &[(&str, &str)]) -> TypeGraph {
        let defs = defs
            .iter()
            .map(|(name, src)| {
                let docs = parse(src).unwrap();
                crate::typedef::parse_type_def(
                    Path::new(&format!("/v/type/{name}.type.yaml")),
                    src,
                    0,
                    &docs[0],
                )
                .type_def
                .unwrap()
            })
            .collect();
        build_graph(defs).graph
    }

    fn targets_for(graph: &TypeGraph, frontmatter: &str) -> RecordTargets {
        let docs = parse(frontmatter).unwrap();
        let inst = crate::instance::parse_instance(Path::new("/v/m.md"), frontmatter, 0, &docs[0])
            .instance
            .unwrap();
        collect_record_targets(graph, None, None, "", &inst)
    }

    fn names<'a>(targets: &'a RecordTargets, id: &str) -> Vec<&'a str> {
        targets[id].claims.iter().map(|n| n.as_str()).collect()
    }

    fn parse_inst(frontmatter: &str) -> crate::instance::Instance {
        let docs = parse(frontmatter).unwrap();
        crate::instance::parse_instance(Path::new("/v/m.md"), frontmatter, 0, &docs[0])
            .instance
            .unwrap()
    }

    #[test]
    fn slot_admits_reference_distinguishes_bare_from_amp() {
        let g = graph_for(&[
            ("holder", "fields:\n  bare: node\n  amp: node&\n"),
            ("node", "fields:\n  content?: String\n"),
        ]);
        // A record in a bare inline-only slot cannot hold a reference.
        let bare = parse_inst("type: holder\nbare:\n  type: node\n  content: x\n");
        let bare_span = bare.fields[0].value_span;
        assert_eq!(
            record_slot_admits_reference_at(&g, None, None, "", &bare, bare_span),
            Some(false)
        );
        // A record in an `&` slot can.
        let amp = parse_inst("type: holder\namp:\n  type: node\n  content: x\n");
        let amp_span = amp.fields[0].value_span;
        assert_eq!(
            record_slot_admits_reference_at(&g, None, None, "", &amp, amp_span),
            Some(true)
        );
    }

    #[test]
    fn slot_shape_at_resolves_a_scalar_reference_value() {
        // A `[[file]]` reference value resolves to its slot shape, the inline
        // host-slot guard's input: a `*` slot is reference-only (inline rejects),
        // an `&` slot admits the folded record, a `file*` slot is named for the
        // referrer guard.
        let g = graph_for(&[
            (
                "host",
                "fields:\n  star: node*\n  amp: node&\n  asset: file*\n",
            ),
            ("node", "fields:\n  content?: String\n"),
        ]);
        let inst = parse_inst("type: host\nstar: \"[[a]]\"\namp: \"[[a]]\"\nasset: \"[[a]]\"\n");
        let star = inst.fields[0].value_span;
        let amp = inst.fields[1].value_span;
        let asset = inst.fields[2].value_span;
        assert_eq!(
            slot_shape_at(&g, None, None, "", &inst, star),
            Some(Shape::Reference("node".into()))
        );
        assert!(!record_slot_admits_reference_at(&g, None, None, "", &inst, star).unwrap());
        assert!(record_slot_admits_reference_at(&g, None, None, "", &inst, amp).unwrap());
        assert_eq!(
            slot_shape_at(&g, None, None, "", &inst, asset),
            Some(Shape::Reference("file".into()))
        );
    }

    #[test]
    fn slot_admits_reference_is_none_for_an_undeclared_slot() {
        // A record in an extra (undeclared) field has no governing shape, so no
        // constraint is imposed.
        let g = graph_for(&[
            ("holder", "fields:\n  title?: String\n"),
            ("node", "fields:\n  content?: String\n"),
        ]);
        let inst = parse_inst("type: holder\nstray:\n  type: node\n  content: x\n");
        let span = inst.fields[0].value_span;
        assert_eq!(
            record_slot_admits_reference_at(&g, None, None, "", &inst, span),
            None
        );
    }

    #[test]
    fn explicit_claim_wins() {
        let g = graph_for(&[
            ("canvas", "fields:\n  nodes?: node&[]\n"),
            ("node", "fields:\n  content?: String\n"),
            ("special", "type: node\n"),
        ]);
        let t = targets_for(
            &g,
            "type: canvas\nnodes:\n  - ^: n1\n    type: special\n    content: x\n",
        );
        assert_eq!(names(&t, "n1"), vec!["special"]);
    }

    #[test]
    fn claim_less_record_pins_to_the_slot() {
        let g = graph_for(&[
            ("canvas", "fields:\n  nodes?: node&[]\n"),
            ("node", "fields:\n  content?: String\n"),
        ]);
        let t = targets_for(&g, "type: canvas\nnodes:\n  - ^: n1\n    content: x\n");
        assert_eq!(names(&t, "n1"), vec!["node"]);
    }

    #[test]
    fn nested_record_pins_through_the_claimed_types_shape() {
        let g = graph_for(&[
            ("canvas", "fields:\n  root?: node\n"),
            ("node", "fields:\n  child?: node\n"),
        ]);
        let t = targets_for(
            &g,
            "type: canvas\nroot:\n  ^: outer\n  child:\n    ^: inner\n",
        );
        assert_eq!(names(&t, "outer"), vec!["node"]);
        assert_eq!(names(&t, "inner"), vec!["node"]);
    }

    #[test]
    fn record_with_no_claim_and_no_pin_indexes_claimless() {
        // An extra field carries no slot; the id still indexes so
        // not-found stays accurate, with no claims for the typed check.
        let g = graph_for(&[("canvas", "fields:\n  title?: String\n")]);
        let t = targets_for(&g, "type: canvas\nstray:\n  ^: s1\n");
        assert!(names(&t, "s1").is_empty());
    }

    #[test]
    fn first_occurrence_wins_on_duplicate_ids() {
        let g = graph_for(&[
            ("canvas", "fields:\n  nodes?: node&[]\n"),
            ("node", "fields:\n  content?: String\n"),
            ("other", "fields: {}\n"),
        ]);
        let t = targets_for(
            &g,
            "type: canvas\nnodes:\n  - ^: n1\n    content: a\nstray:\n  ^: n1\n  type: other\n",
        );
        assert_eq!(names(&t, "n1"), vec!["node"]);
    }

    fn enum_for(graph: &TypeGraph, frontmatter: &str) -> Vec<NestedRecord> {
        enumerate_nested_records(graph, None, None, "", &parse_inst(frontmatter))
    }

    fn seg_field(s: &str) -> PathSegment {
        PathSegment::Field(s.to_string())
    }

    fn plan_graph() -> TypeGraph {
        graph_for(&[
            ("plan", "fields:\n  phases?: phase[]\n"),
            ("phase", "fields:\n  actions: action[+]\n"),
            ("action", "fields:\n  desc?: String\n"),
        ])
    }

    const PLAN_SRC: &str = "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action\n        desc: a\n      - type: action\n        desc: b\n";

    #[test]
    fn enumerate_yields_nested_records_at_every_depth_with_paths() {
        let g = plan_graph();
        let recs = enum_for(&g, PLAN_SRC);
        let shape: Vec<(Vec<&str>, Vec<PathSegment>)> = recs
            .iter()
            .map(|r| {
                (
                    r.claims.iter().map(|n| n.as_str()).collect(),
                    r.field_path.clone(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    vec!["phase"],
                    vec![seg_field("phases"), PathSegment::Index(0)]
                ),
                (
                    vec!["action"],
                    vec![
                        seg_field("phases"),
                        PathSegment::Index(0),
                        seg_field("actions"),
                        PathSegment::Index(0),
                    ]
                ),
                (
                    vec!["action"],
                    vec![
                        seg_field("phases"),
                        PathSegment::Index(0),
                        seg_field("actions"),
                        PathSegment::Index(1),
                    ]
                ),
            ]
        );
    }

    #[test]
    fn enumerate_emits_block_id_less_records() {
        // The gap over collect_record_targets: records with no `^:` id are still
        // enumerated, their `block_id` None.
        let g = plan_graph();
        let recs = enum_for(&g, PLAN_SRC);
        assert_eq!(recs.len(), 3);
        assert!(recs.iter().all(|r| r.block_id.is_none()));
    }

    #[test]
    fn enumerate_captures_block_id_when_present() {
        let g = plan_graph();
        let src = "type: plan\nphases:\n  - type: phase\n    actions:\n      - ^: act1\n        type: action\n        desc: a\n";
        let recs = enum_for(&g, src);
        let action = recs
            .iter()
            .find(|r| r.claims.iter().any(|n| n.as_str() == "action"))
            .unwrap();
        assert_eq!(action.block_id.as_deref(), Some("act1"));
        let phase = recs
            .iter()
            .find(|r| r.claims.iter().any(|n| n.as_str() == "phase"))
            .unwrap();
        assert_eq!(phase.block_id, None);
    }

    #[test]
    fn enumerate_skips_the_top_level_instance() {
        // A plan with no nested records yields nothing; the file instance is the
        // `file` origin, not enumerated here.
        let g = plan_graph();
        let recs = enum_for(&g, "type: plan\n");
        assert!(recs.is_empty());
    }

    fn as_record(loc: Option<Located>) -> LocatedRecord {
        match loc {
            Some(Located::Record(r)) => r,
            other => panic!("expected a record, got {other:?}"),
        }
    }

    fn as_sequence(loc: Option<Located>) -> LocatedSequence {
        match loc {
            Some(Located::Sequence(s)) => s,
            other => panic!("expected a sequence, got {other:?}"),
        }
    }

    #[test]
    fn locate_finds_a_nested_record_and_its_fields() {
        let src =
            "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action\n        desc: a\n";
        let inst = parse_inst(src);
        let rec = as_record(locate_field_path(
            &inst,
            &[
                seg_field("phases"),
                PathSegment::Index(0),
                seg_field("actions"),
                PathSegment::Index(0),
            ],
        ));
        // `type:` is the claim, not a field, so only `desc` is authored.
        assert_eq!(rec.fields.len(), 1);
        assert_eq!(rec.fields[0].key, "desc");
        assert_eq!(rec.fields[0].kind, ValueKind::Scalar);
        assert_eq!(
            &src[rec.fields[0].value_span.start..rec.fields[0].value_span.end],
            "a"
        );
        let ts = rec
            .type_span
            .expect("a claim-bearing record has a type span");
        assert_eq!(&src[ts.start..ts.end], "action");
    }

    #[test]
    fn locate_a_bare_type_only_record_has_no_fields_but_a_type_span() {
        let src = "type: plan\nphases:\n  - type: phase\n";
        let inst = parse_inst(src);
        let rec = as_record(locate_field_path(
            &inst,
            &[seg_field("phases"), PathSegment::Index(0)],
        ));
        assert!(
            rec.fields.is_empty(),
            "a bare type-only record has an empty fields list"
        );
        let ts = rec.type_span.expect("the type span anchors the insert");
        assert_eq!(&src[ts.start..ts.end], "phase");
    }

    #[test]
    fn locate_captures_a_block_id_span() {
        let src = "type: plan\nphases:\n  - ^: p1\n    type: phase\n";
        let inst = parse_inst(src);
        let rec = as_record(locate_field_path(
            &inst,
            &[seg_field("phases"), PathSegment::Index(0)],
        ));
        assert!(rec.block_id_span.is_some());
    }

    #[test]
    fn locate_a_sequence_and_its_elements() {
        let src = "type: plan\nphases:\n  - type: phase\n  - type: phase\n";
        let inst = parse_inst(src);
        let seq = as_sequence(locate_field_path(&inst, &[seg_field("phases")]));
        assert_eq!(seq.elements.len(), 2);
    }

    #[test]
    fn locate_an_empty_sequence() {
        let src = "type: plan\nphases: []\n";
        let inst = parse_inst(src);
        let seq = as_sequence(locate_field_path(&inst, &[seg_field("phases")]));
        assert!(seq.elements.is_empty());
    }

    #[test]
    fn locate_the_file_instance_for_an_empty_path() {
        let src = "type: plan\ntitle: t\n";
        let inst = parse_inst(src);
        let rec = as_record(locate_field_path(&inst, &[]));
        assert_eq!(rec.span, inst.source_span);
        assert_eq!(rec.fields.len(), 1);
        assert_eq!(rec.fields[0].key, "title");
    }

    #[test]
    fn locate_a_stale_path_is_none() {
        let src = "type: plan\nphases:\n  - type: phase\n";
        let inst = parse_inst(src);
        assert!(locate_field_path(&inst, &[seg_field("nope")]).is_none());
        assert!(
            locate_field_path(&inst, &[seg_field("phases"), PathSegment::Index(5)]).is_none(),
            "an out-of-range index resolves to no node"
        );
    }

    #[test]
    fn locate_a_scalar_leaf_is_none() {
        // A path ending at a scalar addresses neither a record nor a sequence.
        let src = "type: plan\ntitle: t\n";
        let inst = parse_inst(src);
        assert!(locate_field_path(&inst, &[seg_field("title")]).is_none());
    }

    #[test]
    fn enumerate_pins_a_claim_less_record_to_its_slot() {
        let g = graph_for(&[
            ("canvas", "fields:\n  root?: node\n"),
            ("node", "fields:\n  content?: String\n"),
        ]);
        // No explicit `type:` on the nested record, it pins to the slot's `node`.
        let recs = enum_for(&g, "type: canvas\nroot:\n  content: x\n");
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0]
                .claims
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>(),
            vec!["node"]
        );
        assert_eq!(recs[0].field_path, vec![seg_field("root")]);
        assert_eq!(recs[0].block_id, None);
    }

    #[test]
    fn extended_fold_memoizes_per_seed_set_within_a_walk() {
        // Finding 1.1 (code review 2609101214): sibling nested records sharing a
        // `::repo` seed set must re-use ONE extended fold, not re-fold per record.
        // The memo keys by sorted+deduped seed-set, so a repeated seed set returns
        // the SAME cached graph (ptr-equal) and the cache holds one entry; a
        // distinct seed set adds a second. Correctness-invisible (every behaviour
        // test stays byte-identical); this asserts the collapse directly.
        use crate::resolution::{fold, PeerGraphResolver};
        use std::collections::BTreeMap;

        let weave = graph_for(&[
            (
                "research-extraction",
                "fields:\n  concepts?: concept-candidate&[]\n",
            ),
            ("concept-candidate", "fields:\n  salience?: String\n"),
        ]);
        let app = graph_for(&[]);
        struct MapResolver {
            graphs: BTreeMap<String, TypeGraph>,
        }
        impl PeerGraphResolver for MapResolver {
            fn graph_of(&self, repo: &str) -> Option<&TypeGraph> {
                self.graphs.get(repo)
            }
        }
        let resolver = MapResolver {
            graphs: [
                ("app".to_string(), app.clone()),
                ("weave".to_string(), weave),
            ]
            .into_iter()
            .collect(),
        };
        let rg = fold(
            "app",
            &[(TypeName("research-extraction".into()), "weave".into())],
            &resolver,
        );

        let ctx = ResolveCtx::new(&app, Some(&rg), Some(&resolver), "app");
        let seeds = [(TypeName("concept-candidate".into()), "weave".to_string())];
        let first = ctx.extended_fold(&rg, &resolver, &seeds);
        let second = ctx.extended_fold(&rg, &resolver, &seeds);
        assert!(
            Rc::ptr_eq(&first, &second),
            "the same seed set must hit the memo, not re-fold"
        );
        assert_eq!(
            ctx.fold_cache.borrow().len(),
            1,
            "one cache entry for one distinct seed set"
        );

        // Order and duplicates must not matter: a reordered/duplicated seed set is
        // the same canonical key, so it hits the one entry.
        let dup = [
            (TypeName("concept-candidate".into()), "weave".to_string()),
            (TypeName("concept-candidate".into()), "weave".to_string()),
        ];
        let third = ctx.extended_fold(&rg, &resolver, &dup);
        assert!(
            Rc::ptr_eq(&first, &third),
            "a duplicate seed set is the same key"
        );
        assert_eq!(ctx.fold_cache.borrow().len(), 1);

        // A genuinely different seed set folds separately.
        let other = [(TypeName("research-extraction".into()), "weave".to_string())];
        let _ = ctx.extended_fold(&rg, &resolver, &other);
        assert_eq!(
            ctx.fold_cache.borrow().len(),
            2,
            "a distinct seed set adds its own entry"
        );
    }

    #[test]
    fn enumerate_pins_a_cross_repo_slot_pinned_record_to_the_owner_repo() {
        // The reported bug. A file claims a peer type whose slot pins
        // `concept-candidate`, a type OWNED by the peer. A claim-less inline record
        // in that slot must be discovered as `concept-candidate::weave`, resolved
        // owner-relative the way validation resolves it, not dropped.
        use crate::resolution::{fold, PeerGraphResolver};
        use std::collections::BTreeMap;

        let weave = graph_for(&[
            (
                "research-extraction",
                "fields:\n  concepts?: concept-candidate&[]\n",
            ),
            ("concept-candidate", "fields:\n  salience?: String\n"),
        ]);
        let app = graph_for(&[]);

        struct MapResolver {
            graphs: BTreeMap<String, TypeGraph>,
        }
        impl PeerGraphResolver for MapResolver {
            fn graph_of(&self, repo: &str) -> Option<&TypeGraph> {
                self.graphs.get(repo)
            }
        }
        let resolver = MapResolver {
            graphs: [
                ("app".to_string(), app.clone()),
                ("weave".to_string(), weave),
            ]
            .into_iter()
            .collect(),
        };
        // app imports `research-extraction::weave` via its instance claim.
        let rg = fold(
            "app",
            &[(TypeName("research-extraction".into()), "weave".into())],
            &resolver,
        );

        let inst = parse_inst("type: research-extraction::weave\nconcepts:\n  - salience: focal\n");
        let recs = enumerate_nested_records(&app, Some(&rg), Some(&resolver), "app", &inst);

        assert_eq!(
            recs.len(),
            1,
            "the claim-less nested record must be discovered via the owner-relative pin"
        );
        let qualified: Vec<String> = recs[0].qualified.iter().map(|c| c.authored()).collect();
        assert_eq!(
            qualified,
            vec!["concept-candidate::weave".to_string()],
            "a slot-pinned record's identity is qualified to the slot type's OWNER repo"
        );
    }
}
