//! The wire-DTO layer: the serializable shapes for the engine's reads.
//!
//! One home for the serialization shapes, reached by both renderers over the
//! held analysis: the CLI's `--introspect` output and the daemon's read
//! catalog. Pure serde shapes — they wrap au-core data without adding
//! semantics. Spans carry canonical byte offsets plus the derived `line_col`
//! rendering wherever the build holds the file's `LineIndex`, so
//! line-oriented consumers don't re-read files to correlate spans.
//!
//! Schema policy: additive evolution (new fields may appear; consumers must
//! ignore unknowns; field removal / rename / type change bumps the schema
//! version the renderer stamps).

use serde::{Deserialize, Serialize};

use au_core::CrossRepoResolver;
use au_core::TypeId;
use au_core::{
    closure_of, enumerate_nested_records, folded_closure_ids, splice_effective_body, BodyItem,
    BodyTemplate, Contribution, ContributionValue, EffectiveShape, FieldDecl, FieldOrigin,
    FillsContract, InlineValue, Instance, InstanceField, InstanceValue, MetaBlock, PathSegment,
    Surface, TypeClaim, TypeDef, TypeGraph, TypeName, TypeNameClaim, ValueContainer,
};
use au_diagnostics::{ByteRange, LineColRange, LineIndex};
use au_grammar::{CompoundRefOp, DefBound, RefMode, Shape};
use au_parser::{scan_body, BodyEvent};

use std::path::{Path, PathBuf};

use crate::parse::FileParse;
use crate::repo::{MemberRole, RepoName};
use crate::KnowledgeBase;

#[derive(Debug, Serialize)]
pub struct GraphIntrospection {
    pub types: Vec<TypeIntrospection>,
}

#[derive(Debug, Serialize)]
pub struct TypeIntrospection {
    pub name: String,
    /// The type's identity hash, the closure-hash half of its `TypeId`, hex.
    /// Content-complete over the referenced closure, the same identity
    /// `instances_of` / `type_sites` carry. Two same-named results across repos
    /// are the SAME type iff their `(name, hash)` match, so a consumer
    /// distinguishes them inline, with no `type_sites` fan-out.
    pub hash: String,
    pub parents: Vec<String>,
    /// `Some(branches)` if this type-def is sealed; `None` otherwise.
    /// Empty `Some(vec![])` is impossible — sealed implies non-empty.
    pub sealed: Option<Vec<String>>,
    /// `true` when the type-def declares `abstract: true`, a non-claimable base
    /// ([[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]]).
    /// `sealed` is separate; a consumer's non-claimable check is `abstract || sealed`.
    #[serde(rename = "abstract")]
    pub is_abstract: bool,
    /// The type-def's own `required:` meta obligations, authored forms (with any
    /// `::repo`), empty when none. Every non-abstract type whose closure includes
    /// this def must carry each named meta ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
    pub required_meta: Vec<String>,
    /// For a non-abstract type, the required-meta obligations it does NOT satisfy,
    /// meta type names, sorted. Empty when satisfied or exempt. The read half of
    /// the `subtype-missing-required-meta` computation. Empty on a read with no
    /// resolution graph (the low-level `introspect_graph`).
    pub unmet_required_meta: Vec<String>,
    pub fields: Vec<FieldIntrospection>,
    /// Declared meta sub-regions on this type-def. Distinguishes spec [[type-def meta::au-type-system]]'s
    /// three states:
    /// - `None` → `meta:` key absent.
    /// - `Some(vec![])` → explicit `meta: []` suppression marker — stops
    ///   consumer walks at this node.
    /// - `Some(vec![..])` → one entry per declared sub-region.
    ///
    /// Resolved walks (consumer-facing `lookup_meta` results) are NOT
    /// serialized — clients compose them from this raw per-TypeDef data
    /// at their layer if needed.
    pub meta_blocks: Option<Vec<MetaBlockIntrospection>>,
    /// Source-form `body:` template per [[type-def body::au-type-system]]. `None` when the type-def
    /// has no `body:` key. `Some(vec![])` for explicit empty `body: []`
    /// (treated as "no template" by validation but distinct on the
    /// wire). `Some(non-empty)` for declared templates.
    pub body: Option<Vec<BodyItemIntrospection>>,
    /// Post-splice template — `body:` with every top-level `use: T`
    /// resolved recursively against the type graph. `None` when this
    /// type-def carries no body of its own.
    pub effective_body: Option<Vec<BodyItemIntrospection>>,
    /// The type-def's own `#:` docstring, a leading block before the first
    /// key, absent when none. Advisory, never validated. Spec [[type docstring::au-type-system]].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// The brand this def declares, when it names a `shape:` instead of `fields:`
    /// ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
    /// `None` for a record. Additive; a consumer that does not model brands
    /// ignores it, and `fields` is empty for a brand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<BrandIntrospection>,
    /// The type-def's `location:` block, where its instances live
    /// ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]).
    /// `None` when the type declares no `location:`. Additive; a consumer that
    /// does not model placement ignores it. Advisory, out of the identity hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationSpecIntrospection>,
    pub source: SourceLoc,
}

/// A type-def's `location:` block on the wire, the raw authored forms
/// ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]).
/// Every sub-key optional; `strict` defaults `false`.
#[derive(Debug, Serialize)]
pub struct LocationSpecIntrospection {
    /// The `name` template's raw source, e.g. `"${.type} - ${.slug}"`. Absent
    /// when the block declares no `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `path` glob's raw source, e.g. `"**/plan/"`. Absent when no `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The `fileType` pin, `"md"` or `"yaml"`. Absent when no `fileType`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    /// `true` when the block declares `strict: true`, a mandatory placement
    /// (a mismatch is an error, not a warning).
    pub strict: bool,
}

impl LocationSpecIntrospection {
    fn from_spec(spec: &au_core::location::LocationSpec) -> Self {
        LocationSpecIntrospection {
            name: spec.name.as_ref().map(|n| n.raw.clone()),
            path: spec.path.as_ref().map(|p| p.raw.clone()),
            file_type: spec.file_type.map(|ft| ft.extension().to_string()),
            strict: spec.strict,
        }
    }
}

/// A brand's underlying shape on the wire ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
/// Present on a `TypeIntrospection` whose def declares `shape:`.
#[derive(Debug, Serialize)]
pub struct BrandIntrospection {
    /// The underlying shape as a structured AST: a scalar `Primitive` / `Refined`,
    /// a named `Enum`, a named union (`Union` / `CompoundReference`), or a tuple.
    /// Same `WireShape` a field's `shape_ast` uses. Spec [[spec - shape ast on the wire]].
    pub shape: WireShape,
    /// Per-enum-member `#:` docstrings, keyed by member literal. Empty for a
    /// non-enum brand or a docless enum. Advisory. Spec [[type docstring::au-type-system]].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub member_docs: std::collections::BTreeMap<String, String>,
}

/// One body-template item — spec [[type-def body::au-type-system]] discriminator surface.
///
/// `kind` discriminator: `"use"` / `"section"` / `"fills"`. A type-def's
/// `body:` value renders as `Vec<BodyItemIntrospection>`; each Section's
/// nested `body:` recurses with the same shape.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BodyItemIntrospection {
    /// `- use: T` — splice another type-def's body inline. Only valid at
    /// the top-level body (the parser rejects nested use:; this field
    /// surfaces the source-form item, so the same restriction holds in
    /// the wire shape).
    Use { target: String },
    /// `- section: Name` / `- section?: Name`. `optional` is `true` for
    /// the `section?:` form. `fills` carries the section-level contract
    /// if declared. `guidance` is the free-form authoring hint, if any.
    /// `body` is the nested template under this section (`None` when no
    /// `body:` was declared at this section; `Some([])` when declared but
    /// empty).
    Section {
        name: String,
        optional: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        fills: Option<FillsContractIntrospection>,
        #[serde(skip_serializing_if = "Option::is_none")]
        guidance: Option<String>,
        body: Option<Vec<BodyItemIntrospection>>,
    },
    /// `- fills:` / `- fills!:` as a bare body item — a contract that
    /// applies to the enclosing scope rather than to a specific section.
    Fills {
        contract: FillsContractIntrospection,
    },
}

/// [[type-def body fills::au-type-system]] fills contract. `fields` is the declared field-name list (always
/// non-empty for parsed contracts; the parser elides empty lists). The
/// `!:` form sets `exclusive = true`.
#[derive(Debug, Serialize)]
pub struct FillsContractIntrospection {
    pub fields: Vec<String>,
    pub exclusive: bool,
}

#[derive(Debug, Serialize)]
pub struct FieldIntrospection {
    pub name: String,
    /// Source-form slot expression (e.g. `String`, `[low, moderate]`,
    /// `decision*[]`). `Ok` shapes render via `Shape: Display`; `Err`
    /// shapes (deferred grammar features, syntax errors) fall back to
    /// the user-written `raw_shape`.
    pub shape: String,
    /// The parsed slot shape as a structured AST, beside the source-form
    /// `shape` string. `null` when `parsed_shape` is `Err` — the
    /// `shape-syntax-error` diagnostic carries the failure. Additive; a
    /// string-only consumer ignores it. Spec [[spec - shape ast on the wire]].
    pub shape_ast: Option<WireShape>,
    pub required: bool,
    /// The field-name byte span in the owning type-def's source file, for
    /// go-to-def onto the field's declaration line (the file is the type-def's
    /// `source.file`). Same `SpanRange` shape as `SourceLoc.span`, file-relative.
    /// Present on the `types` / `type` reads; absent on `type_closure`, whose
    /// fields are gathered across origins without a per-origin source site.
    /// Additive; a consumer that does not navigate ignores it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_span: Option<SpanRange>,
    /// The field's `#:` docstring, absent when none. Advisory, never
    /// validated. Spec [[type docstring::au-type-system]].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

/// Structured serialization of a slot `Shape` ([[type-def field shape::au-type-system]]) — the parsed AST
/// beside the source-form `shape` string. A tagged union on `kind`,
/// mirroring `au_grammar::Shape` faithfully. Consumers map structure to
/// their target (a codegen target type, a semantic-token color) without
/// re-parsing the slot grammar. Spec [[spec - shape ast on the wire]].
///
/// References carry bare names, not nested shapes: only `List` wraps an
/// inner `WireShape`. The built-in any-repo-file is `Reference { name:
/// "file" }`, there is no dedicated `file` kind.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WireShape {
    /// `String` | `Number` | `Boolean` | `Date` | `DateTime` | `Url`.
    Primitive { name: &'static str },
    /// The no-type inline slot, bare `any` ([[type-def shape any::au-type-system]]). No payload.
    /// The reference forms `any*` / `any&` are `Reference` / `InlineOrReference`
    /// with `name: "any"`, not this kind.
    Any,
    /// The uninterpreted inline slot, bare `opaque` ([[type-def shape opaque::au-type-system]]).
    /// No payload. Inline-only, there is no `opaque*` / `opaque&`. Additive on
    /// the wire, a consumer that does not know it treats it as an unconstrained
    /// slot like `any`.
    Opaque,
    /// Inline closed enum, members in declaration order.
    Enum { members: Vec<String> },
    /// Typed reference `name*`. `name: "file"` is the any-repo-file built-in.
    Reference { name: String },
    /// Bare-name inline record `name`.
    Record { name: String },
    /// Inline-or-reference `name&`.
    InlineOrReference { name: String },
    /// List `inner[min..max]` ([[type-def shape suffixes::au-type-system]]). `min` is the
    /// inclusive lower bound on the element count, `max` the inclusive upper
    /// (`null` is unbounded above). The source suffixes desugar: `[]` is
    /// `{min:0}`, `[+]` is `{min:1}`, `[n]` is `{min:n, max:n}`, `[..m]` is
    /// `{min:0, max:m}`, `[x..y]` is `{min:x, max:y}`.
    List {
        min: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
        inner: Box<WireShape>,
    },
    /// Slot-level union `<A | B>`, branch order significant.
    Union { branches: Vec<WireShape> },
    /// Slot-level intersection `<A & B>`, branch order significant.
    Intersection { branches: Vec<WireShape> },
    /// Reference over a compound of bare names, `<a | b>*` / `<a & b>&`.
    CompoundReference {
        mode: WireRefMode,
        op: WireCompoundOp,
        branches: Vec<String>,
    },
    /// Typed reference to a type-def, `type<T>*` / `type*` ([[type-def shape def-ref::au-type-system]]).
    /// `bound` is the def-axis ceiling: absent for the unconstrained `type*`,
    /// present for the constrained `type<T>*`. Reference-only, no inline form.
    DefReference {
        #[serde(skip_serializing_if = "Option::is_none")]
        bound: Option<WireDefBound>,
    },
    /// Commit-pinned reference, the `*@` postfix ([[type-def shape suffixes::au-type-system]]). Wraps
    /// the inner reference shape; every value must carry a `@commit` pin. The
    /// second wrapper kind beside `List`. Spec
    /// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    Pinned { inner: Box<WireShape> },
    /// Value refinement `Base{predicate}` ([[type-def field shape::au-type-system]]). `base` is
    /// the refinable primitive name, `refinement` the predicate meet.
    Refined {
        base: &'static str,
        refinement: WireRefinement,
    },
    /// Tuple `(A, B, ...)` ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]),
    /// a fixed-arity positional product. `elements` are the per-position shapes,
    /// order significant. Additive wire shape.
    Tuple { elements: Vec<WireShape> },
}

/// A value refinement on the wire ([[type-def field shape::au-type-system]]): comparison bounds,
/// an `integer` flag, and a regex pattern, at most one of each. Absent members
/// are omitted.
///
/// A flat bag, faithful to the engine's internal `Refinement` meet. The base
/// (on the enclosing `Refined`) determines which members appear — `String` only
/// `pattern`, `Number` only `lower` / `upper` / `integer`, `Date` / `DateTime`
/// only `lower` / `upper` — a producer invariant the engine always upholds, so a
/// consumer may narrow by base. See `WIRE.md`, the `refined` kind.
#[derive(Debug, Serialize)]
pub struct WireRefinement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lower: Option<WireBound>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper: Option<WireBound>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub integer: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// A comparison bound in a [`WireRefinement`]. `inclusive` is `>=` / `<=`
/// versus the strict `>` / `<`.
#[derive(Debug, Serialize)]
pub struct WireBound {
    pub value: String,
    pub inclusive: bool,
}

/// The ceiling inside a `type<...>*` def-reference ([[type-def shape def-ref::au-type-system]]).
/// `single` is `type<T>*`; `compound` is `type<a | b>*` / `type<a & b>*`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WireDefBound {
    Single {
        name: String,
    },
    Compound {
        op: WireCompoundOp,
        branches: Vec<String>,
    },
}

/// The suffix mode on a compound reference. `ref` is `*`, `inline-or-ref` is `&`.
#[derive(Debug, Serialize)]
pub enum WireRefMode {
    #[serde(rename = "ref")]
    Ref,
    #[serde(rename = "inline-or-ref")]
    InlineOrRef,
}

/// The operator on a compound reference.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireCompoundOp {
    Union,
    Intersection,
}

fn wire_bound(b: &au_grammar::Bound) -> WireBound {
    WireBound {
        value: b.value.clone(),
        inclusive: b.inclusive,
    }
}

impl From<&Shape> for WireShape {
    fn from(shape: &Shape) -> Self {
        match shape {
            Shape::Primitive(p) => WireShape::Primitive { name: p.as_str() },
            Shape::Any => WireShape::Any,
            Shape::Opaque => WireShape::Opaque,
            Shape::Enum(members) => WireShape::Enum {
                members: members.clone(),
            },
            Shape::Reference(name) => WireShape::Reference {
                name: name.to_string(),
            },
            Shape::Record(name) => WireShape::Record {
                name: name.to_string(),
            },
            Shape::InlineOrReference(name) => WireShape::InlineOrReference {
                name: name.to_string(),
            },
            Shape::List { inner, min, max } => WireShape::List {
                min: *min,
                max: *max,
                inner: Box::new(WireShape::from(inner.as_ref())),
            },
            Shape::Refined { base, refinement } => WireShape::Refined {
                base: base.as_str(),
                refinement: WireRefinement {
                    lower: refinement.lower.as_ref().map(wire_bound),
                    upper: refinement.upper.as_ref().map(wire_bound),
                    integer: refinement.integer,
                    pattern: refinement.pattern.clone(),
                },
            },
            Shape::Union(branches) => WireShape::Union {
                branches: branches.iter().map(WireShape::from).collect(),
            },
            Shape::Intersection(branches) => WireShape::Intersection {
                branches: branches.iter().map(WireShape::from).collect(),
            },
            Shape::CompoundReference { mode, op, branches } => WireShape::CompoundReference {
                mode: WireRefMode::from(*mode),
                op: WireCompoundOp::from(*op),
                branches: branches.iter().map(|n| n.to_string()).collect(),
            },
            Shape::DefReference(bound) => WireShape::DefReference {
                bound: bound.as_ref().map(WireDefBound::from),
            },
            Shape::Pinned(inner) => WireShape::Pinned {
                inner: Box::new(WireShape::from(inner.as_ref())),
            },
            Shape::Tuple(elements) => WireShape::Tuple {
                elements: elements.iter().map(WireShape::from).collect(),
            },
        }
    }
}

impl From<&DefBound> for WireDefBound {
    fn from(bound: &DefBound) -> Self {
        match bound {
            DefBound::Single(name) => WireDefBound::Single {
                name: name.to_string(),
            },
            DefBound::Compound { op, branches } => WireDefBound::Compound {
                op: WireCompoundOp::from(*op),
                branches: branches.iter().map(|n| n.to_string()).collect(),
            },
        }
    }
}

impl From<RefMode> for WireRefMode {
    fn from(mode: RefMode) -> Self {
        match mode {
            RefMode::Star => WireRefMode::Ref,
            RefMode::Inline => WireRefMode::InlineOrRef,
        }
    }
}

impl From<CompoundRefOp> for WireCompoundOp {
    fn from(op: CompoundRefOp) -> Self {
        match op {
            CompoundRefOp::Union => WireCompoundOp::Union,
            CompoundRefOp::Intersection => WireCompoundOp::Intersection,
        }
    }
}

/// Introspection of one meta sub-region: the named meta-type-def
/// (`type:` discriminator), its body fields (name + value), and the
/// sub-region's source span. Body values surface as JSON via
/// `serde_json::Value` — scalars render directly, sequences as arrays,
/// nested inline values as objects (with their own `"type"` key when
/// declared). Consumers (e.g. UIs) can render meta content directly
/// without re-parsing the source YAML.
///
/// Source span uses `block_span` (whole sub-region including the `type:`
/// discriminator). Body-only precision would require splitting the span
/// at parse time — see `MetaBlock`'s `body_span = block_span` simplification.
#[derive(Debug, Serialize)]
pub struct MetaBlockIntrospection {
    pub type_name: String,
    pub body: Vec<MetaFieldIntrospection>,
    pub source: SourceLoc,
}

/// One body field inside a meta sub-region: key name + value as JSON.
/// Multi-key shapes (sequence, nested mapping) recurse through
/// `instance_value_to_json`; see that helper for the conversion rules.
#[derive(Debug, Serialize)]
pub struct MetaFieldIntrospection {
    pub name: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct SourceLoc {
    pub file: String,
    pub span: SpanRange,
}

/// Half-open byte range. Offsets are UTF-8 bytes from the start of the
/// containing file. Single shape across every span on the wire —
/// `SourceLoc.span`, `LocationIntrospection.byte_range`, body event
/// spans, section_presence spans all serialize as
/// `{ "start": N, "end": M, "line_col": { .. } }`.
///
/// Byte offsets are canonical; `line_col` is the derived 1-based rendering
/// (columns in UTF-8 bytes), attached wherever the build holds the file's
/// `LineIndex`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SpanRange {
    pub start: usize,
    pub end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_col: Option<LineColRange>,
}

impl SpanRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            line_col: None,
        }
    }

    /// Attach the line/column rendering from the containing file's index;
    /// a `None` index leaves the span byte-only.
    pub fn located(mut self, lines: Option<&LineIndex>) -> Self {
        if let Some(idx) = lines {
            self.line_col = Some(idx.line_col_range(ByteRange::new(self.start, self.end)));
        }
        self
    }
}

impl From<au_diagnostics::ByteRange> for SpanRange {
    fn from(r: au_diagnostics::ByteRange) -> Self {
        Self::new(r.start, r.end)
    }
}

/// Where a type-def's source lives for the wire: the file to name and that
/// file's line index.
struct SourceSite<'a> {
    file: String,
    lines: Option<&'a LineIndex>,
}

impl<'a> SourceSite<'a> {
    /// A plain file: its own path and whatever index is at hand.
    fn plain(path: &Path, lines: Option<&'a LineIndex>) -> Self {
        SourceSite {
            file: path.display().to_string(),
            lines,
        }
    }

    /// The site for a type-def held in a knowledge base: the def's own path.
    fn of_def(kb: &'a KnowledgeBase, def: &TypeDef) -> Self {
        SourceSite::plain(&def.source_path, kb.line_index(&def.source_path))
    }
}

#[derive(Debug, Serialize)]
pub struct InstanceIntrospection {
    pub file: String,
    pub claim: Vec<String>,
    pub closure: Vec<String>,
    pub effective_shape: Vec<EffectiveShapeEntry>,
    /// [[type value container::au-type-system]] effective values — per-field `ValueContainer`s carrying every
    /// contribution surface (frontmatter / body wikilink / body fence
    /// / body inline code). One entry per field that has at least one
    /// contribution; absent for unfilled closure fields.
    pub effective_values: Vec<FieldValuesEntry>,
    /// [[type-def body section::au-type-system]] presence of each top-level declared section in the instance's
    /// effective body template. `None` when the instance's type has no
    /// `body:` template; `Some(vec![])` for a body-declaring type whose
    /// template carries no `section:` items (e.g. body-level `fills:`
    /// only). Nested sections are NOT enumerated here — only the
    /// validator's top-level coverage is exposed in v1.
    pub section_presence: Option<Vec<SectionPresenceEntry>>,
    /// Raw markdown body event stream per [[type value container::au-type-system]] / au-parser.
    /// `None` for pure-YAML instances (no body to scan); `Some(vec![])`
    /// for markdown instances whose body produces no events. Spans are
    /// absolute byte offsets (already shifted by the body start).
    pub body_events: Option<Vec<BodyEventIntrospection>>,
    /// Addressable inline records ([[type block-id::au-type-system]]): `^:` id, effective
    /// claim names (explicit or slot-pinned), the id value's span.
    /// Body-side markers and fence ids ride `body_events`; together the
    /// two lists are the file's addressable-id surface (completion for
    /// `[[^`). Omitted when the instance carries none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub record_block_ids: Vec<RecordBlockIdIntrospection>,
}

/// One addressable inline record per [[type block-id::au-type-system]].
#[derive(Debug, Serialize)]
pub struct RecordBlockIdIntrospection {
    pub id: String,
    /// Effective claim names, explicit or slot-pinned; empty for a
    /// claim-less record (diagnosed at the file).
    pub claims: Vec<String>,
    pub span: SpanRange,
}

/// One field's contribution set per [[type value container::au-type-system]].
#[derive(Debug, Serialize)]
pub struct FieldValuesEntry {
    pub field: String,
    pub containers: Vec<ValueContainerIntrospection>,
}

/// One unique value plus the contributions that produced it. Multiple
/// contributions with equal values collapse into one container per [[type value container::au-type-system]].
#[derive(Debug, Serialize)]
pub struct ValueContainerIntrospection {
    pub value: ContributionValueIntrospection,
    pub contributions: Vec<ContributionIntrospection>,
}

/// One contribution: surface + location + section-path + resolved value.
#[derive(Debug, Serialize)]
pub struct ContributionIntrospection {
    pub surface: SurfaceIntrospection,
    pub location: LocationIntrospection,
    /// Root-to-leaf section chain enclosing the contribution; empty for
    /// frontmatter and body-preamble contributions. Each entry carries
    /// the 1-based sibling index at its level plus the heading text.
    /// Engine-internal representation is `"<index> <text>"`; the wire
    /// surfaces a structured `{ index, text }` so consumers don't
    /// re-parse the string. Same-name siblings disambiguate via the
    /// `index` field; the literal-`1`-heading edge case from [[type value container::au-type-system]]
    /// (`# 1 Why` → `SectionPathSegment { index: 1, text: "1 Why" }`)
    /// stays unambiguous.
    pub section_path: Vec<SectionPathSegment>,
    pub value: ContributionValueIntrospection,
    /// The collision qualifier a body attribution carried ([[type-def fields collision - auto-unify and qualified field::au-type-system]]):
    /// `field{type}` / `field{type::repo}`, naming which divergent origin this
    /// contribution fills. Absent for a bare attribution or a frontmatter
    /// contribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<QualifierIntrospection>,
}

