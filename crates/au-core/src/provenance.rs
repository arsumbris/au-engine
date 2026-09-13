//! Per-field effective values with provenance per spec [[type value container::au-type-system]].
//!
//! Every field on an instance is materialized as a list of `ValueContainer`s.
//! Each container carries one unique value plus the `Contribution`s that
//! produced it — frontmatter, wikilink, marked fence, or inline code.
//! Equal values from multiple surfaces collapse into one container per
//! [[type value container::au-type-system]]; ordering follows source-position per [[type value container::au-type-system]].
//!
//! This module is the value model that body-typing validation consumes (fills
//! contracts, cardinality across surfaces, frontmatter visibility).

use std::collections::BTreeMap;
use std::path::PathBuf;

use au_diagnostics::ByteRange;
use au_parser::{derive_section_paths, BodyEvent};
use au_references::{
    extract_field_marker, looks_like_wikilink, parse_wikilink, parse_wikilink_inner,
    WikilinkParseError,
};

use crate::instance::{Instance, InstanceField, InstanceValue, SequenceElement};
use crate::typedef::FieldName;
use crate::validate::{parse_qualified_key, QualifiedKey};

/// Split a body attribution's raw field text (`field` or `field{type}` /
/// `field{type::repo}`) into its base [`FieldName`] and optional
/// [`QualifiedOrigin`] ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). The parsers upstream (au-references,
/// the inline-marker check) validate well-formedness, so a `Malformed` result
/// degrades to a bare field name rather than dropping the contribution.
fn parse_attribution(raw: &str) -> (FieldName, Option<QualifiedOrigin>) {
    match parse_qualified_key(raw) {
        QualifiedKey::Qualified {
            type_name,
            repo,
            field_name,
        } => (field_name, Some(QualifiedOrigin { type_name, repo })),
        QualifiedKey::Bare | QualifiedKey::Malformed { .. } => (FieldName(raw.to_string()), None),
    }
}

/// The field decl a body contribution to `field` validates against, plus the
/// origin's source path: the resolved field's canonical decl, or — for a
/// divergent field — the decl of the origin the contribution's `qualifier`
/// names. `None` for an absent field, or a bare (unqualified) contribution to a
/// divergent field (that ambiguity is the frontmatter anchor's `mixin-collision`,
/// not resolved here).
///
/// This is the VALUE-PHASE slot lookup, distinct from the validation-phase
/// authority `validate::resolve_qualifier`. It is pure (no diagnostics) and
/// works off the already-folded [`EffectiveShape`] alone — it never walks the
/// type graph — so it is LENIENT by contract: for an auto-unified field it
/// returns the canonical decl and ignores the qualifier, and for a divergent
/// field it matches an origin by exact [`OriginId`] or returns `None`. It cannot
/// tell whether a descendant qualifier reaches one origin or several (that needs
/// the graph closure), so it TRUSTS the validation phase to have already
/// rejected an illegitimate qualifier (`qualifier-not-in-closure` /
/// `-does-not-declare-field` / `-ambiguous`). Its job is only to pick a slot to
/// read the value against, never to judge the qualifier's legality.
///
/// [`EffectiveShape`]: crate::closure::EffectiveShape
/// [`OriginId`]: crate::closure::OriginId
pub(crate) fn contrib_target_decl<'a>(
    shape: &'a crate::closure::EffectiveShape,
    field: &FieldName,
    qualifier: Option<&QualifiedOrigin>,
) -> Option<&'a crate::closure::OriginInfo> {
    if let Some(fo) = shape.get(field) {
        return Some(fo.canonical().1);
    }
    let (fo, q) = (shape.get_divergent(field)?, qualifier?);
    let oid = match &q.repo {
        Some(r) => crate::closure::OriginId(format!("{}::{}", q.type_name.as_str(), r)),
        None => crate::closure::OriginId(q.type_name.as_str().to_string()),
    };
    fo.origins()
        .find(|(id, _)| **id == oid)
        .map(|(_, info)| info)
}

/// The slot [`Shape`] a body attribution's value is read against, resolving a
/// divergent field to its qualified origin. `None` when there is no shape (an
/// unresolved claim), an absent field, or a bare divergent use.
fn attribution_slot<'a>(
    shape: Option<&'a crate::closure::EffectiveShape>,
    field: &FieldName,
    qualifier: Option<&QualifiedOrigin>,
) -> Option<&'a au_grammar::Shape> {
    contrib_target_decl(shape?, field, qualifier)
        .and_then(|info| info.decl.parsed_shape.as_ref().ok())
}

/// Which surface a contribution came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Top-level frontmatter key.
    Frontmatter,
    /// `[[target:field]]` wikilink in body prose.
    BodyWikilink,
    /// Marked ``` ```[:field] ``` fence in body. Its content-form comes from the
    /// slot, so this names the CARRIER, never a content type.
    BodyFence,
    /// Inline `` `[:field] value` `` code span in body prose.
    BodyInlineCode,
}

/// File + byte range of one contribution.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub file: PathBuf,
    pub byte_range: ByteRange,
}

/// A collision qualifier on a body attribution, `field{type}` / `field{type::repo}`
/// ([[type-def fields collision - auto-unify and qualified field::au-type-system]]): which divergent origin this contribution fills. `None`
/// for a bare attribution. INTERNAL for now — not serialized on the wire; the
/// divergent-fields wire surfacing lands with the schema bump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedOrigin {
    pub type_name: crate::typedef::TypeName,
    pub repo: Option<String>,
}

/// One occurrence of a value for a field.
#[derive(Debug, Clone, PartialEq)]
pub struct Contribution {
    pub surface: Surface,
    pub location: Location,
    /// Root-to-leaf section chain enclosing the contribution, per [[type value container::au-type-system]].
    /// Empty for body-preamble and frontmatter contributions.
    pub section_path: Vec<String>,
    pub value: ContributionValue,
    /// The brand constructor name the author WROTE in this value, `Some("meter")`
    /// for `meter(5)` (a peer brand keeps its qualifier, `Some("meter::units")`),
    /// `None` for a bare value or a reserved-primitive escape (`String("x")`, not a
    /// brand). The resolved value collapses to its underlying form regardless, so
    /// this is the round-trip / discriminator side-channel, see
    /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    pub brand: Option<String>,
    /// The collision qualifier a body attribution carried, selecting which
    /// divergent origin it fills ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). `None` for a bare
    /// attribution or a frontmatter contribution (frontmatter keeps the qualified
    /// key verbatim). Internal, not serialized.
    pub qualifier: Option<QualifiedOrigin>,
}

/// Resolved value of one contribution.
///
/// One authored value turned into meaning at its slot, the model produced once
/// by [`effective_values`] and consumed by validation and reads, see
/// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
/// Total, un-typeable surface becomes an explicit error node ([`MalformedConstructor`],
/// [`MalformedReference`]) rather than a raw string a consumer must re-interpret.
///
/// [`MalformedConstructor`]: ContributionValue::MalformedConstructor
/// [`MalformedReference`]: ContributionValue::MalformedReference
#[derive(Debug, Clone, PartialEq)]
pub enum ContributionValue {
    /// Primitive scalar (string, number, bool, date, …) carried as the
    /// originating `InstanceValue`.
    Scalar(InstanceValue),
    /// `[[target]]` reference with optional fragments (anchor, block-id, commit).
    /// The `:field` fragment is identity (attribution) and not part of the
    /// value, so it is intentionally not stored here.
    Reference {
        target: String,
        /// The `::repo` qualifier, `Some` for a cross-repo reference. Carried
        /// so the cross-repo layer resolves the value without re-parsing.
        repo: Option<String>,
        /// The `@commit` pin, `Some` for a commit-pinned reference. Carried so
        /// the pin diagnostics and a pinned read survive the value layer.
        commit: Option<String>,
        anchor: Option<String>,
        /// The block-id fragment with its mode ([`au_references::BlockId`]): a
        /// bare `^id` (navigational, the FILE is the contributed value) or a
        /// `^^id` (block-referent, the BLOCK's value is contributed).
        block_id: Option<au_references::BlockId>,
    },
    /// Inline-record value (a marked fence reading as a record, or a frontmatter inline
    /// map), carried as the parsed mapping for downstream [[type value container::au-type-system]] validation.
    InlineRecord(InstanceValue),
    /// A fixed-arity positional product, the value of a tuple slot or a tuple
    /// brand ([[type-def shape tuple::au-type-system]]). Recursive, each element is its own
    /// value plus its optional written brand. One value, one container, never
    /// the list path.
    Tuple(Vec<TupleElement>),
    /// An error node for a value that starts a `Name(...)` constructor shape at a
    /// brand or tuple slot but does not close as a well-formed one. Carries the
    /// raw surface, so the validator reads a node instead of re-parsing.
    MalformedConstructor(String),
    /// An error node for a malformed `[[...]]` at a reference slot. Carries the
    /// parse error and the raw surface, so the malformed-wikilink diagnostics map
    /// from a node instead of a re-parse.
    MalformedReference(WikilinkParseError, String),
}

