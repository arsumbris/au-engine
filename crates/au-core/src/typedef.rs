//! Type-def AST and parser.
//!
//! Structural extraction only — no semantic checks. The parser recovers what
//! it can from a malformed type-def and emits diagnostics for shape
//! mismatches; semantic load-time checks (regex, redeclare, sealed
//! reachability) live in `load_checks`.
//!
//! Spans on the AST are file-relative (already shifted by `yaml_offset`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};
use au_grammar::{parse_shape, Shape, ShapeParseError};
use au_parser::yaml::{scan_duplicate_keys, span_to_byte_range, MarkedYaml, Scalar, YamlData};

use crate::codes;
use crate::instance::{classify_value, scalar_nav_links, InstanceField};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct TypeName(pub String);

impl TypeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldName(pub String);

impl FieldName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A single claimed type-name, on an instance `type:` (identity) or a type-def
/// `type:` (parent). `name` is the base type-def name. `repo` is an optional
/// `::repo` peer qualifier ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]):
/// `None` is a name in the authoring repo's own graph (bare `foo`), `Some(r)` a
/// peer's type (`foo::r`). A name can never legally contain `:`, so the parser
/// splits on the first `::`. au-core resolves only own (unqualified) claims;
/// a qualified claim is deferred to the cross-repo fold, the engine gates its
/// peer reference. The verbatim repo string is held as-is; the engine validates
/// it against the declared peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNameClaim {
    pub name: TypeName,
    pub repo: Option<String>,
    pub span: ByteRange,
}

impl TypeNameClaim {
    /// A claim on a type in the authoring repo's own graph, no `::repo`.
    pub fn own(name: TypeName, span: ByteRange) -> Self {
        Self {
            name,
            repo: None,
            span,
        }
    }

    /// Split a raw claim scalar into its base name and optional `::repo`
    /// qualifier. A type name cannot contain `:`, so the first `::` can only be
    /// the qualifier. The repo string is stored verbatim, the engine validates
    /// it; this never errors.
    pub fn parse(raw: &str, span: ByteRange) -> Self {
        match raw.split_once("::") {
            Some((base, repo)) => Self {
                name: TypeName(base.to_string()),
                repo: Some(repo.to_string()),
                span,
            },
            None => Self::own(TypeName(raw.to_string()), span),
        }
    }

    /// True when the claim carries a `::repo` peer qualifier, so au-core defers
    /// its resolution to the cross-repo fold.
    pub fn is_qualified(&self) -> bool {
        self.repo.is_some()
    }