/// The `{type}` / `{type::repo}` qualifier on a body attribution.
#[derive(Debug, Serialize)]
pub struct QualifierIntrospection {
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// One root-to-leaf section path entry. Index is 1-based per [[type value container::au-type-system]].
#[derive(Debug, Serialize)]
pub struct SectionPathSegment {
    pub index: u32,
    pub text: String,
}

/// Which surface the contribution originated from.
///
/// Serializes as `"frontmatter"` / `"body_wikilink"` / `"body_fence"`
/// / `"body_inline_code"`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceIntrospection {
    Frontmatter,
    BodyWikilink,
    BodyFence,
    BodyInlineCode,
}

/// File + byte range of a contribution. Byte range serializes as
/// `{ "start": …, "end": … }` (half-open, UTF-8 bytes, matches
/// `SourceLoc` and every other span on the wire).
#[derive(Debug, Serialize)]
pub struct LocationIntrospection {
    pub file: String,
    pub byte_range: SpanRange,
}

/// Resolved contribution value.
///
/// `kind` discriminator: `"scalar"` carries the originating value as JSON;
/// `"reference"` carries the wikilink target + optional anchor + optional
/// block-id; `"inline_record"` carries a parsed mapping as JSON. The
/// wikilink `:field` fragment is identity (attribution), not a value, so
/// it is intentionally NOT carried under `reference`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContributionValueIntrospection {
    Scalar {
        value: serde_json::Value,
        /// The brand constructor written for this value, `"meter"` for a written
        /// `meter(5)` (a peer brand keeps its qualifier, `"meter::units"`), the
        /// discriminator at a union brand and a round-trip signal elsewhere. The
        /// `value` is always the resolved underlying form, so a bare value
        /// (`5`) and a reserved-primitive escape (`String("x")`) carry no brand.
        /// Omitted when absent. See
        /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
        #[serde(skip_serializing_if = "Option::is_none")]
        brand: Option<String>,
    },
    Reference {
        target: String,
        anchor: Option<String>,
        /// The block-id fragment with its mode: `{ id, referent }`. `referent`
        /// is `true` for a `^^id` block-referent (the block's value fills the
        /// slot), `false` for a bare `^id` (navigational, the file is the value).
        block_id: Option<au_references::BlockId>,
        /// The `::repo` qualifier when the wikilink target is cross-repo, e.g.
        /// `[[bar::other]]`. Omitted for an own-repo reference. Carries the same
        /// `::repo` the sibling reference surfaces (`references_out`,
        /// `body_events.wikilink.parsed`) already expose, so two contributions
        /// that differ only by repo stay distinguishable on the wire.
        #[serde(skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        /// The `@commit` pin when the reference is commit-pinned, e.g.
        /// `[[bar::@a1b2c3d]]`. Omitted for an unpinned reference. Additive, no
        /// `SCHEMA_VERSION` bump.
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    /// An inline-record value: a nested mapping validated as a record. `fields`
    /// carries each nested field resolved through the value model, exactly like a
    /// top-level field, recursively to any depth — so a tuple / brand / reference
    /// inside a nested record reads resolved, not as a raw string. Replaces the
    /// former raw-mapping `value`. See
    /// [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
    InlineRecord {
        fields: Vec<InlineRecordFieldIntrospection>,
    },
    /// A fixed-arity positional product ([[type-def shape tuple::au-type-system]]). `elements`
    /// carries each position's resolved value and its optional written brand;
    /// `brand` is the outer tuple's written brand. Additive value kind, no
    /// `SCHEMA_VERSION` bump.
    Tuple {
        elements: Vec<TupleElementIntrospection>,
        #[serde(skip_serializing_if = "Option::is_none")]
        brand: Option<String>,
    },
    /// A value that started a `Name(...)` constructor at a brand or tuple slot
    /// but did not close well-formed. `raw` is the surface. Additive value kind.
    MalformedConstructor { raw: String },
    /// A malformed `[[...]]` at a reference slot. `raw` is the surface. Additive
    /// value kind.
    MalformedReference { raw: String },
}

/// One element of a [`ContributionValueIntrospection::Tuple`], its resolved
/// value and the brand written for it, if any.
#[derive(Debug, Serialize)]
pub struct TupleElementIntrospection {
    pub value: ContributionValueIntrospection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
}

/// One field of a [`ContributionValueIntrospection::InlineRecord`], resolved
/// through the value model like a top-level field. `values` is the field's list
/// of resolved value containers (one for a scalar, N for a list slot), each a
/// recursively-resolved [`ContributionValueIntrospection`].
#[derive(Debug, Serialize)]
pub struct InlineRecordFieldIntrospection {
    pub field: String,
    pub values: Vec<ContributionValueIntrospection>,
}

/// One declared top-level section's coverage in the instance.
///
/// `present` is `true` iff a heading matching `name` appears at the
/// expected nesting depth in the body. `span` carries the matched
/// heading's absolute byte range when present; `None` when absent.
///
/// `depth` is 1 for top-level declared sections, 2 for sub-sections,
/// etc. `path` carries the stripped name-only path from the body root
/// to this section.
#[derive(Debug, Serialize)]
pub struct SectionPresenceEntry {
    pub name: String,
    pub optional: bool,
    pub depth: u8,
    pub path: Vec<String>,
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SpanRange>,
}

/// One raw body event. `kind` discriminator mirrors `au_parser::BodyEvent`.
///
/// Wikilink events carry a `parsed` breakdown of the canonical
/// `target[::repo][@commit][#anchor][^block_id][:field]` fragment grammar
/// (`@commit` binds to `::repo`) — `None` when the raw text fails to parse
/// (malformed input still surfaces with `raw`, satisfying consumers that only
/// need the source bytes).
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BodyEventIntrospection {
    Heading {
        level: u8,
        text: String,
        span: SpanRange,
    },
    FencedBlock {
        info: String,
        body: String,
        span: SpanRange,
        #[serde(skip_serializing_if = "Option::is_none")]
        trailing_block_id: Option<String>,
    },
    InlineCode {
        content: String,
        span: SpanRange,
    },
    Wikilink {
        raw: String,
        span: SpanRange,
        #[serde(skip_serializing_if = "Option::is_none")]
        parsed: Option<au_references::WikilinkRef>,
    },
    BlockIdMarker {
        id: String,
        span: SpanRange,
    },
    UnterminatedFenceOpen {
        info: String,
        span: SpanRange,
    },
}

#[derive(Debug, Serialize)]
pub struct EffectiveShapeEntry {
    pub field: String,
    /// The canonical (lex-min origin) shape. For a DIVERGENT field there is no
    /// single shape, so this is one origin's, and `divergent` is true — read the
    /// per-origin `origins[].shape` instead.
    pub shape: String,
    pub required: bool,
    /// True when the origins do NOT agree on shape ([[type-def fields collision - auto-unify and qualified field::au-type-system]]): a divergent
    /// field, resolved per-origin by a `field{type}` qualifier. Each origin's own
    /// shape is on `origins[].shape`.
    pub divergent: bool,
    /// One entry per contributing origin. Several under mixin auto-unify or a
    /// divergent field; single for a plain single-claim instance.
    pub origins: Vec<OriginEntry>,
}

#[derive(Debug, Serialize)]
pub struct OriginEntry {
    /// The origin's BARE type name. For a divergent cross-repo field two origins
    /// can share this (`note` vs `note::base`); `repo` distinguishes them.
    pub name: String,
    /// The owner repo for a folded peer origin (`Some("base")` for `note::base`),
    /// `None` for an own-graph origin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub origin_path: String,
    /// This origin's OWN declared shape and required bit — they differ across
    /// origins of a divergent field.
    pub shape: String,
    pub required: bool,
}

pub fn introspect_graph(graph: &TypeGraph) -> GraphIntrospection {
    // Low-level, no resolution graph: `unmet_required_meta` renders empty. The
    // knowledge-base-keyed entry points supply the repo's resolution graph.
    introspect_graph_with(
        graph,
        &|def| SourceSite::plain(&def.source_path, None),
        None,
        None,
    )
}

/// [`introspect_graph`] with a per-def source-site lookup, so spans carry
/// their line/column rendering. The knowledge-base-keyed entry points thread
/// the catalog through here.
///
/// `peer` is the body-splice seam so a type-def's served `effective_body` splices
/// a cross-repo `use: parent::repo`; `None` splices own bodies only.
fn introspect_graph_with<'a>(
    graph: &TypeGraph,
    site_of: &impl Fn(&TypeDef) -> SourceSite<'a>,
    peer: Option<&dyn au_core::CrossRepoResolver>,
    resolution: Option<&au_core::ResolutionGraph>,
) -> GraphIntrospection {
    let types = graph
        .iter()
        .map(|(name, def)| introspect_type(graph, name, def, &site_of(def), peer, resolution))
        .collect();
    GraphIntrospection { types }
}

fn introspect_type(
    graph: &TypeGraph,
    name: &TypeName,
    def: &TypeDef,
    site: &SourceSite<'_>,
    peer: Option<&dyn au_core::CrossRepoResolver>,
    resolution: Option<&au_core::ResolutionGraph>,
) -> TypeIntrospection {
    let parents = def.parents.iter().map(|p| p.authored()).collect();
    let sealed_branches = graph.sealed_branches_of(name);
    let sealed = if sealed_branches.is_empty() {
        None
    } else {
        Some(sealed_branches.iter().map(|s| s.authored()).collect())
    };
    let fields = def
        .fields
        .iter()
        .map(|f| introspect_field(f, Some(site)))
        .collect();
    // Map `Option<Vec<MetaBlock>>` to `Option<Vec<MetaBlockIntrospection>>`
    // preserving all three [[type-def meta::au-type-system]] states (None / Some(vec![]) / Some(vec![..])).
    // The serialized JSON renders these as `null` / `[]` / `[..]` respectively.
    //
    // `MetaBlock` doesn't carry its own file path; the host TypeDef's
    // source site is the natural anchor for its sub-regions.
    let meta_blocks = def.meta_blocks.as_ref().map(|blocks| {
        blocks
            .iter()
            .map(|b| introspect_meta_block(b, site))
            .collect()
    });
    let body = def
        .body
        .as_ref()
        .map(|tmpl| body_template_to_introspection(tmpl));
    let effective_body =
        splice_effective_body(graph, name, peer).map(|tmpl| body_template_to_introspection(&tmpl));
    let required_meta = def.required_meta.iter().map(|r| r.authored()).collect();
    // The unmet-obligation set, computed over the repo's resolution graph. Empty
    // without one (the low-level `introspect_graph`), or when satisfied / exempt.
    let unmet_required_meta = resolution
        .map(|rg| {
            let mut names: Vec<String> = au_core::unmet_required_meta(graph, rg, def)
                .iter()
                .map(|u| u.meta.name.as_str().to_string())
                .collect();
            names.sort();
            names.dedup();
            names
        })
        .unwrap_or_default();
    TypeIntrospection {
        name: name.as_str().to_string(),
        // The identity hash, memoized per def at graph build; present for any def
        // in the graph. Rendered like the `instances_of` / `type_sites` hash.
        hash: graph
            .closure_id(name)
            .map_or_else(String::new, |h| format!("{:016x}", h.0)),
        parents,
        sealed,
        is_abstract: def.declared_abstract,
        required_meta,
        unmet_required_meta,
        fields,
        meta_blocks,
        body,
        effective_body,
        doc: def.doc.clone(),
        brand: def.shape.as_ref().map(|b| BrandIntrospection {
            shape: WireShape::from(&b.shape),
            member_docs: b.member_docs.clone(),
        }),
        location: def
            .location
            .as_ref()
            .map(LocationSpecIntrospection::from_spec),
        source: SourceLoc {
            file: site.file.clone(),
            span: SpanRange::new(def.source_span.start, def.source_span.end).located(site.lines),
        },
    }
}

fn body_template_to_introspection(template: &BodyTemplate) -> Vec<BodyItemIntrospection> {
    template.iter().map(body_item_to_introspection).collect()
}

fn body_item_to_introspection(item: &BodyItem) -> BodyItemIntrospection {
    match item {
        BodyItem::Use {
            type_name, repo, ..
        } => BodyItemIntrospection::Use {
            // Serve the authored form, `T` or `T::repo`, so a consumer sees the
            // cross-repo splice target.
            target: match repo {
                Some(r) => format!("{}::{}", type_name.as_str(), r),
                None => type_name.as_str().to_string(),
            },
        },
        BodyItem::Section {
            name,
            optional,
            fills,
            guidance,
            body,
            ..
        } => BodyItemIntrospection::Section {
            name: name.clone(),
            optional: *optional,
            fills: fills.as_ref().map(fills_contract_to_introspection),
            guidance: guidance.clone(),
            body: body
                .as_ref()
                .map(|inner| body_template_to_introspection(inner)),
        },
        BodyItem::Fills { contract, .. } => BodyItemIntrospection::Fills {
            contract: fills_contract_to_introspection(contract),
        },
    }
}

fn fills_contract_to_introspection(contract: &FillsContract) -> FillsContractIntrospection {
    FillsContractIntrospection {
        fields: contract
            .fields
            .iter()
            .map(|f| f.name.as_str().to_string())
            .collect(),
        exclusive: contract.exclusive,
    }
}

fn introspect_meta_block(block: &MetaBlock, site: &SourceSite<'_>) -> MetaBlockIntrospection {
    let body = block
        .fields
        .iter()
        .map(|f| MetaFieldIntrospection {
            name: f.key.clone(),
            value: instance_value_to_json(&f.value),
        })
        .collect();
    MetaBlockIntrospection {
        // Serve the authored form, `T` or `T::repo`, so a consumer sees a
        // cross-repo meta type.
        type_name: match &block.repo {
            Some(r) => format!("{}::{}", block.type_name.as_str(), r),
            None => block.type_name.as_str().to_string(),
        },
        body,
        source: SourceLoc {
            file: site.file.clone(),
            span: SpanRange::new(block.block_span.start, block.block_span.end).located(site.lines),
        },
    }
}

/// Convert an `InstanceValue` into a `serde_json::Value`. Recursive across
/// `Sequence` (→ JSON array) and `Mapping(InlineValue)` (→ JSON object).
///
/// Mapping shape: the inline value's `type:` claim renders under the
/// `"type"` key (Bare → JSON string; List → JSON array of strings); a
/// `^:` block-id renders under the literal `"^"` key ([[type block-id::au-type-system]]),
/// so a consumer maps `[[^id]]` references onto the records it received
/// without re-parsing the file. Body fields render as named entries
/// alongside. Spec [[type-def::au-type-system]] / [[type list form::au-type-system]] reserve `type:`
/// everywhere and field names can never be `^`, so no compliant field
/// collides — if that ever changes, this helper would silently drop the
/// colliding entry.
///
/// `NotYetSupported` (YAML constructs the AST classifier doesn't model)
/// renders as a marker string so consumers can detect coverage gaps.
fn instance_value_to_json(value: &InstanceValue) -> serde_json::Value {
    match value {
        InstanceValue::String(s) => serde_json::Value::String(s.clone()),
        InstanceValue::Integer(i) => serde_json::Value::Number((*i).into()),
        InstanceValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        InstanceValue::Boolean(b) => serde_json::Value::Bool(*b),
        InstanceValue::Null => serde_json::Value::Null,
        InstanceValue::Sequence(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|el| instance_value_to_json(&el.value))
                .collect(),
        ),
        InstanceValue::Mapping(inline) => {
            serde_json::Value::Object(inline_value_to_json_map(inline))
        }
        InstanceValue::NotYetSupported => {
            serde_json::Value::String("<not-yet-supported>".to_string())
        }
    }
}

fn inline_value_to_json_map(inline: &InlineValue) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::with_capacity(inline.fields.len() + 2);
    if let Some(decl) = &inline.block_id {
        map.insert("^".to_string(), serde_json::Value::String(decl.id.clone()));
    }
    if let Some(claim) = &inline.type_claim {
        map.insert("type".to_string(), type_claim_to_json(claim));
    }
    for f in &inline.fields {
        map.insert(f.key.clone(), instance_value_to_json(&f.value));
    }
    map
}

fn type_claim_to_json(claim: &TypeClaim) -> serde_json::Value {
    // Authored form, `T` or `T::repo`, symmetric with the top-level
    // `instances_of` claim render (WIRE §270-274): a `::repo` claim stays
    // qualified on every served surface, inline included.
    match claim {
        TypeClaim::Bare(c) => serde_json::Value::String(c.authored()),
        TypeClaim::List { items, .. } => serde_json::Value::Array(
            items
                .iter()
                .map(|c| serde_json::Value::String(c.authored()))
                .collect(),
        ),
    }
}

fn introspect_field(decl: &FieldDecl, site: Option<&SourceSite<'_>>) -> FieldIntrospection {
    // The field-name span, line-located exactly like the type-def's
    // `source.span`, so a consumer navigates with one coordinate system. `None`
    // where no source site is threaded (the `type_closure` read).
    let key_span =
        site.map(|s| SpanRange::new(decl.name_span.start, decl.name_span.end).located(s.lines));
    FieldIntrospection {
        name: decl.name.as_str().to_string(),
        shape: decl.shape_display(),
        shape_ast: decl.parsed_shape.as_ref().ok().map(WireShape::from),
        required: !decl.optional,
        key_span,
        doc: decl.doc.clone(),
    }
}

/// The instance `closure` field: the type names an instance conforms to, its
/// claim plus every transitive ancestor, each rendered relative to the INSTANCE'S
/// OWN REPO (WIRE §272). An identity the instance's own repo defines is bare, an
/// identity only a peer owns keeps its `::repo`. A diamond's two divergent
/// same-named identities surface as two distinct strings.
///
/// When the instance's repo resolves (`Some`), derive from the folded closure ids.
/// The qualifier is OWNERSHIP-relative, NOT the fold node's stored origin: an
/// in-sync identity that own and peer both define folds to ONE node whose retained
/// origin is processing-order-dependent (it can be the peer), so rendering from it
/// would call an own type `note::base`. The `own_graph` identity check (name +
/// closure-hash) is deterministic and matches how the instance's repo names the
/// type. A genuinely peer-only ancestor is absent from `own_graph`, so it falls
/// through to the fold origin (`node.authored()`), the import edge it was reached
/// by. The own-only path (no resolution graph) has no cross-repo ancestor, so its
/// bare names are already correct.
///
/// The one projection both the `instances` (`introspect_instance`) and the
/// `resolved` (`resolved_view`) reads call, so the two cannot drift.
pub fn instance_closure_authored(
    own_graph: &TypeGraph,
    resolution: Option<&au_core::ResolutionGraph>,
    claim: &TypeClaim,
    effective_shape: &EffectiveShape,
) -> Vec<String> {
    let mut closure: Vec<String> = match resolution {
        Some(rg) => folded_closure_ids(rg, claim)
            .iter()
            .filter_map(|tid| {
                rg.get(tid).map(|node| {
                    // Bare iff the instance's OWN repo defines this exact identity
                    // (name + closure-hash), so a shared in-sync type reads as the
                    // own repo names it, not by the fold node's retained origin.
                    if own_graph.closure_id(&tid.name) == Some(tid.hash) {
                        tid.name.as_str().to_string()
                    } else {
                        node.authored()
                    }
                })
            })
            .collect(),
        None => effective_shape
            .instance_closure()
            .iter()
            .map(|n| n.as_str().to_string())
            .collect(),
    };
    closure.sort();
    closure
}

#[allow(clippy::too_many_arguments)]
pub fn introspect_instance(
    instance: &Instance,
    claim: &TypeClaim,
    effective_shape: &EffectiveShape,
    effective_template: Option<&BodyTemplate>,
    body_events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    is_markdown: bool,
    lines: Option<&LineIndex>,
    // The instance's own graph and resolution graph, so a NESTED inline-record
    // value resolves its `type:` claim to an effective shape and reads resolved.
    own_graph: &TypeGraph,
    resolution: Option<&au_core::ResolutionGraph>,
    // The held knowledge base, so a slot-pinned peer record the source fold never
    // imported resolves its nested shape owner-relative (else its fields degrade
    // to untyped). Threaded to the value-layer context below.
    kb: Option<&KnowledgeBase>,
) -> InstanceIntrospection {
    // Authored form, so a `::repo` claim stays qualified (WIRE §272), matching
    // `instances_of`. It was served bare here.
    let claim_names = claim.iter().map(|c| c.authored()).collect();
    let closure = instance_closure_authored(own_graph, resolution, claim, effective_shape);
    // Resolved (auto-unified) fields plus divergent fields ([[type-def fields collision - auto-unify and qualified field::au-type-system]]), the
    // latter flagged `divergent: true` with per-origin shapes. Both surface here,
    // so a consumer reads the whole effective shape from one list.
    let effective = effective_shape
        .iter()
        .map(|(name, origin)| introspect_effective_entry(name.as_str(), origin, false))
        .chain(
            effective_shape
                .divergent()
                .map(|(name, origin)| introspect_effective_entry(name.as_str(), origin, true)),
        )
        .collect();
    let values_map = au_core::effective_values(
        instance,
        body_events,
        body_byte_offset,
        Some(effective_shape),
    );
    let ctx = ValueReadCtx {
        lines,
        source_path: &instance.source_path,
        own_graph,
        resolution,
        kb,
    };
    let effective_values = values_map
        .into_iter()
        .map(|(name, containers)| {
            let field_slot = field_element_shape(effective_shape, name.as_str());
            FieldValuesEntry {
                field: name.as_str().to_string(),
                containers: containers
                    .into_iter()
                    .map(|c| value_container_to_introspection(c, field_slot, &ctx))
                    .collect(),
            }
        })
        .collect();
    let section_presence = effective_template.map(|tmpl| {
        convert_section_presence(
            au_core::compute_section_presence(tmpl, body_events, body_byte_offset),
            lines,
        )
    });
    let body_events_intro = if is_markdown {
        Some(
            body_events
                .iter()
                .map(|e| body_event_to_introspection(e, body_byte_offset, lines))
                .collect(),
        )
    } else {
        None
    };
    InstanceIntrospection {
        file: instance.source_path.display().to_string(),
        claim: claim_names,
        closure,
        effective_shape: effective,
        effective_values,
        section_presence,
        body_events: body_events_intro,
        // Needs the type graph for slot pinning; callers holding one
        // attach it via [`record_block_ids_of`].
        record_block_ids: Vec::new(),
    }
}

/// The addressable inline records of one instance ([[type block-id::au-type-system]]),
/// for the `record_block_ids` introspection field.
pub fn record_block_ids_of(
    kb: &KnowledgeBase,
    path: &Path,
    instance: &Instance,
    lines: Option<&LineIndex>,
) -> Vec<RecordBlockIdIntrospection> {
    crate::resolution_build::record_targets_of_kb(kb, path, instance)
        .into_iter()
        .map(|(id, target)| RecordBlockIdIntrospection {
            id,
            // The qualified form, so a `::repo` record claim stays qualified (the
            // `qualified` field preserves it; `claims` is bare), matching
            // `resolve_block_id`.
            claims: target.qualified.iter().map(|c| c.authored()).collect(),
            span: SpanRange::new(target.span.start, target.span.end).located(lines),
        })
        .collect()
}

/// Project a held knowledge base's type graph into the wire shape.
///
/// A thin pass over [`introspect_graph`] keyed on the knowledge base's own graph, so the
/// daemon and the CLI render the type-graph wire from one entry point.
pub fn introspect_kb_graph(kb: &KnowledgeBase) -> GraphIntrospection {
    let peer_body = crate::crossref::PeerBodyResolver {
        repos: &kb.repos,
        graphs: &kb.graphs,
        outcomes: &kb.outcomes,
    };
    let root_resolution = kb
        .repos
        .root()
        .and_then(|r| kb.resolution_graphs.of(&r.name));
    introspect_graph_with(
        kb.root_graph(),
        &|def| SourceSite::of_def(kb, def),
        Some(&peer_body),
        root_resolution,
    )
}