/// One element of a [`ContributionValue::Tuple`], its value plus the brand the
/// author wrote for it, if any ([[type brand constructor::au-type-system]]). The brand sits on
/// the element, the outer tuple's brand rides on the [`ValueContainer`], so
/// nesting is uniform one level down.
#[derive(Debug, Clone, PartialEq)]
pub struct TupleElement {
    pub value: ContributionValue,
    pub brand: Option<String>,
}

/// One unique value for a field, plus every contribution that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueContainer {
    pub value: ContributionValue,
    /// The brand constructor written for this value, the explicit brand among the
    /// collapsed contributions (`None` if every contribution was bare). The
    /// discriminator at a union brand, and a round-trip signal elsewhere, see
    /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    pub brand: Option<String>,
    pub contributions: Vec<Contribution>,
}

/// Build per-field `ValueContainer`s from an instance plus its scanned
/// body events.
///
/// - Frontmatter fields surface as `Surface::Frontmatter` contributions
///   in YAML key order.
/// - Body wikilinks with `:field` fragments surface as `BodyWikilink`.
/// - Body inline-code `[:field] value` surfaces as `BodyInlineCode`.
/// - Body marked fences (` ```[:field] `) surface as `BodyFence`.
/// - Equal values collapse into one `ValueContainer` per [[type value container::au-type-system]]; container
///   ordering and intra-container contribution ordering follow [[type value container::au-type-system]].
///
/// `shape` is the instance's effective shape, used ONLY to decide whether a
/// whole-value `[[wikilink]]` is a `Reference` value or the literal string —
/// [[type reference::au-type-system]]'s validated-reference rule, which needs the SLOT.
/// `None` (an unresolved claim, so no shape) leaves every wikilink a scalar,
/// so a consumer cannot read `Scalar` as proof the slot is not a reference.
pub fn effective_values<'a>(
    instance: &'a Instance,
    body_events: &'a [BodyEvent<'a>],
    body_byte_offset: usize,
    shape: Option<&crate::closure::EffectiveShape>,
) -> BTreeMap<FieldName, Vec<ValueContainer>> {
    // `bool` = this contribution is one ELEMENT of an authored sequence, so it
    // is its own value slot and never collapses into another. Internal to the
    // collapse pass; it never reaches `Contribution`, so the wire is unchanged.
    let mut by_field: BTreeMap<FieldName, Vec<(Contribution, bool)>> = BTreeMap::new();

    // Frontmatter pathway — the graph-free field interpretation shared with
    // every nested and meta surface, see [`elaborate_fields`].
    push_frontmatter_contributions(
        &mut by_field,
        &instance.fields,
        &instance.source_path,
        shape,
    );

    // Body pathway — walk events with their derived section paths.
    let paired = derive_section_paths(body_events);
    for (section_path, event) in paired {
        let contrib_opt = build_body_contribution(
            &instance.source_path,
            event,
            section_path,
            body_byte_offset,
            shape,
        );
        if let Some((field_name, contrib)) = contrib_opt {
            // A body contribution carries no position, so it corroborates an
            // existing value rather than declaring a new slot.
            by_field
                .entry(field_name)
                .or_default()
                .push((contrib, false));
        }
    }

    // Brand / tuple constructor resolution over the collected contributions,
    // frontmatter and body alike (the shared interpretation site).
    resolve_brands_and_tuples(&mut by_field, shape);

    // Collapse equal values within each field per [[type value container::au-type-system]].
    let mut out: BTreeMap<FieldName, Vec<ValueContainer>> = BTreeMap::new();
    for (name, contribs) in by_field {
        out.insert(name, collapse_to_containers(contribs));
    }
    out
}

/// Elaborate a bare field set against its slot shapes into per-field value
/// containers, the graph-free core of [`effective_values`] with NO body pathway.
///
/// This is the single interpretation site applied to any value surface, a
/// NESTED inline record's fields or a `meta:` sub-region's fields, not only a
/// top-level instance's frontmatter. A consumer that has the graph resolves the
/// nested `type:` claim to its `EffectiveShape` and calls this, recursing to any
/// depth, see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
///
/// `source_path` is the file the fields live in, stamped onto each
/// contribution's location. `shape` is the surface's effective shape (`None`
/// leaves values faithful but untyped, the degrading contract).
pub fn elaborate_fields(
    fields: &[InstanceField],
    source_path: &std::path::Path,
    shape: Option<&crate::closure::EffectiveShape>,
) -> BTreeMap<FieldName, Vec<ValueContainer>> {
    let mut by_field: BTreeMap<FieldName, Vec<(Contribution, bool)>> = BTreeMap::new();
    push_frontmatter_contributions(&mut by_field, fields, source_path, shape);
    resolve_brands_and_tuples(&mut by_field, shape);
    let mut out: BTreeMap<FieldName, Vec<ValueContainer>> = BTreeMap::new();
    for (name, contribs) in by_field {
        out.insert(name, collapse_to_containers(contribs));
    }
    out
}

/// Push one frontmatter `Contribution` per field into `by_field`, list slots
/// split per element. The graph-free frontmatter pathway shared by
/// [`effective_values`] and [`elaborate_fields`].
///
/// [[type-instance body contribution::au-type-system]]: a `key:` with null value marks the field
/// as "filled by body", a visibility marker, not a value contribution, so it is
/// skipped rather than surfacing as a distinct `ValueContainer` beside a real
/// body contribution. The `bool` pushed is `false` for a whole-field value and
/// carried from `slot_contributions` for a list element (its own value slot).
fn push_frontmatter_contributions(
    by_field: &mut BTreeMap<FieldName, Vec<(Contribution, bool)>>,
    fields: &[InstanceField],
    source_path: &std::path::Path,
    shape: Option<&crate::closure::EffectiveShape>,
) {
    for field in fields {
        if matches!(field.value, InstanceValue::Null) {
            continue;
        }
        let field_name = FieldName(field.key.clone());
        let slot = slot_shape_for(shape, &field_name);
        let admits_ref = slot.is_some_and(au_grammar::slot_admits_reference);

        // A reference slot: a whole-value wikilink is a Reference value, and a
        // LIST slot yields one contribution PER ELEMENT, per [[type value container::au-type-system]]
        // ("a field's effective value is a list of value containers"). Both are
        // what let a frontmatter reference collapse with a body one naming the
        // same target, which is what keeps a bare slot from falsely tripping
        // `field-cardinality-exceeded`.
        if let Some(slot) = slot {
            if let Some((contribs, from_sequence)) =
                slot_contributions(source_path, field, slot, admits_ref)
            {
                by_field
                    .entry(field_name)
                    .or_default()
                    .extend(contribs.into_iter().map(|c| (c, from_sequence)));
                continue;
            }
        }

        let cv = match &field.value {
            InstanceValue::Mapping(_) => {
                // Normalize spans on the frontmatter mapping so it can
                // compare equal to a body fence contribution
                // carrying the same structural value ([[type value container::au-type-system]]).
                let mut v = field.value.clone();
                normalize_value_spans(&mut v);
                ContributionValue::InlineRecord(v)
            }
            InstanceValue::Sequence(_) => {
                // Same normalization for sequences — frontmatter list
                // elements carry per-element spans that would otherwise
                // block cross-surface collapse.
                let mut v = field.value.clone();
                normalize_value_spans(&mut v);
                ContributionValue::Scalar(v)
            }
            other => ContributionValue::Scalar(other.clone()),
        };
        let contrib = Contribution {
            surface: Surface::Frontmatter,
            location: Location {
                file: source_path.to_path_buf(),
                byte_range: field.value_span,
            },
            section_path: Vec::new(),
            value: cv,
            brand: None,
            // Frontmatter keeps the qualified key verbatim (the field-name key),
            // so a per-contribution qualifier is not tracked here.
            qualifier: None,
        };
        by_field
            .entry(field_name)
            .or_default()
            .push((contrib, false));
    }
}