    /// The claim in authored form: `name` for an own claim, `name::repo` for a
    /// qualified one. This is the verbatim served-string form ([[spec - diagnostic codes::au-type-system]]
    /// aside, WIRE §270-274): a `::repo` type name reaches the wire unstripped,
    /// symmetric with the meta and body-`use` surfaces.
    pub fn authored(&self) -> String {
        match &self.repo {
            Some(r) => format!("{}::{}", self.name.as_str(), r),
            None => self.name.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentClaimForm {
    /// `type: foo` — bare-name parent claim.
    BareName,
    /// `type: [foo, ...]` — proper list form.
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentClaim {
    pub form: ParentClaimForm,
    pub value_span: ByteRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDecl {
    pub name: FieldName,
    pub optional: bool,
    pub raw_shape: String,
    pub name_span: ByteRange,
    pub shape_span: ByteRange,
    pub entry_span: ByteRange,
    /// au-grammar's parse of `raw_shape`, computed once at parse time. `Err`
    /// here does NOT surface at graph load — it is held until the validator
    /// processes an instance field of this declaration (lazy surfacing). The
    /// diagnostic carries the type-def's `shape_span`; the validator wraps it
    /// for the instance use site.
    pub parsed_shape: Result<Shape, Diagnostic>,
    /// Optional `#:` docstring on this field declaration, captured by the
    /// docstring side-pass. Advisory, never validated, excluded from the
    /// canonical hash. See [[type docstring::au-type-system]].
    pub doc: Option<String>,
}

impl FieldDecl {
    /// Render the field's shape as a string, preferring the
    /// canonicalized form from `parsed_shape` when available and
    /// falling back to the verbatim `raw_shape` when parsing failed.
    /// Both wire-surface and human-output renderings funnel through
    /// this so the fallback policy lives in one place.
    ///
    /// The canonical hash renders field shapes through this, so normalized
    /// equality holds only for shapes that PARSE. Two unparseable shapes
    /// compare by their verbatim `raw_shape`, so a formatting-only difference
    /// between them reads as a real difference. A broken shape is a load error
    /// regardless (`shape-syntax-error`), so this only affects the hash of an
    /// already-diagnosed def.
    pub fn shape_display(&self) -> String {
        match &self.parsed_shape {
            Ok(s) => s.to_string(),
            Err(_) => self.raw_shape.clone(),
        }
    }

    /// Re-qualify this declaration's shape, rewriting bare user-type names to
    /// `repo` (see [`Shape::qualify_bare`]). Both `parsed_shape` and `raw_shape`
    /// are updated so every consumer (validation, the wire's `shape_display`,
    /// diagnostic messages) reads the same re-qualified form. Used on a FOLDED
    /// peer type's fields, whose bare names are the peer's OWN types. An
    /// unparseable shape is left verbatim, it never held a resolvable name.
    pub fn qualified_to(&self, repo: &str) -> FieldDecl {
        match &self.parsed_shape {
            Ok(shape) => {
                let requalified = shape.qualify_bare(repo);
                FieldDecl {
                    raw_shape: requalified.to_string(),
                    parsed_shape: Ok(requalified),
                    ..self.clone()
                }
            }
            Err(_) => self.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetaBlock {
    pub type_name: TypeName,
    /// The `::repo` peer qualifier on the meta type, `None` for an own meta type.
    /// A qualified meta type resolves against the peer's graph via the fold, like
    /// a `::repo` claim; own-graph paths defer it.
    pub repo: Option<String>,
    pub type_name_span: ByteRange,
    /// Full YAML span of the sub-region (including the `type:` discriminator).
    pub block_span: ByteRange,
    /// Body fields beyond the `type:` discriminator. Each entry is a key/value
    /// pair to be validated against the named meta-type-def's effective shape.
    /// Empty for `- type: x` with no further keys (which validates only if
    /// every field on `x` is optional — distinct from `meta: []` suppression).
    pub fields: Vec<InstanceField>,
    /// Span covering the sub-region body — used as the anchor for
    /// required-field-absent diagnostics so they point at the body rather
    /// than the host TypeDef. Today this matches `block_span`; narrow it to
    /// body-only later if diagnostic precision warrants.
    pub body_span: ByteRange,
    /// The block's own `#:` head docstring, a leading `#:` block before its
    /// first key, `None` when absent. Advisory, see [[type docstring::au-type-system]].
    pub doc: Option<String>,
    /// Per-field `#:` docstrings of the meta body, keyed by field key. Only
    /// documented fields appear. Advisory.
    pub field_docs: std::collections::BTreeMap<String, String>,
}

/// `meta_blocks` is `Option<Vec<MetaBlock>>` to distinguish spec [[type-def meta::au-type-system]]'s three
/// states:
/// - `None` → `meta:` key absent. Consumer walks ([[type-def meta::au-type-system]]) flow to ancestors.
/// - `Some(vec![])` → explicit `meta: []`. Suppression marker — walks halt here.
/// - `Some(vec![..])` → declared sub-regions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypeDef {
    pub name: TypeName,
    pub source_path: PathBuf,
    pub source_span: ByteRange,
    pub parent_claim: Option<ParentClaim>,
    pub parents: Vec<TypeNameClaim>,
    pub fields: Vec<FieldDecl>,
    /// A brand's underlying shape, `Some` when the def declares `shape:` instead
    /// of `fields:`, naming a scalar, named enum, named union, or tuple as a
    /// reusable type. A def is a brand XOR a record, enforced in `load_checks`.
    /// See [[type-def::au-type-system]] and
    /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    pub shape: Option<BrandShape>,
    pub sealed: Vec<TypeNameClaim>,
    /// The raw `abstract: true` marker: this type-def is non-claimable but open
    /// to any subtype ([[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]]).
    /// This is the DECLARED flag only. The derived "not directly claimable"
    /// predicate is `declared_abstract OR sealed`, so callers wanting the full
    /// property go through the graph's `is_abstract`, never this field alone.
    pub declared_abstract: bool,
    pub meta_blocks: Option<Vec<MetaBlock>>,
    /// `required:` obligations declared inside `meta:`: every non-abstract type
    /// whose closure includes this def must carry a meta of each named type
    /// ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
    /// Each entry is a meta type name, `::repo`-qualifiable. Empty for a def with
    /// no `required:` item. Distinct from `meta_blocks`, which holds descriptive
    /// value blocks; a `required:` item carries an obligation, not a value.
    pub required_meta: Vec<TypeNameClaim>,
    /// Body template per [[type-def body::au-type-system]]. `None` → key absent; `Some(vec![])` → empty
    /// `body: []` (treated like absent for behavior); `Some(non-empty)` →
    /// declared template, type-def becomes markdown-only per [[type-def body::au-type-system]].
    pub body: Option<crate::body::BodyTemplate>,
    /// Optional `#:` docstring for the type-def itself: a leading `#:` block
    /// before the first top-level key. Advisory, never validated, excluded from
    /// the canonical hash. See [[type docstring::au-type-system]].
    pub doc: Option<String>,
    /// Optional `location:` block constraining where this type's instances live
    /// ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]).
    /// ADVISORY and out of the canonical hash, the `doc` precedent: a declared
    /// property that is not the value contract, so it never participates in type
    /// identity. Per-instance matching lives in `validate`.
    pub location: Option<crate::location::LocationSpec>,
}

/// A brand's underlying shape ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
/// Held on a [`TypeDef`] whose `shape:` key names a scalar, named enum, named
/// union, or tuple instead of declaring `fields:`.
#[derive(Debug, Clone, PartialEq)]
pub struct BrandShape {
    /// The parsed underlying shape. A scalar is `Shape::Primitive` / `Refined`,
    /// a named enum is `Shape::Enum`, a named union is `Shape::Union` /
    /// `CompoundReference`, a tuple is `Shape::Tuple`.
    pub shape: Shape,
    /// Per-enum-member `#:` docstrings, keyed by the member literal. Empty for a
    /// non-enum brand and for a docless enum. Advisory, recovered by a side pass
    /// and excluded from the canonical hash, like every docstring. See
    /// [[type docstring::au-type-system]].
    pub member_docs: BTreeMap<String, String>,
    /// The `shape:` value span, for diagnostics and per-member doc recovery.
    pub span: ByteRange,
}

impl BrandShape {
    /// A NOMINAL brand, a scalar / enum / tuple: a fresh identity over a
    /// representation, inline-only, so `*` / `&` reject and the value is a bare
    /// scalar or a `Name(...)` constructor. A STRUCTURAL brand (a union over
    /// record types) is the complement, referenceable and inline-record-valued.
    /// Keyed on the underlying shape, see
    /// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    pub fn is_nominal(&self) -> bool {
        matches!(
            self.shape,
            Shape::Primitive(_) | Shape::Refined { .. } | Shape::Enum(_) | Shape::Tuple(_)
        )
    }
}

#[derive(Debug, Default)]
pub struct ParseResult {
    pub type_def: Option<TypeDef>,
    pub diagnostics: Vec<Diagnostic>,
    /// Navigational `[[...]]` wikilinks captured from the type-def's `#:`
    /// docstrings, each tagged with the head or field it documents. Navigational
    /// only, derived from the doc text, so advisory and out of the canonical
    /// hash. A derived parse artifact, held beside the `TypeDef` rather than in
    /// it so the widely-constructed struct stays lean. See
    /// [[spec - docstring navigational links - a docstring's wikilinks resolve as navigational edges tagged to their declaration]].
    pub doc_links: Vec<crate::instance::DocstringLink>,
}

/// Derive a type-def's name from its file path.
///
/// - `*.type.yaml` / `*.type.yml` → strip the `.type.{yaml,yml}` suffix.
/// - any other `*.yaml` / `*.yml` → strip the `.{yaml,yml}` suffix.
///
/// This does NOT classify a path as a type-def: it strips a yaml suffix from any
/// yaml path, so the caller passes only paths already known to be type-defs (a
/// `*.type.yaml` / `*.type.yml` file; see [`au_parser::classify_by_path`]). For a
/// non-type-def yaml path the result is
/// just its stem, which callers may rely on — a plain `.yaml` instance's derived
/// name equals its stem, so au-engine's rename spelling logic reads the same
/// value whether it treats the path as a type-def or not.
pub fn type_name_from_path(path: &Path) -> Option<TypeName> {
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    let stripped = file_name
        .strip_suffix(".type.yaml")
        .or_else(|| file_name.strip_suffix(".type.yml"))
        .or_else(|| file_name.strip_suffix(".yaml"))
        .or_else(|| file_name.strip_suffix(".yml"))?;
    if stripped.is_empty() {
        return None;
    }
    Some(TypeName(stripped.to_string()))
}

/// Parse a type-def from a YAML document. `source` is the entire file content;
/// `yaml_offset` is the byte offset within `source` where the YAML body begins
/// (0 for pure-YAML type-def files; the frontmatter offset for markdown).
pub fn parse_type_def(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    doc: &MarkedYaml<'_>,
) -> ParseResult {
    let mut diagnostics = Vec::new();

    let name = match type_name_from_path(path) {
        Some(n) => n,
        None => {
            return ParseResult {
                type_def: None,
                diagnostics,
                doc_links: Vec::new(),
            }
        }
    };

    let source_span = span_to_byte_range(source, yaml_offset, doc.span);

    let mapping = match &doc.data {
        YamlData::Mapping(m) => m,
        _ => {
            diagnostics.push(diag(
                codes::TYPE_DEF_NOT_A_MAPPING,
                Severity::Error,
                path,
                source_span,
                "type-def must be a YAML mapping at the top level",
            ));
            return ParseResult {
                type_def: None,
                diagnostics,
                doc_links: Vec::new(),
            };
        }
    };

    let mut parent_claim: Option<ParentClaim> = None;
    let mut parents: Vec<TypeNameClaim> = Vec::new();
    let mut fields: Vec<FieldDecl> = Vec::new();
    let mut sealed: Vec<TypeNameClaim> = Vec::new();
    // The `abstract: true` marker; false until seen, see [[type-def sealed::au-type-system]] for
    // the sealed-implies-abstract factoring resolved in the graph predicate.
    let mut declared_abstract = false;
    // `None` until a `meta:` key is seen; `Some(vec![])` for explicit
    // `meta: []` ([[type-def meta::au-type-system]] suppression marker); `Some(vec![..])` once any
    // sub-region parses. Sub-regions that fail to parse still leave the
    // outer Option populated — the user wrote `meta:`, so absence is wrong.
    let mut meta_blocks: Option<Vec<MetaBlock>> = None;
    // `required:` obligations gathered from `meta:` items, see [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
    let mut required_meta: Vec<TypeNameClaim> = Vec::new();
    let mut body: Option<crate::body::BodyTemplate> = None;
    // A brand's underlying shape, `Some` once a `shape:` key parses. A def is a
    // brand XOR a record, the conflict is a load check, not a parse error.
    let mut brand_shape: Option<BrandShape> = None;
    // The `location:` block, `Some` once it parses. Advisory, out of the hash.
    let mut location: Option<crate::location::LocationSpec> = None;
    // First top-level key offset, the boundary for the type-def docstring: a
    // leading `#:` block before it documents the def, see [[type docstring::au-type-system]].
    let mut first_key_offset: Option<usize> = None;
    // Byte ranges the docstring side-pass skips. `meta:` and `body:` values
    // hold free-form text a user may write as a YAML block scalar, where a
    // `#:`-looking line is prose, not a doc comment. Docstrings attach only to
    // the type-def head and field declarations, so excluding these ranges keeps
    // capture correct without block-scalar lexing. See [[type docstring::au-type-system]].
    let mut doc_skip_ranges: Vec<ByteRange> = Vec::new();
    // Navigational links captured from the type-def's `#:` docstrings, the head
    // and fields pass plus the separate meta-block pass. See [[type docstring::au-type-system]].
    let mut doc_links: Vec<crate::instance::DocstringLink> = Vec::new();

    for (key, value) in mapping.iter() {
        let key_start = span_to_byte_range(source, yaml_offset, key.span).start;
        first_key_offset = Some(first_key_offset.map_or(key_start, |o| o.min(key_start)));
        let Some(key_str) = scalar_string(key) else {
            diagnostics.push(diag(
                codes::MAPPING_KEY_NOT_A_STRING,
                Severity::Warning,
                path,
                span_to_byte_range(source, yaml_offset, key.span),
                "type-def top-level mapping key must be a string",
            ));
            continue;
        };
        match key_str.as_str() {
            "extends" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                match &value.data {
                    YamlData::Value(Scalar::String(s)) => {
                        parents.push(TypeNameClaim::parse(s, value_span));
                        parent_claim = Some(ParentClaim {
                            form: ParentClaimForm::BareName,
                            value_span,
                        });
                    }
                    YamlData::Sequence(items) => {
                        parent_claim = Some(ParentClaim {
                            form: ParentClaimForm::List,
                            value_span,
                        });
                        // Empty parent list (`type: []`) is rejected at parse
                        // time per [[type list form::au-type-system]]. Reuses
                        // PARENT_CLAIM_BAD_SHAPE so callers see a single
                        // claim-shape error rather than a downstream surprise.
                        if items.is_empty() {
                            diagnostics.push(diag(
                                codes::PARENT_CLAIM_BAD_SHAPE,
                                Severity::Error,
                                path,
                                value_span,
                                "`extends:` list cannot be empty — a list-form parent claim must name at least one type",
                            ));
                            continue;
                        }
                        for item in items {
                            let item_span = span_to_byte_range(source, yaml_offset, item.span);
                            if let Some(s) = scalar_string(item) {
                                parents.push(TypeNameClaim::parse(&s, item_span));
                            } else {
                                diagnostics.push(diag(
                                    codes::PARENT_CLAIM_BAD_SHAPE,
                                    Severity::Error,
                                    path,
                                    item_span,
                                    "`extends:` list entry must be a string",
                                ));
                            }
                        }
                    }
                    _ => {
                        diagnostics.push(diag(
                            codes::PARENT_CLAIM_BAD_SHAPE,
                            Severity::Error,
                            path,
                            value_span,
                            "`extends:` must be a string or list of strings",
                        ));
                    }
                }
            }
            "fields" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                match &value.data {
                    // The canonical form: a map keyed by field name. An empty
                    // `{}` is a [[type tag::au-type-system]], the same as omitting `fields:`.
                    YamlData::Mapping(m) => {
                        let mut direct_key_ranges: Vec<ByteRange> = Vec::new();
                        for (key, val) in m.iter() {
                            let name_range = span_to_byte_range(source, yaml_offset, key.span);
                            direct_key_ranges.push(name_range);
                            let val_range = span_to_byte_range(source, yaml_offset, val.span);
                            // Anchor the entry at the key so `#:` doc binding
                            // (`attach_docstrings`) works over the map entries.
                            let entry_span = ByteRange::new(name_range.start, val_range.end);
                            parse_field_entry(
                                path,
                                source,
                                yaml_offset,
                                key,
                                val,
                                entry_span,
                                &mut fields,
                                &mut diagnostics,
                            );
                        }
                        // saphyr collapses duplicate keys (keeps the last), so a
                        // twice-declared field is invisible above. Recover it from
                        // the source event stream, and surface each as
                        // `duplicate-field` so the dropped earlier decl is not
                        // silent. A dup counts only when one of its two
                        // occurrences is a DIRECT field key (its span matches a
                        // `m.iter()` key span); a plain byte-span containment
                        // would also catch a duplicate key nested inside a field's
                        // inline-record shape (`bar: {x: A, x: B}`), which is a
                        // record key, not a field. Match either occurrence,
                        // since which of the two saphyr keeps is not relied on.
                        // The engine's generic scan drops the coinciding
                        // `duplicate-key-in-mapping` at this span.
                        for dup in scan_duplicate_keys(source) {
                            let dup_range =
                                span_to_byte_range(source, yaml_offset, dup.duplicate_span);
                            let first_range =
                                span_to_byte_range(source, yaml_offset, dup.first_span);
                            if direct_key_ranges.contains(&dup_range)
                                || direct_key_ranges.contains(&first_range)
                            {
                                diagnostics.push(Diagnostic {
                                    code: codes::DUPLICATE_FIELD,
                                    // Advisory: a duplicate is recoverable
                                    // (saphyr last-wins), so it surfaces the
                                    // dropped decl without aborting the def.
                                    severity: Severity::Warning,
                                    span: Span::new(path, dup_range),
                                    message: format!(
                                        "field '{}' is declared more than once in this type-def",
                                        dup.key
                                    ),
                                    related: vec![Span::new(path, first_range)],
                                    fix: None,
                                });
                            }
                        }
                    }
                    // A sequence is the retired list form; guide the migration.
                    YamlData::Sequence(_) => {
                        let mut d = diag(
                            codes::FIELDS_NOT_A_MAP,
                            Severity::Error,
                            path,
                            value_span,
                            "`fields:` must be a map keyed by field name, not a list",
                        );
                        d.fix = Some(au_diagnostics::SuggestedFix {
                            description:
                                "write each field as `name: shape` under `fields:`, not `- name: shape`"
                                    .to_string(),
                        });
                        diagnostics.push(d);
                    }
                    _ => {
                        diagnostics.push(diag(
                            codes::FIELDS_NOT_A_MAP,
                            Severity::Error,
                            path,
                            value_span,
                            "`fields:` must be a map keyed by field name",
                        ));
                    }
                }
            }
            "sealed" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                let Some(items) = as_sequence(value) else {
                    diagnostics.push(diag(
                        codes::SEALED_BAD_SHAPE,
                        Severity::Error,
                        path,
                        value_span,
                        "`sealed:` must be a list of type names",
                    ));
                    continue;
                };
                for item in items {
                    let item_span = span_to_byte_range(source, yaml_offset, item.span);
                    if let Some(s) = scalar_string(item) {
                        // Sealed branch names are this repo's own type-defs; a
                        // `::repo` qualifier is not meaningful on a sealed leaf.
                        sealed.push(TypeNameClaim::own(TypeName(s), item_span));
                    } else {
                        diagnostics.push(diag(
                            codes::SEALED_BAD_SHAPE,
                            Severity::Error,
                            path,
                            item_span,
                            "`sealed:` entry must be a string",
                        ));
                    }
                }
            }
            "abstract" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                match &value.data {
                    YamlData::Value(Scalar::Boolean(b)) => {
                        declared_abstract = *b;
                    }
                    _ => {
                        diagnostics.push(diag(
                            codes::ABSTRACT_MARKER_BAD_SHAPE,
                            Severity::Error,
                            path,
                            value_span,
                            "`abstract:` must be a boolean (`true` or `false`)",
                        ));
                    }
                }
            }
            "meta" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                doc_skip_ranges.push(value_span);
                let Some(items) = as_sequence(value) else {
                    diagnostics.push(diag(
                        codes::META_NOT_A_LIST,
                        Severity::Error,
                        path,
                        value_span,
                        "`meta:` must be a list of typed sub-regions",
                    ));
                    continue;
                };
                if items.is_empty() {
                    // Explicit `meta: []` — the [[type-def meta::au-type-system]] suppression marker.
                    meta_blocks.get_or_insert_with(Vec::new);
                } else {
                    // Buffer locally so a list whose sub-regions all fail
                    // to parse stays distinguishable from explicit
                    // suppression — without this guard, `Some(vec![])`
                    // would halt `lookup_meta` and hide ancestor metas
                    // because of authoring mistakes.
                    let mut parsed: Vec<MetaBlock> = Vec::new();
                    for item in items {
                        // A `meta:` item is either a `required:` obligation or a
                        // `type:` value block, discriminated by the key. A
                        // `required:` item carries an obligation, not a value, so
                        // it never becomes a MetaBlock ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
                        if let YamlData::Mapping(m) = &item.data {
                            let required_value = m.iter().find_map(|(k, v)| {
                                (scalar_string(k).as_deref() == Some("required")).then_some(v)
                            });
                            if let Some(rv) = required_value {
                                parse_required_meta_item(
                                    path,
                                    source,
                                    yaml_offset,
                                    rv,
                                    &mut required_meta,
                                    &mut diagnostics,
                                );
                                continue;
                            }
                        }
                        parse_meta_block(
                            path,
                            source,
                            yaml_offset,
                            item,
                            &mut parsed,
                            &mut diagnostics,
                        );
                    }
                    // Recover `#:` docstrings over the meta list. The type-def
                    // head/field pass skips the `meta:` value range, so meta-block
                    // docs are recovered here. `key` is the `meta` key node.
                    let meta_key_end = span_to_byte_range(source, yaml_offset, key.span).end;
                    attach_meta_docs(
                        path,
                        source,
                        meta_key_end,
                        &mut parsed,
                        &mut diagnostics,
                        &mut doc_links,
                    );
                    if !parsed.is_empty() {
                        match &mut meta_blocks {
                            Some(existing) => existing.extend(parsed),
                            None => meta_blocks = Some(parsed),
                        }
                    }
                }
            }
            "body" => {
                doc_skip_ranges.push(span_to_byte_range(source, yaml_offset, value.span));
                let items = crate::body::parse_body_value(
                    path,
                    source,
                    yaml_offset,
                    value,
                    false,
                    &mut diagnostics,
                );
                body = Some(items);
            }
            "shape" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                // Skip the value range in the head docstring pass: a named enum's
                // per-member `#:` docs live inside the block-list sequence here,
                // recovered by a separate pass, and must not read as dangling. The
                // range is snapped to the last member's line end, so a trailing
                // `#:` on the final member (just past the sequence value span) is
                // skipped too. Harmless for a scalar shape, whose value has no `#:`.
                let skip_end = crate::instance::snap_to_next_line(source, value_span.end);
                doc_skip_ranges.push(ByteRange::new(value_span.start, skip_end));
                brand_shape = parse_brand_shape(
                    path,
                    source,
                    yaml_offset,
                    value,
                    value_span,
                    &mut diagnostics,
                );
            }
            "location" => {
                let value_span = span_to_byte_range(source, yaml_offset, value.span);
                location = parse_location_block(
                    path,
                    source,
                    yaml_offset,
                    value,
                    value_span,
                    &mut diagnostics,
                );
            }
            "type" => {
                // On a type-def the inheritance claim is `extends:`; a top-level
                // `type:` is almost always an `extends:` claim written with the
                // wrong key. Fire a targeted error rather than dropping it as
                // UNKNOWN_TOP_LEVEL_KEY, which would silently strip the parent
                // and vanish its fields from every instance. See [[type-def extends::au-type-system]].
                diagnostics.push(diag(
                    codes::TYPE_KEY_ON_TYPE_DEF,
                    Severity::Error,
                    path,
                    span_to_byte_range(source, yaml_offset, key.span),
                    "`type:` on a type-def is not a parent claim; did you mean `extends:`?",
                ));
            }
            _ => {
                diagnostics.push(diag(
                    codes::UNKNOWN_TOP_LEVEL_KEY,
                    Severity::Warning,
                    path,
                    span_to_byte_range(source, yaml_offset, key.span),
                    format!(
                        "unknown top-level key '{}'; expected one of: extends, fields, sealed, abstract, meta, body, shape, location",
                        key_str
                    ),
                ));
            }
        }
    }