/// Project every resolved instance in a held knowledge base into the wire shape.
///
/// One entry per instance whose claim resolved — an unresolved claim has no
/// effective shape to introspect. Joins the resolved layer (the shape) with the
/// parse layer in the catalog (the instance and its body); the body event
/// stream is re-scanned from the held body source, no file is re-read.
/// Iterating the resolved map yields path-sorted entries.
///
/// This is the IR-to-wire conversion the daemon serves; the CLI's
/// `--introspect` renders the same projection.
pub fn introspect_kb_instances(kb: &KnowledgeBase) -> Vec<InstanceIntrospection> {
    let mut intros = Vec::new();
    for (path, resolved) in &kb.instances {
        let Some(shape) = &resolved.effective_shape else {
            continue;
        };
        let Some(FileParse::Instance {
            instance: Some(instance),
            body,
            body_offset,
            is_markdown,
            ..
        }) = kb.file_parse(path)
        else {
            continue;
        };
        let events = scan_body(body);
        // Effective template: the first body-declaring claim splices its body;
        // if none of the claims carry one, no template surfaces.
        let graph = kb.graph_for_path(path);
        let peer_body = crate::crossref::PeerBodyResolver {
            repos: &kb.repos,
            graphs: &kb.graphs,
            outcomes: &kb.outcomes,
        };
        let effective_template = instance.type_claim.iter().find_map(|c| {
            // A `::repo` claim's body lives in the peer's graph, not the own graph.
            let g = match &c.repo {
                None => graph,
                Some(repo) => peer_body.peer_graph(repo.as_str())?,
            };
            splice_effective_body(g, &c.name, Some(&peer_body))
        });
        let lines = kb.line_index(path);
        // The instance's own resolution graph (its repo's), so a nested inline
        // record resolves its `type:` claim, cross-repo folds included.
        let resolution = kb
            .repos
            .repo_of(path)
            .and_then(|r| kb.resolution_graphs.of(&r.name));
        let mut intro = introspect_instance(
            instance,
            &instance.type_claim,
            shape,
            effective_template.as_ref(),
            &events,
            *body_offset,
            *is_markdown,
            lines,
            graph,
            resolution,
            Some(kb),
        );
        intro.record_block_ids = record_block_ids_of(kb, path, instance, lines);
        intros.push(intro);
    }
    intros
}

/// All implicit-identity candidates in one instance file, ranked per
/// [[type candidate scan::au-type-system]]. A file scanned with no candidates is still present,
/// so a consumer tells scanned-and-empty from not-scanned.
#[derive(Debug, Serialize)]
pub struct FileCandidatesIntrospection {
    pub file: String,
    pub candidates: Vec<CandidateIntrospection>,
}

/// One ranked candidate: a type the file could claim but does not.
#[derive(Debug, Serialize)]
pub struct CandidateIntrospection {
    pub type_name: String,
    pub scope: CandidateScopeIntrospection,
    pub satisfied_required: Vec<String>,
    pub also_satisfied_optional: usize,
    /// Leaves of the same sealed family this candidate would supersede; empty
    /// for a clean addition.
    pub supersedes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CandidateScopeIntrospection {
    pub file_path: String,
    /// RFC 6901 JSON pointer to the scope inside the file (`""` = top level).
    pub inline_path: String,
}

/// One file's candidates in the lightweight summary: the file plus just the
/// candidate type names, in ranked order. The per-candidate detail (scope,
/// satisfied fields, supersedes) is dropped; the full read serves it.
#[derive(Debug, Serialize)]
pub struct FileCandidatesSummaryIntrospection {
    pub file: String,
    pub candidates: Vec<String>,
}

/// The `candidates` read's `files` payload: full detail, or the lightweight
/// summary projection, selected by the `summary` arg. Untagged, so both render
/// as a plain JSON array.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum WireCandidateFiles {
    Summary(Vec<FileCandidatesSummaryIntrospection>),
    Full(Vec<FileCandidatesIntrospection>),
}

/// The knowledge-base-wide candidate scan, grouped by file in the catalog's sorted
/// order, paged by `offset` / `limit` (over files), full detail or the summary
/// projection. Reads the candidates already computed in the held resolved
/// layer, no rescan. A scanned file with no candidates is still present (so
/// scanned-and-empty stays distinct from not-scanned), and pages like any other.
/// Compute a file's implicit-identity candidates on demand.
///
/// Candidates are not stored in the resolved layer, they are a read-only
/// affordance. The scan is deterministic over the file's held graph and parsed
/// instance, both reachable here, so a read recomputes exactly what an eager
/// build would have produced. `graph_for_path` is the same repo routing the
/// build used. A non-instance or unparsed path has no candidates.
pub fn candidates_for(kb: &KnowledgeBase, path: &Path) -> Vec<au_core::Candidate> {
    let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = kb.file_parse(path)
    else {
        return Vec::new();
    };
    au_core::scan(kb.graph_for_path(path), inst)
}

pub fn introspect_kb_candidates_paged(
    kb: &KnowledgeBase,
    offset: usize,
    limit: Option<usize>,
    summary: bool,
) -> WireCandidateFiles {
    let files = kb
        .instances
        .iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX));
    if summary {
        WireCandidateFiles::Summary(
            files
                .map(|(path, _resolved)| FileCandidatesSummaryIntrospection {
                    file: path.display().to_string(),
                    candidates: candidates_for(kb, path)
                        .iter()
                        .map(|c| c.type_name.as_str().to_string())
                        .collect(),
                })
                .collect(),
        )
    } else {
        WireCandidateFiles::Full(
            files
                .map(|(path, _resolved)| FileCandidatesIntrospection {
                    file: path.display().to_string(),
                    candidates: candidates_for(kb, path)
                        .iter()
                        .map(|c| CandidateIntrospection {
                            type_name: c.type_name.as_str().to_string(),
                            scope: CandidateScopeIntrospection {
                                file_path: c.scope.file_path.display().to_string(),
                                inline_path: c.scope.inline_path.clone(),
                            },
                            satisfied_required: c
                                .satisfied_required
                                .iter()
                                .map(|n| n.as_str().to_string())
                                .collect(),
                            also_satisfied_optional: c.also_satisfied_optional,
                            supersedes: c
                                .supersedes
                                .iter()
                                .map(|n| n.as_str().to_string())
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        )
    }
}

/// The `candidate_counts` read result: the shape of the candidate scan without
/// materializing every file's candidates. `by_type` is the "N untyped files
/// could claim type X" histogram, counting FILES (a type is counted once per
/// file even if it is a candidate at several scopes there).
#[derive(Debug, Serialize)]
pub struct CandidateCountsView {
    /// True when a broken vocabulary skipped the scan, matching the
    /// `candidates` read, so a zero count from a skipped scan is distinct from
    /// an empty one.
    pub aborted_at_load: bool,
    /// Total scanned files (every scanned instance, candidate-bearing or not).
    pub total_files: usize,
    /// Files with at least one candidate.
    pub files_with_candidates: usize,
    /// Candidate type name to the number of files it is a candidate for,
    /// name-sorted.
    pub by_type: std::collections::BTreeMap<String, usize>,
}

/// The `candidate_counts` read: totals plus the by-type file histogram over the
/// full scan, no paging. Reads the candidates already computed in the held
/// resolved layer, no rescan.
pub fn introspect_candidate_counts(kb: &KnowledgeBase) -> CandidateCountsView {
    let mut total_files = 0usize;
    let mut files_with_candidates = 0usize;
    let mut by_type: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (path, _resolved) in kb.instances.iter() {
        total_files += 1;
        let candidates = candidates_for(kb, path);
        if candidates.is_empty() {
            continue;
        }
        files_with_candidates += 1;
        // Dedupe candidate type names WITHIN this file, so by_type counts files,
        // not per-scope candidate occurrences.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for c in candidates.iter() {
            if seen.insert(c.type_name.as_str()) {
                *by_type.entry(c.type_name.as_str().to_string()).or_default() += 1;
            }
        }
    }
    CandidateCountsView {
        aborted_at_load: kb.any_aborted(),
        total_files,
        files_with_candidates,
        by_type,
    }
}

/// One type identity's instance count. Identity-keyed (name + closure-hash) so
/// same-named cross-repo identities stay distinct, carrying its owner repos like
/// every cross-repo result. See
/// [[spec - cross-repo identity on the wire - a name conflates, a qualifier scopes to identity, every result carries owner and hash]].
#[derive(Debug, Serialize)]
pub struct InstanceTypeCount {
    pub name: String,
    /// The closure-hash identity, hex; equal hashes are the same type.
    pub hash: String,
    /// The repos owning a def with this identity (usually one).
    pub type_owners: Vec<String>,
    /// Instance sites whose effective closure contains this identity.
    pub count: usize,
}

/// The `instance_counts` read result: the total authored instances in scope
/// plus a per-identity histogram. CLOSURE-INCLUSIVE — a site counts toward every
/// type in its closure (claim + ancestors), so a `by_type` count equals the
/// length of the matching `instances_of` drill-in over the same origins, and the
/// counts do NOT sum to `total`. The instances dual of `type_counts`.
#[derive(Debug, Default, Serialize)]
pub struct InstanceCountsView {
    /// True when a broken vocabulary aborted a repo's load, matching
    /// `candidate_counts`: closures can be incomplete then, so a count from an
    /// aborted load is distinct from a settled one.
    pub aborted_at_load: bool,
    /// Total authored instance SITES in scope, each counted once regardless of
    /// how broad its closure is. AUTHORED instances only: file-level instances
    /// and nested inline records. Type-def `meta` blocks (the `Meta` origin
    /// `instances_of` also serves) are EXCLUDED — they are type-def annotations,
    /// not browsable documents, and the always-present `au.engine.*` builtins'
    /// meta would otherwise inflate every vocabulary overview.
    pub total: usize,
    /// Per-identity instance counts, sorted by (name, hash).
    pub by_type: Vec<InstanceTypeCount>,
}

/// The `instance_counts` read: fold the held instances into a per-identity
/// count, so a consumer renders a vocabulary-with-counts overview in one
/// round-trip. Scoping filters the instance SITE by the repo it lives in (where
/// the instance is authored), the same `repo` / `scope` axis `type_counts`
/// applies to a def's repo; an absent `repo` spans every in-scope member, a
/// present one pins that member. `scope` still applies UNDER a pin, exactly as
/// `type_counts` does: `repo=<dependency>, scope=own` counts nothing, since a
/// dependency is not the user's own. `None` for an unknown `repo`, the wire's
/// unresolved-lookup signal.
///
/// CLOSURE-INCLUSIVE (see [`InstanceCountsView`]): every type in a site's
/// closure is credited, reusing the same `instance_closure_tids` pass as
/// `instances_of` over its file and nested-inline-record origins, so a row's
/// count equals its `instances_of` drill-in length over those origins. Meta
/// origins are excluded (see [`InstanceCountsView::total`]).
pub fn introspect_instance_counts(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: TypeScope,
) -> Option<InstanceCountsView> {
    // A present-but-unknown repo is the wire's unresolved-lookup null, matching
    // `type_counts`.
    if let Some(r) = repo {
        kb.graph_for_repo(r)?;
    }

    let owners = owners_by_identity(kb);
    let mut total = 0usize;
    let mut counts: std::collections::BTreeMap<TypeId, usize> = std::collections::BTreeMap::new();

    // Credit every identity in a site's closure once (claim ∪ ancestors deduped),
    // mirroring `emit_matches`.
    let mut tally = |claimed: &std::collections::BTreeSet<TypeId>,
                     ancestors: &std::collections::BTreeSet<TypeId>| {
        total += 1;
        let closure: std::collections::BTreeSet<&TypeId> =
            claimed.iter().chain(ancestors.iter()).collect();
        for tid in closure {
            *counts.entry(tid.clone()).or_default() += 1;
        }
    };

    let resolver = crate::resolution_build::RepoGraphResolver {
        graphs: &kb.graphs,
        repos: &kb.repos,
    };

    // File and nested-inline-record sites walk the held instances.
    for (path, _resolved) in &kb.instances {
        let Some(FileParse::Instance {
            instance: Some(instance),
            ..
        }) = kb.file_parse(path)
        else {
            continue;
        };
        let Some(repo_ref) = kb.repos.repo_of(path) else {
            continue;
        };
        // A site is in scope when the repo it lives in (where the instance is
        // authored, not the type identity's owner) passes the repo pin AND the
        // own/all filter. Gating on `includes_repo` in BOTH branches keeps parity
        // with `type_counts`, whose pinned branch honors `scope` too: a
        // `repo=<dependency>, scope=own` counts nothing. The `&Repo` is already in
        // hand from `repo_of`, so this stays O(instances), not O(instances×repos).
        let name_ok = match repo {
            Some(r) => repo_ref.name.0 == r,
            None => true,
        };
        if !(name_ok && scope.includes_repo(kb, repo_ref)) {
            continue;
        }
        let site_repo = repo_ref.name.clone();
        let own = kb.graphs.of(&site_repo);
        let res = kb.resolution_graphs.of(&site_repo);

        let (claimed, ancestors) =
            crate::resolution_build::instance_closure_tids(own, res, &instance.type_claim);
        tally(&claimed, &ancestors);

        for nr in enumerate_nested_records(own, res, Some(&resolver), site_repo.as_str(), instance)
        {
            let claim = tclaim_from_claims(&nr.qualified);
            let (claimed, ancestors) = nested_closure_tids(kb, own, res, &claim);
            tally(&claimed, &ancestors);
        }
    }

    let by_type = counts
        .into_iter()
        .map(|(tid, count)| InstanceTypeCount {
            name: tid.name.as_str().to_string(),
            hash: format!("{:016x}", tid.hash.0),
            type_owners: owners
                .get(&tid)
                .map(|s| s.iter().map(|r| r.0.clone()).collect())
                .unwrap_or_default(),
            count,
        })
        .collect();

    Some(InstanceCountsView {
        aborted_at_load: kb.any_aborted(),
        total,
        by_type,
    })
}

/// One type-def by name, in the wire shape. `None` when the graph has no such
/// type. Serves the by-name type read, behind `BodyTemplatePort` and a single
/// `TypeIndexPort` row.
pub fn introspect_type_named(graph: &TypeGraph, name: &str) -> Option<TypeIntrospection> {
    let type_name = TypeName(name.to_string());
    let def = graph.get(&type_name)?;
    // No repo context here (graph-only), so a cross-repo `use:` splices own-only.
    // The knowledge-base-keyed reads (`introspect_workspace_type_named` etc.) carry the seam.
    Some(introspect_type(
        graph,
        &type_name,
        def,
        &SourceSite::plain(&def.source_path, None),
        None,
        // Graph-only, no repo resolution graph: `unmet_required_meta` is empty.
        None,
    ))
}

/// Where an instance was found, the site kind. See
/// [[spec - instances-of read - flat match records tagged by identity and claimed-or-inherited]].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// A file whose top-level `type:` claims the type.
    File,
    /// A nested inline record inside another instance's field value.
    Nested,
    /// A `meta:` block on a type-def.
    Meta,
}

/// One segment of a nested record's `field_path`: a field name or a list index.
/// Serializes untagged, so a path renders as a mixed array like `["phases", 0]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PathSeg {
    Field(String),
    Index(usize),
}

impl From<&PathSegment> for PathSeg {
    fn from(seg: &PathSegment) -> Self {
        match seg {
            PathSegment::Field(f) => PathSeg::Field(f.clone()),
            PathSegment::Index(i) => PathSeg::Index(*i),
        }
    }
}

/// The origin-specific locator: the identity of a match WITHIN its `path`.
/// `null` for a `file` match, where the path is the whole identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Locator {
    /// A nested inline record, by its structured path from the file root plus
    /// its `^` block-id when it carries one.
    Nested {
        field_path: Vec<PathSeg>,
        block_id: Option<String>,
    },
    /// A meta block, by its semantic key on the host type-def. The singleton
    /// rule makes `(meta_type, repo)` unique per host, so no block-id is needed.
    Meta {
        meta_type: String,
        repo: Option<String>,
    },
}

/// One match record: an instance paired with a matched type identity, tagged by
/// origin and relationship. See
/// [[spec - instances-of read - flat match records tagged by identity and claimed-or-inherited]].
#[derive(Debug, Serialize)]
pub struct InstanceMatch {
    /// The path of the FILE that contains the instance (the instance file, the
    /// host instance file, or the host type-def file).
    pub path: String,
    /// The instance's effective type claim, in authored form (a `::repo` claim
    /// qualified).
    pub claim: Vec<String>,
    /// The instance's own field values keyed by field name, values as JSON. The
    /// `type:` claim is carried in `claim`, not here.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// The matched type's name.
    pub name: String,
    /// The matched type's closure-hash identity, hex. Equal hashes are the same
    /// type; a bare query can match several distinct hashes named alike.
    pub hash: String,
    /// The repos that define this TYPE identity. Several when repos share a
    /// byte-identical definition (the dedup). Named `type_owners`, not `owners`,
    /// because it answers who owns the TYPE, never who owns this instance FILE —
    /// a consumer read the bare `owners` as the latter and was misled. The
    /// file's owner is `member`.
    pub type_owners: Vec<String>,
    /// The declared name of the workspace member owning the instance FILE. The
    /// natural grouping and attribution key, and self-describing beside
    /// `type_owners`. A string, not the full `resolve_member` record: a consumer
    /// needing `root` / `editable` / `role` calls `members` ONCE and joins by
    /// name, so the per-match cost stays a short string.
    pub member: String,
    /// The instance directly claims this identity in its `type:`.
    pub claimed: bool,
    /// This identity is a transitive ancestor of a type the instance claims.
    pub inherited: bool,
    /// The site kind this instance was found at.
    pub origin: Origin,
    /// A byte span into `path`, for tooling resolution. Always present, whatever
    /// the origin.
    pub span: SpanRange,
    /// The origin-specific locator within `path`. `null` for a `file` match.
    pub locator: Option<Locator>,
    /// The instance's own `#:` head docstring, absent when none. The
    /// value-surface twin of the type-def docstrings the schema read carries.
    /// See [[type docstring::au-type-system]].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Per-field `#:` docstrings, keyed by field name, documented fields only,
    /// omitted when empty. Advisory.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub field_docs: std::collections::BTreeMap<String, String>,
}

/// A parsed `instances_of` type query. A bare `name` matches every identity so
/// named across the workspace; a `name::repo` matches the one identity that repo
/// owns. This plus [`query_matches_tid`] is the drift-prone identity rule, shared
/// by the projected read ([`introspect_instances_of`]) and the lean scope
/// ([`instance_files_of`]) so the two can never scope to different sets.
enum InstanceQuery<'a> {
    /// A bare name, matched against every identity's name.
    Bare(&'a str),
    /// A `name::repo` resolved to the one identity that repo holds.
    Demanded(TypeId),
}

/// Parse a type query. `None` when a `name::repo` names an absent repo or a name
/// that repo does not define, which matches nothing (the caller returns empty).
fn parse_instance_query<'a>(kb: &KnowledgeBase, type_name: &'a str) -> Option<InstanceQuery<'a>> {
    match type_name.split_once("::") {
        Some((base, repo)) => kb
            .graphs
            .of(&RepoName(repo.to_string()))
            .closure_id(&TypeName(base.to_string()))
            .map(|hash| {
                InstanceQuery::Demanded(TypeId {
                    name: TypeName(base.to_string()),
                    hash,
                })
            }),
        None => Some(InstanceQuery::Bare(type_name)),
    }
}

/// Does one identity in a site's closure satisfy the query? A demanded identity
/// matches exactly; a bare base matches by name.
fn query_matches_tid(query: &InstanceQuery, tid: &TypeId) -> bool {
    match query {
        InstanceQuery::Bare(base) => tid.name.as_str() == *base,
        InstanceQuery::Demanded(d) => tid == d,
    }
}

/// The distinct FILES that contain an instance of `type_name`, a file-level
/// claim or a nested inline record. The scope primitive the `pins` fold needs.
///
/// It shares the identity rule with [`introspect_instances_of`] (via
/// [`parse_instance_query`] / [`query_matches_tid`]), so the two never scope to
/// different file sets — but it SKIPS the per-match projection (the owners map,
/// field-JSON, docstrings, locators, one `InstanceMatch` per matched identity)
/// that a path-only caller would allocate and discard.
///
/// META origins are excluded by design: a `meta:` block sits on a type-def, and
/// a type-def carries no walkable outbound pins, so a `source_type` matched only
/// through a `meta:` block contributes nothing to the fold. File and nested
/// instance surfaces are the whole scope.
pub fn instance_files_of(kb: &KnowledgeBase, type_name: &str) -> Vec<PathBuf> {
    let Some(query) = parse_instance_query(kb, type_name) else {
        return Vec::new();
    };
    let hits = |claimed: &std::collections::BTreeSet<TypeId>,
                ancestors: &std::collections::BTreeSet<TypeId>| {
        claimed
            .iter()
            .chain(ancestors.iter())
            .any(|tid| query_matches_tid(&query, tid))
    };

    let resolver = crate::resolution_build::RepoGraphResolver {
        graphs: &kb.graphs,
        repos: &kb.repos,
    };

    // `kb.instances` is keyed by unique `PathBuf`, so each file is visited once
    // and pushed at most once — the result is distinct with no explicit dedup.
    let mut files = Vec::new();
    for (path, _resolved) in &kb.instances {
        let Some(FileParse::Instance {
            instance: Some(instance),
            ..
        }) = kb.file_parse(path)
        else {
            continue;
        };
        let repo = match kb.repos.repo_of(path) {
            Some(r) => r.name.clone(),
            None => continue,
        };
        let own = kb.graphs.of(&repo);
        let res = kb.resolution_graphs.of(&repo);

        // File origin, then nested — short-circuit once the file qualifies.
        let (claimed, ancestors) =
            crate::resolution_build::instance_closure_tids(own, res, &instance.type_claim);
        let matched = hits(&claimed, &ancestors)
            || enumerate_nested_records(own, res, Some(&resolver), repo.as_str(), instance)
                .into_iter()
                .any(|nr| {
                    let (c, a) =
                        nested_closure_tids(kb, own, res, &tclaim_from_claims(&nr.qualified));
                    hits(&c, &a)
                });
        if matched {
            files.push(path.clone());
        }
    }
    files
}