/// Resolve brand and tuple constructors in-place over the collected
/// contributions, the graph-free interpretation shared by [`effective_values`]
/// and [`elaborate_fields`].
///
/// Branded-scalar collapse ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]):
/// a `Name(...)` constructor collapses on its underlying representation, so
/// `length: 42` and a body `meter(42)` are ONE value. The constructor name is
/// a surface annotation, not part of the value for equality, like an inline
/// record's `^:` id. Normalize a matching constructor to its inner scalar
/// before collapse. The slot for a brand is a bare `Shape::Record(name)`, a
/// scalar value there is a constructor or a bare coercion, never a record
/// (a record value is a Mapping, an `InlineRecord`, so it is skipped below).
/// So a single-arg constructor at such a slot resolves to its inner value
/// uniformly, a single brand and a union member alike, and the written brand
/// is recorded as the discriminator. A `::repo` brand keeps its qualifier,
/// since the name is taken verbatim from the constructor.
///
/// The slot is taken at its ELEMENT shape, unwrapping `[]` / `*@`, so a list
/// of brands (`meter[]`) resolves each element the same as a single slot: one
/// contribution per element already sits in `contribs`, so the same loop
/// resolves them. A brand named through `&` / `*` (`Shape::InlineOrReference` /
/// `Shape::Reference`, a union brand used by that suffix) resolves like the
/// bare-name form; a wikilink there is already a `Reference` node, so only a
/// constructor-shaped string is resolved, never a valid reference value.
fn resolve_brands_and_tuples(
    by_field: &mut BTreeMap<FieldName, Vec<(Contribution, bool)>>,
    shape: Option<&crate::closure::EffectiveShape>,
) {
    for (name, contribs) in by_field.iter_mut() {
        let element_slot = slot_shape_for(shape, name).map(au_grammar::slot_element_shape);
        let record_slot = matches!(
            element_slot,
            Some(
                au_grammar::Shape::Record(_)
                    | au_grammar::Shape::InlineOrReference(_)
                    | au_grammar::Shape::Reference(_)
            )
        );
        let tuple_slot = matches!(element_slot, Some(au_grammar::Shape::Tuple(_)));
        if !record_slot && !tuple_slot {
            continue;
        }
        for (contrib, _) in contribs.iter_mut() {
            let ContributionValue::Scalar(InstanceValue::String(s)) = &contrib.value else {
                continue;
            };
            let s = s.clone();
            if record_slot {
                // A bare-name (brand) slot. A single-arg constructor resolves to
                // its underlying value with the written brand recorded (a scalar /
                // enum brand); a multi-arg constructor is a named tuple brand; a
                // nameless `(...)` coerces to the tuple brand (no name); a
                // reserved-primitive constructor (`String("x")`) is the escape (no
                // brand); a malformed constructor is a total error node. A read
                // never surfaces the raw constructor string.
                match au_grammar::recognize_constructor(&s) {
                    au_grammar::ConstructorMatch::Constructor(c) => {
                        if let [arg] = c.args.as_slice() {
                            contrib.value = ContributionValue::Scalar(parse_constructor_arg(arg));
                            // The written brand rides faithfully, a reserved-
                            // primitive name (`String(...)`) included, per
                            // amendment 2609051620. Admission is the validator's
                            // separate concern: at a single nominal brand slot a
                            // primitive name is UNADMITTED (`String("x")` at a
                            // `meter` slot is `brand-constructor-mismatch`), at a
                            // union it is the escape where the primitive is a
                            // member. Carrying it here lets the one scalar-verdict
                            // walker judge that, instead of the prior `None` which
                            // hid the escape from the body path.
                            contrib.brand = Some(c.name.clone());
                        } else {
                            contrib.value =
                                ContributionValue::Tuple(elaborate_tuple_elements(&c.args));
                            contrib.brand = Some(c.name.clone());
                        }
                    }
                    au_grammar::ConstructorMatch::Malformed => {
                        contrib.value = ContributionValue::MalformedConstructor(s.clone());
                    }
                    au_grammar::ConstructorMatch::NotConstructor => {
                        match au_grammar::recognize_tuple(&s) {
                            au_grammar::TupleMatch::Tuple(args) => {
                                contrib.value =
                                    ContributionValue::Tuple(elaborate_tuple_elements(&args));
                            }
                            au_grammar::TupleMatch::Malformed => {
                                contrib.value = ContributionValue::MalformedConstructor(s.clone());
                            }
                            // A bare scalar coerces to the brand; leave it.
                            au_grammar::TupleMatch::NotTuple => {}
                        }
                    }
                }
            } else {
                // An inline tuple slot `(A, B)`. The value is the nameless paren
                // form; a `[...]` sequence is a list (left for the validator to
                // reject), a named constructor is not an inline-tuple value.
                match au_grammar::recognize_tuple(&s) {
                    au_grammar::TupleMatch::Tuple(args) => {
                        contrib.value = ContributionValue::Tuple(elaborate_tuple_elements(&args));
                    }
                    au_grammar::TupleMatch::Malformed => {
                        contrib.value = ContributionValue::MalformedConstructor(s.clone());
                    }
                    au_grammar::TupleMatch::NotTuple => {}
                }
            }
        }
    }
}