    // Docstring side-pass: scan the type-def's source region for `#:` comments
    // and attach them to field declarations or the type-def. See [[type docstring::au-type-system]].
    // Region starts at `yaml_offset`, not `source_span.start`: the doc span
    // begins at the first node, so a leading `#:` block before the first key
    // (the type-def docstring) would otherwise fall outside the scan.
    let (doc, head_and_field_links) = attach_docstrings(
        path,
        source,
        ByteRange::new(yaml_offset, source_span.end),
        first_key_offset,
        &doc_skip_ranges,
        &mut fields,
        &mut diagnostics,
    );
    doc_links.extend(head_and_field_links);

    ParseResult {
        type_def: Some(TypeDef {
            name,
            source_path: path.to_path_buf(),
            source_span,
            parent_claim,
            parents,
            fields,
            shape: brand_shape,
            sealed,
            declared_abstract,
            meta_blocks,
            required_meta,
            body,
            doc,
            location,
        }),
        diagnostics,
        doc_links,
    }
}

/// One field declaration from its `(key, value)` nodes under the `fields:`
/// map. `entry_span` anchors the declaration for `#:` doc binding, keyed at
/// the field name.
#[allow(clippy::too_many_arguments)]
fn parse_field_entry(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    key: &MarkedYaml<'_>,
    value: &MarkedYaml<'_>,
    entry_span: ByteRange,
    out: &mut Vec<FieldDecl>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(key_str) = scalar_string(key) else {
        diagnostics.push(diag(
            codes::FIELD_DECL_BAD_SHAPE,
            Severity::Error,
            path,
            span_to_byte_range(source, yaml_offset, key.span),
            "field name must be a string",
        ));
        return;
    };

    let (raw_name, optional) = if let Some(stripped) = key_str.strip_suffix('?') {
        (stripped.to_string(), true)
    } else {
        (key_str.clone(), false)
    };

    if !is_valid_field_name(&raw_name) {
        diagnostics.push(diag(
            codes::FIELD_DECL_BAD_SHAPE,
            Severity::Error,
            path,
            span_to_byte_range(source, yaml_offset, key.span),
            format!(
                "field name '{}' is not a valid identifier (must match [A-Za-z][A-Za-z0-9_-]*)",
                raw_name
            ),
        ));
        return;
    }

    let name_span = span_to_byte_range(source, yaml_offset, key.span);
    let shape_span = {
        let mut sp = span_to_byte_range(source, yaml_offset, value.span);
        // saphyr places the end marker for flow `[...]` and `{...}` ON the
        // closing bracket rather than after it, so the slice would drop the
        // closer. Extend by one byte when the next source byte is the
        // expected close — block-style sequences/mappings won't have this
        // pattern at that position, so the check is safe.
        let close = match &value.data {
            YamlData::Sequence(_) => Some(b']'),
            YamlData::Mapping(_) => Some(b'}'),
            _ => None,
        };
        if let Some(c) = close {
            if source.as_bytes().get(sp.end) == Some(&c) {
                sp = ByteRange::new(sp.start, sp.end + 1);
            }
        }
        sp
    };
    // A scalar shape is the YAML scalar's value, so quoting and surrounding
    // whitespace are normalized away before the shape grammar sees it: `"String"`
    // and `String` are the same shape, not a drift. A flow collection (an enum
    // list like `[draft, active]`) is not a scalar, so it falls back to the raw
    // source slice, which the grammar parses directly. `shape_span` still points
    // at the source for diagnostics either way.
    let raw_shape = match scalar_string(value) {
        Some(s) => s,
        None if shape_span.start <= shape_span.end && shape_span.end <= source.len() => {
            source[shape_span.start..shape_span.end].to_string()
        }
        None => String::new(),
    };

    let parsed_shape = parse_shape(&raw_shape).map_err(|e| shape_err_to_diag(e, path, shape_span));

    out.push(FieldDecl {
        name: FieldName(raw_name),
        optional,
        raw_shape,
        name_span,
        shape_span,
        entry_span,
        parsed_shape,
        doc: None,
    });
}

/// Parse a `shape:` value into a [`BrandShape`] ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
///
/// A scalar string is a shape EXPRESSION, a scalar (`Number`, `Number{...}`), a
/// named union (`<A | B>`), or a tuple (`(A, B)`), parsed by the slot grammar,
/// so a bad expression surfaces as `shape-syntax-error`. A YAML sequence is a
/// named enum, a block-list or an inline `[a, b]`, captured as `Shape::Enum`;
/// per-member `#:` docs are recovered by a later pass, so `member_docs` is empty
/// here. Any other value is a `malformed-brand-shape`.
/// The four brand forms ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]):
/// a scalar (`Primitive` / `Refined`), a named enum (`Enum`), a named union
/// (`Union`), or a tuple (`Tuple`). Every other parseable shape, a bare record
/// name, a `*` / `&` reference, a `<...>*` compound reference, a `type*` def-ref,
/// `any`, a list, a pin, an intersection, is NOT a brand and is
/// `malformed-brand-shape`.
fn is_legal_brand_shape(shape: &Shape) -> bool {
    matches!(
        shape,
        Shape::Primitive(_)
            | Shape::Refined { .. }
            | Shape::Enum(_)
            | Shape::Union(_)
            | Shape::Tuple(_)
    )
}