/// Every instance related to a queried type, one record per (instance, matched
/// identity), across all origins.
///
/// An instance is any typed value conforming to the type, wherever it lives: a
/// file (`Origin::File`), a nested inline record (`Origin::Nested`), or a
/// `meta:` block on a type-def (`Origin::Meta`). `origins` narrows the set,
/// `None` means all.
///
/// A bare `name` matches every identity so named across the workspace; a
/// `name::repo` matches the one identity owned by that repo. Each record carries
/// its origin, a byte span, an origin-specific locator, the matched identity
/// (name, closure-hash, `type_owners`), the `member` owning the instance file,
/// and the `claimed` / `inherited` flags. A flat stream, the consumer groups and
/// sorts. Empty when the name resolves to no identity.
///
/// The adjacent per-match facts an `include:` set adds (`instance`, `content`)
/// are spliced at the serve layer, which holds the resolved-view and disk-read
/// machinery; this pure held-state pass produces the base records plus `member`.
pub fn introspect_instances_of(
    kb: &KnowledgeBase,
    type_name: &str,
    origins: Option<&[Origin]>,
) -> Vec<InstanceMatch> {
    let owners = owners_by_identity(kb);
    // The identity rule is shared with `instance_files_of` so the two cannot
    // scope differently. `None` is a `name::repo` naming an absent repo/name.
    let Some(query) = parse_instance_query(kb, type_name) else {
        return Vec::new();
    };

    let wants = |o: Origin| match origins {
        None => true,
        Some(list) => list.contains(&o),
    };

    let mut out = Vec::new();

    // Owner-relative nested-shape seam: lets the nested walk resolve a peer type
    // nested inside another peer type by extending the source fold on demand.
    let resolver = crate::resolution_build::RepoGraphResolver {
        graphs: &kb.graphs,
        repos: &kb.repos,
    };

    // File and nested origins both walk the held instances.
    if wants(Origin::File) || wants(Origin::Nested) {
        for (path, _resolved) in &kb.instances {
            let Some(FileParse::Instance {
                instance: Some(instance),
                ..
            }) = kb.file_parse(path)
            else {
                continue;
            };
            let repo = match kb.repos.repo_of(path) {
                Some(r) => r.name.clone(),
                None => continue,
            };
            let own = kb.graphs.of(&repo);
            let res = kb.resolution_graphs.of(&repo);
            let path_str = path.display().to_string();

            if wants(Origin::File) {
                let (claimed, ancestors) =
                    crate::resolution_build::instance_closure_tids(own, res, &instance.type_claim);
                emit_matches(
                    &mut out,
                    &owners,
                    &query,
                    &path_str,
                    repo.as_str(),
                    authored_claim(&instance.type_claim),
                    fields_to_json(&instance.fields),
                    Origin::File,
                    instance.source_span.into(),
                    None,
                    instance.doc.as_deref(),
                    &instance.field_docs,
                    &claimed,
                    &ancestors,
                );
            }

            if wants(Origin::Nested) {
                for nr in
                    enumerate_nested_records(own, res, Some(&resolver), repo.as_str(), instance)
                {
                    let claim = tclaim_from_claims(&nr.qualified);
                    let (claimed, ancestors) = nested_closure_tids(kb, own, res, &claim);
                    let locator = Locator::Nested {
                        field_path: nr.field_path.iter().map(PathSeg::from).collect(),
                        block_id: nr.block_id.clone(),
                    };
                    emit_matches(
                        &mut out,
                        &owners,
                        &query,
                        &path_str,
                        repo.as_str(),
                        nr.qualified.iter().map(|c| c.authored()).collect(),
                        fields_to_json(&nr.fields),
                        Origin::Nested,
                        nr.span.into(),
                        Some(locator),
                        nr.doc.as_deref(),
                        &nr.field_docs,
                        &claimed,
                        &ancestors,
                    );
                }
            }
        }
    }

    // Meta origins walk each repo's own type-defs.
    if wants(Origin::Meta) {
        for (repo, graph) in kb.graphs.iter() {
            let res = kb.resolution_graphs.of(repo);
            for (_name, td) in graph.iter() {
                let Some(blocks) = &td.meta_blocks else {
                    continue;
                };
                for mb in blocks {
                    let claim = TypeClaim::Bare(TypeNameClaim {
                        name: mb.type_name.clone(),
                        repo: mb.repo.clone(),
                        span: mb.type_name_span,
                    });
                    let (claimed, ancestors) =
                        crate::resolution_build::instance_closure_tids(graph, res, &claim);
                    let locator = Locator::Meta {
                        meta_type: mb.type_name.as_str().to_string(),
                        repo: mb.repo.clone(),
                    };
                    emit_matches(
                        &mut out,
                        &owners,
                        &query,
                        &td.source_path.display().to_string(),
                        repo.as_str(),
                        vec![authored_meta(mb)],
                        fields_to_json(&mb.fields),
                        Origin::Meta,
                        mb.block_span.into(),
                        Some(locator),
                        mb.doc.as_deref(),
                        &mb.field_docs,
                        &claimed,
                        &ancestors,
                    );
                }
            }
        }
    }

    out
}

/// Emit one `InstanceMatch` per matched identity in a site's closure. The
/// closure is `claimed ∪ ancestors`; a `demanded` identity matches exactly, a
/// bare `base` matches by name.
#[allow(clippy::too_many_arguments)]
fn emit_matches(
    out: &mut Vec<InstanceMatch>,
    owners: &std::collections::BTreeMap<TypeId, std::collections::BTreeSet<RepoName>>,
    query: &InstanceQuery,
    path: &str,
    member: &str,
    claim: Vec<String>,
    fields: serde_json::Map<String, serde_json::Value>,
    origin: Origin,
    span: SpanRange,
    locator: Option<Locator>,
    doc: Option<&str>,
    field_docs: &std::collections::BTreeMap<String, String>,
    claimed: &std::collections::BTreeSet<TypeId>,
    ancestors: &std::collections::BTreeSet<TypeId>,
) {
    let closure: std::collections::BTreeSet<&TypeId> =
        claimed.iter().chain(ancestors.iter()).collect();
    for tid in closure
        .into_iter()
        .filter(|tid| query_matches_tid(query, tid))
    {
        let owner_list = owners
            .get(tid)
            .map(|s| s.iter().map(|r| r.0.clone()).collect())
            .unwrap_or_default();
        out.push(InstanceMatch {
            path: path.to_string(),
            claim: claim.clone(),
            fields: fields.clone(),
            name: tid.name.as_str().to_string(),
            hash: format!("{:016x}", tid.hash.0),
            type_owners: owner_list,
            member: member.to_string(),
            claimed: claimed.contains(tid),
            inherited: ancestors.contains(tid),
            origin,
            span,
            locator: locator.clone(),
            doc: doc.map(str::to_string),
            field_docs: field_docs.clone(),
        });
    }
}

fn fields_to_json(fields: &[InstanceField]) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    for f in fields {
        m.insert(f.key.clone(), instance_value_to_json(&f.value));
    }
    m
}

/// A `TypeClaim` from a record's qualified claim, for the closure walk. A single
/// claim is `Bare`, a mixin is a `List`. The span is synthetic, the closure walk
/// reads only the names and `::repo` qualifiers.
fn tclaim_from_claims(qualified: &[TypeNameClaim]) -> TypeClaim {
    if qualified.len() == 1 {
        TypeClaim::Bare(qualified[0].clone())
    } else {
        TypeClaim::List {
            items: qualified.to_vec(),
            value_span: ByteRange::new(0, 0),
        }
    }
}

/// A nested record's claim resolved to closure `TypeId`s, owner-relative.
///
/// A `::repo` claim the SOURCE repo never imported — a slot-pinned peer record,
/// whose owner-qualified identity is not a node in the source fold — resolves in
/// the OWNER repo's graph instead, the same `kb.graphs.of(repo)` lookup
/// [`parse_instance_query`] uses for a demanded query, so the record's identity
/// and the query's identity are the identical `TypeId`. A claim the source fold
/// DOES resolve is left to it, so a legitimately-imported peer stays unchanged.
/// Purely additive over [`crate::resolution_build::instance_closure_tids`].
fn nested_closure_tids(
    kb: &KnowledgeBase,
    source_own: &TypeGraph,
    source_res: Option<&au_core::ResolutionGraph>,
    claim: &TypeClaim,
) -> (
    std::collections::BTreeSet<TypeId>,
    std::collections::BTreeSet<TypeId>,
) {
    let (mut claimed, mut ancestors) =
        crate::resolution_build::instance_closure_tids(source_own, source_res, claim);
    for c in claim.iter() {
        let Some(owner) = c.repo.as_deref() else {
            continue;
        };
        // Already resolved via the source fold: a legitimately-imported peer.
        if source_res
            .and_then(|rg| rg.resolve_authored(&c.name, Some(owner)))
            .is_some()
        {
            continue;
        }
        // Owner-relative: resolve the peer type in ITS repo's graph / fold.
        let Some(owner_repo) = kb.repos.by_name(owner) else {
            continue;
        };
        let owner_own = kb.graphs.of(&owner_repo.name);
        let owner_res = kb.resolution_graphs.of(&owner_repo.name);
        let bare = TypeClaim::Bare(TypeNameClaim::own(c.name.clone(), c.span));
        let (cl, an) = crate::resolution_build::instance_closure_tids(owner_own, owner_res, &bare);
        claimed.extend(cl);
        ancestors.extend(an);
    }
    (claimed, ancestors)
}

fn authored_claim(claim: &TypeClaim) -> Vec<String> {
    claim.iter().map(|c| c.authored()).collect()
}

fn authored_meta(mb: &MetaBlock) -> String {
    match &mb.repo {
        Some(r) => format!("{}::{}", mb.type_name.as_str(), r),
        None => mb.type_name.as_str().to_string(),
    }
}

/// Every type identity's owner repos: the repos whose own graph defines a type
/// resolving to that `TypeId`. Several owners when repos share a byte-identical
/// definition (equal closure-hash), the dedup surfaced on each match record.
fn owners_by_identity(
    kb: &KnowledgeBase,
) -> std::collections::BTreeMap<TypeId, std::collections::BTreeSet<RepoName>> {
    let mut m: std::collections::BTreeMap<TypeId, std::collections::BTreeSet<RepoName>> =
        std::collections::BTreeMap::new();
    for (repo, graph) in kb.graphs.iter() {
        for (name, _td) in graph.iter() {
            if let Some(hash) = graph.closure_id(name) {
                m.entry(TypeId {
                    name: name.clone(),
                    hash,
                })
                .or_default()
                .insert(repo.clone());
            }
        }
    }
    m
}

/// One discovered import: a peer type a repo folds into its graph, by identity.
/// See [[spec - list-imports read - a flat stream of discovered per-repo import records]].
#[derive(Debug, Serialize)]
pub struct ImportView {
    /// The repo whose files authored the `::repo` use.
    pub importer: String,
    /// The imported peer type's name.
    pub name: String,
    /// The peer repo it is imported from, the `::repo` as authored.
    pub owner: String,
    /// The resolved closure-hash identity, hex; equal hashes are the same type.
    pub hash: String,
}

/// The workspace's discovered import set: one record per (importing repo,
/// imported type identity), the fold-axis `::repo` types each repo authors
/// (claim / parent / meta / body-use). A field-shape `foo::repo*` reference is a
/// seam concern, not an import, and is excluded. An unresolvable `::repo` (absent
/// peer or missing type) yields no record, the gate diagnostics own that. Sorted
/// by (importer, name, owner).
pub fn introspect_list_imports(kb: &KnowledgeBase, scope: TypeScope) -> Vec<ImportView> {
    let mut out = Vec::new();
    for (importer, name, owner) in crate::resolution_build::collect_imports(&kb.repos, &kb.catalog)
    {
        // `own` keeps only imports MADE BY an own repo, and drops the automatic
        // `au.engine.*` builtin fold (owned by the builtin), which every repo
        // imports and is not a user-authored dependency edge.
        if scope.own_only
            && (!scope.includes_name(kb, &importer)
                || owner == crate::engine_schema::BUILTIN_ENGINE_REPO)
        {
            continue;
        }
        let Some(hash) = kb.graphs.of(&RepoName(owner.clone())).closure_id(&name) else {
            continue;
        };
        out.push(ImportView {
            importer: importer.0,
            name: name.as_str().to_string(),
            owner,
            hash: format!("{:016x}", hash.0),
        });
    }
    out
}

/// One workspace member on the wire: its declared name, absolute root, and
/// whether it sits inside the workspace tree or scattered outside it.
#[derive(Debug, Serialize)]
pub struct MemberView {
    /// The member's declared name (its `::repo` label).
    pub repo: String,
    /// The member's absolute root directory on this machine.
    pub root: String,
    /// `true` when the root is outside the workspace root (reached by absolute
    /// path), `false` for a subdir of it.
    pub scattered: bool,
    /// `true` when the member is an editable authoring surface, `false` for a
    /// consumed member. Role-derived: the entry and an `edit` member are
    /// editable, a `dep` or `discover` member is consumed, regardless of where it
    /// resolved on disk. A co-present dep is served from a live working tree yet
    /// stays `editable: false`, it is a dependency, not an authoring surface. The
    /// "is this mine" axis: a consumer hides consumed members from the editable
    /// tree.
    pub editable: bool,
    /// `true` when the member is mounted from a live local working tree (a
    /// co-present sibling or a registry path), `false` for a read-only cache
    /// snapshot mounted by its locked sha. The LOCATION axis, orthogonal to
    /// `editable` (the role axis): a co-present dep is `editable: false, local:
    /// true` (consumed but writable in place), an `edit` member present only in
    /// the cache is `editable: true, local: false` (yours but read-only here). A
    /// consumer scopes physical writes with `editable && local`.
    pub local: bool,
    /// The member's workspace role: `entry`, `edit`, `discover`, or `dep`. The
    /// full signal `editable` derives from (`entry` / `edit` are editable). Lets
    /// a consumer distinguish a `discover` mount from a plain `dep`. For a
    /// `disabled` member the role is its DECLARED role (`edit` / `discover`),
    /// remembered though the member is not mounted.
    pub role: String,
    /// `true` when the member is DECLARED in the workspace's `disabled:` overlay:
    /// intentionally not mounted (contributes no files, types, or vocabulary),
    /// as opposed to a mounted member (`false`). Distinct from the role-keyed
    /// unmounted diagnostics (declared-but-cannot-be-found): a disabled member is
    /// found-but-switched-off-on-purpose, so it fires no diagnostic and surfaces
    /// only here. A disabled member carries an empty `root` and `local: false`
    /// (it is not resolved here; its device location lives in the registry), and
    /// its `role` is its declared `edit` / `discover` role. A consumer renders it
    /// greyed with a toggle. See
    /// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
    pub disabled: bool,
    /// Whether a git working tree covers the member, and which one.
    pub git: MemberGitView,
}

/// Which git working tree covers a member, the engine's own git-ness answer.
///
/// A member NESTED inside a larger working tree is covered by it: that tree
/// physically holds the member's files, so it is the only tree that can commit
/// them. A monorepo holding many members in one tree therefore reports every one
/// of them `tracked: true` with the same `root`.
///
/// Served because a consumer cannot derive it. Mirroring the rule with its own
/// `.git` check starts lying the moment the engine's rule changes, and the
/// member's own directory is usually NOT the answer.
#[derive(Debug, Serialize)]
pub struct MemberGitView {
    /// `true` when some working tree covers the member. `false` is the genuine
    /// non-git case: writes land but never commit, and a structural refactor
    /// touching this member is refused, since it could not be rolled back.
    pub tracked: bool,
    /// The absolute root of the covering working tree, `null` when `tracked` is
    /// `false`. Often an ANCESTOR of the member's own `root` rather than equal
    /// to it, which is the fact a consumer cannot guess: it is what says "these
    /// twelve members all commit into one tree".
    pub root: Option<String>,
}

/// The `members` read: the workspace's declared members and their locations.
#[derive(Debug, Serialize)]
pub struct MembersView {
    pub members: Vec<MemberView>,
}

/// The workspace role a repo carries (the entry, an `edit` / `discover` member,
/// or a `dep`), searched across every workspace that declares it. The `editable`
/// and `role` wire flags and the `own` scope derive from it. Defaults to `Entry`
/// (an editable authoring surface) for a repo carried by no workspace role,
/// matching the "the user's own content" default; unreachable for a mounted
/// non-builtin repo, which always assembles with a role.
pub(crate) fn member_role(kb: &KnowledgeBase, name: &RepoName) -> MemberRole {
    // First workspace that names this repo wins. Today there is exactly one
    // assembled workspace, so the choice is unambiguous; if multiple workspaces
    // ever coexist this mirrors `resolve_repo_scope`'s same first-match rule.
    kb.workspaces
        .iter()
        .find_map(|w| w.member_roles.get(name).copied())
        .unwrap_or(MemberRole::Entry)
}

/// The `scope` filter for the workspace-wide type reads (`types`, `type_tree`,
/// `type_counts`, `subtypes`, `list_imports`, and the repo-scoped variants).
///
/// `all` (the default) surfaces every mounted repo's vocabulary, unchanged.
/// `own` keeps only the user's OWN repos, a repo that is an editable authoring
/// surface ([`member_role`] editable, the entry or an `edit` member). That hides
/// every dependency, a consumed `dep` or `discover` member, and the compiled-in
/// `au.engine.*` builtin. Role-derived: an `edit` member served only from the
/// read-only cache is still the user's own vocabulary and stays in scope.
#[derive(Clone, Copy)]
pub struct TypeScope {
    own_only: bool,
}

impl TypeScope {
    /// The default, everything in scope.
    pub fn all() -> Self {
        Self { own_only: false }
    }

    /// `own_only` gates to the user's own (editable) repos.
    pub fn new(own_only: bool) -> Self {
        Self { own_only }
    }

    /// Whether this scope narrows to the user's own repos. Lets a caller skip
    /// per-item owner resolution when the scope is `all` and nothing is gated.
    pub fn own_only(&self) -> bool {
        self.own_only
    }

    /// Whether a repo is in scope. Public so a non-vocabulary read can apply
    /// the SAME own-vs-all rule rather than restating it — `top_level_dirs`
    /// filters directories by it, and a second spelling of "the user's own
    /// repos" is exactly the drift this arg exists to prevent.
    pub fn includes_repo(&self, kb: &KnowledgeBase, repo: &crate::repo::Repo) -> bool {
        self.includes(kb, repo)
    }

    /// Whether a repo's vocabulary is in scope.
    fn includes(&self, kb: &KnowledgeBase, repo: &crate::repo::Repo) -> bool {
        if !self.own_only {
            return true;
        }
        !repo.builtin && member_role(kb, &repo.name).editable()
    }

    /// Whether a repo named `name` is in scope, resolving the name to its repo.
    /// An unknown name is out of scope under `own` (it owns no own-vocabulary).
    fn includes_name(&self, kb: &KnowledgeBase, name: &RepoName) -> bool {
        if !self.own_only {
            return true;
        }
        kb.repos
            .repos()
            .iter()
            .find(|r| &r.name == name)
            .is_some_and(|r| self.includes(kb, r))
    }
}

/// The workspace's member topology: each declared member's name, absolute root,
/// scattered-vs-subdir flag relative to `workspace_root`, a role-derived
/// `editable` flag, a location-derived `local` flag, and the `role` string.
/// Sorted by name, so the result is stable.
///
/// The two axes are orthogonal. `editable` is role-derived: the entry and an
/// `edit` member are editable authoring surfaces, a `dep` or `discover` member
/// is consumed. `local` is location-derived: a live working tree (root not under
/// `cache_root`) versus a read-only cache snapshot. `cache_root` `None` (no home
/// directory) means no member can be cache-mounted, so every member is `local`.
/// A consumer hides consumed members with `!editable` and scopes physical writes
/// with `editable && local`.
pub fn introspect_members(
    kb: &KnowledgeBase,
    workspace_root: &Path,
    cache_root: Option<&Path>,
) -> MembersView {
    let mut members: Vec<MemberView> = kb
        .repos
        .repos()
        .iter()
        // The compiled-in `au-engine` repo is a type SOURCE (its `au.engine.*`
        // types surface like any dependency), not a workspace MEMBER: it has no
        // real root, only a sentinel. Excluding it keeps a consumer from trying
        // to mount a nonexistent tree.
        .filter(|r| !r.builtin)
        .map(|r| {
            let role = member_role(kb, &r.name);
            MemberView {
                repo: r.name.as_str().to_string(),
                root: r.root.display().to_string(),
                scattered: !r.root.starts_with(workspace_root),
                editable: role.editable(),
                local: root_is_local(&r.root, cache_root),
                role: role.as_str().to_string(),
                // A repo present in `kb.repos` mounted, so it is never disabled: a
                // disabled member is dropped before discovery and added below.
                disabled: false,
                // The same predicate the saga groups by, so the badge a consumer
                // renders and the tree a mutation commits into cannot disagree.
                // A few stat calls per member up the ancestor chain; `members` is
                // a topology read, not a hot path.
                git: match crate::gitwriter::working_tree_of(&r.root) {
                    Some(tree) => MemberGitView {
                        tracked: true,
                        root: Some(tree.display().to_string()),
                    },
                    None => MemberGitView {
                        tracked: false,
                        root: None,
                    },
                },
            }
        })
        .collect();
    // Disabled members are excluded from the mount set (dropped before discovery),
    // so they are absent from `kb.repos`. Add them from each workspace's
    // `disabled:` overlay so a consumer sees them greyed with a toggle. A disabled
    // name that is not actually declared (in neither `edit` nor `discover`) is a
    // typo, owned by `disabled-member-not-declared`, and is not a member here. The
    // declared role gives `role` / `editable`; the member is not resolved, so
    // `root` is empty and `local` is false (its device location lives in the
    // registry).
    for ws in kb.workspaces.iter() {
        for name in &ws.disabled {
            let (role, editable) = if ws.edit.iter().any(|n| n == name) {
                ("edit", true)
            } else if ws.discover.iter().any(|n| n == name) {
                ("discover", false)
            } else {
                continue;
            };
            if members.iter().any(|m| m.repo == name.as_str()) {
                continue;
            }
            members.push(MemberView {
                repo: name.as_str().to_string(),
                root: String::new(),
                scattered: false,
                editable,
                local: false,
                role: role.to_string(),
                disabled: true,
                git: MemberGitView {
                    tracked: false,
                    root: None,
                },
            });
        }
    }
    members.sort_by(|a, b| a.repo.cmp(&b.repo));
    MembersView { members }
}