fn build_body_contribution(
    instance_path: &std::path::Path,
    event: &BodyEvent<'_>,
    section_path: Vec<String>,
    body_byte_offset: usize,
    shape: Option<&crate::closure::EffectiveShape>,
) -> Option<(FieldName, Contribution)> {
    match event {
        BodyEvent::Wikilink { raw, span } => {
            let parsed = parse_wikilink_inner(raw).ok()?;
            let raw_field = parsed.field.clone()?;
            // The `:field` may be qualified, `field{type}` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]); split it
            // into base field + origin. The slot that decides reference-ness is
            // the target origin's — the qualified origin for a divergent field.
            let (field_name, qualifier) = parse_attribution(&raw_field);
            // Slot-gated, exactly like the frontmatter pathway. A `[[x:field]]`
            // aimed at a non-reference slot is NOT a reference value — that
            // input is already a `body-slot-shape-mismatch`, so only invalid
            // input changes shape here, and the two surfaces stay consistent.
            let admits = attribution_slot(shape, &field_name, qualifier.as_ref())
                .is_some_and(au_grammar::slot_admits_reference);
            let value = if admits {
                ContributionValue::Reference {
                    target: parsed.target,
                    repo: parsed.repo,
                    commit: parsed.commit,
                    anchor: parsed.anchor,
                    block_id: parsed.block_id,
                }
            } else {
                ContributionValue::Scalar(InstanceValue::String(raw.trim().to_string()))
            };
            Some((
                field_name,
                Contribution {
                    surface: Surface::BodyWikilink,
                    location: Location {
                        file: instance_path.to_path_buf(),
                        byte_range: absolute_span(*span, body_byte_offset),
                    },
                    section_path,
                    value,
                    brand: None,
                    qualifier,
                },
            ))
        }
        BodyEvent::InlineCode { content, span } => {
            // Strict [[type-instance body contribution::au-type-system]] shape: `[:fieldName] value` with no whitespace
            // inside the brackets. Anything looser is treated as a
            // malformed-attribution-marker by body_validate and must not
            // also count as a contribution here.
            let trimmed = content.strip_prefix("[:")?;
            let end = trimmed.find(']')?;
            let raw_field = &trimmed[..end];
            let raw_field_trimmed = raw_field.trim();
            if raw_field_trimmed.is_empty() || raw_field != raw_field_trimmed {
                return None;
            }
            let value_text = trimmed[end + 1..].trim();
            if value_text.is_empty() {
                return None;
            }
            let (field_name, qualifier) = parse_attribution(raw_field_trimmed);
            // A wikilink-shaped value at a reference-admitting slot is a Reference
            // value, resolved once here so the body reference pass and every read
            // consume the parsed node instead of re-parsing the surface, the
            // value-model invariant ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
            // Slot-gated exactly like the frontmatter and prose-wikilink pathways.
            // The inline-code value carries the BRACKETED form, so it needs the
            // bracket-stripping `parse_wikilink`. A malformed wikilink is a total
            // error node; a non-wikilink or non-reference slot keeps the authored
            // scalar (a wrong scalar there is a shape mismatch, owned downstream).
            let admits = attribution_slot(shape, &field_name, qualifier.as_ref())
                .is_some_and(au_grammar::slot_admits_reference);
            let value = if admits && looks_like_wikilink(value_text) {
                match parse_wikilink(value_text) {
                    Ok(parsed) => ContributionValue::Reference {
                        target: parsed.target,
                        repo: parsed.repo,
                        commit: parsed.commit,
                        anchor: parsed.anchor,
                        block_id: parsed.block_id,
                    },
                    Err(e) => ContributionValue::MalformedReference(e, value_text.to_string()),
                }
            } else {
                ContributionValue::Scalar(parse_inline_scalar(value_text))
            };
            Some((
                field_name,
                Contribution {
                    surface: Surface::BodyInlineCode,
                    location: Location {
                        file: instance_path.to_path_buf(),
                        byte_range: absolute_span(*span, body_byte_offset),
                    },
                    section_path,
                    value,
                    brand: None,
                    qualifier,
                },
            ))
        }
        BodyEvent::FencedBlock {
            info, body, span, ..
        } => {
            let field = extract_field_marker(info)?;
            let (field_name, qualifier) = parse_attribution(field);
            // A marked fence is a MULTI-LINE CHANNEL, not a yaml channel: the
            // declared slot decides how its content reads
            // ([[type-instance body contribution::au-type-system]]). The language tag carries no
            // engine meaning, so it is never consulted.
            let value = read_fence_content(
                body,
                attribution_slot(shape, &field_name, qualifier.as_ref()),
            );
            Some((
                field_name,
                Contribution {
                    surface: Surface::BodyFence,
                    location: Location {
                        file: instance_path.to_path_buf(),
                        byte_range: absolute_span(*span, body_byte_offset),
                    },
                    section_path,
                    value,
                    brand: None,
                    qualifier,
                },
            ))
        }
        _ => None,
    }
}

/// Read a marked fence's content per its slot, [[type-instance body contribution::au-type-system]].
///
/// `slot` is `None` for an unresolved [[type-instance type::au-type-system]] claim, which leaves
/// no effective shape. With no contract to honor, the fence falls back to
/// VALUE-SHAPE, the same fallback the frontmatter pathway uses: a mapping is
/// unambiguously structured so it reads as a record, anything else keeps its
/// text. That symmetry is load-bearing — a frontmatter mapping and a fence of
/// equal structure must still collapse into one [[type value container::au-type-system]] on an
/// unresolved instance, and a text read would split them.
///
/// A reference-only slot admits no fence at all. The content is still captured
/// verbatim so the contribution EXISTS; the verdict is not this function's, it is
/// emitted as `body-slot-shape-mismatch` by the fence validator in
/// `body_validate`, which has the same slot in hand.
pub(crate) fn read_fence_content(
    body: &str,
    slot: Option<&au_grammar::Shape>,
) -> ContributionValue {
    let Some(slot) = slot else {
        return match au_parser::yaml::parse(body)
            .ok()
            .and_then(|d| d.into_iter().next())
        {
            Some(doc) if matches!(doc.data, au_parser::yaml::YamlData::Mapping(_)) => {
                ContributionValue::InlineRecord(parse_yaml_to_instance_value(&doc))
            }
            _ => ContributionValue::Scalar(InstanceValue::String(fence_text(body))),
        };
    };
    let text_form = au_grammar::slot_text_form(slot);

    // The body is parsed more than once here (the classifier, then the record
    // read). Deliberate: `fence_reads_as_record` is the SHARED classifier that
    // the validator also calls, so threading a pre-parsed doc through would
    // either change its signature for both callers or duplicate the union rule
    // locally. A duplicated rule is exactly the drift this design exists to
    // prevent, and a fence is a few hundred bytes.
    if fence_reads_as_record(body, Some(slot)) {
        if let Some(doc) = au_parser::yaml::parse(body)
            .ok()
            .and_then(|d| d.into_iter().next())
        {
            return ContributionValue::InlineRecord(parse_yaml_to_instance_value(&doc));
        }
        // Unparseable at a record-only slot: keep the text so the contribution
        // still exists and validation reports against it.
        return ContributionValue::Scalar(InstanceValue::String(fence_text(body)));
    }

    match text_form {
        Some(au_grammar::TextForm::Scalar) => {
            ContributionValue::Scalar(parse_inline_scalar(fence_text(body).trim()))
        }
        // Verbatim, and the reference-only fallback.
        _ => ContributionValue::Scalar(InstanceValue::String(fence_text(body))),
    }
}

/// Does a marked fence read as an inline RECORD, given its slot?
///
/// The single source for the question, shared by the value layer
/// ([`read_fence_content`]) and the embedded-record validator, so the two can
/// never disagree about what a given fence is.
///
/// A compound admitting BOTH forms has no single answer, so the CONTENT
/// disambiguates: a mapping carrying `type:` takes the record branch, anything
/// else takes the text branch. The same rule that already makes an inline
/// `type:` mandatory at a union slot ([[type-def shape record::au-type-system]]).
///
/// With no slot (an unresolved claim) the fallback is value-shape, matching the
/// frontmatter pathway, see [`read_fence_content`].
pub(crate) fn fence_reads_as_record(body: &str, slot: Option<&au_grammar::Shape>) -> bool {
    let Some(slot) = slot else {
        return matches!(
            au_parser::yaml::parse(body).ok().and_then(|d| d.into_iter().next()),
            Some(doc) if matches!(doc.data, au_parser::yaml::YamlData::Mapping(_))
        );
    };
    match (
        au_grammar::slot_admits_record(slot),
        au_grammar::slot_text_form(slot),
    ) {
        (true, None) => true,
        (false, _) => false,
        (true, Some(_)) => fence_parses_as_typed_mapping(body),
    }
}

/// The fence's value: exactly the lines between the delimiters.
///
/// The scanner hands back everything from the end of the open-fence line to the
/// start of the close-fence line, so that slice carries the open line's
/// terminator at the front and the last content line's terminator at the back.
/// Both belong to the delimiters, not the value ([[type-instance body contribution::au-type-system]]).
/// Nothing else is touched: no trimming, no dedenting, no yaml scalar folding,
/// so blank lines and a multi-paragraph value survive intact.
fn fence_text(body: &str) -> String {
    let s = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.to_string()
}

/// Does the content read as an inline record, i.e. parse to a YAML mapping that
/// carries a `type:` key? The union disambiguator, see [[type-def shape record::au-type-system]].
fn fence_parses_as_typed_mapping(body: &str) -> bool {
    use au_parser::yaml::{Scalar, YamlData};
    let Ok(docs) = au_parser::yaml::parse(body) else {
        return false;
    };
    let Some(doc) = docs.first() else {
        return false;
    };
    let YamlData::Mapping(map) = &doc.data else {
        return false;
    };
    map.iter()
        .any(|(k, _)| matches!(&k.data, YamlData::Value(Scalar::String(s)) if s.as_ref() == "type"))
}