fn parse_brand_shape(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    value_span: ByteRange,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BrandShape> {
    match &value.data {
        YamlData::Value(Scalar::String(s)) => match parse_shape(s) {
            Ok(shape) if is_legal_brand_shape(&shape) => Some(BrandShape {
                shape,
                member_docs: BTreeMap::new(),
                span: value_span,
            }),
            // A parseable shape that is not one of the four brand forms, a bare
            // record name, a `*` / `&` reference, a `<...>*` compound reference, a
            // `type*` def-ref, `any`, a list, a pin, or an intersection. A brand
            // names a scalar, enum, union, or tuple, nothing else.
            Ok(bad) => {
                diagnostics.push(diag(
                    codes::MALFORMED_BRAND_SHAPE,
                    Severity::Error,
                    path,
                    value_span,
                    format!(
                        "`shape:` is '{bad}', not a brand form; a brand names a scalar, a named enum, a `<A | B>` union, or a `(A, B)` tuple"
                    ),
                ));
                None
            }
            Err(e) => {
                diagnostics.push(shape_err_to_diag(e, path, value_span));
                None
            }
        },
        YamlData::Sequence(items) => {
            if items.is_empty() {
                diagnostics.push(diag(
                    codes::MALFORMED_BRAND_SHAPE,
                    Severity::Error,
                    path,
                    value_span,
                    "`shape:` names an empty enum; a named enum must list at least one member",
                ));
                return None;
            }
            let mut spans: Vec<(String, ByteRange)> = Vec::with_capacity(items.len());
            for item in items {
                let item_span = span_to_byte_range(source, yaml_offset, item.span);
                match scalar_string(item) {
                    Some(m) => {
                        // A member must match the enum-literal charset (the
                        // [[type-def legal names::au-type-system]] token), the same rule the inline
                        // enum grammar enforces. Without it a member with an
                        // embedded comma (`"a, b"`) survives, and two different
                        // member sets canonicalize to the same `[a, b, c]`
                        // rendering, an identity collision.
                        if !crate::load_checks::is_valid_type_name(&m) {
                            diagnostics.push(diag(
                                codes::MALFORMED_BRAND_SHAPE,
                                Severity::Error,
                                path,
                                item_span,
                                format!(
                                    "`shape:` enum member '{m}' is not a legal name; a member is a letter then letters, digits, '_' or '-'"
                                ),
                            ));
                            return None;
                        }
                        spans.push((m, item_span));
                    }
                    None => {
                        diagnostics.push(diag(
                            codes::MALFORMED_BRAND_SHAPE,
                            Severity::Error,
                            path,
                            item_span,
                            "`shape:` enum member must be a string literal",
                        ));
                        return None;
                    }
                }
            }
            let member_docs = enum_member_docs(source, &spans, value_span);
            let members = spans.into_iter().map(|(m, _)| m).collect();
            Some(BrandShape {
                shape: Shape::Enum(members),
                member_docs,
                span: value_span,
            })
        }
        _ => {
            diagnostics.push(diag(
                codes::MALFORMED_BRAND_SHAPE,
                Severity::Error,
                path,
                value_span,
                "`shape:` must be a scalar shape, a `<A | B>` union, a `(A, B)` tuple, or a list of enum members",
            ));
            None
        }
    }
}

/// Recover per-member `#:` docstrings for a named-enum brand ([[type docstring::au-type-system]]).
///
/// A TRAILING `#:` on a member's line documents that member, keyed by the member
/// literal. The enum's HEAD doc is the type-def docstring, a `#:` block before
/// `shape:`, captured by the main docstring pass, not here. Docs are advisory and
/// excluded from the closure hash. `members` carries each member's source span,
/// so a trailing comment is matched to the member scalar on its line.
fn enum_member_docs(
    source: &str,
    members: &[(String, ByteRange)],
    value_span: ByteRange,
) -> BTreeMap<String, String> {
    // Extend to the end of the last member's line: a trailing `#:` on the final
    // member sits just past the sequence value span.
    let region_end = crate::instance::snap_to_next_line(source, value_span.end);
    let docs = scan_doc_comments(source, value_span.start, region_end, &[]);
    let mut out = BTreeMap::new();
    for doc in &docs {
        // Only a trailing doc: the member scalar precedes the `#:` on the line.
        if doc.own_line {
            continue;
        }
        if let Some((name, _)) = members
            .iter()
            .find(|(_, sp)| sp.start >= doc.line_start && sp.start < doc.hash_offset)
        {
            out.entry(name.clone()).or_insert_with(|| doc.text.clone());
        }
    }
    out
}

fn shape_err_to_diag(err: ShapeParseError, path: &Path, shape_span: ByteRange) -> Diagnostic {
    Diagnostic {
        code: err.code,
        severity: err.severity,
        span: Span::new(path, shape_span),
        message: err.message,
        related: vec![],
        fix: None,
    }
}

/// Parse a `required:` meta item's value into obligation claims. The value is
/// one meta type name or a list of names, per [[type list form::au-type-system]], each
/// `::repo`-qualifiable. A shape that is neither is `required-meta-bad-shape`.
/// See [[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]].
fn parse_required_meta_item(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    out: &mut Vec<TypeNameClaim>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &value.data {
        YamlData::Value(Scalar::String(s)) => {
            let span = span_to_byte_range(source, yaml_offset, value.span);
            out.push(TypeNameClaim::parse(s, span));
        }
        YamlData::Sequence(items) if !items.is_empty() => {
            for item in items {
                let item_span = span_to_byte_range(source, yaml_offset, item.span);
                if let Some(s) = scalar_string(item) {
                    out.push(TypeNameClaim::parse(&s, item_span));
                } else {
                    diagnostics.push(diag(
                        codes::REQUIRED_META_BAD_SHAPE,
                        Severity::Error,
                        path,
                        item_span,
                        "`required:` list entry must be a meta type name",
                    ));
                }
            }
        }
        _ => {
            diagnostics.push(diag(
                codes::REQUIRED_META_BAD_SHAPE,
                Severity::Error,
                path,
                span_to_byte_range(source, yaml_offset, value.span),
                "`required:` must be a meta type name or a non-empty list of names",
            ));
        }
    }
}

fn parse_meta_block(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    item: &MarkedYaml<'_>,
    out: &mut Vec<MetaBlock>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let block_span = span_to_byte_range(source, yaml_offset, item.span);

    let map = match &item.data {
        YamlData::Mapping(m) => m,
        _ => {
            diagnostics.push(diag(
                codes::META_BLOCK_BAD_SHAPE,
                Severity::Error,
                path,
                block_span,
                "meta block must be a mapping",
            ));
            return;
        }
    };

    let mut type_entry: Option<&MarkedYaml<'_>> = None;
    for (k, v) in map.iter() {
        if scalar_string(k).as_deref() == Some("type") {
            type_entry = Some(v);
            break;
        }
    }

    let Some(value) = type_entry else {
        diagnostics.push(diag(
            codes::META_BLOCK_BAD_SHAPE,
            Severity::Error,
            path,
            block_span,
            "meta block missing `type:` discriminator",
        ));
        return;
    };

    let (type_name, repo) = match &value.data {
        // Split the `::repo` peer qualifier, mirroring a `::repo` claim. A `:` is
        // illegal in a type name, so the `::` is unambiguous.
        YamlData::Value(Scalar::String(s)) => match s.split_once("::") {
            Some((base, r)) => (TypeName(base.to_string()), Some(r.to_string())),
            None => (TypeName(s.to_string()), None),
        },
        YamlData::Sequence(_) => {
            // Spec [[type-def meta::au-type-system]]: the meta sub-region's `type:` is single-name only.
            // The YAML-sequence arm catches both `type: [a, b]` (mixin form)
            // and `type: []` (empty list); the diagnostic message names
            // both so the corrective action is unambiguous regardless of
            // which edge case the user hit. Distinct code from
            // META_BLOCK_BAD_SHAPE so the rule is greppable on its own.
            diagnostics.push(diag(
                codes::META_MIXIN_NOT_SUPPORTED,
                Severity::Error,
                path,
                span_to_byte_range(source, yaml_offset, value.span),
                "meta sub-region `type:` must be a single name; list forms (`type: [a, b]` or `type: []`) are not supported",
            ));
            return;
        }
        _ => {
            diagnostics.push(diag(
                codes::META_BLOCK_BAD_SHAPE,
                Severity::Error,
                path,
                span_to_byte_range(source, yaml_offset, value.span),
                "meta block `type:` must be a single name (string)",
            ));
            return;
        }
    };

    // Body fields: every key other than `type:`. Reserved keys (`fields:`,
    // `sealed:`, `meta:`) inside a meta body fire RESERVED_KEY_ON_INSTANCE —
    // the same rule that protects inline values. Mapping keys that aren't
    // strings emit MAPPING_KEY_NOT_A_STRING.
    let mut fields: Vec<InstanceField> = Vec::new();
    for (k, v) in map.iter() {
        let Some(key_str) = scalar_string(k) else {
            diagnostics.push(diag(
                codes::MAPPING_KEY_NOT_A_STRING,
                Severity::Warning,
                path,
                span_to_byte_range(source, yaml_offset, k.span),
                "meta sub-region mapping key must be a string",
            ));
            continue;
        };
        if key_str == "type" {
            continue;
        }
        let key_span = span_to_byte_range(source, yaml_offset, k.span);
        let value_span = span_to_byte_range(source, yaml_offset, v.span);
        if matches!(
            key_str.as_str(),
            "fields" | "sealed" | "meta" | "body" | "shape"
        ) {
            diagnostics.push(diag(
                codes::RESERVED_KEY_ON_INSTANCE,
                Severity::Error,
                path,
                key_span,
                format!(
                    "`{}:` is a type-def-only key and cannot appear in a meta sub-region body",
                    key_str
                ),
            ));
            continue;
        }
        fields.push(InstanceField {
            key: key_str,
            key_span,
            value: classify_value(path, source, yaml_offset, v, diagnostics),
            value_span,
            nav_links: scalar_nav_links(source, yaml_offset, v),
        });
    }

    out.push(MetaBlock {
        type_name,
        repo,
        type_name_span: span_to_byte_range(source, yaml_offset, value.span),
        block_span,
        fields,
        // Today body_span equals block_span — narrows to body-only later if
        // diagnostic precision needs it (the additional precision is small;
        // the host TypeDef name is never the anchor regardless).
        body_span: block_span,
        // Filled by `attach_meta_docs` after the whole `meta:` list is parsed,
        // which owns the per-block region boundaries.
        doc: None,
        field_docs: std::collections::BTreeMap::new(),
    });
}

/// Recover `#:` docstrings for each parsed meta block over the `meta:` list.
///
/// Meta blocks are list items, so this mirrors the instance sequence walk:
/// each block's head `#:` sits before its first key (between the `meta:` key
/// or the previous block and this block's `type:`), and its span is greedy, so
/// each block is bounded by its own content end. `lower` is the `meta:` key's
/// end. See [[type docstring::au-type-system]] and the instance docstring pass.
fn attach_meta_docs(
    path: &Path,
    source: &str,
    lower: usize,
    blocks: &mut [MetaBlock],
    diagnostics: &mut Vec<Diagnostic>,
    doc_links: &mut Vec<crate::instance::DocstringLink>,
) {
    let mut prev_end = lower;
    for block in blocks.iter_mut() {
        let elem_lower = crate::instance::snap_to_next_line(source, prev_end.saturating_sub(1));
        let content_end = block
            .fields
            .iter()
            .map(|f| f.value_span.end)
            .chain(std::iter::once(block.type_name_span.end))
            .max()
            .unwrap_or(block.block_span.end);
        let elem_upper = crate::instance::snap_to_next_line(source, content_end);
        // The block's first key is where its span opens (`type:`); a leading
        // block before it is the head doc.
        let first_key = Some(block.block_span.start);
        let (doc, field_docs) = crate::instance::recover_scope(
            path,
            source,
            ByteRange::new(elem_lower, elem_upper),
            first_key,
            &block.fields,
            diagnostics,
            doc_links,
        );
        block.doc = doc;
        block.field_docs = field_docs;
        for field in block.fields.iter_mut() {
            crate::instance::recurse_docs(
                path,
                source,
                &mut field.value,
                field.key_span.end,
                field.value_span.end,
                diagnostics,
                doc_links,
            );
        }
        prev_end = content_end;
    }
}

/// True iff `s` is a valid field identifier: leading ASCII letter
/// followed by ASCII letters, digits, `_`, or `-`. Mirrors the single-
/// segment form of `load_checks::is_valid_type_name`. The optional-suffix
/// `?` is consumed by the parser before this is called, so `?`-trailing
/// names never reach this predicate.
fn is_valid_field_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ----- docstrings -----

/// One `#:` doc comment found in a YAML source region. Shared by the type-def
/// docstring pass here and the instance docstring pass in `instance.rs`.
#[derive(Clone)]
pub(crate) struct DocComment {
    /// File-relative byte offset of the `#`.
    pub(crate) hash_offset: usize,
    /// True when only whitespace precedes the `#` on its line.
    pub(crate) own_line: bool,
    /// The doc text after `#:`, trimmed.
    pub(crate) text: String,
    /// File-relative offset of the line start.
    pub(crate) line_start: usize,
    /// Span of `#:` to end of line, the anchor for a dangling-doc diagnostic.
    pub(crate) span: ByteRange,
}

/// The byte range of a doc comment's content, after the `#:` sigil (2 bytes)
/// to end of line. The one place the "`#:` is 2 bytes" fact lives, shared by the
/// type-def and instance docstring passes when they scan a comment for links.
pub(crate) fn doc_content_range(c: &DocComment) -> ByteRange {
    ByteRange::new(c.hash_offset + 2, c.span.end)
}

/// Scan a YAML region for `#:` doc comments. Respects YAML string
/// lexing, so a `#` inside a quoted scalar, or one not preceded by whitespace,
/// is not a comment. `skip` ranges (the `meta:` / `body:` values) are passed
/// through inert: their bytes still advance line tracking, but no comment is
/// captured there, so a `#:`-looking line inside a block scalar is not
/// mis-read. See [[type docstring::au-type-system]].
pub(crate) fn scan_doc_comments(
    source: &str,
    start: usize,
    end: usize,
    skip: &[ByteRange],
) -> Vec<DocComment> {
    let bytes = source.as_bytes();
    let end = end.min(source.len());
    let mut out = Vec::new();
    let mut i = start;
    let mut line_start = start;
    let mut line_has_content = false;
    enum Q {
        Normal,
        Single,
        Double,
    }
    let mut q = Q::Normal;
    while i < end {
        let c = bytes[i];
        // A skipped range (`meta:` / `body:` value) only advances line tracking;
        // comment detection is suppressed so block-scalar prose can't be read as
        // a docstring. The range starts at a value boundary, so `q` is `Normal`.
        if skip.iter().any(|r| i >= r.start && i < r.end) {
            if c == b'\n' {
                line_start = i + 1;
                line_has_content = false;
            }
            i += 1;
            continue;
        }
        match q {
            Q::Normal => match c {
                b'\n' => {
                    line_start = i + 1;
                    line_has_content = false;
                    i += 1;
                }
                b'\'' => {
                    q = Q::Single;
                    line_has_content = true;
                    i += 1;
                }
                b'"' => {
                    q = Q::Double;
                    line_has_content = true;
                    i += 1;
                }
                b' ' | b'\t' => i += 1,
                b'#' => {
                    let prev_ws = i == line_start || matches!(bytes[i - 1], b' ' | b'\t');
                    if prev_ws {
                        let own_line = !line_has_content;
                        let is_doc = bytes.get(i + 1) == Some(&b':');
                        let mut j = i + 1;
                        while j < end && bytes[j] != b'\n' {
                            j += 1;
                        }
                        if is_doc {
                            out.push(DocComment {
                                hash_offset: i,
                                own_line,
                                text: source[(i + 2)..j].trim().to_string(),
                                line_start,
                                span: ByteRange::new(i, j),
                            });
                        }
                        i = j;
                    } else {
                        // `#` glued to a scalar (no preceding whitespace) is content.
                        line_has_content = true;
                        i += 1;
                    }
                }
                _ => {
                    line_has_content = true;
                    i += 1;
                }
            },
            Q::Single => match c {
                // `''` is an escaped quote inside a single-quoted scalar.
                b'\'' => {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                    } else {
                        q = Q::Normal;
                        line_has_content = true;
                        i += 1;
                    }
                }
                b'\n' => {
                    line_start = i + 1;
                    line_has_content = false;
                    i += 1;
                }
                _ => i += 1,
            },
            Q::Double => match c {
                // `\` escapes the next char in a double-quoted scalar.
                b'\\' => i += 2,
                b'"' => {
                    q = Q::Normal;
                    line_has_content = true;
                    i += 1;
                }
                b'\n' => {
                    line_start = i + 1;
                    line_has_content = false;
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }
    out
}

/// Attach `#:` docstrings to field declarations and the type-def. Returns the
/// type-def's own doc (a leading block before the first key). A comment that
/// binds to nothing fires `dangling-doc-comment`. See [[type docstring::au-type-system]].
fn attach_docstrings(
    path: &Path,
    source: &str,
    region: ByteRange,
    first_key_offset: Option<usize>,
    skip: &[ByteRange],
    fields: &mut [FieldDecl],
    diagnostics: &mut Vec<Diagnostic>,
) -> (Option<String>, Vec<crate::instance::DocstringLink>) {
    let comments = scan_doc_comments(source, region.start, region.end, skip);
    if comments.is_empty() {
        return (None, Vec::new());
    }
    let mut td_parts: Vec<String> = Vec::new();
    let mut field_parts: Vec<Vec<String>> = vec![Vec::new(); fields.len()];
    let mut doc_links: Vec<crate::instance::DocstringLink> = Vec::new();
    // Scanning the raw comment content (via `doc_content_range`) keeps link
    // spans absolute and honors the `\[[` escape.
    for c in &comments {
        let target = if c.own_line {
            if first_key_offset.map_or(false, |fk| c.hash_offset < fk) {
                td_parts.push(c.text.clone());
                for link in crate::instance::nav_links_in_range(source, doc_content_range(c)) {
                    doc_links.push(crate::instance::DocstringLink {
                        origin: crate::instance::DocOrigin::Head,
                        link,
                    });
                }
                continue;
            }
            // Leading block binds to the nearest following field declaration.
            fields
                .iter()
                .position(|f| f.entry_span.start > c.hash_offset)
        } else {
            // Trailing comment binds to the field declaration on its own line.
            fields.iter().position(|f| {
                f.entry_span.start >= c.line_start && f.entry_span.start < c.hash_offset
            })
        };
        match target {
            Some(idx) => {
                field_parts[idx].push(c.text.clone());
                let origin =
                    crate::instance::DocOrigin::Field(fields[idx].name.as_str().to_string());
                for link in crate::instance::nav_links_in_range(source, doc_content_range(c)) {
                    doc_links.push(crate::instance::DocstringLink {
                        origin: origin.clone(),
                        link,
                    });
                }
            }
            None => diagnostics.push(diag(
                codes::DANGLING_DOC_COMMENT,
                Severity::Warning,
                path,
                c.span,
                "`#:` doc comment is attached to no declaration; it is dropped",
            )),
        }
    }
    for (idx, parts) in field_parts.into_iter().enumerate() {
        if !parts.is_empty() {
            fields[idx].doc = Some(parts.join("\n"));
        }
    }
    let doc = (!td_parts.is_empty()).then(|| td_parts.join("\n"));
    (doc, doc_links)
}

// ----- saphyr helpers -----

fn scalar_string(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn as_sequence<'a, 'b>(node: &'a MarkedYaml<'b>) -> Option<&'a [MarkedYaml<'b>]> {
    match &node.data {
        YamlData::Sequence(items) => Some(items),
        _ => None,
    }
}

/// Parse a `location:` block into a [`crate::location::LocationSpec`]. Emits
/// `location-bad-shape` for a non-mapping block, a mistyped sub-value, an
/// unknown sub-key, or a malformed `name` template / `path` glob. The
/// field-safety and `fileType`/`body` checks that need the type's own fields
/// run later in `load_checks`. Returns `Some` even with per-sub-key errors, so a
/// partly-valid block still carries the sub-keys that parsed.
fn parse_location_block(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    value_span: ByteRange,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<crate::location::LocationSpec> {
    use crate::location::{FileType, LocationSpec, NameTemplate, PathGlob};
    let YamlData::Mapping(m) = &value.data else {
        diagnostics.push(diag(
            codes::LOCATION_BAD_SHAPE,
            Severity::Error,
            path,
            value_span,
            "`location:` must be a mapping of name / path / fileType / strict",
        ));
        return None;
    };
    let mut spec = LocationSpec {
        block_span: value_span,
        ..LocationSpec::default()
    };
    for (k, v) in m.iter() {
        let v_span = span_to_byte_range(source, yaml_offset, v.span);
        let Some(k_str) = scalar_string(k) else {
            diagnostics.push(diag(
                codes::LOCATION_BAD_SHAPE,
                Severity::Error,
                path,
                span_to_byte_range(source, yaml_offset, k.span),
                "`location` sub-key must be a string",
            ));
            continue;
        };
        let bad = |msg: String, d: &mut Vec<Diagnostic>| {
            d.push(diag(
                codes::LOCATION_BAD_SHAPE,
                Severity::Error,
                path,
                v_span,
                msg,
            ));
        };
        match k_str.as_str() {
            "name" => {
                spec.name_span = v_span;
                match scalar_string(v) {
                    Some(s) => match NameTemplate::parse(&s) {
                        Ok(t) => spec.name = Some(t),
                        Err(e) => bad(format!("`location.name`: {e}"), diagnostics),
                    },
                    None => bad(
                        "`location.name` must be a string template".into(),
                        diagnostics,
                    ),
                }
            }
            "path" => match scalar_string(v) {
                Some(s) => match PathGlob::parse(&s) {
                    Ok(g) => spec.path = Some(g),
                    Err(e) => bad(format!("`location.path`: {e}"), diagnostics),
                },
                None => bad("`location.path` must be a string glob".into(), diagnostics),
            },
            "fileType" => {
                spec.file_type_span = v_span;
                match scalar_string(v) {
                    Some(s) => match FileType::parse(&s) {
                        Some(ft) => spec.file_type = Some(ft),
                        None => bad(
                            "`location.fileType` must be `md` or `yaml`".into(),
                            diagnostics,
                        ),
                    },
                    None => bad(
                        "`location.fileType` must be `md` or `yaml`".into(),
                        diagnostics,
                    ),
                }
            }
            "strict" => match &v.data {
                YamlData::Value(Scalar::Boolean(b)) => spec.strict = *b,
                _ => bad("`location.strict` must be a boolean".into(), diagnostics),
            },
            other => bad(
                format!(
                    "unknown `location` sub-key '{other}'; expected name, path, fileType, strict"
                ),
                diagnostics,
            ),
        }
    }
    Some(spec)
}

fn diag(
    code: DiagnosticCodeAlias,
    severity: Severity,
    path: &Path,
    range: ByteRange,
    message: impl Into<String>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        span: Span::new(path, range),
        message: message.into(),
        related: vec![],
        fix: None,
    }
}

type DiagnosticCodeAlias = au_diagnostics::DiagnosticCode;

#[cfg(test)]
mod tests {
    use super::*;
    use au_parser::yaml::parse;

    fn parse_one(source: &str, path: &str) -> ParseResult {
        let docs = parse(source).unwrap();
        parse_type_def(Path::new(path), source, 0, &docs[0])
    }

    #[test]
    fn brand_scalar_parses_as_a_shape() {
        let r = parse_one("shape: Number\n", "/v/meter.type.yaml");
        let td = r.type_def.expect("parses");
        assert!(td.fields.is_empty(), "a brand has no fields");
        let brand = td.shape.expect("a brand shape");
        assert_eq!(brand.shape, Shape::Primitive(au_grammar::Primitive::Number));
        assert!(brand.member_docs.is_empty());
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    }

    #[test]
    fn brand_refined_scalar_parses_as_a_refined_shape() {
        // `percent := shape: Number{>=0 & <=100}` reuses the shipped refinement
        // machinery: the brand accumulator stores a `Shape::Refined`, and a
        // refined scalar is still NOMINAL, inline-only.
        let r = parse_one("shape: Number{>=0 & <=100}\n", "/v/percent.type.yaml");
        let td = r.type_def.expect("parses");
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
        let brand = td.shape.expect("a brand shape");
        assert!(
            matches!(
                brand.shape,
                Shape::Refined {
                    base: au_grammar::Primitive::Number,
                    ..
                }
            ),
            "got {:?}",
            brand.shape
        );
        assert!(brand.is_nominal(), "a refined scalar brand is nominal");
    }

    #[test]
    fn a_shape_outside_the_four_forms_is_malformed() {
        // A parseable shape that is not scalar / enum / union / tuple is not a
        // brand: a bare record name, a reference, a compound reference, a def-ref,
        // any, a list, a pin, an intersection.
        for src in [
            "shape: note\n",     // bare record name
            "shape: note*\n",    // typed reference
            "shape: note&\n",    // inline-or-reference
            "shape: <a | b>*\n", // compound reference
            "shape: type*\n",    // def-reference
            "shape: any\n",      // no-type
            "shape: Number[]\n", // list
            "shape: <a & b>\n",  // intersection
        ] {
            let r = parse_one(src, "/v/bad.type.yaml");
            assert!(
                r.diagnostics
                    .iter()
                    .any(|d| d.code == crate::codes::MALFORMED_BRAND_SHAPE),
                "{src:?} should be malformed-brand-shape, got {:?}",
                r.diagnostics
                    .iter()
                    .map(|d| d.code.as_str())
                    .collect::<Vec<_>>()
            );
            assert!(
                r.type_def.map(|t| t.shape.is_none()).unwrap_or(true),
                "{src:?} should not store a brand"
            );
        }
    }

    #[test]
    fn a_block_list_enum_member_must_be_a_legal_name() {
        // A comma-bearing member would canonicalize identically to a different
        // member set, an identity collision (code-review 3.3).
        let r = parse_one("shape:\n  - \"a, b\"\n  - c\n", "/v/e.type.yaml");
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == crate::codes::MALFORMED_BRAND_SHAPE),
            "an illegal enum member is malformed-brand-shape, got {:?}",
            r.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_four_brand_forms_are_accepted() {
        for src in [
            "shape: Number\n",
            "shape: Number{>=0}\n",
            "shape: [low, high]\n",
            "shape: <paper | observation>\n",
            "shape: (Number, Number)\n",
        ] {
            let r = parse_one(src, "/v/ok.type.yaml");
            assert!(r.diagnostics.is_empty(), "{src:?}: {:?}", r.diagnostics);
            assert!(
                r.type_def.unwrap().shape.is_some(),
                "{src:?} stores a brand"
            );
        }
    }

    #[test]
    fn brand_named_union_parses_as_a_structural_union() {
        // `shape: <paper | observation>` reuses `parse_shape`; the members carry
        // their own record identity, and the brand is STRUCTURAL (not nominal).
        let r = parse_one(
            "shape: <paper | observation>\n",
            "/v/evidence-kind.type.yaml",
        );
        let td = r.type_def.expect("parses");
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
        let brand = td.shape.expect("a brand shape");
        assert!(
            !brand.is_nominal(),
            "a union brand is structural, not nominal"
        );
        match &brand.shape {
            Shape::Union(members) => assert_eq!(members.len(), 2),
            other => panic!("expected a Union, got {other:?}"),
        }
    }

    #[test]
    fn brand_named_enum_block_list_parses_members() {
        let src = "shape:\n  - save\n  - delete\n  - open\n";
        let r = parse_one(src, "/v/icon-role.type.yaml");
        let brand = r.type_def.expect("parses").shape.expect("a brand");
        assert_eq!(
            brand.shape,
            Shape::Enum(vec!["save".into(), "delete".into(), "open".into()])
        );
    }

    #[test]
    fn brand_inline_enum_parses_members() {
        let r = parse_one("shape: [low, high]\n", "/v/level.type.yaml");
        let brand = r.type_def.expect("parses").shape.expect("a brand");
        assert_eq!(brand.shape, Shape::Enum(vec!["low".into(), "high".into()]));
    }

    #[test]
    fn brand_named_enum_captures_per_member_docs() {
        let src = "shape:\n  - save        #: persist state\n  - delete      #: remove it\n  - open        #: reveal\n";
        let r = parse_one(src, "/v/icon-role.type.yaml");
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
        let brand = r.type_def.expect("parses").shape.expect("a brand");
        assert_eq!(
            brand.member_docs.get("save").map(String::as_str),
            Some("persist state")
        );
        assert_eq!(
            brand.member_docs.get("delete").map(String::as_str),
            Some("remove it")
        );
        // The LAST member's trailing doc, just past the sequence span, is captured.
        assert_eq!(
            brand.member_docs.get("open").map(String::as_str),
            Some("reveal")
        );
    }

    #[test]
    fn brand_enum_head_doc_is_the_type_def_doc_not_a_member_doc() {
        let src = "#: the maturity ladder\nshape:\n  - seed   #: base stub\n";
        let r = parse_one(src, "/v/quality.type.yaml");
        assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
        let td = r.type_def.expect("parses");
        assert_eq!(td.doc.as_deref(), Some("the maturity ladder"));
        let brand = td.shape.expect("a brand");
        assert_eq!(
            brand.member_docs.get("seed").map(String::as_str),
            Some("base stub")
        );
        assert_eq!(brand.member_docs.len(), 1);
    }

    #[test]
    fn brand_docless_enum_has_empty_member_docs() {
        let r = parse_one("shape:\n  - low\n  - high\n", "/v/level.type.yaml");
        let brand = r.type_def.expect("parses").shape.expect("a brand");
        assert!(brand.member_docs.is_empty());
    }

    #[test]
    fn brand_empty_enum_is_malformed() {
        let r = parse_one("shape: []\n", "/v/bad.type.yaml");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == codes::MALFORMED_BRAND_SHAPE));
    }

    #[test]
    fn brand_mapping_value_is_malformed() {
        let r = parse_one("shape:\n  a: 1\n", "/v/bad.type.yaml");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == codes::MALFORMED_BRAND_SHAPE));
    }

    #[test]
    fn brand_bad_expression_is_shape_syntax_error() {
        // A malformed shape EXPRESSION (string form) surfaces as the grammar's
        // shape-syntax-error, not malformed-brand-shape.
        let r = parse_one("shape: <a |\n", "/v/bad.type.yaml");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == au_grammar::SHAPE_SYNTAX_ERROR));
    }

    #[test]
    fn unknown_top_level_key_message_lists_shape() {
        let r = parse_one("mystery: 1\nfields:\n  a: String\n", "/v/x.type.yaml");
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == codes::UNKNOWN_TOP_LEVEL_KEY && d.message.contains("shape")));
    }

    #[test]
    fn type_name_strips_known_suffixes() {
        assert_eq!(
            type_name_from_path(Path::new("/v/decision.type.yaml")),
            Some(TypeName("decision".into()))
        );
        assert_eq!(
            type_name_from_path(Path::new("/v/type/decision.yaml")),
            Some(TypeName("decision".into()))
        );
        assert_eq!(
            type_name_from_path(Path::new("/v/type/decision.decided.yaml")),
            Some(TypeName("decision.decided".into()))
        );
    }

    #[test]
    fn type_name_is_basename_only_regardless_of_nesting_depth() {
        // Subfolders under `type/` are organizational — the derived name is
        // the basename, not the path.
        assert_eq!(
            type_name_from_path(Path::new("/v/type/meta/display-meta.type.yaml")),
            Some(TypeName("display-meta".into()))
        );
        assert_eq!(
            type_name_from_path(Path::new("/v/type/a/b/c/d/decision.pending.yaml")),
            Some(TypeName("decision.pending".into()))
        );
    }

    #[test]
    fn empty_type_def_parses_to_tag() {
        let res = parse_one("fields: {}\n", "/v/note.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.name.as_str(), "note");
        assert!(td.parents.is_empty());
        assert!(td.fields.is_empty());
        assert!(td.sealed.is_empty());
        // No `meta:` key written → None (distinct from `meta: []` suppression).
        assert!(td.meta_blocks.is_none());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn unknown_top_level_key_emits_warning() {
        // Likely a typo (`feilds:` instead of `fields:`); pre-fix this was
        // silently ignored.
        let res = parse_one("feilds: []\n", "/v/note.type.yaml");
        let warns: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "unknown-top-level-key")
            .collect();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].severity, Severity::Warning);
        assert!(warns[0].message.contains("'feilds'"));
        assert!(warns[0].message.contains("extends, fields, sealed"));
        assert!(warns[0].message.contains("abstract"));
    }

    #[test]
    fn abstract_true_marks_declared_abstract() {
        let res = parse_one("abstract: true\nfields: {}\n", "/v/pane.type.yaml");
        let td = res.type_def.unwrap();
        assert!(td.declared_abstract);
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn abstract_false_and_absent_are_concrete() {
        let explicit = parse_one("abstract: false\nfields: {}\n", "/v/pane.type.yaml");
        assert!(!explicit.type_def.unwrap().declared_abstract);
        assert!(explicit.diagnostics.is_empty());

        // Absent key defaults to concrete, no diagnostic.
        let absent = parse_one("fields: {}\n", "/v/pane.type.yaml");
        assert!(!absent.type_def.unwrap().declared_abstract);
    }

    #[test]
    fn abstract_non_boolean_is_diagnosed() {
        // A non-boolean value fires `abstract-marker-bad-shape`; the type-def
        // still parses (defaulting to concrete) so the rest of the file loads.
        let res = parse_one("abstract: maybe\nfields: {}\n", "/v/pane.type.yaml");
        let td = res.type_def.unwrap();
        assert!(!td.declared_abstract);
        let errs: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "abstract-marker-bad-shape")
            .collect();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].severity, Severity::Error);
    }

    #[test]
    fn required_meta_single_name_parses() {
        let res = parse_one("meta:\n  - required: tool-presentation\n", "/v/t.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.required_meta.len(), 1);
        assert_eq!(td.required_meta[0].name.as_str(), "tool-presentation");
        assert!(td.required_meta[0].repo.is_none());
        // A required-only meta declares no value blocks, so meta_blocks stays
        // None and ancestor surfacing still flows.
        assert!(td.meta_blocks.is_none());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn meta_block_head_and_field_docs_are_captured() {
        let src = "\
meta:
  #: how this note is displayed
  - type: display-meta
    color: red        #: the accent color
    icon: star
  - type: runtime-meta
    ttl: 30
";
        let res = parse_one(src, "/v/t.type.yaml");
        let blocks = res.type_def.unwrap().meta_blocks.unwrap();
        assert_eq!(blocks.len(), 2);
        // Head doc binds to the first block; a trailing `#:` binds to its field.
        assert_eq!(blocks[0].doc.as_deref(), Some("how this note is displayed"));
        assert_eq!(
            blocks[0].field_docs.get("color").map(String::as_str),
            Some("the accent color")
        );
        assert!(!blocks[0].field_docs.contains_key("icon"));
        // The second block gets no bleed from the first block's fields.
        assert!(blocks[1].doc.is_none());
        assert!(blocks[1].field_docs.is_empty());
    }

    #[test]
    fn docstring_wikilinks_are_captured_as_tagged_nav_links() {
        use crate::instance::DocOrigin;
        let src = "\
#: see [[design note]]
fields:
  quote: String   #: cites [[the source]]
";
        let res = parse_one(src, "/v/region.type.yaml");
        // Head-doc link, tagged to the head.
        let head = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Head)
            .expect("head doc link");
        assert_eq!(head.link.raw, "design note");
        // Span is absolute and covers the link including its brackets.
        assert_eq!(
            &src[head.link.span.start..head.link.span.end],
            "[[design note]]"
        );
        // Field-doc link, tagged to the field it documents.
        let field = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Field("quote".into()))
            .expect("field doc link");
        assert_eq!(field.link.raw, "the source");
        assert_eq!(
            &src[field.link.span.start..field.link.span.end],
            "[[the source]]"
        );
        // Exactly the two links, nothing spurious.
        assert_eq!(res.doc_links.len(), 2);
    }

    #[test]
    fn escaped_docstring_wikilink_forms_no_link() {
        // A plain `#` comment is not a docstring, and an escaped `\[[` in a
        // docstring is documentation of the syntax, not an edge.
        let src = "\
fields:
  x: String   #: literal \\[[not a link]]
  y: String   # [[not a doc comment]]
";
        let res = parse_one(src, "/v/e.type.yaml");
        assert!(res.doc_links.is_empty());
    }

    #[test]
    fn docstring_without_wikilinks_captures_no_links() {
        let res = parse_one("#: a plain doc\nfields:\n  x: String\n", "/v/p.type.yaml");
        assert_eq!(res.type_def.unwrap().doc.as_deref(), Some("a plain doc"));
        assert!(res.doc_links.is_empty());
    }

    #[test]
    fn meta_block_docstring_wikilink_is_captured() {
        use crate::instance::DocOrigin;
        // A `#:` on a meta-block field forms a nav-link, captured through the
        // separate meta-doc pass, tagged by that field's key.
        let src = "\
meta:
  - type: display-meta
    color: red        #: accent, see [[palette]]
";
        let res = parse_one(src, "/v/t.type.yaml");
        let color = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Field("color".into()))
            .expect("meta field doc link");
        assert_eq!(color.link.raw, "palette");
        assert_eq!(
            &src[color.link.span.start..color.link.span.end],
            "[[palette]]"
        );
    }

    #[test]
    fn required_meta_list_and_repo_qualifier_parse() {
        let res = parse_one(
            "meta:\n  - required:\n      - a-meta\n      - b-meta::sdk\n",
            "/v/t.type.yaml",
        );
        let td = res.type_def.unwrap();
        assert_eq!(td.required_meta.len(), 2);
        assert_eq!(td.required_meta[0].name.as_str(), "a-meta");
        assert_eq!(td.required_meta[1].name.as_str(), "b-meta");
        assert_eq!(td.required_meta[1].repo.as_deref(), Some("sdk"));
    }

    #[test]
    fn required_meta_coexists_with_a_value_block() {
        let res = parse_one(
            "meta:\n  - required: p-meta\n  - type: display-meta\n    tldr: hi\n",
            "/v/t.type.yaml",
        );
        let td = res.type_def.unwrap();
        assert_eq!(td.required_meta.len(), 1);
        assert_eq!(td.required_meta[0].name.as_str(), "p-meta");
        // The value block still lands in meta_blocks.
        let blocks = td.meta_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].type_name.as_str(), "display-meta");
    }

    #[test]
    fn required_meta_bad_shape_is_diagnosed() {
        let res = parse_one("meta:\n  - required: 42\n", "/v/t.type.yaml");
        let td = res.type_def.unwrap();
        assert!(td.required_meta.is_empty());
        let errs: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "required-meta-bad-shape")
            .collect();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].severity, Severity::Error);
    }

    #[test]
    fn abstract_is_a_recognized_key_not_unknown() {
        let res = parse_one("abstract: true\nfields: {}\n", "/v/pane.type.yaml");
        assert!(res
            .diagnostics
            .iter()
            .all(|d| d.code.as_str() != "unknown-top-level-key"));
    }

    #[test]
    fn location_block_parses_onto_typedef() {
        let src = "\
fields:
  slug: String
location:
  name: \"${.type} - ${.slug}\"
  path: \"**/plan/\"
  fileType: md
  strict: true
";
        let res = parse_one(src, "/v/plan.type.yaml");
        let loc = res.type_def.unwrap().location.expect("location parsed");
        assert!(loc.name.is_some());
        assert!(loc.path.is_some());
        assert_eq!(loc.file_type, Some(crate::location::FileType::Md));
        assert!(loc.strict);
        assert!(res
            .diagnostics
            .iter()
            .all(|d| d.code.as_str() != "location-bad-shape"));
        // recognized key, never dropped as unknown
        assert!(res
            .diagnostics
            .iter()
            .all(|d| d.code.as_str() != "unknown-top-level-key"));
    }

    #[test]
    fn location_malformed_sub_values_each_emit_bad_shape() {
        // unknown sub-key, bad fileType, unterminated name template
        let src = "\
fields: {}
location:
  fileType: txt
  bogus: 1
  name: \"${.slug\"
";
        let res = parse_one(src, "/v/x.type.yaml");
        let n = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "location-bad-shape")
            .count();
        assert_eq!(n, 3);
    }

    #[test]
    fn location_not_a_mapping_is_bad_shape() {
        let res = parse_one("fields: {}\nlocation: \"nope\"\n", "/v/x.type.yaml");
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "location-bad-shape"));
        assert!(res.type_def.unwrap().location.is_none());
    }

    #[test]
    fn non_string_top_level_key_on_type_def_emits_warning() {
        let src = "\
fields: {}
1: numeric-key
";
        let res = parse_one(src, "/v/note.type.yaml");
        let warns: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "mapping-key-not-a-string")
            .collect();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].severity, Severity::Warning);
    }

    #[test]
    fn list_parent_claim_extracts_parents() {
        let res = parse_one("extends: [a, b]\n", "/v/c.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.parents.len(), 2);
        assert_eq!(td.parents[0].name.as_str(), "a");
        assert_eq!(td.parents[1].name.as_str(), "b");
        assert_eq!(td.parent_claim.unwrap().form, ParentClaimForm::List);
    }

    #[test]
    fn empty_parent_list_is_diagnosed() {
        let res = parse_one("extends: []\n", "/v/c.type.yaml");
        // Type-def still parses (empty parents = no parents) but the
        // bad-shape diagnostic surfaces the authoring error.
        let td = res.type_def.unwrap();
        assert!(td.parents.is_empty());
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "parent-claim-bad-shape");
        assert!(res.diagnostics[0].message.contains("cannot be empty"));
    }

    #[test]
    fn bare_parent_claim_is_recorded_as_bare() {
        let res = parse_one("extends: a\n", "/v/c.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.parents.len(), 1);
        assert_eq!(td.parent_claim.unwrap().form, ParentClaimForm::BareName);
        // No diagnostic at this layer — bare-name is structurally valid.
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn type_key_on_type_def_is_diagnosed_not_dropped() {
        // A stray `type:` on a type-def (the identity key, misused for the
        // parent claim) fires a targeted error, never the silent-dropping
        // unknown-top-level-key. See [[type-def extends::au-type-system]].
        let res = parse_one("type: base\nfields: {}\n", "/v/c.type.yaml");
        let errs: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "type-key-on-type-def")
            .collect();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].severity, Severity::Error);
        assert!(errs[0].message.contains("extends:"));
        // Not treated as a parent, and never dropped as unknown-top-level-key.
        let td = res.type_def.unwrap();
        assert!(td.parents.is_empty());
        assert!(res
            .diagnostics
            .iter()
            .all(|d| d.code.as_str() != "unknown-top-level-key"));
    }

    // ----- fields as a map -----

    #[test]
    fn map_form_extracts_names_and_optional_marker() {
        let src = "fields:\n  description: String\n  decided_by?: person*\n";
        let res = parse_one(src, "/v/decision.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.fields.len(), 2);
        assert_eq!(td.fields[0].name.as_str(), "description");
        assert!(!td.fields[0].optional);
        assert_eq!(td.fields[0].raw_shape, "String");
        assert_eq!(td.fields[1].name.as_str(), "decided_by");
        assert!(td.fields[1].optional);
        assert_eq!(td.fields[1].raw_shape, "person*");
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn map_form_preserves_declaration_order() {
        // Field order is declaration order, so the mapping surface must be
        // insertion-ordered. Guards against a hash surface scrambling it.
        let src = "fields:\n  a: String\n  b: Number\n  c: Boolean\n  d: Date\n";
        let td = parse_one(src, "/v/x.type.yaml").type_def.unwrap();
        let names: Vec<&str> = td.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }

    #[test]
    fn map_form_empty_map_is_a_tag() {
        let res = parse_one("fields: {}\n", "/v/note.type.yaml");
        let td = res.type_def.unwrap();
        assert!(td.fields.is_empty());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn map_form_flow_enum_and_quoted_suffixed_enum() {
        let src = "fields:\n  level: [low, high]\n  levels: \"[low, high][]\"\n";
        let td = parse_one(src, "/v/x.type.yaml").type_def.unwrap();
        assert_eq!(td.fields[0].raw_shape, "[low, high]");
        assert_eq!(td.fields[1].raw_shape, "[low, high][]");
        assert!(td.fields[0].parsed_shape.is_ok());
        assert!(td.fields[1].parsed_shape.is_ok());
    }

    #[test]
    fn map_form_trailing_docstring_attaches_to_field() {
        let src = "fields:\n  page: Number   #: the page number\n";
        let td = parse_one(src, "/v/x.type.yaml").type_def.unwrap();
        assert_eq!(td.fields[0].doc.as_deref(), Some("the page number"));
    }

    #[test]
    fn map_form_leading_and_trailing_docstrings_bind_by_position() {
        let src = "fields:\n  #: lead\n  page: Number   #: trail\n  #: ctx\n  prefix?: String\n";
        let td = parse_one(src, "/v/x.type.yaml").type_def.unwrap();
        assert_eq!(td.fields[0].doc.as_deref(), Some("lead\ntrail"));
        assert_eq!(td.fields[1].doc.as_deref(), Some("ctx"));
    }

    #[test]
    fn map_form_head_doc_is_the_typedef_doc() {
        let src = "#: a region in a PDF\nfields:\n  page: Number\n";
        let td = parse_one(src, "/v/region.type.yaml").type_def.unwrap();
        assert_eq!(td.doc.as_deref(), Some("a region in a PDF"));
        assert!(td.fields[0].doc.is_none());
    }

    #[test]
    fn fields_scalar_is_not_a_map() {
        let res = parse_one("fields: 3\n", "/v/x.type.yaml");
        let d: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "fields-not-a-map")
            .collect();
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("must be a map"));
    }

    #[test]
    fn fields_sequence_is_rejected_as_not_a_map() {
        // The retired list form errors with a migration fix.
        let res = parse_one("fields:\n  - x: String\n", "/v/x.type.yaml");
        let d: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "fields-not-a-map")
            .collect();
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("not a list"));
        assert!(d[0]
            .fix
            .as_ref()
            .unwrap()
            .description
            .contains("name: shape"));
    }

    #[test]
    fn duplicate_field_name_is_a_warning() {
        let src = "fields:\n  a: String\n  a: Number\n";
        let res = parse_one(src, "/v/x.type.yaml");
        // saphyr collapses to one field (last wins), so the closure sees one.
        assert_eq!(res.type_def.as_ref().unwrap().fields.len(), 1);
        let d: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "duplicate-field")
            .collect();
        assert_eq!(d.len(), 1);
        // Advisory, recoverable last-wins, never aborts the def.
        assert_eq!(d[0].severity, Severity::Warning);
        assert!(d[0].message.contains("'a'"));
        assert!(!d[0].related.is_empty(), "points at the first declaration");
        // The collapse means no spurious mixin-collision from the closure.
        assert!(!res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "mixin-collision"));
    }

    #[test]
    fn duplicate_field_errors_even_with_identical_shape() {
        let src = "fields:\n  a: String\n  a: String\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let n = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "duplicate-field")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_single_field_map_has_no_duplicate_field() {
        let res = parse_one("fields:\n  a: String\n  b: Number\n", "/v/x.type.yaml");
        assert!(!res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "duplicate-field"));
    }

    #[test]
    fn duplicate_key_in_a_field_inline_record_is_not_a_duplicate_field() {
        // A dup key inside a field's inline flow-record shape is a record key,
        // not a field, so it must not fire `duplicate-field`. (The shape is also
        // invalid, but that is a separate concern.) Guards the direct-field-key
        // scoping against the byte-span-containment false positive.
        let src = "fields:\n  bar: {x: String, x: Number}\n";
        let res = parse_one(src, "/v/x.type.yaml");
        assert!(
            !res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "duplicate-field"),
            "nested record key must not fire duplicate-field; got {:?}",
            res.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    // ----- docstrings -----

    #[test]
    fn leading_docstring_block_of_multiple_lines_joins_for_the_next_field() {
        // The multi-line leading-block join, distinct from the single-line
        // leading case in `map_form_leading_and_trailing_docstrings_bind_by_position`.
        let src = "fields:\n  #: first line\n  #: second line\n  page: Number\n";
        let td = parse_one(src, "/v/x.type.yaml").type_def.unwrap();
        assert_eq!(td.fields[0].doc.as_deref(), Some("first line\nsecond line"));
    }

    #[test]
    fn hash_inside_quoted_value_is_not_a_docstring() {
        // The string-lexing edge: `#:` inside a quoted scalar is content, not a
        // comment, so it neither documents nor dangles.
        let src = "type: \"base #: still a parent name\"\nfields:\n  q: String\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.as_ref().unwrap();
        assert!(td.doc.is_none());
        assert!(td.fields[0].doc.is_none());
        assert!(
            !res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "dangling-doc-comment"),
            "a `#:` inside a quoted value must not be a docstring; got {:?}",
            res.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hash_inside_meta_block_scalar_is_not_a_docstring() {
        // A `#:`-looking line inside a `meta:` block scalar is prose, not a doc
        // comment. The side-pass skips the `meta:` value range, so it neither
        // mis-attaches to the following field nor dangles. See [[type docstring::au-type-system]].
        let src = "type: base\nmeta:\n  - type: descr-meta\n    text: |\n      first line\n      #: block-scalar prose, not a doc\nfields:\n  q: String\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.as_ref().unwrap();
        assert!(
            td.fields[0].doc.is_none(),
            "block-scalar `#:` must not become a field doc, got {:?}",
            td.fields[0].doc
        );
        assert!(td.doc.is_none());
        assert!(
            !res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "dangling-doc-comment"),
            "a `#:` inside a meta block scalar must not dangle; got {:?}",
            res.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn plain_hash_comment_is_ignored() {
        let src = "# just a header\nfields:\n  q: String   # trailing plain\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.as_ref().unwrap();
        assert!(td.doc.is_none());
        assert!(td.fields[0].doc.is_none());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn docstring_binding_to_no_declaration_warns() {
        // Own-line `#:` after the last field, before another key, binds to no
        // field declaration.
        let src = "fields:\n  q: String\n  #: dangles\nsealed:\n  - x.y\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let diags: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "dangling-doc-comment")
            .collect();
        assert_eq!(diags.len(), 1, "got {:?}", res.diagnostics);
        assert_eq!(diags[0].severity, Severity::Warning);
    }

    #[test]
    fn sealed_list_extracts_entries() {
        let res = parse_one("sealed: [a.b, a.c]\n", "/v/a.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.sealed.len(), 2);
        assert_eq!(td.sealed[0].name.as_str(), "a.b");
    }

    #[test]
    fn meta_blocks_record_type_name() {
        let src =
            "meta:\n  - type: display-meta\n    icon: x\n  - type: runtime-meta\n    version: 1\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let blocks = td.meta_blocks.as_ref().expect("`meta:` declared → Some");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].type_name.as_str(), "display-meta");
        assert_eq!(blocks[1].type_name.as_str(), "runtime-meta");
    }

    #[test]
    fn meta_block_without_type_emits_diagnostic() {
        let res = parse_one("meta:\n  - icon: x\n", "/v/x.type.yaml");
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "meta-block-bad-shape"));
    }

    #[test]
    fn meta_empty_list_is_suppression_marker() {
        // Spec [[type-def meta::au-type-system]]: `meta: []` is a stop signal for the canonical lookup
        // walk — DISTINCT from absent `meta:`. Both shapes must be
        // representable so the walk helper can branch on which one fired.
        let res = parse_one("meta: []\n", "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(td.meta_blocks, Some(vec![]));
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn meta_block_body_fields_captured() {
        let src = "meta:\n  - type: display-meta\n    tldr: \"abc\"\n    icon: x\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let blocks = td.meta_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.type_name.as_str(), "display-meta");
        // `type:` is the discriminator — it is NOT carried in body fields.
        // Remaining keys land as InstanceField entries in source order.
        let keys: Vec<&str> = block.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["tldr", "icon"]);
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn meta_block_with_type_only_has_empty_body() {
        // Spec [[type-def meta::au-type-system]]: an empty sub-region body (`- type: x` with no further
        // keys) is just an empty declaration of that named type-def — NOT
        // suppression. Body validation will require all of x's
        // fields to be optional; parsing succeeds either way.
        let res = parse_one("meta:\n  - type: display-meta\n", "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let blocks = td.meta_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].type_name.as_str(), "display-meta");
        assert!(blocks[0].fields.is_empty());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn meta_block_type_list_emits_meta_mixin_not_supported() {
        // Spec [[type-def meta::au-type-system]]: a meta sub-region's `type:` is single-name only —
        // mixin (`type: [a, b]`) is forbidden. Distinct code from
        // meta-block-bad-shape so the rule is greppable on its own.
        let res = parse_one(
            "meta:\n  - type: [display-meta, runtime-meta]\n",
            "/v/x.type.yaml",
        );
        let mixin_diags: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "meta-mixin-not-supported")
            .collect();
        assert_eq!(mixin_diags.len(), 1);
        assert_eq!(mixin_diags[0].severity, Severity::Error);
        // The diagnostic message names BOTH list edge cases (mixin and
        // empty) so the corrective action is unambiguous.
        assert!(mixin_diags[0].message.contains("`type: [a, b]`"));
        assert!(mixin_diags[0].message.contains("`type: []`"));
        // The block is rejected and not stored. With no sub-region
        // successfully parsed, `meta_blocks` stays `None` — the type-def
        // is effectively as if `meta:` was absent, so `lookup_meta` walks
        // ancestors instead of treating a bad-author list as suppression.
        let td = res.type_def.unwrap();
        assert_eq!(td.meta_blocks, None);
    }

    #[test]
    fn meta_block_empty_type_list_emits_meta_mixin_not_supported() {
        // Spec [[type-def meta::au-type-system]] sequence-arm edge case: `type: []` (empty list) is
        // structurally a YAML sequence at the `type:` position — fires the
        // same code as mixin form. The shared diagnostic message names
        // both forms so users authoring either edge case see the rule.
        let res = parse_one("meta:\n  - type: []\n", "/v/x.type.yaml");
        let mixin_diags: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "meta-mixin-not-supported")
            .collect();
        assert_eq!(mixin_diags.len(), 1);
        assert_eq!(mixin_diags[0].severity, Severity::Error);
        // Same reasoning as the sibling test above — the only sub-region
        // failed to parse, so `meta_blocks` is `None`, not the [[type-def meta::au-type-system]]
        // suppression form.
        let td = res.type_def.unwrap();
        assert_eq!(td.meta_blocks, None);
    }

    #[test]
    fn flow_collection_with_trailing_whitespace_keeps_closer_in_raw_shape() {
        // Regression guard pinning saphyr's span-end behavior for flow
        // `[...]` / `{...}` collections: across every whitespace variant
        // we can devise (tight, space-before-close, both sides spaced,
        // tabs, trailing commas, multi-line) `value.span.end` lands ON
        // the closing bracket, so the +1 extension in `parse_field_entry`
        // consistently captures it. If a future saphyr update shifts the
        // marker (e.g. past the bracket, or onto the preceding
        // whitespace), one of these cases will trip raw_shape into an
        // unbalanced form and parse_shape will surface a misleading
        // "unbalanced brackets" diagnostic.
        for (src, closer, want_inner) in [
            ("fields:\n  foo: [a, b]\n", ']', "[a, b]"),
            ("fields:\n  foo: [a , b ]\n", ']', "[a , b ]"),
            ("fields:\n  foo: [ a , b ]\n", ']', "[ a , b ]"),
            ("fields:\n  foo: [a, b\t]\n", ']', "[a, b\t]"),
            ("fields:\n  foo: {a: 1}\n", '}', "{a: 1}"),
            ("fields:\n  foo: {a: 1 }\n", '}', "{a: 1 }"),
            ("fields:\n  foo: { a: 1 }\n", '}', "{ a: 1 }"),
            // Trailing comma + space before close (review's specific
            // failure mode):
            ("fields:\n  foo: [a , ]\n", ']', "[a , ]"),
            ("fields:\n  foo: [a, b , ]\n", ']', "[a, b , ]"),
            // Multi-line flow — newline + indent before close.
            ("fields:\n  foo: [a,\n      b]\n", ']', "[a,\n      b]"),
        ] {
            let res = parse_one(src, "/v/x.type.yaml");
            let td = res.type_def.expect("type-def parses");
            assert_eq!(td.fields.len(), 1, "src={src:?} got {:?}", td.fields);
            let f = &td.fields[0];
            assert!(
                f.raw_shape.ends_with(closer),
                "src={src:?} closer={closer:?} raw_shape={:?}",
                f.raw_shape
            );
            assert_eq!(f.raw_shape, want_inner, "src={src:?} raw_shape mismatch",);
        }
    }

    #[test]
    fn field_name_empty_after_optional_suffix_is_rejected() {
        // `?: String` strips to an empty name. Without validation the
        // empty FieldName flowed into the graph and downstream
        // diagnostics referenced an unnamed field.
        let res = parse_one("fields:\n  '?': String\n", "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        assert!(
            td.fields.is_empty(),
            "expected no fields stored; got {:?}",
            td.fields
        );
        assert!(
            res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "field-decl-bad-shape"),
            "expected field-decl-bad-shape, got {:?}",
            res.diagnostics
                .iter()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn field_name_with_only_question_marks_is_rejected() {
        // `??: String` strips to "?", which fails the leading-letter
        // requirement of the identifier rule.
        let res = parse_one("fields:\n  '??': String\n", "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        assert!(td.fields.is_empty());
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "field-decl-bad-shape"));
    }

    #[test]
    fn field_name_with_invalid_chars_is_rejected() {
        // Space inside the key — fails the identifier rule even though
        // YAML accepts it as a string key.
        let res = parse_one("fields:\n  'foo bar': String\n", "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        assert!(td.fields.is_empty());
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "field-decl-bad-shape"));
    }

    #[test]
    fn meta_with_only_bad_subregions_leaves_meta_blocks_none() {
        // `meta:` written with sub-regions that all fail to parse must NOT
        // collapse to `Some(vec![])` — that's the [[type-def meta::au-type-system]] suppression state
        // (explicit `meta: []`), and conflating the two halts `lookup_meta`
        // and hides ancestor metas because of authoring mistakes. With no
        // sub-regions parsed, the type-def is effectively as if `meta:`
        // was absent; diagnostics already explain why per entry.
        let res = parse_one(
            "meta:\n  - type: [display-meta, runtime-meta]\n",
            "/v/x.type.yaml",
        );
        let td = res.type_def.unwrap();
        assert_eq!(td.meta_blocks, None);
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "meta-mixin-not-supported"));
    }

    #[test]
    fn meta_with_mix_of_good_and_bad_subregions_keeps_the_good() {
        // Partial parse: one bad entry + one good entry. The good block
        // is stored; the bad one fires a diagnostic. `meta_blocks` is
        // `Some(vec![good])`, NOT `None` (the user clearly meant to
        // declare metas, and we got one out of it).
        let src = "meta:\n  - type: [bad, list]\n  - type: display-meta\n    tldr: \"ok\"\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let blocks = td
            .meta_blocks
            .as_ref()
            .expect("partial parse keeps the good block");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].type_name.as_str(), "display-meta");
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "meta-mixin-not-supported"));
    }

    #[test]
    fn meta_block_reserved_key_in_body_is_rejected() {
        // Reserved keys (`fields:` / `sealed:` / `meta:`) inside a meta
        // sub-region body fire reserved-key-on-instance — the same rule that
        // protects inline values. Mirrors the inline-value behavior; the
        // meta body is structurally a sub-instance per [[type-def meta::au-type-system]].
        let src = "meta:\n  - type: display-meta\n    fields:\n      x: String\n";
        let res = parse_one(src, "/v/x.type.yaml");
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "reserved-key-on-instance"));
    }

    #[test]
    fn top_level_not_mapping_is_diagnosed() {
        let res = parse_one("- foo\n", "/v/x.type.yaml");
        assert!(res.type_def.is_none());
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "type-def-not-a-mapping");
    }

    // ----- au-grammar integration -----

    use au_grammar::{Primitive, Shape};

    #[test]
    fn primitive_field_shape_parses_to_ok() {
        let src = "fields:\n  description: String\n";
        let res = parse_one(src, "/v/note.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(
            td.fields[0].parsed_shape,
            Ok(Shape::Primitive(Primitive::String))
        );
    }

    #[test]
    fn all_primitives_parse_to_ok() {
        let src = "\
fields:
  a: String
  b: Number
  c: Boolean
  d: Date
  e: DateTime
";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let kinds: Vec<_> = td
            .fields
            .iter()
            .map(|f| f.parsed_shape.as_ref().unwrap().clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                Shape::Primitive(Primitive::String),
                Shape::Primitive(Primitive::Number),
                Shape::Primitive(Primitive::Boolean),
                Shape::Primitive(Primitive::Date),
                Shape::Primitive(Primitive::DateTime),
            ]
        );
    }

    #[test]
    fn enum_shape_lands_as_ok_on_field_decl() {
        // [[type-def shape enum::au-type-system]] inline closed enum. The field parse wires
        // au-grammar's parse through `parse_field_entry`; verify the enum shape
        // carries through with no load-time diagnostic.
        let src = "fields:\n  priority: [low, moderate, high]\n";
        let res = parse_one(src, "/v/task.type.yaml");
        let td = res.type_def.unwrap();
        assert_eq!(
            td.fields[0].parsed_shape,
            Ok(Shape::Enum(vec![
                "low".into(),
                "moderate".into(),
                "high".into()
            ]))
        );
        assert!(
            res.diagnostics.is_empty(),
            "enum field shouldn't produce load-time diagnostics, got {:?}",
            res.diagnostics
        );
    }

    #[test]
    fn record_slot_shape_parses_as_record() {
        // Bare-name record slots (spec [[type-def shape record::au-type-system]]) parse to `Shape::Record`
        // — the inline-value entry point ([[type-def shape record::au-type-system]] case 1). au-core
        // verifies the name exists in the type graph at load time.
        let src = "fields:\n  rationale: rationale\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();

        let shape = td.fields[0].parsed_shape.as_ref().unwrap();
        assert_eq!(shape, &au_grammar::Shape::Record("rationale".into()));
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn malformed_shape_is_stored_as_err_with_syntax_code() {
        let src = "fields:\n  x: '@bad'\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let err = td.fields[0].parsed_shape.as_ref().unwrap_err();
        assert_eq!(err.code.as_str(), "shape-syntax-error");
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn shape_err_span_falls_inside_source() {
        // The shape diagnostic's byte-range must point at the actual shape
        // text, not the whole field entry. This guards against a regression
        // where `entry_span` leaks into the shape diagnostic.
        let src = "fields:\n  foo: '@bad'\n";
        let res = parse_one(src, "/v/x.type.yaml");
        let td = res.type_def.unwrap();
        let f = &td.fields[0];
        let err = f.parsed_shape.as_ref().unwrap_err();
        let range = err.span.range;
        // Span covers the shape text only — for quoted YAML the saphyr
        // span includes the surrounding quotes.
        assert!(&src[range.start..range.end].contains("@bad"));
        assert_eq!(range, f.shape_span);
    }
}