/// Whether a member `root` is a LIVE working tree (not under the read-only package
/// cache). Compares CANONICALIZED paths, so a symlinked member root or cache root
/// is not misclassified: a false `local: true` would let a consumer's
/// `editable && local` write gate target a read-only cache snapshot. Falls back to
/// the raw prefix check if canonicalization fails (e.g. a path removed under a
/// race). `cache_root` `None` (no cache) means every mounted root is a live tree.
fn root_is_local(root: &Path, cache_root: Option<&Path>) -> bool {
    let Some(cache) = cache_root else {
        return true;
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    !canon(root).starts_with(canon(cache))
}

/// One device-global engine-schema file on the wire: its resolved path, whether
/// it is present, its raw content, and the field-shape diagnostics.
#[derive(Debug, Serialize)]
pub struct DeviceConfigFileView {
    /// The absolute path this file resolves to under the per-user config dir,
    /// present even when the file itself is absent.
    pub path: String,
    /// `true` when the file exists and was read, `false` for a legitimate
    /// not-yet-authored state (no content, no diagnostics, the config is
    /// optional).
    pub exists: bool,
    /// The raw file text when present and UTF-8, so a consumer reads or authors
    /// the config; null when the file is absent or not valid UTF-8 (the
    /// not-UTF-8 case still carries a diagnostic).
    pub content: Option<String>,
    /// The field-shape verdict against the hardwired `au.engine.*` def, the same
    /// catalog a walked instance gets, spans into THIS real file. Empty for an
    /// absent or clean file.
    pub diagnostics: Vec<au_diagnostics::Diagnostic>,
}

/// The `device_config` read: the per-user device-global engine-schema files,
/// `repos.yaml` and `workspaces.yaml`, each typed against its hardwired
/// `au.engine.*` def (`au.engine.repos` / `au.engine.workspaces`) and field-shape
/// validated. These sit OUTSIDE every knowledge base, so they are not knowledge base nodes and the
/// diagnostics land here, not on the workspace `diagnostics` read. An entry is
/// null when its path cannot be resolved (no device root, `$HOME` unset).
#[derive(Debug, Serialize)]
pub struct DeviceConfigView {
    pub repos: Option<DeviceConfigFileView>,
    pub workspaces: Option<DeviceConfigFileView>,
}

/// Build the `device_config` view from the two files' resolved paths and their
/// bytes (read off the held lock, outside any knowledge base). `au.engine.repos` /
/// `au.engine.workspaces` resolve in the builtin `au-engine` graph the held
/// `knowledge base` always carries, so the field-shape check runs on the snapshot.
pub fn device_config_view(
    kb: &KnowledgeBase,
    repos: (Option<PathBuf>, Option<Vec<u8>>),
    workspaces: (Option<PathBuf>, Option<Vec<u8>>),
) -> DeviceConfigView {
    DeviceConfigView {
        repos: device_config_file(kb, repos, "au.engine.repos"),
        workspaces: device_config_file(kb, workspaces, "au.engine.workspaces"),
    }
}

fn device_config_file(
    kb: &KnowledgeBase,
    (path, bytes): (Option<PathBuf>, Option<Vec<u8>>),
    type_name: &str,
) -> Option<DeviceConfigFileView> {
    // No path means no device root ($HOME unset), so there is no device config,
    // the entry is null.
    let path = path?;
    let path_str = path.display().to_string();
    let Some(bytes) = bytes else {
        // The path resolves but the file is absent: a legitimate
        // not-yet-authored state, no content and no diagnostics.
        return Some(DeviceConfigFileView {
            path: path_str,
            exists: false,
            content: None,
            diagnostics: Vec::new(),
        });
    };
    let content = String::from_utf8(bytes.clone()).ok();
    // The device registries are hardwired `au.engine.*` defs, so they resolve in
    // the builtin `au-engine` graph the held knowledge base always carries.
    let diagnostics = crate::value_validate::validate_device_file(
        kb,
        &path,
        &bytes,
        type_name,
        Some(crate::engine_schema::BUILTIN_ENGINE_REPO),
    );
    Some(DeviceConfigFileView {
        path: path_str,
        exists: true,
        content,
        diagnostics,
    })
}

/// The `config` file's stamped `type` did not resolve to a def in the scope's
/// graph, so the field-shape check could not run and the value is STORED as-is.
/// Advisory `warning`, never a refusal: machine-scope config stays writable
/// regardless of the served workspace, and a consumer's vocabulary may simply not
/// be mounted. See [[spec - diagnostic codes::au-type-system^config-type-unresolved]] and
/// [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].
pub const CONFIG_TYPE_UNRESOLVED: au_diagnostics::DiagnosticCode =
    au_diagnostics::DiagnosticCode::from_static("config-type-unresolved");

/// A `config` file carries no written `type:` key, so it does not self-describe on
/// disk. The declared wire `type` is stamped regardless, so the file stays fully
/// correct and field-shape validated, this is only the nudge to self-describe.
/// Advisory `drift`, high-attention, never blocks: a governed `set_config` write
/// injects `type:`, so an absent one reads as a hand-authored or seeded file. The
/// config-channel sibling of `engine-schema-type-unwritten` (whose message is for
/// the engine's OWN device files, a kind-assigned floor). See
/// [[spec - diagnostic codes::au-type-system^config-type-unwritten]] and
/// [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].
pub const CONFIG_TYPE_UNWRITTEN: au_diagnostics::DiagnosticCode =
    au_diagnostics::DiagnosticCode::from_static("config-type-unwritten");

/// The `config` read's per-file view: one consumer config file under the scoped
/// channel, `<scope>/<consumer>/config/<file>`. Mirrors [`DeviceConfigFileView`],
/// but parametric over `(scope, consumer, file, type)`, so it is off-band the same
/// way (never a walked node) and carries a field-shape verdict against a STAMPED
/// `type`.
#[derive(Debug, Serialize)]
pub struct ConfigFileView {
    /// The absolute path the file resolves to, present even when the file is
    /// absent, so a consumer knows where to author. Null only when the scope has
    /// no resolvable base (machine scope, `$HOME` unset).
    pub path: Option<String>,
    /// `true` when the file exists and was read, `false` for a legitimate
    /// not-yet-authored state.
    pub exists: bool,
    /// The raw file text when present and UTF-8; null when absent or not UTF-8.
    pub content: Option<String>,
    /// The verdict against the file's own `type:` (else the declared `type`),
    /// resolved in the scope's graph, spans into THIS real file. Structural
    /// diagnostics (a YAML parse error) always surface; a `config-type-unwritten`
    /// drift when the file has no `type:`; a `config-type-unresolved` advisory
    /// (replacing the type verdict, structural diagnostics kept) when the type does
    /// not resolve. Empty for an absent or clean file.
    pub diagnostics: Vec<au_diagnostics::Diagnostic>,
}

/// Build the `config` read's view for one resolved path. `bytes` is read
/// off-band (disk, off the runtime workers), like `device_config`. The stamped
/// `type` is resolved in `scope_repo`'s graph: if it resolves, the field-shape
/// check runs; if not, the file is STORED as-is and carries one
/// `config-type-unresolved` advisory rather than the hard `unknown-type-claim`.
pub fn config_view(
    kb: &KnowledgeBase,
    path: &Path,
    bytes: Option<Vec<u8>>,
    type_name: &str,
    scope_repo: &str,
) -> ConfigFileView {
    let path_str = path.display().to_string();
    let Some(bytes) = bytes else {
        return ConfigFileView {
            path: Some(path_str),
            exists: false,
            content: None,
            diagnostics: Vec::new(),
        };
    };
    let content = String::from_utf8(bytes.clone()).ok();

    // Always run the device-file validator, so a STRUCTURAL problem (malformed
    // YAML, a duplicate key) surfaces whether or not the type resolves — a
    // malformed config is never hidden behind the store-as-is advisory.
    let mut diagnostics =
        crate::value_validate::validate_device_file(kb, path, &bytes, type_name, Some(scope_repo));

    if config_type_resolves(kb, scope_repo, type_name) {
        // The type resolved, so the field-shape verdict is real. A consumer config
        // file self-describes with its own `type:`, honored over the stamped floor;
        // when absent, the engine-schema validator emits its own
        // `engine-schema-type-unwritten` drift, whose message is for the engine's
        // OWN device files (a kind-assigned `au.engine.*` floor). Remap it to the
        // config-channel `config-type-unwritten`, config-appropriate message, same
        // drift severity and span. See [[spec - scoped config channel ...]] "the
        // file self-describes with a `type:` key".
        for d in diagnostics.iter_mut() {
            if d.code == crate::engine_schema::ENGINE_SCHEMA_TYPE_UNWRITTEN {
                d.code = CONFIG_TYPE_UNWRITTEN;
                d.message = format!(
                    "config file carries no written `type:`; the declared type `{type_name}` is applied regardless, but the file does not self-describe on disk"
                );
                d.fix = Some(au_diagnostics::SuggestedFix {
                    description: format!("add `type: {type_name}` so the file self-describes"),
                });
            }
        }
    } else {
        // The type is unresolved: drop the walked-file type verdict (the hard
        // `unknown-type-claim` / peer codes, and the self-description nudge — moot
        // when the declared type itself does not resolve) and carry the store-as-is
        // advisory instead. Every STRUCTURAL diagnostic stays.
        diagnostics.retain(|d| !is_unresolved_type_noise(d.code.as_str()));
        diagnostics.insert(
            0,
            config_type_unresolved(path, content.as_deref(), type_name, scope_repo),
        );
    }

    ConfigFileView {
        path: Some(path_str),
        exists: true,
        content,
        diagnostics,
    }
}

/// The diagnostics a config read swaps out when the declared type does not
/// resolve: the walked-file type-resolution errors (`unknown-type-claim` and the
/// `::repo` peer codes) and the self-description drift. Everything else — a YAML
/// parse error, a duplicate key — is STRUCTURAL and stays, so a malformed config
/// still surfaces.
fn is_unresolved_type_noise(code: &str) -> bool {
    matches!(
        code,
        "unknown-type-claim"
            | "peer-type-not-found"
            | "type-repo-unknown"
            | "type-repo-not-a-dependency"
            | "type-repo-unavailable"
            | "engine-schema-type-unwritten"
    )
}

/// Whether the stamped `type` resolves to a def visible from `scope_repo`: a bare
/// name in that repo's own graph, a `foo::repo` name in the named peer's graph.
/// The signal that chooses the field-shape check over the store-and-advisory
/// `config-type-unresolved`.
fn config_type_resolves(kb: &KnowledgeBase, scope_repo: &str, type_name: &str) -> bool {
    // `rsplit_once` splits on the LAST `::`, the single trailing `::repo`
    // resolution-scope qualifier (a type name never contains a bare `::`), so
    // `a::b::c` reads as base `a::b` in peer `c`. See [[type repo qualifier::au-type-system]].
    match type_name.rsplit_once("::") {
        Some((base, peer)) => kb
            .graph_for_repo(peer)
            .is_some_and(|g| g.contains(&TypeName(base.to_string()))),
        None => kb
            .graph_for_repo(scope_repo)
            .is_some_and(|g| g.contains(&TypeName(type_name.to_string()))),
    }
}

/// The `config-type-unresolved` advisory, anchored at the file head with line/col
/// when the content is UTF-8, so a consumer surfaces it on the real file.
fn config_type_unresolved(
    path: &Path,
    content: Option<&str>,
    type_name: &str,
    scope_repo: &str,
) -> au_diagnostics::Diagnostic {
    let mut span = au_diagnostics::Span::new(path.to_path_buf(), ByteRange::new(0, 0));
    if let Some(text) = content {
        span.attach_line_col(&LineIndex::new(text.as_bytes()));
    }
    au_diagnostics::Diagnostic {
        code: CONFIG_TYPE_UNRESOLVED,
        severity: au_diagnostics::Severity::Warning,
        span,
        message: format!(
            "type `{type_name}` does not resolve in `{scope_repo}`, so the config is stored as-is and not field-shape checked"
        ),
        related: vec![],
        fix: None,
    }
}

/// One path's owning workspace member: the declared name and absolute root, plus
/// the same `editable` / `local` / `role` flags the `members` read carries, so
/// the cage can scope a write by the member it lands in.
#[derive(Debug, Serialize)]
pub struct MemberOfView {
    pub repo: String,
    pub root: String,
    /// `true` for an editable authoring surface, `false` for a consumed member.
    /// Role-derived. See [`MemberView::editable`].
    pub editable: bool,
    /// `true` for a live local working tree, `false` for a read-only cache
    /// snapshot. Location-derived. See [`MemberView::local`].
    pub local: bool,
    /// The member's workspace role: `entry` / `edit` / `discover` / `dep`. See
    /// [`MemberView::role`].
    pub role: String,
}

/// The `resolve_member` read: which declared member owns a path, by the same
/// deepest-ancestor rule reads and writes use. `None` for a path under no
/// declared member. `editable` / `local` / `role` carry the same meaning as in
/// [`introspect_members`].
pub fn resolve_member(
    kb: &KnowledgeBase,
    path: &Path,
    cache_root: Option<&Path>,
) -> Option<MemberOfView> {
    kb.repos.repo_of(path).map(|r| {
        let role = member_role(kb, &r.name);
        MemberOfView {
            repo: r.name.as_str().to_string(),
            root: r.root.display().to_string(),
            editable: role.editable(),
            local: cache_root.is_none_or(|c| !r.root.starts_with(c)),
            role: role.as_str().to_string(),
        }
    })
}

/// One member's scope rules for the `ignores` read: the editable `.auignore`
/// patterns plus the non-editable defaults and floor. When the read requested
/// `resolve`, `resolved` carries the boundary-level effect. See
/// [[spec - scope management surface - an ignores read and a set_ignores config mutation]].
#[derive(Debug, Serialize)]
pub struct MemberIgnoresView {
    /// The member's absolute root directory.
    pub root: String,
    /// The member's declared repo name.
    pub repo: String,
    /// The editable `.auignore` lines, verbatim (comments and blanks included,
    /// so a `set_ignores` round-trip preserves the file). Empty when the file is
    /// absent.
    pub patterns: Vec<String>,
    /// The seeded, overridable default excludes (`node_modules`, `target`).
    pub default_excludes: Vec<String>,
    /// The unconditional, NOT-editable floor (`.git`, `.arsumbris`).
    pub floor: Vec<String>,
    /// The boundary-level effect, present only when the read requested `resolve`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedScopeView>,
}

/// The boundary-level effect of a member's scope: the directories pruned and the
/// files individually excluded, reported at the boundary the walk decides at,
/// never the contents below a pruned directory. A pruned `node_modules` is ONE
/// `ignored_dirs` entry.
#[derive(Debug, Serialize)]
pub struct ResolvedScopeView {
    /// Absolute paths of pruned directory boundaries; contents never enumerated.
    pub ignored_dirs: Vec<String>,
    /// Absolute paths of individually-excluded files (whose parent was entered).
    pub ignored_files: Vec<String>,
}

/// The `ignores` read result: one member's scope rules, or every member's.
#[derive(Debug, Serialize)]
pub struct IgnoresView {
    /// The payload, keyed by the read's own name. Formerly `members`, which
    /// collided in shape-name with the `members` read while carrying a
    /// different element type.
    pub ignores: Vec<MemberIgnoresView>,
}

/// Build the `ignores` read over the given members (name, absolute root),
/// reading each `.auignore` out-of-band through `fs`. When `resolve`, a bounded
/// boundary walk records the pruned directories and dropped files. The disk work
/// lives here (not over held state) so the daemon runs it off the runtime
/// workers, the same out-of-band pattern the build's scope load uses.
pub fn ignores_view(
    fs: &impl au_parser::FileSystem,
    members: &[(String, std::path::PathBuf)],
    resolve: bool,
) -> IgnoresView {
    let members = members
        .iter()
        .map(|(repo, root)| member_ignores(fs, repo.clone(), root.clone(), resolve))
        .collect();
    IgnoresView { ignores: members }
}

fn member_ignores(
    fs: &impl au_parser::FileSystem,
    repo: String,
    root: std::path::PathBuf,
    resolve: bool,
) -> MemberIgnoresView {
    let auignore = root.join(".arsumbris").join(".auignore");
    // Present: the raw lines drive `patterns`; the same bytes build the filter
    // for `resolve`. Absent: no patterns, the default excludes are the filter.
    let contents = match fs.read_file(&auignore) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => None,
    };
    let patterns = contents
        .as_deref()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();
    let resolved = resolve.then(|| {
        // The filter actually in effect: the `.auignore` layered on the default
        // excludes, falling back to the defaults on a malformed file, so
        // `resolved` mirrors the scope the real build applies.
        let filter = match contents.as_deref() {
            Some(c) => au_parser::WalkFilter::with_auignore(&root, c)
                .unwrap_or_else(|_| au_parser::WalkFilter::default_excludes(&root)),
            None => au_parser::WalkFilter::default_excludes(&root),
        };
        let (boundaries, _errs) = fs.walk_scope_boundaries(&root, &filter).unwrap_or_default();
        ResolvedScopeView {
            ignored_dirs: boundaries
                .ignored_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            ignored_files: boundaries
                .ignored_files
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        }
    });
    MemberIgnoresView {
        root: root.display().to_string(),
        repo,
        patterns,
        default_excludes: au_parser::DEFAULT_EXCLUDE_DIR_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        floor: au_parser::FLOOR_DIR_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        resolved,
    }
}

/// One subtype on the wire: the owner repo plus the owner's type-def
/// introspection. The owner's def carries its `meta_blocks`.
#[derive(Debug, Serialize)]
pub struct SubtypeView {
    /// The repo that owns this type-def.
    pub repo: String,
    /// The owner's type-def, the same shape the `types` read yields: `name`,
    /// `parents`, `meta_blocks`, `source`, and the rest. Flattened, so each
    /// subtype is one object carrying `repo` beside the type-def fields.
    #[serde(flatten)]
    pub def: TypeIntrospection,
}

/// The `subtypes` read result: every type-def across the workspace whose closure
/// includes the base, each as its owner copy with the owner repo.
#[derive(Debug, Serialize)]
pub struct SubtypesView {
    pub base: String,
    pub subtypes: Vec<SubtypeView>,
}

/// A def's parent closure as identity `TypeId`s. For a member that imports a
/// peer, the FOLDED closure (`folded_closure_ids`) resolves each `parent::repo`
/// edge to the peer's id; for a non-importing member (no resolution graph, e.g. a
/// base's owner repo), the own-graph `closure_of` names are mapped to ids via the
/// memoized per-name closure-id. Both yield content-derived ids, so an owner def,
/// an importer's resolved peer edge, and a peer with a byte-identical def share one id.
fn def_closure_ids(
    graph: &TypeGraph,
    rg: Option<&au_core::ResolutionGraph>,
    name: &TypeName,
) -> std::collections::BTreeSet<TypeId> {
    match rg {
        Some(rg) => {
            let claim = TypeClaim::Bare(TypeNameClaim::own(name.clone(), ByteRange::new(0, 0)));
            folded_closure_ids(rg, &claim)
        }
        None => closure_of(graph, name)
            .iter()
            .filter_map(|n| {
                graph.closure_id(n).map(|hash| TypeId {
                    name: n.clone(),
                    hash,
                })
            })
            .collect(),
    }
}

/// One identity in a `type_closure` result: the type, its owning repo, and its
/// content-derived closure hash. The same `(name, repo, hash)` triple every
/// other cross-repo result carries.
#[derive(Debug, Clone, Serialize)]
pub struct ClosureIdentity {
    pub name: String,
    pub repo: String,
    pub hash: String,
}

/// One verdict in a `validate_value` result: which identity the value was
/// validated against, and what that identity's validator said.
///
/// `identity` is NULL in exactly one case: no mounted repo owns the requested
/// name. The verdict then carries the `unknown-type-claim` diagnostic a FILE
/// claiming an absent type gets, which is the read's standing promise (see
/// [[spec - value validation - validate a typed value against a named type-def not just a file]]).
///
/// It is a null-identity VERDICT rather than an empty result because the empty
/// result fails OPEN: a consumer folding `diagnostics` across the list — the
/// natural loop at a consumer's tool boundary — reads zero errors as "valid",
/// so a typo'd or unmounted type name would wave an unvalidated value through.
/// An unknown name is a finding, not an absence.
#[derive(Debug, Serialize)]
pub struct ValueVerdictView {
    pub identity: Option<ClosureIdentity>,
    pub diagnostics: Vec<au_diagnostics::Diagnostic>,
    /// Value keys not declared by this identity's effective shape. Advisory:
    /// undeclared fields are LEGAL under open-world validation, so this is not a
    /// diagnostic — it surfaces them in the response so a caller that asked to
    /// validate can spot a typo'd extra (a near-miss key that quietly passed).
    /// Empty for a null identity, which has no shape to compare against.
    pub undeclared_fields: Vec<String>,
}

/// One effective field in a `type_closure` result: the field as `types` renders
/// it, plus the identity of the type-def that DECLARES it.
///
/// `origin` is the field's go-to-definition target, the query a consumer was
/// walking parent links client-side to answer.
#[derive(Debug, Serialize)]
pub struct ClosureFieldView {
    #[serde(flatten)]
    pub field: FieldIntrospection,
    /// The declaring type-def. A field auto-unified across several origins
    /// reports the lex-min one, the same canonical choice the validator makes.
    pub origin: ClosureIdentity,
}

/// One resolved closure: a type identity, its ancestors, and its effective
/// field set.
#[derive(Debug, Serialize)]
pub struct TypeClosureView {
    pub identity: ClosureIdentity,
    /// The ancestor closure, SELF FIRST, then the rest name-sorted. Each entry
    /// is owner-resolved to a concrete repo, so a `parent::repo` edge reports
    /// the peer that actually owns it rather than the authored qualifier.
    pub ancestors: Vec<ClosureIdentity>,
    /// The effective field set: own fields plus every ancestor's, deduped by
    /// name, each carrying its declaring origin. Name-sorted.
    ///
    /// A field excluded by a mixin collision is ABSENT, matching what the
    /// validator resolves; the collision itself is a diagnostic, not this
    /// read's payload.
    pub fields: Vec<ClosureFieldView>,
}

/// The `type_closure` read: the resolved ancestor closure and effective field
/// set of a type identity.
///
/// Serves the three queries consumers were each re-deriving by walking `parents`
/// over the raw `types` read — effective fields, field origin, and ancestor
/// membership — from ONE traversal, the same walk the validator already runs.
/// Cross-repo names made those client-side walks fragile: every consumer had to
/// re-key its walk by `(name, owner-repo)` identity to follow a qualified
/// parent.
///
/// MULTI-FIT: a BARE `name` conflates across mounted repos, so it returns one
/// closure PER matching identity, each fully qualified — it does not guess a
/// winner. A `repo` arg, or a `::repo` in the name, scopes to the 0-or-1
/// identity that repo owns. An empty array means no mounted type by that name.
pub fn introspect_type_closure(
    kb: &KnowledgeBase,
    name: &str,
    repo: Option<&str>,
) -> Vec<TypeClosureView> {
    let mut out: Vec<TypeClosureView> = Vec::new();
    for fit in name_fits(kb, name, repo) {
        let Some(graph) = kb.graph_for_repo(fit.member.as_str()) else {
            continue;
        };
        let rg = kb.resolution_graphs.of(&fit.member);

        // Ancestors: the same folded-or-own walk `subtypes` uses, so an
        // importing member resolves `parent::repo` to the peer's identity and a
        // non-importing one walks its own graph. Self first, then name-sorted.
        let mut ancestors: Vec<ClosureIdentity> = def_closure_ids(graph, rg, &fit.name)
            .into_iter()
            .filter(|tid| tid.name != fit.name)
            .map(|tid| ClosureIdentity {
                repo: owner_of_id(kb, &tid).unwrap_or_else(|| fit.member.as_str().to_string()),
                name: tid.name.as_str().to_string(),
                hash: format!("{:016x}", tid.hash.0),
            })
            .collect();
        ancestors.sort_by(|a, b| (&a.name, &a.repo).cmp(&(&b.name, &b.repo)));
        ancestors.insert(0, fit.identity.clone());

        let fields = closure_fields(kb, graph, rg, fit.member.as_str(), &fit.name);
        out.push(TypeClosureView {
            identity: fit.identity,
            ancestors,
            fields,
        });
    }
    out
}

/// One mounted identity a type NAME resolves to: the owning member, the bare
/// name, and the `(name, repo, hash)` triple.
pub(crate) struct NameFit {
    pub member: RepoName,
    pub name: TypeName,
    pub identity: ClosureIdentity,
}

/// Every mounted identity a type name denotes, the shared multi-fit rule behind
/// `type_closure` and `validate_value`.
///
/// A BARE name conflates across mounted repos, so it yields one fit PER owning
/// member; a `repo` arg, or a `::repo` in the name, narrows to the 0-or-1
/// identity that repo owns. A `::repo` in the name WINS over the `repo` arg, so
/// `("foo::a", repo: "b")` is not a silent contradiction resolved in the
/// caller's favour.
///
/// Factored so the two multi-fit reads cannot drift on what counts as a fit: a
/// name that `type_closure` reports N closures for is the same N identities
/// `validate_value` returns verdicts for.
pub(crate) fn name_fits(kb: &KnowledgeBase, name: &str, repo: Option<&str>) -> Vec<NameFit> {
    let claim = TypeNameClaim::parse(name, ByteRange::new(0, 0));
    let want_repo = claim.repo.as_deref().or(repo);

    let mut out = Vec::new();
    for member in kb.repos.repos() {
        if want_repo.is_some_and(|r| member.name.as_str() != r) {
            continue;
        }
        let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
            continue;
        };
        if graph.get(&claim.name).is_none() {
            continue;
        }
        let Some(hash) = graph.closure_id(&claim.name) else {
            continue;
        };
        out.push(NameFit {
            member: member.name.clone(),
            name: claim.name.clone(),
            identity: ClosureIdentity {
                name: claim.name.as_str().to_string(),
                repo: member.name.as_str().to_string(),
                hash: format!("{:016x}", hash.0),
            },
        });
    }
    out
}

/// The repo that OWNS an identity: the member whose own graph carries that
/// exact `(name, hash)`. A folded peer edge resolves to the peer, not to the
/// importing member, which is what makes an ancestor list owner-authoritative
/// rather than a restatement of the authored qualifier.
fn owner_of_id(kb: &KnowledgeBase, tid: &TypeId) -> Option<String> {
    kb.repos.repos().iter().find_map(|m| {
        let graph = kb.graph_for_repo(m.name.as_str())?;
        let hash = graph.closure_id(&tid.name)?;
        (hash == tid.hash).then(|| m.name.as_str().to_string())
    })
}

/// The effective field set of one type identity, each field carrying its
/// declaring origin. Uses the validator's own resolution so the read cannot
/// drift from what actually validates.
pub(crate) fn closure_fields(
    kb: &KnowledgeBase,
    graph: &TypeGraph,
    rg: Option<&au_core::ResolutionGraph>,
    member: &str,
    name: &TypeName,
) -> Vec<ClosureFieldView> {
    let claim = TypeClaim::Bare(TypeNameClaim::own(name.clone(), ByteRange::new(0, 0)));
    let shape = match rg {
        Some(rg) => au_core::effective_shape_resolved(rg, graph, &claim).ok(),
        None => au_core::effective_shape(graph, &claim).ok(),
    };
    let Some(shape) = shape else {
        return Vec::new();
    };
    // Resolved fields, then divergent fields ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) so the type-level
    // read does not omit a field the instance read surfaces. A divergent field
    // reports its canonical (lex-min) origin here; the per-origin shapes are on
    // the instance-level effective shape.
    shape
        .iter()
        .chain(shape.divergent())
        .map(|(_, origin)| {
            let (origin_id, info) = origin.canonical();
            // The `OriginId` is `name` or `name::repo`; the repo half names the
            // OWNER for a folded peer, else this member owns it. The closure-id
            // lookup needs the BARE name, which `info.type_name` carries.
            let origin_repo = origin_id
                .as_str()
                .split_once("::")
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| member.to_string());
            let hash = kb
                .graph_for_repo(&origin_repo)
                .and_then(|g| g.closure_id(&info.type_name))
                .map(|h| format!("{:016x}", h.0))
                .unwrap_or_default();
            ClosureFieldView {
                field: introspect_field(&info.decl, None),
                origin: ClosureIdentity {
                    name: info.type_name.as_str().to_string(),
                    repo: origin_repo,
                    hash,
                },
            }
        })
        .collect()
}