pub(crate) fn parse_inline_scalar(text: &str) -> InstanceValue {
    if let Ok(b) = text.parse::<bool>() {
        return InstanceValue::Boolean(b);
    }
    if let Ok(i) = text.parse::<i64>() {
        return InstanceValue::Integer(i);
    }
    if let Ok(f) = text.parse::<f64>() {
        // Only a finite float is a numeric value. A non-finite parse means the
        // literal was `inf`/`NaN` text, or a number so long it overflowed f64;
        // keep it as the authored String rather than coercing to a non-finite
        // value that would then fail the Number-shape check on something the
        // author wrote as plain text. A finite f64 past i64 range still loses
        // integer precision — accepted, the value is a float.
        if f.is_finite() {
            return InstanceValue::Float(f);
        }
    }
    if text.eq_ignore_ascii_case("null") || text == "~" {
        return InstanceValue::Null;
    }
    if looks_like_wikilink(text) {
        // Leave the wikilink text as a String, the value is the text the
        // author wrote in the inline code span.
    }
    InstanceValue::String(text.to_string())
}

/// Parse a `Name(...)` constructor argument into a scalar value. A double-quoted
/// arg is unwrapped, the quotes being delimiters, so `String("looks(foo)")` is
/// the string `looks(foo)` and `meter("42")` is the string `"42"` (a quoted arg
/// forces String, no numeric coercion, matching YAML). An unquoted arg is parsed
/// as a plain inline scalar. `'` is not a delimiter (an apostrophe is an ordinary
/// character), matching the grammar recognizer. Shared by the validation arg
/// checks and the value-container collapse so both interpret an arg identically.
/// See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub(crate) fn parse_constructor_arg(arg: &str) -> InstanceValue {
    let a = arg.trim();
    if a.len() >= 2 && a.starts_with('"') && a.ends_with('"') {
        return InstanceValue::String(a[1..a.len() - 1].to_string());
    }
    parse_inline_scalar(a)
}

/// The recursion cap for value elaboration, mirroring au-grammar's
/// `MAX_SHAPE_DEPTH`. A pathologically nested value becomes a malformed node, not
/// a stack overflow ([[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
const MAX_VALUE_DEPTH: usize = 64;

/// Elaborate one value STRING into a typed value plus its written brand, purely
/// from syntax ([[type-def shape tuple::au-type-system]], decision A). Used for tuple ELEMENTS:
/// each position's TYPE is the def's (validated later), but the value STRUCTURE is
/// the syntax's — a bare scalar, a `Name(...)` brand or named tuple, or a nameless
/// `(...)` tuple. Depth-bounded; a value nested past the cap is a malformed node.
fn elaborate_element(s: &str, depth: usize) -> TupleElement {
    if depth > MAX_VALUE_DEPTH {
        return TupleElement {
            value: ContributionValue::MalformedConstructor(s.to_string()),
            brand: None,
        };
    }
    match au_grammar::recognize_constructor(s) {
        au_grammar::ConstructorMatch::Constructor(c) => {
            if let [arg] = c.args.as_slice() {
                // A single-arg constructor is a scalar / enum brand. The written
                // brand rides faithfully, a reserved-primitive name included, the
                // per-element parallel of the top-level scalar collapse; admission
                // is the validator's separate concern.
                TupleElement {
                    value: ContributionValue::Scalar(parse_constructor_arg(arg)),
                    brand: Some(c.name.clone()),
                }
            } else {
                // A multi-arg constructor is a named tuple brand.
                let elements = c
                    .args
                    .iter()
                    .map(|a| elaborate_element(a, depth + 1))
                    .collect();
                TupleElement {
                    value: ContributionValue::Tuple(elements),
                    brand: Some(c.name.clone()),
                }
            }
        }
        au_grammar::ConstructorMatch::Malformed => TupleElement {
            value: ContributionValue::MalformedConstructor(s.to_string()),
            brand: None,
        },
        au_grammar::ConstructorMatch::NotConstructor => match au_grammar::recognize_tuple(s) {
            au_grammar::TupleMatch::Tuple(args) => {
                let elements = args
                    .iter()
                    .map(|a| elaborate_element(a, depth + 1))
                    .collect();
                TupleElement {
                    value: ContributionValue::Tuple(elements),
                    brand: None,
                }
            }
            au_grammar::TupleMatch::Malformed => TupleElement {
                value: ContributionValue::MalformedConstructor(s.to_string()),
                brand: None,
            },
            au_grammar::TupleMatch::NotTuple => TupleElement {
                value: ContributionValue::Scalar(parse_constructor_arg(s)),
                brand: None,
            },
        },
    }
}

/// Elaborate the top-level args of a tuple (a constructor's or a nameless paren's)
/// into elements.
fn elaborate_tuple_elements(args: &[String]) -> Vec<TupleElement> {
    args.iter().map(|a| elaborate_element(a, 1)).collect()
}

fn parse_yaml_to_instance_value(doc: &au_parser::yaml::MarkedYaml<'_>) -> InstanceValue {
    use au_parser::yaml::{Scalar, YamlData};
    match &doc.data {
        YamlData::Value(Scalar::String(s)) => InstanceValue::String(s.to_string()),
        YamlData::Value(Scalar::Integer(i)) => InstanceValue::Integer(*i),
        YamlData::Value(Scalar::FloatingPoint(f)) => InstanceValue::Float(f.into_inner()),
        YamlData::Value(Scalar::Boolean(b)) => InstanceValue::Boolean(*b),
        YamlData::Value(Scalar::Null) => InstanceValue::Null,
        YamlData::Sequence(items) => {
            let elements: Vec<SequenceElement> = items
                .iter()
                .map(|item| SequenceElement {
                    value: parse_yaml_to_instance_value(item),
                    span: ByteRange::new(0, 0),
                    // Span-zeroed structural form for cross-surface equality;
                    // body wikilinks are scanned on the body surface, not here.
                    nav_links: Vec::new(),
                })
                .collect();
            InstanceValue::Sequence(elements)
        }
        YamlData::Mapping(map) => {
            let mut fields = Vec::new();
            let mut type_claim: Option<crate::instance::TypeClaim> = None;
            for (k, v) in map.iter() {
                let key = match &k.data {
                    YamlData::Value(Scalar::String(s)) => s.to_string(),
                    _ => continue,
                };
                // [[type value container::au-type-system]] inline-record equality compares the type claim.
                // Body fences carry `type:` declared in the fence;
                // capture it (spans zeroed for cross-surface equality)
                // so a body fence can collapse with an equivalent
                // frontmatter inline record.
                if key == "type" {
                    type_claim = extract_type_claim_zeroed(v);
                    continue;
                }
                // `^:` is addressability of the contribution site, not
                // part of the value ([[type block-id::au-type-system]]) — excluded from
                // equality the way spans are zeroed.
                if key == "^" {
                    continue;
                }
                fields.push(InstanceField {
                    key,
                    key_span: ByteRange::new(0, 0),
                    value: parse_yaml_to_instance_value(v),
                    value_span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                });
            }
            InstanceValue::Mapping(crate::instance::InlineValue {
                type_claim,
                block_id: None,
                fields,
                doc: None,
                field_docs: Default::default(),
            })
        }
        _ => InstanceValue::Null,
    }
}

/// Extract a `type:` value as a `TypeClaim` with every span zeroed.
/// Used by both the body-fence parser AND the frontmatter
/// normalizer so contributions from either surface can compare equal
/// per [[type value container::au-type-system]].
fn extract_type_claim_zeroed(
    val: &au_parser::yaml::MarkedYaml<'_>,
) -> Option<crate::instance::TypeClaim> {
    use crate::instance::TypeClaim;
    use crate::typedef::TypeNameClaim;
    use au_parser::yaml::{Scalar, YamlData};
    let zero = ByteRange::new(0, 0);
    match &val.data {
        YamlData::Value(Scalar::String(s)) => Some(TypeClaim::Bare(TypeNameClaim::parse(s, zero))),
        YamlData::Sequence(items) => {
            let names: Vec<TypeNameClaim> = items
                .iter()
                .filter_map(|item| match &item.data {
                    YamlData::Value(Scalar::String(s)) => Some(TypeNameClaim::parse(s, zero)),
                    _ => None,
                })
                .collect();
            Some(TypeClaim::List {
                items: names,
                value_span: zero,
            })
        }
        _ => None,
    }
}

