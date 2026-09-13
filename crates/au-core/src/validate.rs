//! Pure instance validation: `(ValidateContext, Instance) -> Vec<Diagnostic>`.
//!
//! No I/O, no global state. The validator dispatches the closure walk
//! through `closure::effective_shape` and emits the [[type validation::au-type-system]] subset that doesn't
//! depend on sealed-leaf / inline-value / compound grammar (those light up
//! as their grammar lands).
//!
//! Per [[type open-world validation::au-type-system]]: extras (instance fields outside the closure) pass
//! silently. They become candidates for promotion via the candidate scan.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span, SuggestedFix};
use au_grammar::{CompoundRefOp, DefBound, Primitive, QualifiedName, RefMode, Shape};
use au_references::{
    looks_like_wikilink, RepoIndex, ResolutionError, WikilinkParseError, WikilinkRef,
};

use crate::closure::{
    closure_of, effective_shape, effective_shape_resolved, folded_closure_ids, EffectiveShape,
    EffectiveShapeError, FieldOrigin, OriginId,
};
use crate::codes;
use crate::graph::TypeGraph;
use crate::instance::{InlineValue, Instance, InstanceField, InstanceValue, TypeClaim};
use crate::load_checks::{
    check_redundant_claims, classify_union_member, is_valid_type_name, UnionMemberKind,
};
use crate::provenance::{elaborate_fields, ContributionValue, Surface};
use crate::resolution::ResolutionGraph;
use crate::typedef::{FieldName, MetaBlock, TypeDef, TypeName, TypeNameClaim};

/// Engine state the validator needs across every per-instance call: the
/// type graph, the repo index (resolves wikilink targets), and the
/// claims-by-path map (maps each instance file to its declared `type:`
/// names so reference-target closure checks don't re-parse target files).
///
/// Built once per CLI / LSP / watcher run; passed by reference to every
/// `validate` call. Holds nothing instance-specific.
pub struct ValidateContext<'a> {
    pub graph: &'a TypeGraph,
    pub repo_index: &'a RepoIndex,
    /// Resolves the per-target data the validator looks up by path: a target
    /// instance's type claims, its markdown body, and its addressable inline
    /// records. A seam, like [`cross_repo`], so the engine can back it by the
    /// held catalog and a per-instance validation looks up only the targets it
    /// references, never assembling a whole-knowledge-base map.
    ///
    /// [`cross_repo`]: ValidateContext::cross_repo
    pub ref_data: &'a dyn RefData,
    /// Resolver for `[[name::repo]]` references that cross a repo boundary.
    ///
    /// au-core is repo-agnostic and sees only one graph, so it cannot reach a
    /// target in another repo. The engine supplies this seam; au-core calls it
    /// to resolve the `::repo` target into the named repo's graph, then
    /// type-checks the slot by `(name, canonical-hash)` identity across the
    /// boundary. `None` (au-core's own tests) skips the cross-repo typed check;
    /// the existence and `reference-repo-*` diagnostics live in the engine.
    pub cross_repo: Option<&'a dyn CrossRepoResolver>,
    /// R's cross-repo resolution graph, present when R imports any peer type
    /// ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]).
    ///
    /// When `Some`, claim / parent resolution runs over it, so a `type: foo::repo`
    /// gathers the folded peer fields; `None` (a non-importing repo, or au-core's
    /// own tests) falls back to the own `graph`, unchanged. The own graph stays
    /// for the own-graph-only checks. Field-shape demands stay on `cross_repo`.
    pub resolution: Option<&'a ResolutionGraph>,
    /// The engine's nominal meta marker identity, injected by au-engine so au-core
    /// stays domain-pure ([[spec - meta type marker - the meta position admits only types that mix in the engine meta base]]).
    /// A `meta:` block's type is meta-legal only if its RESOLVED closure includes
    /// this `(name, repo)`. `None` (au-core's own tests, or a build that does not
    /// supply it) skips the nominal check.
    pub meta_marker: Option<MetaMarker<'a>>,
}

/// The engine meta marker identity, `(au.engine.meta, au-engine)`, held by
/// reference so au-core never hardcodes an engine name. Injected via
/// [`ValidateContext::meta_marker`].
#[derive(Debug, Clone, Copy)]
pub struct MetaMarker<'a> {
    /// The marker type-def name, `au.engine.meta`.
    pub name: &'a str,
    /// The repo that owns it, the builtin `au-engine`.
    pub repo: &'a str,
}

/// Resolves the per-target data the validator looks up by path during
/// reference checks: a target instance's type claims, its markdown body (for
/// cross-file `^block-id` resolution), and its addressable inline records
/// ([[type block-id::au-type-system]]).
///
/// A seam so the caller chooses the backing. The whole-knowledge-base path uses
/// [`MapRefData`] over pre-assembled maps; the engine's incremental path backs
/// it by the held catalog, so validating one instance looks up only the targets
/// it references rather than assembling a map over every file.
pub trait RefData {
    /// The target's declared `type:` claim names, `None` when the path is not a
    /// known instance.
    ///
    /// Returns a [`Cow`] so a map-backed resolver borrows its stored slice while
    /// a catalog-backed one computes the names on demand from the target's
    /// `TypeClaim` and returns them owned, without holding a whole-knowledge-base map.
    fn claims(&self, path: &Path) -> Option<Cow<'_, [TypeName]>>;
    /// The target's markdown body text, `None` when the path is not a markdown
    /// instance. Always borrowed: the body string lives in the held parse, so
    /// every backing can lend it directly.
    fn body(&self, path: &Path) -> Option<&str>;
    /// The target's addressable inline records, `None` when the path has none.
    ///
    /// [`Cow`] for the same reason as [`claims`]: the records are derived
    /// (`collect_record_targets`), so a catalog-backed resolver computes them on
    /// demand and returns them owned.
    ///
    /// [`claims`]: RefData::claims
    fn record_targets(&self, path: &Path) -> Option<Cow<'_, crate::record_targets::RecordTargets>>;
}

/// A [`RefData`] backed by three pre-assembled maps, the whole-knowledge-base build's
/// resolver and the shape au-core's own tests construct. Every lookup borrows
/// from the maps, so the full build allocates nothing per reference.
pub struct MapRefData<'a> {
    pub claims_by_path: &'a BTreeMap<PathBuf, Vec<TypeName>>,
    pub body_sources: &'a BTreeMap<PathBuf, String>,
    pub record_targets: &'a BTreeMap<PathBuf, crate::record_targets::RecordTargets>,
}

impl RefData for MapRefData<'_> {
    fn claims(&self, path: &Path) -> Option<Cow<'_, [TypeName]>> {
        self.claims_by_path
            .get(path)
            .map(|v| Cow::Borrowed(v.as_slice()))
    }
    fn body(&self, path: &Path) -> Option<&str> {
        self.body_sources.get(path).map(String::as_str)
    }
    fn record_targets(&self, path: &Path) -> Option<Cow<'_, crate::record_targets::RecordTargets>> {
        self.record_targets.get(path).map(Cow::Borrowed)
    }
}

/// Resolves a `[[name::repo]]`-qualified reference into the named repo, the
/// engine-supplied seam that lets repo-agnostic au-core type-check across a
/// repo boundary.
///
/// The engine owns the workspace's repos, so only it can map a repo label to a
/// graph and resolve a target within it. au-core consumes the result and
/// compares the source slot's demanded type against the target's effective type
/// closure by canonical-hash identity.
pub trait CrossRepoResolver: Sync {
    /// Resolve `target` in repo `repo` as referenced from `source`.
    ///
    /// `Some` when the named repo is present and holds the target: the resolved
    /// path plus that repo's graph. `None` when the reference is unresolvable
    /// here (unknown repo, absent peer, missing target), leaving its existence
    /// diagnostics to the engine's cross-repo pass.
    fn resolve(&self, source: &Path, repo: &str, target: &str) -> Option<CrossRepoTarget<'_>>;

    /// Identity inputs for a qualified DEMANDED type (`foo::repo*`) referencing a
    /// resolved target file, the qualified sibling of the unqualified reference
    /// check. The seam yields the demanded `TypeId` (the demand repo's identity for
    /// `demand_base`) and the target's FULL folded closure ids (the target's repo
    /// resolution graph over the target's parsed, still-qualified claims); au-core
    /// does the membership.
    ///
    /// `target_claim` overrides which of the target's claims is folded: `None`
    /// folds the target FILE's frontmatter claim (a whole-file target, engine-read
    /// from the parse), `Some(claim)` folds a given claim (a `^block-id` target's
    /// own BLOCK claim, which au-core resolved).
    ///
    /// `None` when the demand repo (or the target's repo) is unresolvable or has a
    /// broken vocabulary, or when the override carries a `::repo` claim the target
    /// repo did not fold (an ungated body position), so the reference check skips
    /// rather than false-fires (the `crosstype` gate owns the demand-repo
    /// diagnostic).
    ///
    /// The default returns `None`: a resolver with no workspace behind it
    /// (au-core's own tests) cannot fold, so a qualified demand is left unchecked
    /// here, never false-flagged.
    fn qualified_demand(
        &self,
        _demand_base: &str,
        _demand_repo: &str,
        _target_path: &Path,
        _target_claim: Option<&TypeClaim>,
    ) -> Option<QualifiedDemand> {
        None
    }

    /// The effective shape of the demanded peer type `demand_base` as
    /// `demand_repo` defines it, for validating an INLINE value at a qualified
    /// demand (`foo::repo`, or the inline branch of `foo::repo&`). The inline
    /// map's fields are checked against the OWNER repo's contract, not the source
    /// repo's, the fold-vs-demand rule's inline case
    /// ([[example - cross-repo type fold versus field demand, a worked verification trace]] case 3).
    ///
    /// `None` when the demand repo is unresolvable / has a broken vocabulary, the
    /// type is absent, or it is SEALED (an inline at a sealed peer slot needs an
    /// explicit descendant type, deferred), so the inline typed check skips.
    ///
    /// The default returns `None`: a resolver with no workspace behind it
    /// (au-core's own tests) cannot reach the owner, so the check is skipped.
    fn owner_effective_shape(
        &self,
        _demand_base: &str,
        _demand_repo: &str,
    ) -> Option<EffectiveShape> {
        None
    }

    /// The demanded peer type's identity and sealed-ness at a qualified INLINE
    /// demand (`foo::repo` / the inline branch of `foo::repo&`), for the inline
    /// sibling of the reference membership check. `id` is
    /// `(demand_base, demand_repo.closure_id(demand_base))`, the same identity a
    /// reference demand compares; `sealed` is whether the peer type is a sealed
    /// parent (an inline at a sealed peer slot needs an explicit non-sealed
    /// descendant, so an omitted `type:` there is `inline-value-missing-type`).
    ///
    /// au-core compares the demanded `id` against the inline claim's FOLDED
    /// closure (over the SOURCE resolution graph, where the inline's own `::repo`
    /// claim is a fold seed), the inline sibling of
    /// [`CrossRepoResolver::qualified_demand`].
    ///
    /// `None` when the demand repo is unresolvable / has a broken vocabulary or
    /// the type is absent, so the inline check skips (the `type-repo-*` gate owns
    /// the diagnostic). The default returns `None`: a resolver with no workspace
    /// behind it (au-core's own tests) cannot reach the demand repo.
    fn peer_type_id(&self, _demand_base: &str, _demand_repo: &str) -> Option<PeerType> {
        None
    }

    /// The whole [`TypeGraph`] of a peer repo by its global name, the seam a
    /// cross-repo body `use: parent::repo` splices through. A body `use:` pulls a
    /// peer type's body SECTIONS, which live on the peer's def in the peer's graph
    /// (the resolution graph holds parent/field ids, not bodies), so the splice
    /// needs the peer graph directly. `None` for an absent / undeclared repo (the
    /// use stays unspliced, the `crosstype` gate owns its diagnostic); the default
    /// `None` leaves au-core's own tests splicing own bodies only.
    fn peer_graph(&self, _repo: &str) -> Option<&TypeGraph> {
        None
    }
}

/// The engine-supplied identity of a qualified INLINE demand's peer type, see
/// [`CrossRepoResolver::peer_type_id`]. au-core checks `id` for membership in the
/// inline claim's folded closure, and reads `sealed` to require an explicit
/// descendant at a sealed peer slot.
pub struct PeerType {
    /// The demanded peer type's identity in the demand repo,
    /// `(demand_base, demand_repo.closure_id(demand_base))`.
    pub id: crate::resolution::TypeId,
    /// Whether the peer type is a sealed parent, which an inline value may not
    /// claim (nor default to); it must declare a non-sealed descendant.
    pub sealed: bool,
    /// Whether the peer type declares `abstract: true` (the raw flag). Like
    /// `sealed`, a non-claimable ceiling: an inline value at an abstract peer
    /// demand must declare an explicit concrete `type:`, never default to the
    /// ceiling's own identity.
    pub declared_abstract: bool,
}

/// A `::repo` reference resolved into another repo: the target file plus the
/// graph that repo's types live in, for the cross-boundary identity check.
pub struct CrossRepoTarget<'a> {
    pub path: PathBuf,
    pub graph: &'a TypeGraph,
}

/// The engine-supplied operands for a qualified-demand reference membership
/// (`foo::repo*`), see [`CrossRepoResolver::qualified_demand`]. au-core compares
/// `demanded ∈ target_folded`, the qualified sibling of
/// [`target_closure_includes_cross_repo`].
pub struct QualifiedDemand {
    /// The demanded type's identity in the demand repo,
    /// `(demand_base, demand_repo.closure_id(demand_base))`.
    pub demanded: crate::resolution::TypeId,
    /// The target value's FULL folded closure ids. Full, not reduced to a boolean,
    /// so a future diamond diagnostic can tell a name-absent target from a
    /// closure-diverged one (a `foo` at a different hash) by `TypeId` comparison
    /// over au-core's own vocabulary.
    pub target_folded: BTreeSet<crate::resolution::TypeId>,
}

/// Compute a claim's effective shape over the right layer: the cross-repo
/// resolution graph when R imports (so a `::repo` claim resolves), else the own
/// graph unchanged. The single seam every effective-shape call in the validator
/// routes through.
/// Classic Levenshtein edit distance. Field names are short, so the O(n*m) DP
/// is cheap, and it runs only per missing required field.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Undeclared bare present keys, the near-miss candidates for a missing
/// required field. A qualified `field{origin}` key is not a field-name typo, so
/// it is excluded (a qualified key is the only key form carrying a `{`).
fn undeclared_keys<'a>(
    shape: &EffectiveShape,
    fields: &'a [InstanceField],
) -> Vec<(&'a str, ByteRange)> {
    let declared: BTreeSet<&str> = shape.iter().map(|(n, _)| n.as_str()).collect();
    fields
        .iter()
        .filter(|f| !f.key.contains('{') && !declared.contains(f.key.as_str()))
        .map(|f| (f.key.as_str(), f.key_span))
        .collect()
}

/// The nearest undeclared present key to a missing field name, within a small
/// edit distance, so a typo'd key (`siize` for `size`) surfaces as a hint
/// rather than reading as a plain absence. Deterministic: smallest distance,
/// then the lexicographically smallest key. `None` when nothing is close.
fn nearest_undeclared_field<'a>(
    missing: &str,
    undeclared: &[(&'a str, ByteRange)],
) -> Option<(&'a str, ByteRange)> {
    const MAX_DISTANCE: usize = 2;
    undeclared
        .iter()
        .filter_map(|&(key, span)| {
            let d = levenshtein(missing, key);
            (1..=MAX_DISTANCE).contains(&d).then_some((d, key, span))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, key, span)| (key, span))
}

pub(crate) fn effective_shape_for(
    ctx: &ValidateContext,
    claim: &TypeClaim,
) -> Result<EffectiveShape, EffectiveShapeError> {
    match ctx.resolution {
        Some(rg) => effective_shape_resolved(rg, ctx.graph, claim),
        None => effective_shape(ctx.graph, claim),
    }
}

/// Shape-conformance for ONE scalar value contributed from the body.
///
/// The frontmatter pass walks `instance.fields`, so a value arriving only from
/// the body — an inline `` `[:field] value` `` marker, or a text fence — reaches
/// its field without ever being compared to the slot. [[type-instance body contribution::au-type-system]]
/// says a contribution whose value misses the slot shape is an error, so this is
/// that check for the scalar carrier.
///
/// `slot` is the slot's ELEMENT shape: a body contribution supplies one element,
/// never a whole list, so a `String[]` slot checks its member against `String`.
pub(crate) fn check_body_scalar_value(
    ctx: &ValidateContext,
    instance_path: &Path,
    value: &InstanceValue,
    value_span: ByteRange,
    slot: &Shape,
    field_key: &str,
    origin_path: &Path,
    shape_span: ByteRange,
) -> Vec<Diagnostic> {
    let scope = Scope {
        ctx,
        instance_path,
        model: None,
    };
    let origin = Origin {
        path: origin_path,
        shape_span,
    };
    check_value_against_shape(&scope, value, value_span, slot, field_key, &origin, None)
}

/// Validate a body-originated reference container (an inline-code contribution
/// the elaborator classified into a [`ContributionValue::Reference`] /
/// [`ContributionValue::MalformedReference`]) against its slot.
///
/// It carries the node in a one-entry model and runs the SAME reference arms the
/// frontmatter surface uses, so a body inline-code reference keeps the frontmatter
/// reference codes (`reference-target-type-mismatch`, the pin diagnostics, …) it
/// had before the value model classified it, and never re-parses the surface, the
/// value-model invariant, see
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
/// Prose references (`Surface::BodyWikilink`) stay with the body reference pass
/// and its `body-slot-shape-mismatch`, so the caller gates this to inline-code /
/// fence-origin containers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_body_reference_container(
    ctx: &ValidateContext,
    instance_path: &Path,
    node: &ContributionValue,
    brand: Option<&str>,
    value_span: ByteRange,
    slot: &Shape,
    field_key: &str,
    origin_path: &Path,
    shape_span: ByteRange,
) -> Vec<Diagnostic> {
    // The reference arms read the node from `scope.model` by span; a
    // reconstructed surface string stands in for the value the arm matches on and
    // for the `raw_display` a local `[[^id]]` message names. A non-local
    // reference never consults it, so the reconstruction is exact where it shows.
    let display = reconstruct_reference_display(node);
    let model = FieldModel::single(value_span, node.clone(), brand.map(str::to_string));
    let scope = Scope {
        ctx,
        instance_path,
        model: Some(&model),
    };
    let origin = Origin {
        path: origin_path,
        shape_span,
    };
    check_value_against_shape(
        &scope,
        &InstanceValue::String(display),
        value_span,
        slot,
        field_key,
        &origin,
        None,
    )
}

/// Reconstruct a wikilink surface string from a resolved reference node, for the
/// value the reference arms match on and the `raw_display` a local-form message
/// names. A [`ContributionValue::MalformedReference`] carries its exact raw; a
/// [`ContributionValue::Reference`] is rebuilt from its fragments (only consulted
/// for the local `[[^id]]` form, so the fragment order is immaterial elsewhere).
fn reconstruct_reference_display(node: &ContributionValue) -> String {
    match node {
        ContributionValue::MalformedReference(_, raw) => raw.clone(),
        ContributionValue::Reference {
            target,
            repo,
            commit,
            anchor,
            block_id,
        } => {
            let mut inner = target.clone();
            if let Some(r) = repo {
                inner.push_str("::");
                inner.push_str(r);
            }
            if let Some(c) = commit {
                inner.push('@');
                inner.push_str(c);
            }
            if let Some(b) = block_id {
                inner.push('^');
                if b.referent {
                    inner.push('^');
                }
                inner.push_str(&b.id);
            }
            if let Some(a) = anchor {
                inner.push('#');
                inner.push_str(a);
            }
            format!("[[{inner}]]")
        }
        _ => String::new(),
    }
}

/// Validate ONE contributed value against its slot, from the typed value model,
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
///
/// The single value-verdict walker: it dispatches on the [`ContributionValue`]
/// kind and never re-parses a surface string. The body path consumes it today;
/// the frontmatter path adopts it once the remaining kinds land, retiring the
/// raw-string re-parse.
///
/// `slot` is the slot's ELEMENT shape: one contribution supplies one element,
/// never a whole list.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_contribution_value(
    ctx: &ValidateContext,
    instance_path: &Path,
    value: &ContributionValue,
    written_brand: Option<&str>,
    value_span: ByteRange,
    slot: &Shape,
    field_key: &str,
    element_index: Option<usize>,
    origin_path: &Path,
    shape_span: ByteRange,
) -> Vec<Diagnostic> {
    match value {
        ContributionValue::Scalar(iv) => {
            // At a BRAND slot the verdict is computed entirely from the model here,
            // never delegating back to the re-parsing arm ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
            // `effective_values` resolved a constructor to (underlying value,
            // written brand), so the brand rides on the container. A NOMINAL brand:
            // a written brand must name it, then the underlying value checks. A
            // UNION brand: a written brand picks the member, a bare value (no
            // brand) runs the ambiguity / coercion check. This is the one path both
            // surfaces use, so the reserved-primitive escape and union-of-nominals
            // cannot diverge frontmatter-versus-body. A non-brand slot falls to
            // `check_body_scalar_value`.
            if let Some(name) = brand_slot_name(slot) {
                let scope = Scope {
                    ctx,
                    instance_path,
                    model: None,
                };
                if let Some(brand) = resolve_brand(&scope, name) {
                    let origin = Origin {
                        path: origin_path,
                        shape_span,
                    };
                    let prefix = format_field_prefix(field_key, element_index);
                    if brand.is_nominal() {
                        if let Some(written) = written_brand {
                            let brand_name = name.to_string();
                            if written != brand_name {
                                return vec![brand_constructor_mismatch_diag(
                                    &scope,
                                    value_span,
                                    &origin,
                                    format!(
                                        "{} constructor names brand '{}', but the slot demands '{}'",
                                        prefix, written, brand_name
                                    ),
                                )];
                            }
                        }
                        return check_value_against_shape(
                            &scope,
                            iv,
                            value_span,
                            &brand.shape,
                            field_key,
                            &origin,
                            element_index,
                        );
                    }
                    if let Shape::Union(members) = &brand.shape {
                        let Some((mg, mr)) = brand_member_graph(&scope, name) else {
                            // Unresolvable members: skip, like the frontmatter arm.
                            return Vec::new();
                        };
                        return match written_brand {
                            // A written brand picks the member (nominal / escape).
                            Some(written) => discriminate_scalar_union_by_brand(
                                &scope,
                                iv,
                                value_span,
                                &name.to_string(),
                                members,
                                mg,
                                mr,
                                written,
                                field_key,
                                &origin,
                                element_index,
                                &prefix,
                            ),
                            // A bare value: coerce to a unique member, else ambiguous.
                            None => check_bare_against_union(
                                &scope,
                                iv,
                                value_span,
                                &name.to_string(),
                                members,
                                mg,
                                field_key,
                                &origin,
                                element_index,
                                &prefix,
                            ),
                        };
                    }
                }
            }
            check_body_scalar_value(
                ctx,
                instance_path,
                iv,
                value_span,
                slot,
                field_key,
                origin_path,
                shape_span,
            )
        }
        // A `Name(...)`-shaped string that does not close well-formed. The
        // graph-free producer ([`effective_values`]) mints this node at ANY
        // `Record` / `&` / `*` brand-bearing slot, so it can appear at a slot
        // whose name is NOT a brand (an ordinary `foo*` reference type). It is a
        // genuine malformed BRAND constructor ONLY when the slot resolves to a
        // brand; there `malformed-constructor` is the same verdict the raw brand
        // arm emitted. Otherwise the value is just an invalid string for the slot,
        // so reproduce the slot's own verdict (as the frontmatter fallback does),
        // keeping the two surfaces byte-identical. This gates the arm the way the
        // `Scalar` arm gates itself on `resolve_brand`.
        ContributionValue::MalformedConstructor(raw) => {
            let scope = Scope {
                ctx,
                instance_path,
                model: None,
            };
            let origin = Origin {
                path: origin_path,
                shape_span,
            };
            let is_brand = brand_slot_name(slot)
                .and_then(|n| resolve_brand(&scope, n))
                .is_some();
            if is_brand {
                let prefix = format_field_prefix(field_key, element_index);
                vec![Diagnostic {
                    code: codes::MALFORMED_CONSTRUCTOR,
                    severity: Severity::Warning,
                    span: Span::new(instance_path.to_path_buf(), value_span),
                    message: format!(
                        "{} value looks like a `Name(...)` constructor but is malformed",
                        prefix
                    ),
                    related: vec![origin.related_span()],
                    fix: None,
                }]
            } else {
                // Not a brand slot: the producer mis-classified a plain invalid
                // string. Validate the raw string against the slot, the same
                // verdict the frontmatter re-parse fallback reaches.
                check_value_against_shape(
                    &scope,
                    &InstanceValue::String(raw.clone()),
                    value_span,
                    slot,
                    field_key,
                    &origin,
                    element_index,
                )
            }
        }
        // A fixed-arity positional product ([[type-def shape tuple::au-type-system]]). Validate
        // the model tuple against the slot: a named tuple brand's name must match,
        // arity must match, and each element validates against its position shape.
        ContributionValue::Tuple(elements) => {
            let scope = Scope {
                ctx,
                instance_path,
                model: None,
            };
            let origin = Origin {
                path: origin_path,
                shape_span,
            };
            let prefix = format_field_prefix(field_key, element_index);
            // Resolve the declared element shapes, and (at a brand slot) check the
            // written brand names the slot's brand.
            let element_shapes: Option<Vec<Shape>> = match slot {
                Shape::Tuple(els) => Some(els.clone()),
                Shape::Record(name) => match resolve_brand(&scope, name) {
                    Some(brand) => match &brand.shape {
                        // A nominal TUPLE brand named directly: the written brand
                        // must be this brand (a bare nameless tuple coerces).
                        Shape::Tuple(els) => {
                            if let Some(written) = written_brand {
                                let brand_name = name.to_string();
                                if written != brand_name {
                                    return vec![brand_constructor_mismatch_diag(
                                        &scope,
                                        value_span,
                                        &origin,
                                        format!(
                                            "{} constructor names brand '{}', but the slot demands '{}'",
                                            prefix, written, brand_name
                                        ),
                                    )];
                                }
                            }
                            Some(els.clone())
                        }
                        // A UNION brand: the written brand names a member whose
                        // TUPLE shape gives the element shapes. The body twin of
                        // `check_structural_brand_value`'s constructor arm, closing
                        // the union-of-tuple-brand divergence (a `point(1, 2)` at a
                        // `<point | second>` slot on the body). See
                        // [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
                        Shape::Union(members) => {
                            let Some((mg, mr)) = brand_member_graph(&scope, name) else {
                                // Unresolvable members: skip, like the scalar path.
                                return Vec::new();
                            };
                            let Some(written) = written_brand else {
                                return vec![brand_constructor_required_diag(
                                    &scope,
                                    value_span,
                                    &origin,
                                    format!(
                                        "{} tuple value at union brand '{}' needs a `Name(...)` constructor to pick a member",
                                        prefix, name
                                    ),
                                )];
                            };
                            let qualify = |qn: &QualifiedName| match mr {
                                Some(r) => qn.qualified_to(r),
                                None => qn.clone(),
                            };
                            let member = members.iter().find_map(|m| {
                                if classify_member_xrepo(&scope, m, mg)
                                    != UnionMemberKind::NominalBrand
                                {
                                    return None;
                                }
                                let (Shape::Record(qn)
                                | Shape::Reference(qn)
                                | Shape::InlineOrReference(qn)) = m
                                else {
                                    return None;
                                };
                                if qualify(qn).to_string() != written {
                                    return None;
                                }
                                resolve_member_brand(&scope, qn, mg)
                            });
                            match member.map(|b| &b.shape) {
                                Some(Shape::Tuple(els)) => Some(els.clone()),
                                // Named a member that is not a tuple, or a name that
                                // is no member: a mismatch, mirroring the scalar path.
                                _ => {
                                    return vec![brand_constructor_mismatch_diag(
                                        &scope,
                                        value_span,
                                        &origin,
                                        format!(
                                            "{} constructor names brand '{}', not a tuple member of union '{}'",
                                            prefix, written, name
                                        ),
                                    )];
                                }
                            }
                        }
                        _ => None,
                    },
                    None => None,
                },
                _ => None,
            };
            let Some(els) = element_shapes else {
                return vec![field_shape_mismatch_diag(
                    &scope,
                    value_span,
                    &origin,
                    format!("{} value is a tuple, but the slot is {}", prefix, slot),
                )];
            };
            if elements.len() != els.len() {
                return vec![tuple_arity_mismatch_diag(
                    &scope,
                    value_span,
                    &origin,
                    els.len(),
                    elements.len(),
                    &prefix,
                )];
            }
            let mut diags = Vec::new();
            for (el, el_shape) in elements.iter().zip(&els) {
                diags.extend(check_contribution_value(
                    ctx,
                    instance_path,
                    &el.value,
                    el.brand.as_deref(),
                    value_span,
                    el_shape,
                    field_key,
                    // A tuple sub-element is not a list element, so it carries no
                    // "element N" index of its own.
                    None,
                    origin_path,
                    shape_span,
                ));
            }
            diags
        }
        // A reference's value verdicts are owned by the slot-gated reference
        // checks today; the reference value-kind routes them here in a later
        // step, carrying the parsed structure (incl. `commit`).
        ContributionValue::Reference { .. } => Vec::new(),
        // An inline record's verdicts are owned by the marked-fence pass today.
        ContributionValue::InlineRecord(_) => Vec::new(),
        // Malformed references land in their own step.
        ContributionValue::MalformedReference(..) => Vec::new(),
    }
}

/// Discriminate a resolved SCALAR value at a union brand slot by the WRITTEN
/// brand the model carries — the body-path parallel of the frontmatter re-parse
/// in [`check_structural_brand_value`].
///
/// `effective_values` already resolved `meter(42)` to `(value 42, brand
/// "meter")`, so the member is picked by the brand name, never re-parsed. This
/// mirrors the constructor arm of [`check_structural_brand_value`]: a nominal
/// member named by the brand validates the value against its underlying shape, a
/// reserved-primitive brand names a primitive member (the escape), and any other
/// name is a mismatch. A bare value (no written brand) never reaches here — its
/// container has `brand: None`, so the [`check_contribution_value`] `Scalar` arm
/// delegates it to the ambiguity check instead. Tuple / enum members follow the
/// same underlying-shape check; a tuple VALUE comes through the `Tuple` arm, not
/// here.
#[allow(clippy::too_many_arguments)]
fn discriminate_scalar_union_by_brand(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    union_name: &str,
    members: &[Shape],
    member_graph: &TypeGraph,
    member_repo: Option<&str>,
    written: &str,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
    prefix: &str,
) -> Vec<Diagnostic> {
    let kind = |m: &Shape| classify_member_xrepo(scope, m, member_graph);
    let qualify = |qn: &QualifiedName| match member_repo {
        Some(r) => qn.qualified_to(r),
        None => qn.clone(),
    };
    // A nominal member named by the written brand: validate the resolved value
    // against its underlying shape.
    let nominal = members.iter().find_map(|m| {
        if kind(m) != UnionMemberKind::NominalBrand {
            return None;
        }
        let (Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn)) = m else {
            return None;
        };
        if qualify(qn).to_string() != written {
            return None;
        }
        resolve_member_brand(scope, qn, member_graph)
    });
    if let Some(brand) = nominal {
        return check_value_against_shape(
            scope,
            value,
            value_span,
            &brand.shape,
            field_key,
            origin,
            element_index,
        );
    }
    // The reserved-primitive escape: the written brand names a primitive member,
    // forcing that branch.
    if let Some(prim) = members
        .iter()
        .find(|m| kind(m) == UnionMemberKind::Bare && primitive_keyword(m) == Some(written))
    {
        return check_value_against_shape(
            scope,
            value,
            value_span,
            prim,
            field_key,
            origin,
            element_index,
        );
    }
    // Not a member of the union: a foreign brand, or a record member named by a
    // constructor (a record is claimed with an inline `type:`, not a constructor).
    let record_named = members.iter().any(|m| {
        matches!(m, Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn)
            if qualify(qn).to_string() == written)
    });
    let detail = if record_named {
        format!(
            "{} constructor names record member '{}' of union '{}'; a record member is claimed with an inline `type:`, not a constructor",
            prefix, written, union_name
        )
    } else {
        format!(
            "{} constructor names brand '{}', not a member of union '{}'",
            prefix, written, union_name
        )
    };
    vec![brand_constructor_mismatch_diag(
        scope, value_span, origin, detail,
    )]
}

/// Validate a body typed-block's inline-record field VALUES against `shape`,
/// recursively and resolution-aware — the value-conformance half of
/// [`check_inline_fields`], exposed for `body_validate`'s embedded-block check.
///
/// The shallow embedded check only verified required-fields-PRESENCE. This adds
/// the per-field-value validation it omitted, so a nested `type: peer::repo`
/// record inside a fence validates like the same record in frontmatter.
pub(crate) fn validate_body_block_values(
    ctx: &ValidateContext,
    instance_path: &Path,
    inline: &InlineValue,
    shape: &EffectiveShape,
) -> Vec<Diagnostic> {
    // The fence embedded record's value model, so a reference / brand / pin value
    // verdict consumes the elaborated node instead of re-parsing, the value-model
    // invariant, the same seam `check_inline_fields` and `validate_meta_subregion`
    // use ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
    let model = build_field_model(&inline.fields, instance_path, shape);
    let scope = Scope {
        ctx,
        instance_path,
        model: Some(&model),
    };
    let mut diags = Vec::new();
    for f in &inline.fields {
        let Some(field_origin) = shape.get(&FieldName(f.key.clone())) else {
            continue;
        };
        let decl = field_origin.canonical_decl();
        if let Ok(parsed) = &decl.parsed_shape {
            let origin = Origin {
                path: field_origin.canonical_origin_path(),
                shape_span: decl.shape_span,
            };
            diags.extend(check_value_against_shape(
                &scope,
                &f.value,
                f.value_span,
                parsed,
                &f.key,
                &origin,
                None,
            ));
        }
    }
    diags
}

/// True if a claimed type is a sealed parent, which an instance may not claim
/// directly. An own claim reads the own graph; a `::repo` claim reads the folded
/// peer node in the resolution graph (sealed-ness is the peer's, surfaced here).
fn claim_is_sealed(ctx: &ValidateContext, claim: &TypeNameClaim) -> bool {
    match (claim.is_qualified(), ctx.resolution) {
        (true, Some(rg)) => rg
            .resolve_authored(&claim.name, claim.repo.as_deref())
            .and_then(|id| rg.get(id))
            .is_some_and(|node| !node.sealed.is_empty()),
        // Qualified but no resolution graph (unreachable in a built knowledge base, the
        // engine folds a repo that uses `::repo`): defer, do not false-fire.
        (true, None) => false,
        (false, _) => ctx.graph.is_sealed(&claim.name),
    }
}

/// True if a claimed type declares `abstract: true` (the raw flag, not the
/// sealed-implies-abstract predicate). Mirror of [`claim_is_sealed`]: an own
/// claim reads the own graph, a `::repo` claim reads the folded peer node. Used
/// to fire `abstract-type-claimed` for a declared-abstract, non-sealed claim,
/// without double-firing where `sealed-parent-claimed` already covers it.
fn claim_is_declared_abstract(ctx: &ValidateContext, claim: &TypeNameClaim) -> bool {
    match (claim.is_qualified(), ctx.resolution) {
        (true, Some(rg)) => rg
            .resolve_authored(&claim.name, claim.repo.as_deref())
            .and_then(|id| rg.get(id))
            .is_some_and(|node| node.declared_abstract),
        (true, None) => false,
        (false, _) => ctx.graph.declared_abstract_of(&claim.name),
    }
}

/// The `abstract-type-claimed` diagnostic for a claim naming a declared-abstract
/// type. Shared by every claim site (file-level, inline record, meta sub-region)
/// so the message and code stay identical.
fn abstract_type_claimed_diag(path: &Path, span: ByteRange, name: &str) -> Diagnostic {
    Diagnostic {
        code: codes::ABSTRACT_TYPE_CLAIMED,
        severity: Severity::Error,
        span: Span::new(path.to_path_buf(), span),
        message: format!(
            "type-def '{}' is abstract and cannot be claimed directly; claim a concrete subtype",
            name
        ),
        related: vec![],
        fix: None,
    }
}

/// Validate `instance` against `ctx.graph`, returning all per-instance
/// diagnostics. Pure; safe to run in parallel across instances against a
/// shared immutable context.
pub fn validate(ctx: &ValidateContext, instance: &Instance) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let path = &instance.source_path;

    // Redundant-claim warnings ([[type-instance type::au-type-system]]) — duplicate-claim and
    // subsumption-in-mixin. Run before effective_shape so duplicate
    // detection still fires when an unknown name would otherwise short-
    // circuit the validator. No-op for bare claims and 1-element lists.
    if let TypeClaim::List { items, .. } = &instance.type_claim {
        diags.extend(check_redundant_claims(
            ctx.graph,
            ctx.resolution,
            items,
            path,
        ));
    }

    let shape = match effective_shape_for(ctx, &instance.type_claim) {
        Ok(s) => s,
        Err(EffectiveShapeError::UnknownType(name)) => {
            diags.push(Diagnostic {
                code: codes::UNKNOWN_TYPE_CLAIM,
                severity: Severity::Error,
                span: Span::new(path.clone(), claim_span(&instance.type_claim)),
                message: format!(
                    "type-def '{}' is not present in the type graph",
                    name.as_str()
                ),
                related: vec![],
                fix: None,
            });
            return diags;
        }
    };

    // Mixin-collision ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) for a divergent field fires only on a BARE use,
    // handled in the per-field loop below (a divergent field is in `shape.divergent`,
    // resolved per-origin by a `field{type}` qualifier). An untouched divergent
    // field is clean; every use must qualify.

    // Sealed-leaf rule ([[type-def sealed::au-type-system]]) — per-claim. A non-sealed sibling
    // in a mixin does not excuse a sealed claim; duplicates fire once
    // per element (with `duplicate-claim` surfacing the repetition).
    // Inline-value `type:` keys are checked in `validate_inline_value`.
    for claim_el in instance.type_claim.iter() {
        // Own sealed-ness reads the own graph; a `::repo` claim's reads the
        // folded peer node, so a sealed peer parent claimed directly still fires.
        // Sealed is the more specific case, so it wins; a declared-abstract,
        // non-sealed claim fires `abstract-type-claimed` instead, never both.
        if claim_is_sealed(ctx, claim_el) {
            diags.push(Diagnostic {
                code: codes::SEALED_PARENT_CLAIMED,
                severity: Severity::Error,
                span: Span::new(path.clone(), claim_el.span),
                message: format!(
                    "type-def '{}' is sealed; claims must drill to a non-sealed descendant",
                    claim_el.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        } else if claim_is_declared_abstract(ctx, claim_el) {
            diags.push(abstract_type_claimed_diag(
                path,
                claim_el.span,
                claim_el.name.as_str(),
            ));
        }
    }

    diags.extend(check_multi_leaf_in_sealed_family(
        ctx.graph,
        ctx.resolution,
        path.as_path(),
        &instance.type_claim,
    ));

    // The frontmatter value model for this surface, produced once so a
    // reference / brand value verdict consumes the node instead of re-parsing.
    let model = build_field_model(&instance.fields, &instance.source_path, &shape);
    let scope = Scope {
        ctx,
        instance_path: path,
        model: Some(&model),
    };

    // The qualifier-aware per-field walk (routing, per-origin required,
    // divergent collision + required, mixed-form), shared with the inline-record
    // and meta sub-region surfaces (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]).
    check_field_surface(
        &scope,
        &shape,
        &instance.fields,
        claim_span(&instance.type_claim),
        claim_span(&instance.type_claim),
        FieldSurface::Instance,
        &mut diags,
    );

    // Sort by (span start, code) for stable, predictable output. Path is the
    // instance file throughout, so we don't need to compare it. Strengthens
    // the function's contract — direct consumers (LSP, watcher) get sorted
    // diagnostics without re-implementing the sort.
    diags.sort_by(|a, b| {
        a.span
            .range
            .start
            .cmp(&b.span.range.start)
            .then(a.code.as_str().cmp(b.code.as_str()))
    });
    diags
}

/// Per-instance scope: `ctx` plus the path of the file under validation.
/// Internal — every check helper takes a `&Scope` instead of threading
/// the same three-or-four references through each call.
struct Scope<'a> {
    ctx: &'a ValidateContext<'a>,
    instance_path: &'a Path,
    /// The value model for the surface being validated, or `None`.
    ///
    /// `Some` for every surface that builds a field model, the top-level instance
    /// validation ([`validate`]), a nested inline record, and a meta sub-region: a
    /// value verdict at a reference / brand slot consumes the node
    /// [`effective_values`] produced for the field's span instead of re-parsing
    /// the surface string, the value-model invariant, see
    /// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
    /// `None` in the helper scopes that validate one value directly and do no
    /// model-node span lookup, the brand / tuple / malformed-constructor arms and
    /// the block-identity checks.
    model: Option<&'a FieldModel>,
}

/// The instance surface's frontmatter value model, a span → parsed node lookup.
///
/// Keyed by each frontmatter contribution's byte range, the SAME span
/// [`check_value_against_shape`] receives for a whole field or a list element.
/// Built once per instance from [`effective_values`], so a reference or brand
/// value verdict reads the node the model produced rather than re-parsing the
/// surface, see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
struct FieldModel {
    /// A span keys `Some((node, brand))` when exactly one frontmatter value sits
    /// there, `None` when two share it (an ambiguous correlation). The `brand` is
    /// the written brand the container carries, the discriminator a brand verdict
    /// needs. Real file spans are distinct, so an ambiguity arises only from
    /// synthetic zero-spans (unit tests); there the lookup misses and the
    /// re-parse fallback runs, which is authoritative either way.
    by_span: std::collections::HashMap<(usize, usize), Option<(ContributionValue, Option<String>)>>,
}

impl FieldModel {
    /// The model node for a value at `span`, `None` when the span is not a
    /// top-level frontmatter field or element (a nested / meta value, or a
    /// value the producer left untyped), or when two values share the span.
    fn node(&self, span: ByteRange) -> Option<&ContributionValue> {
        self.by_span
            .get(&(span.start, span.end))
            .and_then(|o| o.as_ref())
            .map(|(node, _)| node)
    }

    /// The model node plus the written brand for a value at `span`, the pair a
    /// brand verdict consumes. Same `None` cases as [`FieldModel::node`].
    fn node_and_brand(&self, span: ByteRange) -> Option<(&ContributionValue, Option<&str>)> {
        self.by_span
            .get(&(span.start, span.end))
            .and_then(|o| o.as_ref())
            .map(|(node, brand)| (node, brand.as_deref()))
    }

    /// A one-entry model carrying a single already-resolved node at `span`, so a
    /// body-originated value (an inline-code contribution the elaborator classified
    /// into a [`ContributionValue`]) validates through the shared value arms without
    /// re-parsing its surface, see [`check_body_reference_container`].
    fn single(span: ByteRange, node: ContributionValue, brand: Option<String>) -> FieldModel {
        let mut by_span = std::collections::HashMap::new();
        by_span.insert((span.start, span.end), Some((node, brand)));
        FieldModel { by_span }
    }
}

/// Build the value model for a field surface, a span → node lookup.
///
/// Runs the graph-free elaborator [`elaborate_fields`] over the surface's fields
/// against its effective `shape`. A frontmatter field's model node is built from
/// the field set alone (no body), so this serves the top-level instance surface
/// AND, given a nested record's or meta sub-region's fields plus their resolved
/// shape, every nested surface, see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
/// Each contribution's span keys its resolved node.
fn build_field_model(
    fields: &[InstanceField],
    source_path: &Path,
    shape: &EffectiveShape,
) -> FieldModel {
    use std::collections::hash_map::Entry;
    let mut by_span: std::collections::HashMap<
        (usize, usize),
        Option<(ContributionValue, Option<String>)>,
    > = std::collections::HashMap::new();
    let values = elaborate_fields(fields, source_path, Some(shape));
    for containers in values.values() {
        for container in containers {
            for contrib in &container.contributions {
                if matches!(contrib.surface, Surface::Frontmatter) {
                    let r = contrib.location.byte_range;
                    match by_span.entry((r.start, r.end)) {
                        // The written brand rides on the collapsed container, the
                        // discriminator a brand verdict needs.
                        Entry::Vacant(e) => {
                            e.insert(Some((contrib.value.clone(), container.brand.clone())));
                        }
                        // Two frontmatter values share a span: the correlation
                        // is ambiguous, so mark it `None` and let the lookup miss.
                        Entry::Occupied(mut e) => {
                            e.insert(None);
                        }
                    }
                }
            }
        }
    }
    FieldModel { by_span }
}

/// Type-def-side info for diagnostics that point back at where the slot
/// shape was declared. Lives only as long as the call chain that produced
/// the diagnostic — borrows the EffectiveShape's `FieldOrigin`.
struct Origin<'a> {
    path: &'a Path,
    shape_span: ByteRange,
}

impl Origin<'_> {
    fn related_span(&self) -> Span {
        Span::new(self.path.to_path_buf(), self.shape_span)
    }
}

/// The diagnostic for a typed-slot wikilink whose target did not resolve.
///
/// Two outcomes wear one shape at the resolver, "no such path", and they mean
/// opposite things. A target that is merely absent is open-world growth, and may
/// be authored next. A target that LEAVES its repo can never resolve: a wikilink
/// is repo-scoped and `::repo` is how an edge crosses, so the address
/// contradicts its own scope, and no later authoring fixes it.
///
/// Reporting both as missing puts an impossible address in the same bucket as a
/// renamed or not-yet-written one, which is exactly where a nonsense pin becomes
/// indistinguishable from a legitimately drifted one.
///
/// Both stay `warning`. The engine is advisory, and the value here is
/// DISTINGUISHABILITY plus a fix, not blocking.
fn unresolved_target_diagnostic(
    instance_path: &Path,
    value_span: ByteRange,
    prefix: &str,
    target: &str,
    related: Vec<Span>,
) -> Diagnostic {
    let span = Span::new(instance_path.to_path_buf(), value_span);
    if au_references::target_escapes_repo(target) {
        return Diagnostic {
            code: au_references::codes::REFERENCE_PATH_ESCAPES_REPO,
            severity: Severity::Warning,
            span,
            message: format!(
                "{prefix} references '{target}', a path that leaves this repo; a wikilink is \
                 repo-scoped, so it can never resolve"
            ),
            related,
            fix: Some(SuggestedFix {
                description: au_references::ESCAPES_REPO_FIX.to_string(),
            }),
        };
    }
    Diagnostic {
        code: au_references::codes::REFERENCE_TARGET_MISSING,
        severity: Severity::Warning,
        span,
        message: format!("{prefix} references '{target}' which does not exist in the repo"),
        related,
        fix: None,
    }
}

/// The four LIVE-graph resolution outcomes an inert `*@` pin suppresses.
///
/// A pin is a snapshot into an immutable past, so it is not re-resolved against
/// the current graph. These four verdicts (the target's live existence, its
/// type, and its def-closure) are therefore dropped for a pinned value, neither
/// an error nor drift. Everything else `check_value_against_shape` produces is a
/// STRUCTURAL fact about the reference itself (a path escape, a malformed shape)
/// and still applies. A `@commit` pin in a plain `file*` slot, not `*@`, never
/// reaches this, so its live-missing target stays `reference-target-missing`.
///
/// See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
fn is_pinned_live_divergence(d: &Diagnostic) -> bool {
    d.code == au_references::codes::REFERENCE_TARGET_MISSING
        || d.code == codes::REFERENCE_TARGET_TYPE_MISMATCH
        || d.code == codes::DEF_REF_TARGET_NOT_A_TYPE_DEF
        || d.code == codes::DEF_REF_CLOSURE_MISMATCH
}

/// A commit-pinned value in a slot whose shape admits no pin, `unexpected-commit-pin`.
/// The mirror of `value-not-pinned`. See [`codes::UNEXPECTED_COMMIT_PIN`].
fn unexpected_commit_pin_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    prefix: &str,
    shape: &Shape,
) -> Diagnostic {
    // Mirrors `value-not-pinned`'s message shape, the two are a pair.
    Diagnostic {
        code: codes::UNEXPECTED_COMMIT_PIN,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message: format!(
            "{prefix} value is commit-pinned — declared shape {shape} admits no pin; use a '*@' slot or a '< T* | T*@ >' union"
        ),
        related: vec![origin.related_span()],
        fix: None,
    }
}

/// Recursive per-value check. Called once per field at top level, then
/// re-entered for each element of `Shape::List` with `element_index =
/// Some(i)` so list-element diagnostics render "field 'X' element N
/// value..." instead of "field 'X' value..." (which reads as a
/// whole-field error). Nested lists report the innermost element index
/// only — path-style indexing (`[0][1]`) is a future refinement.
/// Check one value against a slot shape, WITH the pin-admission guard.
///
/// A named commit-pin (`[[file::@sha]]`) is legal only where the shape ADMITS a
/// pin: a `Shape::Pinned` (the `*@` slot), or a `Shape::Union` that dispatches to
/// a `*@` branch. Every other reference shape forbids it, `unexpected-commit-pin`.
/// A bare commit-referent (`[[::@sha]]`, empty target) is exempt, it names a
/// COMMIT not a file, so it is allowed anywhere.
///
/// The guard wraps [`check_value_against_shape_inner`]. Every caller uses this
/// guarded entry EXCEPT the `Shape::Pinned` arm's own structural delegation,
/// which calls the inner directly: it owns the pin decision, so the pin is
/// expected there, not unexpected. A union branch re-enters HERE, so a pin in a
/// non-`*@` branch is rejected while a `*@` branch admits it.
fn check_value_against_shape(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    if matches!(
        shape,
        Shape::Reference(_)
            | Shape::InlineOrReference(_)
            | Shape::CompoundReference { .. }
            | Shape::DefReference(_)
    ) {
        if matches!(value, InstanceValue::String(_)) {
            // A pinned value at a non-`*@` reference slot. Pinned-ness comes from
            // the model node: a `Reference` carrying a parsed `commit`. Any other
            // node, or no node, is not a pinned reference — the elaborator classifies
            // every `[[…]]`-shaped value into a `Reference` / `MalformedReference`
            // node, so a String with no `Reference` node here is not a wikilink and
            // never carries a pin. A malformed pin is NOT caught here; it falls
            // through to the inner arm's node handling, as before.
            let pinned = matches!(
                scope.model.and_then(|m| m.node(value_span)),
                Some(ContributionValue::Reference { commit: Some(_), target, .. })
                    if !target.is_empty()
            );
            if pinned {
                let prefix = format_field_prefix(field_key, element_index);
                return vec![unexpected_commit_pin_diag(
                    scope, value_span, origin, &prefix, shape,
                )];
            }
        }
    }
    check_value_against_shape_inner(
        scope,
        value,
        value_span,
        shape,
        field_key,
        origin,
        element_index,
    )
}

fn check_value_against_shape_inner(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    // [[type-instance body contribution::au-type-system]]: a null value marks the field as "filled by body" — a visibility
    // marker, not a value to check. Higher-layer body validation enforces
    // that body contributions exist (required-field-absent / fills-contract-unmet
    // / body-fills-without-frontmatter-key surface absences). Null at list-
    // element position is rare and treated the same.
    if matches!(value, InstanceValue::Null) {
        return Vec::new();
    }
    let prefix = format_field_prefix(field_key, element_index);
    match shape {
        // The uninterpreted slot ([[type-def shape opaque::au-type-system]]): any value passes,
        // no shape check, no descent, so a nested `type:` is plain data. The
        // wikilink and candidate scans skip its content separately (candidates /
        // body_validate), and its fence reads verbatim.
        Shape::Opaque => Vec::new(),
        // The interpreted top ([[type-def shape any::au-type-system]]): the shape constraint is
        // trivially satisfied, but the value IS read. A mapping is an inline
        // record validated on its own terms (its `type:` claim, if any), with no
        // slot demand (`InlineCompat::Open`). Any non-mapping imposes no
        // constraint and passes; its wikilinks are real edges and its content is
        // candidate-scanned, both owned by the backlink / candidate passes.
        Shape::Any => match value {
            InstanceValue::Mapping(inline) => validate_inline_value(
                scope,
                inline,
                value_span,
                &InlineCompat::Open,
                None,
                field_key,
                origin,
                element_index,
            ),
            _ => Vec::new(),
        },
        Shape::Primitive(_) | Shape::Enum(_) => {
            if value_matches_simple_shape(value, shape) {
                Vec::new()
            } else if let (Shape::Primitive(Primitive::Number), InstanceValue::Float(f)) =
                (shape, value)
            {
                // A finite float matches Number, so a failed match here is a
                // non-finite value. Name the real problem.
                vec![Diagnostic {
                    code: codes::NON_FINITE_NUMBER,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!("{} is a non-finite number ({})", prefix, f),
                    related: vec![origin.related_span()],
                    fix: None,
                }]
            } else {
                vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} value does not match declared shape {}",
                        prefix, shape
                    ),
                )]
            }
        }
        Shape::Refined { base, refinement } => {
            // Value refinement ([[type-def field shape::au-type-system]]): first the base
            // primitive, then the predicate meet.
            let base_diags = check_value_against_shape_inner(
                scope,
                value,
                value_span,
                &Shape::Primitive(*base),
                field_key,
                origin,
                element_index,
            );
            if !base_diags.is_empty() {
                return base_diags;
            }
            // A provably-empty region fires `refinement-unsatisfiable` once on
            // the type-def; suppress the per-value error so it is not drowned.
            if refinement_unsatisfiable(*base, refinement) {
                return Vec::new();
            }
            match refinement_value_violation(*base, refinement, value) {
                Some(detail) => vec![Diagnostic {
                    code: codes::VALUE_OUT_OF_REFINEMENT,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!("{} {}", prefix, detail),
                    related: vec![origin.related_span()],
                    fix: None,
                }],
                None => Vec::new(),
            }
        }
        Shape::Tuple(elements) => {
            // A fixed-arity positional product ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
            // An inline tuple value is the PAREN form `(a, b)`, never a `[...]`
            // sequence — a bracket is always a LIST value (decision A, see
            // [[type-def shape tuple::au-type-system]]). The `Name(...)` constructor form is a
            // brand's, handled at the brand layer (`check_nominal_brand_value`).
            //
            // The elaborator classifies a paren tuple into a `Tuple` node; consume
            // it via the ONE tuple validator ([`check_contribution_value`]'s `Tuple`
            // arm, shared with the body path), never re-parsing the surface, the
            // value-model invariant ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
            // Any other node / value is not a paren tuple, so a `String` is a shape
            // mismatch and a `[...]` sequence is a list, both reported here.
            if let Some((node @ ContributionValue::Tuple(_), brand)) =
                scope.model.and_then(|m| m.node_and_brand(value_span))
            {
                return check_contribution_value(
                    scope.ctx,
                    scope.instance_path,
                    node,
                    brand,
                    value_span,
                    shape,
                    field_key,
                    element_index,
                    origin.path,
                    origin.shape_span,
                );
            }
            match value {
                InstanceValue::String(_) => vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} value is not a tuple — declared shape {} expects a `(...)` paren tuple of {} element(s)",
                        prefix,
                        shape,
                        elements.len()
                    ),
                )],
                _ => vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} value is not a tuple — a `[...]` sequence is a list; declared shape {} expects a `(...)` paren tuple of {} element(s)",
                        prefix,
                        shape,
                        elements.len()
                    ),
                )],
            }
        }
        Shape::Pinned(inner) => {
            // [[type-def shape suffixes::au-type-system]] `*@`: the value must be a commit-pinned
            // reference. The reference branch (a wikilink) must carry a `@commit`;
            // the inline branch (only for a `T&@` inner) needs no pin. See
            // [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
            // A string value is parsed as a wikilink. A MALFORMED one (a non-oid
            // pin, an empty commit) surfaces its precise parse error, not a
            // misleading value-not-pinned. A string that is not a wikilink at all
            // falls through to the non-reference handling below.
            if let InstanceValue::String(s) = value {
                // `clean_commit` is `Some(has_commit)` for a clean wikilink (the
                // pin verdict), `None` when the value is not a clean wikilink (a
                // non-wikilink Scalar falls through to the final value-not-pinned;
                // a malformed wikilink returns its precise parse error inline). It
                // comes from the model node when present, else a re-parse.
                let clean_commit: Option<bool> =
                    match scope.model.and_then(|m| m.node(value_span)) {
                        Some(ContributionValue::Reference { commit, .. }) => Some(commit.is_some()),
                        Some(ContributionValue::MalformedReference(err, _)) => match err {
                            WikilinkParseError::NotAWikilink | WikilinkParseError::EmptyInner => None,
                            err => {
                                return vec![wikilink_parse_diag(
                                    scope, value_span, origin, &prefix, s, err,
                                )]
                            }
                        },
                        // A non-wikilink Scalar node — falls through below.
                        Some(_) => None,
                        // No model node: a String reaching a pinned reference slot
                        // with no `Reference` node is not a `[[…]]`-shaped value (the
                        // elaborator classifies every such value into a node), so it
                        // is not a clean wikilink — fall through to value-not-pinned.
                        None => None,
                    };
                if let Some(has_commit) = clean_commit {
                    if !has_commit {
                        return vec![Diagnostic {
                            code: codes::VALUE_NOT_PINNED,
                            severity: Severity::Error,
                            span: Span::new(scope.instance_path.to_path_buf(), value_span),
                            message: format!(
                                "{} value is not commit-pinned — declared shape {} requires a '[[…::@commit]]' pin",
                                prefix, shape
                            ),
                            related: vec![origin.related_span()],
                            fix: None,
                        }];
                    }
                    // A pin is an INERT snapshot: it is not re-resolved against
                    // the live graph, so its four live-resolution outcomes are
                    // suppressed, neither an error nor drift. Its live
                    // counterpart may have moved or diverged, which a fixed past
                    // cannot. STRUCTURAL checks still apply, a path-escaping or
                    // otherwise malformed reference is a real authoring error
                    // regardless of the pin. Delegates to the UNGUARDED inner:
                    // a pin is expected in this `*@` slot, so the pin-admission
                    // guard must not fire on the inner reference. See
                    // [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
                    return check_value_against_shape_inner(
                        scope, value, value_span, inner, field_key, origin, element_index,
                    )
                    .into_iter()
                    .filter(|d| !is_pinned_live_divergence(d))
                    .collect();
                }
            }
            // A reference-only `*@` slot given a non-reference value. `@` requires
            // `*`, so the inner is always a `*` reference (a `&` inline is rejected
            // at parse), and there is no inline branch to fall to.
            vec![Diagnostic {
                code: codes::VALUE_NOT_PINNED,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), value_span),
                message: format!(
                    "{} value is not a commit-pinned reference — declared shape {} requires a '[[…::@commit]]' pin",
                    prefix, shape
                ),
                related: vec![origin.related_span()],
                fix: None,
            }]
        }
        Shape::Reference(name) => {
            // A brand slot with `*`, own-repo or `::repo`: a nominal brand is
            // inline-only (the load check owns `brand-not-referenceable`), so
            // suppress the per-value error — the type is broken, not the data. A
            // STRUCTURAL union brand resolves against its RECORD members'
            // closures (own-repo here; cross-repo in the structural path).
            if let Some(brand) = resolve_brand(scope, name) {
                if brand.is_nominal() {
                    return Vec::new();
                }
                if let Shape::Union(members) = &brand.shape {
                    let Some((mg, mr)) = brand_member_graph(scope, name) else {
                        return Vec::new();
                    };
                    return check_structural_brand_reference(
                        scope,
                        value,
                        value_span,
                        name.as_str(),
                        members,
                        mg,
                        mr,
                        field_key,
                        origin,
                        element_index,
                    );
                }
            }
            // `name*` (typed reference, spec [[type-def shape suffixes::au-type-system]] / [[type reference::au-type-system]]) — value MUST
            // be a wikilink string. Inline maps belong at `name` (Record)
            // or `name&` (InlineOrReference) slots, not here. The value-model
            // path consumes the parsed node when the instance surface produced
            // one, else re-parses.
            reference_check(
                scope,
                value,
                value_span,
                name.as_str(),
                name.repo.as_deref(),
                field_key,
                origin,
                element_index,
            )
        }
        Shape::Record(name) => {
            // A brand slot: `name` may resolve to a def declaring `shape:`
            // instead of fields ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
            // A NOMINAL brand (scalar / enum / tuple) validates the value against
            // its underlying shape, not as a record, so a bare `icon: save`
            // checks against the enum members. Own-repo only here; a `::repo`
            // brand slot resolves cross-repo (structural brands, later).
            // A brand slot, own-repo or `::repo`. A nominal brand validates its
            // value against its (self-contained) shape either way. A structural
            // union used by BARE name discriminates inline / constructor / bare;
            // own-repo here, a `::repo` union resolves its members cross-repo (the
            // structural cross-repo path).
            if let Some(brand) = resolve_brand(scope, name) {
                // A brand STRING value (constructor / bare / paren) is already
                // resolved by the model; route it through the one value-model
                // walker so no verdict re-parses ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
                // A Mapping (inline record) and an absent node (nested / meta) fall
                // to the surface-specific arms below.
                if let Some(diags) =
                    brand_value_via_model(scope, value, value_span, shape, field_key, origin, element_index)
                {
                    return diags;
                }
                if brand.is_nominal() {
                    return check_nominal_brand_value(
                        scope,
                        value,
                        value_span,
                        &name.to_string(),
                        brand,
                        field_key,
                        origin,
                        element_index,
                        &prefix,
                    );
                }
                if let Shape::Union(members) = &brand.shape {
                    let Some((mg, mr)) = brand_member_graph(scope, name) else {
                        return Vec::new();
                    };
                    return check_structural_brand_value(
                        scope,
                        value,
                        value_span,
                        &name.to_string(),
                        members,
                        mg,
                        mr,
                        false,
                        field_key,
                        origin,
                        element_index,
                        &prefix,
                    );
                }
            }
            // [[type-def shape record::au-type-system]] — bare-name record slot. Value MUST be an
            // inline YAML map. Anything else (string, number, etc.) is a
            // shape mismatch. [[type-def shape record::au-type-system]] cases 1 + 2 dispatch here.
            match value {
                InstanceValue::Mapping(inline) => validate_inline_value(
                    scope,
                    inline,
                    value_span,
                    &InlineCompat::SingleDemand(name.as_str()),
                    name.repo.as_deref(),
                    field_key,
                    origin,
                    element_index,
                ),
                _ => vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} value is not an inline record — declared shape '{}' expects a YAML map",
                        prefix, name
                    ),
                )],
            }
        }
        Shape::InlineOrReference(name) if name.as_str() == "any" => {
            // `any&` ([[type-def shape any::au-type-system]]): a whole-value `[[...]]` selects the
            // reference branch, resolved as `any*` (no closure check). Every
            // other value is the interpreted inline branch (the top): a mapping
            // is an inline record validated on its own terms, any other value
            // passes.
            if matches!(value, InstanceValue::String(_)) {
                // Reference-ness comes from the model node: a `Reference` node is a
                // clean wikilink. A `MalformedReference` or a non-wikilink Scalar (or
                // no node) is the inline branch — the elaborator classifies
                // every `[[…]]`-shaped value into a node, so a String with no
                // `Reference` node is not a clean wikilink.
                let is_ref = matches!(
                    scope.model.and_then(|m| m.node(value_span)),
                    Some(ContributionValue::Reference { .. })
                );
                if is_ref {
                    // `any` is never `::repo`-qualified (the grammar forbids it),
                    // so this stays an unqualified existence-only check. The
                    // value-model path consumes the parsed node.
                    return reference_check(
                        scope,
                        value,
                        value_span,
                        name.as_str(),
                        name.repo.as_deref(),
                        field_key,
                        origin,
                        element_index,
                    );
                }
            }
            // The interpreted inline branch: a mapping validates its own claim.
            if let InstanceValue::Mapping(inline) = value {
                return validate_inline_value(
                    scope,
                    inline,
                    value_span,
                    &InlineCompat::Open,
                    None,
                    field_key,
                    origin,
                    element_index,
                );
            }
            Vec::new()
        }
        Shape::InlineOrReference(name) => {
            // A brand slot with `&`, own-repo or `::repo`: a nominal brand is
            // inline-only (load check owns `brand-not-referenceable`), suppress
            // the per-value error. A STRUCTURAL union brand takes an inline record
            // OR a reference over its RECORD members, so `allow_reference` is true
            // (own-repo here; cross-repo in the structural path).
            if let Some(brand) = resolve_brand(scope, name) {
                if brand.is_nominal() {
                    return Vec::new();
                }
                if let Shape::Union(members) = &brand.shape {
                    // A constructor / bare STRING value routes through the one
                    // value-model walker (no re-parse). A wikilink (a `Reference`
                    // node) and an inline map are the reference / inline paths, left
                    // to the arm below. See [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
                    if let Some(diags) = brand_value_via_model(
                        scope,
                        value,
                        value_span,
                        shape,
                        field_key,
                        origin,
                        element_index,
                    ) {
                        return diags;
                    }
                    let Some((mg, mr)) = brand_member_graph(scope, name) else {
                        return Vec::new();
                    };
                    return check_structural_brand_value(
                        scope,
                        value,
                        value_span,
                        &name.to_string(),
                        members,
                        mg,
                        mr,
                        true,
                        field_key,
                        origin,
                        element_index,
                        &prefix,
                    );
                }
            }
            // [[type-def shape suffixes::au-type-system]] `name&` — value may be an inline map ([[type-def shape record::au-type-system]]) OR a
            // wikilink string whose target's `type:` closure includes
            // `name`. Dispatch by value shape.
            match value {
                InstanceValue::Mapping(inline) => validate_inline_value(
                    scope,
                    inline,
                    value_span,
                    &InlineCompat::SingleDemand(name.as_str()),
                    name.repo.as_deref(),
                    field_key,
                    origin,
                    element_index,
                ),
                InstanceValue::String(_) => reference_check(
                    scope,
                    value,
                    value_span,
                    name.as_str(),
                    name.repo.as_deref(),
                    field_key,
                    origin,
                    element_index,
                ),
                _ => vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} value is neither an inline record nor a wikilink — declared shape '{}&' expects a YAML map or '[[target]]' string",
                        prefix, name
                    ),
                )],
            }
        }
        Shape::List { inner, min, max } => check_list(
            scope,
            value,
            value_span,
            inner,
            *min,
            *max,
            field_key,
            origin,
            element_index,
        ),
        Shape::Union(branches) => {
            // [[type-def shape record::au-type-system]] case 3: inline value at a union slot — `type:` is
            // required, identifies which branch the value satisfies.
            // Records (Shape::Record / Reference / InlineOrReference)
            // contribute names; primitives/enums are skipped (they
            // can't host an inline map).
            if let InstanceValue::Mapping(inline) = value {
                let names = collect_inline_branches(branches);
                if !names.is_empty() {
                    return validate_inline_value(
                        scope,
                        inline,
                        value_span,
                        &InlineCompat::Union(names),
                        // Each branch carries its own `::repo`; the compound compat
                        // checks per branch, so no single demand_repo applies.
                        None,
                        field_key,
                        origin,
                        element_index,
                    );
                }
                // No record-shape branches — fall through to the per-
                // branch iteration which will produce per-branch
                // mismatch diagnostics.
            }
            // [[type reference::au-type-system]]: in primitive-vs-reference unions, wikilink-pattern
            // strings (`"[[...]]"`) resolve as references — they skip
            // the String branch and route to the reference branches in
            // source order. Without this precheck, the String branch's
            // any-of match would trivially accept any string value and
            // a malformed wikilink would silently pass.
            //
            // Failed-resolution behavior:
            // - N = 1 reference branch: return that branch's diagnostic
            //   verbatim (single failure is unambiguous).
            // - N ≥ 2 with target-missing or target-ambiguous: all
            //   branches yield the same diagnostic (the target either
            //   exists or doesn't, regardless of branch closure check)
            //   — return the first to avoid N copies of the same
            //   message.
            // - N ≥ 2 with target-type-mismatch on every branch:
            //   aggregate into ONE diagnostic naming every attempted
            //   reference branch. A first-failure-only message would
            //   misleadingly suggest the named branch was the sole
            //   option.
            if let InstanceValue::String(s) = value {
                if looks_like_wikilink(s) && branches.iter().any(is_wikilink_handling_shape) {
                    let ref_branches: Vec<&Shape> =
                        branches.iter().filter(|b| is_wikilink_handling_shape(b)).collect();
                    let mut failures: Vec<Vec<Diagnostic>> = Vec::new();
                    for branch in &ref_branches {
                        let diags = check_value_against_shape(
                            scope,
                            value,
                            value_span,
                            branch,
                            field_key,
                            origin,
                            element_index,
                        );
                        if diags.is_empty() {
                            return Vec::new();
                        }
                        failures.push(diags);
                    }
                    // Single reference branch — return its failure verbatim.
                    if failures.len() == 1 {
                        return failures.into_iter().next().unwrap();
                    }
                    // All failed; check the first failure's code to
                    // decide between "return first" and "aggregate".
                    let first_code = failures[0]
                        .first()
                        .map(|d| d.code.as_str())
                        .unwrap_or("");
                    let target_globally_unresolved = first_code
                        == au_references::codes::REFERENCE_TARGET_MISSING.as_str()
                        || first_code
                            == au_references::codes::REFERENCE_TARGET_AMBIGUOUS.as_str();
                    // A pinned value no branch admits: every branch rejected it
                    // with `unexpected-commit-pin`, so the first says it cleanly,
                    // rather than the misleading "satisfies none of the branches".
                    let pin_admitted_nowhere =
                        first_code == codes::UNEXPECTED_COMMIT_PIN.as_str();
                    if target_globally_unresolved || pin_admitted_nowhere {
                        return failures.into_iter().next().unwrap();
                    }
                    // Aggregate: one diagnostic naming every attempted
                    // reference branch. Branches render via Display
                    // (e.g. `rationale*`, `<a | b>*`).
                    let attempted: Vec<String> =
                        ref_branches.iter().map(|b| b.to_string()).collect();
                    return vec![field_shape_mismatch_diag(
                        scope,
                        value_span,
                        origin,
                        format!(
                            "{} value '{}' is a wikilink but its target satisfies none of the reference branches in declared shape {} (attempted: {})",
                            prefix,
                            s,
                            shape,
                            attempted.join(", ")
                        ),
                    )];
                }
                // [[type reference::au-type-system]]: in unions containing a Url branch, the `^https?://`
                // prefix commits the value to the Url branch — without
                // this precheck the String branch's trivial any-of match
                // would silently accept a malformed URL. Syntactically
                // disjoint from the wikilink check above; order is
                // immaterial.
                if has_http_url_prefix(s) {
                    if let Some(url_branch) = branches
                        .iter()
                        .find(|b| matches!(b, Shape::Primitive(Primitive::Url)))
                    {
                        return check_value_against_shape(
                            scope,
                            value,
                            value_span,
                            url_branch,
                            field_key,
                            origin,
                            element_index,
                        );
                    }
                }
            }
            // Any-of (spec [[type-def shape compound::au-type-system]]). Try each branch; first to succeed wins.
            // If all fail, emit ONE field-shape-mismatch with the union's
            // source-form rendering — branch-level diagnostics are not
            // propagated (avoids "satisfies neither" noise).
            for branch in branches {
                let diags = check_value_against_shape(
                    scope,
                    value,
                    value_span,
                    branch,
                    field_key,
                    origin,
                    element_index,
                );
                if diags.is_empty() {
                    return Vec::new();
                }
            }
            vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value does not match any branch of declared shape {}",
                    prefix, shape
                ),
            )]
        }
        Shape::Intersection(branches) => {
            // [[type-def shape record::au-type-system]] case 4: inline value at an intersection slot —
            // `type:` is required and must satisfy ALL reference-shape
            // branches (single-name claim or mixin). Validation against
            // the resolved identity's effective shape covers the union
            // of fields from all branches.
            if let InstanceValue::Mapping(inline) = value {
                let names = collect_inline_branches(branches);
                if !names.is_empty() {
                    return validate_inline_value(
                        scope,
                        inline,
                        value_span,
                        &InlineCompat::Intersection(names),
                        None,
                        field_key,
                        origin,
                        element_index,
                    );
                }
                // No record-shape branches — fall through to per-branch
                // failures.
            }
            // All-of (spec [[type-def shape compound::au-type-system]]). Each branch must validate; concatenate
            // per-branch failures so the user sees every constraint that
            // failed. Spec [[type-def shape compound::au-type-system]]: uninhabitability is never enforced —
            // missing-required-field cascades will surface real conflicts
            // at the use site.
            let mut all = Vec::new();
            for branch in branches {
                all.extend(check_value_against_shape(
                    scope,
                    value,
                    value_span,
                    branch,
                    field_key,
                    origin,
                    element_index,
                ));
            }
            all
        }
        Shape::DefReference(bound) => def_reference_check(
            scope,
            value,
            value_span,
            bound.as_ref(),
            shape,
            field_key,
            origin,
            element_index,
        ),
        Shape::CompoundReference {
            mode: RefMode::Star,
            op,
            branches,
        } => compound_reference_check(
            scope,
            value,
            value_span,
            *op,
            branches,
            shape,
            field_key,
            origin,
            element_index,
        ),
        Shape::CompoundReference {
            mode: RefMode::Inline,
            op,
            branches,
        } => match value {
            // [[type-def shape record::au-type-system]] + [[type-def shape suffixes::au-type-system]]: inline value at `<X | Y>&` / `<X & Y>&`
            // slot. Synthesize the InlineCompat from the compound's op
            // and branches, then run the same identity-resolution +
            // per-field walk as the single-name inline path.
            InstanceValue::Mapping(inline) => {
                let branch_list: Vec<QualBranch> = branches
                    .iter()
                    .map(|q| QualBranch {
                        base: q.as_str(),
                        repo: q.repo.as_deref(),
                    })
                    .collect();
                let compat = match op {
                    CompoundRefOp::Union => InlineCompat::Union(branch_list),
                    CompoundRefOp::Intersection => InlineCompat::Intersection(branch_list),
                };
                validate_inline_value(
                    scope,
                    inline,
                    value_span,
                    &compat,
                    None,
                    field_key,
                    origin,
                    element_index,
                )
            }
            // Wikilink string → reuse the same compound-reference
            // closure check as `<...>*` slots. Same target-resolution
            // and op-aware closure-includes semantics; the only
            // difference between `*` and `&` at the reference path is
            // that `&` also accepts inline maps (handled above).
            InstanceValue::String(_) => compound_reference_check(
                scope,
                value,
                value_span,
                *op,
                branches,
                shape,
                field_key,
                origin,
                element_index,
            ),
            _ => vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value is neither an inline record nor a wikilink — declared shape {} expects a YAML map or '[[target]]' string",
                    prefix, shape
                ),
            )],
        },
    }
}

/// Validate a value against a `<X | Y>*` / `<X & Y>*` slot. Same wikilink
/// resolution as `check_reference`, but the target's `type:` closure must
/// satisfy any (Union) or all (Intersection) of `branches`.
fn check_compound_reference_star(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    op: CompoundRefOp,
    branches: &[QualifiedName],
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    let s = match value {
        InstanceValue::String(s) => s,
        _ => {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value is not a wikilink — declared shape {} needs a '[[target]]' value",
                    prefix, shape
                ),
            )];
        }
    };
    check_parsed_compound_outcome(
        scope,
        // A String reaching this fallback is provably not a wikilink: the
        // elaborator classifies every `[[…]]`-shaped value into a `Reference` /
        // `MalformedReference` node (consumed above), and `looks_like_wikilink` is
        // exactly `parse_wikilink`'s `[[…]]`-shape gate, so a non-node String is
        // `NotAWikilink`. No surface re-parse, the value-model invariant.
        Err(&WikilinkParseError::NotAWikilink),
        s,
        value_span,
        op,
        branches,
        shape,
        field_key,
        origin,
        element_index,
    )
}

/// The compound sibling of [`check_parsed_reference_outcome`], the dispatch tail
/// shared by the re-parsing [`check_compound_reference_star`] and the value-model
/// [`compound_reference_check`].
#[allow(clippy::too_many_arguments)]
fn check_parsed_compound_outcome(
    scope: &Scope,
    parse: Result<&WikilinkRef, &WikilinkParseError>,
    s: &str,
    value_span: ByteRange,
    op: CompoundRefOp,
    branches: &[QualifiedName],
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    match parse {
        Ok(wikilink) => check_compound_reference_star_parsed(
            scope,
            wikilink,
            s,
            value_span,
            op,
            branches,
            shape,
            field_key,
            origin,
            element_index,
        ),
        Err(WikilinkParseError::NotAWikilink | WikilinkParseError::EmptyInner) => {
            vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value '{}' is not a wikilink — declared shape {} needs a '[[target]]' value",
                    prefix, s, shape
                ),
            )]
        }
        Err(err) => vec![wikilink_parse_diag(
            scope, value_span, origin, &prefix, s, err,
        )],
    }
}

/// A `<X | Y>*` / `<X & Y>*` value verdict, via the value model when present.
#[allow(clippy::too_many_arguments)]
fn compound_reference_check(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    op: CompoundRefOp,
    branches: &[QualifiedName],
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    with_reference_model(
        scope,
        value,
        value_span,
        |parse, s| {
            check_parsed_compound_outcome(
                scope,
                parse,
                s,
                value_span,
                op,
                branches,
                shape,
                field_key,
                origin,
                element_index,
            )
        },
        || {
            check_compound_reference_star(
                scope,
                value,
                value_span,
                op,
                branches,
                shape,
                field_key,
                origin,
                element_index,
            )
        },
    )
}

/// The graph-existence half of [`check_compound_reference_star`], keyed on the
/// PARSED [`WikilinkRef`] rather than the raw string, the compound sibling of
/// [`check_reference_parsed`]. `raw_display` names the local-form target.
#[allow(clippy::too_many_arguments)]
fn check_compound_reference_star_parsed(
    scope: &Scope,
    wikilink: &WikilinkRef,
    raw_display: &str,
    value_span: ByteRange,
    op: CompoundRefOp,
    branches: &[QualifiedName],
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);

    // A commit-referent (`[[::@sha]]` / `[[::repo@sha]]`) names a COMMIT, not a
    // file: there is nothing to resolve or type-check, and it is never dangling.
    // Anchor-only, see [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    if wikilink.is_commit_referent() {
        return Vec::new();
    }

    // A `::repo` reference crosses a repo boundary. Repo-local resolution
    // cannot see the target; the engine supplies a resolver that reaches into
    // the named repo and yields its graph, so the target's type is checked by
    // `(name, canonical-hash)` identity across the boundary. With no resolver
    // wired (au-core's own tests) or an unresolvable target (the engine's
    // cross-repo pass owns the `reference-repo-*` existence diagnostics), the
    // typed check is skipped here.
    let cross_target = if let Some(repo) = &wikilink.repo {
        match scope
            .ctx
            .cross_repo
            .and_then(|r| r.resolve(scope.instance_path, repo, &wikilink.target))
        {
            Some(t) => Some(t),
            None => return Vec::new(),
        }
    } else {
        None
    };

    // Local form per [[type reference::au-type-system]]: empty name + locating fragment
    // resolves to the host file itself — name lookup is skipped, so no
    // missing/ambiguous outcome exists.
    let resolved = if let Some(t) = &cross_target {
        t.path.clone()
    } else if wikilink.is_local() {
        scope.instance_path.to_path_buf()
    } else {
        match scope.ctx.repo_index.resolve(&wikilink.target) {
            Ok(p) => p,
            Err(ResolutionError::Missing) => {
                return vec![unresolved_target_diagnostic(
                    scope.instance_path,
                    value_span,
                    &prefix,
                    &wikilink.target,
                    vec![origin.related_span()],
                )];
            }
            Err(ResolutionError::Ambiguous(matches)) => {
                return vec![Diagnostic {
                    code: au_references::codes::REFERENCE_TARGET_AMBIGUOUS,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!(
                        "{} reference '{}' is ambiguous: {} files share the basename — use a repo-relative path",
                        prefix,
                        wikilink.target,
                        matches.len()
                    ),
                    related: matches.iter().map(|p| Span::for_file(p.clone())).collect(),
                    fix: None,
                }];
            }
        }
    };

    // Local refs read better named by their raw form than by the empty
    // target string; a cross-repo ref keeps its `::repo` qualifier.
    let shown_target: String = if let Some(repo) = &wikilink.repo {
        format!("{}::{}", wikilink.target, repo)
    } else if wikilink.is_local() {
        raw_display.to_string()
    } else {
        wikilink.target.clone()
    };

    // Anchor existence is independent of the typed check below —
    // navigational everywhere, see [[type reference::au-type-system]].
    let mut diags = slot_anchor_diags(scope, &resolved, wikilink, value_span, &prefix);

    // Per [[type-def shape file::au-type-system]] the built-in `file` branch is existence-only; successful
    // resolution above satisfies it without any `type:` claim. Other
    // branches still require the target's closure to include them.
    //
    // [[type block-id::au-type-system]]: with a `^block-id` the addressed entity's own
    // claims replace the file-level ones — a typed block or an inline
    // record, resolved the same way as the single-name path.
    let mut block_qualified: Vec<TypeNameClaim> = Vec::new();
    let target_claims: Cow<[TypeName]> = 'claims: {
        if let Some(block_id) = wikilink.block_id_str() {
            // A bare `^` on a union with a `file` branch is satisfied by
            // existence alone (the file branch takes the whole file), so it is
            // only checked navigationally, like an anchor: a dangling `^id` is a
            // broken link regardless. A `^^` block-referent is NOT the whole file
            // — it demands the block's value — so it falls through to the typed
            // resolution below and is checked against the non-file branches.
            let referent = wikilink.block_id.as_ref().is_some_and(|b| b.referent);
            if !referent
                && matches!(op, CompoundRefOp::Union)
                && branches.iter().any(|b| b.as_str() == "file")
            {
                diags.extend(block_id_existence_diags(
                    scope, &resolved, wikilink, value_span, origin, &prefix,
                ));
                return diags;
            }
            match resolve_block_target(
                scope, &resolved, wikilink, block_id, value_span, origin, &prefix,
            ) {
                BlockTarget::Claims { names, qualified } => {
                    if names.is_empty() {
                        // Claim-less record: diagnosed at the target, skip.
                        return diags;
                    }
                    block_qualified = qualified;
                    break 'claims Cow::Owned(names);
                }
                // A bare `^` navigational anchor: the FILE is the referent. Fall
                // through to the file's claim below.
                BlockTarget::FileReferent => {}
                // No checkable referent: a dangling bare `^` (warning) or a `^^`
                // that is plain / absent (errors). Emit and skip.
                BlockTarget::Unresolved { diags: block_diags } => {
                    diags.extend(block_diags);
                    return diags;
                }
            }
        }
        scope
            .ctx
            .ref_data
            .claims(&resolved)
            .unwrap_or(Cow::Borrowed(&[]))
    };

    // A qualified branch (`<a::r1 | b::r2>*`) checks the peer type by `TypeId`
    // membership over the target's folded closure,
    // the compound sibling of the single-name qualified demand. A `^block-id`
    // target folds its BLOCK's own claim (the override); an unfolded body
    // typed-fence `::repo` claim is uncheckable.
    // Only a `^^` block-referent folds the BLOCK's claim; a bare `^` file-referent
    // folds the file's claim (override None).
    let block_claim = wikilink
        .block_id
        .as_ref()
        .filter(|b| b.referent)
        .map(|_| TypeClaim::List {
            items: block_qualified.clone(),
            value_span,
        });
    // A `^^` block-referent is NOT a whole file, so it must not satisfy the
    // `file` branch of a union — its block's own claim has to satisfy a non-file
    // branch. A bare `^` with a `file` branch is short-circuited to
    // existence-only earlier, so anything reaching here with a block-id is a
    // `^^`; a whole-file `[[file]]` (no block-id) keeps `referent == false`.
    let referent = wikilink.block_id.as_ref().is_some_and(|b| b.referent);
    let mut branch_pass: Vec<bool> = Vec::with_capacity(branches.len());
    for b in branches {
        let ok =
            if b.as_str() == "file" {
                !referent
            } else if let Some(repo) = b.repo.as_deref() {
                match scope.ctx.cross_repo.and_then(|r| {
                    r.qualified_demand(b.as_str(), repo, &resolved, block_claim.as_ref())
                }) {
                    Some(qd) => qd.target_folded.contains(&qd.demanded),
                    // Unresolvable demand repo (crosstype flagged the shape) or an
                    // uncheckable block claim: skip the whole compound membership
                    // rather than risk a false mismatch.
                    None => return diags,
                }
            } else {
                match &cross_target {
                    Some(t) => target_closure_includes_cross_repo(
                        scope.ctx.graph,
                        t.graph,
                        &target_claims,
                        b.as_str(),
                    ),
                    None => target_closure_includes(scope.ctx.graph, &target_claims, b.as_str()),
                }
            };
        branch_pass.push(ok);
    }
    let satisfied_count = branch_pass.iter().filter(|&&ok| ok).count();
    let passes = match op {
        CompoundRefOp::Union => satisfied_count > 0,
        CompoundRefOp::Intersection => satisfied_count == branches.len(),
    };
    if passes {
        return diags;
    }
    let message = if target_claims.is_empty() {
        format!(
            "{} references '{}' which has no `type:` claim; declared shape {} needs a target whose closure satisfies it",
            prefix, shown_target, shape
        )
    } else {
        let claim_names: Vec<&str> = target_claims.iter().map(|n| n.as_str()).collect();
        format!(
            "{} references '{}' (claims: {}) but its `type:` closure does not satisfy declared shape {}",
            prefix,
            shown_target,
            claim_names.join(", "),
            shape
        )
    };
    diags.push(Diagnostic {
        code: codes::REFERENCE_TARGET_TYPE_MISMATCH,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message,
        related: vec![origin.related_span()],
        fix: None,
    });
    diags
}

/// True if `shape` accepts a `[[wikilink]]` string at validate time.
/// Used by [[type reference::au-type-system]]'s primitive-vs-reference disambiguation precheck:
/// strings matching the wikilink pattern route to whichever branches
/// satisfy this predicate, skipping plain `Shape::Primitive(String)`.
///
/// `Shape::Record(_)` is intentionally excluded — it's inline-only and
/// rejects strings outright.
fn is_wikilink_handling_shape(shape: &Shape) -> bool {
    match shape {
        Shape::Reference(_) | Shape::InlineOrReference(_) | Shape::CompoundReference { .. } => true,
        // A `*@` branch of a union, `< T* | T*@ >`. Its inner is reference-bearing,
        // so a wikilink value routes to it; a PINNED value matches it (inert), an
        // unpinned one matches the plain `T*` branch beside it. Without this the
        // union would ignore the pinned branch and a pin would be rejected by the
        // only branch it can satisfy.
        Shape::Pinned(inner) => is_wikilink_handling_shape(inner),
        _ => false,
    }
}

/// One branch of a compound inline demand, carrying the optional `::repo`
/// qualifier so a `<a::r1 | b::r2>&` slot checks each branch against the right
/// repo. A bare branch (`repo: None`) is a name match over the declared type's
/// closure; a qualified branch is `TypeId` membership over its FOLDED closure.
#[derive(Clone, Copy)]
struct QualBranch<'a> {
    base: &'a str,
    repo: Option<&'a str>,
}

impl<'a> QualBranch<'a> {
    /// The authored form, `base` or `base::repo`, for diagnostic messages.
    fn render(&self) -> String {
        match self.repo {
            Some(r) => format!("{}::{}", self.base, r),
            None => self.base.to_string(),
        }
    }
}

/// Pull reference-shape branches out of a Union or Intersection branch list,
/// keeping each branch's `::repo` qualifier. Primitive / Enum / List /
/// nested-compound branches are skipped — they can't host an inline value, so
/// they contribute no candidate identity for [[type-def shape record::au-type-system]] cases 3/4.
fn collect_inline_branches(branches: &[Shape]) -> Vec<QualBranch<'_>> {
    branches
        .iter()
        .filter_map(|b| match b {
            Shape::Reference(n) | Shape::Record(n) | Shape::InlineOrReference(n) => {
                Some(QualBranch {
                    base: n.as_str(),
                    repo: n.repo.as_deref(),
                })
            }
            _ => None,
        })
        .collect()
}

/// What inline `type:` claims the slot expects to be compatible with.
/// Drives the missing-type / not-compatible message variants and the
/// closure-includes check across [[type-def shape record::au-type-system]]'s four cases.
enum InlineCompat<'a> {
    /// [[type-def shape record::au-type-system]] case 1 + 2: declared closure must include `slot_demand`.
    /// `type:` is required iff `slot_demand` itself is sealed. A qualified
    /// `foo::repo` demand is handled ahead of here by
    /// [`validate_qualified_inline_demand`], so this is the unqualified case.
    SingleDemand(&'a str),
    /// [[type-def shape record::au-type-system]] case 3: declared closure must include at least one of the
    /// reference-branch demands. `type:` is always required.
    Union(Vec<QualBranch<'a>>),
    /// [[type-def shape record::au-type-system]] case 4: declared closure must include every reference-
    /// branch demand. `type:` is always required.
    Intersection(Vec<QualBranch<'a>>),
    /// The interpreted top `any` ([[type-def shape any::au-type-system]]): no demand, so a `type:`
    /// is optional and, when present, validated on its own terms (an unknown
    /// claim fires, the record's fields validate against the claimed type). No
    /// closure compat check, `any` demands nothing. A mapping with no `type:` is
    /// an untyped open value that passes.
    Open,
}

impl<'a> InlineCompat<'a> {
    /// Render the slot demand for use in diagnostic messages.
    fn render(&self) -> String {
        let join = |bs: &[QualBranch], sep: &str| {
            bs.iter().map(|b| b.render()).collect::<Vec<_>>().join(sep)
        };
        match self {
            InlineCompat::SingleDemand(name) => format!("'{}'", name),
            InlineCompat::Union(branches) => format!("'<{}>'", join(branches, " | ")),
            InlineCompat::Intersection(branches) => format!("'<{}>'", join(branches, " & ")),
            InlineCompat::Open => "'any'".to_string(),
        }
    }
}

/// Validate an inline value ([[type-def shape record::au-type-system]]) at a record-typed slot. Resolves the
/// inline's identity per the `compat` rule ([[type-def shape record::au-type-system]] cases 1-4), then walks
/// the inline's fields against the resolved effective shape.
///
/// Identity resolution:
/// - Inline `type:` omitted: case 1 (non-sealed `SingleDemand`) defaults
///   to the slot's demand; cases 2/3/4 fire `inline-value-missing-type`
///   and stop.
/// - Inline `type:` declared: identity is the declared claim, AND the
///   declared closure must satisfy `compat` (else
///   `inline-value-type-not-compatible`). Mixin form is allowed; the
///   unioned closure is what's checked.
///
/// Mixin machinery flows through: `check_redundant_claims`
/// for `duplicate-claim` / `subsumption-in-mixin` warnings, and
/// `effective_shape`'s collisions for `mixin-collision`. Sealed-parent
/// claims on inline `type:` fire `sealed-parent-claimed` per-claim
/// (mirrors the file-level rule).
///
/// Nested inline values fall out of `check_value_against_shape`'s
/// recursion: when this helper hits a record-typed inner field whose
/// value is also a `Mapping`, dispatch re-enters here. Bounded by the
/// AST tree depth.
/// Gate a marked fence's record against the SLOT it fills.
///
/// The body-surface sibling of the frontmatter inline-record path. Both build an
/// [`InlineCompat`] from the slot and route through
/// [`check_inline_claim_compat`], so the two surfaces cannot disagree about
/// which types a slot admits — the branch's whole premise.
///
/// OWNERSHIP: this answers only "does the claim satisfy the slot". The record's
/// own contract (required fields, per-field values) stays with
/// `check_marked_fences`, which reports it as `embedded-record-validation-failure`.
/// Routing the fence through `validate_inline_value` wholesale would run BOTH
/// and double-report.
///
/// Returns empty when the slot imposes no inline demand (so nothing to check) or
/// when the claim satisfies it.
pub(crate) fn check_fence_record_against_slot(
    ctx: &ValidateContext,
    instance_path: &Path,
    shape: &EffectiveShape,
    identity_claim: &TypeClaim,
    slot: &Shape,
    field_key: &str,
    origin_path: &Path,
    shape_span: ByteRange,
) -> Vec<Diagnostic> {
    let scope = Scope {
        ctx,
        instance_path,
        model: None,
    };
    let origin = Origin {
        path: origin_path,
        shape_span,
    };
    let prefix = format_field_prefix(field_key, None);
    let compat = match slot {
        Shape::Record(n) | Shape::InlineOrReference(n) => InlineCompat::SingleDemand(n.as_str()),
        Shape::Union(branches) => {
            let names = collect_inline_branches(branches);
            if names.is_empty() {
                return Vec::new();
            }
            InlineCompat::Union(names)
        }
        Shape::Intersection(branches) => {
            let names = collect_inline_branches(branches);
            if names.is_empty() {
                return Vec::new();
            }
            InlineCompat::Intersection(names)
        }
        Shape::CompoundReference {
            mode: RefMode::Inline,
            op,
            branches,
        } => {
            let list: Vec<QualBranch<'_>> = branches
                .iter()
                .map(|n| QualBranch {
                    base: n.as_str(),
                    repo: n.repo.as_deref(),
                })
                .collect();
            match op {
                CompoundRefOp::Union => InlineCompat::Union(list),
                CompoundRefOp::Intersection => InlineCompat::Intersection(list),
            }
        }
        // Every other slot kind imposes no inline-record demand: a text or
        // reference-only slot never reaches here (the fence does not read as a
        // record), and an `any` slot deliberately imposes nothing.
        _ => return Vec::new(),
    };
    check_inline_claim_compat(&scope, shape, identity_claim, &compat, &prefix, &origin)
}

/// The [[type-def shape record::au-type-system]] compat check: does a DECLARED inline claim satisfy
/// the slot it fills?
///
/// - case 1/2 (`SingleDemand`), the closure includes the slot's demand.
/// - case 3 (`Union`), the closure includes any branch.
/// - case 4 (`Intersection`), the closure includes every branch.
///
/// Extracted so the two inline surfaces share ONE implementation. A frontmatter
/// inline record reaches it through [`validate_inline_value`]; a marked body
/// fence reaches it through [`check_fence_record_against_slot`]. Before the
/// split, the fence path never checked its record against the slot at all, so a
/// union slot accepted a record whose type it never admitted.
///
/// Empty means the claim satisfies the slot. A non-empty result means the caller
/// must STOP: without a compatible identity, a per-field walk only adds noise.
fn check_inline_claim_compat(
    scope: &Scope,
    shape: &EffectiveShape,
    identity_claim: &TypeClaim,
    compat: &InlineCompat<'_>,
    prefix: &str,
    outer_origin: &Origin,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let unified_closure = shape.instance_closure();
    let in_closure = |name: &str| unified_closure.iter().any(|tn| tn.as_str() == name);
    let claim_summary: Vec<&str> = identity_claim.iter().map(|c| c.name.as_str()).collect();

    // A compound branch is satisfied per its qualifier: a bare branch by NAME
    // in the declared type's closure; a QUALIFIED branch by `TypeId` membership
    // of the demanded peer id in the declared type's FOLDED closure (the
    // precise sibling of the reference-form check, not a name match that an own
    // same-named type would false-pass). `None` = uncheckable, an unresolvable
    // qualified branch whose shape diagnostic the peer gate owns, so the whole
    // compound compat is skipped rather than risk a false mismatch.
    let folded = match scope.ctx.resolution {
        Some(rg) => folded_closure_ids(rg, &identity_claim),
        None => BTreeSet::new(),
    };
    let branch_pass = |b: &QualBranch| -> Option<bool> {
        match b.repo {
            None => Some(in_closure(b.base)),
            Some(repo) => scope
                .ctx
                .cross_repo
                .and_then(|r| r.peer_type_id(b.base, repo))
                .map(|peer| folded.contains(&peer.id)),
        }
    };

    let incompat_msg: Option<String> = match compat {
        InlineCompat::SingleDemand(name) => {
            // Unqualified only here; a qualified `foo::repo` demand is
            // intercepted by `validate_qualified_inline_demand` above.
            if !in_closure(name) {
                Some(format!(
                    "{}: inline value claims '{}' but the slot demands '{}' — declared closure does not include it",
                    &prefix,
                    claim_summary.join(", "),
                    name
                ))
            } else {
                None
            }
        }
        InlineCompat::Union(branches) => {
            match branches
                .iter()
                .map(|b| branch_pass(b))
                .collect::<Option<Vec<bool>>>()
            {
                // An uncheckable branch (peer gate owns it): the value may
                // satisfy it, so do not fire.
                None => None,
                Some(oks) if oks.iter().any(|&ok| ok) => None,
                Some(_) => Some(format!(
                    "{}: inline value claims '{}' but no branch of union slot {} is in its closure",
                    &prefix,
                    claim_summary.join(", "),
                    compat.render()
                )),
            }
        }
        InlineCompat::Intersection(branches) => {
            match branches
                .iter()
                .map(|b| branch_pass(b))
                .collect::<Option<Vec<bool>>>()
            {
                // An uncheckable branch: cannot confirm every branch holds, so
                // do not fire (the peer gate owns the branch's shape).
                None => None,
                Some(oks) if oks.iter().all(|&ok| ok) => None,
                Some(oks) => {
                    let missing: Vec<String> = branches
                        .iter()
                        .zip(oks)
                        .filter(|(_, ok)| !ok)
                        .map(|(b, _)| b.render())
                        .collect();
                    Some(format!(
                        "{}: inline value claims '{}' but its closure does not satisfy intersection slot {} — missing: {}",
                        prefix,
                        claim_summary.join(", "),
                        compat.render(),
                        missing.join(", ")
                    ))
                }
            }
        }
        // The interpreted top `any` demands nothing, so any declared claim is
        // compatible; only the claim's own validity matters ([[type-def shape any::au-type-system]]).
        InlineCompat::Open => None,
    };

    if let Some(msg) = incompat_msg {
        diags.push(Diagnostic {
            code: codes::INLINE_VALUE_TYPE_NOT_COMPATIBLE,
            severity: Severity::Error,
            span: Span::new(
                scope.instance_path.to_path_buf(),
                claim_span(&identity_claim),
            ),
            message: msg,
            related: vec![outer_origin.related_span()],
            fix: None,
        });
        // Stop — without a compatible identity, per-field walk would
        // surface noise. Fix the type claim and re-validate.
        //
        // ASYMMETRY: file-level `validate` still runs the sealed-walk
        // after incompat; inline skips it so one wrong `type:` yields
        // one primary error instead of two. Revisit if real-knowledge-base
        // feedback finds the single-error form confusing.
        return diags;
    }
    diags
}

fn validate_inline_value(
    scope: &Scope,
    inline: &InlineValue,
    inline_span: ByteRange,
    compat: &InlineCompat<'_>,
    demand_repo: Option<&str>,
    field_key: &str,
    outer_origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let prefix = format_field_prefix(field_key, element_index);

    // Qualified inline demand (`foo::repo`, or the inline branch of `foo::repo&`),
    // the fold-vs-demand rule's inline case
    // ([[example - cross-repo type fold versus field demand, a worked verification trace]] case 3).
    if let (InlineCompat::SingleDemand(demand), Some(repo)) = (compat, demand_repo) {
        return validate_qualified_inline_demand(
            scope,
            inline,
            inline_span,
            demand,
            repo,
            &prefix,
            outer_origin,
        );
    }

    // Whether the slot requires a declared inline `type:`. SingleDemand
    // (cases 1 + 2) requires only when the demand itself is sealed.
    // Union/Intersection (cases 3, 4) always require.
    //
    // INVARIANT: Union and Intersection MUST require a declared type.
    // The identity-resolution match below (search for `unreachable!`)
    // depends on this — flipping Union/Intersection to optional would
    // make the unreachable! arm reachable. If a future spec change
    // permits omitted `type:` at Union/Intersection slots, update both
    // sites together.
    // A single-demand slot requires an explicit inline `type:` when the demand is
    // NON-CLAIMABLE, sealed OR abstract: an omitted claim would synthesize the
    // ceiling's own identity, an implicit claim of a type that cannot be claimed.
    // Union/Intersection always require. See [[type-def shape record::au-type-system]].
    let type_required = match compat {
        InlineCompat::SingleDemand(name) => {
            scope.ctx.graph.is_abstract(&TypeName(name.to_string()))
        }
        InlineCompat::Union(_) | InlineCompat::Intersection(_) => true,
        // The interpreted top demands nothing, so a `type:` is optional: an
        // untyped mapping is an open value ([[type-def shape any::au-type-system]]).
        InlineCompat::Open => false,
    };

    // [[type-def shape record::au-type-system]] cases 2/3/4: declare inline `type:` or fire missing-type.
    // Stops before identity synthesis; without an identity the per-
    // field walk would surface noise.
    if inline.type_claim.is_none() && type_required {
        let msg = match compat {
            InlineCompat::SingleDemand(name) => {
                // Distinguish the ceiling's flavour, both fire the same code.
                let kind = if scope.ctx.graph.is_sealed(&TypeName(name.to_string())) {
                    "sealed-parent"
                } else {
                    "abstract"
                };
                format!(
                    "{}: inline value at {} slot '{}' must declare `type:` naming a concrete (claimable) descendant",
                    prefix, kind, name
                )
            }
            InlineCompat::Union(branches) => format!(
                "{}: inline value at union slot '<{}>' must declare `type:` identifying which branch the value satisfies",
                prefix,
                branches.iter().map(|b| b.render()).collect::<Vec<_>>().join(" | ")
            ),
            InlineCompat::Intersection(branches) => format!(
                "{}: inline value at intersection slot '<{}>' must declare `type:` identifying a type whose closure satisfies all branches",
                prefix,
                branches.iter().map(|b| b.render()).collect::<Vec<_>>().join(" & ")
            ),
            // `Open` sets `type_required` false, so this block is never entered
            // for it.
            InlineCompat::Open => unreachable!("Open never requires a declared type:"),
        };
        diags.push(Diagnostic {
            code: codes::INLINE_VALUE_MISSING_TYPE,
            severity: Severity::Error,
            span: Span::new(scope.instance_path.to_path_buf(), inline_span),
            message: msg,
            related: vec![outer_origin.related_span()],
            fix: None,
        });
        return diags;
    }

    // The interpreted top `any` with no `type:` is an untyped open mapping:
    // no claim to resolve, no shape to validate against, so it passes
    // ([[type-def shape any::au-type-system]]). Any candidate / navigational scanning of its
    // content is owned by the candidate and body passes, not here.
    if inline.type_claim.is_none() && matches!(compat, InlineCompat::Open) {
        return diags;
    }

    // Resolve the identity claim. Omitted (only reachable for case 1
    // with non-sealed demand) → synthesize a bare claim from the demand;
    // by definition it satisfies the slot.
    let (identity_claim, declared) = match (&inline.type_claim, compat) {
        (Some(c), _) => (c.clone(), true),
        (None, InlineCompat::SingleDemand(name)) => (
            TypeClaim::Bare(TypeNameClaim::own(TypeName(name.to_string()), inline_span)),
            false,
        ),
        // `Open` with no `type:` returned above; `Open` with a `type:` takes the
        // `(Some(c), _)` arm. So `(None, Open)` never reaches here.
        (None, InlineCompat::Open) => {
            unreachable!("validate_inline_value: untyped Open handled by the early return above")
        }
        // Unreachable per the INVARIANT documented at the
        // `type_required` site above: Union and Intersection always
        // require a declared inline `type:`, so omitted-type fires
        // `inline-value-missing-type` and returns before reaching this
        // match. If this ever fires, the type_required invariant has
        // drifted — re-read the comment block at type_required and
        // align both sites.
        (None, InlineCompat::Union(_)) | (None, InlineCompat::Intersection(_)) => {
            unreachable!(
                "validate_inline_value: type_required invariant violated — Union/Intersection should never reach identity-resolution with omitted type:"
            )
        }
    };

    // Mixin-side warnings on declared inline claims ([[type-instance type::au-type-system]]).
    if let TypeClaim::List { items, .. } = &identity_claim {
        diags.extend(check_redundant_claims(
            scope.ctx.graph,
            scope.ctx.resolution,
            items,
            scope.instance_path,
        ));
    }

    let shape = match effective_shape_for(scope.ctx, &identity_claim) {
        Ok(s) => s,
        Err(EffectiveShapeError::UnknownType(name)) => {
            // Mirrors the file-level `unknown-type-claim`. Span is the
            // inline `type:` claim site (or, if omitted, the inline
            // value's own span — but unknown type only fires when
            // declared, so this branch always has a real claim span).
            diags.push(Diagnostic {
                code: codes::UNKNOWN_TYPE_CLAIM,
                severity: Severity::Error,
                span: Span::new(
                    scope.instance_path.to_path_buf(),
                    claim_span(&identity_claim),
                ),
                message: format!(
                    "{}: inline value claims type-def '{}' which is not present in the type graph",
                    &prefix,
                    name.as_str()
                ),
                related: vec![outer_origin.related_span()],
                fix: None,
            });
            return diags;
        }
    };

    // [[type-def shape record::au-type-system]] compat check on the declared identity's closure,
    // shared with the body-fence surface. See `check_inline_claim_compat`.
    if declared {
        let compat_diags = check_inline_claim_compat(
            scope,
            &shape,
            &identity_claim,
            compat,
            &prefix,
            outer_origin,
        );
        if !compat_diags.is_empty() {
            diags.extend(compat_diags);
            // Stop — without a compatible identity, per-field walk would
            // surface noise. Fix the type claim and re-validate.
            return diags;
        }
    }

    // Sealed-parent-claimed ([[type-def sealed::au-type-system]]) per-claim on declared inline
    // `type:`. Even at a non-sealed slot, claiming a sealed type
    // directly is a validation error (mirrors file-level rule). The
    // [[type-def shape record::au-type-system]] case-2 missing-type rule (omitted `type:` at sealed slot is
    // an error) lights up alongside this when the slot itself is
    // sealed — that's a separate rule.
    if declared {
        for claim_el in identity_claim.iter() {
            // Own sealed-ness reads the own graph; a `::repo` claim's reads the
            // folded peer node, so a sealed peer parent claimed directly on an
            // inline value still fires (parity with the file-level `:378`,
            // qualified-inline-demand, and meta sealed-parent-claimed checks).
            // A declared-abstract, non-sealed inline claim fires
            // `abstract-type-claimed` instead, never alongside sealed.
            if claim_is_sealed(scope.ctx, claim_el) {
                diags.push(Diagnostic {
                    code: codes::SEALED_PARENT_CLAIMED,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), claim_el.span),
                    message: format!(
                        "type-def '{}' is sealed; claims must drill to a non-sealed descendant",
                        claim_el.name.as_str()
                    ),
                    related: vec![],
                    fix: None,
                });
            } else if claim_is_declared_abstract(scope.ctx, claim_el) {
                diags.push(abstract_type_claimed_diag(
                    scope.instance_path,
                    claim_el.span,
                    claim_el.name.as_str(),
                ));
            }
        }
        // Multi-leaf-in-sealed-family ([[type-instance type::au-type-system]]) — symmetric to the
        // file-level rule. Only meaningful for declared `type:` claims
        // (an undeclared inline value can't claim multiple leaves).
        diags.extend(check_multi_leaf_in_sealed_family(
            scope.ctx.graph,
            scope.ctx.resolution,
            scope.instance_path,
            &identity_claim,
        ));
    }

    // Mixin-collisions, per-field shape conformance (bare-keys-only), and
    // per-originator required-field-absent, the walk shared with the qualified
    // cross-repo delegation above.
    diags.extend(check_inline_fields(
        scope,
        inline,
        inline_span,
        &shape,
        &prefix,
        claim_span(&identity_claim),
    ));

    diags
}

/// Validate an inline value at a qualified `SingleDemand` slot (`foo::repo`, or
/// the inline branch of `foo::repo&`), the fold-vs-demand rule's inline case
/// ([[example - cross-repo type fold versus field demand, a worked verification trace]] case 3).
///
/// The inline's OWN identity decides the contract:
/// - omitted `type:`, non-sealed demand: the demand IS the identity, delegate to
///   the OWNER repo's effective shape for `base` (the D3b-ii scalar case).
/// - omitted `type:`, sealed demand: `inline-value-missing-type`, a sealed peer
///   slot needs an explicit non-sealed descendant.
/// - explicit `type: X`: X's FOLDED closure (over the SOURCE resolution graph,
///   where the inline's own `::repo` claim is a fold seed) must include the
///   demanded peer `TypeId`, the inline sibling of the reference membership; else
///   `inline-value-type-not-compatible`. A satisfying X then validates the map
///   against X's own effective shape.
///
/// Skips (no diagnostic) when the demand repo is unresolvable / has a broken
/// vocabulary, or an explicit qualified claim names an unfolded peer, the
/// `type-repo-*` / `peer-type-not-found` gate owns those.
fn validate_qualified_inline_demand(
    scope: &Scope,
    inline: &InlineValue,
    inline_span: ByteRange,
    base: &str,
    repo: &str,
    prefix: &str,
    outer_origin: &Origin,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // The demanded peer identity + sealed-ness, from the demand repo. `None` = the
    // demand repo / type is unresolvable (the gate owns that), so skip.
    let Some(peer) = scope
        .ctx
        .cross_repo
        .and_then(|r| r.peer_type_id(base, repo))
    else {
        return diags;
    };
    let shown_demand = format!("{base}::{repo}");

    let Some(claim) = &inline.type_claim else {
        // Omitted inline `type:`. A non-claimable peer ceiling, sealed OR
        // abstract, needs an explicit concrete descendant; defaulting to the
        // ceiling's own identity would implicitly claim a non-claimable type.
        if peer.sealed || peer.declared_abstract {
            let kind = if peer.sealed {
                "sealed-parent"
            } else {
                "abstract"
            };
            diags.push(Diagnostic {
                code: codes::INLINE_VALUE_MISSING_TYPE,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), inline_span),
                message: format!(
                    "{}: inline value at {} slot '{}' must declare `type:` naming a concrete (claimable) descendant",
                    prefix, kind, shown_demand
                ),
                related: vec![outer_origin.related_span()],
                fix: None,
            });
            return diags;
        }
        // Non-sealed: the demand is the identity, delegate to the owner's shape.
        // `None` when the owner is unresolvable (the gate owns it), so skip.
        let Some(owner_shape) = scope
            .ctx
            .cross_repo
            .and_then(|r| r.owner_effective_shape(base, repo))
        else {
            return diags;
        };
        return check_inline_fields(
            scope,
            inline,
            inline_span,
            &owner_shape,
            prefix,
            inline_span,
        );
    };

    // Explicit inline `type: X`. An unresolvable qualified claim element is an
    // absent / ungated peer the `peer-type-not-found` gate owns; skip rather than
    // false-fire (mirrors the reference seam). Without a resolution graph, a
    // qualified claim cannot be folded at all, same skip.
    match scope.ctx.resolution {
        Some(rg) => {
            for c in claim.iter() {
                if c.repo.is_some() && rg.resolve_authored(&c.name, c.repo.as_deref()).is_none() {
                    return diags;
                }
            }
        }
        None => {
            if claim.iter().any(|c| c.repo.is_some()) {
                return diags;
            }
        }
    }

    // X's effective shape, import-aware. A bare OWN claim absent everywhere is an
    // `unknown-type-claim`, mirroring the file-level path.
    let shape = match effective_shape_for(scope.ctx, claim) {
        Ok(s) => s,
        Err(EffectiveShapeError::UnknownType(name)) => {
            diags.push(Diagnostic {
                code: codes::UNKNOWN_TYPE_CLAIM,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), claim_span(claim)),
                message: format!(
                    "{}: inline value claims type-def '{}' which is not present in the type graph",
                    &prefix,
                    name.as_str()
                ),
                related: vec![outer_origin.related_span()],
                fix: None,
            });
            return diags;
        }
    };

    // Membership: X's folded closure must include the demanded peer `TypeId`, the
    // inline sibling of the reference-demand check. A pure own type (no resolution
    // graph, or a claim that reaches no `::repo` edge) cannot reach the peer.
    let folded = match scope.ctx.resolution {
        Some(rg) => folded_closure_ids(rg, claim),
        None => BTreeSet::new(),
    };
    if !folded.contains(&peer.id) {
        let claim_names: Vec<&str> = claim.iter().map(|c| c.name.as_str()).collect();
        diags.push(Diagnostic {
            code: codes::INLINE_VALUE_TYPE_NOT_COMPATIBLE,
            severity: Severity::Error,
            span: Span::new(scope.instance_path.to_path_buf(), claim_span(claim)),
            message: format!(
                "{}: inline value claims '{}' but the slot demands the peer type '{}' — declared closure does not include it",
                prefix,
                claim_names.join(", "),
                shown_demand
            ),
            related: vec![outer_origin.related_span()],
            fix: None,
        });
        return diags;
    }

    // X satisfies the demand. The mixin-side warnings, sealed-parent-claimed, and
    // multi-leaf checks mirror the file-level and same-repo inline paths.
    if let TypeClaim::List { items, .. } = claim {
        diags.extend(check_redundant_claims(
            scope.ctx.graph,
            scope.ctx.resolution,
            items,
            scope.instance_path,
        ));
    }
    // Claiming a sealed type directly is an error, even at a satisfying membership
    // (a sealed peer parent's own id IS in its folded closure). Reads the folded
    // node for a `::repo` claim, the own graph for a bare one. A declared-abstract,
    // non-sealed claim fires `abstract-type-claimed` instead, never both.
    for claim_el in claim.iter() {
        if claim_is_sealed(scope.ctx, claim_el) {
            diags.push(Diagnostic {
                code: codes::SEALED_PARENT_CLAIMED,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), claim_el.span),
                message: format!(
                    "type-def '{}' is sealed; claims must drill to a non-sealed descendant",
                    claim_el.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        } else if claim_is_declared_abstract(scope.ctx, claim_el) {
            diags.push(abstract_type_claimed_diag(
                scope.instance_path,
                claim_el.span,
                claim_el.name.as_str(),
            ));
        }
    }
    diags.extend(check_multi_leaf_in_sealed_family(
        scope.ctx.graph,
        scope.ctx.resolution,
        scope.instance_path,
        claim,
    ));

    // Validate the map against X's own effective shape.
    diags.extend(check_inline_fields(
        scope,
        inline,
        inline_span,
        &shape,
        prefix,
        claim_span(claim),
    ));
    diags
}

/// The inline-value field walk shared by the same-repo inline path and the
/// qualified cross-repo delegation: mixin collisions, each field's value against
/// its declared shape, and per-originator required-field-absent. `shape` is the
/// effective shape the map validates against, the demand's own repo for a bare
/// slot, or the OWNER repo for a qualified `foo::repo` demand. `identity_span`
/// anchors collision diagnostics (the inline `type:` claim, or the value span
/// when the identity is pin-driven).
fn check_inline_fields(
    scope: &Scope,
    inline: &InlineValue,
    inline_span: ByteRange,
    shape: &EffectiveShape,
    prefix: &str,
    identity_span: ByteRange,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // The nested surface's value model, built from THIS inline record's fields
    // against its resolved shape, so a nested reference / brand / pin value
    // verdict consumes the node the elaborator produced instead of re-parsing.
    // Rebuilt per nesting level as `validate_inline_value` recurses, so a value
    // resolves at any depth, see
    // [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
    let model = build_field_model(&inline.fields, scope.instance_path, shape);
    let nested = Scope {
        ctx: scope.ctx,
        instance_path: scope.instance_path,
        model: Some(&model),
    };

    // The qualifier-aware per-field walk, shared with the frontmatter and meta
    // surfaces (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]).
    check_field_surface(
        &nested,
        shape,
        &inline.fields,
        inline_span,
        identity_span,
        FieldSurface::Inline { prefix },
        &mut diags,
    );

    diags
}

/// Validate every meta sub-region body on every TypeDef in `ctx.graph`.
/// Each block's body validates against the named meta-type-def's effective
/// shape using the same per-field machinery as instance validation:
/// required-field-absent, field-shape-mismatch (via `check_value_against_shape`),
/// sealed-leaf rule, unknown-type-claim, reference-target-missing/-type-
/// mismatch. Spec [[type-def meta::au-type-system]].
///
/// Meta-body errors fire as Error severity. Per [[type validation::au-type-system]] "errors at step 1
/// prevent step 2", downstream per-instance validation should be aborted
/// when this returns any Error — callers wire that gate the same way
/// they handle type-graph load errors today.
///
/// Pure: builds on `ctx.graph` (for type lookup, sealed-rule, closure),
/// `ctx.repo_index` (for reference target resolution inside meta bodies),
/// and `ctx.claims_by_path` (for reference closure checks). Diagnostics
/// are sorted by (path, span start, code) before return.
pub fn validate_meta_bodies(ctx: &ValidateContext) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    for (_, td) in ctx.graph.iter() {
        diags.extend(check_required_meta_is_meta(ctx, td));
        diags.extend(check_subtype_required_meta(ctx, td));
        let Some(blocks) = &td.meta_blocks else {
            continue;
        };
        for block in blocks {
            diags.extend(validate_meta_subregion(ctx, td, block));
        }
    }
    // Stable order across runs. Path first because meta blocks span multiple
    // TypeDef files, then position, then code.
    diags.sort_by(|a, b| {
        a.span
            .file
            .cmp(&b.span.file)
            .then(a.span.range.start.cmp(&b.span.range.start))
            .then(a.code.as_str().cmp(b.code.as_str()))
    });
    diags
}

/// Check that each `required:` obligation on `td` names a META type, its resolved
/// closure includes the engine meta marker. The base-side sibling of the
/// meta-position check in `validate_meta_subregion`. Fires
/// `required-meta-not-a-meta-type` for a PRESENT, resolvable target that is not
/// meta-legal. Skips the cases other gates own:
/// - a bare target absent from the graph, `required-meta-absent-type` (load_checks).
/// - a qualified target that does not resolve, the `type-repo-*` / `peer-type-not-found` gate.
/// Skipped entirely without a marker or cross-repo seam (au-core's own tests).
fn check_required_meta_is_meta(ctx: &ValidateContext, td: &TypeDef) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let Some(marker) = ctx.meta_marker else {
        return diags;
    };
    let Some(marker_id) = ctx
        .cross_repo
        .and_then(|r| r.peer_type_id(marker.name, marker.repo))
        .map(|p| p.id)
    else {
        return diags;
    };
    for r in &td.required_meta {
        if r.is_qualified() {
            // An unresolvable qualified target is the cross-repo gate's.
            let resolvable = ctx
                .resolution
                .and_then(|rg| rg.resolve_authored(&r.name, r.repo.as_deref()))
                .is_some();
            if !resolvable {
                continue;
            }
        } else if !ctx.graph.contains(&r.name) {
            // A bare absent target is `required-meta-absent-type`'s.
            continue;
        }
        let claim = TypeClaim::Bare(r.clone());
        let meta_legal = ctx
            .resolution
            .map(|rg| folded_closure_ids(rg, &claim).contains(&marker_id))
            .unwrap_or(false);
        if !meta_legal {
            diags.push(Diagnostic {
                code: codes::REQUIRED_META_NOT_A_META_TYPE,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), r.span),
                message: format!(
                    "type-def '{}' requires meta '{}', which is not a meta type; it must mix in '{}::{}'",
                    td.name.as_str(),
                    r.authored(),
                    marker.name,
                    marker.repo
                ),
                related: vec![],
                fix: None,
            });
        }
    }
    diags
}

/// Surface unmet `required:` obligations on a concrete type ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
///
/// A type `td` is on the hook when it is NON-ABSTRACT and some type in its closure
/// (reflexive, so a concrete declaring base is on its own hook) declares a
/// `required:` obligation. It satisfies each obligation by declaring, in its OWN
/// `meta:`, a value block whose type's closure includes the required meta type,
/// compared by folded `TypeId`. Fires `subtype-missing-required-meta` (warning)
/// per unmet obligation, `related` at the obligating base's def.
///
/// Runs over the resolution graph, so a cross-repo base's obligation and a
/// cross-repo required meta type both resolve. A repo that imports nothing has no
/// valid obligation (a meta type mixes in the `::au-engine` marker, an import), so
/// `resolution: None` short-circuits. Satisfaction is LITERAL: only `td`'s own
/// blocks count, an ancestor's surfaced block does not; `meta: []` (no blocks)
/// exempts nothing.
/// One unmet `required:` obligation on a concrete type: the required meta type,
/// plus the def that obligated it (for a diagnostic's `related` or a read).
#[derive(Debug, Clone)]
pub struct UnmetRequiredMeta {
    /// The required meta type's resolved identity.
    pub meta: crate::resolution::TypeId,
    /// The obligating base's source file and def span.
    pub base_source_path: PathBuf,
    pub base_span: ByteRange,
}

/// Compute the unmet `required:` obligations on `td`, the one computation behind
/// both the `subtype-missing-required-meta` diagnostic and the read. See
/// [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
///
/// A type is on the hook when NON-ABSTRACT and some type in its folded (parent)
/// closure declares a `required:` obligation. It satisfies each by declaring, in
/// its OWN `meta:`, a block whose type's folded closure includes the required
/// meta id. Abstract / sealed types are exempt (return empty). `rg` is the type's
/// repo's resolution graph, over which cross-repo obligations resolve.
pub fn unmet_required_meta(
    graph: &TypeGraph,
    rg: &crate::resolution::ResolutionGraph,
    td: &TypeDef,
) -> Vec<UnmetRequiredMeta> {
    // Abstract / sealed types are not instance-claimable, so they are exempt.
    if graph.is_abstract(&td.name) {
        return Vec::new();
    }

    // Obligations: each ancestor in td's folded closure contributes its resolved
    // required-meta ids, keyed to the obligating base's def for `related`.
    let td_claim = TypeClaim::Bare(TypeNameClaim::own(td.name.clone(), td.source_span));
    let mut obligations: BTreeMap<crate::resolution::TypeId, (PathBuf, ByteRange)> =
        BTreeMap::new();
    for anc_id in folded_closure_ids(rg, &td_claim) {
        let Some(node) = rg.get(&anc_id) else {
            continue;
        };
        for m in &node.required_meta {
            obligations
                .entry(m.clone())
                .or_insert_with(|| (node.source_path.clone(), node.source_span));
        }
    }
    if obligations.is_empty() {
        return Vec::new();
    }

    // Satisfied set: the union of the folded closures of td's OWN block types.
    // Literal, ancestor surfacing is never consulted.
    let mut satisfied: BTreeSet<crate::resolution::TypeId> = BTreeSet::new();
    if let Some(blocks) = &td.meta_blocks {
        for block in blocks {
            let claim = TypeClaim::Bare(TypeNameClaim {
                name: block.type_name.clone(),
                repo: block.repo.clone(),
                span: block.type_name_span,
            });
            satisfied.extend(folded_closure_ids(rg, &claim));
        }
    }

    obligations
        .into_iter()
        .filter(|(meta_id, _)| !satisfied.contains(meta_id))
        .map(|(meta, (base_source_path, base_span))| UnmetRequiredMeta {
            meta,
            base_source_path,
            base_span,
        })
        .collect()
}

fn check_subtype_required_meta(ctx: &ValidateContext, td: &TypeDef) -> Vec<Diagnostic> {
    let Some(rg) = ctx.resolution else {
        return Vec::new();
    };
    unmet_required_meta(ctx.graph, rg, td)
        .into_iter()
        .map(|u| Diagnostic {
            code: codes::SUBTYPE_MISSING_REQUIRED_META,
            severity: Severity::Warning,
            span: Span::new(td.source_path.clone(), td.source_span),
            message: format!(
                "type-def '{}' is concrete but does not declare the required meta '{}'; a base in its closure obligates it",
                td.name.as_str(),
                u.meta.name.as_str()
            ),
            related: vec![Span::new(u.base_source_path, u.base_span)],
            fix: None,
        })
        .collect()
}

/// Validate one meta sub-region body against its named meta-type-def.
///
/// Single-name only per [[type-def meta::au-type-system]] — parser already rejects mixin at meta sites
/// (`META_MIXIN_NOT_SUPPORTED`). The per-field walk mirrors
/// `validate_inline_value`'s post-compat path: provided-set, recursive
/// `check_value_against_shape`, per-origin required-field-absent.
fn validate_meta_subregion(
    ctx: &ValidateContext,
    host: &TypeDef,
    block: &MetaBlock,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // The meta-type-def's identity claim, single-name (no mixin per [[type-def meta::au-type-system]]),
    // synthesized at the sub-region's `type:` span so downstream diagnostics
    // anchor correctly. A `::repo` meta type resolves against the peer's graph via
    // the fold, so it carries the qualifier; `effective_shape_for` then gathers
    // the folded peer fields, exactly as an own meta type gathers its own.
    //
    // A qualified meta that does NOT resolve in the fold (an absent / typo'd peer,
    // whose diagnostic the crosstype gate owns) is deferred here, so it neither
    // false-errors nor validates against a phantom shape.
    let claim_el = match &block.repo {
        Some(repo) => {
            let resolves = ctx
                .resolution
                .and_then(|rg| rg.resolve_authored(&block.type_name, Some(repo.as_str())))
                .is_some();
            if !resolves {
                return diags;
            }
            TypeNameClaim {
                name: block.type_name.clone(),
                repo: Some(repo.clone()),
                span: block.type_name_span,
            }
        }
        None => TypeNameClaim::own(block.type_name.clone(), block.type_name_span),
    };

    let scope = Scope {
        ctx,
        instance_path: &host.source_path,
        model: None,
    };
    let identity_claim = TypeClaim::Bare(claim_el.clone());

    let shape = match effective_shape_for(ctx, &identity_claim) {
        Ok(s) => s,
        Err(EffectiveShapeError::UnknownType(name)) => {
            diags.push(Diagnostic {
                code: codes::UNKNOWN_TYPE_CLAIM,
                severity: Severity::Error,
                span: Span::new(host.source_path.clone(), block.type_name_span),
                message: format!(
                    "meta sub-region on type-def '{}' names type-def '{}' which is not present in the type graph",
                    host.name.as_str(),
                    name.as_str()
                ),
                related: vec![],
                fix: None,
            });
            return diags;
        }
    };

    // Universal [[type-def sealed::au-type-system]] sealed-leaf rule fires at the meta site too,
    // mirroring the file-level and inline-value precedent. Same code,
    // new span anchor. The host's own sealing is irrelevant — what
    // matters is whether the meta-type-def the block claims is itself
    // sealed. An own claim reads the own graph; a `::repo` claim reads the folded
    // peer node.
    if claim_is_sealed(ctx, &claim_el) {
        diags.push(Diagnostic {
            code: codes::SEALED_PARENT_CLAIMED,
            severity: Severity::Error,
            span: Span::new(host.source_path.clone(), block.type_name_span),
            message: format!(
                "type-def '{}' is sealed; claims must drill to a non-sealed descendant",
                block.type_name.as_str()
            ),
            related: vec![],
            fix: None,
        });
    } else if claim_is_declared_abstract(ctx, &claim_el) {
        // A declared-abstract, non-sealed meta type claimed directly. Same
        // anchor as the sealed case, the meta sub-region's type-name span.
        diags.push(abstract_type_claimed_diag(
            &host.source_path,
            block.type_name_span,
            block.type_name.as_str(),
        ));
    } else if let Some(marker) = ctx.meta_marker {
        // Nominal meta-legality ([[spec - meta type marker - the meta position admits only types that mix in the engine meta base]]):
        // a claimable meta type is legal only if its RESOLVED closure includes the
        // engine meta marker. Only for a CLAIMABLE type, a non-claimable one
        // already fired the sharper sealed / abstract error above, so no
        // double-signal. An unresolvable X cannot reach here: a bare absent X
        // errored `unknown-type-claim`, a qualified absent X returned early.
        //
        // The marker id comes from the builtin via the cross-repo seam, present
        // whether or not the repo references the marker. X's folded closure comes
        // from the resolution graph. A repo that imports NOTHING has no resolution
        // graph, so X cannot mix in the cross-repo marker and is not meta-legal,
        // exactly the plain-record case this catches. Deferred only when the marker
        // itself cannot be identified (no cross-repo seam, au-core's own tests).
        let marker_id = ctx
            .cross_repo
            .and_then(|r| r.peer_type_id(marker.name, marker.repo))
            .map(|p| p.id);
        if let Some(marker_id) = marker_id {
            let meta_legal = ctx
                .resolution
                .map(|rg| folded_closure_ids(rg, &identity_claim).contains(&marker_id))
                .unwrap_or(false);
            if !meta_legal {
                diags.push(Diagnostic {
                    code: codes::NON_META_TYPE_IN_META_POSITION,
                    severity: Severity::Error,
                    span: Span::new(host.source_path.clone(), block.type_name_span),
                    message: format!(
                        "type-def '{}' is not a meta type; a meta sub-region's type must mix in '{}::{}'",
                        block.type_name.as_str(),
                        marker.name,
                        marker.repo
                    ),
                    related: vec![],
                    fix: None,
                });
            }
        }
    }

    // Per-field walk against the meta-type-def's effective shape. Mirrors
    // `validate_inline_value`'s post-compat-check loop — every body field
    // either fills a known field-origin or passes silently as an extra
    // (per [[type open-world validation::au-type-system]]). Reserved keys were rejected at parse time.
    //
    // [[type-def meta::au-type-system]] lock: this walk validates body fields against the named
    // type-def's STRUCTURE, not its meta. We never look at the named
    // type-def's own `meta_blocks` here — that's `lookup_meta`'s job,
    // and even there the walk targets the host's chain.
    // The meta sub-region's value model, built from the block's fields against
    // the meta-type-def's effective shape, so a meta reference / brand / pin value
    // verdict consumes the elaborated node instead of re-parsing, see
    // [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
    let model = build_field_model(&block.fields, &host.source_path, &shape);
    let meta_scope = Scope {
        ctx: scope.ctx,
        instance_path: scope.instance_path,
        model: Some(&model),
    };

    // The qualifier-aware per-field walk, shared with the frontmatter and inline
    // surfaces (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). A meta type is a single claim
    // but can inherit a divergent field from its own parents, so the meta surface
    // reaches the divergent case too.
    check_field_surface(
        &meta_scope,
        &shape,
        &block.fields,
        block.body_span,
        block.type_name_span,
        FieldSurface::Meta {
            host: host.name.as_str(),
            meta_type: block.type_name.as_str(),
        },
        &mut diags,
    );

    diags
}

fn check_reference(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    target_type: &str,
    demand_repo: Option<&str>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    // The demand's authored form for messages: bare `foo`, or `foo::repo` for a
    // qualified DEMAND (`foo::repo*`, the cross-repo membership case below).
    let shown_demand = match demand_repo {
        Some(r) => format!("{target_type}::{r}"),
        None => target_type.to_string(),
    };
    let s = match value {
        InstanceValue::String(s) => s,
        _ => {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value is not a wikilink — typed reference shape '{}*' needs a '[[target]]' value",
                    prefix, shown_demand
                ),
            )];
        }
    };
    // The re-parsing entry: parse the surface, then share the dispatch tail with
    // the value-model path (`reference_check`), so both reach the identical
    // verdict from the same parsed structure.
    check_parsed_reference_outcome(
        scope,
        // A String reaching this fallback is provably not a wikilink: the
        // elaborator classifies every `[[…]]`-shaped value into a `Reference` /
        // `MalformedReference` node (consumed above), and `looks_like_wikilink` is
        // exactly `parse_wikilink`'s `[[…]]`-shape gate, so a non-node String is
        // `NotAWikilink`. No surface re-parse, the value-model invariant.
        Err(&WikilinkParseError::NotAWikilink),
        s,
        value_span,
        target_type,
        demand_repo,
        field_key,
        origin,
        element_index,
    )
}

/// The reference value verdict from a PARSE OUTCOME, the single dispatch tail
/// shared by the re-parsing [`check_reference`] and the value-model
/// [`reference_check`].
///
/// Both hand it the same parsed structure — the re-parser from
/// [`parse_wikilink`], the model path from a [`ContributionValue::Reference`] /
/// [`MalformedReference`] node — so a reference value verdict is single-sourced
/// and cannot drift, see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
///
/// [`MalformedReference`]: ContributionValue::MalformedReference
#[allow(clippy::too_many_arguments)]
fn check_parsed_reference_outcome(
    scope: &Scope,
    parse: Result<&WikilinkRef, &WikilinkParseError>,
    s: &str,
    value_span: ByteRange,
    target_type: &str,
    demand_repo: Option<&str>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    let shown_demand = match demand_repo {
        Some(r) => format!("{target_type}::{r}"),
        None => target_type.to_string(),
    };
    match parse {
        Ok(wikilink) => check_reference_parsed(
            scope,
            wikilink,
            s,
            value_span,
            target_type,
            demand_repo,
            field_key,
            origin,
            element_index,
        ),
        Err(WikilinkParseError::NotAWikilink | WikilinkParseError::EmptyInner) => {
            vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value '{}' is not a wikilink — typed reference shape '{}*' needs a '[[target]]' value",
                    prefix, s, shown_demand
                ),
            )]
        }
        Err(err) => vec![wikilink_parse_diag(
            scope, value_span, origin, &prefix, s, err,
        )],
    }
}

/// The shared "consume the value-model node or re-parse" seam for every
/// reference arm (typed reference, compound reference, def reference, `any&`).
///
/// When the instance surface produced a node for this span, hand its parsed
/// structure to `on_node` — a clean [`ContributionValue::Reference`] as `Ok`, a
/// [`MalformedReference`] as `Err` — so the verdict never re-parses the surface,
/// the value-model invariant, see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
/// Any other node (a `Scalar`, a non-wikilink string) or an absent node (a
/// nested / meta value, an unmigrated surface) runs `reparse`, the arm's
/// existing re-parsing path, which yields the identical verdict. `on_node` gets
/// the caller's held string as `raw_display`, passed through verbatim rather
/// than rebuilt from the parsed structure.
///
/// [`MalformedReference`]: ContributionValue::MalformedReference
fn with_reference_model(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    on_node: impl FnOnce(Result<&WikilinkRef, &WikilinkParseError>, &str) -> Vec<Diagnostic>,
    reparse: impl FnOnce() -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    if let (Some(model), InstanceValue::String(s)) = (scope.model, value) {
        match model.node(value_span) {
            Some(ContributionValue::Reference {
                target,
                repo,
                commit,
                anchor,
                block_id,
            }) => {
                // The `:field` fragment is attribution, absent from a
                // frontmatter value, so `field: None`.
                let wikilink = WikilinkRef {
                    target: target.clone(),
                    repo: repo.clone(),
                    commit: commit.clone(),
                    anchor: anchor.clone(),
                    block_id: block_id.clone(),
                    field: None,
                };
                return on_node(Ok(&wikilink), s);
            }
            Some(ContributionValue::MalformedReference(err, _raw)) => {
                return on_node(Err(err), s);
            }
            _ => {}
        }
    }
    reparse()
}

/// A typed-reference (`name*`) value verdict, via the value model when present.
#[allow(clippy::too_many_arguments)]
fn reference_check(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    target_type: &str,
    demand_repo: Option<&str>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    with_reference_model(
        scope,
        value,
        value_span,
        |parse, s| {
            check_parsed_reference_outcome(
                scope,
                parse,
                s,
                value_span,
                target_type,
                demand_repo,
                field_key,
                origin,
                element_index,
            )
        },
        || {
            check_reference(
                scope,
                value,
                value_span,
                target_type,
                demand_repo,
                field_key,
                origin,
                element_index,
            )
        },
    )
}

/// The graph-existence half of [`check_reference`], keyed on the PARSED
/// [`WikilinkRef`] rather than the raw string.
///
/// [`check_reference`] parses then calls this; the value-model walker builds a
/// `WikilinkRef` from a [`ContributionValue::Reference`] node and calls it
/// directly, so a reference value verdict never re-parses the surface, see
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
///
/// `raw_display` is the original value string, used only to name the target in a
/// diagnostic for the local form (`[[^id]]`), whose parsed target is empty.
#[allow(clippy::too_many_arguments)]
fn check_reference_parsed(
    scope: &Scope,
    wikilink: &WikilinkRef,
    raw_display: &str,
    value_span: ByteRange,
    target_type: &str,
    demand_repo: Option<&str>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    // The demand's authored form for messages: bare `foo`, or `foo::repo` for a
    // qualified DEMAND (`foo::repo*`, the cross-repo membership case below).
    let shown_demand = match demand_repo {
        Some(r) => format!("{target_type}::{r}"),
        None => target_type.to_string(),
    };

    // A commit-referent (`[[::@sha]]` / `[[::repo@sha]]`) names a COMMIT, not a
    // file: there is nothing to resolve or type-check, and it is never dangling.
    // Anchor-only, see [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    if wikilink.is_commit_referent() {
        return Vec::new();
    }

    // A `::repo` reference crosses a repo boundary. Repo-local resolution
    // cannot see the target; the engine supplies a resolver that reaches into
    // the named repo and yields its graph, so the target's type is checked by
    // `(name, canonical-hash)` identity across the boundary. With no resolver
    // wired (au-core's own tests) or an unresolvable target (the engine's
    // cross-repo pass owns the `reference-repo-*` existence diagnostics), the
    // typed check is skipped here.
    let cross_target = if let Some(repo) = &wikilink.repo {
        match scope
            .ctx
            .cross_repo
            .and_then(|r| r.resolve(scope.instance_path, repo, &wikilink.target))
        {
            Some(t) => Some(t),
            None => return Vec::new(),
        }
    } else {
        None
    };

    // Local form per [[type reference::au-type-system]]: empty name + locating fragment
    // resolves to the host file itself — name lookup is skipped, so no
    // missing/ambiguous outcome exists.
    let resolved = if let Some(t) = &cross_target {
        t.path.clone()
    } else if wikilink.is_local() {
        scope.instance_path.to_path_buf()
    } else {
        match scope.ctx.repo_index.resolve(&wikilink.target) {
            Ok(p) => p,
            Err(ResolutionError::Missing) => {
                return vec![unresolved_target_diagnostic(
                    scope.instance_path,
                    value_span,
                    &prefix,
                    &wikilink.target,
                    vec![origin.related_span()],
                )];
            }
            Err(ResolutionError::Ambiguous(matches)) => {
                return vec![Diagnostic {
                    code: au_references::codes::REFERENCE_TARGET_AMBIGUOUS,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!(
                        "{} reference '{}' is ambiguous: {} files share the basename — use a repo-relative path",
                        prefix,
                        wikilink.target,
                        matches.len()
                    ),
                    related: matches.iter().map(|p| Span::for_file(p.clone())).collect(),
                    fix: None,
                }];
            }
        }
    };

    // Local refs read better named by their raw form than by the empty
    // target string; a cross-repo ref keeps its `::repo` qualifier.
    let shown_target: String = if let Some(repo) = &wikilink.repo {
        format!("{}::{}", wikilink.target, repo)
    } else if wikilink.is_local() {
        raw_display.to_string()
    } else {
        wikilink.target.clone()
    };

    // Anchor existence is independent of the typed check below —
    // navigational everywhere, see [[type reference::au-type-system]].
    let mut diags = slot_anchor_diags(scope, &resolved, wikilink, value_span, &prefix);

    // `any*` references any node by existence, no closure check ([[type-def shape any::au-type-system]]).
    // A `^block-id` target is allowed; its existence is still checked
    // navigationally, like an anchor on a `file*`.
    if target_type == "any" {
        diags.extend(block_id_existence_diags(
            scope, &resolved, wikilink, value_span, origin, &prefix,
        ));
        return diags;
    }

    // Built-in `file*` references a whole file by existence ([[type-def shape file::au-type-system]]).
    // A `^block-id` or `#head` addresses a part of the file, which `file*`
    // does not permit — `any*` is the block-addressing form. The fragment
    // is rejected outright, so the resolved-but-fragmented value never
    // reaches the navigational block-id / anchor checks.
    if target_type == "file" {
        if wikilink.block_id.is_some() || wikilink.anchor.is_some() {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} references '{}' with a sub-file fragment — `file*` references a whole file; use `any*` to address a `^block-id` or `#head`",
                    prefix, raw_display
                ),
            )];
        }
        return diags;
    }

    // [[type block-id::au-type-system]]: a `^block-id` wikilink targets an addressable
    // entity inside the file — a typed block or an inline record — not
    // the file itself. The entity's own claim is what the slot checks.
    // For a `^block-id`, the block's own claim in qualified form drives a
    // qualified DEMAND's fold (below); empty otherwise (a whole-file target folds
    // its frontmatter claim engine-side).
    let mut block_qualified: Vec<TypeNameClaim> = Vec::new();
    let target_claims: Cow<[TypeName]> = 'claims: {
        if let Some(block_id) = wikilink.block_id_str() {
            match resolve_block_target(
                scope, &resolved, wikilink, block_id, value_span, origin, &prefix,
            ) {
                BlockTarget::Claims { names, qualified } => {
                    if names.is_empty() {
                        // The target record carries no claim — that's already
                        // diagnosed at the target file (missing-type under a
                        // demanding slot). Don't double-fire here.
                        return diags;
                    }
                    block_qualified = qualified;
                    break 'claims Cow::Owned(names);
                }
                // A bare `^` navigational anchor over an existing (or no-body)
                // target: the FILE is the referent. Fall through to the file's
                // claim below (the whole-file path).
                BlockTarget::FileReferent => {}
                // No checkable referent: a dangling bare `^` (warning) or a `^^`
                // block-referent that is plain / absent (errors). Emit and skip
                // the type check — there is nothing to check.
                BlockTarget::Unresolved { diags: block_diags } => {
                    diags.extend(block_diags);
                    return diags;
                }
            }
        }
        let Some(target_claims) = scope.ctx.ref_data.claims(&resolved) else {
            // Target exists but isn't a typed instance — typed refs (`name*`)
            // require the target to declare `type:` whose closure includes
            // `name`. A plain note or asset doesn't qualify.
            // Related span points at the slot's shape decl; the target file
            // is named in the message itself, so a zero-range related span
            // for it would be redundant noise.
            diags.push(Diagnostic {
                code: codes::REFERENCE_TARGET_TYPE_MISMATCH,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), value_span),
                message: format!(
                    "{} references '{}' which has no `type:` claim; typed reference '{}*' needs a target whose closure includes '{}'",
                    prefix, shown_target, shown_demand, shown_demand
                ),
                related: vec![origin.related_span()],
                fix: None,
            });
            return diags;
        };
        target_claims
    };

    // A qualified DEMAND (`foo::repo*`) checks the peer type `foo` as `demand_repo`
    // defines it, so membership is `TypeId` equality over the target's FOLDED
    // closure (its own repo's resolution graph), not a name + `closure_id` compare
    // over plain graphs. The seam supplies the demanded id and the target's folded
    // ids; au-core compares — the qualified sibling of the unqualified path below.
    if let Some(demand_repo) = demand_repo {
        // A `^block-id` target's satisfaction is its BLOCK's own claim, folded
        // over the target repo's resolution graph, so pass the block's qualified
        // claim as the fold override; a whole-file target folds its frontmatter
        // claim (override `None`, engine-read). An ungated / unfolded body
        // typed-fence `::repo` claim is uncheckable, so the seam returns `None`
        // and the membership is skipped rather than mis-judged.
        // Only a `^^` block-referent overrides with the BLOCK's own claim; a
        // bare `^` is a file-referent, so it folds the file's claim (override None).
        let block_claim =
            wikilink
                .block_id
                .as_ref()
                .filter(|b| b.referent)
                .map(|_| TypeClaim::List {
                    items: block_qualified.clone(),
                    value_span,
                });
        match scope.ctx.cross_repo.and_then(|r| {
            r.qualified_demand(target_type, demand_repo, &resolved, block_claim.as_ref())
        }) {
            Some(qd) if !qd.target_folded.contains(&qd.demanded) => {
                let claim_names: Vec<&str> = target_claims.iter().map(|n| n.as_str()).collect();
                diags.push(Diagnostic {
                    code: codes::REFERENCE_TARGET_TYPE_MISMATCH,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!(
                        "{} references '{}' (claims: {}) but its `type:` closure does not include the peer type '{}'",
                        prefix,
                        shown_target,
                        claim_names.join(", "),
                        shown_demand
                    ),
                    related: vec![origin.related_span()],
                    fix: None,
                });
            }
            // Satisfied, or the demand / target repo is unresolvable (skip; the
            // `crosstype` gate owns that diagnostic) or no resolver wired.
            _ => {}
        }
        return diags;
    }

    let satisfied = match &cross_target {
        Some(t) => target_closure_includes_cross_repo(
            scope.ctx.graph,
            t.graph,
            &target_claims,
            target_type,
        ),
        None => target_closure_includes(scope.ctx.graph, &target_claims, target_type),
    };
    if !satisfied {
        let claim_names: Vec<&str> = target_claims.iter().map(|n| n.as_str()).collect();
        diags.push(Diagnostic {
            code: codes::REFERENCE_TARGET_TYPE_MISMATCH,
            severity: Severity::Error,
            span: Span::new(scope.instance_path.to_path_buf(), value_span),
            message: format!(
                "{} references '{}' (claims: {}) but its `type:` closure does not include '{}'",
                prefix,
                shown_target,
                claim_names.join(", "),
                target_type
            ),
            related: vec![origin.related_span()],
            fix: None,
        });
    }

    diags
}

/// Validate a value against a `type<T>*` / `type*` def-reference slot
/// ([[type-def shape def-ref::au-type-system]]). The value must be a wikilink to a type-def file; the
/// constraint is checked on the def axis — the target def's own closure (itself
/// plus its `type:` parents) must satisfy the bound. The unconstrained `type*`
/// (bound `None`) skips the closure check, any type-def by existence.
///
/// Mirrors `check_reference`'s wikilink resolution, but the target is a def, not
/// an instance: a `^block-id` / `#head` fragment is meaningless, a def is
/// whole-def only like `file*`.
#[allow(clippy::too_many_arguments)]
fn check_def_reference(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    bound: Option<&DefBound>,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    let s = match value {
        InstanceValue::String(s) => s,
        _ => {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value is not a wikilink — def-reference shape '{}' needs a '[[target]]' value",
                    prefix, shape
                ),
            )];
        }
    };
    check_parsed_def_outcome(
        scope,
        // A String reaching this fallback is provably not a wikilink: the
        // elaborator classifies every `[[…]]`-shaped value into a `Reference` /
        // `MalformedReference` node (consumed above), and `looks_like_wikilink` is
        // exactly `parse_wikilink`'s `[[…]]`-shape gate, so a non-node String is
        // `NotAWikilink`. No surface re-parse, the value-model invariant.
        Err(&WikilinkParseError::NotAWikilink),
        s,
        value_span,
        bound,
        shape,
        field_key,
        origin,
        element_index,
    )
}

/// The def-axis sibling of [`check_parsed_reference_outcome`], the dispatch tail
/// shared by the re-parsing [`check_def_reference`] and the value-model
/// [`def_reference_check`].
#[allow(clippy::too_many_arguments)]
fn check_parsed_def_outcome(
    scope: &Scope,
    parse: Result<&WikilinkRef, &WikilinkParseError>,
    s: &str,
    value_span: ByteRange,
    bound: Option<&DefBound>,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    match parse {
        Ok(wikilink) => check_def_reference_parsed(
            scope,
            wikilink,
            s,
            value_span,
            bound,
            shape,
            field_key,
            origin,
            element_index,
        ),
        Err(WikilinkParseError::NotAWikilink | WikilinkParseError::EmptyInner) => {
            vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value '{}' is not a wikilink — def-reference shape '{}' needs a '[[target]]' value",
                    prefix, s, shape
                ),
            )]
        }
        Err(err) => vec![wikilink_parse_diag(
            scope, value_span, origin, &prefix, s, err,
        )],
    }
}

/// A `type<T>*` / `type*` value verdict, via the value model when present.
#[allow(clippy::too_many_arguments)]
fn def_reference_check(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    bound: Option<&DefBound>,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    with_reference_model(
        scope,
        value,
        value_span,
        |parse, s| {
            check_parsed_def_outcome(
                scope,
                parse,
                s,
                value_span,
                bound,
                shape,
                field_key,
                origin,
                element_index,
            )
        },
        || {
            check_def_reference(
                scope,
                value,
                value_span,
                bound,
                shape,
                field_key,
                origin,
                element_index,
            )
        },
    )
}

/// The graph-existence half of [`check_def_reference`], keyed on the PARSED
/// [`WikilinkRef`] rather than the raw string, the def-axis sibling of
/// [`check_reference_parsed`]. `raw_display` names the target in the sub-file-
/// fragment message.
#[allow(clippy::too_many_arguments)]
fn check_def_reference_parsed(
    scope: &Scope,
    wikilink: &WikilinkRef,
    raw_display: &str,
    value_span: ByteRange,
    bound: Option<&DefBound>,
    shape: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);

    // A def is whole-def only — a sub-file fragment is meaningless, like
    // `file*` ([[type-def shape def-ref::au-type-system]] / [[type-def shape file::au-type-system]]).
    if wikilink.block_id.is_some() || wikilink.anchor.is_some() {
        return vec![field_shape_mismatch_diag(
            scope,
            value_span,
            origin,
            format!(
                "{} references '{}' with a sub-file fragment — a def-reference points at a whole type-def; drop the '^block-id' / '#head'",
                prefix, raw_display
            ),
        )];
    }

    // Cross-repo: defer to the resolver like `check_reference`. Without one,
    // the engine's cross-repo pass owns the existence diagnostics; skip here.
    let cross_target = if let Some(repo) = &wikilink.repo {
        match scope
            .ctx
            .cross_repo
            .and_then(|r| r.resolve(scope.instance_path, repo, &wikilink.target))
        {
            Some(t) => Some(t),
            None => return Vec::new(),
        }
    } else {
        None
    };

    let shown_target: String = if let Some(repo) = &wikilink.repo {
        format!("{}::{}", wikilink.target, repo)
    } else {
        wikilink.target.clone()
    };

    // Resolve like any reference, through the repo index — a type-def file is
    // reachable by its type-name ([[type-def shape def-ref::au-type-system]]), so the same
    // resolution serves validation, navigational edges, and backlinks alike.
    // The resolved file must be a type-def: its path maps to a name the graph
    // holds. A cross-repo target is resolved by the engine's resolver and
    // checked in its own graph.
    let def_graph: &TypeGraph = match &cross_target {
        Some(t) => t.graph,
        None => scope.ctx.graph,
    };
    let resolved = if let Some(t) = &cross_target {
        t.path.clone()
    } else {
        match scope.ctx.repo_index.resolve(&wikilink.target) {
            Ok(p) => p,
            Err(ResolutionError::Missing) => {
                return vec![unresolved_target_diagnostic(
                    scope.instance_path,
                    value_span,
                    &prefix,
                    &wikilink.target,
                    vec![origin.related_span()],
                )];
            }
            Err(ResolutionError::Ambiguous(matches)) => {
                return vec![Diagnostic {
                    code: au_references::codes::REFERENCE_TARGET_AMBIGUOUS,
                    severity: Severity::Error,
                    span: Span::new(scope.instance_path.to_path_buf(), value_span),
                    message: format!(
                        "{} reference '{}' is ambiguous: {} files share the basename — use a repo-relative path",
                        prefix,
                        wikilink.target,
                        matches.len()
                    ),
                    related: matches.iter().map(|p| Span::for_file(p.clone())).collect(),
                    fix: None,
                }];
            }
        }
    };
    let def_name: TypeName = match crate::typedef::type_name_from_path(&resolved) {
        Some(n) if def_graph.contains(&n) => n,
        _ => {
            // A type-def by this name exists but a same-named non-type file won
            // the stem rule (a single-segment name colliding with a note). Point
            // at the explicit path so the author can reach the def.
            let collision = def_graph.contains(&TypeName(wikilink.target.clone()));
            let hint = if collision {
                format!(
                    " — a non-type file shares the name '{}'; reference the def by its path 'type/{}.type.yaml'",
                    wikilink.target, wikilink.target
                )
            } else {
                String::new()
            };
            return vec![Diagnostic {
                code: codes::DEF_REF_TARGET_NOT_A_TYPE_DEF,
                severity: Severity::Error,
                span: Span::new(scope.instance_path.to_path_buf(), value_span),
                message: format!(
                    "{} references '{}', which is not a type-def — def-reference shape '{}' needs a type-def target{}",
                    prefix, shown_target, shape, hint
                ),
                related: vec![origin.related_span()],
                fix: None,
            }];
        }
    };

    // Unconstrained `type*` — any type-def by existence, no closure check.
    let Some(bound) = bound else {
        return Vec::new();
    };

    // Constrained — the target def's parent closure must satisfy the bound:
    // any-of for a union, all-of for an intersection. A ceiling is matched by
    // `(name, canonical-hash)` identity, the def-axis reuse of the instance-
    // reference cross-repo check: cross-repo, a drifted vendor of the ceiling
    // (same name, diverged contract) is a mismatch, not a match. Repo-local the
    // source and target graphs are the same, so this reduces to name membership.
    let source_graph = scope.ctx.graph;
    let def_claim_names = [def_name.clone()];
    // A `::repo` ceiling is the def-axis qualified demand: the ceiling's peer id
    // must be in the target DEF's FOLDED parent closure, via the same seam as the
    // instance-reference demand, with the def's OWN name as the folded claim (a
    // def's "closure" is its parent chain). A bare ceiling keeps the name+hash
    // path over the def's graph.
    let def_claim = TypeClaim::Bare(TypeNameClaim::own(def_name.clone(), value_span));
    // `Some(bool)` = checked; `None` = uncheckable (an unresolvable `::repo`
    // ceiling whose shape diagnostic the `crosstype` gate owns), so skip.
    let ceiling_ok = |t: &QualifiedName| -> Option<bool> {
        match &t.repo {
            None => Some(target_closure_includes_cross_repo(
                source_graph,
                def_graph,
                &def_claim_names,
                t.as_str(),
            )),
            Some(r) => scope
                .ctx
                .cross_repo
                .and_then(|res| res.qualified_demand(t.as_str(), r, &resolved, Some(&def_claim)))
                .map(|qd| qd.target_folded.contains(&qd.demanded)),
        }
    };
    let results: Vec<Option<bool>> = match bound {
        DefBound::Single(t) => vec![ceiling_ok(t)],
        DefBound::Compound { branches, .. } => branches.iter().map(|t| ceiling_ok(t)).collect(),
    };
    // An uncheckable ceiling makes the whole bound uncheckable (the gate owns the
    // ceiling's shape diagnostic), so skip rather than false-fire.
    if results.iter().any(|r| r.is_none()) {
        return Vec::new();
    }
    let oks: Vec<bool> = results.into_iter().flatten().collect();
    let satisfied = match bound {
        DefBound::Single(_) => oks[0],
        DefBound::Compound {
            op: CompoundRefOp::Union,
            ..
        } => oks.iter().any(|&o| o),
        DefBound::Compound {
            op: CompoundRefOp::Intersection,
            ..
        } => oks.iter().all(|&o| o),
    };
    if satisfied {
        return Vec::new();
    }
    vec![Diagnostic {
        code: codes::DEF_REF_CLOSURE_MISMATCH,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message: format!(
            "{} references type-def '{}' but its parent closure does not include {}",
            prefix,
            def_name.as_str(),
            describe_def_bound(bound)
        ),
        related: vec![origin.related_span()],
        fix: None,
    }]
}

/// Render a `DefBound` for the `def-ref-closure-mismatch` message.
fn describe_def_bound(bound: &DefBound) -> String {
    match bound {
        DefBound::Single(t) => format!("the required type '{}'", t),
        DefBound::Compound {
            op: CompoundRefOp::Union,
            branches,
        } => format!(
            "any of the required types '{}'",
            branches
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        DefBound::Compound {
            op: CompoundRefOp::Intersection,
            branches,
        } => format!(
            "all of the required types '{}'",
            branches
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" & ")
        ),
    }
}

/// Render a numeric instance value for a diagnostic message.
fn num_display(value: &InstanceValue) -> String {
    match value {
        InstanceValue::Integer(i) => i.to_string(),
        InstanceValue::Float(f) => f.to_string(),
        _ => String::new(),
    }
}

/// Check a value against a slot's value refinement ([[type-def field shape::au-type-system]]),
/// returning a message describing the first failed predicate, or `None` when
/// the value satisfies the meet. The value has already passed the base check.
/// A malformed numeric bound literal is ignored here (flagged at load).
fn refinement_value_violation(
    base: Primitive,
    r: &au_grammar::Refinement,
    value: &InstanceValue,
) -> Option<String> {
    match base {
        Primitive::Number => {
            let v = match value {
                InstanceValue::Integer(i) => *i as f64,
                InstanceValue::Float(f) => *f,
                _ => return None,
            };
            if let Some(b) = &r.lower {
                if let Ok(bv) = b.value.parse::<f64>() {
                    let ok = if b.inclusive { v >= bv } else { v > bv };
                    if !ok {
                        return Some(format!(
                            "value {} is not {} {}",
                            num_display(value),
                            if b.inclusive { ">=" } else { ">" },
                            b.value
                        ));
                    }
                }
            }
            if let Some(b) = &r.upper {
                if let Ok(bv) = b.value.parse::<f64>() {
                    let ok = if b.inclusive { v <= bv } else { v < bv };
                    if !ok {
                        return Some(format!(
                            "value {} is not {} {}",
                            num_display(value),
                            if b.inclusive { "<=" } else { "<" },
                            b.value
                        ));
                    }
                }
            }
            if r.integer {
                let is_int = match value {
                    InstanceValue::Integer(_) => true,
                    InstanceValue::Float(f) => f.fract() == 0.0,
                    _ => true,
                };
                if !is_int {
                    return Some(format!("value {} is not an integer", num_display(value)));
                }
            }
            None
        }
        // The canonical `Date` / `DateTime` forms compare lexicographically the
        // same as chronologically, so a string compare enforces the bound. A
        // bound with a MALFORMED literal is skipped here (flagged at load as
        // `refinement-bad-shape`), symmetric with the numeric parse guard, so a
        // garbage bound never spuriously fails an instance value.
        Primitive::Date | Primitive::DateTime => {
            let InstanceValue::String(s) = value else {
                return None;
            };
            let valid_literal = |lit: &str| match base {
                Primitive::Date => is_iso_date(lit),
                Primitive::DateTime => is_iso_datetime(lit),
                _ => true,
            };
            if let Some(b) = &r.lower {
                if valid_literal(&b.value) {
                    let ok = if b.inclusive {
                        s.as_str() >= b.value.as_str()
                    } else {
                        s.as_str() > b.value.as_str()
                    };
                    if !ok {
                        return Some(format!(
                            "value '{}' is not {} {}",
                            s,
                            if b.inclusive { ">=" } else { ">" },
                            b.value
                        ));
                    }
                }
            }
            if let Some(b) = &r.upper {
                if valid_literal(&b.value) {
                    let ok = if b.inclusive {
                        s.as_str() <= b.value.as_str()
                    } else {
                        s.as_str() < b.value.as_str()
                    };
                    if !ok {
                        return Some(format!(
                            "value '{}' is not {} {}",
                            s,
                            if b.inclusive { "<=" } else { "<" },
                            b.value
                        ));
                    }
                }
            }
            None
        }
        Primitive::String => {
            let InstanceValue::String(s) = value else {
                return None;
            };
            if let Some(pat) = &r.pattern {
                // A malformed pattern is flagged at load; skip enforcement here.
                if let Ok(re) = regex::Regex::new(pat) {
                    if !re.is_match(s) {
                        return Some(format!("value '{}' does not match /{}/", s, pat));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// A load-time refinement error ([[type-def field shape::au-type-system]]): a regex that does
/// not compile (also rejecting the non-regular backreference / lookahead forms),
/// or a `Date` / `DateTime` bound literal that is not a valid date. `None` when
/// the refinement is well-formed. Surfaced as `refinement-bad-shape`.
pub(crate) fn refinement_load_error(base: Primitive, r: &au_grammar::Refinement) -> Option<String> {
    if let Some(pat) = &r.pattern {
        if regex::Regex::new(pat).is_err() {
            return Some(format!(
                "regex predicate /{}/ is not a valid regular expression",
                pat
            ));
        }
    }
    if matches!(base, Primitive::Date | Primitive::DateTime) {
        for b in [r.lower.as_ref(), r.upper.as_ref()].into_iter().flatten() {
            let valid = match base {
                Primitive::Date => is_iso_date(&b.value),
                Primitive::DateTime => is_iso_datetime(&b.value),
                _ => true,
            };
            if !valid {
                return Some(format!(
                    "'{}' is not a valid {} literal",
                    b.value,
                    base.as_str()
                ));
            }
        }
    }
    None
}

/// Whether a numeric value refinement's meet admits no value at all
/// ([[type-def field shape::au-type-system]]): an empty interval, a single exclusive point, or
/// no integer in the range under `integer`. Only numeric emptiness is detected;
/// a malformed bound literal makes the check conservative (not empty).
pub(crate) fn refinement_unsatisfiable(base: Primitive, r: &au_grammar::Refinement) -> bool {
    if base != Primitive::Number {
        return false;
    }
    let lo = r
        .lower
        .as_ref()
        .and_then(|b| b.value.parse::<f64>().ok().map(|v| (v, b.inclusive)));
    let hi = r
        .upper
        .as_ref()
        .and_then(|b| b.value.parse::<f64>().ok().map(|v| (v, b.inclusive)));
    let (Some((lv, li)), Some((hv, hincl))) = (lo, hi) else {
        return false;
    };
    if lv > hv {
        return true;
    }
    if lv == hv && (!li || !hincl) {
        return true;
    }
    if r.integer {
        return !interval_contains_integer(lv, li, hv, hincl);
    }
    false
}

/// Whether every value the refinement `a` accepts is also accepted by `b` — the
/// region SUBSET test on values ([[spec - field-level refinement - value predicates and range cardinality as meets on a primitive slot]]).
/// `None` is the unconstrained base (the ⊤ region). Sound and conservative: an
/// unprovable case (a malformed numeric literal, or two differing regexes)
/// returns `false`. The shared primitive a compatibility classifier or field
/// narrowing calls; both `a` and `b` are refinements of `base`.
pub(crate) fn refinement_region_subset(
    base: Primitive,
    a: Option<&au_grammar::Refinement>,
    b: Option<&au_grammar::Refinement>,
) -> bool {
    let Some(b) = b else {
        return true; // everything ⊆ ⊤
    };
    let Some(a) = a else {
        return false; // ⊤ ⊄ a proper refinement
    };
    match base {
        Primitive::Number => {
            let (Ok(alo), Ok(ahi), Ok(blo), Ok(bhi)) = (
                num_bound(&a.lower),
                num_bound(&a.upper),
                num_bound(&b.lower),
                num_bound(&b.upper),
            ) else {
                return false; // a malformed literal makes subset unprovable
            };
            lower_subset(alo, blo) && upper_subset(ahi, bhi) && (!b.integer || a.integer)
        }
        // The canonical `Date` / `DateTime` forms order lexicographically. A
        // malformed bound literal (flagged at load) makes the subset unprovable,
        // so be conservative, symmetric with the numeric `num_bound` guard.
        Primitive::Date | Primitive::DateTime => {
            let valid = |bd: &Option<au_grammar::Bound>| {
                bd.as_ref().map_or(true, |b| match base {
                    Primitive::Date => is_iso_date(&b.value),
                    Primitive::DateTime => is_iso_datetime(&b.value),
                    _ => true,
                })
            };
            if !(valid(&a.lower) && valid(&a.upper) && valid(&b.lower) && valid(&b.upper)) {
                return false;
            }
            lower_subset(str_bound(&a.lower), str_bound(&b.lower))
                && upper_subset(str_bound(&a.upper), str_bound(&b.upper))
        }
        Primitive::String => match (&a.pattern, &b.pattern) {
            (_, None) => true,
            (None, Some(_)) => false,
            // Conservative: only an equal pattern is a provable subset.
            (Some(pa), Some(pb)) => pa == pb,
        },
        _ => false, // Boolean / Url carry no refinement
    }
}

fn num_bound(b: &Option<au_grammar::Bound>) -> Result<Option<(f64, bool)>, ()> {
    match b {
        None => Ok(None),
        Some(bd) => bd
            .value
            .parse::<f64>()
            .map(|v| Some((v, bd.inclusive)))
            .map_err(|_| ()),
    }
}

fn str_bound(b: &Option<au_grammar::Bound>) -> Option<(&str, bool)> {
    b.as_ref().map(|bd| (bd.value.as_str(), bd.inclusive))
}

/// Lower-bound subset: is every value satisfying `a`'s lower bound also
/// satisfying `b`'s? `None` is unbounded below (⊤ on that side).
fn lower_subset<T: PartialOrd>(a: Option<(T, bool)>, b: Option<(T, bool)>) -> bool {
    match (a, b) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some((av, ai)), Some((bv, bi))) => bv < av || (bv == av && (bi || !ai)),
    }
}

/// Upper-bound subset, the mirror of [`lower_subset`].
fn upper_subset<T: PartialOrd>(a: Option<(T, bool)>, b: Option<(T, bool)>) -> bool {
    match (a, b) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some((av, ai)), Some((bv, bi))) => bv > av || (bv == av && (bi || !ai)),
    }
}

/// Whether a list count-range `a` is a subset of `b`: `b`'s floor is no higher
/// and its ceiling no lower ([[spec - field-level refinement - value predicates and range cardinality as meets on a primitive slot]]).
pub(crate) fn cardinality_subset(
    a_min: u32,
    a_max: Option<u32>,
    b_min: u32,
    b_max: Option<u32>,
) -> bool {
    if b_min > a_min {
        return false;
    }
    match (a_max, b_max) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(am), Some(bm)) => am <= bm,
    }
}

/// Whether the interval bounded by `(lv, li)` below and `(hv, hincl)` above
/// contains at least one integer. Inclusivity flags say whether each endpoint
/// is admitted.
fn interval_contains_integer(lv: f64, li: bool, hv: f64, hincl: bool) -> bool {
    // Smallest integer admitted by the lower bound.
    let n = if li {
        lv.ceil()
    } else if lv.fract() == 0.0 {
        lv + 1.0
    } else {
        lv.ceil()
    };
    if hincl {
        n <= hv
    } else {
        n < hv
    }
}

fn check_list(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    inner: &Shape,
    min: u32,
    max: Option<u32>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let prefix = format_field_prefix(field_key, element_index);
    let suffix = au_grammar::list_suffix_string(min, max);
    let elements = match value {
        InstanceValue::Sequence(els) => els,
        _ => {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value is not a list — declared shape is {}{}",
                    prefix, inner, suffix
                ),
            )];
        }
    };
    let len = u32::try_from(elements.len()).unwrap_or(u32::MAX);
    if len < min {
        return vec![field_shape_mismatch_diag(
            scope,
            value_span,
            origin,
            format!(
                "{} value has {} element(s) — declared shape {}{} requires at least {}",
                prefix, len, inner, suffix, min
            ),
        )];
    }
    if let Some(m) = max {
        if len > m {
            return vec![field_shape_mismatch_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} value has {} element(s) — declared shape {}{} allows at most {}",
                    prefix, len, inner, suffix, m
                ),
            )];
        }
    }
    let mut out = Vec::new();
    for (i, el) in elements.iter().enumerate() {
        // 1-indexed in user-facing messages: "element 1, element 2, ..."
        // matches how a reader would describe positions in prose.
        out.extend(check_value_against_shape(
            scope,
            &el.value,
            el.span,
            inner,
            field_key,
            origin,
            Some(i + 1),
        ));
    }
    out
}

/// Render the field-name portion of a diagnostic message. For top-level
/// fields → `"field 'X'"`. For list elements → `"field 'X' element N"`
/// where N is 1-indexed. Output drops into messages as the leading
/// `"{}"` slot.
fn format_field_prefix(field_key: &str, element_index: Option<usize>) -> String {
    match element_index {
        None => format!("field '{}'", field_key),
        Some(n) => format!("field '{}' element {}", field_key, n),
    }
}

/// `anchor-not-found` warning for a slot value's `#head` fragment,
/// when the resolved target verifiably lacks the heading. Anchors are
/// navigational everywhere, so the slot side warns like prose does.
fn slot_anchor_diags(
    scope: &Scope,
    resolved: &Path,
    wikilink: &au_references::WikilinkRef,
    value_span: ByteRange,
    prefix: &str,
) -> Vec<Diagnostic> {
    let Some(anchor) = wikilink.anchor.as_deref() else {
        return Vec::new();
    };
    if crate::body_validate::anchor_exists_in(scope.ctx, resolved, anchor) != Some(false) {
        return Vec::new();
    }
    let target_label: &str = if wikilink.is_local() {
        "this file"
    } else {
        &wikilink.target
    };
    vec![Diagnostic {
        code: codes::ANCHOR_NOT_FOUND,
        severity: Severity::Warning,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message: format!("{prefix} wikilink — heading `{anchor}` not found in {target_label}"),
        related: vec![],
        fix: None,
    }]
}

/// Outcome of resolving a block-id fragment ([[type block-id::au-type-system]]) in a typed
/// slot, keyed on the sigil MODE, not the target's typed-ness.
enum BlockTarget {
    /// A `^^` block-referent that resolved to a typed block or inline record:
    /// the entity's effective claim, which checks the slot. Empty `names` means
    /// the entity exists but carries no claim (already diagnosed at the target),
    /// so the caller skips the typed check.
    Claims {
        /// Bare claim names for the same-repo closure check. A fence `::repo`
        /// stays unsplit here, so it never spuriously matches an own bare demand.
        names: Vec<TypeName>,
        /// The claim in QUALIFIED form, for a qualified DEMAND that folds the
        /// block's own claim over its repo's resolution graph.
        qualified: Vec<TypeNameClaim>,
    },
    /// A bare `^` navigational anchor over an existing (or unverifiable no-body)
    /// target: the FILE is the referent, the caller type-checks its claim and
    /// `^id` is a jump anchor ([[type block-id::au-type-system]]).
    FileReferent,
    /// The block-id could not resolve to a checkable referent: a dangling bare
    /// `^` (a `navigational-block-id-not-found` warning), or a `^^` block-referent
    /// whose block is plain (`block-id-not-typed`) or absent (`block-id-not-found`),
    /// both errors. The caller emits `diags` and skips the typed check — there is
    /// no valid referent to check.
    Unresolved { diags: Vec<Diagnostic> },
}

/// Resolve a block-id fragment against the target's two addressable
/// surfaces ([[type block-id::au-type-system]]): inline records first (frontmatter
/// precedes the body in document order), fenced blocks and markers
/// second. Gated to reference slots by its call sites.
///
/// The MODE decides the outcome, locally, not the target's typed-ness:
/// - a bare `^` is navigational — the FILE is the referent, so any existing
///   block (or an unverifiable no-body target) is a `FileReferent`, and an
///   absent id is a `navigational-block-id-not-found` warning.
/// - a `^^` is a block-referent — the BLOCK's claim checks the slot, so a
///   typed block / inline record gives `Claims`, a plain block is
///   `block-id-not-typed`, and an absent id is `block-id-not-found`.
fn resolve_block_target(
    scope: &Scope,
    resolved: &Path,
    wikilink: &au_references::WikilinkRef,
    block_id: &str,
    value_span: ByteRange,
    origin: &Origin,
    prefix: &str,
) -> BlockTarget {
    let referent = wikilink.block_id.as_ref().is_some_and(|b| b.referent);
    let records = scope.ctx.ref_data.record_targets(resolved);
    let record = records.as_ref().and_then(|targets| targets.get(block_id));

    let target_label = if wikilink.is_local() {
        "this file".to_string()
    } else {
        wikilink.target.clone()
    };
    let caret = if referent { "^^" } else { "^" };
    let link = format!("[[{}{caret}{block_id}]]", wikilink.target);
    let unresolved = |code, severity, message| BlockTarget::Unresolved {
        diags: vec![Diagnostic {
            code,
            severity,
            span: Span::new(scope.instance_path.to_path_buf(), value_span),
            message,
            related: vec![origin.related_span()],
            fix: None,
        }],
    };

    if !referent {
        // Bare `^`: navigational, the FILE is the referent. Only the anchor's
        // EXISTENCE matters — an existing block (record or body, typed or plain)
        // resolves the anchor; an absent one is a navigational warning.
        if record.is_some() {
            return BlockTarget::FileReferent;
        }
        let Some(target_body) = scope.ctx.ref_data.body(resolved) else {
            // No body held (an asset, or a plain note the engine does not parse):
            // the anchor is unverifiable, so stay silent, the FILE is the
            // referent. A false warning is worse than a missed one.
            return BlockTarget::FileReferent;
        };
        let events = au_parser::scan_body(target_body);
        return match au_references::resolve_block_id(&events, block_id) {
            Ok(_) | Err(au_references::BlockResolutionError::NotTyped) => BlockTarget::FileReferent,
            Err(au_references::BlockResolutionError::NotFound) => unresolved(
                codes::NAVIGATIONAL_BLOCK_ID_NOT_FOUND,
                Severity::Warning,
                format!("{prefix} wikilink `{link}` — block-id `{block_id}` not found in {target_label}"),
            ),
        };
    }

    // `^^`: block-referent, the BLOCK's own claim checks the slot.
    if let Some(target) = record {
        return BlockTarget::Claims {
            names: target.claims.clone(),
            qualified: target.qualified.clone(),
        };
    }
    let Some(target_body) = scope.ctx.ref_data.body(resolved) else {
        // No body and no record: the demanded block does not exist.
        return unresolved(
            codes::BLOCK_ID_NOT_FOUND,
            Severity::Error,
            format!("{prefix} wikilink `{link}` — block-id `{block_id}` not found in {target_label} (no body, no record id)"),
        );
    };
    let events = au_parser::scan_body(target_body);
    match au_references::resolve_block_id(&events, block_id) {
        Ok(resolved_block) => {
            let (names, qualified) = block_claims_of(resolved_block.body);
            BlockTarget::Claims { names, qualified }
        }
        // Present but plain: `^^` demanded a typed value, the block has none.
        Err(au_references::BlockResolutionError::NotTyped) => unresolved(
            codes::BLOCK_ID_NOT_TYPED,
            Severity::Error,
            format!("{prefix} wikilink `{link}` — block `{block_id}` exists but isn't typed (no `[:field]` fence)"),
        ),
        // Absent on both surfaces: `^^` demanded a value from a block that does not exist.
        Err(au_references::BlockResolutionError::NotFound) => unresolved(
            codes::BLOCK_ID_NOT_FOUND,
            Severity::Error,
            format!("{prefix} wikilink `{link}` — block-id `{block_id}` not found in {target_label}"),
        ),
    }
}

/// Block-id existence check for the existence-only shapes (`any*`, and a `file`
/// branch of a union), where the block's TYPE is never checked — only that its
/// referent exists ([[type-def shape any::au-type-system]], [[type-def shape file::au-type-system]]).
///
/// Mode-aware on absence, since the referent differs:
/// - a bare `^` is navigational (the FILE is the referent), so an absent anchor
///   is a `navigational-block-id-not-found` warning, exactly like a `#head`.
/// - a `^^` is a block-referent (the BLOCK is the referent), so an absent block
///   is a `block-id-not-found` error — the demanded node does not exist.
///
/// A present block never fires here, typed or plain: existence-only shapes do
/// not demand a type, so `block-id-not-typed` is not theirs to raise.
fn block_id_existence_diags(
    scope: &Scope,
    resolved: &Path,
    wikilink: &au_references::WikilinkRef,
    value_span: ByteRange,
    origin: &Origin,
    prefix: &str,
) -> Vec<Diagnostic> {
    let Some(block) = wikilink.block_id.as_ref() else {
        return Vec::new();
    };
    let block_id = block.id.as_str();
    let target_label = if wikilink.is_local() {
        "this file".to_string()
    } else {
        wikilink.target.clone()
    };
    let caret = if block.referent { "^^" } else { "^" };
    let link = format!("[[{}{caret}{block_id}]]", wikilink.target);
    let absent = |suffix: &str| {
        let (code, severity) = if block.referent {
            (codes::BLOCK_ID_NOT_FOUND, Severity::Error)
        } else {
            (codes::NAVIGATIONAL_BLOCK_ID_NOT_FOUND, Severity::Warning)
        };
        vec![Diagnostic {
            code,
            severity,
            span: Span::new(scope.instance_path.to_path_buf(), value_span),
            message: format!(
                "{prefix} wikilink `{link}` — block-id `{block_id}` not found in {target_label}{suffix}"
            ),
            related: vec![origin.related_span()],
            fix: None,
        }]
    };

    // Existence across both surfaces, mode-agnostic. Records first.
    if scope
        .ctx
        .ref_data
        .record_targets(resolved)
        .is_some_and(|targets| targets.get(block_id).is_some())
    {
        return Vec::new();
    }
    let Some(target_body) = scope.ctx.ref_data.body(resolved) else {
        // No body held: a bare `^` anchor is unverifiable, so stay silent; a `^^`
        // block-referent cannot resolve to a value, so it is not-found.
        return if block.referent {
            absent(" (no body, no record id)")
        } else {
            Vec::new()
        };
    };
    let events = au_parser::scan_body(target_body);
    if matches!(
        au_references::resolve_block_id(&events, block_id),
        Err(au_references::BlockResolutionError::NotFound)
    ) {
        absent("")
    } else {
        Vec::new()
    }
}

/// Parse a resolved typed block's yaml body and extract its claim
/// names. Empty on parse failure — the embedded-record validation owns
/// those diagnostics.
fn block_claims_of(body: &str) -> (Vec<TypeName>, Vec<TypeNameClaim>) {
    let Ok(docs) = au_parser::yaml::parse(body) else {
        return (Vec::new(), Vec::new());
    };
    let Some(doc) = docs.first() else {
        return (Vec::new(), Vec::new());
    };
    let raws = crate::body_validate::extract_block_type_claims(doc);
    // `names` keep the raw string unsplit, so a fence `foo::repo` does not match
    // an own bare `foo` on the same-repo path; `qualified` splits the `::repo` for
    // the cross-repo fold. A body typed-fence `::repo` claim is an ungated,
    // unfolded position,
    // so the seam finds it unresolvable and skips rather than mis-judges.
    let names = raws.iter().map(|s| TypeName(s.clone())).collect();
    let qualified = raws
        .iter()
        .map(|s| TypeNameClaim::parse(s, ByteRange::new(0, 0)))
        .collect();
    (names, qualified)
}

fn target_closure_includes(graph: &TypeGraph, claims: &[TypeName], required: &str) -> bool {
    for claim in claims {
        if !graph.contains(claim) {
            continue;
        }
        let closure = closure_of(graph, claim);
        if closure.iter().any(|n| n.as_str() == required) {
            return true;
        }
    }
    false
}

/// Cross-repo analogue of [`target_closure_includes`]: the target lives in
/// `target_graph` (another repo), the slot's demanded type in `source_graph`.
///
/// A name match across repos is not enough, repo B's `note` may differ from
/// repo A's. Nor is the demanded type's own hash enough: two repos can write
/// the same `myType (type: foo)` while their `foo` diverges, so `myType` hashes
/// equal yet validates instances differently. Satisfaction is substitutability,
/// which depends on the EFFECTIVE contract, so the target's `required` type must
/// match the source's across the WHOLE referenced closure, not on the top-level
/// hash alone. The match is by precomputed `closure_id`, the FNV-1a fold of the
/// closure's `{name -> local-hash}` signature, equal ids mean the same type.
///
/// A `required` absent from the source graph is a repo-local dangling slot,
/// owned at load by `slot-references-absent-type`. There is no contract to
/// compare, so this returns unsatisfied — and the caller then fires an
/// instance-level `reference-target-type-mismatch` on top, the same double-fire
/// the repo-local path produces for an absent required. Consistent, not a
/// cross-repo quirk. Whether a source-absent required should instead be treated
/// as uncheckable to suppress that redundant instance-level error is an open
/// call, left matching repo-local for now.
///
/// This is a strict generalization: when `source_graph` and `target_graph` are
/// the same graph (repo-local), the closure ids match whenever the name is
/// reached, so it agrees with `target_closure_includes`.
pub(crate) fn target_closure_includes_cross_repo(
    source_graph: &TypeGraph,
    target_graph: &TypeGraph,
    claims: &[TypeName],
    required: &str,
) -> bool {
    let required_name = TypeName(required.to_string());
    if !source_graph.contains(&required_name) {
        return false;
    }
    // Does any claim's closure in the target reach a same-named `required`?
    let reached = claims.iter().any(|claim| {
        target_graph.contains(claim) && closure_of(target_graph, claim).contains(&required_name)
    });
    if !reached {
        return false;
    }
    // Same name reached. It satisfies the slot only if the two repos' `required`
    // types are the same type: equal referenced-closure identity. Read the
    // precomputed O(V+E) `closure_id` (memoized at build) instead of re-walking
    // and comparing whole `{name -> local-hash}` signatures per call, collapsing
    // the per-reference redundancy. The id is an FNV-1a fold of exactly that
    // signature, so it carries the same verdict, with the closure-level hash
    // collision surface the engine's other provisional content hashes (the
    // def-local `canonical_hash` this once compared) already accept.
    source_graph.closure_id(&required_name) == target_graph.closure_id(&required_name)
}

/// Per-field-name use-site index for the `mixed-bare-and-qualified-field`
/// rule (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). Both lists hold indices into `instance.fields` so
/// the caller can recover key spans when constructing the diagnostic.
#[derive(Default)]
struct FieldUseSites {
    bare: Vec<usize>,
    qualified: Vec<usize>,
}

/// Outcome of parsing an instance field's key. `Bare` means the key has no
/// `{`; `Qualified` means the key parses cleanly as `field{type}` (or
/// `field{type::repo}`) per [[type-def fields collision - auto-unify and qualified field::au-type-system]];
/// `Malformed` means the key uses the `{...}` qualifier but breaks the syntax
/// (unclosed brace, empty parts, stray braces, or a qualifier type that
/// violates the type-name regex).
pub(crate) enum QualifiedKey {
    Bare,
    Qualified {
        type_name: TypeName,
        repo: Option<String>,
        field_name: FieldName,
    },
    Malformed {
        reason: String,
    },
}

/// Parse an instance field's key into `Bare`, `Qualified { T, F }`, or
/// `Malformed`. Bare keys (no `{`) take the existing per-field path.
/// Qualified keys carry the spec [[type-def fields collision - auto-unify and qualified field::au-type-system]] semantics: `field{type}`
/// scopes the value to a specific closure ancestor, `field{type::repo}` to a
/// peer's. Field and type names cannot contain `{` or `}` ([[type-def legal names::au-type-system]]),
/// so the field / qualifier split is unambiguous.
pub(crate) fn parse_qualified_key(key: &str) -> QualifiedKey {
    // The field is everything before the first `{`; the qualifier (which may
    // carry a `::repo`) is enclosed in the trailing `{...}`.
    let Some(open) = key.find('{') else {
        return QualifiedKey::Bare;
    };
    let field = &key[..open];
    let rest = &key[open + 1..];
    let Some(inner) = rest.strip_suffix('}') else {
        return QualifiedKey::Malformed {
            reason: "qualifier brace is not closed; expected `field{type}`".into(),
        };
    };
    if field.is_empty() {
        return QualifiedKey::Malformed {
            reason: "field name before `{` is empty".into(),
        };
    }
    // A single enclosing `{...}` group: no stray braces on either side.
    if field.contains('}') || inner.contains('{') || inner.contains('}') {
        return QualifiedKey::Malformed {
            reason: "qualifier must be a single `{type}` or `{type::repo}` group".into(),
        };
    }
    if inner.is_empty() {
        return QualifiedKey::Malformed {
            reason: "qualifier inside `{}` is empty".into(),
        };
    }
    // Split an optional `::repo` off the qualifier type.
    let (base, repo) = match inner.split_once("::") {
        Some((b, r)) => (b, Some(r)),
        None => (inner, None),
    };
    // A stray extra colon (a second `::`, or a single `:`) is malformed.
    if base.contains(':') || repo.is_some_and(|r| r.contains(':')) {
        return QualifiedKey::Malformed {
            reason: "qualifier must be `{type}` or `{type::repo}`".into(),
        };
    }
    if base.is_empty() {
        return QualifiedKey::Malformed {
            reason: "qualifier type before `::` is empty".into(),
        };
    }
    if !is_valid_type_name(base) {
        return QualifiedKey::Malformed {
            reason: format!("qualifier type '{base}' violates the type-name regex"),
        };
    }
    if repo.is_some_and(|r| r.is_empty()) {
        return QualifiedKey::Malformed {
            reason: "qualifier repo after `::` is empty".into(),
        };
    }
    QualifiedKey::Qualified {
        type_name: TypeName(base.into()),
        repo: repo.map(|s| s.to_string()),
        field_name: FieldName(field.into()),
    }
}

/// The three surfaces the qualifier-aware per-field walk runs over. Names each
/// call site so `check_field_surface` can build surface-specific diagnostic
/// messages while running ONE copy of the routing + required + divergent +
/// mixed-form logic (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]).
enum FieldSurface<'a> {
    /// A file-level instance's frontmatter fields.
    Instance,
    /// An inline record / embedded typed block; `prefix` identifies the slot
    /// the value fills, for the message.
    Inline { prefix: &'a str },
    /// A meta sub-region body on `host`, claiming meta type `meta_type`.
    Meta { host: &'a str, meta_type: &'a str },
}

impl FieldSurface<'_> {
    /// The subject clause of a "missing required field" message, composed as
    /// `"{subject} is missing required field '…' (declared on '…')"` so each
    /// surface reads naturally.
    fn missing_subject(&self) -> String {
        match self {
            FieldSurface::Instance => "instance".to_string(),
            FieldSurface::Inline { prefix } => format!("{prefix}: inline value"),
            FieldSurface::Meta { host, meta_type } => {
                format!("meta sub-region '{meta_type}' on type-def '{host}'")
            }
        }
    }

    /// The locator phrase inside a `mixed-bare-and-qualified` message, e.g.
    /// `"field '…' has both bare and qualified entries {phrase} — …"`.
    fn mixed_form_phrase(&self) -> &'static str {
        match self {
            FieldSurface::Instance => "on this instance",
            FieldSurface::Inline { .. } => "on this inline value",
            FieldSurface::Meta { .. } => "in this meta sub-region",
        }
    }
}

/// The qualifier-aware per-field walk, shared by the frontmatter, inline-record,
/// and meta sub-region validators (three near-identical loops before this
/// extraction). For each key it routes bare / `field{type}` / malformed
/// (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]), then runs three post-passes:
/// - per-origin `required-field-absent` over the auto-unified (resolved) fields.
/// - divergent fields: a bare use fires `mixin-collision`, and every unfilled
///   required origin fires `required-field-absent`, additively.
/// - `mixed-bare-and-qualified-field` for an auto-unified field used both ways.
///
/// `required_span` anchors the required / divergent-required diagnostics (the
/// claim / inline / body span); `collision_span` anchors the bare-use
/// `mixin-collision` (the claim / identity / meta type-name span). The file
/// path is `scope.instance_path` throughout.
fn check_field_surface(
    scope: &Scope,
    shape: &EffectiveShape,
    fields: &[InstanceField],
    required_span: ByteRange,
    collision_span: ByteRange,
    surface: FieldSurface,
    diags: &mut Vec<Diagnostic>,
) {
    let path = scope.instance_path;

    // Per-origin provided set (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]). Each
    // `(origin_id, field_name)` is a slot the user filled, keyed by IDENTITY
    // (`OriginId`) so two same-named cross-repo identities (`note` vs
    // `note::base`) key apart. A bare key contributes one entry per auto-unified
    // origin; a qualified key contributes only the resolved origin's slot.
    let mut provided: BTreeSet<(OriginId, FieldName)> = BTreeSet::new();
    // Per-FieldName use-site map for `mixed-bare-and-qualified-field`. Indices
    // into `fields` so we can recover key spans for related diagnostics.
    let mut use_sites: BTreeMap<FieldName, FieldUseSites> = BTreeMap::new();

    for (i, f) in fields.iter().enumerate() {
        match parse_qualified_key(&f.key) {
            QualifiedKey::Bare => {
                let field_name = FieldName(f.key.clone());
                use_sites.entry(field_name).or_default().bare.push(i);
                check_bare_field(scope, shape, f, diags, &mut provided);
            }
            QualifiedKey::Malformed { reason } => {
                diags.push(Diagnostic {
                    code: codes::MALFORMED_QUALIFIER_KEY,
                    severity: Severity::Error,
                    span: Span::new(path.to_path_buf(), f.key_span),
                    message: format!(
                        "field key '{}' uses qualifier syntax (`field{{type}}`) but is malformed: {}",
                        f.key, reason
                    ),
                    related: vec![],
                    fix: None,
                });
            }
            QualifiedKey::Qualified {
                type_name,
                repo,
                field_name,
            } => {
                use_sites
                    .entry(field_name.clone())
                    .or_default()
                    .qualified
                    .push(i);
                check_qualified_field(
                    scope,
                    shape,
                    f,
                    &type_name,
                    repo.as_deref(),
                    &field_name,
                    diags,
                    &mut provided,
                );
            }
        }
    }

    // Required-field-absent (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]) — per-originator over the
    // auto-unified fields. Auto-unify collapses shapes, not optional-ness, so we
    // walk per-origin to honor per-origin required bits. A near-miss present key
    // adds a "did you mean" hint plus a related span at the suspected typo.
    let undeclared = undeclared_keys(shape, fields);
    for (field_name, field_origin) in shape.iter() {
        for (origin_id, info) in &field_origin.origins {
            if info.decl.optional {
                continue;
            }
            if !provided.contains(&(origin_id.clone(), field_name.clone())) {
                let mut message = format!(
                    "{} is missing required field '{}' (declared on '{}')",
                    surface.missing_subject(),
                    field_name.as_str(),
                    info.type_name.as_str()
                );
                let mut related = vec![Span::new(info.origin_path.clone(), info.decl.name_span)];
                if let Some((key, key_span)) =
                    nearest_undeclared_field(field_name.as_str(), &undeclared)
                {
                    message.push_str(&format!(" — did you mean '{key}'?"));
                    related.push(Span::new(path.to_path_buf(), key_span));
                }
                diags.push(Diagnostic {
                    code: codes::REQUIRED_FIELD_ABSENT,
                    severity: Severity::Error,
                    span: Span::new(path.to_path_buf(), required_span),
                    message,
                    related,
                    fix: None,
                });
            }
        }
    }

    // Divergent fields (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]) — required is required:
    // each REQUIRED origin not filled by a `field{origin}` qualifier fires
    // `required-field-absent`, ALWAYS (a bare value fills no origin; an
    // all-optional divergent field is clean when untouched). A BARE use fires
    // `mixin-collision`, ADDITIVELY.
    for (field_name, field_origin) in shape.divergent() {
        if use_sites
            .get(field_name)
            .is_some_and(|s| !s.bare.is_empty())
        {
            diags.push(mixin_collision_diag(
                path,
                collision_span,
                field_name,
                field_origin,
            ));
        }
        for (origin_id, info) in &field_origin.origins {
            if info.decl.optional {
                continue;
            }
            if !provided.contains(&(origin_id.clone(), field_name.clone())) {
                diags.push(Diagnostic {
                    code: codes::REQUIRED_FIELD_ABSENT,
                    severity: Severity::Error,
                    span: Span::new(path.to_path_buf(), required_span),
                    message: format!(
                        "{} is missing required field '{}' (declared on '{}') — qualify it as '{}{{{}}}'",
                        surface.missing_subject(),
                        field_name.as_str(),
                        origin_id.as_str(),
                        field_name.as_str(),
                        origin_id.as_str()
                    ),
                    related: vec![Span::new(info.origin_path.clone(), info.decl.name_span)],
                    fix: None,
                });
            }
        }
    }

    // Mixed-bare-and-qualified-field (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]) — one diagnostic per
    // field name used both bare and qualified. A divergent field's bare use is a
    // `mixin-collision`, not the mixed-form error, so skip it here.
    for (field_name, sites) in &use_sites {
        if sites.bare.is_empty() || sites.qualified.is_empty() {
            continue;
        }
        if shape.get_divergent(field_name).is_some() {
            continue;
        }
        let primary_idx = sites.bare[0];
        let mut related: Vec<Span> =
            Vec::with_capacity(sites.bare.len() + sites.qualified.len() - 1);
        for &i in sites.bare.iter().skip(1) {
            related.push(Span::new(path.to_path_buf(), fields[i].key_span));
        }
        for &i in &sites.qualified {
            related.push(Span::new(path.to_path_buf(), fields[i].key_span));
        }
        diags.push(Diagnostic {
            code: codes::MIXED_BARE_AND_QUALIFIED_FIELD,
            severity: Severity::Error,
            span: Span::new(path.to_path_buf(), fields[primary_idx].key_span),
            message: format!(
                "field '{}' has both bare and qualified entries {} — must be either all bare (auto-unify) or all qualified (explicit per-origin distinction)",
                field_name.as_str(),
                surface.mixed_form_phrase()
            ),
            related,
            fix: None,
        });
    }
}

/// Validate a bare-keyed instance field. Looks up the field name in the
/// auto-unified `shape.fields`; extras pass silently per [[type extras::au-type-system]]; mixin
/// collisions skip here (handled by the mixin-collision pass at the top
/// of `validate`). Lazy-surfaces deferred-shape errors at the use site.
///
/// Contributes `(origin, field_name)` to `provided` for every origin in
/// the field's auto-unified set — bare values satisfy the slot at every
/// auto-unified origin (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]).
fn check_bare_field(
    scope: &Scope,
    shape: &EffectiveShape,
    f: &InstanceField,
    diags: &mut Vec<Diagnostic>,
    provided: &mut BTreeSet<(OriginId, FieldName)>,
) {
    let field_name = FieldName(f.key.clone());
    // A bare key resolves only against the auto-unified `fields`. A divergent
    // field is absent here (it lives in `shape.divergent`); a bare use of it is a
    // `mixin-collision`, fired in the caller's post-loop, so this bare key
    // contributes nothing and validates nothing.
    let Some(field_origin) = shape.get(&field_name) else {
        return;
    };
    for origin_id in field_origin.origins.keys() {
        provided.insert((origin_id.clone(), field_name.clone()));
    }
    let canonical_decl = field_origin.canonical_decl();
    let origin = Origin {
        path: field_origin.canonical_origin_path(),
        shape_span: canonical_decl.shape_span,
    };
    match &canonical_decl.parsed_shape {
        Ok(parsed) => {
            diags.extend(check_value_against_shape(
                scope,
                &f.value,
                f.value_span,
                parsed,
                &f.key,
                &origin,
                None,
            ));
        }
        Err(shape_err) => {
            diags.push(Diagnostic {
                code: shape_err.code.clone(),
                severity: shape_err.severity,
                span: Span::new(scope.instance_path.to_path_buf(), f.value_span),
                message: format!("field '{}': {}", f.key, shape_err.message),
                related: vec![shape_err.span.clone()],
                fix: None,
            });
        }
    }
}

/// Validate a qualified instance field per [[type-def fields collision - auto-unify and qualified field::au-type-system]].
///
/// Resolution sequence:
/// 1. `T` must be in the instance's effective closure → else
///    `qualifier-not-in-closure`.
/// 2. Some type-def in `closure_of(T)` must literally declare `F` in its
///    `fields:` list → else `qualifier-does-not-declare-field`. The
///    no-redeclare rule guarantees that originator (if any) is unique.
/// 3. Validate the value against the originator's `parsed_shape` using
///    the same machinery as bare fields — same `field-shape-mismatch`
///    code, same lazy-surfacing for deferred-shape errors.
///
/// On clean resolution, contributes `(originator, field_name)` to
/// `provided` — exactly one slot, the one named by the qualifier (spec
/// [[type-def fields collision - auto-unify and qualified field::au-type-system]]). Failed-resolution branches do NOT contribute (the qualifier
/// diagnostic is the failure mode).
fn check_qualified_field(
    scope: &Scope,
    shape: &EffectiveShape,
    f: &InstanceField,
    qualifier_type: &TypeName,
    qualifier_repo: Option<&str>,
    field_name: &FieldName,
    diags: &mut Vec<Diagnostic>,
    provided: &mut BTreeSet<(OriginId, FieldName)>,
) {
    // Validate the qualifier and resolve the origin whose shape the value checks
    // against. The closure-membership, declares-field, and ambiguity rules live
    // in `resolve_qualifier`, shared with the body per-contribution path.
    let (originator_id, origin_path, decl) = match resolve_qualifier(
        scope.ctx,
        shape,
        scope.instance_path,
        field_name,
        qualifier_type,
        qualifier_repo,
        f.key_span,
    ) {
        Ok(resolved) => resolved,
        Err(diag) => {
            diags.push(diag);
            return;
        }
    };

    // Contribute the resolved (origin, field) slot — qualified values
    // satisfy only the named origin's slot, not all auto-unified origins
    // (spec [[type-def fields collision - auto-unify and qualified field::au-type-system]]).
    provided.insert((originator_id.clone(), field_name.clone()));

    let origin = Origin {
        path: &origin_path,
        shape_span: decl.shape_span,
    };
    match &decl.parsed_shape {
        Ok(parsed) => {
            diags.extend(check_value_against_shape(
                scope,
                &f.value,
                f.value_span,
                parsed,
                &f.key,
                &origin,
                None,
            ));
        }
        Err(shape_err) => {
            diags.push(Diagnostic {
                code: shape_err.code.clone(),
                severity: shape_err.severity,
                span: Span::new(scope.instance_path.to_path_buf(), f.value_span),
                message: format!("field '{}': {}", f.key, shape_err.message),
                related: vec![shape_err.span.clone()],
                fix: None,
            });
        }
    }
}

/// Validate a `field{type}` / `field{type::repo}` qualifier against the
/// instance's closure and resolve it to the origin whose decl the value checks
/// against, or the diagnostic explaining why it cannot:
/// `qualifier-not-in-closure`, `qualifier-does-not-declare-field`, or
/// `qualifier-ambiguous` (a divergent field reached through a non-declaring
/// descendant). The single home for qualifier legality, shared by the
/// frontmatter/inline key path ([`check_qualified_field`]) and the body
/// per-contribution path, so the two surfaces validate a qualifier identically
/// ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
///
/// A `::repo` qualifier resolves over the resolution graph (the peer type lives
/// there, not the own graph); a bare qualifier over the own graph. Either branch
/// yields the originator's authored `OriginId` (`name` for own, `name::repo` for
/// a peer), so a caller keys the `provided` slot by the SAME identity the
/// effective shape stored it under.
pub(crate) fn resolve_qualifier(
    ctx: &ValidateContext<'_>,
    shape: &EffectiveShape,
    instance_path: &Path,
    field_name: &FieldName,
    qualifier_type: &TypeName,
    qualifier_repo: Option<&str>,
    span: ByteRange,
) -> Result<(OriginId, PathBuf, crate::typedef::FieldDecl), Diagnostic> {
    // The authored qualifier, `T` or `T::repo`, for diagnostics.
    let shown_qualifier = match qualifier_repo {
        Some(r) => format!("{}::{}", qualifier_type.as_str(), r),
        None => qualifier_type.as_str().to_string(),
    };
    if !shape.instance_closure().contains(qualifier_type) {
        return Err(Diagnostic {
            code: codes::QUALIFIER_NOT_IN_CLOSURE,
            severity: Severity::Error,
            span: Span::new(instance_path.to_path_buf(), span),
            message: format!(
                "qualifier '{}' is not in the instance's closure — a valid qualifier must be a claimed type or one of its ancestors",
                shown_qualifier
            ),
            related: vec![],
            fix: None,
        });
    }
    // Every origin in the qualifier's closure that declares the field, in
    // deterministic closure order, so the ambiguity grouping is stable.
    let candidates: Vec<crate::closure::QualifierCandidate> = match qualifier_repo {
        Some(repo) => ctx
            .resolution
            .map(|rg| gather_qualifier_candidates_resolved(rg, qualifier_type, repo, field_name))
            .unwrap_or_default(),
        None => {
            let qualifier_closure = closure_of(ctx.graph, qualifier_type);
            qualifier_closure
                .iter()
                .filter_map(|tn| {
                    ctx.graph.get(tn).and_then(|td| {
                        td.fields
                            .iter()
                            .find(|fld| &fld.name == field_name)
                            .map(|decl| crate::closure::QualifierCandidate {
                                origin: OriginId(tn.as_str().to_string()),
                                path: td.source_path.clone(),
                                decl: decl.clone(),
                            })
                    })
                })
                .collect()
        }
    };
    match crate::closure::resolve_qualifier_candidates(candidates) {
        crate::closure::QualifierResolution::Unique(c) => Ok((c.origin, c.path, c.decl)),
        crate::closure::QualifierResolution::Absent => Err(Diagnostic {
            code: codes::QUALIFIER_DOES_NOT_DECLARE_FIELD,
            severity: Severity::Error,
            span: Span::new(instance_path.to_path_buf(), span),
            message: format!(
                "qualifier '{}' (and its closure) does not declare field '{}' — no type-def in the qualifier's chain has it in `fields:`",
                shown_qualifier,
                field_name.as_str()
            ),
            related: vec![],
            fix: None,
        }),
        crate::closure::QualifierResolution::Ambiguous(origins) => {
            let named = origins
                .iter()
                .map(|o| format!("{}{{{}}}", field_name.as_str(), o.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(Diagnostic {
                code: codes::QUALIFIER_AMBIGUOUS,
                severity: Severity::Error,
                span: Span::new(instance_path.to_path_buf(), span),
                message: format!(
                    "qualifier '{}' reaches divergent field '{}' at more than one origin — name a declaring origin directly ({})",
                    shown_qualifier,
                    field_name.as_str(),
                    named
                ),
                related: vec![],
                fix: None,
            })
        }
    }
}

/// The name of a brand-BEARING slot: `foo` (`Record`), `foo&`
/// (`InlineOrReference`), and `foo*` (`Reference`) all name a candidate brand
/// `foo`. Whether the name IS a brand is [`resolve_brand`]'s call; this only
/// selects the name-bearing shapes so the value-model walker can try a brand
/// verdict at every slot a brand can sit in. `None` for any other shape.
fn brand_slot_name(slot: &Shape) -> Option<&QualifiedName> {
    match slot {
        Shape::Record(n) | Shape::InlineOrReference(n) | Shape::Reference(n) => Some(n),
        _ => None,
    }
}

/// Route a brand VALUE verdict through the one value-model walker when the
/// instance surface produced a resolved node for it, else `None` (the caller
/// runs its own arm).
///
/// `effective_values` resolves a brand's constructor / bare / paren STRING value
/// to a `Scalar` / `Tuple` / `MalformedConstructor` node — the value the walker
/// owns — so a brand verdict never re-parses ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
/// A Mapping value (an inline record) and a `Reference` node (a wikilink at a
/// `foo&` / `foo*` union brand) are the surface-specific inline / reference paths,
/// so this returns `None` and leaves them to the arm; an absent node does too,
/// leaving the value to the caller's own arm.
fn brand_value_via_model(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    slot: &Shape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Option<Vec<Diagnostic>> {
    if matches!(value, InstanceValue::Mapping(_)) {
        return None;
    }
    let (node, brand) = scope.model?.node_and_brand(value_span)?;
    match node {
        ContributionValue::Scalar(_)
        | ContributionValue::Tuple(_)
        | ContributionValue::MalformedConstructor(_) => Some(check_contribution_value(
            scope.ctx,
            scope.instance_path,
            node,
            brand,
            value_span,
            slot,
            field_key,
            element_index,
            origin.path,
            origin.shape_span,
        )),
        // A Reference / inline-record / malformed-reference node is a
        // surface-specific path, not the brand value the walker owns.
        _ => None,
    }
}

/// Resolve a slot's named brand to its shape, own-repo or `::repo`. An own-repo
/// name reads the local graph; a `::repo` name reads the peer's graph through the
/// cross-repo resolver's `peer_graph` seam, so a peer brand validates exactly
/// like an own one. `None` when the name is not a brand (a record type, an
/// unresolvable peer), leaving the ordinary record / reference path to run.
fn resolve_brand<'a>(
    scope: &Scope<'a>,
    name: &QualifiedName,
) -> Option<&'a crate::typedef::BrandShape> {
    let key = TypeName(name.as_str().to_string());
    match &name.repo {
        None => scope.ctx.graph.get(&key).and_then(|t| t.shape.as_ref()),
        Some(repo) => scope
            .ctx
            .cross_repo?
            .peer_graph(repo)?
            .get(&key)
            .and_then(|t| t.shape.as_ref()),
    }
}

/// Classify a union member, resolving a `::repo` member against the PEER graph
/// (code-review 3.4). `classify_union_member` alone only resolves own-repo names,
/// so a peer member (`meter::units` inside a union) would be mis-classified as a
/// record and its nominal-brand value wrongly rejected. Here a `::repo` member is
/// resolved through the cross-repo resolver so a peer nominal brand is seen as
/// nominal, exactly as own-repo.
fn classify_member_xrepo(
    scope: &Scope,
    member: &Shape,
    member_graph: &TypeGraph,
) -> UnionMemberKind {
    if let Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) = member {
        if qn.repo.is_some() {
            return match resolve_member_brand(scope, qn, member_graph) {
                Some(b) if b.is_nominal() => UnionMemberKind::NominalBrand,
                _ => UnionMemberKind::Record,
            };
        }
    }
    classify_union_member(member, member_graph)
}

/// Resolve a union member's brand shape from the right graph: a `::repo` member
/// from the peer graph, an own member from `member_graph`. `None` when the name
/// is not a brand or the peer is unresolvable.
fn resolve_member_brand<'a>(
    scope: &Scope<'a>,
    qn: &QualifiedName,
    member_graph: &'a TypeGraph,
) -> Option<&'a crate::typedef::BrandShape> {
    let key = TypeName(qn.as_str().to_string());
    match &qn.repo {
        Some(r) => scope
            .ctx
            .cross_repo?
            .peer_graph(r)?
            .get(&key)
            .and_then(|t| t.shape.as_ref()),
        None => member_graph.get(&key).and_then(|t| t.shape.as_ref()),
    }
}

/// The graph a brand slot's union members resolve in, and the repo to qualify
/// them to for downstream cross-repo resolution. Own-repo: the local graph, no
/// qualifier. `::repo`: the peer's graph and its repo. `None` when a `::repo`
/// peer is unresolvable — the caller suppresses (the cross-repo gate owns the
/// diagnostic), so the per-value check never false-fires.
fn brand_member_graph<'a, 'n>(
    scope: &Scope<'a>,
    name: &'n QualifiedName,
) -> Option<(&'a TypeGraph, Option<&'n str>)> {
    match &name.repo {
        None => Some((scope.ctx.graph, None)),
        Some(r) => scope
            .ctx
            .cross_repo?
            .peer_graph(r)
            .map(|pg| (pg, Some(r.as_str()))),
    }
}

/// Validate a value at a NOMINAL brand slot (scalar / refined / enum), spec
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
///
/// Three value shapes, one rule: a bare value COERCES to the slot's brand (the
/// slot is the authority, the same as records), an explicit `Name(...)`
/// constructor CHECKS its name against the slot's brand, and a value that starts
/// a `Name(` shape but does not close is `malformed-constructor` (a warning).
/// So nominal safety lands exactly where the author was explicit, never as a
/// barrier to a plain value.
///
/// `brand_name` is the slot's own-repo brand name. A constructor naming any
/// other brand — a different own-repo brand (`second(42)` in a `meter` slot) or
/// a `::repo`-qualified peer brand — is `brand-constructor-mismatch`.
#[allow(clippy::too_many_arguments)]
fn check_nominal_brand_value(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    brand_name: &str,
    brand: &crate::typedef::BrandShape,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
    prefix: &str,
) -> Vec<Diagnostic> {
    // A String brand value (a `Name(...)` constructor, a bare scalar, a nameless
    // paren tuple) is resolved by the elaborator into a `Scalar` / `Tuple` /
    // `MalformedConstructor` node and validated through the value-model walker
    // (`brand_value_via_model`, [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]),
    // so it never reaches here. The only value that does is a NON-String (a Mapping
    // at a nominal brand slot, a shape mismatch), which coerces to the brand's
    // underlying shape below. No constructor is re-parsed.
    let _ = (brand_name, prefix);
    check_value_against_shape(
        scope,
        value,
        value_span,
        &brand.shape,
        field_key,
        origin,
        element_index,
    )
}

/// The reserved-primitive keyword a union member's constructor names, if any.
/// A plain or refined primitive member carries its base keyword (`String`,
/// `Number`, ...), the name its escape constructor uses; every other member
/// (enum, record, nominal brand) has none. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
fn primitive_keyword(shape: &Shape) -> Option<&'static str> {
    match shape {
        Shape::Primitive(p) => Some(p.as_str()),
        Shape::Refined { base, .. } => Some(base.as_str()),
        _ => None,
    }
}

fn tuple_arity_mismatch_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    expected: usize,
    got: usize,
    prefix: &str,
) -> Diagnostic {
    Diagnostic {
        code: codes::TUPLE_ARITY_MISMATCH,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message: format!(
            "{} tuple has {} element(s), but the declared shape has {}",
            prefix, got, expected
        ),
        related: vec![origin.related_span()],
        fix: None,
    }
}

/// Validate a value at a STRUCTURAL (union) brand slot, the unified
/// discrimination model ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
///
/// One rule closes every hole: a NOMINAL-brand member is selectable ONLY via its
/// `Name(...)` constructor, a PRIMITIVE member matches a bare value, a RECORD
/// member discriminates by an inline `type:`. So a bare `42` never silently
/// picks one branch of `<meter | second>`.
///
/// `allow_reference` is true for the `&` slot, where a whole-value wikilink
/// routes to the reference branch. The `*` slot resolves references before
/// reaching here; the bare slot admits no reference.
///
/// `member_graph` is the graph the union's members resolve in — the own graph
/// for an own-repo brand, the PEER graph for a `::repo` brand — and `member_repo`
/// qualifies the members to that peer for downstream cross-repo resolution, so a
/// peer union validates exactly like an own one.
#[allow(clippy::too_many_arguments)]
fn check_structural_brand_value(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    brand_name: &str,
    members: &[Shape],
    member_graph: &TypeGraph,
    member_repo: Option<&str>,
    allow_reference: bool,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
    prefix: &str,
) -> Vec<Diagnostic> {
    let kind = |m: &Shape| classify_member_xrepo(scope, m, member_graph);
    // A record member, qualified to the peer repo (if any) so downstream inline /
    // reference resolution crosses the boundary.
    let qualify = |qn: &QualifiedName| match member_repo {
        Some(r) => qn.qualified_to(r),
        None => qn.clone(),
    };
    let record_names: Vec<QualifiedName> = members
        .iter()
        .filter(|m| kind(m) == UnionMemberKind::Record)
        .filter_map(|m| match m {
            Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) => {
                Some(qualify(qn))
            }
            _ => None,
        })
        .collect();
    match value {
        // An inline record: a RECORD member, discriminated by its `type:`.
        InstanceValue::Mapping(inline) => {
            if record_names.is_empty() {
                return vec![field_shape_mismatch_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} inline value at union brand '{}' has no record member to accept it",
                        prefix, brand_name
                    ),
                )];
            }
            if inline.type_claim.is_none() {
                return vec![brand_constructor_required_diag(
                    scope,
                    value_span,
                    origin,
                    format!(
                        "{} inline value at union brand '{}' needs a `type:` to pick a member",
                        prefix, brand_name
                    ),
                )];
            }
            let branches: Vec<QualBranch> = record_names
                .iter()
                .map(|q| QualBranch {
                    base: q.as_str(),
                    repo: q.repo.as_deref(),
                })
                .collect();
            validate_inline_value(
                scope,
                inline,
                value_span,
                &InlineCompat::Union(branches),
                None,
                field_key,
                origin,
                element_index,
            )
        }
        InstanceValue::String(_) => {
            // Every constructor / bare / paren String brand value is resolved by
            // the elaborator into a `Scalar` / `Tuple` / `MalformedConstructor` node
            // and validated through the value-model walker (`brand_value_via_model`),
            // so it never reaches here. The only String that does is a whole-value
            // wikilink at a `&` union brand, carried as a `Reference` /
            // `MalformedReference` node — route it to the union's record members as a
            // compound reference. No constructor is re-parsed, the value-model
            // invariant ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
            let is_ref_node = matches!(
                scope.model.and_then(|m| m.node(value_span)),
                Some(
                    ContributionValue::Reference { .. } | ContributionValue::MalformedReference(..)
                )
            );
            // A whole-value wikilink at a `&` slot routes to the reference
            // branch (an all-record union, guaranteed by the load check).
            if allow_reference && is_ref_node && !record_names.is_empty() {
                let compound = Shape::CompoundReference {
                    mode: RefMode::Star,
                    op: CompoundRefOp::Union,
                    branches: record_names.clone(),
                };
                return check_value_against_shape(
                    scope,
                    value,
                    value_span,
                    &compound,
                    field_key,
                    origin,
                    element_index,
                );
            }
            check_bare_against_union(
                scope,
                value,
                value_span,
                brand_name,
                members,
                member_graph,
                field_key,
                origin,
                element_index,
                prefix,
            )
        }
        // A bare scalar: coerces to a unique member, a nominal member via its
        // underlying shape; an indistinguishable overlap is `brand-constructor-required`.
        _ => check_bare_against_union(
            scope,
            value,
            value_span,
            brand_name,
            members,
            member_graph,
            field_key,
            origin,
            element_index,
            prefix,
        ),
    }
}

/// The base primitive a bare scalar value belongs to for overlap detection:
/// `String` / `Number` / `Boolean`. A string keeps `String` even when it is
/// ISO-date or URL shaped, since `Date` / `Url` are distinguishing members, not
/// the value's base. A sequence or mapping is not a bare scalar here.
fn base_primitive_of(value: &InstanceValue) -> Option<Primitive> {
    match value {
        InstanceValue::String(_) => Some(Primitive::String),
        InstanceValue::Integer(_) | InstanceValue::Float(_) => Some(Primitive::Number),
        InstanceValue::Boolean(_) => Some(Primitive::Boolean),
        _ => None,
    }
}

/// Whether a union member is a PLAIN, non-distinguishing branch over `base`: a
/// bare `Primitive(base)`, or a nominal brand whose shape is `Primitive(base)`.
/// A refined primitive, an enum, `Date` / `DateTime` / `Url`, and a record are
/// distinguishing, so they never count. Two or more plain members over one base
/// make a bare value ambiguous, [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
fn member_is_plain_over(
    scope: &Scope,
    member: &Shape,
    base: Primitive,
    member_graph: &TypeGraph,
) -> bool {
    match member {
        Shape::Primitive(p) => *p == base,
        Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) => {
            classify_member_xrepo(scope, member, member_graph) == UnionMemberKind::NominalBrand
                && matches!(
                    resolve_member_brand(scope, qn, member_graph).map(|b| &b.shape),
                    Some(Shape::Primitive(p)) if *p == base
                )
        }
        _ => false,
    }
}

/// A bare value at a union brand slot.
///
/// First, overlap-ambiguity: a bare value whose base primitive is claimed by two
/// or more INDISTINGUISHABLE members (a plain primitive, or a nominal brand over
/// that plain primitive) cannot be assigned to one branch, so the author names it
/// (`brand-constructor-required`). A distinguishing predicate discriminates, so
/// `<String | Url>`, `<String | meter>`, `<Number | Date>` never fire it.
///
/// Otherwise the value coerces to the first member that accepts it, a Bare member
/// directly or a NOMINAL brand member via its underlying shape, so a unique
/// nominal branch coerces (`10` at `<String | meter>` is `meter`). A value no
/// member accepts is a plain shape mismatch.
#[allow(clippy::too_many_arguments)]
fn check_bare_against_union(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    brand_name: &str,
    members: &[Shape],
    member_graph: &TypeGraph,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
    prefix: &str,
) -> Vec<Diagnostic> {
    if let Some(base) = base_primitive_of(value) {
        let plain_over_base = members
            .iter()
            .filter(|m| member_is_plain_over(scope, m, base, member_graph))
            .count();
        if plain_over_base >= 2 {
            return vec![brand_constructor_required_diag(
                scope,
                value_span,
                origin,
                format!(
                    "{} bare value is ambiguous at union brand '{}'; name the branch with a `Name(...)` constructor",
                    prefix, brand_name
                ),
            )];
        }
    }
    // Coerce to the first member that accepts the value by its representation.
    // A Bare member (primitive / refined / enum) is checked directly; a NOMINAL
    // brand member against its underlying shape, so a UNIQUE nominal branch
    // coerces (`10` at `<String | meter>` is `meter`). A RECORD member needs an
    // inline `type:`, so a bare value never picks it. The overlap guard above has
    // already rejected the indistinguishable ≥2 case, so a coercion here is
    // unambiguous.
    for member in members {
        let shape: &Shape = match member {
            Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) => {
                match classify_member_xrepo(scope, member, member_graph) {
                    UnionMemberKind::NominalBrand => {
                        match resolve_member_brand(scope, qn, member_graph) {
                            Some(b) => &b.shape,
                            None => continue,
                        }
                    }
                    _ => continue,
                }
            }
            _ => member,
        };
        if check_value_against_shape(
            scope,
            value,
            value_span,
            shape,
            field_key,
            origin,
            element_index,
        )
        .is_empty()
        {
            return Vec::new();
        }
    }
    vec![field_shape_mismatch_diag(
        scope,
        value_span,
        origin,
        format!(
            "{} value matches no member of union brand '{}'",
            prefix, brand_name
        ),
    )]
}

/// Resolve a reference (`*` / the ref branch of `&`) at a STRUCTURAL union brand
/// slot: the target's closure must include one of the union's RECORD members.
/// Builds the equivalent `<r1 | r2 | ...>*` compound over the record members and
/// reuses the compound-reference machinery. If the union has no record member it
/// is not referenceable — the load check owns `brand-not-referenceable`, so the
/// per-value error is suppressed (the type is broken, not the data).
#[allow(clippy::too_many_arguments)]
fn check_structural_brand_reference(
    scope: &Scope,
    value: &InstanceValue,
    value_span: ByteRange,
    _brand_name: &str,
    members: &[Shape],
    member_graph: &TypeGraph,
    member_repo: Option<&str>,
    field_key: &str,
    origin: &Origin,
    element_index: Option<usize>,
) -> Vec<Diagnostic> {
    let record_names: Vec<QualifiedName> = members
        .iter()
        .filter(|m| classify_member_xrepo(scope, m, member_graph) == UnionMemberKind::Record)
        .filter_map(|m| match m {
            Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) => {
                Some(match member_repo {
                    Some(r) => qn.qualified_to(r),
                    None => qn.clone(),
                })
            }
            _ => None,
        })
        .collect();
    if record_names.is_empty() {
        return Vec::new();
    }
    let compound = Shape::CompoundReference {
        mode: RefMode::Star,
        op: CompoundRefOp::Union,
        branches: record_names,
    };
    check_value_against_shape(
        scope,
        value,
        value_span,
        &compound,
        field_key,
        origin,
        element_index,
    )
}

fn brand_constructor_mismatch_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    message: String,
) -> Diagnostic {
    Diagnostic {
        code: codes::BRAND_CONSTRUCTOR_MISMATCH,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message,
        related: vec![origin.related_span()],
        fix: None,
    }
}

fn brand_constructor_required_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    message: String,
) -> Diagnostic {
    Diagnostic {
        code: codes::BRAND_CONSTRUCTOR_REQUIRED,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message,
        related: vec![origin.related_span()],
        fix: None,
    }
}

fn field_shape_mismatch_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    message: String,
) -> Diagnostic {
    Diagnostic {
        code: codes::FIELD_SHAPE_MISMATCH,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message,
        related: vec![origin.related_span()],
        fix: None,
    }
}

/// Map a malformed-wikilink parse error onto a precise diagnostic. The
/// caller pre-filters `NotAWikilink` / `EmptyInner` (those fall back to
/// the generic field-shape-mismatch) — every variant reaching here has a
/// dedicated `wikilink-*` code.
fn wikilink_parse_diag(
    scope: &Scope,
    value_span: ByteRange,
    origin: &Origin,
    prefix: &str,
    raw: &str,
    err: &WikilinkParseError,
) -> Diagnostic {
    let (code, detail) = match err {
        WikilinkParseError::EmptyTarget => (
            au_references::codes::WIKILINK_EMPTY_TARGET,
            "wikilink has no target before its field delimiter",
        ),
        WikilinkParseError::EmptyAnchor => (
            au_references::codes::WIKILINK_EMPTY_ANCHOR,
            "wikilink ends with `#` but no anchor value",
        ),
        WikilinkParseError::EmptyBlockId => (
            au_references::codes::WIKILINK_EMPTY_BLOCK_ID,
            "wikilink ends with `^` but no block-id value",
        ),
        WikilinkParseError::ReversedDelimiters => (
            au_references::codes::WIKILINK_REVERSED_DELIMITERS,
            "wikilink has `^` before `#`; canonical order is `target[#anchor][^block_id][:field]`",
        ),
        WikilinkParseError::EmptyField => (
            au_references::codes::WIKILINK_EMPTY_FIELD,
            "wikilink ends with `:` but no field value",
        ),
        WikilinkParseError::FieldOutOfOrder => (
            au_references::codes::WIKILINK_FRAGMENT_ORDER,
            "wikilink `:field` fragment placed before `#anchor` or `^block-id`; canonical order is `target[#anchor][^block_id][:field]`",
        ),
        WikilinkParseError::InvalidFieldName => (
            au_references::codes::WIKILINK_INVALID_FIELD_NAME,
            "wikilink `:field` value violates the field-name regex",
        ),
        WikilinkParseError::EmptyRepo => (
            au_references::codes::WIKILINK_EMPTY_REPO,
            "wikilink ends with `::` but no repo value",
        ),
        WikilinkParseError::EmptyCommit => (
            au_references::codes::WIKILINK_EMPTY_COMMIT,
            "wikilink has `@` but no commit value",
        ),
        WikilinkParseError::CommitNotOid => (
            au_references::codes::PINNED_COMMIT_NOT_OID,
            "wikilink pins a non-oid commit; a pin's commit must be an immutable oid, not a branch, tag, or relative rev",
        ),
        WikilinkParseError::InvalidRepoName => (
            au_references::codes::WIKILINK_INVALID_REPO_NAME,
            "wikilink `::repo` value violates the repo-name regex",
        ),
        WikilinkParseError::RepoOutOfOrder => (
            au_references::codes::WIKILINK_FRAGMENT_ORDER,
            "wikilink `::repo` qualifier out of order; canonical order is `name ::repo #anchor ^block-id :field`, at most one `::repo`",
        ),
        // Caller filters these before reaching this helper.
        WikilinkParseError::NotAWikilink | WikilinkParseError::EmptyInner => (
            au_references::codes::WIKILINK_EMPTY_TARGET,
            "internal: wikilink parser fell through to malformed path with non-malformed variant",
        ),
    };
    Diagnostic {
        code,
        severity: Severity::Error,
        span: Span::new(scope.instance_path.to_path_buf(), value_span),
        message: format!("{prefix} value '{raw}': {detail}"),
        related: vec![origin.related_span()],
        fix: None,
    }
}

/// Render a divergent field's bare use as a `mixin-collision` at the claim site.
/// Each origin's shape decl gets a related span; the message lists every origin
/// (in authored `OriginId` form, so a peer reads as `note::base`) plus its raw
/// shape, so a reader sees the divergence without opening each type-def file.
pub(crate) fn mixin_collision_diag(
    instance_path: &Path,
    claim_span: ByteRange,
    field_name: &FieldName,
    fo: &FieldOrigin,
) -> Diagnostic {
    let origin_summary = fo
        .origins()
        .map(|(id, info)| format!("'{}' ({})", id.as_str(), info.decl.raw_shape))
        .collect::<Vec<_>>()
        .join(", ");
    Diagnostic {
        code: codes::MIXIN_COLLISION,
        severity: Severity::Error,
        span: Span::new(instance_path.to_path_buf(), claim_span),
        message: format!(
            "field '{}' is declared with non-token-equal shapes across mixin origins: {} — a bare use is ambiguous; qualify each use as `{}{{type}}`",
            field_name.as_str(),
            origin_summary,
            field_name.as_str()
        ),
        related: fo
            .origins()
            .map(|(_, info)| Span::new(info.origin_path.clone(), info.decl.shape_span))
            .collect(),
        fix: None,
    }
}

/// Span covering the `type:` claim value (the right-hand side, not the key).
fn claim_span(claim: &TypeClaim) -> au_diagnostics::ByteRange {
    match claim {
        TypeClaim::Bare(c) => c.span,
        TypeClaim::List { value_span, .. } => *value_span,
    }
}

/// Multi-leaf-in-sealed-family rule ([[type-instance type::au-type-system]]). For each non-sealed
/// claim, walk its sealed-ancestor chain and bucket the claim under each
/// sealed ancestor it descends from. A sealed family is a discriminated
/// union — two distinct leaves of the same sealed parent in one claim
/// list are a validation error.
///
/// Implements spec [[type-instance type::au-type-system]]'s "Diagnostic emission discipline" paragraph:
/// innermost-only suppression on identical leaf sets. When nested sums
/// overlap and an inner sealed parent S' has the same offending leaf
/// set as an outer ancestor S, only S' fires. When the outer's leaf set
/// is a strict superset, both fire — each describes a distinct
/// violation level.
///
/// Sealed claim elements themselves are skipped here (already covered by
/// `sealed-parent-claimed` upstream). Unknown-type claims are skipped
/// (covered by `unknown-type-claim` upstream).
///
/// Called from `validate` (file-level claim) and `validate_inline_value`
/// (inline-value identity claim) symmetrically.
pub(crate) fn check_multi_leaf_in_sealed_family(
    graph: &TypeGraph,
    resolution: Option<&crate::resolution::ResolutionGraph>,
    path: &Path,
    type_claim: &TypeClaim,
) -> Vec<Diagnostic> {
    // An importing instance resolves every claim (own + peer) to a folded
    // `TypeId`, so bucket over the resolution graph, which subsumes the own-graph
    // check (every own def is a fold seed); a single-repo instance (no resolution)
    // uses the own graph. This closes finding 2.3: a `type: [k.a::base, k.b::base]`
    // claiming two leaves of a PEER sealed family was silently skipped.
    match resolution {
        Some(rg) => check_multi_leaf_folded(rg, path, type_claim),
        None => check_multi_leaf_own(graph, path, type_claim),
    }
}

/// The single-repo (own-graph) multi-leaf-in-sealed-family check, keyed by
/// `TypeName`. Used when the instance's repo does not import.
fn check_multi_leaf_own(graph: &TypeGraph, path: &Path, type_claim: &TypeClaim) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // sealed_ancestor → set of non-sealed claim leaves bucketed under it.
    let mut buckets: BTreeMap<TypeName, BTreeSet<TypeName>> = BTreeMap::new();
    // First-seen span per claim name (for related diagnostics).
    let mut claim_spans: BTreeMap<TypeName, ByteRange> = BTreeMap::new();

    for claim_el in type_claim.iter() {
        if claim_el.is_qualified() {
            // A `::repo` peer claim does not participate in this graph's
            // sealed-family bucketing; the fold resolves it.
            continue;
        }
        if graph.is_sealed(&claim_el.name) {
            // Sealed claim — separate diagnostic handles it.
            continue;
        }
        if !graph.contains(&claim_el.name) {
            // Unknown — separate diagnostic handles it.
            continue;
        }
        claim_spans
            .entry(claim_el.name.clone())
            .or_insert(claim_el.span);
        // Walk closure; bucket under each sealed ancestor (excluding self,
        // which we already know is non-sealed).
        for ancestor in closure_of(graph, &claim_el.name) {
            if ancestor == claim_el.name {
                continue;
            }
            if graph.is_sealed(&ancestor) {
                buckets
                    .entry(ancestor)
                    .or_default()
                    .insert(claim_el.name.clone());
            }
        }
    }

    // Collect candidate violations (buckets with ≥2 distinct leaves).
    let violated: Vec<(TypeName, BTreeSet<TypeName>)> = buckets
        .into_iter()
        .filter(|(_, leaves)| leaves.len() >= 2)
        .collect();

    // Innermost-only suppression: a violation at outer sealed S is
    // suppressed if there exists a violation at inner sealed S' where
    // S' descends from S AND the leaf sets are identical.
    //
    // Pre-compute closures once per violated sealed parent so the inner
    // loop is O(1) lookups instead of O(graph-walk). K (violated count)
    // is bounded by sealed-nesting depth in practice — typically ≤ 3 —
    // but the K² pairing makes the memo worthwhile regardless.
    let closures: BTreeMap<TypeName, BTreeSet<TypeName>> = violated
        .iter()
        .map(|(s, _)| (s.clone(), closure_of(graph, s)))
        .collect();
    let mut suppressed: BTreeSet<TypeName> = BTreeSet::new();
    for (s, leaves) in &violated {
        for (other_s, other_leaves) in &violated {
            if other_s == s {
                continue;
            }
            if leaves != other_leaves {
                continue;
            }
            // other_s strictly descends from s if s is in other_s's closure
            // and other_s != s (checked above).
            if closures[other_s].contains(s) {
                suppressed.insert(s.clone());
                break;
            }
        }
    }

    for (sealed_parent, leaves) in &violated {
        if suppressed.contains(sealed_parent) {
            continue;
        }
        let leaf_names: Vec<String> = leaves.iter().map(|n| format!("'{}'", n.as_str())).collect();
        let related: Vec<Span> = leaves
            .iter()
            .filter_map(|name| {
                claim_spans
                    .get(name)
                    .map(|sp| Span::new(path.to_path_buf(), *sp))
            })
            .collect();
        diags.push(Diagnostic {
            code: codes::MULTI_LEAF_IN_SEALED_FAMILY,
            severity: Severity::Error,
            span: Span::new(path.to_path_buf(), claim_span(type_claim)),
            message: format!(
                "instance claims multiple distinct leaves of sealed family '{}': {} — a sealed family is a discriminated union",
                sealed_parent.as_str(),
                leaf_names.join(", ")
            ),
            related,
            fix: None,
        });
    }

    diags
}

/// The cross-repo sibling of [`check_multi_leaf_own`]: bucket claim leaves under
/// their sealed ancestors over the FOLDED resolution graph, keyed by `TypeId`, so
/// two leaves of a PEER sealed family (`type: [k.a::base, k.b::base]`) are caught
/// at single-repo parity. Own claims fold to the same ids (every own def is a fold
/// seed), so this subsumes the own-graph check for an importing instance.
fn check_multi_leaf_folded(
    rg: &crate::resolution::ResolutionGraph,
    path: &Path,
    type_claim: &TypeClaim,
) -> Vec<Diagnostic> {
    use crate::resolution::TypeId;
    let mut diags = Vec::new();

    // sealed-ancestor TypeId → set of non-sealed leaf TypeIds bucketed under it.
    let mut buckets: BTreeMap<TypeId, BTreeSet<TypeId>> = BTreeMap::new();
    let mut leaf_spans: BTreeMap<TypeId, ByteRange> = BTreeMap::new();

    for claim_el in type_claim.iter() {
        let Some(leaf_id) = rg.resolve_authored(&claim_el.name, claim_el.repo.as_deref()) else {
            continue; // unresolvable — the crosstype gate owns it
        };
        let Some(node) = rg.get(leaf_id) else {
            continue;
        };
        if !node.sealed.is_empty() {
            continue; // a sealed claim — sealed-parent-claimed handles it
        }
        leaf_spans.entry(leaf_id.clone()).or_insert(claim_el.span);
        // Bucket under each sealed ancestor (excluding self, known non-sealed).
        for ancestor in folded_parent_closure(rg, leaf_id) {
            if &ancestor == leaf_id {
                continue;
            }
            if rg.get(&ancestor).is_some_and(|a| !a.sealed.is_empty()) {
                buckets.entry(ancestor).or_default().insert(leaf_id.clone());
            }
        }
    }

    let violated: Vec<(TypeId, BTreeSet<TypeId>)> = buckets
        .into_iter()
        .filter(|(_, leaves)| leaves.len() >= 2)
        .collect();

    // Innermost-only suppression, mirroring the own path over `TypeId`s.
    let closures: BTreeMap<TypeId, BTreeSet<TypeId>> = violated
        .iter()
        .map(|(s, _)| (s.clone(), folded_parent_closure(rg, s)))
        .collect();
    let mut suppressed: BTreeSet<TypeId> = BTreeSet::new();
    for (s, leaves) in &violated {
        for (other_s, other_leaves) in &violated {
            if other_s == s || leaves != other_leaves {
                continue;
            }
            if closures[other_s].contains(s) {
                suppressed.insert(s.clone());
                break;
            }
        }
    }

    for (sealed_parent, leaves) in &violated {
        if suppressed.contains(sealed_parent) {
            continue;
        }
        let leaf_names: Vec<String> = leaves
            .iter()
            .map(|id| format!("'{}'", folded_authored(rg, id)))
            .collect();
        let related: Vec<Span> = leaves
            .iter()
            .filter_map(|id| {
                leaf_spans
                    .get(id)
                    .map(|sp| Span::new(path.to_path_buf(), *sp))
            })
            .collect();
        diags.push(Diagnostic {
            code: codes::MULTI_LEAF_IN_SEALED_FAMILY,
            severity: Severity::Error,
            span: Span::new(path.to_path_buf(), claim_span(type_claim)),
            message: format!(
                "instance claims multiple distinct leaves of sealed family '{}': {} — a sealed family is a discriminated union",
                folded_authored(rg, sealed_parent),
                leaf_names.join(", ")
            ),
            related,
            fix: None,
        });
    }

    diags
}

/// The transitive parent-`TypeId` closure of `start` over the resolution graph,
/// including `start`. Cycle-safe via the visited set.
fn folded_parent_closure(
    rg: &crate::resolution::ResolutionGraph,
    start: &crate::resolution::TypeId,
) -> BTreeSet<crate::resolution::TypeId> {
    let mut out = BTreeSet::new();
    let mut stack = vec![start.clone()];
    while let Some(id) = stack.pop() {
        if !out.insert(id.clone()) {
            continue;
        }
        if let Some(node) = rg.get(&id) {
            for p in &node.parents {
                stack.push(p.clone());
            }
        }
    }
    out
}

/// A folded node's authored name for a diagnostic message: `name::origin` for a
/// peer node, bare `name` for an own node.
fn folded_authored(
    rg: &crate::resolution::ResolutionGraph,
    id: &crate::resolution::TypeId,
) -> String {
    match rg.get(id).and_then(|n| n.origin.as_deref()) {
        Some(repo) => format!("{}::{}", id.name.as_str(), repo),
        None => id.name.as_str().to_string(),
    }
}

/// Every origin in a peer qualifier's folded closure that declares `field`, in
/// closure order. Mirrors the own-graph gather in [`resolve_qualifier`] so the
/// cross-repo path detects an ambiguous divergent reach identically. Each
/// candidate's `origin` is the authored `OriginId` (`name` for an own-graph
/// ancestor of the peer, `name::repo` for a peer-owned one).
fn gather_qualifier_candidates_resolved(
    rg: &crate::resolution::ResolutionGraph,
    base: &TypeName,
    repo: &str,
    field: &FieldName,
) -> Vec<crate::closure::QualifierCandidate> {
    let Some(qualifier_tid) = rg.resolve_authored(base, Some(repo)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ancestor in folded_parent_closure(rg, qualifier_tid) {
        if let Some(node) = rg.get(&ancestor) {
            if let Some(decl) = node.fields.iter().find(|fld| &fld.name == field) {
                let origin = match &node.origin {
                    Some(r) => OriginId(format!("{}::{}", node.id.name.as_str(), r)),
                    None => OriginId(node.id.name.as_str().to_string()),
                };
                out.push(crate::closure::QualifierCandidate {
                    origin,
                    path: node.source_path.clone(),
                    decl: decl.clone(),
                });
            }
        }
    }
    out
}

pub(crate) fn value_matches_simple_shape(value: &InstanceValue, shape: &Shape) -> bool {
    match shape {
        Shape::Primitive(p) => match (p, value) {
            (Primitive::String, InstanceValue::String(_)) => true,
            (Primitive::Number, InstanceValue::Integer(_)) => true,
            // `.nan` / `.inf` parse as floats but are not valid numbers.
            // Rejected here; the field check emits a dedicated diagnostic.
            (Primitive::Number, InstanceValue::Float(f)) => f.is_finite(),
            (Primitive::Boolean, InstanceValue::Boolean(_)) => true,
            (Primitive::Date, InstanceValue::String(s)) => is_iso_date(s),
            (Primitive::DateTime, InstanceValue::String(s)) => is_iso_datetime(s),
            (Primitive::Url, InstanceValue::String(s)) => is_http_url(s),
            _ => false,
        },
        Shape::Enum(literals) => match value {
            InstanceValue::String(s) => literals.iter().any(|l| l == s),
            _ => false,
        },
        // The no-type slot ([[type-def shape any::au-type-system]]) and the uninterpreted
        // ([[type-def shape opaque::au-type-system]]) slot accept every value.
        Shape::Any | Shape::Opaque => true,
        // A refined scalar matches when its base primitive matches; the
        // predicate meet is enforced by the range-aware validation pass.
        Shape::Refined { base, .. } => value_matches_simple_shape(value, &Shape::Primitive(*base)),
        // Reference / Record / InlineOrReference / List / Union /
        // Intersection / CompoundReference have their own check paths.
        // Kept exhaustive so a new Shape variant becomes a compile error.
        Shape::Reference(_)
        | Shape::Record(_)
        | Shape::InlineOrReference(_)
        | Shape::List { .. }
        | Shape::Union(_)
        | Shape::Intersection(_)
        | Shape::CompoundReference { .. }
        | Shape::DefReference(_)
        | Shape::Tuple(_)
        | Shape::Pinned(_) => false,
    }
}

/// Parse a run of ASCII digits into its value, or `None` if any byte is not a
/// digit (or the slice is empty). Used to read fixed-width date/time fields.
fn parse_ascii_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: u32 = 0;
    for b in bytes {
        value = value * 10 + u32::from(b - b'0');
    }
    Some(value)
}

/// A real calendar date: month `1..=12`, day `1..=last-of-month`, with February
/// honouring the proleptic-Gregorian leap rule. Rejects `2026-02-30`,
/// `2026-04-31`, and the like.
fn is_valid_ymd(year: u32, month: u32, day: u32) -> bool {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let last_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=last_day).contains(&day)
}

/// Strict ISO 8601 calendar date: `YYYY-MM-DD`. Positions, separators, AND
/// field ranges, so an impossible date like `2026-13-40` is rejected, not just
/// a malformed one.
pub(crate) fn is_iso_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (
        parse_ascii_digits(&bytes[0..4]),
        parse_ascii_digits(&bytes[5..7]),
        parse_ascii_digits(&bytes[8..10]),
    ) else {
        return false;
    };
    is_valid_ymd(year, month, day)
}

/// The engine's canonical `DateTime`, UTC-only and colon-free, one strict form:
///   `YYYY-MM-DDThhmmssZ`
///
/// The date is `YYYY-MM-DD`, then `T`, then a six-digit `hhmmss` time with no
/// separators, then a mandatory `Z`. Always UTC, no timezone offsets, no
/// fractional seconds, no optional precision. Exactly 18 bytes.
pub(crate) fn is_iso_datetime(s: &str) -> bool {
    let bytes = s.as_bytes();
    // YYYY-MM-DD (10) + T (1) + hhmmss (6) + Z (1) = 18.
    if bytes.len() != 18 {
        return false;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[17] != b'Z' {
        return false;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        parse_ascii_digits(&bytes[0..4]),
        parse_ascii_digits(&bytes[5..7]),
        parse_ascii_digits(&bytes[8..10]),
        parse_ascii_digits(&bytes[11..13]),
        parse_ascii_digits(&bytes[13..15]),
        parse_ascii_digits(&bytes[15..17]),
    ) else {
        return false;
    };
    // Field ranges, not just digit positions: an impossible time like
    // `T999999` is rejected.
    is_valid_ymd(year, month, day) && hour <= 23 && minute <= 59 && second <= 59
}

/// [[type reference::au-type-system]] disambiguator predicate: does the string carry the `^https?://`
/// prefix? Used by `<String | Url>`-style union routing to commit the
/// value to the Url branch before the String branch's trivial any-of
/// match would silently accept it. Syntactically disjoint from
/// `looks_like_wikilink` — a value cannot match both.
fn has_http_url_prefix(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// HTTP/HTTPS URL per spec [[type-def shape primitive::au-type-system]]. Starts with `http://` or `https://`,
/// non-empty remainder, ASCII-only, no spaces or control characters. The
/// `^https?://` prefix is the disambiguator ([[type-def shape primitive::au-type-system]] / [[type reference::au-type-system]]); RFC 3986's
/// full authority + path grammar isn't enforced.
fn is_http_url(s: &str) -> bool {
    let rest = if let Some(r) = s.strip_prefix("https://") {
        r
    } else if let Some(r) = s.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    rest.chars()
        .all(|c| c.is_ascii() && c != ' ' && !c.is_ascii_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::instance::{Instance, InstanceField, InstanceValue, SequenceElement, TypeClaim};
    use crate::typedef::{BrandShape, FieldDecl, FieldName, TypeDef, TypeName, TypeNameClaim};
    use au_diagnostics::ByteRange;
    use au_grammar::{DefBound, Primitive, Shape};
    use std::path::PathBuf;

    fn td(name: &str, parents: &[&str], fields: &[(&str, bool, Result<Shape, &str>)]) -> TypeDef {
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
                .map(|(fname, optional, parsed)| FieldDecl {
                    name: FieldName((*fname).into()),
                    optional: *optional,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: parsed.clone().map_err(|raw| Diagnostic {
                        code: au_grammar::NOT_YET_IMPLEMENTED_SHAPE_FEATURE,
                        severity: Severity::Error,
                        span: Span::new(PathBuf::from("/v/x.type.yaml"), ByteRange::new(0, 0)),
                        message: format!("shape '{}' not yet implemented", raw),
                        related: vec![],
                        fix: None,
                    }),
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

    fn prim(p: Primitive) -> Result<Shape, &'static str> {
        Ok(Shape::Primitive(p))
    }

    fn sh(s: &str) -> Result<Shape, &'static str> {
        Ok(au_grammar::parse_shape(s).expect("valid shape in test"))
    }

    fn num_codes(g: &TypeGraph, key: &str, v: InstanceValue) -> Vec<String> {
        let diags = validate_simple(g, &instance(bare("rec"), vec![(key, v)]));
        codes_of(&diags).iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn refinement_numeric_bounds_are_enforced() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("n", false, sh("Number{>=0 & <=10}"))],
        )])
        .graph;
        assert!(num_codes(&g, "n", InstanceValue::Integer(0)).is_empty());
        assert!(num_codes(&g, "n", InstanceValue::Integer(10)).is_empty());
        assert_eq!(
            num_codes(&g, "n", InstanceValue::Integer(-1)),
            ["value-out-of-refinement"]
        );
        assert_eq!(
            num_codes(&g, "n", InstanceValue::Integer(11)),
            ["value-out-of-refinement"]
        );
    }

    #[test]
    fn refinement_integer_and_strict_bounds_are_enforced() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("n", false, sh("Number{>0 & integer}"))],
        )])
        .graph;
        assert!(num_codes(&g, "n", InstanceValue::Integer(2)).is_empty());
        assert!(num_codes(&g, "n", InstanceValue::Float(2.0)).is_empty()); // whole float is an integer
        assert_eq!(
            num_codes(&g, "n", InstanceValue::Integer(0)),
            ["value-out-of-refinement"]
        ); // >0
        assert_eq!(
            num_codes(&g, "n", InstanceValue::Float(1.5)),
            ["value-out-of-refinement"]
        ); // not int
    }

    #[test]
    fn refinement_base_mismatch_is_field_shape_mismatch_not_out_of_refinement() {
        let g = build_graph(vec![td("rec", &[], &[("n", false, sh("Number{>=0}"))])]).graph;
        assert_eq!(
            num_codes(&g, "n", InstanceValue::String("x".into())),
            ["field-shape-mismatch"]
        );
    }

    #[test]
    fn refinement_date_bound_is_enforced() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("d", false, sh("Date{>=2020-01-01}"))],
        )])
        .graph;
        assert!(num_codes(&g, "d", InstanceValue::String("2020-06-01".into())).is_empty());
        assert_eq!(
            num_codes(&g, "d", InstanceValue::String("2019-12-31".into())),
            ["value-out-of-refinement"]
        );
    }

    fn refn(s: &str) -> au_grammar::Refinement {
        match au_grammar::parse_shape(s).unwrap() {
            Shape::Refined { refinement, .. } => refinement,
            _ => panic!("not a refined shape: {s}"),
        }
    }

    #[test]
    fn refinement_region_subset_numeric() {
        let sub = |a: &str, b: &str| {
            refinement_region_subset(Primitive::Number, Some(&refn(a)), Some(&refn(b)))
        };
        assert!(sub("Number{>=5}", "Number{>=0}")); // tighter lower ⊆ looser
        assert!(!sub("Number{>=0}", "Number{>=5}"));
        assert!(sub("Number{>=0 & <=10}", "Number{>=0 & <=100}"));
        assert!(!sub("Number{>=0 & <=100}", "Number{>=0 & <=10}"));
        assert!(sub("Number{>0}", "Number{>=0}")); // strict ⊆ inclusive
        assert!(!sub("Number{>=0}", "Number{>0}")); // inclusive ⊄ strict (0 excluded)
        assert!(sub("Number{>=0 & integer}", "Number{>=0}")); // integers ⊆ all
        assert!(!sub("Number{>=0}", "Number{>=0 & integer}"));
    }

    #[test]
    fn refinement_region_subset_top_string_date() {
        let p = Primitive::Number;
        // ⊤ handling.
        assert!(refinement_region_subset(
            p,
            Some(&refn("Number{>=0}")),
            None
        )); // x ⊆ ⊤
        assert!(!refinement_region_subset(
            p,
            None,
            Some(&refn("Number{>=0}"))
        )); // ⊤ ⊄ x
        assert!(refinement_region_subset(p, None, None)); // ⊤ ⊆ ⊤
                                                          // String pattern equality (conservative).
        let s = Primitive::String;
        assert!(refinement_region_subset(
            s,
            Some(&refn("String{/a/}")),
            Some(&refn("String{/a/}"))
        ));
        assert!(!refinement_region_subset(
            s,
            Some(&refn("String{/a/}")),
            Some(&refn("String{/b/}"))
        ));
        // Date lexical bounds.
        let d = Primitive::Date;
        assert!(refinement_region_subset(
            d,
            Some(&refn("Date{>=2020-06-01}")),
            Some(&refn("Date{>=2020-01-01}"))
        ));
        assert!(!refinement_region_subset(
            d,
            Some(&refn("Date{>=2020-01-01}")),
            Some(&refn("Date{>=2020-06-01}"))
        ));
    }

    #[test]
    fn cardinality_subset_intervals() {
        assert!(cardinality_subset(3, Some(5), 2, Some(6))); // [3..5] ⊆ [2..6]
        assert!(!cardinality_subset(2, Some(6), 3, Some(5)));
        assert!(cardinality_subset(3, None, 2, None)); // [3..] ⊆ [2..]
        assert!(!cardinality_subset(2, None, 3, None)); // [2..] ⊄ [3..]
        assert!(cardinality_subset(3, Some(5), 3, None)); // [3..5] ⊆ [3..]
        assert!(!cardinality_subset(3, None, 3, Some(5))); // [3..] ⊄ [3..5]
        assert!(cardinality_subset(1, None, 0, None)); // [+] ⊆ []
        assert!(!cardinality_subset(0, None, 1, None)); // [] ⊄ [+]
    }

    #[test]
    fn malformed_date_bound_does_not_spuriously_flag_instance() {
        // A bad Date bound is flagged `refinement-bad-shape` at load, but must
        // NOT make an instance value spuriously fail — the fault is the type-def.
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("d", false, sh("Date{>=2020-13-45}"))],
        )])
        .graph;
        let load = crate::load_checks::run_graph_structure_checks(&g);
        assert!(load
            .iter()
            .any(|d| d.code == au_grammar::REFINEMENT_BAD_SHAPE));
        let per = validate_simple(
            &g,
            &instance(
                bare("rec"),
                vec![("d", InstanceValue::String("2019-06-01".into()))],
            ),
        );
        assert!(per.iter().all(|d| d.code != codes::VALUE_OUT_OF_REFINEMENT));
    }

    #[test]
    fn refinement_regex_is_enforced() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("s", false, sh("String{/^[a-z]+$/}"))],
        )])
        .graph;
        assert!(num_codes(&g, "s", InstanceValue::String("abc".into())).is_empty());
        assert_eq!(
            num_codes(&g, "s", InstanceValue::String("ABC".into())),
            ["value-out-of-refinement"]
        );
        assert_eq!(
            num_codes(&g, "s", InstanceValue::String("a1".into())),
            ["value-out-of-refinement"]
        );
    }

    #[test]
    fn refinement_load_errors_fire_bad_shape() {
        // A regex that does not compile.
        let g = build_graph(vec![td("rec", &[], &[("s", false, sh("String{/[/}"))])]).graph;
        let load = crate::load_checks::run_graph_structure_checks(&g);
        assert!(load
            .iter()
            .any(|d| d.code == au_grammar::REFINEMENT_BAD_SHAPE));
        // A lookahead is non-regular, so the engine rejects it too.
        let g2 = build_graph(vec![td("rec", &[], &[("s", false, sh("String{/(?=x)/}"))])]).graph;
        let load2 = crate::load_checks::run_graph_structure_checks(&g2);
        assert!(load2
            .iter()
            .any(|d| d.code == au_grammar::REFINEMENT_BAD_SHAPE));
        // A bad Date bound literal.
        let g3 = build_graph(vec![td(
            "rec",
            &[],
            &[("d", false, sh("Date{>=2020-13-01}"))],
        )])
        .graph;
        let load3 = crate::load_checks::run_graph_structure_checks(&g3);
        assert!(load3
            .iter()
            .any(|d| d.code == au_grammar::REFINEMENT_BAD_SHAPE));
    }

    #[test]
    fn unsatisfiable_refinement_warns_at_load_and_suppresses_per_value() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("n", false, sh("Number{>=5 & <=1}"))],
        )])
        .graph;
        // A load-time warning names the unsatisfiable slot.
        let load = crate::load_checks::run_graph_structure_checks(&g);
        assert!(load
            .iter()
            .any(|d| d.code == codes::REFINEMENT_UNSATISFIABLE));
        // A value against the impossible slot does NOT get per-value spam.
        let per_value = validate_simple(
            &g,
            &instance(bare("rec"), vec![("n", InstanceValue::Integer(3))]),
        );
        assert!(per_value
            .iter()
            .all(|d| d.code != codes::VALUE_OUT_OF_REFINEMENT));
    }

    fn enum_shape(literals: &[&str]) -> Result<Shape, &'static str> {
        Ok(Shape::Enum(
            literals.iter().map(|s| (*s).to_string()).collect(),
        ))
    }

    fn ref_shape(name: &str) -> Result<Shape, &'static str> {
        Ok(Shape::Reference(name.into()))
    }

    /// Bare-name record-slot shape (`name`). Inline-value entry point.
    fn record_shape(name: &str) -> Result<Shape, &'static str> {
        Ok(Shape::Record(name.into()))
    }

    /// Bare-name inline-or-reference shape (`name&`).
    fn inline_or_ref_shape(name: &str) -> Result<Shape, &'static str> {
        Ok(Shape::InlineOrReference(name.into()))
    }

    fn list_shape(inner: Shape) -> Result<Shape, &'static str> {
        Ok(Shape::List {
            inner: Box::new(inner),
            min: 0,
            max: None,
        })
    }

    fn non_empty_list_shape(inner: Shape) -> Result<Shape, &'static str> {
        Ok(Shape::List {
            inner: Box::new(inner),
            min: 1,
            max: None,
        })
    }

    fn instance(claim: TypeClaim, fields: Vec<(&str, InstanceValue)>) -> Instance {
        // Assign DISTINCT byte spans to every field value and every list element,
        // the way a real parsed file does. The instance-surface value model is
        // keyed by span, so all-zero synthetic spans would collide and force the
        // re-parse fallback, hiding the model path from the unit oracle. With
        // distinct spans a reference / brand value verdict runs through the model
        // here exactly as it does over a real file.
        let mut next = 1usize;
        let mut instance_fields = Vec::with_capacity(fields.len());
        for (n, mut v) in fields {
            let value_span = ByteRange::new(next, next + 1);
            next += 2;
            if let InstanceValue::Sequence(elems) = &mut v {
                for e in elems.iter_mut() {
                    e.span = ByteRange::new(next, next + 1);
                    next += 2;
                }
            }
            instance_fields.push(InstanceField {
                key: n.into(),
                key_span: ByteRange::new(0, 0),
                value: v,
                value_span,
                nav_links: Vec::new(),
            });
        }
        Instance {
            source_path: PathBuf::from("/v/inst.md"),
            source_span: ByteRange::new(0, 0),
            type_claim: claim,
            fields: instance_fields,
            doc: None,
            field_docs: Default::default(),
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

    fn codes_of(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_str()).collect()
    }

    /// Test helper: build a `ValidateContext` from the three borrowed
    /// pieces and call `validate`. Lets tests stay readable —
    /// `validate_with(&g, &idx, &claims, &inst)` reads close to the old
    /// 4-arg `validate` call without bypassing the public API.
    fn validate_with(
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
            resolution: None,
            meta_marker: None,
        };
        validate(&ctx, instance)
    }

    /// `validate_with` plus block-id target data, for `[[file^id]]` /
    /// `[[^id]]` resolution tests ([[type block-id::au-type-system]]).
    fn validate_with_targets(
        graph: &TypeGraph,
        idx: &RepoIndex,
        claims: &BTreeMap<PathBuf, Vec<TypeName>>,
        body_sources: &BTreeMap<PathBuf, String>,
        record_targets: &BTreeMap<PathBuf, crate::record_targets::RecordTargets>,
        instance: &Instance,
    ) -> Vec<Diagnostic> {
        let ref_data = MapRefData {
            claims_by_path: claims,
            body_sources,
            record_targets,
        };
        let ctx = ValidateContext {
            graph,
            repo_index: idx,
            ref_data: &ref_data,
            cross_repo: None,
            resolution: None,
            meta_marker: None,
        };
        validate(&ctx, instance)
    }

    /// Test helper for primitive/enum/list-of-primitive cases that don't
    /// need reference resolution — empty `RepoIndex` + empty `claims`.
    fn validate_simple(graph: &TypeGraph, instance: &Instance) -> Vec<Diagnostic> {
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
        let claims = BTreeMap::new();
        validate_with(graph, &idx, &claims, instance)
    }

    fn enum_brand(name: &str, members: &[&str]) -> TypeDef {
        let mut td_ = td(name, &[], &[]);
        td_.shape = Some(BrandShape {
            shape: Shape::Enum(members.iter().map(|m| (*m).to_string()).collect()),
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        });
        td_
    }

    #[test]
    fn a_bare_member_validates_against_an_enum_brand_slot() {
        let g = build_graph(vec![
            enum_brand("ir", &["save", "delete"]),
            td("host", &[], &[("icon", false, record_shape("ir"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("icon", InstanceValue::String("save".into()))],
        );
        let diags = validate_simple(&g, &inst);
        assert!(diags.is_empty(), "{:?}", codes_of(&diags));
    }

    #[test]
    fn a_non_member_fails_an_enum_brand_slot() {
        let g = build_graph(vec![
            enum_brand("ir", &["save", "delete"]),
            td("host", &[], &[("icon", false, record_shape("ir"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("icon", InstanceValue::String("nope".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    fn scalar_brand(name: &str, base: Primitive) -> TypeDef {
        let mut td_ = td(name, &[], &[]);
        td_.shape = Some(BrandShape {
            shape: Shape::Primitive(base),
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        });
        td_
    }

    /// A graph with a `meter` and a `second` scalar brand (both `Number`) plus a
    /// `host` record whose `length` slot demands `meter`.
    fn meter_host_graph() -> TypeGraph {
        build_graph(vec![
            scalar_brand("meter", Primitive::Number),
            scalar_brand("second", Primitive::Number),
            td("host", &[], &[("length", false, record_shape("meter"))]),
        ])
        .graph
    }

    #[test]
    fn a_bare_value_coerces_to_a_scalar_brand_slot() {
        // `length: 42` — the slot is the authority, the same as records.
        let g = meter_host_graph();
        let inst = instance(bare("host"), vec![("length", InstanceValue::Integer(42))]);
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_matching_constructor_is_accepted_at_a_scalar_brand_slot() {
        // `length: meter(42)` — the explicit form, identical result to the bare 42.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String("meter(42)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_foreign_constructor_is_a_brand_constructor_mismatch() {
        // `length: second(42)` names a foreign brand — a mismatch, though both
        // are `Number`. Nominal safety lands where the author was explicit.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String("second(42)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-mismatch"));
    }

    #[test]
    fn a_reserved_primitive_constructor_is_a_mismatch_at_a_single_brand_slot() {
        // `length: Number(5)` at a `meter` slot — a reserved primitive is admitted
        // ONLY as a union member, never at a single brand slot. "not-meter Number"
        // is meaningless here, so it is a mismatch, not a coercion.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String("Number(5)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-mismatch"));
    }

    #[test]
    fn a_wrong_typed_value_fails_a_scalar_brand_slot() {
        // A bare `"x"` coerces to the brand's `Number` shape and fails there.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String("x".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    #[test]
    fn a_constructor_matching_the_brand_but_wrong_arg_fails_the_shape() {
        // `meter("x")` matches the brand but its inner value is not a Number.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String(r#"meter("x")"#.into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    #[test]
    fn a_malformed_constructor_warns_at_a_scalar_brand_slot() {
        // `meter(42` starts a constructor but does not close — a warning, and the
        // one actionable diagnostic, not a secondary shape mismatch.
        let g = meter_host_graph();
        let inst = instance(
            bare("host"),
            vec![("length", InstanceValue::String("meter(42".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let cs = codes_of(&diags);
        assert!(cs.contains(&"malformed-constructor"), "{cs:?}");
        assert!(
            !cs.contains(&"field-shape-mismatch"),
            "no secondary mismatch: {cs:?}"
        );
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() != "malformed-constructor"
                    || d.severity == Severity::Warning),
            "malformed-constructor is a warning",
        );
    }

    // ----- structural (union) brand discrimination -----

    fn union_brand(name: &str, members: &[Shape]) -> TypeDef {
        let mut td_ = td(name, &[], &[]);
        td_.shape = Some(BrandShape {
            shape: Shape::Union(members.to_vec()),
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        });
        td_
    }

    fn rec_member(name: &str) -> Shape {
        Shape::Record(name.into())
    }

    /// A graph with a `paper` / `observation` record pair and an `evidence-kind`
    /// union brand over them, plus a `host` record whose slot uses the union.
    fn evidence_kind_graph(slot: Result<Shape, &'static str>) -> TypeGraph {
        build_graph(vec![
            td("paper", &[], &[("t", false, prim(Primitive::String))]),
            td("observation", &[], &[("t", false, prim(Primitive::String))]),
            union_brand(
                "evidence-kind",
                &[rec_member("paper"), rec_member("observation")],
            ),
            td("host", &[], &[("e", false, slot)]),
        ])
        .graph
    }

    #[test]
    fn an_inline_record_discriminates_a_records_union_brand() {
        let g = evidence_kind_graph(record_shape("evidence-kind"));
        let inst = instance(
            bare("host"),
            vec![(
                "e",
                inline(
                    Some(bare("paper")),
                    vec![("t", InstanceValue::String("x".into()))],
                ),
            )],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_typeless_inline_at_a_union_brand_fires_brand_constructor_required() {
        let g = evidence_kind_graph(record_shape("evidence-kind"));
        let inst = instance(
            bare("host"),
            vec![(
                "e",
                inline(None, vec![("t", InstanceValue::String("x".into()))]),
            )],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-required"));
    }

    #[test]
    fn a_reference_resolves_a_records_union_brand() {
        // `e: evidence-kind*` — a wikilink whose target's closure includes a
        // RECORD member (`paper`) resolves.
        let g = evidence_kind_graph(ref_shape("evidence-kind"));
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), vec![PathBuf::from("/v/paper-a.md")]);
        let mut claims = BTreeMap::new();
        claims.insert(
            PathBuf::from("/v/paper-a.md"),
            vec![TypeName("paper".into())],
        );
        let inst = instance(
            bare("host"),
            vec![("e", InstanceValue::String("[[paper-a]]".into()))],
        );
        assert!(
            validate_with(&g, &idx, &claims, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_with(&g, &idx, &claims, &inst))
        );
    }

    /// A graph with two scalar brands and a union over them — the hole case.
    fn scalar_union_graph() -> TypeGraph {
        build_graph(vec![
            scalar_brand("meter", Primitive::Number),
            scalar_brand("second", Primitive::Number),
            union_brand("quantity", &[rec_member("meter"), rec_member("second")]),
            td("host", &[], &[("v", false, record_shape("quantity"))]),
        ])
        .graph
    }

    #[test]
    fn a_bare_value_at_a_scalar_union_brand_fires_brand_constructor_required() {
        // THE HOLE: `42` at `<meter | second>` cannot pick a branch, and a
        // nominal member is never matched bare — so it never silently becomes
        // `meter`. The author must write `meter(42)` / `second(42)`.
        let g = scalar_union_graph();
        let inst = instance(bare("host"), vec![("v", InstanceValue::Integer(42))]);
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-required"));
    }

    #[test]
    fn a_constructor_discriminates_a_scalar_union_brand() {
        let g = scalar_union_graph();
        for name in ["meter(42)", "second(42)"] {
            let inst = instance(
                bare("host"),
                vec![("v", InstanceValue::String(name.into()))],
            );
            assert!(
                validate_simple(&g, &inst).is_empty(),
                "{name} should validate: {:?}",
                codes_of(&validate_simple(&g, &inst))
            );
        }
        // A constructor naming a non-member is a mismatch.
        let inst = instance(
            bare("host"),
            vec![("v", InstanceValue::String("foot(42)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-mismatch"));
    }

    // ----- tuple brand validation -----

    fn tuple_brand(name: &str, elements: Vec<Shape>) -> TypeDef {
        let mut td_ = td(name, &[], &[]);
        td_.shape = Some(BrandShape {
            shape: Shape::Tuple(elements),
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        });
        td_
    }

    /// A `host` record whose `pt` slot demands a `point` tuple brand `(Number, Number)`.
    fn point_host_graph() -> TypeGraph {
        build_graph(vec![
            tuple_brand(
                "point",
                vec![
                    Shape::Primitive(Primitive::Number),
                    Shape::Primitive(Primitive::Number),
                ],
            ),
            td("host", &[], &[("pt", false, record_shape("point"))]),
        ])
        .graph
    }

    #[test]
    fn a_tuple_constructor_validates_positionally() {
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![("pt", InstanceValue::String("point(20, 30)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_bare_paren_tuple_validates_against_a_tuple_brand() {
        // Decision A: the paren form coerces to the tuple brand, like a bare
        // scalar coerces to a scalar brand.
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![("pt", InstanceValue::String("(20, 30)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_bracket_sequence_at_a_tuple_brand_is_a_list_not_a_tuple() {
        // Decision A: a `[...]` sequence is always a LIST value, so at a tuple
        // slot it is a shape mismatch, never a tuple.
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "pt",
                seq(vec![InstanceValue::Integer(20), InstanceValue::Integer(30)]),
            )],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    #[test]
    fn a_wrong_arity_tuple_constructor_fires_tuple_arity_mismatch() {
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![("pt", InstanceValue::String("point(20, 30, 40)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"tuple-arity-mismatch"));
    }

    #[test]
    fn a_wrong_arity_paren_tuple_fires_tuple_arity_mismatch() {
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![("pt", InstanceValue::String("(20)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"tuple-arity-mismatch"));
    }

    #[test]
    fn a_wrong_element_type_in_a_tuple_fires_field_shape_mismatch() {
        // `point("x", 30)` — the first element is not a Number.
        let g = point_host_graph();
        let inst = instance(
            bare("host"),
            vec![("pt", InstanceValue::String(r#"point("x", 30)"#.into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    /// A brand def whose shape is parsed from source — for compositions too
    /// verbose to build by hand (a refined union, a tuple of brands).
    fn brand_from(name: &str, shape_src: &str) -> TypeDef {
        let mut td_ = td(name, &[], &[]);
        td_.shape = Some(BrandShape {
            shape: au_grammar::parse_shape(shape_src).expect("shape parses"),
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        });
        td_
    }

    /// `color-value` is a union of a refined `Number` (an 0-255 channel) and a
    /// refined hex `String`; `rgb` is a tuple of three `color-value`. A tuple
    /// whose elements are themselves union brands, over refined scalars.
    fn rgb_host_graph() -> TypeGraph {
        build_graph(vec![
            brand_from(
                "color-value",
                "<Number{integer & >=0 & <=255} | String{/^#[0-9a-fA-F]+$/}>",
            ),
            brand_from("rgb", "(color-value, color-value, color-value)"),
            td("host", &[], &[("col", false, record_shape("rgb"))]),
        ])
        .graph
    }

    #[test]
    fn an_rgb_tuple_of_a_refined_union_brand_validates() {
        let g = rgb_host_graph();
        let inst = instance(
            bare("host"),
            vec![("col", InstanceValue::String("rgb(200, 100, 50)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn an_rgb_channel_may_take_the_hex_branch() {
        // The third channel is a hex string, the String branch of color-value.
        let g = rgb_host_graph();
        let inst = instance(
            bare("host"),
            vec![("col", InstanceValue::String("rgb(200, 100, #ff)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn an_out_of_range_rgb_channel_matches_no_branch() {
        // 300 is neither an 0-255 integer nor a hex string, so the channel
        // matches no member of the union.
        let g = rgb_host_graph();
        let inst = instance(
            bare("host"),
            vec![("col", InstanceValue::String("rgb(300, 0, 0)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"field-shape-mismatch"));
    }

    #[test]
    fn a_wrong_arity_rgb_fires_tuple_arity_mismatch() {
        let g = rgb_host_graph();
        let inst = instance(
            bare("host"),
            vec![("col", InstanceValue::String("rgb(200, 100)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"tuple-arity-mismatch"));
    }

    #[test]
    fn a_tuple_member_of_a_union_accepts_its_own_constructor() {
        // `<point | second>` where point is a tuple `(Number, Number)`. A
        // `point(20, 30)` value must validate positionally, not be rejected as
        // "takes a single value" (code-review 2.1).
        let g = build_graph(vec![
            tuple_brand(
                "point",
                vec![
                    Shape::Primitive(Primitive::Number),
                    Shape::Primitive(Primitive::Number),
                ],
            ),
            scalar_brand("second", Primitive::Number),
            union_brand("measurement", &[rec_member("point"), rec_member("second")]),
            td("host", &[], &[("m", false, record_shape("measurement"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("m", InstanceValue::String("point(20, 30)".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
        // and a wrong arity through the union still fires the tuple check
        let bad = instance(
            bare("host"),
            vec![("m", InstanceValue::String("point(20, 30, 40)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &bad)).contains(&"tuple-arity-mismatch"));
    }

    #[test]
    fn a_bare_value_matches_a_primitive_member_of_a_union_brand() {
        // `<evidence | String>` — a bare string is the primitive branch, an
        // inline record is the evidence branch.
        let g = build_graph(vec![
            td("evidence", &[], &[("t", false, prim(Primitive::String))]),
            union_brand(
                "evidence-or-note",
                &[rec_member("evidence"), Shape::Primitive(Primitive::String)],
            ),
            td(
                "host",
                &[],
                &[("e", false, record_shape("evidence-or-note"))],
            ),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("e", InstanceValue::String("just a note".into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    /// A graph with a `<String | label>` union brand, where `label` is a scalar
    /// `String` brand — the representationally-overlapping case.
    fn string_label_graph() -> TypeGraph {
        build_graph(vec![
            scalar_brand("label", Primitive::String),
            union_brand(
                "stringy",
                &[Shape::Primitive(Primitive::String), rec_member("label")],
            ),
            td("host", &[], &[("v", false, record_shape("stringy"))]),
        ])
        .graph
    }

    #[test]
    fn a_primitive_member_constructor_escapes_a_constructor_shaped_literal() {
        // `String("looks(foo)")` at `<String | label>` forces the plain-String
        // branch, the literal escape for a constructor-shaped string.
        let g = string_label_graph();
        let inst = instance(
            bare("host"),
            vec![("v", InstanceValue::String(r#"String("looks(foo)")"#.into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_nominal_member_constructor_brands_at_an_overlapping_union() {
        // `label("looks(foo)")` at `<String | label>` brands the same literal.
        let g = string_label_graph();
        let inst = instance(
            bare("host"),
            vec![("v", InstanceValue::String(r#"label("looks(foo)")"#.into()))],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "{:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn a_non_member_primitive_constructor_is_a_mismatch_at_a_union() {
        // `Number(5)` at `<String | meter>` — Number is not a member (meter and
        // String are), so it is not admitted, a mismatch.
        let g = build_graph(vec![
            scalar_brand("meter", Primitive::Number),
            union_brand(
                "measure-or-text",
                &[Shape::Primitive(Primitive::String), rec_member("meter")],
            ),
            td(
                "host",
                &[],
                &[("v", false, record_shape("measure-or-text"))],
            ),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("v", InstanceValue::String("Number(5)".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-mismatch"));
    }

    #[test]
    fn a_bare_value_at_an_overlapping_union_is_ambiguous() {
        // `<String | label>` — a bare string could be plain String or a label,
        // both represented as String with no discriminator, so it is ambiguous.
        let g = string_label_graph();
        let inst = instance(
            bare("host"),
            vec![("v", InstanceValue::String("hi".into()))],
        );
        assert!(codes_of(&validate_simple(&g, &inst)).contains(&"brand-constructor-required"));
    }

    #[test]
    fn a_bare_value_coerces_to_a_unique_nominal_member() {
        // `<String | meter>` — 10 fits only meter (String rejects a number), so it
        // coerces to meter with no error; "hi" fits only String.
        let g = build_graph(vec![
            scalar_brand("meter", Primitive::Number),
            union_brand(
                "measure-or-text",
                &[Shape::Primitive(Primitive::String), rec_member("meter")],
            ),
            td(
                "host",
                &[],
                &[("v", false, record_shape("measure-or-text"))],
            ),
        ])
        .graph;
        for v in [
            InstanceValue::Integer(10),
            InstanceValue::String("hi".into()),
        ] {
            let inst = instance(bare("host"), vec![("v", v)]);
            assert!(
                validate_simple(&g, &inst).is_empty(),
                "{:?}",
                codes_of(&validate_simple(&g, &inst))
            );
        }
    }

    #[test]
    fn a_distinguishing_union_is_not_ambiguous() {
        // `<String | Url>` — Url is distinguishing (prefix), so a bare value picks
        // by its shape, never ambiguous. Non-regression for the shipped pattern.
        let g = build_graph(vec![
            union_brand(
                "text-or-link",
                &[
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Url),
                ],
            ),
            td("host", &[], &[("v", false, record_shape("text-or-link"))]),
        ])
        .graph;
        for s in ["hi", "https://example.com"] {
            let inst = instance(bare("host"), vec![("v", InstanceValue::String(s.into()))]);
            assert!(
                validate_simple(&g, &inst).is_empty(),
                "{s}: {:?}",
                codes_of(&validate_simple(&g, &inst))
            );
        }
    }

    /// Drive `validate_body` (the body pass) over an instance plus a raw body
    /// string. Mirrors the build pipeline, which calls `validate` then
    /// `validate_body` on each instance.
    fn validate_body_with(
        graph: &TypeGraph,
        instance: &Instance,
        body_source: &str,
        is_markdown: bool,
    ) -> Vec<Diagnostic> {
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
        let claims = BTreeMap::new();
        let body_sources = BTreeMap::new();
        let record_targets = BTreeMap::new();
        let ref_data = MapRefData {
            claims_by_path: &claims,
            body_sources: &body_sources,
            record_targets: &record_targets,
        };
        let ctx = ValidateContext {
            graph,
            repo_index: &idx,
            ref_data: &ref_data,
            cross_repo: None,
            resolution: None,
            meta_marker: None,
        };
        crate::body_validate::validate_body(&ctx, instance, body_source, 0, is_markdown)
    }

    /// `validate_body_with` with a populated `RepoIndex` + claims, for body
    /// wikilink contributions whose target must RESOLVE. `target_type_satisfies`
    /// returns early (no mismatch) on an unresolvable target, so the closure
    /// check — and the `any` existence-only branch under test — only runs when
    /// the target resolves and carries a claim.
    fn validate_body_in_repo(
        graph: &TypeGraph,
        idx: &RepoIndex,
        claims: &BTreeMap<PathBuf, Vec<TypeName>>,
        instance: &Instance,
        body_source: &str,
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
            resolution: None,
            meta_marker: None,
        };
        crate::body_validate::validate_body(&ctx, instance, body_source, 0, true)
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

    #[test]
    fn valid_singleton_emits_no_diagnostics() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("description", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(
            bare("note"),
            vec![("description", InstanceValue::String("hi".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn unknown_type_claim_fires() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let inst = instance(bare("missing"), vec![]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["unknown-type-claim"]
        );
    }

    #[test]
    fn required_field_absent_fires_per_missing_field() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[
                ("a", false, prim(Primitive::String)),
                ("b", false, prim(Primitive::String)),
                ("c", true, prim(Primitive::String)),
            ],
        )])
        .graph;
        let inst = instance(bare("note"), vec![("a", InstanceValue::String("x".into()))]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(codes, vec!["required-field-absent"]);
    }

    #[test]
    fn required_field_absent_hints_a_near_miss_undeclared_key() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("size", false, prim(Primitive::Number))],
        )])
        .graph;
        // `siize` is an undeclared near-miss of the missing required `size`.
        let inst = instance(bare("note"), vec![("siize", InstanceValue::Integer(7))]);
        let diags = validate_simple(&g, &inst);
        let d = diags
            .iter()
            .find(|d| d.code.as_str() == "required-field-absent")
            .expect("required-field-absent");
        assert!(
            d.message.contains("did you mean 'siize'"),
            "expected a near-miss hint, got: {}",
            d.message
        );
        // Primary span stays on the type claim; the hint ADDS a related span at
        // the typo'd key, on top of the origin decl span already present.
        assert!(
            d.related.len() >= 2,
            "expected an added related span at the near-miss key, got {:?}",
            d.related
        );
    }

    #[test]
    fn required_field_absent_has_no_hint_when_nothing_is_close() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("size", false, prim(Primitive::Number))],
        )])
        .graph;
        // `color` is not within edit distance of `size`, so no hint fires.
        let inst = instance(
            bare("note"),
            vec![("color", InstanceValue::String("red".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let d = diags
            .iter()
            .find(|d| d.code.as_str() == "required-field-absent")
            .expect("required-field-absent");
        assert!(
            !d.message.contains("did you mean"),
            "no hint expected, got: {}",
            d.message
        );
        assert_eq!(
            d.related.len(),
            1,
            "only the origin decl span: {:?}",
            d.related
        );
    }

    #[test]
    fn required_field_check_walks_parent_closure() {
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "decision",
                &["note"],
                &[("decided_by", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            bare("decision"),
            vec![("description", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["required-field-absent"]
        );
    }

    #[test]
    fn field_shape_mismatch_fires_for_each_primitive() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[
                ("s", false, prim(Primitive::String)),
                ("n", false, prim(Primitive::Number)),
                ("b", false, prim(Primitive::Boolean)),
                ("d", false, prim(Primitive::Date)),
                ("dt", false, prim(Primitive::DateTime)),
            ],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![
                ("s", InstanceValue::Integer(1)),
                ("n", InstanceValue::String("not a number".into())),
                ("b", InstanceValue::Integer(0)),
                ("d", InstanceValue::String("yesterday".into())),
                ("dt", InstanceValue::String("2026-04-12".into())),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "field-shape-mismatch")
                .count(),
            5
        );
    }

    #[test]
    fn opaque_slot_accepts_every_value_shape() {
        // [[type-def shape opaque::au-type-system]]: an `opaque` slot imposes no type and reads
        // nothing inside. A scalar, an array, and an object carrying a nested
        // `type:` all pass with no diagnostic. The nested `type:` is plain data,
        // never read as an inline claim, so a bogus claim inside an `opaque`
        // value is inert.
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[
                ("blob", false, Ok(Shape::Opaque)),
                (
                    "list",
                    false,
                    Ok(Shape::List {
                        inner: Box::new(Shape::Opaque),
                        min: 0,
                        max: None,
                    }),
                ),
            ],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![
                (
                    "blob",
                    inline(
                        Some(bare("not-a-real-type")),
                        vec![("k", InstanceValue::String("v".into()))],
                    ),
                ),
                (
                    "list",
                    seq(vec![
                        InstanceValue::String("s".into()),
                        InstanceValue::Integer(1),
                        InstanceValue::Boolean(true),
                    ]),
                ),
            ],
        );
        let diags = validate_simple(&g, &inst);
        assert!(
            diags.is_empty(),
            "an opaque slot imposes no type and reads nothing, expected no diagnostics, got {:?}",
            codes_of(&diags)
        );
    }

    #[test]
    fn any_slot_interprets_a_nested_type_claim() {
        // [[type-def shape any::au-type-system]]: the interpreted top READS the value. A mapping
        // carrying a nested `type:` is an inline claim, so a bogus claim fires
        // `unknown-type-claim` — the contrast with an `opaque` slot, where the
        // same value is inert.
        let g = build_graph(vec![td("rec", &[], &[("v", false, Ok(Shape::Any))])]).graph;
        let inst = instance(
            bare("rec"),
            vec![(
                "v",
                inline(
                    Some(bare("not-a-real-type")),
                    vec![("k", InstanceValue::String("x".into()))],
                ),
            )],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["unknown-type-claim"]
        );
    }

    #[test]
    fn any_slot_untyped_mapping_passes() {
        // A mapping under `any` with NO `type:` is an untyped open value: it
        // reads clean, nothing to validate against ([[type-def shape any::au-type-system]]).
        let g = build_graph(vec![td("rec", &[], &[("v", false, Ok(Shape::Any))])]).graph;
        let inst = instance(
            bare("rec"),
            vec![(
                "v",
                inline(None, vec![("k", InstanceValue::String("x".into()))]),
            )],
        );
        assert!(
            validate_simple(&g, &inst).is_empty(),
            "an untyped mapping under `any` passes, got {:?}",
            codes_of(&validate_simple(&g, &inst))
        );
    }

    #[test]
    fn non_finite_number_fires_its_own_code_not_a_generic_mismatch() {
        // `.nan` / `.inf` parse as floats but are not valid numbers. A finite
        // float still passes; the non-finite ones get a dedicated diagnostic.
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[
                ("ok", false, prim(Primitive::Number)),
                ("nan", false, prim(Primitive::Number)),
                ("inf", false, prim(Primitive::Number)),
            ],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![
                ("ok", InstanceValue::Float(1.5)),
                ("nan", InstanceValue::Float(f64::NAN)),
                ("inf", InstanceValue::Float(f64::INFINITY)),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes.iter().filter(|c| **c == "non-finite-number").count(),
            2,
            "nan and inf each fire non-finite-number, got {codes:?}"
        );
        assert!(
            !codes.contains(&"field-shape-mismatch"),
            "non-finite must not surface as a generic mismatch, got {codes:?}"
        );
    }

    #[test]
    fn primitive_conformance_accepts_valid_values() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[
                ("s", false, prim(Primitive::String)),
                ("n_int", false, prim(Primitive::Number)),
                ("n_float", false, prim(Primitive::Number)),
                ("b", false, prim(Primitive::Boolean)),
                ("d", false, prim(Primitive::Date)),
                ("dt", false, prim(Primitive::DateTime)),
            ],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![
                ("s", InstanceValue::String("hi".into())),
                ("n_int", InstanceValue::Integer(42)),
                ("n_float", InstanceValue::Float(3.14)),
                ("b", InstanceValue::Boolean(true)),
                ("d", InstanceValue::String("2026-04-12".into())),
                ("dt", InstanceValue::String("2026-04-12T133000Z".into())),
            ],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn extras_pass_silently() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("description", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(
            bare("note"),
            vec![
                ("description", InstanceValue::String("x".into())),
                ("extra_field", InstanceValue::String("anything".into())),
            ],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn mixin_with_no_overlap_validates_when_all_required_provided() {
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("audience", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![
                ("description", InstanceValue::String("d".into())),
                ("audience", InstanceValue::String("external".into())),
            ],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn mixin_auto_unify_validates_clean_with_single_bare_value() {
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![("description", InstanceValue::String("d".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn untouched_required_divergent_field_fires_required_per_origin() {
        // `[a, b]`, divergent REQUIRED `f`, never written: required is required,
        // so both unfilled origins fire `required-field-absent`. No bare use, so
        // no collision.
        let g = build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(list(&["a", "b"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes,
            vec!["required-field-absent", "required-field-absent"],
            "both required origins of an untouched divergent field must fire"
        );
    }

    #[test]
    fn untouched_optional_divergent_field_is_clean() {
        // Divergent but OPTIONAL at both origins → untouched is clean.
        let g = build_graph(vec![
            td("a", &[], &[("f", true, prim(Primitive::String))]),
            td("b", &[], &[("f", true, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(list(&["a", "b"]), vec![]);
        let diags = validate_simple(&g, &inst);
        assert!(diags.is_empty(), "got {:?}", codes_of(&diags));
    }

    #[test]
    fn bare_use_of_a_divergent_field_fires_collision_plus_required() {
        // A bare value fills no origin, so on a REQUIRED divergent field it fires
        // `mixin-collision` (the bare ambiguity) AND `required-field-absent` for
        // each unfilled required origin — additive, no suppression.
        let g = build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(
            list(&["a", "b"]),
            vec![("f", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"mixin-collision"), "got {codes:?}");
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "required-field-absent")
                .count(),
            2,
            "both required origins are unfilled by a bare value: {codes:?}"
        );
        let collision = diags
            .iter()
            .find(|d| d.code.as_str() == "mixin-collision")
            .unwrap();
        assert!(collision.message.contains("'a'"));
        assert!(collision.message.contains("'b'"));
    }

    #[test]
    fn fully_qualified_divergent_field_is_clean() {
        let g = build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(
            list(&["a", "b"]),
            vec![
                ("f{a}", InstanceValue::String("x".into())),
                ("f{b}", InstanceValue::Integer(1)),
            ],
        );
        let diags = validate_simple(&g, &inst);
        assert!(
            diags.is_empty(),
            "both origins qualified and valid must be clean: {:?}",
            codes_of(&diags)
        );
    }

    #[test]
    fn partially_qualified_divergent_field_fires_required_for_the_unfilled_origin() {
        // Qualify one of two required origins → the other required origin is
        // still unfilled, so required-field-absent fires for it, once, and no
        // mixin-collision (no bare use).
        let g = build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(
            list(&["a", "b"]),
            vec![("f{a}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(codes, vec!["required-field-absent"]);
    }

    #[test]
    fn qualified_divergent_value_validates_against_its_origin_shape() {
        // `f{b}` binds to b's Number shape, so a string value there is a
        // field-shape-mismatch — proving the value checks the NAMED origin.
        let g = build_graph(vec![
            td("a", &[], &[("f", true, prim(Primitive::String))]),
            td("b", &[], &[("f", true, prim(Primitive::Number))]),
        ])
        .graph;
        let inst = instance(
            list(&["a", "b"]),
            vec![("f{b}", InstanceValue::String("not a number".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"field-shape-mismatch"), "got {codes:?}");
    }

    #[test]
    fn mixin_required_field_from_one_branch_fires_when_missing() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "deliverable",
                &[],
                &[("audience", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(list(&["note", "deliverable"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"required-field-absent"));
    }

    #[test]
    fn mixin_unknown_type_wins_over_mixin_handling() {
        let g = build_graph(vec![td("a", &[], &[])]).graph;
        let inst = instance(list(&["a", "missing"]), vec![]);
        let diags = validate_simple(&g, &inst);
        assert_eq!(codes_of(&diags), vec!["unknown-type-claim"]);
    }

    #[test]
    fn mixin_claim_order_is_commutative_in_diagnostics() {
        let g = build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
        ])
        .graph;
        let ab_diags = validate_simple(&g, &instance(list(&["a", "b"]), vec![]));
        let ba_diags = validate_simple(&g, &instance(list(&["b", "a"]), vec![]));
        let ab = codes_of(&ab_diags);
        let ba = codes_of(&ba_diags);
        assert_eq!(ab, ba);
    }

    #[test]
    fn duplicate_claim_fires_warning_once_per_repeat() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("description", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(
            list(&["note", "note"]),
            vec![("description", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // Exactly one duplicate-claim warning; the file otherwise validates.
        assert_eq!(codes.iter().filter(|c| **c == "duplicate-claim").count(), 1);
        // It's a warning, not an error.
        let dup = diags
            .iter()
            .find(|d| d.code.as_str() == "duplicate-claim")
            .unwrap();
        assert_eq!(dup.severity, Severity::Warning);
    }

    #[test]
    fn duplicate_claim_does_not_fire_subsumption() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let inst = instance(list(&["note", "note"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"duplicate-claim"));
        assert!(!codes.contains(&"subsumption-in-mixin"));
    }

    #[test]
    fn subsumption_fires_when_one_claim_implies_another() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("decision", &["note"], &[]),
            td("decision.decided", &["decision"], &[]),
        ])
        .graph;
        let inst = instance(list(&["decision", "decision.decided"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // Exactly one subsumption warning; no duplicate-claim.
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "subsumption-in-mixin")
                .count(),
            1
        );
        // Primary span on the redundant (wider) claim — `decision`. Message
        // names both.
        let sub = diags
            .iter()
            .find(|d| d.code.as_str() == "subsumption-in-mixin")
            .unwrap();
        assert_eq!(sub.severity, Severity::Warning);
        assert!(sub.message.contains("'decision'"));
        assert!(sub.message.contains("'decision.decided'"));
    }

    #[test]
    fn subsumption_skips_when_a_claim_is_unknown() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let inst = instance(list(&["note", "missing"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // Unknown-type-claim fires; subsumption skipped because one side is
        // unknown.
        assert!(codes.contains(&"unknown-type-claim"));
        assert!(!codes.contains(&"subsumption-in-mixin"));
    }

    #[test]
    fn redundant_claim_warnings_dont_fire_for_single_claim() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        let inst = instance(bare("note"), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(!codes.contains(&"duplicate-claim"));
        assert!(!codes.contains(&"subsumption-in-mixin"));
    }

    #[test]
    fn one_element_list_claim_validates_like_bare() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("description", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(
            list(&["note"]),
            vec![("description", InstanceValue::String("x".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    // ---------- [[type-def fields collision - auto-unify and qualified field::au-type-system]] qualified field resolution ----------

    #[test]
    fn qualified_field_resolves_to_originator_via_closure() {
        // `decision.decided` instance writes `decision.decided:title`; the
        // originator is `note` (which declares title). Title is optional
        // so required-field-absent doesn't fire and we can isolate the
        // qualifier-path shape conformance.
        let g = build_graph(vec![
            td("note", &[], &[("title", true, prim(Primitive::String))]),
            td("decision", &["note"], &[]),
            td("decision.decided", &["decision"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("decision.decided"),
            vec![(
                "title{decision.decided}",
                InstanceValue::String("hi".into()),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn qualified_field_validates_shape_against_originator_decl() {
        // Wrong-type value through qualified key fires field-shape-mismatch
        // (same code as bare path).
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("title", true, prim(Primitive::Number))],
        )])
        .graph;
        let inst = instance(
            bare("note"),
            vec![("title{note}", InstanceValue::String("not a number".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"field-shape-mismatch"));
    }

    #[test]
    fn qualifier_not_in_closure_fires() {
        let g = build_graph(vec![
            td("note", &[], &[("title", true, prim(Primitive::String))]),
            td("decision", &[], &[]),
        ])
        .graph;
        // Instance claims `note`; `decision` is a sibling root, not in
        // closure_of(note).
        let inst = instance(
            bare("note"),
            vec![("title{decision}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "qualifier-not-in-closure")
                .count(),
            1
        );
    }

    #[test]
    fn qualifier_in_closure_but_does_not_declare_field_fires() {
        // `decision`'s closure includes `note`, but neither `decision` nor
        // `note` declares `audience` — so `decision:audience` resolves to
        // no originator.
        let g = build_graph(vec![td("note", &[], &[]), td("decision", &["note"], &[])]).graph;
        let inst = instance(
            bare("decision"),
            vec![("audience{decision}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "qualifier-does-not-declare-field")
                .count(),
            1
        );
    }

    #[test]
    fn qualifier_reaching_a_divergent_field_via_a_descendant_is_ambiguous() {
        // `a` declares title: String, `b` declares title: Number, `c extends
        // [a, b]`, so title is divergent in c's closure. `title{c}` reaches the
        // field at BOTH origins, so which shape it checks against is arbitrary:
        // rejected as `qualifier-ambiguous`. Filling no origin, both required
        // origins still fire.
        let g = build_graph(vec![
            td("a", &[], &[("title", false, prim(Primitive::String))]),
            td("b", &[], &[("title", false, prim(Primitive::Number))]),
            td("c", &["a", "b"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("c"),
            vec![("title{c}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "qualifier-ambiguous")
                .count(),
            1,
            "got {codes:?}"
        );
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "required-field-absent")
                .count(),
            2,
            "nothing was filled, so both required origins fire; got {codes:?}"
        );
    }

    #[test]
    fn qualifier_naming_a_declaring_origin_of_a_divergent_field_is_not_ambiguous() {
        // Same graph, but `title{a}` names a single declaring origin, so it
        // resolves and validates against a's String shape. Only b's unfilled
        // required origin fires; no ambiguity.
        let g = build_graph(vec![
            td("a", &[], &[("title", false, prim(Primitive::String))]),
            td("b", &[], &[("title", false, prim(Primitive::Number))]),
            td("c", &["a", "b"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("c"),
            vec![("title{a}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            !codes.contains(&"qualifier-ambiguous"),
            "a declaring-origin qualifier is unambiguous; got {codes:?}"
        );
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "required-field-absent")
                .count(),
            1,
            "only b's origin is unfilled; got {codes:?}"
        );
    }

    #[test]
    fn qualifier_via_a_descendant_of_an_auto_unified_field_is_not_ambiguous() {
        // a and b both declare title: String (token-equal → auto-unified), c
        // extends both. `title{c}` reaches one shape, so it resolves cleanly
        // with no ambiguity — the rejection is specific to DIVERGENT reach.
        let g = build_graph(vec![
            td("a", &[], &[("title", true, prim(Primitive::String))]),
            td("b", &[], &[("title", true, prim(Primitive::String))]),
            td("c", &["a", "b"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("c"),
            vec![("title{c}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            !codes.contains(&"qualifier-ambiguous"),
            "an auto-unified field reaches one shape; got {codes:?}"
        );
    }

    #[test]
    fn malformed_qualifier_key_each_subreason() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        // Empty field before `{`.
        let i1 = instance(
            bare("note"),
            vec![("{note}", InstanceValue::String("x".into()))],
        );
        // Empty qualifier inside `{}`.
        let i2 = instance(
            bare("note"),
            vec![("foo{}", InstanceValue::String("x".into()))],
        );
        // Unclosed brace.
        let i3 = instance(
            bare("note"),
            vec![("foo{note", InstanceValue::String("x".into()))],
        );
        // Qualifier type violates type-name regex (digit start).
        let i4 = instance(
            bare("note"),
            vec![("foo{1bad}", InstanceValue::String("x".into()))],
        );

        for inst in [&i1, &i2, &i3, &i4] {
            let diags = validate_simple(&g, inst);
            let codes = codes_of(&diags);
            assert!(
                codes.contains(&"malformed-qualifier-key"),
                "expected malformed-qualifier-key, got {codes:?}"
            );
        }
    }

    #[test]
    fn qualified_key_on_auto_unified_field_resolves_via_any_origin() {
        // Both `note` and `deliverable` declare `description: String`
        // (auto-unify). A `note:description` key on an instance claiming
        // both should resolve cleanly via the note-origin path.
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", true, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", true, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![("description{note}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        // No qualifier-resolution errors.
        let codes = codes_of(&diags);
        assert!(!codes.contains(&"qualifier-not-in-closure"));
        assert!(!codes.contains(&"qualifier-does-not-declare-field"));
        assert!(!codes.contains(&"malformed-qualifier-key"));
        assert!(!codes.contains(&"field-shape-mismatch"));
    }

    // ---------- [[type-def fields collision - auto-unify and qualified field::au-type-system]] per-originator required-field semantics ----------

    #[test]
    fn bare_value_satisfies_all_auto_unified_origins() {
        // Both note and deliverable independently declare required
        // description with token-equal shape → auto-unify. Bare
        // `description` contributes (note, description) AND
        // (deliverable, description) → both slots filled, no
        // required-field-absent.
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![("description", InstanceValue::String("x".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn qualified_value_satisfies_only_named_origin() {
        // Both origins required. Only note:description provided →
        // (note, description) filled, (deliverable, description) empty
        // → required-field-absent fires for the deliverable origin only.
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![("description{note}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let absent: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.code.as_str() == "required-field-absent")
            .collect();
        assert_eq!(absent.len(), 1);
        // Message names the deliverable origin specifically.
        assert!(absent[0].message.contains("'deliverable'"));
        assert!(!absent[0].message.contains("'note'"));
    }

    #[test]
    fn per_origin_optional_asymmetry_under_qualifier_mode() {
        // note: description required; deliverable: description optional.
        // Only note:description provided → (note, description) filled.
        // The deliverable slot is optional, so no required-field-absent
        // fires for it; required-field-absent does NOT fire for note
        // either since note:description satisfies that slot.
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", true, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![("description{note}", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
    }

    #[test]
    fn per_origin_optional_asymmetry_fires_for_required_origin_when_only_optional_filled() {
        // note: required; deliverable: optional. Only deliverable:description
        // provided → deliverable's optional slot filled, but note's required
        // slot is empty → required-field-absent fires for note.
        let g = build_graph(vec![
            td(
                "note",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "deliverable",
                &[],
                &[("description", true, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![(
                "description{deliverable}",
                InstanceValue::String("x".into()),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let absent: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.code.as_str() == "required-field-absent")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("'note'"));
    }

    // ---------- [[type-def fields collision - auto-unify and qualified field::au-type-system]] mixed-bare-and-qualified-field ----------

    #[test]
    fn bare_plus_qualified_on_same_field_fires_once() {
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("description", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(
            bare("note"),
            vec![
                ("description", InstanceValue::String("bare".into())),
                (
                    "description{note}",
                    InstanceValue::String("prefixed".into()),
                ),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let mixed: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.code.as_str() == "mixed-bare-and-qualified-field")
            .collect();
        assert_eq!(mixed.len(), 1);
        assert!(mixed[0].message.contains("'description'"));
        // Related span covers the qualified entry.
        assert_eq!(mixed[0].related.len(), 1);
    }

    #[test]
    fn pure_prefixed_multiple_entries_does_not_fire_mixed() {
        // Two qualified entries (different qualifiers, same field name)
        // should NOT fire mixed-bare-and-qualified.
        let g = build_graph(vec![
            td("note", &[], &[("title", true, prim(Primitive::String))]),
            td(
                "deliverable",
                &[],
                &[("title", true, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["note", "deliverable"]),
            vec![
                ("title{note}", InstanceValue::String("a".into())),
                ("title{deliverable}", InstanceValue::String("b".into())),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(!codes.contains(&"mixed-bare-and-qualified-field"));
    }

    #[test]
    fn unresolved_prefix_does_not_trigger_mixed_bare_and_prefixed() {
        // `decision:audience` doesn't resolve (decision's closure has no
        // audience). A separate bare `audience` is open-world (no field
        // by that name in the closure). The mixed-form rule shouldn't
        // fire here — prefix resolution failed, the prefix-does-not-
        // declare-field diagnostic is the relevant one.
        let g = build_graph(vec![td("note", &[], &[]), td("decision", &["note"], &[])]).graph;
        let inst = instance(
            bare("decision"),
            vec![
                ("audience", InstanceValue::String("a".into())),
                ("audience{decision}", InstanceValue::String("b".into())),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // The prefix diagnostic still fires regardless.
        assert!(codes.contains(&"qualifier-does-not-declare-field"));
        // mixed-bare-and-qualified STILL fires here — the user wrote both
        // forms for the same field name; whether the prefix resolved is
        // a separate concern (the user declared a contradiction about
        // auto-unify; both diagnostics are useful).
        assert!(codes.contains(&"mixed-bare-and-qualified-field"));
    }

    #[test]
    fn unimplemented_shape_surfaces_at_instance_use_site() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("rationale", false, Err("rationale"))],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![("rationale", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"not-yet-implemented-shape-feature"));
    }

    #[test]
    fn union_slot_accepts_value_matching_either_branch() {
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "evidence",
                &[],
                &[(
                    "support",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("rat.md", "rat.md", &["rationale"]),
            ("th.md", "th.md", &["thesis"]),
        ]);
        let rat = instance(
            bare("evidence"),
            vec![("support", InstanceValue::String("[[rat]]".into()))],
        );
        let th = instance(
            bare("evidence"),
            vec![("support", InstanceValue::String("[[th]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &rat).is_empty());
        assert!(validate_with(&g, &idx, &claims, &th).is_empty());
    }

    #[test]
    fn union_slot_rejects_value_matching_no_branch_with_single_diagnostic() {
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("note", &[], &[]),
            td(
                "evidence",
                &[],
                &[(
                    "support",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("note.md", "note.md", &["note"])]);
        let inst = instance(
            bare("evidence"),
            vec![("support", InstanceValue::String("[[note]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(codes_of(&diags), vec!["field-shape-mismatch"]);
        // Branch-level diagnostics must not propagate.
        assert!(
            !diags[0].message.contains("rationale\n") && !diags[0].message.contains("thesis\n"),
            "branch-level details should not appear in the union mismatch: {}",
            diags[0].message
        );
    }

    #[test]
    fn union_slot_with_inline_or_ref_branch_accepts_both_forms() {
        // End-to-end proof of the per-branch `&` fix (finding 2.8, which was
        // purely the grammar rejecting the syntax): the grammar now parses
        // `<rationale& | thesis>`, and au-core's generic union machinery already
        // validates an InlineOrReference branch in BOTH forms — a wikilink
        // reference and an inline record — beside a plain record branch.
        let shape = au_grammar::parse_shape("<rationale& | thesis>")
            .expect("per-branch `&` should parse after the grammar fix");
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("evidence", &[], &[("support", false, Ok(shape))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("rat.md", "rat.md", &["rationale"])]);
        // `rationale&` reference form: a wikilink whose target claims rationale.
        let by_ref = instance(
            bare("evidence"),
            vec![("support", InstanceValue::String("[[rat]]".into()))],
        );
        // `rationale&` inline form: an inline record claiming rationale.
        let by_inline_rationale = instance(
            bare("evidence"),
            vec![("support", inline(Some(bare("rationale")), vec![]))],
        );
        // plain `thesis` record branch: an inline record claiming thesis.
        let by_inline_thesis = instance(
            bare("evidence"),
            vec![("support", inline(Some(bare("thesis")), vec![]))],
        );
        assert!(validate_with(&g, &idx, &claims, &by_ref).is_empty());
        assert!(validate_with(&g, &idx, &claims, &by_inline_rationale).is_empty());
        assert!(validate_with(&g, &idx, &claims, &by_inline_thesis).is_empty());
    }

    #[test]
    fn intersection_slot_accepts_value_matching_all_branches() {
        // For value validation, Intersection of References passes when the
        // target's closure includes every branch's name.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("maturity", &[], &[]),
            // both is a leaf type that claims both rationale and maturity
            // (sealed-leaf-style mixin candidate; here a plain mixin).
            td("both", &["rationale", "maturity"], &[]),
            td(
                "combo",
                &[],
                &[(
                    "joint",
                    false,
                    Ok(Shape::Intersection(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("maturity".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("both.md", "both.md", &["both"])]);
        let inst = instance(
            bare("combo"),
            vec![("joint", InstanceValue::String("[[both]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    #[test]
    fn intersection_slot_concatenates_per_branch_failures() {
        // Target only satisfies one branch; the other fires a per-branch
        // diagnostic. With two missing branches, two diagnostics fire.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("maturity", &[], &[]),
            td("note", &[], &[]),
            td(
                "combo",
                &[],
                &[(
                    "joint",
                    false,
                    Ok(Shape::Intersection(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("maturity".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("note.md", "note.md", &["note"])]);
        let inst = instance(
            bare("combo"),
            vec![("joint", InstanceValue::String("[[note]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        // Both branches fail; both diagnostics fire (concatenated).
        let codes = codes_of(&diags);
        assert_eq!(
            codes.len(),
            2,
            "expected one per failing branch, got {:?}",
            codes
        );
        assert!(codes.iter().all(|c| *c == "reference-target-type-mismatch"));
    }

    #[test]
    fn list_of_union_validates_each_element() {
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("note", &[], &[]),
            td(
                "listy",
                &[],
                &[(
                    "items",
                    false,
                    Ok(Shape::List {
                        inner: Box::new(Shape::Union(vec![
                            Shape::Reference("rationale".into()),
                            Shape::Reference("thesis".into()),
                        ])),
                        min: 0,
                        max: None,
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("rat.md", "rat.md", &["rationale"]),
            ("th.md", "th.md", &["thesis"]),
            ("note.md", "note.md", &["note"]),
        ]);
        // All-good list.
        let good = instance(
            bare("listy"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::String("[[rat]]".into()),
                    InstanceValue::String("[[th]]".into()),
                ]),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &good).is_empty());

        // Bad element fires once at the element's position.
        let bad = instance(
            bare("listy"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::String("[[rat]]".into()),
                    InstanceValue::String("[[note]]".into()),
                ]),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &bad);
        assert_eq!(codes_of(&diags), vec!["field-shape-mismatch"]);
    }

    #[test]
    fn compound_reference_star_union_accepts_any_branch() {
        // `<rationale | thesis>*` — target satisfies AT LEAST ONE.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("rat.md", "rat.md", &["rationale"]),
            ("th.md", "th.md", &["thesis"]),
        ]);
        for target in ["[[rat]]", "[[th]]"] {
            let inst = instance(
                bare("ev"),
                vec![("target", InstanceValue::String(target.into()))],
            );
            let diags = validate_with(&g, &idx, &claims, &inst);
            assert!(
                diags.is_empty(),
                "expected clean validation for {}, got {:?}",
                target,
                diags
            );
        }
    }

    #[test]
    fn compound_reference_star_union_rejects_when_target_satisfies_no_branch() {
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("note", &[], &[]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("note.md", "note.md", &["note"])]);
        let inst = instance(
            bare("ev"),
            vec![("target", InstanceValue::String("[[note]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(codes_of(&diags), vec!["reference-target-type-mismatch"]);
        assert!(diags[0].message.contains("note"));
    }

    #[test]
    fn compound_reference_star_intersection_requires_all_branches() {
        // `<rationale & maturity>*` — target's closure must include every
        // branch.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("maturity", &[], &[]),
            td("both", &["rationale", "maturity"], &[]),
            td(
                "combo",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Intersection,
                        branches: vec!["rationale".into(), "maturity".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("both.md", "both.md", &["both"]),
            ("rat.md", "rat.md", &["rationale"]),
        ]);
        // Target with both names in closure passes.
        let good = instance(
            bare("combo"),
            vec![("target", InstanceValue::String("[[both]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &good).is_empty());
        // Target with only one name fails.
        let bad = instance(
            bare("combo"),
            vec![("target", InstanceValue::String("[[rat]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &bad);
        assert_eq!(codes_of(&diags), vec!["reference-target-type-mismatch"]);
    }

    // `file` is existence-only per [[type-def shape file::au-type-system]]. The standalone-reference path
    // (`file*`) short-circuits as soon as the wikilink resolves; the same
    // semantics must hold when `file` appears as a branch in a compound.

    #[test]
    fn compound_reference_star_union_with_file_branch_accepts_untyped_file() {
        // `<file | note>*` against an untyped asset passes — the `file`
        // branch is satisfied by existence alone, union takes any branch.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[diagram.pdf]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    #[test]
    fn compound_reference_star_union_with_file_branch_accepts_typed_file() {
        // `<file | note>*` against a typed-note file also passes — both
        // branches are satisfied; union needs at least one.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("notes/foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    #[test]
    fn compound_reference_star_union_with_file_branch_silent_block_id_on_asset() {
        // `<file | note>*` value `[[diagram.pdf^nope]]`: the `file` branch is
        // satisfied by existence. The asset holds no body, so the `^nope` anchor
        // is navigational but unverifiable and stays silent — no block-id error
        // (the plain-block anchor decision, 2607052046; consistent with the
        // navigational rule's asset silence).
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("link-card"),
            vec![(
                "target",
                InstanceValue::String("[[diagram.pdf^nope]]".into()),
            )],
        );
        assert!(
            validate_with(&g, &idx, &claims, &inst).is_empty(),
            "asset block-id is silently unchecked, got {:?}",
            codes_of(&validate_with(&g, &idx, &claims, &inst))
        );
    }

    #[test]
    fn compound_union_file_branch_rejects_a_block_referent_with_wrong_claim() {
        // `<file | edge>*` filled by `[[canvas^^n1]]` where record `n1` claims
        // `node`, not `edge`. A `^^` is a block-referent, NOT a whole file, so the
        // `file` branch must not rescue it — the block's claim has to satisfy a
        // non-file branch, and `node` is not `edge`. reference-target-type-mismatch.
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("edge", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "edge".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["edge"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["node"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["reference-target-type-mismatch"]
        );
    }

    #[test]
    fn compound_union_file_branch_accepts_a_whole_file_and_a_satisfying_block_referent() {
        // The fix must not break the two cases the `file` branch legitimately
        // covers: a whole-file `[[canvas]]` (no block-id -> the file branch), and a
        // `^^n1` whose block claim satisfies the non-file branch (`edge`).
        let g = build_graph(vec![
            td("edge", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "edge".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["edge"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["edge"])]);
        // A whole-file reference is satisfied by the `file` branch.
        let whole = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[canvas]]".into()))],
        );
        assert!(
            validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &whole)
                .is_empty(),
            "whole-file [[canvas]] should satisfy the file branch"
        );
        // A `^^` whose block claims `edge` satisfies the non-file branch.
        let block = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        assert!(
            validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &block)
                .is_empty(),
            "a ^^ block claiming edge should satisfy the edge branch"
        );
    }

    #[test]
    fn file_ref_rejects_a_block_id_fragment() {
        // `file*` references a whole file ([[type-def shape file::au-type-system]]). A `^block-id`
        // addresses a part of the file, which `file*` forbids — `any*` is the
        // block-addressing form. The fragment is a shape mismatch, even though
        // the file itself resolves.
        let g = build_graph(vec![td(
            "gallery",
            &[],
            &[("images", false, list_shape(Shape::Reference("file".into())))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("assets/a.png", "a.png", &[])]);
        let inst = instance(
            bare("gallery"),
            vec![(
                "images",
                seq(vec![InstanceValue::String("[[a.png^nope]]".into())]),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(codes_of(&diags), vec!["field-shape-mismatch"]);
        assert!(diags[0].message.contains("any*"));
    }

    #[test]
    fn file_ref_rejects_a_head_anchor_fragment() {
        // A `#head` anchor is sub-file addressing too, also rejected.
        let g = build_graph(vec![td(
            "card",
            &[],
            &[("asset", false, ref_shape("file"))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("doc.md", "doc.md", &[])]);
        let inst = instance(
            bare("card"),
            vec![("asset", InstanceValue::String("[[doc.md#section]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn file_ref_to_whole_file_is_clean() {
        // The whole-file form still resolves clean.
        let g = build_graph(vec![td(
            "card",
            &[],
            &[("asset", false, ref_shape("file"))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("doc.md", "doc.md", &[])]);
        let inst = instance(
            bare("card"),
            vec![("asset", InstanceValue::String("[[doc.md]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn any_star_resolves_any_node_without_closure_check() {
        // `any*` ([[type-def shape any::au-type-system]]) references any node by existence, no closure
        // check. An untyped asset satisfies it, where a typed `T*` would fire
        // reference-target-type-mismatch.
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("ev"),
            vec![("ref", InstanceValue::String("[[diagram.pdf]]".into()))],
        );
        assert!(
            validate_with(&g, &idx, &claims, &inst).is_empty(),
            "any* to an existing untyped asset is clean, got {:?}",
            codes_of(&validate_with(&g, &idx, &claims, &inst))
        );
    }

    #[test]
    fn any_star_missing_target_still_errors() {
        // Existence is still checked, see [[type-def shape any::au-type-system]].
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("ev"),
            vec![("ref", InstanceValue::String("[[gone]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn commit_referent_in_a_typed_slot_is_not_diagnosed() {
        // A commit-referent `[[::@sha]]` / `[[::repo@sha]]` names a COMMIT, not a
        // file. It is anchor-only, so a reference slot never diagnoses it as a
        // missing target, even with no such file in the repo. See the
        // pinned-references spec.
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[]);
        for value in ["[[::@a1b2c3d]]", "[[::peer@a1b2c3d]]"] {
            let inst = instance(
                bare("ev"),
                vec![("ref", InstanceValue::String(value.into()))],
            );
            assert!(
                validate_with(&g, &idx, &claims, &inst).is_empty(),
                "commit-referent {value} must not diagnose, got {:?}",
                codes_of(&validate_with(&g, &idx, &claims, &inst))
            );
        }
    }

    #[test]
    fn commit_referent_in_body_prose_is_not_diagnosed() {
        // A commit-referent in body prose is navigational-inert: no target to
        // resolve, so no `navigational-target-not-found`. A normal dangling
        // prose link (`[[gone]]`) WOULD fire here, so an empty result is real.
        let g = build_graph(vec![td("ev", &[], &[])]).graph;
        let inst = instance(bare("ev"), vec![]);
        let body = "See [[::@a1b2c3d]] and [[::peer@a1b2c3d]] for the commits.\n";
        assert!(
            validate_body_with(&g, &inst, body, true).is_empty(),
            "commit-referents in prose must not diagnose, got {:?}",
            codes_of(&validate_body_with(&g, &inst, body, true))
        );
    }

    #[test]
    fn any_star_block_referent_to_a_plain_block_is_clean() {
        // `any*` is existence-only, so a `^^` to a plain (untyped) block that
        // EXISTS is fine — `block-id-not-typed` is never `any*`'s to raise.
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &[])]);
        let body_sources = BTreeMap::from([(
            PathBuf::from("/v/foo.md"),
            "a plain passage\n^plain\n".to_string(),
        )]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("ev"),
            vec![("ref", InstanceValue::String("[[foo^^plain]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(
            diags.is_empty(),
            "any* ^^ to an existing plain block is clean, got {:?}",
            codes_of(&diags)
        );
    }

    #[test]
    fn any_star_block_referent_absent_is_block_id_not_found() {
        // `any*` discards types, but a `^^` demands the node exist. A body-held
        // file missing the id -> `block-id-not-found` (error), not a nav warning.
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &[])]);
        let body_sources = BTreeMap::from([(
            PathBuf::from("/v/foo.md"),
            "a plain passage\n^present\n".to_string(),
        )]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("ev"),
            vec![("ref", InstanceValue::String("[[foo^^gone]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["block-id-not-found"]
        );
    }

    #[test]
    fn any_star_navigational_absent_is_a_nav_warning() {
        // The bare-`^` mirror on `any*`: navigational, so a missing id warns.
        let g = build_graph(vec![td("ev", &[], &[("ref", false, ref_shape("any"))])]).graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &[])]);
        let body_sources = BTreeMap::from([(
            PathBuf::from("/v/foo.md"),
            "a plain passage\n^present\n".to_string(),
        )]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("ev"),
            vec![("ref", InstanceValue::String("[[foo^gone]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["navigational-block-id-not-found"]
        );
    }

    #[test]
    fn block_referent_to_a_body_held_file_missing_the_id_is_block_id_not_found() {
        // The frontmatter `^^` path with a target that HAS a body but not this id,
        // distinct from the no-body arm. block-id-not-found (error).
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let body_sources = BTreeMap::from([(
            PathBuf::from("/v/foo.md"),
            "some prose\n^present\n".to_string(),
        )]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo^^gone]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["block-id-not-found"]
        );
    }

    #[test]
    fn any_amp_interprets_inline_or_resolves_a_reference() {
        // `any&` ([[type-def shape any::au-type-system]]): the inline branch is the interpreted
        // top (a bogus `type:` fires `unknown-type-claim`), a whole-value
        // `[[...]]` resolves as `any*`.
        let g = build_graph(vec![td(
            "ev",
            &[],
            &[("slot", false, inline_or_ref_shape("any"))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("doc.md", "doc.md", &[])]);

        // Inline mapping with a bogus claim — interpreted, so the claim fires.
        let inline_inst = instance(
            bare("ev"),
            vec![(
                "slot",
                inline(
                    Some(bare("not-a-real-type")),
                    vec![("k", InstanceValue::String("v".into()))],
                ),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inline_inst)),
            vec!["unknown-type-claim"],
            "any& inline branch interprets a nested claim"
        );

        // A whole-value wikilink resolves as a reference (no closure check).
        let ref_inst = instance(
            bare("ev"),
            vec![("slot", InstanceValue::String("[[doc.md]]".into()))],
        );
        assert!(
            validate_with(&g, &idx, &claims, &ref_inst).is_empty(),
            "any& reference to an existing target is clean, got {:?}",
            codes_of(&validate_with(&g, &idx, &claims, &ref_inst))
        );

        // The reference branch still checks existence.
        let (idx2, claims2) = repo_with(&[]);
        let missing_inst = instance(
            bare("ev"),
            vec![("slot", InstanceValue::String("[[gone]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx2, &claims2, &missing_inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn compound_reference_star_intersection_with_file_branch_accepts_typed_target() {
        // `<file & note>*` against a typed-note file passes — `file`
        // branch is existence-only (satisfied by resolution), `note`
        // branch is satisfied by the target's closure.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Intersection,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("notes/foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    #[test]
    fn compound_reference_star_intersection_with_file_branch_rejects_untyped_target() {
        // `<file & note>*` against an untyped file fails — `file` branch
        // is satisfied but `note` requires the target to claim it.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Intersection,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[diagram.pdf]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(codes_of(&diags), vec!["reference-target-type-mismatch"]);
    }

    #[test]
    fn compound_reference_star_with_file_branch_still_rejects_missing_target() {
        // Resolution failure precedes the file-branch short-circuit;
        // a missing target fires reference-target-missing regardless.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Star,
                        op: CompoundRefOp::Union,
                        branches: vec!["file".into(), "note".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[ghost]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    // ----- commit-pinned shapes, `*@` ([[type-def shape suffixes::au-type-system]], pinned refs) -----

    fn pinned_file_log() -> TypeGraph {
        build_graph(vec![td(
            "log",
            &[],
            &[(
                "touched",
                false,
                Ok(Shape::Pinned(Box::new(Shape::Reference("file".into())))),
            )],
        )])
        .graph
    }

    #[test]
    fn pinned_slot_rejects_an_unpinned_value() {
        // `file*@` demands a `@commit`; a plain `[[note]]` violates the contract.
        let g = pinned_file_log();
        let (idx, claims) = repo_with(&[("note.md", "note.md", &[])]);
        let inst = instance(
            bare("log"),
            vec![("touched", InstanceValue::String("[[note]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["value-not-pinned"]
        );
    }

    #[test]
    fn pinned_file_ref_to_a_live_target_is_clean() {
        // A pin is inert: not re-resolved against the graph. A live, resolving
        // target produces no diagnostic, as does a deleted one below.
        let g = pinned_file_log();
        let (idx, claims) = repo_with(&[("note.md", "note.md", &[])]);
        let inst = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[note::@a1b2c3d]]".into()),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected clean, got {:?}", diags);
    }

    #[test]
    fn pinned_slot_with_a_non_oid_commit_fires_pinned_commit_not_oid() {
        // A `*@` slot given `[[note::@main]]`: the pin is malformed (a mutable
        // rev), so it surfaces `pinned-commit-not-oid`, not the misleading
        // `value-not-pinned` (which would claim there is no pin at all).
        let g = pinned_file_log();
        let (idx, claims) = repo_with(&[("note.md", "note.md", &[])]);
        let inst = instance(
            bare("log"),
            vec![("touched", InstanceValue::String("[[note::@main]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["pinned-commit-not-oid"]
        );
    }

    #[test]
    fn a_pin_in_a_plain_reference_slot_fires_unexpected_commit_pin() {
        // `touched: file*` is a plain reference slot, `@` requires a `*@`. A
        // pinned value there is forbidden, the mirror of `value-not-pinned`.
        let g = build_graph(vec![td(
            "log",
            &[],
            &[("touched", false, Ok(Shape::Reference("file".into())))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("note.md", "note.md", &[])]);
        let inst = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[note::@a1b2c3d]]".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["unexpected-commit-pin"]
        );
    }

    #[test]
    fn a_union_of_plain_and_pinned_accepts_both() {
        // `< file* | file*@ >`, the escape hatch for a mixed slot. A pinned value
        // routes to the `*@` branch (inert), an unpinned one to the plain `*`
        // branch. The union gap-close makes the `*@` branch visible to dispatch.
        let g = build_graph(vec![td(
            "log",
            &[],
            &[(
                "touched",
                false,
                Ok(Shape::Union(vec![
                    Shape::Reference("file".into()),
                    Shape::Pinned(Box::new(Shape::Reference("file".into()))),
                ])),
            )],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("note.md", "note.md", &[])]);

        let pinned = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[note::@a1b2c3d]]".into()),
            )],
        );
        assert!(
            validate_with(&g, &idx, &claims, &pinned).is_empty(),
            "a pinned value must match the *@ branch: {:?}",
            validate_with(&g, &idx, &claims, &pinned)
        );

        let live = instance(
            bare("log"),
            vec![("touched", InstanceValue::String("[[note]]".into()))],
        );
        assert!(
            validate_with(&g, &idx, &claims, &live).is_empty(),
            "an unpinned value must match the plain * branch: {:?}",
            validate_with(&g, &idx, &claims, &live)
        );
    }

    #[test]
    fn pinned_file_ref_to_a_deleted_target_emits_nothing() {
        // The deletion-stable floor: a pin is inert, so a missing live target
        // is neither an error nor drift, the live-resolution outcome is dropped.
        let g = pinned_file_log();
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[ghost::@a1b2c3d]]".into()),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected nothing, got {:?}", diags);
    }

    #[test]
    fn a_pin_whose_path_escapes_the_repo_is_not_softened_into_drift() {
        // The bug that opened this whole line of work: a consumer shipped
        // `[[../sibling-repo/notes/x.md::@sha]]` in a `file*@` slot and the engine
        // said nothing useful.
        //
        // The fix depends on `reference-path-escapes-repo` being ABSENT from
        // `is_pinned_live_divergence`'s list, so a pin does not suppress it. An
        // impossible address is a STRUCTURAL error, distinct from a live-resolution
        // outcome the inert pin drops. Suppressing it would silently pass an
        // un-nameable location, exactly the conflation the code exists to prevent.
        let g = pinned_file_log();
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[../sibling-repo/notes/x.md::@a1b2c3d]]".into()),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(
            codes_of(&diags),
            vec!["reference-path-escapes-repo"],
            "an impossible path in a pinned slot was softened into drift"
        );
        assert!(
            diags.iter().all(|d| d.severity == Severity::Warning),
            "expected warning severity, got {diags:?}"
        );
        assert!(
            diags[0].fix.is_some(),
            "the fix naming the `::repo` spelling did not survive the pinned slot"
        );
    }

    #[test]
    fn pinned_typed_ref_to_a_non_conforming_live_target_emits_nothing() {
        // `note*@` against a live file that is not a note would be a type
        // mismatch error on a plain ref; the pin is inert, so the live type
        // is not re-checked and nothing fires.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "log",
                &[],
                &[(
                    "touched",
                    false,
                    Ok(Shape::Pinned(Box::new(Shape::Reference("note".into())))),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("other.md", "other.md", &[])]);
        let inst = instance(
            bare("log"),
            vec![(
                "touched",
                InstanceValue::String("[[other::@a1b2c3d]]".into()),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected nothing, got {:?}", diags);
    }

    #[test]
    fn compound_reference_inline_with_wikilink_uses_compound_star_path() {
        // `<X | Y>&` slot — when value is a wikilink string, dispatch
        // delegates to the same closure-includes check as `<X | Y>*`.
        // Missing target file fires reference-target-missing.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Inline,
                        op: CompoundRefOp::Union,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let inst = instance(
            bare("ev"),
            vec![("target", InstanceValue::String("[[missing]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn compound_reference_inline_with_inline_map_routes_to_inline_value() {
        // [[type-def shape record::au-type-system]] case 3 entry via `<rationale | thesis>&` slot with an
        // inline map. The compound's branches → InlineCompat::Union;
        // identity resolution + per-field walk via the inline-value path.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Inline,
                        op: CompoundRefOp::Union,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let inst = instance(
            bare("ev"),
            vec![(
                "target",
                inline(
                    Some(bare("rationale")),
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn compound_reference_inline_intersection_with_mixin_inline_validates() {
        // `<X & Y>&` slot accepts a mixin-typed inline value covering
        // both branches.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Inline,
                        op: CompoundRefOp::Intersection,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let inst = instance(
            bare("ev"),
            vec![(
                "target",
                inline(
                    Some(list(&["rationale", "thesis"])),
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        ("claim", InstanceValue::String("c".into())),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn compound_reference_inline_with_non_string_non_map_fires_field_shape_mismatch() {
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "ev",
                &[],
                &[(
                    "target",
                    false,
                    Ok(Shape::CompoundReference {
                        mode: RefMode::Inline,
                        op: CompoundRefOp::Union,
                        branches: vec!["rationale".into(), "thesis".into()],
                    }),
                )],
            ),
        ])
        .graph;
        let inst = instance(bare("ev"), vec![("target", InstanceValue::Integer(42))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn string_or_reference_union_routes_plain_string_to_string_branch() {
        // [[type reference::au-type-system]]: non-wikilink strings continue to match the String
        // branch trivially. The precheck only fires on `[[...]]`-shaped
        // values.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("hello world".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn string_or_reference_union_routes_wikilink_to_reference_branch() {
        // [[type reference::au-type-system]]: wikilink-pattern strings skip the String branch and
        // route to the reference branch. A valid target with the
        // required closure produces no diagnostic.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("rat.md", "rat.md", &["rationale"])]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[rat]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn string_or_reference_union_wikilink_missing_target_fires_target_missing() {
        // [[type reference::au-type-system]] precheck commits to the reference path for wikilink-
        // shaped values — failed resolution surfaces the reference
        // branch's diagnostic (target-missing) instead of being
        // silently accepted by the String branch.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[anywhere]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn string_or_url_union_routes_http_value_to_url_branch() {
        // [[type reference::au-type-system]]: `^https?://`-prefixed strings commit to the Url branch.
        // A valid URL passes Url validation and the slot accepts.
        let g = build_graph(vec![td(
            "citation",
            &[],
            &[(
                "source",
                false,
                Ok(Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Url),
                ])),
            )],
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("citation"),
            vec![(
                "source",
                InstanceValue::String("https://arxiv.org/abs/2401.12345".into()),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn string_or_url_union_routes_plain_string_to_string_branch() {
        // [[type reference::au-type-system]]: non-prefix strings continue to match the String branch
        // trivially. The Url precheck only fires on `^https?://`-shaped
        // values.
        let g = build_graph(vec![td(
            "citation",
            &[],
            &[(
                "source",
                false,
                Ok(Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Url),
                ])),
            )],
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("citation"),
            vec![(
                "source",
                InstanceValue::String("Bowman & Williamson, 2024".into()),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn string_or_url_union_malformed_url_does_not_fall_through_to_string() {
        // [[type reference::au-type-system]] precheck commits to the Url path for prefix-shaped values
        // — failed Url validation surfaces FIELD_SHAPE_MISMATCH instead
        // of being silently accepted by the String branch.
        let g = build_graph(vec![td(
            "citation",
            &[],
            &[(
                "source",
                false,
                Ok(Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Url),
                ])),
            )],
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("citation"),
            vec![(
                "source",
                InstanceValue::String("https://exa mple.com".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn string_or_url_or_reference_union_routes_each_value_shape_correctly() {
        // [[type reference::au-type-system]]: wikilink, http, and plain-string discriminators coexist
        // in `<String | Url | T*>`. Each value-shape routes to its own
        // branch; the discriminators are syntactically disjoint.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Primitive(Primitive::Url),
                        Shape::Reference("rationale".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("rat.md", "rat.md", &["rationale"])]);

        // Url-prefixed value → Url branch.
        let inst_url = instance(
            bare("free"),
            vec![("v", InstanceValue::String("https://example.com".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst_url).is_empty());

        // Wikilink-shaped value → reference branch.
        let inst_ref = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[rat]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst_ref).is_empty());

        // Plain string → String branch.
        let inst_str = instance(
            bare("free"),
            vec![("v", InstanceValue::String("just text".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst_str).is_empty());
    }

    #[test]
    fn reference_first_string_second_union_still_routes_by_value_shape() {
        // Branch order doesn't change the dispatch — [[type reference::au-type-system]] routes by
        // value shape, not by source order. `<rationale* | String>`
        // with a plain string value still takes the String branch.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Primitive(Primitive::String),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("plain text".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn primitive_only_union_does_not_trigger_precheck() {
        // `<String | Number>` — no reference branch. Precheck doesn't
        // fire; existing iteration accepts any string via String.
        let g = build_graph(vec![td(
            "free",
            &[],
            &[(
                "v",
                false,
                Ok(Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Number),
                ])),
            )],
        )])
        .graph;
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[looks-like-wikilink]]".into()))],
        );
        // No reference branch → wikilink shape is irrelevant; String
        // accepts it.
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn reference_only_union_with_plain_string_falls_through_precheck() {
        // `<rationale* | thesis*>` against a non-wikilink string. The
        // precheck only runs when the value parses as a wikilink; a
        // plain string falls through to the generic any-of iteration
        // and produces field-shape-mismatch.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("rat.md", "rat.md", &["rationale"])]);
        // Wikilink to a rationale → precheck short-circuits on the
        // first matching branch.
        let good = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[rat]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &good).is_empty());
        // Plain string → no wikilink, precheck skipped; generic
        // iteration produces field-shape-mismatch.
        let bad = instance(
            bare("free"),
            vec![("v", InstanceValue::String("plain".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &bad)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn reference_only_union_with_failing_wikilink_aggregates_branch_names() {
        // `<rationale* | thesis*>` against a wikilink whose target is
        // typed `note` (matches neither branch). The precheck must
        // trigger even without a String branch and aggregate the
        // attempted branches into the failure message — otherwise the
        // user sees only "value does not match shape" and loses every
        // per-branch resolution hint.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("note", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("note-doc.md", "note-doc.md", &["note"])]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[note-doc]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(
            diags.len(),
            1,
            "expected aggregated diagnostic, got {:?}",
            diags
        );
        let msg = &diags[0].message;
        assert!(
            msg.contains("attempted:"),
            "expected aggregated 'attempted:' message; got {:?}",
            msg
        );
        assert!(
            msg.contains("rationale*") && msg.contains("thesis*"),
            "expected both branch names in message; got {:?}",
            msg
        );
    }

    #[test]
    fn multi_reference_union_aggregates_type_mismatch_failures() {
        // [[type reference::au-type-system]]: when ≥2 reference branches fail with target-type-
        // mismatch, surface ONE aggregated diagnostic naming every
        // attempted branch — not the first branch's failure (which
        // would misleadingly suggest the named branch was the only
        // option).
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td("note", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("note-doc.md", "note-doc.md", &["note"])]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[note-doc]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        // Exactly one diagnostic — aggregated.
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].code.as_str(), "field-shape-mismatch");
        // Message names BOTH attempted reference branches.
        assert!(
            diags[0].message.contains("rationale*"),
            "message should name 'rationale*': {}",
            diags[0].message
        );
        assert!(
            diags[0].message.contains("thesis*"),
            "message should name 'thesis*': {}",
            diags[0].message
        );
    }

    #[test]
    fn multi_reference_union_with_missing_target_returns_single_target_missing() {
        // [[type reference::au-type-system]]: when ≥2 reference branches all fail with target-
        // missing (the target file doesn't exist regardless of branch),
        // the failure is the same from every branch — return the first
        // to avoid N copies of the same message. Don't aggregate.
        let g = build_graph(vec![
            td("rationale", &[], &[]),
            td("thesis", &[], &[]),
            td(
                "free",
                &[],
                &[(
                    "v",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Reference("rationale".into()),
                        Shape::Reference("thesis".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("free"),
            vec![("v", InstanceValue::String("[[ghost]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn deterministic_across_field_order_permutation() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[
                ("a", false, prim(Primitive::String)),
                ("b", false, prim(Primitive::String)),
                ("c", false, prim(Primitive::String)),
            ],
        )])
        .graph;
        let inst1 = instance(bare("rec"), vec![]);
        let inst2 = instance(bare("rec"), vec![]);
        assert_eq!(validate_simple(&g, &inst1), validate_simple(&g, &inst2));
    }

    #[test]
    fn iso_date_format_strict() {
        assert!(is_iso_date("2026-04-12"));
        assert!(!is_iso_date("2026-4-12"));
        assert!(!is_iso_date("not a date"));
        assert!(!is_iso_date("2026-04-12T00:00:00"));
    }

    #[test]
    fn iso_date_rejects_impossible_calendar_values() {
        // Well-formed shape, impossible value: caught by the range check.
        assert!(!is_iso_date("2026-13-40")); // month 13, day 40
        assert!(!is_iso_date("2026-00-01")); // month 0
        assert!(!is_iso_date("2026-01-00")); // day 0
        assert!(!is_iso_date("2026-04-31")); // April has 30 days
        assert!(!is_iso_date("2026-02-29")); // 2026 is not a leap year
                                             // Leap-year boundary honoured both ways.
        assert!(is_iso_date("2024-02-29")); // 2024 is a leap year
        assert!(!is_iso_date("2100-02-29")); // century non-leap
        assert!(is_iso_date("2000-02-29")); // 400-divisible leap
    }

    #[test]
    fn iso_datetime_accepts_canonical_utc_form() {
        assert!(is_iso_datetime("2026-04-12T133045Z"));
        assert!(is_iso_datetime("2024-02-29T000000Z")); // leap day, midnight
        assert!(is_iso_datetime("2026-01-01T235959Z")); // max time-of-day
        assert!(!is_iso_datetime("2026-04-12")); // date only
        assert!(!is_iso_datetime("2026-04-12 133045Z")); // space, not T
    }

    #[test]
    fn iso_datetime_rejects_impossible_time_and_date_values() {
        assert!(!is_iso_datetime("2026-01-01T999999Z")); // all fields out of range
        assert!(!is_iso_datetime("2026-01-01T240000Z")); // hour 24
        assert!(!is_iso_datetime("2026-01-01T236000Z")); // minute 60
        assert!(!is_iso_datetime("2026-01-01T235960Z")); // second 60
        assert!(!is_iso_datetime("2026-13-01T000000Z")); // impossible date part
    }

    #[test]
    fn http_url_accepts_http_and_https() {
        assert!(is_http_url("http://example.com"));
        assert!(is_http_url("https://example.com"));
        assert!(is_http_url("https://example.com/path"));
        assert!(is_http_url("https://user:pass@example.com:8080/p?q=1#f"));
        assert!(is_http_url("https://arxiv.org/abs/2401.12345"));
    }

    #[test]
    fn http_url_rejects_other_schemes_and_malformed() {
        assert!(!is_http_url(""));
        assert!(!is_http_url("example.com"));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("mailto:foo@bar.com"));
        assert!(!is_http_url("HTTP://example.com")); // case-sensitive per spec
        assert!(!is_http_url("http://")); // empty authority
        assert!(!is_http_url("http://example.com hello")); // contains space
        assert!(!is_http_url("http://exa\tmple.com")); // control char
    }

    #[test]
    fn iso_datetime_rejects_trailing_garbage() {
        assert!(!is_iso_datetime("2026-04-12T133045Zzzz"));
        assert!(!is_iso_datetime("2026-04-12T133045Z ")); // trailing space
    }

    #[test]
    fn iso_datetime_rejects_old_colon_form() {
        assert!(!is_iso_datetime("2026-04-12T13:30:45")); // extended time, old form
        assert!(!is_iso_datetime("2026-04-12T13:30:45Z"));
        assert!(!is_iso_datetime("2026-04-12T133045")); // colon-free but missing Z
    }

    #[test]
    fn iso_datetime_rejects_fractional_seconds() {
        assert!(!is_iso_datetime("2026-04-12T133045.123Z")); // no fractional in the canonical form
        assert!(!is_iso_datetime("2026-04-12T133045.Z"));
    }

    #[test]
    fn iso_datetime_rejects_timezone_offsets() {
        assert!(!is_iso_datetime("2026-04-12T133045+0530")); // UTC-only, no offsets
        assert!(!is_iso_datetime("2026-04-12T133045+05:30"));
        assert!(!is_iso_datetime("2026-04-12T133045-0800"));
    }

    #[test]
    fn iso_datetime_requires_uppercase_zulu() {
        assert!(!is_iso_datetime("2026-04-12T133045z")); // lowercase z
        assert!(!is_iso_datetime("2026-04-12T133045")); // no marker at all
    }

    #[test]
    fn iso_datetime_rejects_short_or_long_time() {
        assert!(!is_iso_datetime("2026-04-12T1330Z")); // hh mm only, no seconds
        assert!(!is_iso_datetime("2026-04-12T13304Z")); // five-digit time
        assert!(!is_iso_datetime("2026-04-12T1330455Z")); // seven-digit time
    }

    #[test]
    fn iso_datetime_rejects_multi_byte_utf8_without_panic() {
        assert!(!is_iso_datetime("🙂🙂🙂🙂")); // 16 bytes
        assert!(!is_iso_datetime("🙂🙂🙂🙂T133045Z")); // 24 bytes
        assert!(!is_iso_datetime("🙂🙂🙂T13304")); // exactly 18 bytes, multi-byte, no panic
    }

    #[test]
    fn datetime_field_with_multi_byte_value_surfaces_as_field_shape_mismatch() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[("dt", false, prim(Primitive::DateTime))],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![("dt", InstanceValue::String("🙂🙂🙂🙂".into()))],
        );
        let diags = validate_simple(&g, &inst);
        assert_eq!(
            codes_of(&diags),
            vec!["field-shape-mismatch"],
            "multi-byte UTF-8 must produce a diagnostic, not panic"
        );
    }

    // ----- [[type-def shape enum::au-type-system]] inline closed enum -----

    #[test]
    fn enum_member_value_passes() {
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, enum_shape(&["low", "moderate", "high"]))],
        )])
        .graph;
        for member in ["low", "moderate", "high"] {
            let inst = instance(
                bare("task"),
                vec![("priority", InstanceValue::String(member.into()))],
            );
            assert!(
                validate_simple(&g, &inst).is_empty(),
                "expected '{}' to be a valid enum member",
                member
            );
        }
    }

    #[test]
    fn enum_non_member_string_fails_with_field_shape_mismatch() {
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, enum_shape(&["low", "high"]))],
        )])
        .graph;
        let inst = instance(
            bare("task"),
            vec![("priority", InstanceValue::String("urgent".into()))],
        );
        let diags = validate_simple(&g, &inst);
        assert_eq!(codes_of(&diags), vec!["field-shape-mismatch"]);
        assert!(
            diags[0].message.contains("[low, high]"),
            "expected member list in diagnostic message, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn enum_rejects_non_string_values() {
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, enum_shape(&["low", "high"]))],
        )])
        .graph;
        // Null is excluded here: per spec [[type-instance body contribution::au-type-system]], a `key:` with a
        // null value is the "filled by body" visibility marker, not a value to
        // type-check, so it never fires field-shape-mismatch. A REQUIRED null
        // field whose promised body contribution never arrives is caught by the
        // body pass (required-field-absent), see
        // `null_required_field_without_body_contribution_fires_required_field_absent`.
        for non_string in [
            InstanceValue::Integer(0),
            InstanceValue::Float(1.5),
            InstanceValue::Boolean(true),
        ] {
            let inst = instance(bare("task"), vec![("priority", non_string)]);
            assert_eq!(
                codes_of(&validate_simple(&g, &inst)),
                vec!["field-shape-mismatch"]
            );
        }
    }

    #[test]
    fn null_required_field_without_body_contribution_fires_required_field_absent() {
        // A required field written `field:` (null) is the "filled by body"
        // anchor. With no body contribution the promise breaks and the field is
        // absent, so the body pass fires required-field-absent (finding 2.13,
        // previously silent because the null key counted as "provided").
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(bare("task"), vec![("priority", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "# Notes\n\nno contribution here\n", true);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
    }

    /// [[type-instance body contribution::au-type-system]]: "a contribution whose value misses the
    /// slot shape is an error". A value arriving ONLY from the body was never
    /// compared to its slot — the frontmatter pass walks `instance.fields`,
    /// where the field is a null anchor, and a null returns early as a promise.
    /// So this file used to validate completely clean.
    #[test]
    fn body_contributed_scalar_is_shape_checked_against_its_slot() {
        let g = build_graph(vec![td(
            "reading",
            &[],
            &[("pages", false, prim(Primitive::Number))],
        )])
        .graph;
        let inst = instance(bare("reading"), vec![("pages", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "`[:pages] not a number at all`\n", true);
        assert_eq!(
            codes_of(&diags),
            vec!["field-shape-mismatch"],
            "a body-contributed String must not satisfy a Number slot: {diags:?}"
        );
    }

    /// The conforming value passes, so the check is a shape gate and not a ban
    /// on filling a non-`String` slot from the body.
    #[test]
    fn a_conforming_body_scalar_passes() {
        let g = build_graph(vec![td(
            "reading",
            &[],
            &[("pages", false, prim(Primitive::Number))],
        )])
        .graph;
        let inst = instance(bare("reading"), vec![("pages", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "`[:pages] 42`\n", true);
        assert!(
            codes_of(&diags).is_empty(),
            "a parsed number satisfies Number: {diags:?}"
        );
    }

    /// Non-mapping content at a record slot is reported, not silently accepted
    /// as an `inline_record` holding a scalar (a kind the slot never admitted).
    #[test]
    fn prose_at_a_record_slot_is_a_shape_mismatch() {
        let g = build_graph(vec![
            td("point", &[], &[("x", false, prim(Primitive::Number))]),
            td(
                "capture",
                &[],
                &[("origin", false, Ok(Shape::Record("point".into())))],
            ),
        ])
        .graph;
        let inst = instance(bare("capture"), vec![("origin", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "```[:origin]\njust prose, no type\n```\n", true);
        assert_eq!(
            codes_of(&diags),
            vec!["body-slot-shape-mismatch"],
            "prose at a record slot must be reported: {diags:?}"
        );
    }

    /// A fence to a field OUTSIDE the effective shape gets the same advisory the
    /// inline marker does — the carriers differ only by extent.
    #[test]
    fn a_fence_to_an_unknown_field_warns_like_the_marker() {
        // `body` OPTIONAL so its own absence does not fire and mask the assertion.
        let g = build_graph(vec![td(
            "note",
            &[],
            &[("body", true, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(bare("note"), vec![]);
        let diags = validate_body_with(
            &g,
            &inst,
            "```[:sumary]\ntypo in the field name\n```\n",
            true,
        );
        assert_eq!(
            codes_of(&diags),
            vec!["unknown-field-in-prose-contribution"],
            "a typo'd fence field warns, it is not silent: {diags:?}"
        );
    }

    /// A fence record must satisfy the SLOT, not merely its own claimed type.
    /// The frontmatter surface always checked this; the fence surface did not,
    /// so a union slot accepted a record of a type it never admitted.
    #[test]
    fn a_fence_record_is_checked_against_the_slot_it_fills() {
        let g = build_graph(vec![
            td("point", &[], &[("x", false, prim(Primitive::Number))]),
            td("unrelated", &[], &[("z", false, prim(Primitive::Number))]),
            td(
                "capture",
                &[],
                &[(
                    "detail",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Record("point".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let inst = instance(bare("capture"), vec![("detail", InstanceValue::Null)]);

        let good = validate_body_with(&g, &inst, "```[:detail]\ntype: point\nx: 1\n```\n", true);
        assert!(
            codes_of(&good).is_empty(),
            "a record of an admitted type passes: {good:?}"
        );

        let bad = validate_body_with(
            &g,
            &inst,
            "```[:detail]\ntype: unrelated\nz: 1\n```\n",
            true,
        );
        assert!(
            codes_of(&bad).contains(&"inline-value-type-not-compatible"),
            "a VALID record of a type the slot never admitted must be rejected: {bad:?}"
        );
    }

    /// An unresolvable fence claim is a real error, not just a hint. Every other
    /// `UnknownType` site emits `unknown-type-claim`; this path used to emit
    /// nothing, so a consumer filtering hints saw silence.
    #[test]
    fn an_unresolvable_fence_claim_errors_and_the_hint_rides_alongside() {
        let g = build_graph(vec![
            td("point", &[], &[("x", false, prim(Primitive::Number))]),
            td(
                "capture",
                &[],
                &[(
                    "detail",
                    false,
                    Ok(Shape::Union(vec![
                        Shape::Primitive(Primitive::String),
                        Shape::Record("point".into()),
                    ])),
                )],
            ),
        ])
        .graph;
        let inst = instance(bare("capture"), vec![("detail", InstanceValue::Null)]);
        let diags = validate_body_with(
            &g,
            &inst,
            "```[:detail]\ntype: production\nreplicas: 3\n```\n",
            true,
        );
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"unknown-type-claim"),
            "the underlying failure must be reported: {diags:?}"
        );
        assert!(
            codes.contains(&"body-fence-read-as-record"),
            "the hint rides alongside it, naming the cause: {diags:?}"
        );
    }

    /// A fence at a REFERENCE-ONLY slot is `check_marked_fences`'s verdict
    /// (`body-slot-shape-mismatch`, naming the real problem). The generic scalar
    /// check must not pile a second error on top, nor dump the fence body into a
    /// "not a wikilink" message. An INLINE-MARKER scalar at the same slot has no
    /// other owner, so it still reports.
    #[test]
    fn a_reference_only_slot_reports_a_fence_once_but_still_checks_a_marker() {
        let g = build_graph(vec![
            td("point", &[], &[("x", false, prim(Primitive::Number))]),
            td("capture", &[], &[("origin", false, ref_shape("point"))]),
        ])
        .graph;
        let inst = instance(bare("capture"), vec![("origin", InstanceValue::Null)]);

        let fence = validate_body_with(&g, &inst, "```[:origin]\ntype: point\nx: 1\n```\n", true);
        assert_eq!(
            codes_of(&fence),
            vec!["body-slot-shape-mismatch"],
            "the fence gets ONE diagnostic, the one that names the real problem: {fence:?}"
        );

        let marker = validate_body_with(&g, &inst, "`[:origin] not a wikilink`\n", true);
        assert_eq!(
            codes_of(&marker),
            vec!["field-shape-mismatch"],
            "an inline marker has no other owner, so it is still checked: {marker:?}"
        );
    }

    /// A body wikilink to an `any*` slot binds by EXISTENCE — `any*` relaxes the
    /// closure check ([[type-def shape any::au-type-system]]), so any resolvable node satisfies
    /// it. The frontmatter surface already special-cases `any`; the body surface
    /// did not, so a valid `[[node:field]]` false-fired `body-slot-shape-mismatch`.
    #[test]
    fn a_body_wikilink_to_an_any_slot_binds_by_existence() {
        let g = build_graph(vec![
            td("thing", &[], &[("name", true, prim(Primitive::String))]),
            td(
                "event",
                &[],
                &[("about", true, list_shape(Shape::Reference("any".into())))],
            ),
        ])
        .graph;
        // The target resolves and carries a claim whose closure has NO `any` — the
        // exact shape that tripped the missing existence-only branch.
        let (idx, claims) = repo_with(&[("acme.md", "acme.md", &["thing"])]);
        let inst = instance(bare("event"), vec![("about", InstanceValue::Null)]);
        let diags = validate_body_in_repo(&g, &idx, &claims, &inst, "# X\n\n[[acme:about]]\n");
        assert!(
            codes_of(&diags).is_empty(),
            "an any* body-fill to a real node must bind, not fire body-slot-shape-mismatch: {diags:?}"
        );
    }

    /// The fix must not over-relax: a TYPED `thing*` slot still checks the
    /// target's closure on the body surface — a wrong-type target errors, a
    /// right-type one is clean. Guards the `any` branch against becoming a
    /// blanket existence check for every reference slot.
    #[test]
    fn a_body_wikilink_to_a_typed_slot_still_checks_the_closure() {
        let g = build_graph(vec![
            td("thing", &[], &[("name", true, prim(Primitive::String))]),
            td("other", &[], &[("k", true, prim(Primitive::String))]),
            td(
                "event",
                &[],
                &[(
                    "involved",
                    true,
                    list_shape(Shape::Reference("thing".into())),
                )],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("acme.md", "acme.md", &["thing"]),
            ("misc.md", "misc.md", &["other"]),
        ]);
        let inst = instance(bare("event"), vec![("involved", InstanceValue::Null)]);

        let good = validate_body_in_repo(&g, &idx, &claims, &inst, "# X\n\n[[acme:involved]]\n");
        assert!(
            codes_of(&good).is_empty(),
            "a right-type target satisfies thing*: {good:?}"
        );

        let bad = validate_body_in_repo(&g, &idx, &claims, &inst, "# X\n\n[[misc:involved]]\n");
        assert!(
            codes_of(&bad).contains(&"body-slot-shape-mismatch"),
            "a wrong-type target must still fire on a typed slot: {bad:?}"
        );
    }

    /// A LIST slot checks its ELEMENT shape: one contribution is one element,
    /// never the whole list, so `Number[]` must not reject a bare `7`.
    #[test]
    fn a_body_scalar_at_a_list_slot_checks_the_element_shape() {
        let g = build_graph(vec![td(
            "reading",
            &[],
            &[(
                "pages",
                false,
                Ok(Shape::List {
                    inner: Box::new(Shape::Primitive(Primitive::Number)),
                    min: 0,
                    max: None,
                }),
            )],
        )])
        .graph;
        let inst = instance(bare("reading"), vec![("pages", InstanceValue::Null)]);
        assert!(
            codes_of(&validate_body_with(&g, &inst, "`[:pages] 7`\n", true)).is_empty(),
            "an element conforming to the list's inner shape passes"
        );
        assert_eq!(
            codes_of(&validate_body_with(&g, &inst, "`[:pages] seven`\n", true)),
            vec!["field-shape-mismatch"],
            "a non-conforming element is still caught"
        );
    }

    #[test]
    fn null_required_field_with_body_contribution_is_clean() {
        // The promise is kept: a body contribution to the null-anchored field
        // suppresses the diagnostic.
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(bare("task"), vec![("priority", InstanceValue::Null)]);
        let body = "# Notes\n\n`[:priority] high`\n";
        let diags = validate_body_with(&g, &inst, body, true);
        assert!(
            !codes_of(&diags).contains(&"required-field-absent"),
            "a body contribution keeps the filled-by-body promise: {:?}",
            diags
        );
    }

    #[test]
    fn null_optional_field_without_body_contribution_is_clean() {
        // Optional fields are never required, so a null optional anchor with no
        // contribution is fine.
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", true, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(bare("task"), vec![("priority", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "# Notes\n\nno contribution\n", true);
        assert!(
            !codes_of(&diags).contains(&"required-field-absent"),
            "an optional null anchor should not fire: {:?}",
            diags
        );
    }

    #[test]
    fn null_required_field_in_yaml_only_instance_fires() {
        // A yaml-only instance has no body to fill from, so a required null
        // field is unconditionally absent.
        let g = build_graph(vec![td(
            "task",
            &[],
            &[("priority", false, prim(Primitive::String))],
        )])
        .graph;
        let inst = instance(bare("task"), vec![("priority", InstanceValue::Null)]);
        let diags = validate_body_with(&g, &inst, "", false);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
    }

    #[test]
    fn embedded_block_required_field_uses_per_origin_optional_bit() {
        // A mixin `type: [alpha, beta]` inside a `[:slot]` fence: `shared` is
        // optional on the canonical (lex-min) origin `alpha` but required on
        // `beta`. The embedded-block required-field check must use the
        // per-origin required bit, not just the canonical decl, else the
        // missing `shared` is missed (finding 3.10).
        let g = build_graph(vec![
            td("alpha", &[], &[("shared", true, prim(Primitive::String))]),
            td("beta", &[], &[("shared", false, prim(Primitive::String))]),
            td("doc", &[], &[("slot", true, inline_or_ref_shape("alpha"))]),
        ])
        .graph;
        let inst = instance(bare("doc"), vec![("slot", InstanceValue::Null)]);
        let body = "# S\n\n```yaml [:slot]\ntype: [alpha, beta]\n```\n";
        let diags = validate_body_with(&g, &inst, body, true);
        assert!(
            codes_of(&diags).contains(&"embedded-record-validation-failure"),
            "the block omits `shared`, required on beta (optional on the \
             canonical alpha) — expected the failure to fire: {:?}",
            diags
        );
    }

    // ----- [[type-def shape suffixes::au-type-system]] references -----

    fn repo_with(files: &[(&str, &str, &[&str])]) -> (RepoIndex, BTreeMap<PathBuf, Vec<TypeName>>) {
        // (relpath, _basename_unused, type_claims)
        let root = PathBuf::from("/v");
        let paths: Vec<PathBuf> = files.iter().map(|(rel, _, _)| root.join(rel)).collect();
        let (idx, _) = RepoIndex::build(root.clone(), paths);
        let mut claims: BTreeMap<PathBuf, Vec<TypeName>> = BTreeMap::new();
        for (rel, _, type_claims) in files {
            if !type_claims.is_empty() {
                claims.insert(
                    root.join(rel),
                    type_claims.iter().map(|n| TypeName((*n).into())).collect(),
                );
            }
        }
        (idx, claims)
    }

    #[test]
    fn reference_to_typed_target_passes() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("notes/foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    #[test]
    fn reference_to_missing_target_fires_target_missing() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[ghost]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn reference_to_wrong_typed_target_fires_type_mismatch() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("decision", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("decisions/d.md", "d.md", &["decision"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[d]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-type-mismatch"]
        );
    }

    // ----- [[type-def shape def-ref::au-type-system]] typed type-def references -----

    fn def_ref_single(t: &str) -> Result<Shape, &'static str> {
        Ok(Shape::DefReference(Some(DefBound::Single(t.into()))))
    }

    fn def_ref_unconstrained() -> Result<Shape, &'static str> {
        Ok(Shape::DefReference(None))
    }

    /// A repo index holding the `def_ref_graph` type-def files, so a def-ref
    /// `[[type-name]]` resolves through the index like any reference.
    fn def_ref_repo() -> (RepoIndex, BTreeMap<PathBuf, Vec<TypeName>>) {
        repo_with(&[
            ("mcp.tool.type.yaml", "", &[]),
            ("mcp.tool.propose.type.yaml", "", &[]),
            ("other.type.yaml", "", &[]),
        ])
    }

    fn def_ref_graph() -> TypeGraph {
        // `mcp.tool.propose` is_a `mcp.tool`; `other` is unrelated. `mode`
        // carries a constrained slot and an unconstrained one.
        build_graph(vec![
            td("mcp.tool", &[], &[]),
            td("mcp.tool.propose", &["mcp.tool"], &[]),
            td("other", &[], &[]),
            td(
                "mode",
                &[],
                &[
                    ("propose_tool", false, def_ref_single("mcp.tool")),
                    ("any_def", true, def_ref_unconstrained()),
                ],
            ),
        ])
        .graph
    }

    #[test]
    fn def_ref_resolves_to_def_in_closure_by_type_name() {
        // `[[mcp.tool.propose]]` is the type-NAME; the repo index resolves it
        // by name, not by the `.type`-tailed file stem ([[type-def shape def-ref::au-type-system]]).
        let g = def_ref_graph();
        let (idx, claims) = def_ref_repo();
        let inst = instance(
            bare("mode"),
            vec![(
                "propose_tool",
                InstanceValue::String("[[mcp.tool.propose]]".into()),
            )],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected clean, got {:?}", diags);
    }

    #[test]
    fn def_ref_to_def_outside_closure_fires_closure_mismatch() {
        let g = def_ref_graph();
        let (idx, claims) = def_ref_repo();
        let inst = instance(
            bare("mode"),
            vec![("propose_tool", InstanceValue::String("[[other]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["def-ref-closure-mismatch"]
        );
    }

    #[test]
    fn def_ref_to_non_type_def_fires_target_not_a_type_def() {
        // The target exists but is a plain note, not a `.type.yaml` def.
        let g = def_ref_graph();
        let (idx, claims) = repo_with(&[("notes/some-note.md", "some-note.md", &[])]);
        let inst = instance(
            bare("mode"),
            vec![(
                "propose_tool",
                InstanceValue::String("[[some-note]]".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["def-ref-target-not-a-type-def"]
        );
    }

    #[test]
    fn def_ref_to_missing_target_fires_target_missing() {
        let g = def_ref_graph();
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("mode"),
            vec![("propose_tool", InstanceValue::String("[[ghost]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn def_ref_to_name_colliding_with_a_note_hints_the_explicit_path() {
        // A single-segment type-name `tool` collides with a note stem; the note
        // wins the stem rule, so the def-ref is a not-a-type-def with a hint.
        let g = build_graph(vec![
            td("tool", &[], &[]),
            td(
                "mode",
                &[],
                &[("propose_tool", false, def_ref_single("tool"))],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("tool.md", "tool.md", &[]),
            ("tool.type.yaml", "tool.type.yaml", &[]),
        ]);
        let inst = instance(
            bare("mode"),
            vec![("propose_tool", InstanceValue::String("[[tool]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert_eq!(codes_of(&diags), vec!["def-ref-target-not-a-type-def"]);
        assert!(
            diags[0].message.contains("type/tool.type.yaml"),
            "expected an explicit-path hint, got: {}",
            diags[0].message
        );
    }

    #[test]
    fn unconstrained_def_ref_accepts_any_def_but_rejects_a_note() {
        let g = def_ref_graph();
        // `type*` accepts any def, no closure check. (`propose_tool` is
        // required, so supply a valid value alongside.)
        let (idx, claims) = def_ref_repo();
        let inst = instance(
            bare("mode"),
            vec![
                (
                    "propose_tool",
                    InstanceValue::String("[[mcp.tool.propose]]".into()),
                ),
                ("any_def", InstanceValue::String("[[other]]".into())),
            ],
        );
        assert!(
            validate_with(&g, &idx, &claims, &inst).is_empty(),
            "type* accepts any type-def"
        );
        // Still must BE a def — a note is rejected.
        let (idx2, claims2) = repo_with(&[
            ("mcp.tool.type.yaml", "", &[]),
            ("mcp.tool.propose.type.yaml", "", &[]),
            ("notes/n.md", "n.md", &[]),
        ]);
        let inst2 = instance(
            bare("mode"),
            vec![
                (
                    "propose_tool",
                    InstanceValue::String("[[mcp.tool.propose]]".into()),
                ),
                ("any_def", InstanceValue::String("[[n]]".into())),
            ],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx2, &claims2, &inst2)),
            vec!["def-ref-target-not-a-type-def"]
        );
    }

    #[test]
    fn def_ref_rejects_non_wikilink_value() {
        let g = def_ref_graph();
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("mode"),
            vec![(
                "propose_tool",
                InstanceValue::String("mcp.tool.propose".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn def_ref_rejects_block_id_fragment() {
        // A def is whole-def only — a `^block-id` fragment is meaningless.
        let g = def_ref_graph();
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("mode"),
            vec![(
                "propose_tool",
                InstanceValue::String("[[mcp.tool.propose^blk]]".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn reference_target_through_parent_closure_passes() {
        // `decision.decided` is_a `decision`; a `decision*` slot accepts a
        // `decision.decided` target.
        let g = build_graph(vec![
            td("decision", &[], &[]),
            td("decision.decided", &["decision"], &[]),
            td("audit", &[], &[("subject", false, ref_shape("decision"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("decisions/x.md", "x.md", &["decision.decided"])]);
        let inst = instance(
            bare("audit"),
            vec![("subject", InstanceValue::String("[[x]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected clean, got {:?}", diags);
    }

    #[test]
    fn reference_to_ambiguous_target_fires_ambiguous() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("a/foo.md", "foo.md", &["note"]),
            ("b/foo.md", "foo.md", &["note"]),
        ]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-ambiguous"]
        );
    }

    #[test]
    fn reference_value_not_a_wikilink_fires_field_shape_mismatch() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("notes/foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("foo".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn reference_value_not_a_string_fires_field_shape_mismatch() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("notes/foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::Integer(1))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn file_reference_to_existing_asset_passes_without_closure_check() {
        // `file*` resolves any repo file by existence; no closure check.
        let g = build_graph(vec![td(
            "attachment",
            &[],
            &[("blob", false, ref_shape("file"))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("attachment"),
            vec![("blob", InstanceValue::String("[[diagram.pdf]]".into()))],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn file_reference_to_missing_asset_fires_missing() {
        let g = build_graph(vec![td(
            "attachment",
            &[],
            &[("blob", false, ref_shape("file"))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("attachment"),
            vec![("blob", InstanceValue::String("[[ghost.pdf]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    // Malformed-wikilink diagnostics surface at typed-reference slots so
    // the user sees a precise parse error instead of the generic
    // "doesn't match shape" or "target missing".

    #[test]
    fn typed_reference_with_empty_anchor_fires_wikilink_empty_anchor() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo#]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["wikilink-empty-anchor"]
        );
    }

    #[test]
    fn typed_reference_with_empty_block_id_fires_wikilink_empty_block_id() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo^]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["wikilink-empty-block-id"]
        );
    }

    #[test]
    fn typed_reference_with_reversed_delimiters_fires_wikilink_reversed() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let inst = instance(
            bare("link-card"),
            vec![(
                "target",
                InstanceValue::String("[[foo^block#section]]".into()),
            )],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["wikilink-reversed-delimiters"]
        );
    }

    #[test]
    fn typed_reference_with_field_only_wikilink_fires_wikilink_empty_target() {
        // `[[:f]]` — empty name without a locating fragment. The only
        // remaining empty-target trigger; `#head` / `^block-id` grant
        // the empty name (local form).
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[:f]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["wikilink-empty-target"]
        );
    }

    #[test]
    fn local_anchor_reference_is_a_self_reference() {
        // `[[#section]]` resolves to the host file itself; the host's own
        // claim closure satisfies the slot, and the heading exists.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "link-card",
                &["note"],
                &[("target", false, ref_shape("note"))],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("inst.md", "inst.md", &["link-card"])]);
        let body_sources =
            BTreeMap::from([(PathBuf::from("/v/inst.md"), "# Section\n".to_string())]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[#section]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn local_anchor_self_reference_type_mismatch_fires() {
        // The host file's closure misses the demanded type — the
        // self-reference fails the same check a cross-file one would.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("inst.md", "inst.md", &["link-card"])]);
        let body_sources =
            BTreeMap::from([(PathBuf::from("/v/inst.md"), "# Section\n".to_string())]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[#section]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["reference-target-type-mismatch"]
        );
    }

    #[test]
    fn slot_anchor_to_a_missing_heading_warns() {
        // The reference itself is fine; the `#head` fragment misses —
        // anchor-not-found warning, navigational like prose anchors.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let body_sources =
            BTreeMap::from([(PathBuf::from("/v/foo.md"), "# Real Heading\n".to_string())]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo#No Such]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert_eq!(codes_of(&diags), vec!["anchor-not-found"]);
        assert_eq!(diags[0].severity, Severity::Warning);
    }

    #[test]
    fn typed_reference_with_canonical_full_wikilink_resolves() {
        // `[[target#anchor^block_id]]` is the canonical full form. The
        // block-id addresses an entity inside the target ([[type block-id::au-type-system]]);
        // its claim is what the slot checks, the anchor stays navigational.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("foo.md", "foo.md", &["note"])]);
        let body_sources = BTreeMap::from([(
            PathBuf::from("/v/foo.md"),
            "# sec\n\n```yaml [:f]\ntype: note\n```\n^123\n".to_string(),
        )]);
        let record_targets = BTreeMap::new();
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[foo#sec^123]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(
            diags.is_empty(),
            "expected clean validation, got {:?}",
            diags
        );
    }

    // ----- inline-record block-id targets ([[type block-id::au-type-system]]) -----

    /// `record_targets` entry for one file: id → claim names.
    fn record_targets_for(
        path: &str,
        ids: &[(&str, &[&str])],
    ) -> BTreeMap<PathBuf, crate::record_targets::RecordTargets> {
        let targets = ids
            .iter()
            .map(|(id, claims)| {
                (
                    (*id).to_string(),
                    crate::record_targets::RecordTarget {
                        claims: claims.iter().map(|c| TypeName((*c).into())).collect(),
                        qualified: claims
                            .iter()
                            .map(|c| TypeNameClaim::parse(c, ByteRange::new(0, 0)))
                            .collect(),
                        span: ByteRange::new(0, 0),
                    },
                )
            })
            .collect();
        BTreeMap::from([(PathBuf::from(path), targets)])
    }

    #[test]
    fn typed_reference_to_inline_record_checks_the_records_claim() {
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("edge", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["edge"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["node"])]);
        let inst = instance(
            bare("edge"),
            vec![("from", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn typed_reference_to_inline_record_with_wrong_claim_fires_mismatch() {
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("other", &[], &[]),
            td("edge", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["edge"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["other"])]);
        let inst = instance(
            bare("edge"),
            vec![("from", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["reference-target-type-mismatch"]
        );
    }

    #[test]
    fn local_reference_resolves_to_a_host_record() {
        // The canvas pattern: `from: "[[^^n1]]"` against a record in the
        // same file.
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("canvas", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("inst.md", "inst.md", &["canvas"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/inst.md", &[("n1", &["node"])]);
        let inst = instance(
            bare("canvas"),
            vec![("from", InstanceValue::String("[[^^n1]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn dangling_local_block_id_falls_back_to_the_host_file() {
        // `[[^missing]]` in a `node*` slot: a bare `^id` is navigational, so the
        // HOST file is the referent (Shape 2, decision 2607061142). The host
        // claims `canvas`, not `node`, so the FILE fails the slot —
        // `reference-target-type-mismatch`. No body is held for the host here, so
        // the navigational anchor is silent.
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("canvas", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("inst.md", "inst.md", &["canvas"])]);
        let inst = instance(
            bare("canvas"),
            vec![("from", InstanceValue::String("[[^missing]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-type-mismatch"]
        );
    }

    #[test]
    fn bare_caret_to_a_typed_block_checks_the_file_not_the_block() {
        // Shape 2: a bare `^` is navigational, so even a TYPED block is not the
        // referent — the FILE is. The file claims `node` and satisfies the slot,
        // so it is clean, though the block itself claims `edge` (which `^^` rejects).
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("edge", &[], &[]),
            td("host", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["node"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["edge"])]);
        let inst = instance(
            bare("host"),
            vec![("from", InstanceValue::String("[[canvas^n1]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(
            diags.is_empty(),
            "bare ^ should check the file (node), got {diags:?}"
        );
    }

    #[test]
    fn block_referent_to_the_same_typed_block_checks_the_block() {
        // The `^^` mirror: the BLOCK is the referent, and it claims `edge`, not
        // `node` — a mismatch, even though the file would satisfy the slot.
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("edge", &[], &[]),
            td("host", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["node"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &["edge"])]);
        let inst = instance(
            bare("host"),
            vec![("from", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with_targets(
                &g,
                &idx,
                &claims,
                &body_sources,
                &record_targets,
                &inst
            )),
            vec!["reference-target-type-mismatch"]
        );
    }

    #[test]
    fn block_referent_to_a_no_body_target_is_block_id_not_found() {
        // `^^` demands a block value, but the target has no body and no record
        // with the id — the block does not exist, so `block-id-not-found` (error),
        // and the file type-check is skipped (there is no referent to check).
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("host", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("target.md", "target.md", &["node"])]);
        let inst = instance(
            bare("host"),
            vec![("from", InstanceValue::String("[[target^^ghost]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["block-id-not-found"]
        );
    }

    #[test]
    fn bare_caret_to_a_no_body_target_falls_back_to_the_file() {
        // The bare `^` mirror: an unverifiable anchor over a no-body target, so
        // the FILE is the referent, silently. The file satisfies `node`, so it is
        // clean (a `^^` here would be block-id-not-found).
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("host", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("target.md", "target.md", &["node"])]);
        let inst = instance(
            bare("host"),
            vec![("from", InstanceValue::String("[[target^ghost]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(
            diags.is_empty(),
            "bare ^ to a no-body target should be clean, got {diags:?}"
        );
    }

    #[test]
    fn claimless_record_target_skips_the_typed_check() {
        // The record exists but carries no claim — diagnosed at the
        // target file, never double-fired at the reference site.
        let g = build_graph(vec![
            td("node", &[], &[]),
            td("edge", &[], &[("from", false, ref_shape("node"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("canvas.md", "canvas.md", &["edge"])]);
        let body_sources = BTreeMap::new();
        let record_targets = record_targets_for("/v/canvas.md", &[("n1", &[])]);
        let inst = instance(
            bare("edge"),
            vec![("from", InstanceValue::String("[[canvas^^n1]]".into()))],
        );
        let diags = validate_with_targets(&g, &idx, &claims, &body_sources, &record_targets, &inst);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn block_id_wikilink_in_a_string_slot_is_just_a_string() {
        // Resolution is gated to reference slots ([[type reference::au-type-system]]): a
        // primitive slot holding a wikilink-shaped string never block-id
        // resolves. Pre-fix this fired spurious block-id-not-found.
        let g = build_graph(vec![td(
            "card",
            &[],
            &[("title", false, prim(Primitive::String))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[]);
        let inst = instance(
            bare("card"),
            vec![("title", InstanceValue::String("[[foo^bar]]".into()))],
        );
        let diags = validate_with(&g, &idx, &claims, &inst);
        assert!(diags.is_empty(), "expected clean, got {diags:?}");
    }

    #[test]
    fn reference_to_typeless_target_fires_type_mismatch() {
        // An asset (no `type:` claim) can't satisfy a typed reference.
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("link-card", &[], &[("target", false, ref_shape("note"))]),
        ])
        .graph;
        let (idx, claims) = repo_with(&[("assets/diagram.pdf", "diagram.pdf", &[])]);
        let inst = instance(
            bare("link-card"),
            vec![("target", InstanceValue::String("[[diagram.pdf]]".into()))],
        );
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-type-mismatch"]
        );
    }

    // ----- [[type-def shape suffixes::au-type-system]] lists -----

    #[test]
    fn non_empty_list_rejects_empty_sequence() {
        // [[type-def shape suffixes::au-type-system]]: `T[+]` slot with `[]` value fires
        // `field-shape-mismatch` — committed to the non-empty contract.
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                non_empty_list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(bare("rec"), vec![("tags", seq(vec![]))]);
        let diags = validate_simple(&g, &inst);
        assert_eq!(codes_of(&diags), vec!["field-shape-mismatch"]);
        assert!(diags[0].message.contains("requires at least 1"));
    }

    #[test]
    fn non_empty_list_accepts_single_element() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                non_empty_list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![("tags", seq(vec![InstanceValue::String("only".into())]))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn list_of_strings_passes_when_all_strings() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![(
                "tags",
                seq(vec![
                    InstanceValue::String("a".into()),
                    InstanceValue::String("b".into()),
                ]),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn list_with_off_shape_element_fires_per_element() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![(
                "tags",
                seq(vec![
                    InstanceValue::String("a".into()),
                    InstanceValue::Integer(42),
                    InstanceValue::Boolean(true),
                ]),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "field-shape-mismatch")
                .count(),
            2
        );
    }

    #[test]
    fn list_value_not_a_sequence_fires_field_shape_mismatch() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![("tags", InstanceValue::String("not a list".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn list_of_references_validates_each_element() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td(
                "collection",
                &[],
                &[("items", false, list_shape(Shape::Reference("note".into())))],
            ),
        ])
        .graph;
        let (idx, claims) = repo_with(&[
            ("notes/foo.md", "foo.md", &["note"]),
            ("notes/bar.md", "bar.md", &["note"]),
        ]);
        let inst = instance(
            bare("collection"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::String("[[foo]]".into()),
                    InstanceValue::String("[[bar]]".into()),
                ]),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn list_of_file_refs_validates_existence_only() {
        let g = build_graph(vec![td(
            "gallery",
            &[],
            &[("images", false, list_shape(Shape::Reference("file".into())))],
        )])
        .graph;
        let (idx, claims) = repo_with(&[
            ("assets/a.png", "a.png", &[]),
            ("assets/b.png", "b.png", &[]),
        ]);
        let inst = instance(
            bare("gallery"),
            vec![(
                "images",
                seq(vec![
                    InstanceValue::String("[[a.png]]".into()),
                    InstanceValue::String("[[b.png]]".into()),
                ]),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn nested_list_of_strings_validates_recursively() {
        // String[][]: each outer element must be a sequence; each inner
        // element must be a string.
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "matrix",
                false,
                list_shape(Shape::List {
                    inner: Box::new(Shape::Primitive(Primitive::String)),
                    min: 0,
                    max: None,
                }),
            )],
        )])
        .graph;
        let inst = instance(
            bare("rec"),
            vec![(
                "matrix",
                seq(vec![
                    seq(vec![
                        InstanceValue::String("a".into()),
                        InstanceValue::String("b".into()),
                    ]),
                    seq(vec![InstanceValue::Integer(1)]),
                ]),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // Single inner mismatch: the integer in the second sub-list.
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "field-shape-mismatch")
                .count(),
            1
        );
    }

    #[test]
    fn empty_list_value_passes_against_list_shape() {
        let g = build_graph(vec![td(
            "rec",
            &[],
            &[(
                "tags",
                false,
                list_shape(Shape::Primitive(Primitive::String)),
            )],
        )])
        .graph;
        let inst = instance(bare("rec"), vec![("tags", seq(vec![]))]);
        assert!(validate_simple(&g, &inst).is_empty());
    }

    /// Like `td(...)` but with a non-empty `sealed:` list. The branch
    /// names just have to be syntactically valid; load checks aren't
    /// re-run here, so the branches don't need to exist in the graph
    /// for `is_sealed(name)` to return true.
    fn td_sealed(
        name: &str,
        parents: &[&str],
        fields: &[(&str, bool, Result<Shape, &str>)],
        sealed_branches: &[&str],
    ) -> TypeDef {
        let mut t = td(name, parents, fields);
        t.sealed = sealed_branches
            .iter()
            .map(|b| TypeNameClaim::own(TypeName((*b).into()), ByteRange::new(0, 0)))
            .collect();
        t
    }

    /// Like `td(...)` but with `abstract: true` declared.
    fn td_abstract(
        name: &str,
        parents: &[&str],
        fields: &[(&str, bool, Result<Shape, &str>)],
    ) -> TypeDef {
        let mut t = td(name, parents, fields);
        t.declared_abstract = true;
        t
    }

    #[test]
    fn abstract_type_claimed_fires_on_bare_claim() {
        // A declared-abstract, non-sealed type is non-claimable.
        let g = build_graph(vec![
            td_abstract("pane", &[], &[("title", false, prim(Primitive::String))]),
            td("pane.split", &["pane"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("pane"),
            vec![("title", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["abstract-type-claimed"]
        );
    }

    #[test]
    fn concrete_subtype_of_abstract_parent_validates_clean() {
        // The abstract parent is open; a concrete subtype is claimable.
        let g = build_graph(vec![
            td_abstract("pane", &[], &[("title", false, prim(Primitive::String))]),
            td("pane.split", &["pane"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("pane.split"),
            vec![("title", InstanceValue::String("x".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn abstract_in_mixin_fires_only_for_the_abstract_element() {
        // Per-claim: a concrete sibling does not excuse the abstract claim, and
        // only the abstract element fires (parallel to sealed).
        let g = build_graph(vec![
            td_abstract("pane", &[], &[("title", false, prim(Primitive::String))]),
            td("note", &[], &[("body", false, prim(Primitive::String))]),
        ])
        .graph;
        let inst = instance(
            TypeClaim::List {
                items: vec![
                    TypeNameClaim::own(TypeName("note".into()), ByteRange::new(0, 0)),
                    TypeNameClaim::own(TypeName("pane".into()), ByteRange::new(0, 0)),
                ],
                value_span: ByteRange::new(0, 0),
            },
            vec![
                ("title", InstanceValue::String("x".into())),
                ("body", InstanceValue::String("y".into())),
            ],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["abstract-type-claimed"]
        );
    }

    #[test]
    fn sealed_with_explicit_abstract_fires_sealed_only_no_double() {
        // Sealed is the more specific non-claimability; an explicit
        // `abstract: true` on it must NOT double-fire abstract-type-claimed.
        let mut decision = td_sealed(
            "decision",
            &[],
            &[("summary", false, prim(Primitive::String))],
            &["decision.pending"],
        );
        decision.declared_abstract = true;
        let g = build_graph(vec![decision, td("decision.pending", &["decision"], &[])]).graph;
        let inst = instance(
            bare("decision"),
            vec![("summary", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["sealed-parent-claimed"]
        );
    }

    #[test]
    fn inline_value_at_abstract_slot_without_type_fires_missing_type() {
        // A slot-pinned inline record at an abstract ceiling must declare an
        // explicit concrete `type:`, the same demand a sealed ceiling makes.
        let g = build_graph(vec![
            td_abstract("pane", &[], &[("title", false, prim(Primitive::String))]),
            td("host", &[], &[("child", false, record_shape("pane"))]),
        ])
        .graph;
        let inst = instance(bare("host"), vec![("child", inline(None, vec![]))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["inline-value-missing-type"]
        );
    }

    #[test]
    fn inline_value_declaring_abstract_type_fires_abstract_type_claimed() {
        // An explicit inline `type:` naming an abstract type is a direct claim.
        let g = build_graph(vec![
            td_abstract("pane", &[], &[("title", false, prim(Primitive::String))]),
            td("pane.split", &["pane"], &[]),
            td("host", &[], &[("child", false, record_shape("pane"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "child",
                inline(
                    Some(bare("pane")),
                    vec![("title", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"abstract-type-claimed"), "got {:?}", codes);
    }

    #[test]
    fn sealed_parent_claimed_fires_on_bare_claim() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td("decision.decided", &["decision"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("decision"),
            vec![("summary", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["sealed-parent-claimed"]
        );
    }

    #[test]
    fn sealed_intermediate_in_nested_sum_fires() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed", "decision.decided.reverted"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td("decision.decided.reverted", &["decision.decided"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("decision.decided"),
            vec![("summary", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["sealed-parent-claimed"]
        );
    }

    #[test]
    fn non_sealed_leaf_does_not_fire() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed", "decision.decided.reverted"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td("decision.decided.reverted", &["decision.decided"], &[]),
        ])
        .graph;
        let inst = instance(
            bare("decision.decided.committed"),
            vec![("summary", InstanceValue::String("x".into()))],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn sealed_root_with_only_fields_fires() {
        let g = build_graph(vec![
            td_sealed(
                "source",
                &[],
                &[("title", false, prim(Primitive::String))],
                &["source.url", "source.path"],
            ),
            td(
                "source.url",
                &["source"],
                &[("external_url", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            bare("source"),
            vec![("title", InstanceValue::String("x".into()))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["sealed-parent-claimed"]
        );
    }

    #[test]
    fn sealed_in_mixin_fires_only_for_sealed_element() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending"],
            ),
            td("decision.pending", &["decision"], &[]),
            td(
                "maturity",
                &[],
                &[("level", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["decision", "maturity"]),
            vec![
                ("summary", InstanceValue::String("x".into())),
                ("level", InstanceValue::String("y".into())),
            ],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        // Exactly one sealed-parent-claimed (for `decision`).
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "sealed-parent-claimed")
                .count(),
            1,
            "got {:?}",
            codes
        );
    }

    #[test]
    fn sealed_intermediate_in_mixin_fires() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.decided"],
            ),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td(
                "maturity",
                &[],
                &[("level", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst = instance(
            list(&["decision.decided", "maturity"]),
            vec![
                ("summary", InstanceValue::String("x".into())),
                ("level", InstanceValue::String("y".into())),
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
            "got {:?}",
            codes
        );
    }

    #[test]
    fn duplicate_sealed_claim_fires_warning_plus_error_per_occurrence() {
        // `claim.iter()` visits each list element, so a repeated sealed
        // name fires sealed-parent-claimed once per occurrence. The
        // duplicate-claim warning surfaces the repetition itself; once
        // the user collapses, exactly one sealed-parent-claimed remains.
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending"],
            ),
            td("decision.pending", &["decision"], &[]),
        ])
        .graph;
        let inst = instance(
            list(&["decision", "decision"]),
            vec![("summary", InstanceValue::String("x".into()))],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(
            codes.iter().filter(|c| **c == "duplicate-claim").count(),
            1,
            "got {:?}",
            codes
        );
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "sealed-parent-claimed")
                .count(),
            2,
            "got {:?}",
            codes
        );
    }

    #[test]
    fn unknown_type_claim_short_circuits_sealed_check() {
        // `effective_shape` errors with UnknownType before the sealed
        // pass — verify the sealed check doesn't fire on the unknown.
        let g = build_graph(vec![td_sealed("decision", &[], &[], &["decision.pending"])]).graph;
        let inst = instance(bare("foo-missing"), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert_eq!(codes, vec!["unknown-type-claim"]);
    }

    #[test]
    fn sealed_claim_order_is_commutative_in_diagnostics() {
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending"],
            ),
            td("decision.pending", &["decision"], &[]),
            td(
                "maturity",
                &[],
                &[("level", false, prim(Primitive::String))],
            ),
        ])
        .graph;
        let inst_ab = instance(
            list(&["decision", "maturity"]),
            vec![
                ("summary", InstanceValue::String("x".into())),
                ("level", InstanceValue::String("y".into())),
            ],
        );
        let inst_ba = instance(
            list(&["maturity", "decision"]),
            vec![
                ("summary", InstanceValue::String("x".into())),
                ("level", InstanceValue::String("y".into())),
            ],
        );
        let diags_ab = validate_simple(&g, &inst_ab);
        let diags_ba = validate_simple(&g, &inst_ba);
        let mut codes_ab: Vec<&str> = codes_of(&diags_ab);
        let mut codes_ba: Vec<&str> = codes_of(&diags_ba);
        codes_ab.sort();
        codes_ba.sort();
        assert_eq!(codes_ab, codes_ba);
    }

    // ----- [[type-def shape record::au-type-system]] case 1 — inline values at non-sealed, non-union slots -----

    /// Build an `InstanceValue::Mapping(InlineValue { ... })` from a
    /// claim plus field name/value pairs. Mirrors `instance(...)` for
    /// inline-value testing.
    fn inline(claim: Option<TypeClaim>, fields: Vec<(&str, InstanceValue)>) -> InstanceValue {
        InstanceValue::Mapping(InlineValue {
            type_claim: claim,
            block_id: None,
            fields: fields
                .into_iter()
                .map(|(k, v)| InstanceField {
                    key: k.into(),
                    key_span: ByteRange::new(0, 0),
                    value: v,
                    value_span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                })
                .collect(),
            doc: None,
            field_docs: Default::default(),
        })
    }

    #[test]
    fn inline_value_omitted_type_validates_against_slot_demand() {
        // [[type-def shape record::au-type-system]] case 1: inline value at a non-sealed record slot with
        // `type:` omitted. Identity is the slot's demand; required
        // fields must still be present.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[
                    ("description", false, prim(Primitive::String)),
                    ("evidence", true, prim(Primitive::String)),
                ],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_value_missing_required_field_fires_required_field_absent() {
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        // Inline value omits the required `description`.
        let inst = instance(bare("host"), vec![("body", inline(None, vec![]))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["required-field-absent"]
        );
    }

    // ---- inline records with a divergent field ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) ----

    /// A host whose `body: c` slot pins a type `c` that inherits a DIVERGENT
    /// `f` from two parents (`a` String, `b` Number). The inline record's shape
    /// therefore carries a divergent field, exercising the inline qualifier path.
    fn divergent_inline_graph() -> TypeGraph {
        build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
            td("c", &["a", "b"], &[]), // divergent inherited `f`, legal at the type-def
            td("host", &[], &[("body", false, record_shape("c"))]),
        ])
        .graph
    }

    #[test]
    fn inline_record_bare_divergent_field_fires_collision_plus_required() {
        // A bare inline key to a divergent REQUIRED field: collision plus a
        // required-field-absent per unfilled origin (a bare value fills none).
        let g = divergent_inline_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(None, vec![("f", InstanceValue::String("x".into()))]),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"mixin-collision"), "got {codes:?}");
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "required-field-absent")
                .count(),
            2,
            "got {codes:?}"
        );
    }

    #[test]
    fn inline_record_qualified_divergent_field_resolves() {
        let g = divergent_inline_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![
                        ("f{a}", InstanceValue::String("x".into())),
                        ("f{b}", InstanceValue::Integer(1)),
                    ],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        assert!(
            diags.is_empty(),
            "both origins qualified and valid must be clean: {:?}",
            codes_of(&diags)
        );
    }

    #[test]
    fn inline_record_qualified_divergent_value_checks_its_origin_shape() {
        // `f{b}` binds to b's Number; a string there is a field-shape-mismatch,
        // proving the inline qualifier resolves per-origin.
        let g = divergent_inline_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![
                        ("f{a}", InstanceValue::String("x".into())),
                        ("f{b}", InstanceValue::String("not a number".into())),
                    ],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"field-shape-mismatch"), "got {codes:?}");
    }

    #[test]
    fn inline_value_missing_required_field_hints_a_near_miss() {
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        // `descriptn` is an undeclared near-miss of the required `description`.
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(None, vec![("descriptn", InstanceValue::String("x".into()))]),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let d = diags
            .iter()
            .find(|d| d.code.as_str() == "required-field-absent")
            .expect("required-field-absent");
        assert!(
            d.message.contains("did you mean 'descriptn'"),
            "expected a near-miss hint on the inline path, got: {}",
            d.message
        );
        assert!(
            d.related.len() >= 2,
            "expected an added related span at the near-miss key, got {:?}",
            d.related
        );
    }

    #[test]
    fn inline_value_declared_type_matches_slot_no_diagnostic() {
        // Declaring the slot's exact demand is redundant but allowed.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("rationale")),
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_value_declared_subtype_satisfies_slot() {
        // [[type-def shape record::au-type-system]] case 1: declared `type:` may name a subtype of the slot's
        // demand. The declared closure includes the slot's demand, so it
        // satisfies the compatibility check.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "rationale-strict",
                &["rationale"],
                &[("strength", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("rationale-strict")),
                    vec![
                        ("description", InstanceValue::String("ok".into())),
                        ("strength", InstanceValue::String("high".into())),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_value_declared_type_unrelated_fires_not_compatible() {
        // [[type-def shape record::au-type-system]] case 1: declared closure does not include the slot's
        // demand → `inline-value-type-not-compatible`.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("thesis")),
                    vec![("claim", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"inline-value-type-not-compatible"),
            "got {:?}",
            codes
        );
    }

    #[test]
    fn inline_value_unknown_declared_type_fires_unknown_type_claim() {
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![("body", inline(Some(bare("ghost")), vec![]))],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["unknown-type-claim"]
        );
    }

    #[test]
    fn inline_value_extras_pass_through() {
        // [[type extras::au-type-system]]: top-level extras pass; same rule applies inside an
        // inline value.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![
                        ("description", InstanceValue::String("ok".into())),
                        ("not-in-closure", InstanceValue::Integer(7)),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn nested_inline_values_validate_recursively() {
        // [[type-def shape record::au-type-system]]: an inline value can contain inline-record fields. Both
        // levels validate via the same dispatch.
        let g = build_graph(vec![
            td(
                "evidence",
                &[],
                &[("source", false, prim(Primitive::String))],
            ),
            td(
                "rationale",
                &[],
                &[
                    ("description", false, prim(Primitive::String)),
                    ("evidence", false, record_shape("evidence")),
                ],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        // Two-deep inline: host -> rationale -> evidence. All required
        // fields present at both levels → clean.
        let good = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        (
                            "evidence",
                            inline(None, vec![("source", InstanceValue::String("s".into()))]),
                        ),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &good).is_empty());

        // Inner level missing required field → required-field-absent
        // fires at the inner span.
        let bad = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        ("evidence", inline(None, vec![])),
                    ],
                ),
            )],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &bad)),
            vec!["required-field-absent"]
        );
    }

    #[test]
    fn inline_value_declared_sealed_type_fires_sealed_parent_claimed() {
        // Universal [[type-def sealed::au-type-system]] rule: claiming a sealed type-def
        // directly is a validation error wherever the claim appears.
        // Even at a non-sealed slot, an inline `type: <sealed>` fires
        // sealed-parent-claimed.
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td("decision.decided", &["decision"], &[]),
            // Slot demand is `decision` itself, but we'll exercise the
            // compatibility-check / sealed-claim interaction by claiming
            // sealed `decision` directly inline. Compatibility check
            // passes (declared closure trivially includes itself); the
            // sealed-claim rule fires.
            td("host", &[], &[("body", false, record_shape("decision"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("decision")),
                    vec![("summary", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"sealed-parent-claimed"), "got {:?}", codes);
    }

    // ----- [[type-def shape record::au-type-system]] case 2 — inline values at sealed-parent slots -----

    /// Build a sealed `decision` family with one non-sealed leaf and a
    /// nested sealed intermediate. Hosts a single record-typed slot
    /// demanding `decision`.
    fn sealed_decision_host_graph() -> TypeGraph {
        build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td("host", &[], &[("choice", false, record_shape("decision"))]),
        ])
        .graph
    }

    #[test]
    fn inline_value_at_sealed_slot_without_type_fires_missing_type() {
        let g = sealed_decision_host_graph();
        let inst = instance(bare("host"), vec![("choice", inline(None, vec![]))]);
        // No required-field-absent — the missing-type rule short-circuits.
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["inline-value-missing-type"]
        );
    }

    #[test]
    fn inline_value_at_sealed_slot_with_sealed_parent_fires_sealed_parent_claimed() {
        let g = sealed_decision_host_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "choice",
                inline(
                    Some(bare("decision")),
                    vec![("summary", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"sealed-parent-claimed"), "got {:?}", codes);
    }

    #[test]
    fn inline_value_at_sealed_slot_with_sealed_intermediate_fires() {
        // [[type-def sealed::au-type-system]] nested sums — `decision.decided` is itself sealed, so
        // claiming it inline still fires `sealed-parent-claimed`.
        let g = sealed_decision_host_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "choice",
                inline(
                    Some(bare("decision.decided")),
                    vec![("summary", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"sealed-parent-claimed"), "got {:?}", codes);
    }

    #[test]
    fn inline_value_at_sealed_slot_with_non_sealed_leaf_validates() {
        let g = sealed_decision_host_graph();
        let inst = instance(
            bare("host"),
            vec![(
                "choice",
                inline(
                    Some(bare("decision.pending")),
                    vec![("summary", InstanceValue::String("x".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn typed_reference_slot_rejects_inline_map_with_field_shape_mismatch() {
        // Regression lock: `name*` (Shape::Reference) is wikilink-only
        // per [[type-def shape suffixes::au-type-system]] / [[type reference::au-type-system]]. An inline map at a typed-reference slot
        // must fire `field-shape-mismatch` — NOT route through
        // validate_inline_value.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, ref_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn inline_or_reference_slot_accepts_wikilink() {
        // `rationale&` (Shape::InlineOrReference) accepts both inline
        // maps and wikilink strings. Verify the wikilink path delegates
        // to `check_reference` and fires reference diagnostics for bad
        // targets, not field-shape-mismatch.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "host",
                &[],
                &[("body", false, inline_or_ref_shape("rationale"))],
            ),
        ])
        .graph;
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
        let claims = BTreeMap::new();
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                InstanceValue::String("[[missing-rationale]]".into()),
            )],
        );
        // Wikilink dispatch routes to check_reference; missing target
        // fires reference-target-missing.
        assert_eq!(
            codes_of(&validate_with(&g, &idx, &claims, &inst)),
            vec!["reference-target-missing"]
        );
    }

    #[test]
    fn inline_or_reference_slot_accepts_inline_map() {
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "host",
                &[],
                &[("body", false, inline_or_ref_shape("rationale"))],
            ),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_or_reference_slot_rejects_other_shapes() {
        // Numbers, lists, booleans aren't valid at `name&` slots.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "host",
                &[],
                &[("body", false, inline_or_ref_shape("rationale"))],
            ),
        ])
        .graph;
        let inst = instance(bare("host"), vec![("body", InstanceValue::Integer(42))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["field-shape-mismatch"]
        );
    }

    #[test]
    fn inline_value_at_non_sealed_slot_omitted_type_no_missing_type_diag() {
        // Regression: the missing-type rule must NOT fire when the slot
        // is non-sealed (case 1 default applies).
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("host", &[], &[("body", false, record_shape("rationale"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    None,
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    // ----- [[type-def shape record::au-type-system]] case 3 / 4 — inline values at union / intersection slots -----

    /// Build a graph with `rationale` and `thesis` (disjoint records),
    /// and a host with one configurable union or intersection slot.
    fn record_union_host_graph(host_field_shape: Shape) -> TypeGraph {
        build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            TypeDef {
                shape: None,
                name: TypeName("host".into()),
                source_path: PathBuf::from("/v/host.type.yaml"),
                source_span: ByteRange::new(0, 0),
                parent_claim: None,
                parents: vec![],
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(host_field_shape),
                    doc: None,
                }],
                sealed: vec![],
                declared_abstract: false,
                meta_blocks: None,
                required_meta: Vec::new(),
                body: None,
                doc: None,
                ..Default::default()
            },
        ])
        .graph
    }

    fn union_of_records(names: &[&str]) -> Shape {
        Shape::Union(names.iter().map(|n| Shape::Record((*n).into())).collect())
    }

    fn intersection_of_records(names: &[&str]) -> Shape {
        Shape::Intersection(names.iter().map(|n| Shape::Record((*n).into())).collect())
    }

    #[test]
    fn inline_at_union_slot_without_type_fires_missing_type() {
        let g = record_union_host_graph(union_of_records(&["rationale", "thesis"]));
        let inst = instance(bare("host"), vec![("body", inline(None, vec![]))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["inline-value-missing-type"]
        );
    }

    #[test]
    fn inline_at_union_slot_with_matching_branch_validates() {
        let g = record_union_host_graph(union_of_records(&["rationale", "thesis"]));
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("rationale")),
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_at_union_slot_with_no_matching_branch_fires_not_compatible() {
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            // Add a third type unrelated to either branch.
            td(
                "evidence",
                &[],
                &[("source", false, prim(Primitive::String))],
            ),
            TypeDef {
                shape: None,
                name: TypeName("host".into()),
                source_path: PathBuf::from("/v/host.type.yaml"),
                source_span: ByteRange::new(0, 0),
                parent_claim: None,
                parents: vec![],
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(union_of_records(&["rationale", "thesis"])),
                    doc: None,
                }],
                sealed: vec![],
                declared_abstract: false,
                meta_blocks: None,
                required_meta: Vec::new(),
                body: None,
                doc: None,
                ..Default::default()
            },
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("evidence")),
                    vec![("source", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"inline-value-type-not-compatible"),
            "got {:?}",
            codes
        );
    }

    #[test]
    fn inline_at_union_slot_with_sealed_branch_intermediate_fires() {
        // [[type-def sealed::au-type-system]] nested sums on a union slot: declared `type: decision`
        // (sealed) at a `<decision | thesis>` slot triggers the
        // universal sealed-claim rule.
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[("summary", false, prim(Primitive::String))],
                &["decision.pending"],
            ),
            td("decision.pending", &["decision"], &[]),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            TypeDef {
                shape: None,
                name: TypeName("host".into()),
                source_path: PathBuf::from("/v/host.type.yaml"),
                source_span: ByteRange::new(0, 0),
                parent_claim: None,
                parents: vec![],
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(union_of_records(&["decision", "thesis"])),
                    doc: None,
                }],
                sealed: vec![],
                declared_abstract: false,
                meta_blocks: None,
                required_meta: Vec::new(),
                body: None,
                doc: None,
                ..Default::default()
            },
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("decision")),
                    vec![("summary", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"sealed-parent-claimed"), "got {:?}", codes);
    }

    #[test]
    fn inline_at_intersection_slot_without_type_fires_missing_type() {
        let g = record_union_host_graph(intersection_of_records(&["rationale", "thesis"]));
        let inst = instance(bare("host"), vec![("body", inline(None, vec![]))]);
        assert_eq!(
            codes_of(&validate_simple(&g, &inst)),
            vec!["inline-value-missing-type"]
        );
    }

    #[test]
    fn inline_at_intersection_slot_with_mixin_covering_branches_validates() {
        let g = record_union_host_graph(intersection_of_records(&["rationale", "thesis"]));
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(list(&["rationale", "thesis"])),
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        ("claim", InstanceValue::String("c".into())),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_at_intersection_slot_missing_branch_fires_not_compatible() {
        let g = record_union_host_graph(intersection_of_records(&["rationale", "thesis"]));
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("rationale")),
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"inline-value-type-not-compatible"),
            "got {:?}",
            codes
        );
        // Missing branch should be named in the message.
        assert!(
            diags.iter().any(|d| d.message.contains("thesis")),
            "missing branch 'thesis' should appear in message: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_at_intersection_slot_with_single_subtype_satisfying_both() {
        // A subtype whose closure includes both intersection branches
        // satisfies the intersection without mixin.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            // `combo` extends both branches.
            td("combo", &["rationale", "thesis"], &[]),
            TypeDef {
                shape: None,
                name: TypeName("host".into()),
                source_path: PathBuf::from("/v/host.type.yaml"),
                source_span: ByteRange::new(0, 0),
                parent_claim: None,
                parents: vec![],
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(intersection_of_records(&["rationale", "thesis"])),
                    doc: None,
                }],
                sealed: vec![],
                declared_abstract: false,
                meta_blocks: None,
                required_meta: Vec::new(),
                body: None,
                doc: None,
                ..Default::default()
            },
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(bare("combo")),
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        ("claim", InstanceValue::String("c".into())),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    #[test]
    fn inline_at_intersection_with_mixin_collision_fires() {
        // [[type-def fields collision - auto-unify and qualified field::au-type-system]]: two mixin ancestors declare the same field with non-
        // token-equal shapes. The mixin-collision rule fires at the
        // inline span from the divergent set, the same per-origin path
        // used at file-level.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("note", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("note", false, prim(Primitive::Number))]),
            TypeDef {
                shape: None,
                name: TypeName("host".into()),
                source_path: PathBuf::from("/v/host.type.yaml"),
                source_span: ByteRange::new(0, 0),
                parent_claim: None,
                parents: vec![],
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: "".into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(intersection_of_records(&["rationale", "thesis"])),
                    doc: None,
                }],
                sealed: vec![],
                declared_abstract: false,
                meta_blocks: None,
                required_meta: Vec::new(),
                body: None,
                doc: None,
                ..Default::default()
            },
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(list(&["rationale", "thesis"])),
                    vec![("note", InstanceValue::String("x".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"mixin-collision"), "got {:?}", codes);
    }

    // ----- inline-site coverage backfill -----

    #[test]
    fn duplicate_claim_warning_fires_at_inline_site() {
        // Inline `type: [a, a]` — same redundant-claim rule as
        // file-level. Reuses `check_redundant_claims` via the same
        // helper path.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td_one_field_helper("host", "body", Shape::Record("rationale".into())),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(list(&["rationale", "rationale"])),
                    vec![("description", InstanceValue::String("ok".into()))],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"duplicate-claim"),
            "expected duplicate-claim at inline site; got {:?}",
            codes
        );
    }

    #[test]
    fn subsumption_in_mixin_warning_fires_at_inline_site() {
        // Inline `type: [a, a-strict]` where a-strict extends a — the
        // wider claim is implied by the narrower ([[type-def shape compound::au-type-system]]-symmetric rule).
        // Reuses the same shared helper as file-level.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td(
                "rationale-strict",
                &["rationale"],
                &[("strength", false, prim(Primitive::String))],
            ),
            td_one_field_helper("host", "body", Shape::Record("rationale".into())),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(list(&["rationale", "rationale-strict"])),
                    vec![
                        ("description", InstanceValue::String("ok".into())),
                        ("strength", InstanceValue::String("high".into())),
                    ],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"subsumption-in-mixin"),
            "expected subsumption-in-mixin at inline site; got {:?}",
            codes
        );
    }

    #[test]
    fn list_of_inline_records_validates_per_element() {
        // `Shape::List(Shape::Record("rationale"))` — list slot whose
        // elements are inline maps. Per-element dispatch through
        // `check_list` → `check_value_against_shape` → Record arm →
        // `validate_inline_value`. Required-field-absent on a bad
        // element fires at that element's span, not the whole list.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td_one_field_helper(
                "host",
                "items",
                Shape::List {
                    inner: Box::new(Shape::Record("rationale".into())),
                    min: 0,
                    max: None,
                },
            ),
        ])
        .graph;
        // All elements valid → no diagnostics.
        let good = instance(
            bare("host"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::Mapping(InlineValue {
                        type_claim: None,
                        block_id: None,
                        fields: vec![InstanceField {
                            key: "description".into(),
                            key_span: ByteRange::new(0, 0),
                            value: InstanceValue::String("a".into()),
                            value_span: ByteRange::new(0, 0),
                            nav_links: Vec::new(),
                        }],
                        doc: None,
                        field_docs: Default::default(),
                    }),
                    InstanceValue::Mapping(InlineValue {
                        type_claim: None,
                        block_id: None,
                        fields: vec![InstanceField {
                            key: "description".into(),
                            key_span: ByteRange::new(0, 0),
                            value: InstanceValue::String("b".into()),
                            value_span: ByteRange::new(0, 0),
                            nav_links: Vec::new(),
                        }],
                        doc: None,
                        field_docs: Default::default(),
                    }),
                ]),
            )],
        );
        assert!(validate_simple(&g, &good).is_empty());

        // One element missing required field → required-field-absent
        // fires once at that element's inline span.
        let bad = instance(
            bare("host"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::Mapping(InlineValue {
                        type_claim: None,
                        block_id: None,
                        fields: vec![InstanceField {
                            key: "description".into(),
                            key_span: ByteRange::new(0, 0),
                            value: InstanceValue::String("a".into()),
                            value_span: ByteRange::new(0, 0),
                            nav_links: Vec::new(),
                        }],
                        doc: None,
                        field_docs: Default::default(),
                    }),
                    InstanceValue::Mapping(InlineValue {
                        type_claim: None,
                        block_id: None,
                        fields: vec![],
                        doc: None,
                        field_docs: Default::default(),
                    }),
                ]),
            )],
        );
        assert_eq!(
            codes_of(&validate_simple(&g, &bad)),
            vec!["required-field-absent"]
        );
    }

    #[test]
    fn list_of_inline_or_reference_accepts_mixed_elements() {
        // `rationale&[]` — list of inline-or-reference. Each element
        // dispatches independently: Mapping → validate_inline_value;
        // wikilink string → check_reference. Mixed list validates if
        // every element resolves.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td_one_field_helper(
                "host",
                "items",
                Shape::List {
                    inner: Box::new(Shape::InlineOrReference("rationale".into())),
                    min: 0,
                    max: None,
                },
            ),
        ])
        .graph;
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), vec![PathBuf::from("/v/rat.md")]);
        let mut claims = BTreeMap::new();
        claims.insert(
            PathBuf::from("/v/rat.md"),
            vec![TypeName("rationale".into())],
        );
        let inst = instance(
            bare("host"),
            vec![(
                "items",
                seq(vec![
                    InstanceValue::Mapping(InlineValue {
                        type_claim: Some(bare("rationale")),
                        block_id: None,
                        fields: vec![InstanceField {
                            key: "description".into(),
                            key_span: ByteRange::new(0, 0),
                            value: InstanceValue::String("inline form".into()),
                            value_span: ByteRange::new(0, 0),
                            nav_links: Vec::new(),
                        }],
                        doc: None,
                        field_docs: Default::default(),
                    }),
                    InstanceValue::String("[[rat]]".into()),
                ]),
            )],
        );
        assert!(validate_with(&g, &idx, &claims, &inst).is_empty());
    }

    #[test]
    fn compound_amp_intersection_with_single_name_subtype_validates() {
        // `<rationale & thesis>&` slot, inline `type: combo` where
        // combo extends both branches. Single-name claim (not mixin)
        // covering both intersection branches via inheritance.
        // Existing tests cover the mixin-claim form; this locks the
        // single-name path.
        let g = build_graph(vec![
            td(
                "rationale",
                &[],
                &[("description", false, prim(Primitive::String))],
            ),
            td("thesis", &[], &[("claim", false, prim(Primitive::String))]),
            // `combo` extends both; closure includes rationale + thesis.
            td("combo", &["rationale", "thesis"], &[]),
            td_one_field_helper(
                "host",
                "target",
                Shape::CompoundReference {
                    mode: RefMode::Inline,
                    op: CompoundRefOp::Intersection,
                    branches: vec!["rationale".into(), "thesis".into()],
                },
            ),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "target",
                inline(
                    Some(bare("combo")),
                    vec![
                        ("description", InstanceValue::String("d".into())),
                        ("claim", InstanceValue::String("c".into())),
                    ],
                ),
            )],
        );
        assert!(validate_simple(&g, &inst).is_empty());
    }

    /// Local helper for inline-site backfill tests — builds a one-field
    /// type-def with an arbitrary Shape. Mirrors `td_one_field` from
    /// au-testkit but lives here to avoid cross-crate test imports.
    fn td_one_field_helper(name: &str, field: &str, shape: Shape) -> TypeDef {
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim: None,
            parents: vec![],
            fields: vec![FieldDecl {
                name: FieldName(field.into()),
                optional: false,
                raw_shape: "".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(shape),
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

    // ----- meta-body validation -----

    /// Attach a populated `meta:` block list to an existing TypeDef. None of
    /// the test helpers carry meta by default; this composes meta onto them.
    fn with_meta(mut t: TypeDef, blocks: Vec<MetaBlock>) -> TypeDef {
        t.meta_blocks = Some(blocks);
        t
    }

    /// Build a meta sub-region with body fields. Spans are zero — diagnostics
    /// match on code + presence rather than on byte ranges.
    fn meta_block(type_name: &str, fields: Vec<(&str, InstanceValue)>) -> MetaBlock {
        MetaBlock {
            type_name: TypeName(type_name.into()),
            repo: None,
            type_name_span: ByteRange::new(0, 0),
            block_span: ByteRange::new(0, 0),
            fields: fields
                .into_iter()
                .map(|(k, v)| InstanceField {
                    key: k.into(),
                    key_span: ByteRange::new(0, 0),
                    value: v,
                    value_span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                })
                .collect(),
            body_span: ByteRange::new(0, 0),
            doc: None,
            field_docs: Default::default(),
        }
    }

    /// Run meta-body validation against `graph`. Most meta tests don't
    /// exercise knowledge-base-aware shapes — empty knowledge base + claims is enough.
    fn validate_meta_simple(graph: &TypeGraph) -> Vec<Diagnostic> {
        let (idx, _) = RepoIndex::build(PathBuf::from("/v"), Vec::<PathBuf>::new());
        let claims: BTreeMap<PathBuf, Vec<TypeName>> = BTreeMap::new();
        let body_sources = BTreeMap::new();
        let record_targets = BTreeMap::new();
        let ref_data = MapRefData {
            claims_by_path: &claims,
            body_sources: &body_sources,
            record_targets: &record_targets,
        };
        let ctx = ValidateContext {
            graph,
            repo_index: &idx,
            ref_data: &ref_data,
            cross_repo: None,
            resolution: None,
            meta_marker: None,
        };
        validate_meta_bodies(&ctx)
    }

    #[test]
    fn meta_body_clean_when_all_required_present() {
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block(
                    "display-meta",
                    vec![("tldr", InstanceValue::String("a decision".into()))],
                )],
            ),
        ])
        .graph;
        assert!(validate_meta_simple(&g).is_empty());
    }

    // A meta type is a single claim but can inherit a DIVERGENT field from its
    // own parents ([[type-def fields collision - auto-unify and qualified field::au-type-system]]), so a meta sub-region reaches the
    // divergent model the frontmatter / inline surfaces do.
    fn meta_divergent_graph(block: MetaBlock) -> TypeGraph {
        build_graph(vec![
            td("a", &[], &[("f", false, prim(Primitive::String))]),
            td("b", &[], &[("f", false, prim(Primitive::Number))]),
            td("dm", &["a", "b"], &[]), // meta type with a divergent inherited `f`
            with_meta(td("decision", &[], &[]), vec![block]),
        ])
        .graph
    }

    #[test]
    fn meta_body_bare_divergent_field_fires_collision_plus_required() {
        let g = meta_divergent_graph(meta_block(
            "dm",
            vec![("f", InstanceValue::String("x".into()))],
        ));
        let diags = validate_meta_simple(&g);
        let codes = codes_of(&diags);
        assert!(codes.contains(&"mixin-collision"), "got {codes:?}");
        assert_eq!(
            codes
                .iter()
                .filter(|c| **c == "required-field-absent")
                .count(),
            2,
            "got {codes:?}"
        );
    }

    #[test]
    fn meta_body_qualified_divergent_field_resolves() {
        let g = meta_divergent_graph(meta_block(
            "dm",
            vec![
                ("f{a}", InstanceValue::String("x".into())),
                ("f{b}", InstanceValue::Integer(1)),
            ],
        ));
        let diags = validate_meta_simple(&g);
        assert!(diags.is_empty(), "got {:?}", codes_of(&diags));
    }

    #[test]
    fn meta_body_missing_required_field_fires_required_field_absent() {
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block("display-meta", vec![])],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
        assert!(diags[0].message.contains("'tldr'"));
        // Message names the host TypeDef so the user knows which file to edit.
        assert!(diags[0].message.contains("'decision'"));
    }

    #[test]
    fn meta_body_missing_required_field_hints_a_near_miss() {
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            with_meta(
                td("decision", &[], &[]),
                // `tldrr` is a near-miss of the required `tldr`.
                vec![meta_block(
                    "display-meta",
                    vec![("tldrr", InstanceValue::String("x".into()))],
                )],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        let d = diags
            .iter()
            .find(|d| d.code.as_str() == "required-field-absent")
            .expect("required-field-absent");
        assert!(
            d.message.contains("did you mean 'tldrr'"),
            "expected a near-miss hint on the meta-body path, got: {}",
            d.message
        );
    }

    #[test]
    fn meta_body_field_shape_mismatch_fires() {
        // String required, Number provided.
        let g = build_graph(vec![
            td(
                "runtime-meta",
                &[],
                &[("version", false, prim(Primitive::Number))],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block(
                    "runtime-meta",
                    vec![("version", InstanceValue::String("not-a-number".into()))],
                )],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "field-shape-mismatch"));
    }

    #[test]
    fn meta_body_unknown_meta_type_fires_unknown_type_claim() {
        let g = build_graph(vec![with_meta(
            td("decision", &[], &[]),
            vec![meta_block("nonexistent-meta", vec![])],
        )])
        .graph;
        let diags = validate_meta_simple(&g);
        assert_eq!(codes_of(&diags), vec!["unknown-type-claim"]);
        assert!(diags[0].message.contains("'nonexistent-meta'"));
        // The host name is in the message so the diagnostic points at the
        // type-def carrying the broken meta.
        assert!(diags[0].message.contains("'decision'"));
    }

    #[test]
    fn meta_body_sealed_meta_type_fires_sealed_parent_claimed() {
        // Sealed meta-type-def; a host claiming the sealed parent fires the
        // universal [[type-def sealed::au-type-system]] rule at the meta site. Same code as the file-level
        // and inline-value rules, new fire surface.
        let g = build_graph(vec![
            td_sealed("base-meta", &[], &[], &["base-meta.x", "base-meta.y"]),
            td("base-meta.x", &["base-meta"], &[]),
            td("base-meta.y", &["base-meta"], &[]),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block("base-meta", vec![])],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "sealed-parent-claimed"));
    }

    #[test]
    fn meta_body_empty_with_all_optional_meta_type_clean() {
        // Spec [[type-def meta::au-type-system]]: an empty sub-region body (`- type: x` with no further
        // keys) is just an empty declaration of x — valid only if x's fields
        // are all optional. Locks the case-distinction from `meta: []`.
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[
                    ("tldr", true, prim(Primitive::String)),
                    ("icon", true, prim(Primitive::String)),
                ],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block("display-meta", vec![])],
            ),
        ])
        .graph;
        assert!(validate_meta_simple(&g).is_empty());
    }

    #[test]
    fn meta_body_empty_with_required_field_fires_required_field_absent() {
        // Counterpart to the all-optional case above. Empty body + required
        // field → required-field-absent (not suppression — that would
        // require `meta: []` at the host level).
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block("display-meta", vec![])],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
    }

    #[test]
    fn meta_body_inherits_required_field_from_parent_meta_type() {
        // Closure walks the meta-type-def's parents. A required field
        // declared on the parent is required on the child unless the
        // body provides it. Locks that effective_shape works the same at
        // a meta site as at any other site.
        let g = build_graph(vec![
            td(
                "base-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            td("display-meta", &["base-meta"], &[]),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block("display-meta", vec![])],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
        // Message names the ORIGINATING type-def for the missing field, not
        // the leaf — mirrors instance-validation behavior.
        assert!(diags[0].message.contains("'base-meta'"));
    }

    #[test]
    fn meta_body_validates_every_typedef_in_graph() {
        // Multiple hosts, each with a meta block. The pass iterates every
        // TypeDef in the graph; one bad body shouldn't suppress the others.
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", false, prim(Primitive::String))],
            ),
            with_meta(
                td("good-host", &[], &[]),
                vec![meta_block(
                    "display-meta",
                    vec![("tldr", InstanceValue::String("ok".into()))],
                )],
            ),
            with_meta(
                td("bad-host", &[], &[]),
                vec![meta_block("display-meta", vec![])],
            ),
        ])
        .graph;
        let diags = validate_meta_simple(&g);
        assert_eq!(codes_of(&diags), vec!["required-field-absent"]);
        // bad-host is the one with the missing field.
        assert!(diags[0].message.contains("'bad-host'"));
    }

    #[test]
    fn meta_body_extras_pass_through() {
        // Spec [[type extras::au-type-system]]: extras (body keys outside the closure) pass silently.
        // Same open-world rule as instance / inline-value validation.
        let g = build_graph(vec![
            td(
                "display-meta",
                &[],
                &[("tldr", true, prim(Primitive::String))],
            ),
            with_meta(
                td("decision", &[], &[]),
                vec![meta_block(
                    "display-meta",
                    vec![
                        ("tldr", InstanceValue::String("note".into())),
                        (
                            "not-in-closure",
                            InstanceValue::String("just an extra".into()),
                        ),
                    ],
                )],
            ),
        ])
        .graph;
        assert!(validate_meta_simple(&g).is_empty());
    }

    // ----- multi-leaf-in-sealed-family ([[type-instance type::au-type-system]]) -----

    fn nested_decision_graph() -> TypeGraph {
        // Mirror the spec [[type-def sealed::au-type-system]] nested-sums example:
        //   decision (sealed) → decision.pending / decision.decided
        //   decision.decided (sealed) → .committed / .reverted
        // plus a disjoint sealed family `source` and a non-sealed `maturity`.
        build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed", "decision.decided.reverted"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td("decision.decided.reverted", &["decision.decided"], &[]),
            td_sealed("source", &[], &[], &["source.url", "source.path"]),
            td("source.url", &["source"], &[]),
            td("source.path", &["source"], &[]),
            td("maturity", &[], &[]),
        ])
        .graph
    }

    #[test]
    fn multi_leaf_two_siblings_of_outer_sealed_fires() {
        // `[decision.pending, decision.decided.committed]` — both descend
        // from sealed `decision`. Fire once at `decision`.
        let g = nested_decision_graph();
        let inst = instance(
            list(&["decision.pending", "decision.decided.committed"]),
            vec![],
        );
        let diags = validate_simple(&g, &inst);
        let multi: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .collect();
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].severity, Severity::Error);
        assert!(multi[0].message.contains("'decision'"));
        assert!(multi[0].message.contains("'decision.pending'"));
        assert!(multi[0].message.contains("'decision.decided.committed'"));
    }

    #[test]
    fn multi_leaf_two_siblings_of_inner_sealed_fires_at_innermost() {
        // `[decision.decided.committed, decision.decided.reverted]` —
        // both bucket under `decision.decided` AND `decision`. Innermost-
        // only suppression: fire once at `decision.decided`, not at the
        // outer `decision` (which would have an identical leaf set).
        let g = nested_decision_graph();
        let inst = instance(
            list(&["decision.decided.committed", "decision.decided.reverted"]),
            vec![],
        );
        let diags = validate_simple(&g, &inst);
        let multi: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .collect();
        assert_eq!(multi.len(), 1);
        assert!(multi[0].message.contains("'decision.decided'"));
        // The outer `decision` is suppressed (same leaf set as the inner).
        assert!(
            !multi[0].message.contains("'decision': "),
            "outer decision should not be named: {:?}",
            multi[0].message
        );
    }

    #[test]
    fn multi_leaf_three_leaves_fires_at_both_outer_and_inner() {
        // `[pending, decided.committed, decided.reverted]` —
        // `decision` bucket: {pending, committed, reverted} (3 leaves)
        // `decision.decided` bucket: {committed, reverted}    (2 leaves)
        // Leaf sets differ → both fire (outer is NOT suppressed).
        let g = nested_decision_graph();
        let inst = instance(
            list(&[
                "decision.pending",
                "decision.decided.committed",
                "decision.decided.reverted",
            ]),
            vec![],
        );
        let diags = validate_simple(&g, &inst);
        let multi: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .collect();
        assert_eq!(multi.len(), 2);
        // One names `decision`, one names `decision.decided`.
        let families: Vec<&str> = multi
            .iter()
            .map(|d| {
                if d.message.contains("'decision.decided'") {
                    "decision.decided"
                } else {
                    "decision"
                }
            })
            .collect();
        assert!(families.contains(&"decision"));
        assert!(families.contains(&"decision.decided"));
    }

    #[test]
    fn multi_leaf_disjoint_families_fire_independently() {
        // `[decision.pending, source.url, decision.decided.committed, source.path]`
        // — two distinct sealed families violated. Two diagnostics.
        let g = nested_decision_graph();
        let inst = instance(
            list(&[
                "decision.pending",
                "source.url",
                "decision.decided.committed",
                "source.path",
            ]),
            vec![],
        );
        let diags = validate_simple(&g, &inst);
        let multi: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .collect();
        assert_eq!(multi.len(), 2);
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

    #[test]
    fn multi_leaf_disjoint_with_non_sealed_does_not_fire() {
        // `[decision.pending, maturity]` — `decision` bucket has one leaf,
        // `maturity` isn't sealed. No diagnostic.
        let g = nested_decision_graph();
        let inst = instance(list(&["decision.pending", "maturity"]), vec![]);
        let diags = validate_simple(&g, &inst);
        assert!(diags
            .iter()
            .all(|d| d.code.as_str() != "multi-leaf-in-sealed-family"));
    }

    #[test]
    fn multi_leaf_single_leaf_does_not_fire() {
        let g = nested_decision_graph();
        let inst = instance(bare("decision.decided.committed"), vec![]);
        let diags = validate_simple(&g, &inst);
        assert!(diags
            .iter()
            .all(|d| d.code.as_str() != "multi-leaf-in-sealed-family"));
    }

    #[test]
    fn multi_leaf_with_sealed_claim_does_not_fire_only_one_leaf_present() {
        // `[decision, decision.decided.committed]` — `decision` fires
        // `sealed-parent-claimed` (not a leaf). decision.decided.committed
        // is the only non-sealed leaf in the claim list. Multi-leaf has
        // nothing to bucket against.
        let g = nested_decision_graph();
        let inst = instance(list(&["decision", "decision.decided.committed"]), vec![]);
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            codes.contains(&"sealed-parent-claimed"),
            "sealed-parent-claimed should fire for 'decision': {:?}",
            codes
        );
        assert!(
            !codes.contains(&"multi-leaf-in-sealed-family"),
            "multi-leaf should not fire: {:?}",
            codes
        );
    }

    #[test]
    fn multi_leaf_fires_on_inline_value_identity_claim() {
        // [[type-def shape record::au-type-system]]: an inline value at a `decision`-typed slot
        // (sealed) carrying `type: [decision.pending,
        // decision.decided.committed]` violates the multi-leaf rule
        // INSIDE the inline value. Host file's own claim is clean.
        let g = build_graph(vec![
            td_sealed(
                "decision",
                &[],
                &[],
                &["decision.pending", "decision.decided"],
            ),
            td("decision.pending", &["decision"], &[]),
            td_sealed(
                "decision.decided",
                &["decision"],
                &[],
                &["decision.decided.committed", "decision.decided.reverted"],
            ),
            td("decision.decided.committed", &["decision.decided"], &[]),
            td("decision.decided.reverted", &["decision.decided"], &[]),
            td("host", &[], &[("body", false, record_shape("decision"))]),
        ])
        .graph;
        let inst = instance(
            bare("host"),
            vec![(
                "body",
                inline(
                    Some(list(&["decision.pending", "decision.decided.committed"])),
                    vec![],
                ),
            )],
        );
        let diags = validate_simple(&g, &inst);
        let multi: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "multi-leaf-in-sealed-family")
            .collect();
        assert_eq!(multi.len(), 1, "got {:?}", codes_of(&diags));
        assert!(multi[0].message.contains("'decision'"));
        assert!(multi[0].message.contains("'decision.pending'"));
        assert!(multi[0].message.contains("'decision.decided.committed'"));
    }

    #[test]
    fn multi_leaf_subsumption_pair_does_not_fire() {
        // `[decision.decided, decision.decided.committed]` — `decision.decided`
        // is sealed (separate diagnostic). Only one non-sealed leaf in the
        // claim. Multi-leaf does NOT fire. Subsumption warning still fires
        // via the existing redundant-claim helper.
        let g = nested_decision_graph();
        let inst = instance(
            list(&["decision.decided", "decision.decided.committed"]),
            vec![],
        );
        let diags = validate_simple(&g, &inst);
        let codes = codes_of(&diags);
        assert!(
            !codes.contains(&"multi-leaf-in-sealed-family"),
            "multi-leaf should not fire on a subsumption pair: {:?}",
            codes
        );
    }

    #[test]
    fn cross_repo_satisfaction_requires_full_closure_match() {
        // Two repos, each `myType (type: foo)`, but `foo` differs across them.
        // `myType`'s own def-alone hash matches (parent CONTENT is excluded), yet
        // the effective shapes differ. A cross-repo `myType*` reference must NOT
        // be satisfied by the divergent target: the check compares the full
        // closure, not just the demanded type's local hash.
        let graph_a = build_graph(vec![
            td("foo", &[], &[("x", false, prim(Primitive::String))]),
            td("myType", &["foo"], &[]),
        ])
        .graph;
        let graph_b_divergent = build_graph(vec![
            td("foo", &[], &[("x", false, prim(Primitive::Number))]),
            td("myType", &["foo"], &[]),
        ])
        .graph;

        let claims = [TypeName("myType".into())];
        assert!(
            !target_closure_includes_cross_repo(&graph_a, &graph_b_divergent, &claims, "myType"),
            "a divergent `foo` in the target repo must make the cross-repo `myType` \
             reference unsound, so it is rejected"
        );

        // Control: a target whose whole closure matches IS satisfied.
        let graph_b_matching = build_graph(vec![
            td("foo", &[], &[("x", false, prim(Primitive::String))]),
            td("myType", &["foo"], &[]),
        ])
        .graph;
        assert!(
            target_closure_includes_cross_repo(&graph_a, &graph_b_matching, &claims, "myType"),
            "matching closures must still satisfy the cross-repo reference"
        );
    }

    // Field-axis companion to `cross_repo_satisfaction_requires_full_closure_match`.
    //
    // Cross-repo reference satisfaction compares the REFERENCED closure (the
    // `closure_id` fold over `referenced_closure_of`), not just the parent
    // closure. The def-local canonical hash encodes a field's type as its rendered
    // NAME token only, so a same-named FIELD type that diverges in CONTENT across
    // repos shares its token but not its own hash. A parent-only walk never reaches
    // the field type and would wrongly deem the reference satisfied; the referenced
    // closure surfaces the divergence, for both a record-typed field (`b: bar`) and
    // a reference-typed field (`b: bar*`).
    #[test]
    fn cross_repo_reference_must_reject_field_type_divergence() {
        let claims = [TypeName("myType".into())];

        // Record-typed field `b: bar`.
        let a_rec = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::String))]),
            td("myType", &[], &[("b", false, record_shape("bar"))]),
        ])
        .graph;
        let b_rec = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::Number))]),
            td("myType", &[], &[("b", false, record_shape("bar"))]),
        ])
        .graph;
        let rec_caught = !target_closure_includes_cross_repo(&a_rec, &b_rec, &claims, "myType");

        // Reference-typed field `b: bar*`.
        let a_ref = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::String))]),
            td("myType", &[], &[("b", false, ref_shape("bar"))]),
        ])
        .graph;
        let b_ref = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::Number))]),
            td("myType", &[], &[("b", false, ref_shape("bar"))]),
        ])
        .graph;
        let ref_caught = !target_closure_includes_cross_repo(&a_ref, &b_ref, &claims, "myType");

        // Sanity control: a PARENT divergence IS caught (the existing behavior).
        let a_par = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::String))]),
            td("myType", &["bar"], &[]),
        ])
        .graph;
        let b_par = build_graph(vec![
            td("bar", &[], &[("x", false, prim(Primitive::Number))]),
            td("myType", &["bar"], &[]),
        ])
        .graph;
        let par_caught = !target_closure_includes_cross_repo(&a_par, &b_par, &claims, "myType");

        assert!(par_caught, "sanity: parent-axis divergence must be caught");
        assert!(
            rec_caught,
            "a divergent record-typed field type must make the cross-repo \
             reference unsound, so it is rejected"
        );
        assert!(
            ref_caught,
            "a divergent reference-typed field type must make the cross-repo \
             reference unsound, so it is rejected"
        );
    }

    #[test]
    fn cross_repo_reference_rejects_transitive_field_type_divergence() {
        // Divergence two field-hops deep: myType -b-> bar -c-> baz, and `baz`
        // diverges across repos. The parent-only walk never reached `bar`, let
        // alone `baz`; the referenced closure follows both field edges, so the
        // cross-repo reference is correctly rejected. Guards against a fix that
        // only looked one field-level deep.
        let claims = [TypeName("myType".into())];
        let a = build_graph(vec![
            td("baz", &[], &[("x", false, prim(Primitive::String))]),
            td("bar", &[], &[("c", false, record_shape("baz"))]),
            td("myType", &[], &[("b", false, record_shape("bar"))]),
        ])
        .graph;
        let b = build_graph(vec![
            td("baz", &[], &[("x", false, prim(Primitive::Number))]),
            td("bar", &[], &[("c", false, record_shape("baz"))]),
            td("myType", &[], &[("b", false, record_shape("bar"))]),
        ])
        .graph;
        assert!(
            !target_closure_includes_cross_repo(&a, &b, &claims, "myType"),
            "divergence at field depth 2 must make the cross-repo reference unsound"
        );

        // Control: identical at every depth still satisfies.
        let b_same = build_graph(vec![
            td("baz", &[], &[("x", false, prim(Primitive::String))]),
            td("bar", &[], &[("c", false, record_shape("baz"))]),
            td("myType", &[], &[("b", false, record_shape("bar"))]),
        ])
        .graph;
        assert!(
            target_closure_includes_cross_repo(&a, &b_same, &claims, "myType"),
            "an identical transitive closure must still satisfy the reference"
        );
    }

    #[test]
    fn cross_repo_reference_with_cyclic_field_types_terminates_and_compares() {
        // myType -o-> other -back-> myType, a reference-field cycle. The
        // referenced-closure walk must terminate on the cycle AND still reach
        // `other`, whose own field `v` carries the divergence. Equal closures
        // satisfy; a divergence inside the cycle rejects. Exercises cycle-safety
        // on the live cross-repo path, not just the closure unit test.
        let claims = [TypeName("myType".into())];
        let mk = |v: Result<Shape, &'static str>| {
            build_graph(vec![
                td("myType", &[], &[("o", false, ref_shape("other"))]),
                td(
                    "other",
                    &[],
                    &[("back", false, ref_shape("myType")), ("v", false, v)],
                ),
            ])
            .graph
        };
        let a = mk(prim(Primitive::String));
        let b_same = mk(prim(Primitive::String));
        let b_diff = mk(prim(Primitive::Number));
        assert!(
            target_closure_includes_cross_repo(&a, &b_same, &claims, "myType"),
            "cyclic field types with equal closures must satisfy"
        );
        assert!(
            !target_closure_includes_cross_repo(&a, &b_diff, &claims, "myType"),
            "a divergence inside the reference cycle must be caught (the parent-only \
             walk never reached `other`)"
        );
    }
}