/// The `subtypes` read: every type-def across the workspace's members whose
/// parent closure includes `base`, deduped by identity to its owner repo,
/// each annotated with that repo. The base itself is
/// excluded. Name-sorted, so the result is stable.
///
/// Cross-repo-fold-aware: a def that extends `base` only through a `parent::repo`
/// edge (the local copy dropped, a pure import) surfaces here. An importing
/// member resolves the base through its FOLDED parent closure, which the
/// own-graph `closure_of` cannot see (it skips a qualified parent). A member that
/// imports nothing keeps the own-graph walk, so the single-repo case is
/// unchanged.
///
/// `base` may be bare (`foo`, global-by-name) or `::repo`-qualified
/// (`foo::repo`, the one identity that repo owns).
///
/// Walks every member's graph rather than one repo's scope, so it answers the
/// "all subtypes of X across the workspace" question in one read instead of a
/// per-repo `types` enumeration plus a per-match `type_sites` fan-out. The dedup
/// is by `(name, closure-hash)` identity, so emitting one entry per identity dedups.
pub fn introspect_subtypes(kb: &KnowledgeBase, base: &str, scope: TypeScope) -> SubtypesView {
    // Split the base into its name and optional `::repo` scope. A bare base is
    // matched by NAME (conflating every same-named identity across the
    // workspace); a `base::repo` base scopes to the ONE identity that repo owns.
    let base_claim = TypeNameClaim::parse(base, ByteRange::new(0, 0));

    // Resolve a qualified base to its demanded identity: `(name, ClosureHash)`
    // where the hash is the owner's own-graph closure-id. Content-derived, so it
    // equals the id the fold assigns a resolved peer edge and the id the owner's
    // own graph carries; a divergent same-named copy has a different hash. Mirror
    // the `instances_of` demanded resolution. An unresolvable qualified base (the
    // repo or the name is absent) names no identity, so no subtypes.
    let demanded: Option<TypeId> = match &base_claim.repo {
        Some(repo) => {
            match kb
                .graphs
                .of(&RepoName(repo.clone()))
                .closure_id(&base_claim.name)
            {
                Some(hash) => Some(TypeId {
                    name: base_claim.name.clone(),
                    hash,
                }),
                None => {
                    return SubtypesView {
                        base: base.to_string(),
                        subtypes: Vec::new(),
                    }
                }
            }
        }
        None => None,
    };

    let peer_body = crate::crossref::PeerBodyResolver {
        repos: &kb.repos,
        graphs: &kb.graphs,
        outcomes: &kb.outcomes,
    };
    let mut subtypes: Vec<SubtypeView> = Vec::new();
    for member in kb.repos.repos() {
        if !scope.includes(kb, member) {
            continue;
        }
        let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
            continue;
        };
        // The member's cross-repo fold, present iff it imports a peer.
        let rg = kb.resolution_graphs.of(&member.name);
        for (name, def) in graph.iter() {
            // Skip the queried base itself; its subtypes are what we enumerate.
            if *name == base_claim.name {
                continue;
            }
            let closure = def_closure_ids(graph, rg, name);
            let is_subtype = match &demanded {
                // Qualified: the def's parent closure must reach the demanded
                // identity. Covers the base's OWN repo (its closure carries the
                // demanded id), an importer (the fold resolves the peer edge to
                // that same id), and a peer with a byte-identical def; a divergent
                // same-named type has a different id and is excluded.
                Some(dem) => closure.contains(dem),
                // Bare: match by name across the fold-aware closure, the
                // conflate-by-name contract.
                None => closure.iter().any(|tid| tid.name == base_claim.name),
            };
            if is_subtype {
                let site = SourceSite::of_def(kb, def);
                subtypes.push(SubtypeView {
                    repo: member.name.as_str().to_string(),
                    def: introspect_type(
                        graph,
                        name,
                        def,
                        &site,
                        Some(&peer_body),
                        kb.resolution_graphs.of(&member.name),
                    ),
                });
            }
        }
    }
    subtypes.sort_by(|a, b| a.def.name.cmp(&b.def.name));
    SubtypesView {
        base: base.to_string(),
        subtypes,
    }
}

/// One type-def on the wire annotated with the repo it is reported from. For the
/// workspace-wide `types` read that is the owner repo (the read dedups by
/// identity to owner repos); for a repo-scoped read it is the scoped repo, the
/// holder of this def.
#[derive(Debug, Serialize)]
pub struct WorkspaceTypeView {
    /// The repo this copy is reported from: the owner for the workspace read, the
    /// scoped repo for a per-repo read.
    pub repo: String,
    /// The type-def, the same shape `subtypes` and the legacy `types` read yield.
    /// Flattened, so each entry is one object carrying `repo` beside the fields.
    #[serde(flatten)]
    pub def: TypeIntrospection,
}

/// The lightweight `types` summary projection: a type's identity and placement,
/// without the heavy per-def detail (`fields`, `body`, `effective_body`,
/// `meta_blocks`, `source`). The browsable form an agent's orientation call
/// wants; the single `type` read is the detail-on-demand dual, keyed by the
/// `name` / `hash` carried here.
#[derive(Debug, Serialize)]
pub struct WorkspaceTypeSummaryView {
    /// The repo this copy is reported from, matching the full view.
    pub repo: String,
    pub name: String,
    /// The identity hash, same value the full view and `instances_of` carry.
    pub hash: String,
    /// Direct parents in authored form (a `::repo` qualifier stays verbatim).
    pub parents: Vec<String>,
    /// `Some(branches)` when sealed, `null` otherwise, matching the full view.
    pub sealed: Option<Vec<String>>,
    /// The type-def's own `#:` docstring, absent when none, matching the full
    /// view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// The type-def's `location:` block, absent when none, matching the full
    /// view. Cheap (raw forms, no splice), so it rides the browsable summary,
    /// see [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationSpecIntrospection>,
}

/// The `types` read result: full detail, or the lightweight summary projection,
/// selected by the `summary` arg. Untagged, so both render as a plain JSON
/// array and a consumer that ignores the distinction just sees a list.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum WireTypesResult {
    Summary(Vec<WorkspaceTypeSummaryView>),
    Full(Vec<WorkspaceTypeView>),
}

/// The summary projection of one type-def, computed directly from the graph and
/// def, so `summary` mode skips the expensive full-def materialization
/// (`effective_body` splice, per-field shape parsing) entirely.
fn type_summary_view(
    graph: &TypeGraph,
    name: &TypeName,
    def: &TypeDef,
    repo: &str,
) -> WorkspaceTypeSummaryView {
    let parents = def.parents.iter().map(|p| p.authored()).collect();
    let sealed_branches = graph.sealed_branches_of(name);
    let sealed = if sealed_branches.is_empty() {
        None
    } else {
        Some(sealed_branches.iter().map(|s| s.authored()).collect())
    };
    WorkspaceTypeSummaryView {
        repo: repo.to_string(),
        name: name.as_str().to_string(),
        hash: graph
            .closure_id(name)
            .map_or_else(String::new, |h| format!("{:016x}", h.0)),
        parents,
        sealed,
        doc: def.doc.clone(),
        location: def
            .location
            .as_ref()
            .map(LocationSpecIntrospection::from_spec),
    }
}

/// Project a name-sorted entry list into a paged `types` result, full or
/// summary. Paging is applied BEFORE materializing, so a page's cost is the
/// page, not the whole graph; summary mode never calls `introspect_type` at all.
fn project_types_page<'a>(
    kb: &'a KnowledgeBase,
    entries: Vec<(&'a str, &'a TypeName, &'a TypeDef, &'a TypeGraph)>,
    offset: usize,
    limit: Option<usize>,
    summary: bool,
) -> WireTypesResult {
    let paged = entries
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX));
    if summary {
        WireTypesResult::Summary(
            paged
                .map(|(repo, name, def, graph)| type_summary_view(graph, name, def, repo))
                .collect(),
        )
    } else {
        let peer_body = crate::crossref::PeerBodyResolver {
            repos: &kb.repos,
            graphs: &kb.graphs,
            outcomes: &kb.outcomes,
        };
        WireTypesResult::Full(
            paged
                .map(|(repo, name, def, graph)| {
                    let site = SourceSite::of_def(kb, def);
                    WorkspaceTypeView {
                        repo: repo.to_string(),
                        def: introspect_type(
                            graph,
                            name,
                            def,
                            &site,
                            Some(&peer_body),
                            kb.resolution_graphs.of(&RepoName(repo.to_string())),
                        ),
                    }
                })
                .collect(),
        )
    }
}

/// The paged / projected workspace `types` read: the owner-deduped set, name-
/// sorted, paged by `offset` / `limit`, full detail or the `summary` projection.
/// Same entry set and order as `introspect_workspace_types`; this adds paging
/// and the summary projection over it.
pub fn introspect_workspace_types_paged(
    kb: &KnowledgeBase,
    offset: usize,
    limit: Option<usize>,
    summary: bool,
    scope: TypeScope,
) -> WireTypesResult {
    let mut entries: Vec<(&str, &TypeName, &TypeDef, &TypeGraph)> = Vec::new();
    for member in kb.repos.repos() {
        if !scope.includes(kb, member) {
            continue;
        }
        let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
            continue;
        };
        for (name, def) in graph.iter() {
            entries.push((member.name.as_str(), name, def, graph));
        }
    }
    // Stable sort by name, preserving member iteration order for same-named
    // cross-repo identities, so the full result order matches the unpaged read.
    entries.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));
    project_types_page(kb, entries, offset, limit, summary)
}

/// The paged / projected repo-scoped `types` read: one member's whole graph
/// (its own defs), name-sorted, paged, full or summary. `None` for
/// an unknown repo, the wire's unresolved-lookup signal.
pub fn introspect_repo_types_paged(
    kb: &KnowledgeBase,
    repo: &str,
    offset: usize,
    limit: Option<usize>,
    summary: bool,
    scope: TypeScope,
) -> Option<WireTypesResult> {
    let graph = kb.graph_for_repo(repo)?;
    // `own` over a named repo returns its types only if the repo is itself own,
    // so a dependency named explicitly under `own` yields an empty page.
    if !scope.includes_name(kb, &RepoName(repo.to_string())) {
        return Some(project_types_page(kb, Vec::new(), offset, limit, summary));
    }
    let mut entries: Vec<(&str, &TypeName, &TypeDef, &TypeGraph)> = graph
        .iter()
        .map(|(name, def)| (repo, name, def, graph))
        .collect();
    entries.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));
    Some(project_types_page(kb, entries, offset, limit, summary))
}

/// The `type_counts` read result: the filtered type set's total plus a
/// by-repo histogram, name-sorted (BTreeMap). The shape of the vocabulary
/// without materializing every def, the type dual of `diagnostic_counts`.
#[derive(Debug, Default, Serialize)]
pub struct TypeCountsView {
    pub total: usize,
    pub by_repo: std::collections::BTreeMap<String, usize>,
}

/// The `type_counts` read: a total plus a by-repo histogram over the SAME entry
/// set the `types` read spans for the same `repo` scope. Absent `repo` counts
/// the owner-deduped workspace set by owner repo; a present `repo` counts that
/// member's whole graph (its own defs), all under the scoped repo.
/// `None` for an unknown repo, the wire's unresolved-lookup signal. Counts are
/// over the full set, so no paging.
pub fn introspect_type_counts(
    kb: &KnowledgeBase,
    repo: Option<&str>,
    scope: TypeScope,
) -> Option<TypeCountsView> {
    let mut total = 0usize;
    let mut by_repo: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    match repo {
        None => {
            for member in kb.repos.repos() {
                if !scope.includes(kb, member) {
                    continue;
                }
                let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
                    continue;
                };
                for _ in graph.iter() {
                    total += 1;
                    *by_repo.entry(member.name.as_str().to_string()).or_default() += 1;
                }
            }
        }
        Some(r) => {
            let graph = kb.graph_for_repo(r)?;
            // `own` over a named dependency repo counts nothing (empty histogram).
            if scope.includes_name(kb, &RepoName(r.to_string())) {
                for _ in graph.iter() {
                    total += 1;
                }
                if total > 0 {
                    by_repo.insert(r.to_string(), total);
                }
            }
        }
    }
    Some(TypeCountsView { total, by_repo })
}

/// The workspace-wide `types` read: every type-def across all members, deduped
/// by identity to its owner repo, each annotated with
/// that repo. Name-sorted, so the result is stable.
///
/// The enumeration the per-repo read never gave: "what type-defs exist across
/// the mounted workspace." The same member-graph walk and identity dedup
/// `subtypes` does, without the base filter. A type owned by no mounted member
/// does not appear, matching `subtypes`.
///
/// Retained for the full, unpaged callers (the `types` subscription
/// snapshot); the `types` READ dispatch goes through
/// `introspect_workspace_types_paged`.
pub fn introspect_workspace_types(kb: &KnowledgeBase, scope: TypeScope) -> Vec<WorkspaceTypeView> {
    let peer_body = crate::crossref::PeerBodyResolver {
        repos: &kb.repos,
        graphs: &kb.graphs,
        outcomes: &kb.outcomes,
    };
    let mut out: Vec<WorkspaceTypeView> = Vec::new();
    for member in kb.repos.repos() {
        if !scope.includes(kb, member) {
            continue;
        }
        let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
            continue;
        };
        let resolution = kb.resolution_graphs.of(&member.name);
        for (name, def) in graph.iter() {
            let site = SourceSite::of_def(kb, def);
            out.push(WorkspaceTypeView {
                repo: member.name.as_str().to_string(),
                def: introspect_type(graph, name, def, &site, Some(&peer_body), resolution),
            });
        }
    }
    out.sort_by(|a, b| a.def.name.cmp(&b.def.name));
    out
}

/// Resolve one type name, bare (`foo`, the owner copy across the workspace) or
/// `::repo`-qualified (`foo::repo`, the identity that repo holds), to its wire
/// view. `None` when unresolved. The shared per-name resolution behind both the
/// single `type` read and the batch `names` form.
pub fn introspect_type_by_name(kb: &KnowledgeBase, name: &str) -> Option<WorkspaceTypeView> {
    match name.split_once("::") {
        Some((base, repo)) => introspect_repo_type_named(kb, base, repo),
        None => introspect_workspace_type_named(kb, name),
    }
}

/// The batch `type` read: resolve each name, order-matched, `None` per
/// unresolved. The plural dual of `introspect_type_by_name`, so a consumer
/// drilling into several summary entries pays one round-trip, not N.
pub fn introspect_types_named(
    kb: &KnowledgeBase,
    names: &[String],
) -> Vec<Option<WorkspaceTypeView>> {
    names
        .iter()
        .map(|n| introspect_type_by_name(kb, n))
        .collect()
}

/// The workspace-wide `type` read: one type-def by name, resolved to its owner
/// copy across all members, annotated with its owner repo. `None` when no
/// mounted member owns the name. The single-type dual of the workspace `types`
/// read, so a hover resolves a member-defined type without knowing its repo.
pub fn introspect_workspace_type_named(
    kb: &KnowledgeBase,
    name: &str,
) -> Option<WorkspaceTypeView> {
    let type_name = TypeName(name.to_string());
    for member in kb.repos.repos() {
        let Some(graph) = kb.graph_for_repo(member.name.as_str()) else {
            continue;
        };
        if let Some(def) = graph.get(&type_name) {
            let peer_body = crate::crossref::PeerBodyResolver {
                repos: &kb.repos,
                graphs: &kb.graphs,
                outcomes: &kb.outcomes,
            };
            return Some(WorkspaceTypeView {
                repo: member.name.as_str().to_string(),
                def: introspect_type(
                    graph,
                    &type_name,
                    def,
                    &SourceSite::of_def(kb, def),
                    Some(&peer_body),
                    kb.resolution_graphs.of(&member.name),
                ),
            });
        }
    }
    None
}

/// The repo-scoped `type` read: one type-def by name in a named member's graph,
/// annotated with that repo as its holder. `None` when the repo is unknown or
/// the type is absent from it.
pub fn introspect_repo_type_named(
    kb: &KnowledgeBase,
    name: &str,
    repo: &str,
) -> Option<WorkspaceTypeView> {
    let type_name = TypeName(name.to_string());
    let graph = kb.graph_for_repo(repo)?;
    let def = graph.get(&type_name)?;
    let peer_body = crate::crossref::PeerBodyResolver {
        repos: &kb.repos,
        graphs: &kb.graphs,
        outcomes: &kb.outcomes,
    };
    Some(WorkspaceTypeView {
        repo: repo.to_string(),
        def: introspect_type(
            graph,
            &type_name,
            def,
            &SourceSite::of_def(kb, def),
            Some(&peer_body),
            kb.resolution_graphs.of(&RepoName(repo.to_string())),
        ),
    })
}

/// One node in the cross-repo type tree: a type-def, its owner repo, its direct
/// parent type-defs, and its direct child type-defs within the workspace node
/// set. Type-defs form a DAG (a type may extend several parents), so this is an
/// adjacency record, not a nested forest: a multi-parent node appears once and
/// is referenced by each parent's `children`.
#[derive(Debug, Serialize)]
pub struct TypeTreeNode {
    pub repo: String,
    pub name: String,
    /// The type's identity hash, hex, the same one the `types` view carries.
    /// Distinguishes two same-named nodes owned by different repos.
    pub hash: String,
    /// Direct parents as declared, including any whose owner is not mounted.
    pub parents: Vec<String>,
    /// Direct children present in the workspace node set, name-sorted.
    pub children: Vec<String>,
}

/// The `type_tree` read: the workspace's owner-deduped type-defs as a
/// parent/child adjacency forest, owner-annotated per node. `roots` is the
/// convenience entry set; descending `children` from the roots reaches every
/// node.
#[derive(Debug, Serialize)]
pub struct TypeTreeView {
    /// Nodes with no parent present in the node set (typically no parents at
    /// all). Every node is reachable by descending `children` from some root.
    pub roots: Vec<String>,
    /// One node per owner-deduped type-def, name-sorted.
    pub nodes: Vec<TypeTreeNode>,
}

/// The cross-repo type tree: the workspace-wide owner-deduped type-defs as a
/// parent/child adjacency forest. Composable from the workspace `types` read
/// plus `parents`, but served first-class as the agent-friendly tree form.
///
/// Edges are by type name (names are global), so a subtype in one member links
/// to a base owned in another. `children` carries only in-set edges; `parents`
/// reports the declared parents verbatim, so a subtype of an unmounted base
/// still shows it. Roots are nodes with no in-set parent, which guarantees the
/// children walk covers every node.
pub fn introspect_type_tree(kb: &KnowledgeBase, scope: TypeScope) -> TypeTreeView {
    let types = introspect_workspace_types(kb, scope);
    let names: std::collections::BTreeSet<&str> =
        types.iter().map(|t| t.def.name.as_str()).collect();

    // A parent string is served verbatim, so a cross-repo parent reads
    // `base::repo`. Edges here are by type NAME, which is global (WIRE §222-223),
    // so match on the base name: a subtype in one member links to a base owned in
    // another. Stripping the `::repo` for the in-set test keeps that edge.
    fn base_name(p: &str) -> &str {
        p.split_once("::").map_or(p, |(base, _)| base)
    }

    // Invert parents into children, in-set edges only.
    let mut children: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for t in &types {
        for parent in &t.def.parents {
            let parent = base_name(parent);
            if names.contains(parent) {
                children
                    .entry(parent)
                    .or_default()
                    .push(t.def.name.as_str());
            }
        }
    }

    let mut roots: Vec<String> = Vec::new();
    let nodes: Vec<TypeTreeNode> = types
        .iter()
        .map(|t| {
            let name = t.def.name.as_str();
            let has_in_set_parent = t.def.parents.iter().any(|p| names.contains(base_name(p)));
            if !has_in_set_parent {
                roots.push(name.to_string());
            }
            let mut kids: Vec<String> = children
                .get(name)
                .map(|c| c.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            kids.sort();
            TypeTreeNode {
                repo: t.repo.clone(),
                name: name.to_string(),
                hash: t.def.hash.clone(),
                parents: t.def.parents.clone(),
                children: kids,
            }
        })
        .collect();
    roots.sort();
    TypeTreeView { roots, nodes }
}

/// A [`SourceLoc`] for a held file's byte span, attaching the line/column
/// rendering from the file's index.
pub(crate) fn source_loc_at(kb: &KnowledgeBase, path: &Path, span: ByteRange) -> SourceLoc {
    let site = SourceSite::plain(path, kb.line_index(path));
    SourceLoc {
        file: site.file,
        span: SpanRange::from(span).located(site.lines),
    }
}

/// A file's frontmatter as a JSON map: the `type:` claim under `"type"` (for a
/// typed instance) plus every top-level field, values as JSON. `None` when the
/// path is neither a parsed instance nor a note. Serves
/// `RepoFilesPort.readFrontmatter`, for typed instances and untyped notes
/// alike; a note carries no `"type"` key.
pub fn file_frontmatter(
    kb: &KnowledgeBase,
    path: &Path,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let (type_claim, fields) = match kb.file_parse(path)? {
        FileParse::Instance {
            instance: Some(instance),
            ..
        } => (Some(&instance.type_claim), &instance.fields),
        FileParse::Note { fields, .. } => (None, fields),
        _ => return None,
    };
    let mut map = serde_json::Map::with_capacity(fields.len() + 1);
    if let Some(claim) = type_claim {
        map.insert("type".to_string(), type_claim_to_json(claim));
    }
    for f in fields {
        map.insert(f.key.clone(), instance_value_to_json(&f.value));
    }
    Some(map)
}

/// The per-instance value layer: effective values with full contribution
/// provenance, top-level section presence, and the raw body event stream.
///
/// Computed for any parsed instance regardless of whether its claim resolved —
/// frontmatter and body contributions exist independent of the graph. Only
/// section presence needs an effective template, so it is `None` when no claim
/// carries a body. Serves `ProvenancePort`, and the resolved read embeds it.
pub struct InstanceValueLayer {
    pub effective_values: Vec<FieldValuesEntry>,
    pub section_presence: Option<Vec<SectionPresenceEntry>>,
    pub body_events: Option<Vec<BodyEventIntrospection>>,
}

/// Compute the [`InstanceValueLayer`] for one file. `None` when the path is not
/// a parsed instance.
pub fn instance_value_layer(kb: &KnowledgeBase, path: &Path) -> Option<InstanceValueLayer> {
    let FileParse::Instance {
        instance: Some(instance),
        body,
        body_offset,
        is_markdown,
        ..
    } = kb.file_parse(path)?
    else {
        return None;
    };
    let lines = kb.line_index(path);
    let events = scan_body(body);
    // The slot gate for the value layer: a whole-value wikilink is a reference
    // only where the SLOT admits one. An UNRESOLVED instance has no effective
    // shape, so its values stay scalar — see the WIRE.md note that `scalar` is
    // therefore not proof the slot is not a reference.
    let shape = kb
        .instances
        .get(path)
        .and_then(|r| r.effective_shape.as_ref());
    let resolution = kb
        .repos
        .repo_of(path)
        .and_then(|r| kb.resolution_graphs.of(&r.name));
    let ctx = ValueReadCtx {
        lines,
        source_path: path,
        own_graph: kb.graph_for_path(path),
        resolution,
        kb: Some(kb),
    };
    let effective_values = au_core::effective_values(instance, &events, *body_offset, shape)
        .into_iter()
        .map(|(name, containers)| {
            let field_slot = shape.and_then(|s| field_element_shape(s, name.as_str()));
            FieldValuesEntry {
                field: name.as_str().to_string(),
                containers: containers
                    .into_iter()
                    .map(|c| value_container_to_introspection(c, field_slot, &ctx))
                    .collect(),
            }
        })
        .collect();
    let peer_body = crate::crossref::PeerBodyResolver {
        repos: &kb.repos,
        graphs: &kb.graphs,
        outcomes: &kb.outcomes,
    };
    let effective_template = instance.type_claim.iter().find_map(|c| {
        // A `::repo` claim's body lives in the peer's graph, not the own graph.
        let g = match &c.repo {
            None => kb.graph_for_path(path),
            Some(repo) => peer_body.peer_graph(repo.as_str())?,
        };
        splice_effective_body(g, &c.name, Some(&peer_body))
    });
    let section_presence = effective_template.map(|tmpl| {
        convert_section_presence(
            au_core::compute_section_presence(&tmpl, &events, *body_offset),
            lines,
        )
    });
    let body_events = if *is_markdown {
        Some(
            events
                .iter()
                .map(|e| body_event_to_introspection(e, *body_offset, lines))
                .collect(),
        )
    } else {
        None
    };
    Some(InstanceValueLayer {
        effective_values,
        section_presence,
        body_events,
    })
}

/// Convert au-core's `SectionPresenceInfo` into the introspect wire's
/// `SectionPresenceEntry` (snake_case JSON shape with array spans).
/// The actual walk + presence-determination is shared with
/// `body_validate` via `au_core::compute_section_presence` —
/// CLI no longer carries a private heading-walk.
fn convert_section_presence(
    infos: Vec<au_core::SectionPresenceInfo>,
    lines: Option<&LineIndex>,
) -> Vec<SectionPresenceEntry> {
    infos
        .into_iter()
        .map(|info| SectionPresenceEntry {
            name: info.name,
            optional: info.optional,
            depth: info.depth,
            path: info.path,
            present: info.present,
            span: info.span.map(|s| SpanRange::from(s).located(lines)),
        })
        .collect()
}

fn body_event_to_introspection(
    event: &BodyEvent<'_>,
    body_byte_offset: usize,
    lines: Option<&LineIndex>,
) -> BodyEventIntrospection {
    let abs = |s: au_diagnostics::ByteRange| -> SpanRange {
        SpanRange::new(s.start + body_byte_offset, s.end + body_byte_offset).located(lines)
    };
    match event {
        BodyEvent::Heading { level, text, span } => BodyEventIntrospection::Heading {
            level: *level,
            text: text.to_string(),
            span: abs(*span),
        },
        BodyEvent::FencedBlock {
            info,
            body,
            span,
            trailing_block_id,
        } => BodyEventIntrospection::FencedBlock {
            info: info.to_string(),
            body: body.to_string(),
            span: abs(*span),
            trailing_block_id: trailing_block_id.map(|s| s.to_string()),
        },
        BodyEvent::InlineCode { content, span } => BodyEventIntrospection::InlineCode {
            content: content.to_string(),
            span: abs(*span),
        },
        BodyEvent::Wikilink { raw, span } => {
            let parsed = au_references::parse_wikilink_inner(raw).ok();
            BodyEventIntrospection::Wikilink {
                raw: raw.to_string(),
                span: abs(*span),
                parsed,
            }
        }
        BodyEvent::BlockIdMarker { id, span } => BodyEventIntrospection::BlockIdMarker {
            id: id.to_string(),
            span: abs(*span),
        },
        BodyEvent::UnterminatedFenceOpen { info, span } => {
            BodyEventIntrospection::UnterminatedFenceOpen {
                info: info.to_string(),
                span: abs(*span),
            }
        }
    }
}

/// The per-instance context a value read carries so a NESTED inline record
/// resolves: the graph to turn its `type:` claim into an effective shape, the
/// source path to stamp its elaborated contributions, and the line index for
/// spans. Held once per instance and threaded through the value serialization,
/// see [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]].
struct ValueReadCtx<'a> {
    lines: Option<&'a LineIndex>,
    source_path: &'a Path,
    own_graph: &'a TypeGraph,
    resolution: Option<&'a au_core::ResolutionGraph>,
    /// The held knowledge base, present when the value layer runs off it, so a
    /// nested record whose `::repo` identity the source fold never imported (a
    /// slot-pinned peer record) resolves owner-relative in its owner repo's
    /// graph. `None` in a unit context that never descends into a cross-repo
    /// nested record.
    kb: Option<&'a KnowledgeBase>,
}