/// Recursively zero every byte-range field in an `InstanceValue` tree.
/// [[type value container::au-type-system]] collapse equality is structural — two surfaces contributing
/// the "same" value differ in where the bytes live in their source
/// files, and the value-equality compare must ignore that. Mutates
/// the value in place; safe because the contribution holds its own
/// authoritative `Location.byte_range` independently of the value tree.
fn normalize_value_spans(value: &mut InstanceValue) {
    use crate::instance::TypeClaim;
    let zero = ByteRange::new(0, 0);
    match value {
        InstanceValue::Sequence(items) => {
            for elem in items.iter_mut() {
                elem.span = zero;
                // Navigational links are positional metadata derived from the
                // value text; the body surface carries none, so clear them to
                // keep cross-surface equality on structural value alone.
                elem.nav_links.clear();
                normalize_value_spans(&mut elem.value);
            }
        }
        InstanceValue::Mapping(inline) => {
            if let Some(tc) = inline.type_claim.as_mut() {
                match tc {
                    TypeClaim::Bare(c) => c.span = zero,
                    TypeClaim::List { items, value_span } => {
                        *value_span = zero;
                        for c in items.iter_mut() {
                            c.span = zero;
                        }
                    }
                }
            }
            for field in inline.fields.iter_mut() {
                field.key_span = zero;
                field.value_span = zero;
                field.nav_links.clear();
                normalize_value_spans(&mut field.value);
            }
        }
        _ => {}
    }
}

fn absolute_span(span: ByteRange, offset: usize) -> ByteRange {
    ByteRange::new(span.start + offset, span.end + offset)
}

/// Collapse equal-value contributions per [[type value container::au-type-system]]. Source-order within each
/// container is preserved (the first-encountered contribution determines
/// container position; subsequent equals fold in).
/// The declared shape of `field`'s slot, or `None` when there is no effective
/// shape (an unresolved claim) or the field is not in it. The value layer
/// degrades to scalars then, rather than guessing reference-ness from syntax.
fn slot_shape_for<'s>(
    shape: Option<&'s crate::closure::EffectiveShape>,
    field: &FieldName,
) -> Option<&'s au_grammar::Shape> {
    shape?
        .get(field)?
        .canonical_decl()
        .parsed_shape
        .as_ref()
        .ok()
}

/// Frontmatter contributions for one field, given its declared slot.
///
/// A LIST slot yields one contribution PER ELEMENT, whatever the element type
/// ([[type value container::au-type-system]]: "a field's effective value is a list of value
/// containers", with cardinality over THAT list). Splitting only reference
/// lists would put an arbitrary seam back where this removes one, so every
/// list splits.
///
/// Each element is typed independently: a whole-value wikilink in a
/// reference-admitting slot is a `Reference`, a mapping is an `InlineRecord`,
/// anything else is a `Scalar`. Mixed elements are not an error to bail on —
/// `<paper* | String>[]` is a legal shape whose elements are legitimately of
/// both kinds, and where an element IS wrong for the slot the diagnostic
/// belongs to that element while its siblings stay correctly typed.
///
/// `None` when the field needs no element-wise handling — a bare slot holding
/// anything but a whole-value wikilink — so the caller's ordinary
/// scalar/record path runs and nothing else changes shape.
fn slot_contributions(
    source_path: &std::path::Path,
    field: &InstanceField,
    slot: &au_grammar::Shape,
    admits_ref: bool,
) -> Option<(Vec<Contribution>, bool)> {
    let at = |value: ContributionValue, span: ByteRange| Contribution {
        surface: Surface::Frontmatter,
        location: Location {
            file: source_path.to_path_buf(),
            byte_range: span,
        },
        section_path: Vec::new(),
        value,
        brand: None,
        qualifier: None,
    };
    // One element, typed on its own: a whole-value wikilink in a
    // reference-admitting slot is a reference, a mapping is an inline record,
    // anything else is a scalar.
    let element = |v: &InstanceValue, span: ByteRange| -> Contribution {
        if let (true, InstanceValue::String(raw)) = (admits_ref, v) {
            if looks_like_wikilink(raw) {
                // A frontmatter value carries the BRACKETED form, so it needs
                // the bracket-stripping parser; `parse_wikilink_inner` takes
                // the inner content, which is what a body event yields.
                match parse_wikilink(raw) {
                    Ok(parsed) => {
                        return at(
                            ContributionValue::Reference {
                                target: parsed.target,
                                repo: parsed.repo,
                                commit: parsed.commit,
                                anchor: parsed.anchor,
                                block_id: parsed.block_id,
                            },
                            span,
                        );
                    }
                    // Wikilink-shaped at a reference slot but unparseable: a total
                    // model carries an error node, not a raw scalar, so a read
                    // shows the malformed kind. The diagnostic still fires from
                    // the validator's own pass, this only shapes the value.
                    Err(e) => {
                        return at(
                            ContributionValue::MalformedReference(e, raw.trim().to_string()),
                            span,
                        );
                    }
                }
            }
        }
        // Span-normalize every element, mapping or not, so a list element can
        // still compare equal to a body contribution carrying the same
        // structural value ([[type value container::au-type-system]]).
        let mut owned = v.clone();
        normalize_value_spans(&mut owned);
        match owned {
            InstanceValue::Mapping(_) => at(ContributionValue::InlineRecord(owned), span),
            _ => at(ContributionValue::Scalar(owned), span),
        }
    };

    match (&field.value, au_grammar::slot_is_list(slot)) {
        // A list slot's sequence: one container per element, kinds independent.
        (InstanceValue::Sequence(elems), true) => Some((
            elems.iter().map(|e| element(&e.value, e.span)).collect(),
            // Sequence ELEMENTS: each is its own value slot.
            true,
        )),
        // A list slot may also be filled bare with a single entry
        // ([[type list form::au-type-system]], "single bare, several block").
        (v @ InstanceValue::String(_), true) => Some((vec![element(v, field.value_span)], true)),
        // A bare slot: only a whole-value wikilink in a reference-admitting
        // slot converts; anything else falls through unchanged.
        (InstanceValue::String(raw), false) if admits_ref && looks_like_wikilink(raw) => {
            Some((vec![element(&field.value, field.value_span)], false))
        }
        _ => None,
    }
}