/// A field's slot ELEMENT shape within an effective shape (the canonical declared
/// shape, `[]` unwrapped so one container is one element), for the demand a nested
/// inline record resolves against.
fn field_element_shape<'a>(shape: &'a EffectiveShape, key: &str) -> Option<&'a Shape> {
    shape
        .get(&au_core::FieldName(key.to_string()))
        .and_then(|origin| origin.canonical_decl().parsed_shape.as_ref().ok())
        .map(au_grammar::slot_element_shape)
}

/// The identity a single-demand slot names, for an inline record that omits its
/// own `type:` (a non-sealed `Record` / `&` / `*` slot names the type). A union /
/// intersection demand names none (the record must declare `type:`, else it is a
/// validation error and the read serves it untyped).
fn demand_claim(slot: &Shape) -> Option<TypeClaim> {
    let name = match slot {
        Shape::Record(n) | Shape::InlineOrReference(n) | Shape::Reference(n) => n,
        _ => return None,
    };
    Some(TypeClaim::Bare(TypeNameClaim {
        name: TypeName(name.as_str().to_string()),
        repo: name.repo.clone(),
        span: ByteRange::new(0, 0),
    }))
}

/// Resolve a nested inline record's fields through the value model, recursively.
///
/// The record's identity is its own `type:` claim, else the slot's single demand.
/// That identity's effective shape lets each nested field elaborate against its
/// slot exactly like a top-level field, so a tuple / brand / reference inside a
/// nested record reads resolved. An unresolvable identity degrades to faithful
/// untyped fields (`elaborate_fields` with no shape), never a crash.
fn resolve_inline_record_fields(
    inline: &InlineValue,
    demand_slot: Option<&Shape>,
    ctx: &ValueReadCtx,
) -> Vec<InlineRecordFieldIntrospection> {
    let claim = inline
        .type_claim
        .clone()
        .or_else(|| demand_slot.and_then(demand_claim));
    let nested_shape = claim.as_ref().and_then(|c| match ctx.kb {
        // Owner-relative when the value layer runs off the held knowledge base:
        // a slot-pinned peer record's identity that the source fold never
        // imported resolves in its owner repo's graph, so its fields elaborate
        // typed rather than degrading to untyped strings.
        Some(kb) => crate::resolution_build::owner_relative_effective_shape(
            kb,
            ctx.own_graph,
            ctx.resolution,
            c,
        ),
        None => crate::resolution_build::resolved_effective_shape(ctx.own_graph, ctx.resolution, c),
    });
    au_core::elaborate_fields(&inline.fields, ctx.source_path, nested_shape.as_ref())
        .into_iter()
        .map(|(field, containers)| {
            let field_slot = nested_shape
                .as_ref()
                .and_then(|s| field_element_shape(s, field.as_str()));
            InlineRecordFieldIntrospection {
                field: field.as_str().to_string(),
                values: containers
                    .into_iter()
                    .map(|c| {
                        contribution_value_to_introspection(
                            &c.value,
                            c.brand.as_deref(),
                            field_slot,
                            ctx,
                        )
                    })
                    .collect(),
            }
        })
        .collect()
}

fn value_container_to_introspection(
    c: ValueContainer,
    slot: Option<&Shape>,
    ctx: &ValueReadCtx,
) -> ValueContainerIntrospection {
    ValueContainerIntrospection {
        value: contribution_value_to_introspection(&c.value, c.brand.as_deref(), slot, ctx),
        contributions: c
            .contributions
            .into_iter()
            .map(|c| contribution_to_introspection(c, slot, ctx))
            .collect(),
    }
}

/// Split an engine-internal section-path entry `"<index> <text>"` into
/// its structured form. The split is on the first space — `index` is
/// strictly an ASCII number, `text` is whatever follows (which may
/// itself begin with digits, see [[type value container::au-type-system]]'s `"# 1 Why"` example).
/// Misshapen input (no leading number) renders with `index: 0` and
/// the whole string as `text`; the path was authored by the engine so
/// the misshapen path should never occur, but the fallback keeps the
/// wire well-formed.
fn parse_section_path_entry(s: &str) -> SectionPathSegment {
    if let Some((idx_str, text)) = s.split_once(' ') {
        if let Ok(index) = idx_str.parse::<u32>() {
            return SectionPathSegment {
                index,
                text: text.to_string(),
            };
        }
    }
    SectionPathSegment {
        index: 0,
        text: s.to_string(),
    }
}

fn contribution_to_introspection(
    c: Contribution,
    slot: Option<&Shape>,
    ctx: &ValueReadCtx,
) -> ContributionIntrospection {
    ContributionIntrospection {
        surface: surface_to_introspection(c.surface),
        // Contributions live in the instance's own file, so the instance's
        // line index locates them.
        location: LocationIntrospection {
            file: c.location.file.display().to_string(),
            byte_range: SpanRange::from(c.location.byte_range).located(ctx.lines),
        },
        section_path: c
            .section_path
            .iter()
            .map(|s| parse_section_path_entry(s))
            .collect(),
        value: contribution_value_to_introspection(&c.value, c.brand.as_deref(), slot, ctx),
        // The collision qualifier a body attribution carried ([[type-def fields collision - auto-unify and qualified field::au-type-system]]),
        // `field{type}` / `field{type::repo}` — which divergent origin it fills.
        qualifier: c.qualifier.map(|q| QualifierIntrospection {
            type_name: q.type_name.as_str().to_string(),
            repo: q.repo,
        }),
    }
}

fn surface_to_introspection(s: Surface) -> SurfaceIntrospection {
    match s {
        Surface::Frontmatter => SurfaceIntrospection::Frontmatter,
        Surface::BodyWikilink => SurfaceIntrospection::BodyWikilink,
        Surface::BodyFence => SurfaceIntrospection::BodyFence,
        Surface::BodyInlineCode => SurfaceIntrospection::BodyInlineCode,
    }
}

fn contribution_value_to_introspection(
    v: &ContributionValue,
    brand: Option<&str>,
    slot: Option<&Shape>,
    ctx: &ValueReadCtx,
) -> ContributionValueIntrospection {
    match v {
        ContributionValue::Scalar(iv) => ContributionValueIntrospection::Scalar {
            value: instance_value_to_json(iv),
            brand: brand.map(str::to_string),
        },
        ContributionValue::Reference {
            target,
            anchor,
            block_id,
            repo,
            commit,
        } => ContributionValueIntrospection::Reference {
            target: target.clone(),
            anchor: anchor.clone(),
            block_id: block_id.clone(),
            repo: repo.clone(),
            commit: commit.clone(),
        },
        // A nested inline record: resolve its fields through the value model,
        // recursively, so a tuple / brand / reference inside it reads resolved.
        ContributionValue::InlineRecord(InstanceValue::Mapping(inline)) => {
            ContributionValueIntrospection::InlineRecord {
                fields: resolve_inline_record_fields(inline, slot, ctx),
            }
        }
        // An `InlineRecord` always wraps a `Mapping`; any other shape is degenerate
        // and serves no fields.
        ContributionValue::InlineRecord(_) => {
            ContributionValueIntrospection::InlineRecord { fields: Vec::new() }
        }
        ContributionValue::Tuple(elements) => ContributionValueIntrospection::Tuple {
            elements: elements
                .iter()
                .map(|e| TupleElementIntrospection {
                    // A tuple element's brand lives on the ELEMENT
                    // (`TupleElementIntrospection.brand`), not on its inner value,
                    // per [[spec - branded types ...]] ("an element brand sits on
                    // the ELEMENT"). So the inner value is rendered brand-less,
                    // never duplicating the element brand onto it. A tuple element's
                    // own slot is not threaded here; a nested inline record element
                    // resolves via its own `type:` claim (else serves untyped).
                    value: contribution_value_to_introspection(&e.value, None, None, ctx),
                    brand: e.brand.clone(),
                })
                .collect(),
            brand: brand.map(str::to_string),
        },
        ContributionValue::MalformedConstructor(raw) => {
            ContributionValueIntrospection::MalformedConstructor { raw: raw.clone() }
        }
        ContributionValue::MalformedReference(_, raw) => {
            ContributionValueIntrospection::MalformedReference { raw: raw.clone() }
        }
    }
}

fn introspect_effective_entry(
    name: &str,
    origin: &FieldOrigin,
    divergent: bool,
) -> EffectiveShapeEntry {
    let canonical = origin.canonical_decl();
    let shape = canonical.shape_display();
    let origins = origin
        .origins()
        .map(|(origin_id, info)| OriginEntry {
            name: info.type_name.as_str().to_string(),
            // The owner repo, recovered from the authored `OriginId` (`name::repo`).
            repo: origin_id
                .as_str()
                .split_once("::")
                .map(|(_, r)| r.to_string()),
            origin_path: info.origin_path.display().to_string(),
            shape: info.decl.shape_display(),
            required: !info.decl.optional,
        })
        .collect();
    EffectiveShapeEntry {
        field: name.to_string(),
        shape,
        required: origin.is_required(),
        divergent,
        origins,
    }
}

/// The target verdict of a `preview_mutation`: the would-be file's resolved type
/// identities and its diagnostics, plus its resulting path and content hash. See
/// [[spec - mutation preview read - simulate a write over an overlay and report
/// its product without committing]].
#[derive(Debug, Serialize)]
pub struct PreviewTargetView {
    pub path: String,
    /// The would-be content hash, absent for a delete (the file is gone). Doubles
    /// as the `expected_hash` a later real write can guard on.
    pub hash: Option<String>,
    /// The type identities the would-be file CLAIMS, each resolved to
    /// `(name, repo, hash)`. Empty for a plain note (no `type:`) and for a delete.
    pub identities: Vec<ClosureIdentity>,
    pub diagnostics: Vec<au_diagnostics::Diagnostic>,
}

/// One blast-radius entry: another file the write dirties, and its would-be
/// diagnostics.
#[derive(Debug, Serialize)]
pub struct PreviewBlastView {
    pub path: String,
    pub diagnostics: Vec<au_diagnostics::Diagnostic>,
}

/// A structural reject: the op refused before any product existed.
#[derive(Debug, Serialize)]
pub struct PreviewRejectView {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// The `preview_mutation` result: the target verdict plus the blast radius, or a
/// structural reject. Serialized untagged, so a reject is `{ reject: {...} }` and
/// a product is `{ target: {...}, blast_radius: [...] }`.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PreviewProductView {
    Reject {
        reject: PreviewRejectView,
    },
    Product {
        target: PreviewTargetView,
        blast_radius: Vec<PreviewBlastView>,
    },
}

/// Read off a preview's product from the throwaway knowledge base: the target's
/// resolved identities and diagnostics, plus every OTHER file whose diagnostics
/// differ from the held knowledge base. Pure over the two knowledge bases, so it
/// is testable without a daemon.
pub(crate) fn preview_product_view(
    preview: &KnowledgeBase,
    held: &KnowledgeBase,
    target: &Path,
) -> PreviewProductView {
    let diagnostics: Vec<au_diagnostics::Diagnostic> =
        preview.diagnostics_for_file(target).cloned().collect();
    let hash = preview
        .catalog
        .get(target)
        .and_then(|e| e.hash)
        .map(crate::mutate::hash_hex);
    PreviewProductView::Product {
        target: PreviewTargetView {
            path: target.display().to_string(),
            hash,
            identities: preview_target_identities(preview, target),
            diagnostics,
        },
        blast_radius: preview_blast_radius(preview, held, target),
    }
}

/// The type identities the would-be file claims, each resolved in the file's own
/// repo (a `::repo` claim scopes itself). Empty when the file is a plain note or
/// carries no parsed instance (a delete leaves nothing to resolve).
fn preview_target_identities(preview: &KnowledgeBase, target: &Path) -> Vec<ClosureIdentity> {
    let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = preview.file_parse(target)
    else {
        return Vec::new();
    };
    let repo = preview
        .repos
        .repo_of(target)
        .map(|r| r.name.as_str().to_string());
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for claim in inst.type_claim.iter() {
        // A bare claim resolves in the file's own repo; a `::repo` claim scopes
        // itself, the same rule `validate_value` and `type_closure` follow.
        for fit in name_fits(preview, &claim.authored(), repo.as_deref()) {
            if seen.insert((fit.identity.name.clone(), fit.identity.repo.clone())) {
                out.push(fit.identity);
            }
        }
    }
    out
}

/// Every OTHER file whose diagnostics differ between the preview and the held
/// knowledge base: the write's collateral. A delete's dangled referrers and a
/// type-def edit's broken dependents surface here, as do referrers a write FIXES.
///
/// v1 scope: per-FILE diagnostics (the `File` / `Instance` sources). Repo-level
/// and knowledge-base-level diagnostic changes (a new `duplicate-type-def`, say)
/// are not projected here, see the spec's Friction.
fn preview_blast_radius(
    preview: &KnowledgeBase,
    held: &KnowledgeBase,
    target: &Path,
) -> Vec<PreviewBlastView> {
    use crate::ir::DiagSource;
    // Candidate files: every per-file diagnostic source in either knowledge base,
    // minus the target. The held side is needed too, so a file that LOST its only
    // diagnostic still surfaces as changed.
    let mut paths: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for kb in [preview, held] {
        for (src, _) in &kb.diagnostics_by_source {
            if let DiagSource::File(p) | DiagSource::Instance(p) = src {
                if p != target {
                    paths.insert(p.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    for p in paths {
        let now: Vec<&au_diagnostics::Diagnostic> = preview.diagnostics_for_file(&p).collect();
        let before: Vec<&au_diagnostics::Diagnostic> = held.diagnostics_for_file(&p).collect();
        if now != before {
            out.push(PreviewBlastView {
                path: p.display().to_string(),
                diagnostics: now.into_iter().cloned().collect(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_core::{build_graph, FieldName};

    #[cfg(unix)]
    #[test]
    fn root_is_local_canonicalizes_a_symlinked_cache_root() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        // A real cache dir holding a mounted package, plus a symlink to it.
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(cache.join("pkgsha")).unwrap();
        let link = tmp.path().join("cache-link");
        symlink(&cache, &link).unwrap();
        // A cache member reached via the SYMLINKED path is still under the cache;
        // a raw prefix check would wrongly report it `local` (a false live tree),
        // opening a write into a read-only snapshot. Canonicalization catches it.
        let via_link = link.join("pkgsha");
        assert!(
            !root_is_local(&via_link, Some(&cache)),
            "a cache member reached via a symlink is not local"
        );
        // A live working tree elsewhere is local.
        let live = tmp.path().join("ws").join("base");
        std::fs::create_dir_all(&live).unwrap();
        assert!(root_is_local(&live, Some(&cache)), "a live tree is local");
        // No cache => every mounted root is a live tree.
        assert!(root_is_local(&via_link, None));
    }

    #[test]
    fn tuple_shape_mirrors_onto_the_wire() {
        use au_grammar::parse_shape;
        let shape = parse_shape("(Number, String)").unwrap();
        let wire = WireShape::from(&shape);
        let WireShape::Tuple { elements } = wire else {
            panic!("expected a WireShape::Tuple, got {wire:?}");
        };
        assert_eq!(elements.len(), 2);
        assert!(matches!(
            elements[0],
            WireShape::Primitive { name: "Number" }
        ));
        assert!(matches!(
            elements[1],
            WireShape::Primitive { name: "String" }
        ));
    }

    fn type_def(name: &str, parents: &[&str], fields: &[(&str, &str)]) -> TypeDef {
        use au_core::{ParentClaim, ParentClaimForm, TypeNameClaim};
        use au_diagnostics::ByteRange;
        use au_grammar::parse_shape;
        use std::path::PathBuf;

        TypeDef {
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
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
            fields: fields
                .iter()
                .map(|(n, raw_shape)| FieldDecl {
                    name: FieldName((*n).into()),
                    optional: false,
                    raw_shape: (*raw_shape).into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: parse_shape(raw_shape).map_err(|err| {
                        au_diagnostics::Diagnostic {
                            code: err.code,
                            severity: err.severity,
                            span: au_diagnostics::Span::new(
                                PathBuf::from(format!("/v/{name}.type.yaml")),
                                ByteRange::new(0, 0),
                            ),
                            message: err.message,
                            related: vec![],
                            fix: None,
                        }
                    }),
                    doc: None,
                })
                .collect(),
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

    #[test]
    fn a_brand_surfaces_its_shape_and_member_docs_on_the_type_read() {
        use au_diagnostics::ByteRange;
        let mut ir = type_def("icon-role", &[], &[]);
        let mut docs = std::collections::BTreeMap::new();
        docs.insert("save".to_string(), "persist".to_string());
        ir.shape = Some(au_core::typedef::BrandShape {
            shape: Shape::Enum(vec!["save".into(), "delete".into()]),
            member_docs: docs,
            span: ByteRange::new(0, 0),
        });
        let g = build_graph(vec![ir]).graph;
        let gi = introspect_graph(&g);
        let t = gi
            .types
            .iter()
            .find(|t| t.name == "icon-role")
            .expect("icon-role type on the wire");
        assert!(t.fields.is_empty(), "a brand has no fields");
        let brand = t.brand.as_ref().expect("a brand on the wire");
        match &brand.shape {
            WireShape::Enum { members } => {
                assert_eq!(members, &vec!["save".to_string(), "delete".to_string()])
            }
            other => panic!("expected an enum WireShape, got {other:?}"),
        }
        assert_eq!(
            brand.member_docs.get("save").map(String::as_str),
            Some("persist")
        );
    }

    fn wire_shape_json(raw: &str) -> serde_json::Value {
        use au_grammar::parse_shape;
        let shape = parse_shape(raw).expect("shape parses");
        serde_json::to_value(WireShape::from(&shape)).expect("serializes")
    }

    #[test]
    fn wire_shape_mirrors_each_shape_variant() {
        use serde_json::json;

        assert_eq!(
            wire_shape_json("String"),
            json!({"kind": "primitive", "name": "String"})
        );
        assert_eq!(
            wire_shape_json("[low, moderate, high]"),
            json!({"kind": "enum", "members": ["low", "moderate", "high"]})
        );
        assert_eq!(
            wire_shape_json("decision*"),
            json!({"kind": "reference", "name": "decision"})
        );
        // The built-in any-repo-file is a reference named "file", not a dedicated kind.
        assert_eq!(
            wire_shape_json("file*"),
            json!({"kind": "reference", "name": "file"})
        );
        // The no-type slot `any` is its own kind, payload-free.
        assert_eq!(wire_shape_json("any"), json!({"kind": "any"}));
        assert_eq!(
            wire_shape_json("any[]"),
            json!({"kind": "list", "min": 0, "inner": {"kind": "any"}})
        );
        // Its reference forms ride the reference / inline-or-reference kinds
        // named "any", like `file*`, not the `any` kind.
        assert_eq!(
            wire_shape_json("any*"),
            json!({"kind": "reference", "name": "any"})
        );
        assert_eq!(
            wire_shape_json("any&"),
            json!({"kind": "inline-or-reference", "name": "any"})
        );
        assert_eq!(
            wire_shape_json("rationale"),
            json!({"kind": "record", "name": "rationale"})
        );
        assert_eq!(
            wire_shape_json("rationale&"),
            json!({"kind": "inline-or-reference", "name": "rationale"})
        );
        // List is the one wrapper: `decision*[]` is a list whose inner is the reference.
        assert_eq!(
            wire_shape_json("decision*[]"),
            json!({
                "kind": "list",
                "min": 0,
                "inner": {"kind": "reference", "name": "decision"}
            })
        );
        assert_eq!(
            wire_shape_json("decision*[+]"),
            json!({
                "kind": "list",
                "min": 1,
                "inner": {"kind": "reference", "name": "decision"}
            })
        );
        // Range cardinality carries min/max; an exact count sets both.
        assert_eq!(
            wire_shape_json("String[2..5]"),
            json!({
                "kind": "list",
                "min": 2,
                "max": 5,
                "inner": {"kind": "primitive", "name": "String"}
            })
        );
        assert_eq!(
            wire_shape_json("Number[3]"),
            json!({
                "kind": "list",
                "min": 3,
                "max": 3,
                "inner": {"kind": "primitive", "name": "Number"}
            })
        );
        // Value refinement is its own kind, absent predicate members omitted.
        assert_eq!(
            wire_shape_json("Number{>=0 & integer}"),
            json!({
                "kind": "refined",
                "base": "Number",
                "refinement": {"lower": {"value": "0", "inclusive": true}, "integer": true}
            })
        );
        assert_eq!(
            wire_shape_json("String{/^[a-z]+$/}"),
            json!({
                "kind": "refined",
                "base": "String",
                "refinement": {"pattern": "^[a-z]+$"}
            })
        );
        assert_eq!(
            wire_shape_json("<String | Number>"),
            json!({
                "kind": "union",
                "branches": [
                    {"kind": "primitive", "name": "String"},
                    {"kind": "primitive", "name": "Number"}
                ]
            })
        );
        assert_eq!(
            wire_shape_json("<a* & b*>&"),
            json!({
                "kind": "compound-reference",
                "mode": "inline-or-ref",
                "op": "intersection",
                "branches": ["a", "b"]
            })
        );
        assert_eq!(
            wire_shape_json("<a* | b*>*"),
            json!({
                "kind": "compound-reference",
                "mode": "ref",
                "op": "union",
                "branches": ["a", "b"]
            })
        );
        // Typed type-def reference: `type*` has no bound, `type<T>*` carries
        // a single or compound bound.
        assert_eq!(wire_shape_json("type*"), json!({"kind": "def-reference"}));
        assert_eq!(
            wire_shape_json("type<mcp.tool>*"),
            json!({
                "kind": "def-reference",
                "bound": {"kind": "single", "name": "mcp.tool"}
            })
        );
        assert_eq!(
            wire_shape_json("type<a | b>*"),
            json!({
                "kind": "def-reference",
                "bound": {"kind": "compound", "op": "union", "branches": ["a", "b"]}
            })
        );
        // The `*@` pin postfix is the second wrapper kind: `inner` carries the
        // wrapped reference, and `T*@[]` is a list whose inner is the pin.
        assert_eq!(
            wire_shape_json("file*@"),
            json!({
                "kind": "pinned",
                "inner": {"kind": "reference", "name": "file"}
            })
        );
        assert_eq!(
            wire_shape_json("type<mcp.tool>*@"),
            json!({
                "kind": "pinned",
                "inner": {
                    "kind": "def-reference",
                    "bound": {"kind": "single", "name": "mcp.tool"}
                }
            })
        );
        assert_eq!(
            wire_shape_json("decision*@[]"),
            json!({
                "kind": "list",
                "min": 0,
                "inner": {
                    "kind": "pinned",
                    "inner": {"kind": "reference", "name": "decision"}
                }
            })
        );
    }

    #[test]
    fn graph_introspection_serializes_types_with_shape_and_source() {
        let g = build_graph(vec![
            type_def("note", &[], &[("description", "String")]),
            type_def("task", &["note"], &[("priority", "[low, moderate, high]")]),
        ])
        .graph;
        let intro = introspect_graph(&g);
        assert_eq!(intro.types.len(), 2);

        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        assert!(note.parents.is_empty());
        assert!(note.sealed.is_none());
        assert_eq!(note.fields.len(), 1);
        assert_eq!(note.fields[0].name, "description");
        assert_eq!(note.fields[0].shape, "String");
        assert!(note.fields[0].required);

        let task = intro.types.iter().find(|t| t.name == "task").unwrap();
        assert_eq!(task.parents, vec!["note".to_string()]);
        assert_eq!(task.fields[0].shape, "[low, moderate, high]");

        // Each type carries its identity hash, matching the memoized closure-id,
        // and two types with different closures carry different hashes.
        assert_eq!(
            note.hash,
            format!("{:016x}", g.closure_id(&TypeName("note".into())).unwrap().0)
        );
        assert!(!note.hash.is_empty());
        assert_ne!(
            note.hash, task.hash,
            "different closures must carry different identity hashes"
        );
    }

    #[test]
    fn parent_and_sealed_render_the_repo_qualifier_verbatim() {
        use au_core::TypeNameClaim;
        use au_diagnostics::ByteRange;
        // A leaf extending a PEER base via `type: base::repo`, with a sealed
        // branch also qualified. The own-graph build skips the qualified parent
        // edge, but the wire render must serve the `::repo` verbatim, symmetric
        // with meta and body-use (WIRE §270-274).
        let mut leaf = type_def("mcp.tool.shout", &[], &[]);
        leaf.parents = vec![TypeNameClaim::parse("mcp.tool::pkg", ByteRange::new(0, 0))];
        leaf.sealed = vec![TypeNameClaim::parse(
            "variant-a::peer",
            ByteRange::new(0, 0),
        )];

        let g = build_graph(vec![leaf]).graph;
        let intro = introspect_graph(&g);
        let shout = intro
            .types
            .iter()
            .find(|t| t.name == "mcp.tool.shout")
            .unwrap();
        assert_eq!(shout.parents, vec!["mcp.tool::pkg".to_string()]);
        assert_eq!(
            shout.sealed.as_deref(),
            Some(["variant-a::peer".to_string()].as_slice())
        );
    }

    #[test]
    fn reference_contribution_carries_repo_when_cross_repo_and_omits_it_otherwise() {
        use au_core::ContributionValue;
        // A cross-repo reference value `[[bar::other]]` carries `repo` on the
        // wire, matching the sibling reference surfaces; an own-repo `[[bar]]`
        // omits it (skip_serializing_if), so the key is absent, not null.
        // The reference arm ignores the slot / nested-resolver context, so a
        // minimal empty-graph context suffices.
        let g = build_graph(vec![]).graph;
        let ctx = ValueReadCtx {
            lines: None,
            source_path: std::path::Path::new(""),
            own_graph: &g,
            resolution: None,
            kb: None,
        };
        let cross = contribution_value_to_introspection(
            &ContributionValue::Reference {
                target: "bar".into(),
                repo: Some("other".into()),
                commit: None,
                anchor: None,
                block_id: None,
            },
            None,
            None,
            &ctx,
        );
        let cross = serde_json::to_value(&cross).unwrap();
        assert_eq!(cross["kind"], "reference");
        assert_eq!(cross["repo"], "other");

        let own = contribution_value_to_introspection(
            &ContributionValue::Reference {
                target: "bar".into(),
                repo: None,
                commit: None,
                anchor: None,
                block_id: None,
            },
            None,
            None,
            &ctx,
        );
        let own = serde_json::to_value(&own).unwrap();
        assert!(
            own.get("repo").is_none(),
            "own-repo reference omits repo entirely, got {own}"
        );
    }

    #[test]
    fn inline_type_claim_renders_the_repo_qualifier() {
        use au_core::TypeNameClaim;
        use au_diagnostics::ByteRange;
        // An inline instance value carrying `type: dm::pkg`. The top-level
        // instances_of claim already stays qualified; this inline surface must
        // match rather than strip to the bare `dm`.
        let claim = TypeClaim::Bare(TypeNameClaim::parse("dm::pkg", ByteRange::new(0, 0)));
        assert_eq!(
            type_claim_to_json(&claim),
            serde_json::Value::String("dm::pkg".to_string())
        );
    }

    #[test]
    fn unparseable_shape_falls_back_to_raw_shape_and_nulls_shape_ast() {
        // `file&` is a syntax error — `parse_shape` returns Err. The `shape`
        // string falls back to the raw text the user wrote, and `shape_ast`
        // is null; the `shape-syntax-error` diagnostic carries the failure.
        let g = build_graph(vec![type_def("withref", &[], &[("target", "file&")])]).graph;
        let intro = introspect_graph(&g);
        assert_eq!(intro.types[0].fields[0].shape, "file&");
        assert!(intro.types[0].fields[0].shape_ast.is_none());

        let json = serde_json::to_value(&intro.types[0].fields[0]).unwrap();
        assert_eq!(json["shape_ast"], serde_json::Value::Null);
    }

    #[test]
    fn docstrings_surface_on_field_and_type_def_and_omit_when_absent() {
        let mut td = type_def("region", &[], &[("page", "Number"), ("quote", "String")]);
        td.doc = Some("an atomic text region".into());
        td.fields[0].doc = Some("page in the file".into());
        // td.fields[1] (quote) carries no docstring.
        let g = build_graph(vec![td]).graph;
        let intro = introspect_graph(&g);
        let ty = &intro.types[0];
        assert_eq!(ty.doc.as_deref(), Some("an atomic text region"));
        assert_eq!(ty.fields[0].doc.as_deref(), Some("page in the file"));
        assert!(ty.fields[1].doc.is_none());

        // Absent docs are omitted from the JSON (skip_serializing_if), so a
        // consumer never sees a `doc: null`.
        let quote_json = serde_json::to_value(&ty.fields[1]).unwrap();
        assert!(
            quote_json.get("doc").is_none(),
            "absent field doc must be omitted, got {quote_json}"
        );
        let full = serde_json::to_string(ty).unwrap();
        assert!(full.contains("\"doc\":\"an atomic text region\""));
        assert!(full.contains("\"doc\":\"page in the file\""));
    }

    #[test]
    fn introspection_round_trips_through_serde_json() {
        let g = build_graph(vec![type_def("note", &[], &[("description", "String")])]).graph;
        let intro = introspect_graph(&g);
        let json = serde_json::to_string(&intro).unwrap();
        // Smoke-check shape: top-level "types" array, type-def with name,
        // parents, fields. Full schema lives in the Rust types — this just
        // catches accidental field renames.
        assert!(json.contains("\"types\""));
        assert!(json.contains("\"name\":\"note\""));
        assert!(json.contains("\"shape\":\"String\""));
        assert!(json.contains("\"required\":true"));
    }

    // ----- meta_blocks introspection -----

    /// Build a TypeDef with explicit `meta_blocks` to exercise the three
    /// [[type-def meta::au-type-system]] states: None / Some(vec![]) / Some(vec![..]).
    fn type_def_with_meta(name: &str, meta: Option<Vec<MetaBlock>>) -> TypeDef {
        let mut td = type_def(name, &[], &[]);
        td.meta_blocks = meta;
        td
    }

    fn make_meta_block(type_name: &str, body: &[(&str, InstanceValue)]) -> MetaBlock {
        use au_core::InstanceField;
        use au_diagnostics::ByteRange;

        MetaBlock {
            type_name: TypeName(type_name.into()),
            repo: None,
            type_name_span: ByteRange::new(0, 0),
            block_span: ByteRange::new(10, 50),
            body_span: ByteRange::new(20, 50),
            fields: body
                .iter()
                .map(|(n, v)| InstanceField {
                    key: (*n).into(),
                    key_span: ByteRange::new(0, 0),
                    value: v.clone(),
                    value_span: ByteRange::new(0, 0),
                    nav_links: Vec::new(),
                })
                .collect(),
            doc: None,
            field_docs: Default::default(),
        }
    }

    fn s(value: &str) -> InstanceValue {
        InstanceValue::String(value.into())
    }

    #[test]
    fn meta_blocks_absent_serializes_as_null() {
        // `meta:` key not written → None on the AST → `null` in JSON.
        // Distinct from `meta: []` (suppression) and `meta: [..]` (declared).
        let g = build_graph(vec![type_def_with_meta("note", None)]).graph;
        let json = serde_json::to_string(&introspect_graph(&g)).unwrap();
        assert!(
            json.contains("\"meta_blocks\":null"),
            "absent meta should serialize as null; got: {json}"
        );
    }

    #[test]
    fn meta_blocks_empty_list_serializes_as_empty_array() {
        // `meta: []` → Some(vec![]) on the AST → `[]` in JSON. This is the
        // [[type-def meta::au-type-system]] suppression marker — consumers MUST distinguish it from null.
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![]))]).graph;
        let json = serde_json::to_string(&introspect_graph(&g)).unwrap();
        assert!(
            json.contains("\"meta_blocks\":[]"),
            "meta: [] should serialize as empty array; got: {json}"
        );
    }

    #[test]
    fn meta_blocks_declared_serialize_with_body_values_and_source() {
        let block = make_meta_block(
            "display-meta",
            &[("tldr", s("A piece of content")), ("icon", s("📝"))],
        );
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        let blocks = note.meta_blocks.as_ref().expect("Some(vec![..])");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].type_name, "display-meta");
        assert_eq!(blocks[0].body.len(), 2);
        assert_eq!(blocks[0].body[0].name, "tldr");
        assert_eq!(
            blocks[0].body[0].value,
            serde_json::Value::String("A piece of content".into())
        );
        assert_eq!(blocks[0].body[1].name, "icon");
        assert_eq!(
            blocks[0].body[1].value,
            serde_json::Value::String("📝".into())
        );

        // Round-trip through serde for shape sanity.
        let json = serde_json::to_string(&intro).unwrap();
        assert!(json.contains("\"type_name\":\"display-meta\""));
        assert!(json.contains("\"name\":\"tldr\""));
        assert!(json.contains("\"value\":\"A piece of content\""));
    }

    #[test]
    fn meta_block_with_empty_body_serializes_with_empty_body_array() {
        // `- type: display-meta` with no further keys — body validates as
        // empty (passes only if all of display-meta's fields are optional;
        // that's the validator's concern, not introspect's). The shape
        // surfaces as type_name + empty body array.
        let block = make_meta_block("display-meta", &[]);
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        let blocks = note.meta_blocks.as_ref().unwrap();
        assert!(blocks[0].body.is_empty());
    }

    #[test]
    fn meta_block_source_points_at_host_file_and_block_span() {
        // The block's source SHOULD anchor at the host TypeDef's file
        // (MetaBlock doesn't carry its own path) and at the block's
        // own block_span (the whole sub-region including `type:`).
        let block = make_meta_block("display-meta", &[("tldr", s("x"))]);
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        let block_intro = &note.meta_blocks.as_ref().unwrap()[0];
        assert_eq!(block_intro.source.file, "/v/note.type.yaml");
        assert_eq!(block_intro.source.span.start, 10);
        assert_eq!(block_intro.source.span.end, 50);
    }

    #[test]
    fn meta_block_sequence_value_serializes_as_json_array() {
        use au_core::SequenceElement;
        use au_diagnostics::ByteRange;

        // `tags: [a, b, c]` body field → JSON array of strings.
        let seq = InstanceValue::Sequence(vec![
            SequenceElement {
                value: s("a"),
                span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            },
            SequenceElement {
                value: s("b"),
                span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            },
            SequenceElement {
                value: s("c"),
                span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            },
        ]);
        let block = make_meta_block("tagged-meta", &[("tags", seq)]);
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let blocks = intro.types[0].meta_blocks.as_ref().unwrap();
        let tags_value = &blocks[0].body[0].value;
        assert_eq!(
            *tags_value,
            serde_json::json!(["a", "b", "c"]),
            "Sequence should serialize as JSON array of recursed values"
        );
    }

    #[test]
    fn meta_block_nested_inline_value_serializes_as_object_with_type() {
        use au_core::{InlineValue, TypeNameClaim};
        use au_diagnostics::ByteRange;

        // A meta body field whose value is a nested inline value (e.g. a
        // `rationale` sub-object with `type: rationale, description: ...`).
        // Renders as a JSON object with the `type:` discriminator under
        // the literal key `"type"` plus the body fields.
        let inline = InlineValue {
            type_claim: Some(TypeClaim::Bare(TypeNameClaim::own(
                TypeName("rationale".into()),
                ByteRange::new(0, 0),
            ))),
            block_id: None,
            fields: vec![au_core::InstanceField {
                key: "description".into(),
                key_span: ByteRange::new(0, 0),
                value: s("Performance gains"),
                value_span: ByteRange::new(0, 0),
                nav_links: Vec::new(),
            }],
            doc: None,
            field_docs: Default::default(),
        };
        let block = make_meta_block(
            "complex-meta",
            &[("rationale", InstanceValue::Mapping(inline))],
        );
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let blocks = intro.types[0].meta_blocks.as_ref().unwrap();
        let nested = &blocks[0].body[0].value;
        assert_eq!(
            *nested,
            serde_json::json!({
                "type": "rationale",
                "description": "Performance gains"
            }),
            "Mapping(InlineValue) should serialize as JSON object with `type` discriminator + body fields"
        );
    }

    #[test]
    fn meta_block_mixin_inline_value_serializes_type_as_array() {
        use au_core::{InlineValue, TypeNameClaim};
        use au_diagnostics::ByteRange;

        // Inline value with `type: [a, b]` mixin → JSON object with `type`
        // key carrying a JSON array of the claim names. Mirrors the
        // file-level mixin representation in JSON.
        let inline = InlineValue {
            type_claim: Some(TypeClaim::List {
                items: vec![
                    TypeNameClaim::own(TypeName("rationale".into()), ByteRange::new(0, 0)),
                    TypeNameClaim::own(TypeName("thesis".into()), ByteRange::new(0, 0)),
                ],
                value_span: ByteRange::new(0, 0),
            }),
            block_id: None,
            fields: vec![],
            doc: None,
            field_docs: Default::default(),
        };
        let block = make_meta_block(
            "intersected",
            &[("evidence", InstanceValue::Mapping(inline))],
        );
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let intro = introspect_graph(&g);
        let blocks = intro.types[0].meta_blocks.as_ref().unwrap();
        let nested = &blocks[0].body[0].value;
        assert_eq!(
            *nested,
            serde_json::json!({ "type": ["rationale", "thesis"] })
        );
    }

    #[test]
    fn meta_block_scalar_value_kinds_serialize_natively() {
        use au_core::SequenceElement;
        use au_diagnostics::ByteRange;

        // Integer / Float / Boolean / Null all render as their JSON-native
        // primitive types — not strings. Verifies the conversion helper
        // dispatches per-variant rather than stringifying everything.
        let block = make_meta_block(
            "runtime-meta",
            &[
                ("version", InstanceValue::Integer(3)),
                ("threshold", InstanceValue::Float(0.5)),
                ("enabled", InstanceValue::Boolean(true)),
                ("notes", InstanceValue::Null),
                (
                    "list",
                    InstanceValue::Sequence(vec![SequenceElement {
                        value: InstanceValue::Integer(42),
                        span: ByteRange::new(0, 0),
                        nav_links: Vec::new(),
                    }]),
                ),
            ],
        );
        let g = build_graph(vec![type_def_with_meta("note", Some(vec![block]))]).graph;
        let json = serde_json::to_string(&introspect_graph(&g)).unwrap();
        assert!(json.contains("\"value\":3"));
        assert!(json.contains("\"value\":0.5"));
        assert!(json.contains("\"value\":true"));
        assert!(json.contains("\"value\":null"));
        assert!(json.contains("\"value\":[42]"));
    }

    // ----- body / effective_body introspection -----

    fn type_def_with_body(name: &str, body: Option<BodyTemplate>) -> TypeDef {
        let mut td = type_def(name, &[], &[]);
        td.body = body;
        td
    }

    fn section(name: &str, optional: bool, body: Option<BodyTemplate>) -> BodyItem {
        use au_diagnostics::ByteRange;
        BodyItem::Section {
            name: name.into(),
            optional,
            fills: None,
            guidance: None,
            body,
            name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
            source_path: std::path::PathBuf::from("/test"),
        }
    }

    fn use_item(target: &str) -> BodyItem {
        use au_diagnostics::ByteRange;
        BodyItem::Use {
            type_name: TypeName(target.into()),
            repo: None,
            type_name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
        }
    }

    #[test]
    fn body_absent_serializes_as_null_for_both_raw_and_effective() {
        let g = build_graph(vec![type_def_with_body("note", None)]).graph;
        let json = serde_json::to_string(&introspect_graph(&g)).unwrap();
        assert!(
            json.contains("\"body\":null"),
            "absent body should serialize as null; got: {json}"
        );
        assert!(
            json.contains("\"effective_body\":null"),
            "absent effective_body should serialize as null; got: {json}"
        );
    }

    #[test]
    fn body_empty_list_serializes_raw_as_empty_array_and_effective_null() {
        // `body: []` is the explicit empty marker — raw renders `[]`,
        // post-splice renders `null` since there's nothing to expand.
        let g = build_graph(vec![type_def_with_body("note", Some(vec![]))]).graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        assert!(matches!(&note.body, Some(b) if b.is_empty()));
        assert!(note.effective_body.is_none());
    }

    #[test]
    fn declared_section_renders_as_section_kind_with_name_and_optional() {
        let g = build_graph(vec![type_def_with_body(
            "note",
            Some(vec![
                section("Why", false, None),
                section("Tldr", true, None),
            ]),
        )])
        .graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        let body = note.body.as_ref().expect("body declared");
        assert_eq!(body.len(), 2);
        match &body[0] {
            BodyItemIntrospection::Section { name, optional, .. } => {
                assert_eq!(name, "Why");
                assert!(!*optional);
            }
            other => panic!("expected Section, got {other:?}"),
        }
        match &body[1] {
            BodyItemIntrospection::Section { name, optional, .. } => {
                assert_eq!(name, "Tldr");
                assert!(*optional);
            }
            other => panic!("expected Section, got {other:?}"),
        }
        let json = serde_json::to_string(&intro).unwrap();
        assert!(json.contains("\"kind\":\"section\""));
        assert!(json.contains("\"name\":\"Why\""));
        assert!(json.contains("\"optional\":true"));
    }

    #[test]
    fn use_item_renders_in_raw_body_and_splices_in_effective_body() {
        // `note` uses `mixin`; `mixin` declares a single section. Raw body
        // of `note` carries the Use item; effective_body carries the
        // spliced section from `mixin`.
        let g = build_graph(vec![
            type_def_with_body("note", Some(vec![use_item("mixin")])),
            type_def_with_body("mixin", Some(vec![section("Inner", false, None)])),
        ])
        .graph;
        let intro = introspect_graph(&g);
        let note = intro.types.iter().find(|t| t.name == "note").unwrap();
        let raw = note.body.as_ref().unwrap();
        assert!(matches!(
            raw[0],
            BodyItemIntrospection::Use { target: ref t } if t == "mixin"
        ));
        let eff = note.effective_body.as_ref().expect("post-splice body");
        assert_eq!(eff.len(), 1);
        match &eff[0] {
            BodyItemIntrospection::Section { name, .. } => assert_eq!(name, "Inner"),
            other => panic!("expected spliced Section, got {other:?}"),
        }
        let json = serde_json::to_string(&intro).unwrap();
        assert!(json.contains("\"kind\":\"use\""));
        assert!(json.contains("\"target\":\"mixin\""));
    }

    #[test]
    fn fills_contract_renders_with_exclusive_and_fields() {
        use au_core::{FieldClaim, FillsContract};
        use au_diagnostics::ByteRange;

        let contract = FillsContract {
            fields: vec![
                FieldClaim {
                    name: FieldName("a".into()),
                    span: ByteRange::new(0, 0),
                },
                FieldClaim {
                    name: FieldName("b".into()),
                    span: ByteRange::new(0, 0),
                },
            ],
            exclusive: true,
            fields_span: ByteRange::new(0, 0),
            source_path: std::path::PathBuf::from("/test"),
        };
        let g = build_graph(vec![type_def_with_body(
            "note",
            Some(vec![BodyItem::Fills {
                contract,
                item_span: ByteRange::new(0, 0),
            }]),
        )])
        .graph;
        let intro = introspect_graph(&g);
        let body = intro.types[0].body.as_ref().unwrap();
        match &body[0] {
            BodyItemIntrospection::Fills { contract } => {
                assert!(contract.exclusive);
                assert_eq!(contract.fields, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected Fills, got {other:?}"),
        }
        let json = serde_json::to_string(&intro).unwrap();
        assert!(json.contains("\"kind\":\"fills\""));
        assert!(json.contains("\"exclusive\":true"));
        assert!(json.contains("\"fields\":[\"a\",\"b\"]"));
    }

    #[test]
    fn ignores_view_reports_patterns_defaults_and_floor() {
        use au_parser::MemoryFileSystem;
        use std::path::PathBuf;
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/.auignore", b"docs/\n# a comment\n".to_vec());
        let members = vec![("root".to_string(), PathBuf::from("/v"))];
        let view = ignores_view(&fs, &members, false);
        assert_eq!(view.ignores.len(), 1);
        let m = &view.ignores[0];
        assert_eq!(m.repo, "root");
        assert_eq!(m.root, "/v");
        // Raw lines, comments preserved for a faithful round-trip.
        assert_eq!(
            m.patterns,
            vec!["docs/".to_string(), "# a comment".to_string()]
        );
        assert_eq!(m.default_excludes, vec!["node_modules", "target"]);
        assert_eq!(m.floor, vec![".git", ".arsumbris"]);
        // No `resolve`, no boundary effect.
        assert!(m.resolved.is_none());
    }

    #[test]
    fn ignores_view_absent_auignore_yields_empty_patterns() {
        use au_parser::MemoryFileSystem;
        use std::path::PathBuf;
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/note.md", b"".to_vec());
        let members = vec![("root".to_string(), PathBuf::from("/v"))];
        let view = ignores_view(&fs, &members, false);
        assert!(view.ignores[0].patterns.is_empty());
        assert_eq!(
            view.ignores[0].default_excludes,
            vec!["node_modules", "target"]
        );
    }

    #[test]
    fn ignores_view_resolve_reports_boundaries_not_contents() {
        use au_parser::MemoryFileSystem;
        use std::path::PathBuf;
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/.auignore", b"docs/\n".to_vec());
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert("/v/docs/guide.md", b"".to_vec());
        fs.insert("/v/node_modules/a/index.js", b"".to_vec());
        fs.insert("/v/node_modules/b/index.js", b"".to_vec());
        let members = vec![("root".to_string(), PathBuf::from("/v"))];
        let view = ignores_view(&fs, &members, true);
        let resolved = view.ignores[0]
            .resolved
            .as_ref()
            .expect("resolve requested");
        // A pruned dir with many files is ONE boundary entry.
        assert_eq!(
            resolved.ignored_dirs,
            vec!["/v/docs".to_string(), "/v/node_modules".to_string()]
        );
        assert!(resolved.ignored_files.is_empty());
    }
}