/// Collapse contributions into containers per [[type value container::au-type-system]].
///
/// Equal values share one container — EXCEPT that two elements of one authored
/// sequence never collapse with each other. A list holds N value SLOTS, so
/// `[a, a]` is two values the author wrote twice, not one value contributed
/// twice. Collapsing them would turn a list into a set and lose order:
/// `[a, b, a]` would report `a, b`, unreconstructable.
///
/// A contribution carrying no position (a prose mention) collapses into the
/// FIRST container with an equal value. Where several slots hold that value the
/// choice is arbitrary — a prose mention says nothing about which one it means
/// — so it is a deterministic tie-break, not a semantic claim.
fn collapse_to_containers(mut contribs: Vec<(Contribution, bool)>) -> Vec<ValueContainer> {
    // Stable ordering: frontmatter first (in their original order), then
    // body (in source position order — already source-ordered from the
    // body scanner's sort). This is also what makes "first match"
    // well-defined: sequence elements are laid down in document order before
    // any body contribution looks for one.
    contribs.sort_by_key(|(c, _)| match c.surface {
        Surface::Frontmatter => (0, c.location.byte_range.start),
        _ => (1, c.location.byte_range.start),
    });

    let mut containers: Vec<ValueContainer> = Vec::new();
    for (c, is_element) in contribs {
        // An element declares its own slot, so it never merges into an
        // existing container, even one holding an equal value.
        let existing = if is_element {
            None
        } else {
            containers.iter_mut().find(|cont| cont.value == c.value)
        };
        if let Some(existing) = existing {
            // An explicit brand among the collapsed contributions wins; a bare
            // contribution never clears one already recorded.
            if existing.brand.is_none() {
                existing.brand = c.brand.clone();
            }
            existing.contributions.push(c);
        } else {
            containers.push(ValueContainer {
                value: c.value.clone(),
                brand: c.brand.clone(),
                contributions: vec![c],
            });
        }
    }
    containers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{parse_instance, Instance};
    use au_parser::yaml::parse;
    use au_parser::{scan_body, split_frontmatter};
    use std::path::Path;

    fn parse_md(src: &str) -> (Instance, Vec<BodyEvent<'_>>, usize) {
        let split = split_frontmatter(src).unwrap();
        match split {
            Some(s) => {
                let docs = parse(s.frontmatter).unwrap();
                let doc = docs.first().unwrap();
                let inst =
                    parse_instance(Path::new("/v/inst.md"), src, s.frontmatter_range.start, doc)
                        .instance
                        .unwrap();
                let events = scan_body(s.body);
                (inst, events, s.body_range.start)
            }
            None => panic!("frontmatter required for this test"),
        }
    }

    #[test]
    fn frontmatter_field_yields_one_container() {
        let src = "---\ntype: decision\ndescription: \"x\"\n---\n";
        let split = split_frontmatter(src).unwrap().unwrap();
        let docs = parse(split.frontmatter).unwrap();
        let inst = parse_instance(
            Path::new("/v/inst.md"),
            src,
            split.frontmatter_range.start,
            docs.first().unwrap(),
        )
        .instance
        .unwrap();
        let values = effective_values(&inst, &[], 0, None);
        let desc = values
            .get(&FieldName("description".into()))
            .expect("description present");
        assert_eq!(desc.len(), 1);
        assert_eq!(desc[0].contributions.len(), 1);
        assert_eq!(desc[0].contributions[0].surface, Surface::Frontmatter);
    }

    #[test]
    fn body_inline_code_contributes_scalar() {
        let src = "---\ntype: decision\ndescription: \"x\"\n---\n\n`[:status] made`\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let status = values
            .get(&FieldName("status".into()))
            .expect("status present");
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].contributions[0].surface, Surface::BodyInlineCode);
        match &status[0].value {
            ContributionValue::Scalar(InstanceValue::String(s)) => assert_eq!(s, "made"),
            v => panic!("unexpected value: {:?}", v),
        }
    }

    #[test]
    fn inline_scalar_keeps_non_finite_numerics_as_strings() {
        // A finite number is typed; a non-finite parse (inf / NaN text, or a
        // literal too long for f64) stays the authored String, not a
        // non-finite Float that would later fail the Number-shape check.
        assert_eq!(parse_inline_scalar("42"), InstanceValue::Integer(42));
        assert!(matches!(
            parse_inline_scalar("3.5"),
            InstanceValue::Float(_)
        ));
        assert_eq!(
            parse_inline_scalar("inf"),
            InstanceValue::String("inf".into())
        );
        assert_eq!(
            parse_inline_scalar("NaN"),
            InstanceValue::String("NaN".into())
        );
        // A finite f64 past i64 range is still a (lossy) float, by design.
        assert!(matches!(
            parse_inline_scalar("99999999999999999999"),
            InstanceValue::Float(_)
        ));
    }

    #[test]
    fn body_wikilink_contributes_but_is_only_a_reference_once_a_slot_says_so() {
        // The value layer is the TYPED surface. With no effective shape there
        // is no slot to consult, so a body wikilink contributes its value as a
        // SCALAR — it does not infer reference-ness from the `:field` syntax.
        // Inferring it would answer the NAVIGATIONAL question (any `[[...]]` is
        // an edge), which `backlinks` / `references_out` already answer
        // correctly and syntactically.
        //
        // The typed cases live in au-engine's `value_layer_references`, which
        // can build a real graph: a `paper*` slot yields a reference, a
        // `String` slot yields a scalar.
        let src = "---\ntype: decision\ndescription: \"x\"\nrationale:\n---\n\nSee [[paper-a:rationale]] for context.\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let rat = values
            .get(&FieldName("rationale".into()))
            .expect("rationale present");
        let body_contrib = rat
            .iter()
            .flat_map(|c| c.contributions.iter())
            .find(|c| c.surface == Surface::BodyWikilink)
            .expect("the wikilink still contributes");
        assert!(
            matches!(body_contrib.value, ContributionValue::Scalar(_)),
            "no slot known, so no reference claim: {:?}",
            body_contrib.value
        );
        assert_eq!(body_contrib.section_path, Vec::<String>::new());
    }

    #[test]
    fn same_value_collapses_across_frontmatter_and_body() {
        let src =
            "---\ntype: decision\ndescription: \"x\"\nstatus: made\n---\n\n`[:status] made`\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let status = values
            .get(&FieldName("status".into()))
            .expect("status present");
        // Both contributions ("made" / "made") collapse into one container.
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].contributions.len(), 2);
        let surfaces: Vec<_> = status[0].contributions.iter().map(|c| c.surface).collect();
        assert!(surfaces.contains(&Surface::Frontmatter));
        assert!(surfaces.contains(&Surface::BodyInlineCode));
    }

    #[test]
    fn distinct_values_produce_distinct_containers() {
        let src = "---\ntype: decision\ndescription: \"x\"\nstatus: made\n---\n\n`[:status] superseded`\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let status = values
            .get(&FieldName("status".into()))
            .expect("status present");
        assert_eq!(status.len(), 2);
    }

    #[test]
    fn body_fence_contributes_inline_record() {
        let src = "---\ntype: decision\ndescription: \"x\"\nassumptions:\n---\n\n```yaml [:assumptions]\ntype: assumption\ndescription: stable\n```\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let assumptions = values
            .get(&FieldName("assumptions".into()))
            .expect("assumptions present");
        let block_contrib = assumptions
            .iter()
            .flat_map(|c| c.contributions.iter())
            .find(|c| c.surface == Surface::BodyFence)
            .expect("fence contribution present");
        match &block_contrib.value {
            ContributionValue::InlineRecord(InstanceValue::Mapping(_)) => {}
            v => panic!("unexpected: {:?}", v),
        }
    }

    /// [[type value container::au-type-system]] regression: a Mapping value contributed from both
    /// frontmatter (as an inline-record `field: { type: T, ... }`) and
    /// a body fence (` ```[:field] ` with the same `type: T`
    /// and body fields) must collapse into one ValueContainer with two
    /// Contributions.
    ///
    /// Pre-fix: frontmatter parsed the inline-record with real spans
    /// and a populated `type_claim`; body fence parsed with
    /// zeroed spans and `type_claim: None`. The derived `PartialEq`
    /// compared all those, so the values never collapsed across
    /// surfaces. Now `normalize_value_spans` strips spans on the
    /// frontmatter side AND `parse_yaml_to_instance_value` extracts
    /// the body-block's `type:` claim — both sides converge on the
    /// same structural shape.
    #[test]
    fn same_mapping_value_collapses_across_frontmatter_and_body_fence() {
        let src = "---\n\
type: decision\n\
description: \"x\"\n\
assumption:\n  type: assumption\n  description: stable\n\
---\n\
\n\
```yaml [:assumption]\n\
type: assumption\n\
description: stable\n\
```\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let assumption = values
            .get(&FieldName("assumption".into()))
            .expect("assumption present");
        assert_eq!(
            assumption.len(),
            1,
            "frontmatter Mapping + body fence of equal structure must collapse into one ValueContainer; got: {assumption:?}"
        );
        assert_eq!(
            assumption[0].contributions.len(),
            2,
            "the one container must carry both contributions (frontmatter + body fence)"
        );
        let surfaces: Vec<_> = assumption[0]
            .contributions
            .iter()
            .map(|c| c.surface)
            .collect();
        assert!(surfaces.contains(&Surface::Frontmatter));
        assert!(surfaces.contains(&Surface::BodyFence));
    }

    /// Same setup, but the body fence has a DIFFERENT field
    /// value — they must NOT collapse.
    #[test]
    fn differing_mapping_values_do_not_collapse() {
        let src = "---\n\
type: decision\n\
description: \"x\"\n\
assumption:\n  type: assumption\n  description: stable\n\
---\n\
\n\
```yaml [:assumption]\n\
type: assumption\n\
description: divergent\n\
```\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let assumption = values
            .get(&FieldName("assumption".into()))
            .expect("assumption present");
        assert_eq!(
            assumption.len(),
            2,
            "structurally different values stay separate; got: {assumption:?}"
        );
    }

    #[test]
    fn section_path_attaches_to_body_contributions() {
        let src = "---\ntype: decision\ndescription: \"x\"\nrationale:\n---\n\n# Why\n\nSee [[paper-a:rationale]] in context.\n";
        let (inst, events, body_offset) = parse_md(src);
        let values = effective_values(&inst, &events, body_offset, None);
        let rationale = values
            .get(&FieldName("rationale".into()))
            .expect("rationale present");
        let body_contrib = rationale
            .iter()
            .flat_map(|c| c.contributions.iter())
            .find(|c| c.surface == Surface::BodyWikilink)
            .expect("wikilink contribution present");
        assert_eq!(body_contrib.section_path, vec!["1 Why".to_string()]);
    }

    // ----- body-fence content-form, [[type-instance body contribution::au-type-system]] -----

    fn slot(src: &str) -> au_grammar::Shape {
        au_grammar::parse_shape(src).expect("shape parses")
    }

    fn read(body: &str, shape_src: &str) -> ContributionValue {
        read_fence_content(body, Some(&slot(shape_src)))
    }

    fn text_of(v: &ContributionValue) -> &str {
        match v {
            ContributionValue::Scalar(InstanceValue::String(s)) => s.as_str(),
            other => panic!("expected verbatim text, got {other:?}"),
        }
    }

    /// The whole point of the form: a `String` slot carries many lines, and the
    /// value is exactly the lines between the delimiters.
    #[test]
    fn string_slot_reads_a_fence_verbatim() {
        // As the scanner hands it over: a leading terminator from the open-fence
        // line, and a trailing one before the close.
        let body = "\nfirst paragraph\n\nsecond paragraph\n";
        assert_eq!(
            text_of(&read(body, "String")),
            "first paragraph\n\nsecond paragraph",
            "blank lines and interior structure survive; only the delimiters' own terminators go"
        );
    }

    /// Verbatim means verbatim: content that WOULD parse as yaml is still text
    /// when the slot says text.
    #[test]
    fn string_slot_does_not_yaml_parse_its_content() {
        let body = "\nkey: value\nother: thing\n";
        assert_eq!(text_of(&read(body, "String")), "key: value\nother: thing");
    }

    /// No trimming, no dedenting, no yaml scalar folding.
    #[test]
    fn verbatim_preserves_indentation_and_inner_blank_lines() {
        let body = "\n  indented\n\n    deeper\n";
        assert_eq!(text_of(&read(body, "String")), "  indented\n\n    deeper");
    }

    /// A record slot is unchanged by the content-form work.
    #[test]
    fn record_slot_still_reads_a_record() {
        let body = "\ntype: myRec\ninner: x\n";
        match read(body, "myRec") {
            ContributionValue::InlineRecord(InstanceValue::Mapping(_)) => {}
            other => panic!("expected an inline record, got {other:?}"),
        }
    }

    /// [[type-def shape any::au-type-system]]: an `any` slot imposes no type, so a fence at one is
    /// never parsed into a record even when its content is a mapping.
    #[test]
    fn any_slot_reads_verbatim_not_a_record() {
        let body = "\nkey: not a mapping value here\n";
        assert_eq!(text_of(&read(body, "any")), "key: not a mapping value here");
        // `any&` behaves the same: its inline branch is opaque.
        assert_eq!(
            text_of(&read(body, "any&")),
            "key: not a mapping value here"
        );
    }

    /// A non-`String` primitive must PARSE, so the fence and the inline marker
    /// produce equal values for identical content and therefore collapse.
    #[test]
    fn number_slot_parses_the_fence_scalar() {
        assert_eq!(
            read("\n42\n", "Number"),
            ContributionValue::Scalar(InstanceValue::Integer(42))
        );
        // The collapse invariant this exists to protect: same content, same
        // value, whichever carrier authored it.
        assert_eq!(
            read("\n42\n", "Number"),
            ContributionValue::Scalar(parse_inline_scalar("42"))
        );
    }

    /// A union admits both forms, so the CONTENT disambiguates: a mapping
    /// carrying `type:` takes the record branch.
    #[test]
    fn union_slot_takes_the_record_branch_on_a_typed_mapping() {
        let body = "\ntype: myRec\ninner: x\n";
        match read(body, "<String | myRec>") {
            ContributionValue::InlineRecord(_) => {}
            other => panic!("expected the record branch, got {other:?}"),
        }
    }

    /// The same union takes the TEXT branch for anything else, including a
    /// mapping with no `type:` key.
    #[test]
    fn union_slot_takes_the_text_branch_without_a_type_key() {
        assert_eq!(
            text_of(&read("\njust some prose\n", "<String | myRec>")),
            "just some prose"
        );
        assert_eq!(
            text_of(&read("\ninner: x\n", "<String | myRec>")),
            "inner: x",
            "a mapping without `type:` is not a record claim, so it stays text"
        );
    }

    /// The accepted residual: content that parses WHOLLY as a mapping carrying
    /// `type:` is taken as a record even when it was meant as text. Documented
    /// rather than fixed, and it fails loudly.
    #[test]
    fn union_slot_misreads_text_that_is_a_well_formed_typed_mapping() {
        let body = "\ntype: production\nreplicas: 3\n";
        match read(body, "<String | myRec>") {
            ContributionValue::InlineRecord(_) => {}
            other => panic!("expected the documented misread, got {other:?}"),
        }
    }

    /// The residual is NARROW: the whole fence must be well-formed yaml. Prose
    /// carrying a `type:`-shaped line does not parse as a mapping at all, so it
    /// stays text — the misread cannot reach ordinary prose.
    #[test]
    fn union_slot_keeps_prose_that_merely_leads_with_a_type_line() {
        let body = "\ntype: the sort of thing this covers\nand more prose\n";
        assert_eq!(
            text_of(&read(body, "<String | myRec>")),
            "type: the sort of thing this covers\nand more prose",
            "a `type:` line followed by prose is not valid yaml, so it never reaches the record branch"
        );
    }

    /// No slot (an unresolved claim) falls back to VALUE-SHAPE, the same
    /// fallback the frontmatter pathway uses, so the two surfaces still collapse.
    #[test]
    fn no_slot_falls_back_to_value_shape() {
        let mapping = read_fence_content("\ntype: myRec\ninner: x\n", None);
        match mapping {
            ContributionValue::InlineRecord(_) => {}
            other => panic!("a mapping must stay a record with no slot, got {other:?}"),
        }
        let prose = read_fence_content("\njust some prose\n", None);
        assert_eq!(text_of(&prose), "just some prose");
    }

    /// A reference-only slot admits no fence. The content is still CAPTURED so
    /// the contribution exists and `body-slot-shape-mismatch` can anchor to it;
    /// the verdict itself belongs to the validator, not to this layer.
    #[test]
    fn reference_only_slot_still_captures_the_contribution() {
        assert_eq!(text_of(&read("\nwhatever\n", "myRec*")), "whatever");
        assert_eq!(text_of(&read("\nwhatever\n", "any*")), "whatever");
    }

    /// The classifier's answer for a reference-only slot is what the validator
    /// keys its rejection on: NEITHER a record nor any text form. Pinned here so
    /// a future widening of either predicate cannot silently re-accept a fence
    /// at a slot that has no inline form.
    #[test]
    fn reference_only_slots_admit_neither_form() {
        for src in [
            "myRec*",
            "file*",
            "any*",
            "type<myRec>*",
            "<myRec | other>*",
        ] {
            let s = slot(src);
            assert!(
                !au_grammar::slot_admits_record(&s),
                "{src} must not admit a record"
            );
            assert_eq!(
                au_grammar::slot_text_form(&s),
                None,
                "{src} must not admit any text form"
            );
        }
    }

    /// The language tag carries no engine meaning, so the same content reads the
    /// same way whatever tag is written. The tag never reaches this layer.
    #[test]
    fn a_string_slot_reads_the_same_under_every_tag() {
        // `read_fence_content` takes only the body; the marker extraction that
        // precedes it accepts `yaml`, `md`, or no tag alike. This asserts the
        // consequence: content-form comes from the slot, not the info string.
        let body = "\ntype: myRec\n";
        assert_eq!(text_of(&read(body, "String")), "type: myRec");
        match read(body, "myRec") {
            ContributionValue::InlineRecord(_) => {}
            other => panic!("same content, record slot, expected a record: {other:?}"),
        }
    }
}
