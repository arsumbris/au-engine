//! Instance file AST and parser.
//!
//! Mirrors `typedef.rs` for instance files: structural extraction only.
//! Validation against the type graph (closure walk, required-field check,
//! shape conformance) lives in `validate`.
//!
//! `fields:` / `sealed:` / `meta:` at instance top level →
//! `reserved-key-on-instance` ([[type-def::au-type-system]]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};
use au_parser::yaml::{span_to_byte_range, MarkedYaml, Scalar, YamlData};

use crate::codes;
use crate::typedef::TypeNameClaim;

/// Instance `type:` form. Bare scalar is the [[type list form::au-type-system]] single-claim form;
/// `List` covers proper mixin and the trivial 1-element list form
/// (still single-claim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeClaim {
    Bare(TypeNameClaim),
    List {
        items: Vec<TypeNameClaim>,
        value_span: ByteRange,
    },
}

impl TypeClaim {
    /// Number of named types claimed. `Bare` is always 1; `List` is the list
    /// length (which may be 0, 1, or many).
    pub fn len(&self) -> usize {
        match self {
            TypeClaim::Bare(_) => 1,
            TypeClaim::List { items, .. } => items.len(),
        }
    }

    /// True when the claim names exactly one type (bare scalar, or a
    /// 1-element list). Multi-claim mixin (N≥2 list) is handled by
    /// `closure::effective_shape` directly; this helper is mainly used
    /// where bare/1-element handling differs structurally.
    pub fn is_single(&self) -> bool {
        self.len() == 1
    }

    /// The byte span of the claim value: the bare name's span, or the whole
    /// list's value span. Used to anchor a field insert on a record whose only
    /// authored line is its `type:` claim.
    pub fn span(&self) -> ByteRange {
        match self {
            TypeClaim::Bare(c) => c.span,
            TypeClaim::List { value_span, .. } => *value_span,
        }
    }

    /// Iterate over the claimed type names.
    pub fn iter(&self) -> impl Iterator<Item = &TypeNameClaim> {
        let slice: &[TypeNameClaim] = match self {
            TypeClaim::Bare(c) => std::slice::from_ref(c),
            TypeClaim::List { items, .. } => items.as_slice(),
        };
        slice.iter()
    }
}

/// An instance-field value. `Mapping` carries an inline value ([[type-def shape record::au-type-system]]) — a
/// YAML map mirroring file frontmatter (optional `type:` claim plus
/// arbitrary fields), parsed eagerly so the validator can descend.
///
/// `Float(f64)` makes this `PartialEq` but not `Eq`: a `NaN` value never
/// compares equal to itself. Any equality-based dedup or caching of values
/// (e.g. [[type value container::au-type-system]] collapsing equal contributions) treats two
/// `NaN`s as distinct. Fine in practice — authored data does not carry `NaN` —
/// but a reason this enum stays `PartialEq` only.
#[derive(Debug, Clone, PartialEq)]
pub enum InstanceValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Null,
    /// YAML sequence — each element carries its own value-span so the
    /// validator can point list-element diagnostics at the offending entry
    /// instead of the surrounding list.
    Sequence(Vec<SequenceElement>),
    /// Inline value at a record-typed slot ([[type-def shape record::au-type-system]]): a YAML map with optional
    /// `type:` claim plus fields. Same shape as a top-level instance's
    /// frontmatter — nested inline values fall out of the recursive
    /// `classify_value` walk.
    Mapping(InlineValue),
    NotYetSupported,
}

/// One entry in a `Sequence`. Span covers the YAML node for that element.
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceElement {
    pub value: InstanceValue,
    pub span: ByteRange,
    /// Navigational wikilinks embedded in this element when it is a string,
    /// see [`NavLink`]. Empty for non-string elements.
    pub nav_links: Vec<NavLink>,
}

/// A navigational `[[...]]` wikilink embedded in a string field value.
///
/// `span` is file-relative and covers the link including its brackets;
/// `raw` is the inner text, parsed by the consumer via
/// `parse_wikilink_inner`. Extracted at parse time because the IR is
/// otherwise source-free: the raw scalar token (quotes, escapes) can't be
/// reconstructed downstream, so link offsets must be fixed while the source
/// is in hand. A link is navigational, not a validated reference, see
/// [[type reference::au-type-system]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavLink {
    pub span: ByteRange,
    pub raw: String,
}

/// The declaration a `#:` docstring binds to. The origin tag on a docstring
/// navigational edge, so a consumer tells a documentation reference from an
/// ordinary prose or value mention. See
/// [[spec - docstring navigational links - a docstring's wikilinks resolve as navigational edges tagged to their declaration]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocOrigin {
    /// A head docstring: the type-def or instance head, a nested record's head,
    /// or a meta block's head. Scope-local; the link's span pins which one.
    Head,
    /// A field docstring, named by its key. A type-def or instance field, a
    /// nested-record field, or a meta-block field.
    Field(String),
}

/// A navigational `[[...]]` wikilink captured from a `#:` docstring, tagged
/// with the declaration it documents. Navigational only, never a validated or
/// contributing reference. Derived from the docstring text, so it stays
/// advisory and out of the closure hash. See [[type docstring::au-type-system]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocstringLink {
    pub origin: DocOrigin,
    pub link: NavLink,
}

/// Scan a source byte range for navigational `[[...]]` wikilinks, each returned
/// with a file-relative span. The `\[[` escape is honored by the parser scan,
/// so documentation that shows the wikilink syntax forms no edge. Used for
/// docstring links, whose text is a `#:` comment, not a YAML scalar.
pub(crate) fn nav_links_in_range(source: &str, range: ByteRange) -> Vec<NavLink> {
    let start = range.start.min(source.len());
    let end = range.end.min(source.len());
    if start >= end {
        return Vec::new();
    }
    au_parser::scan_wikilink_spans(&source[start..end], &[])
        .into_iter()
        .map(|(raw, span)| NavLink {
            span: ByteRange::new(start + span.start, start + span.end),
            raw: raw.to_string(),
        })
        .collect()
}

/// An inline value at a record-typed slot ([[type-def shape record::au-type-system]]). Mirrors a file-level
/// `Instance`'s frontmatter shape: optional `type:` claim plus arbitrary
/// fields. Reserved keys (`fields:`, `sealed:`, `meta:`) inside an inline
/// value emit `reserved-key-on-instance` at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineValue {
    pub type_claim: Option<TypeClaim>,
    /// The `^:` block-id ([[type block-id::au-type-system]]): identity-layer beside the
    /// `type:` claim, never a field. Makes the record a reference target
    /// (`[[file^id]]` / `[[^id]]`).
    pub block_id: Option<BlockIdDecl>,
    pub fields: Vec<InstanceField>,
    /// The record's own `#:` head docstring: a leading `#:` block before its
    /// first key, `None` when absent. Advisory, surfaced on the value surface,
    /// see [[type docstring::au-type-system]].
    pub doc: Option<String>,
    /// Per-field `#:` docstrings, keyed by the field key as authored (the
    /// qualified form kept verbatim). Only documented fields appear. Advisory.
    pub field_docs: BTreeMap<String, String>,
}

/// A `^:` block-id declaration on an inline record. Spans cover the key
/// and value for duplicate-namespace and resolution diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockIdDecl {
    pub id: String,
    pub key_span: ByteRange,
    pub value_span: ByteRange,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceField {
    /// The YAML key as it appeared on disk. Not necessarily a valid
    /// `FieldName` — may carry a [[type-def fields collision - auto-unify and qualified field::au-type-system]] qualifier
    /// (`field-name{type-name}`); the validator parses the qualifier shape
    /// and constructs a `FieldName` only after resolving it.
    pub key: String,
    pub key_span: ByteRange,
    pub value: InstanceValue,
    pub value_span: ByteRange,
    /// Navigational wikilinks embedded in this field's value when it is a
    /// string, see [`NavLink`]. Empty for non-string values.
    pub nav_links: Vec<NavLink>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub source_path: PathBuf,
    pub source_span: ByteRange,
    pub type_claim: TypeClaim,
    pub fields: Vec<InstanceField>,
    /// The instance's own `#:` head docstring: a leading `#:` block before the
    /// first frontmatter key, `None` when absent. Advisory, see [[type docstring::au-type-system]].
    pub doc: Option<String>,
    /// Per-field `#:` docstrings, keyed by the frontmatter field key as
    /// authored. Only documented fields appear. Advisory.
    pub field_docs: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct InstanceParseResult {
    pub instance: Option<Instance>,
    pub diagnostics: Vec<Diagnostic>,
    /// Navigational `[[...]]` wikilinks captured from the instance's `#:`
    /// docstrings, each tagged with the head or field it documents. Navigational
    /// only, derived from the doc text, so advisory and out of any hash. Held
    /// beside the `Instance` as a derived parse artifact. Covers the frontmatter
    /// head, its fields, and nested records; a body-fence typed block's own
    /// docstrings are not yet captured here. See
    /// [[spec - docstring navigational links - a docstring's wikilinks resolve as navigational edges tagged to their declaration]].
    pub doc_links: Vec<DocstringLink>,
}

/// Parse an instance file's frontmatter into an `Instance`. `yaml_offset` is
/// the byte offset within the source file where the YAML body begins (the
/// frontmatter delimiter offset for markdown files; 0 for pure YAML); spans
/// on the resulting AST are shifted by it.
///
/// Emits `missing-type-claim` when the document has no top-level `type:` key.
/// The CLI's `type validate` flow filters such files out via classification
/// before reaching here, so the diagnostic only fires for direct library
/// callers (LSP, future watcher, tests) — see `codes::MISSING_TYPE_CLAIM`.
pub fn parse_instance(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    doc: &MarkedYaml<'_>,
) -> InstanceParseResult {
    parse_instance_stamped(path, source, yaml_offset, doc, None)
}

/// Parse an instance, optionally STAMPING a `type:` claim the file itself does
/// not write. A written top-level `type:` always wins; the stamp is used only
/// when the file writes no `type:` key. This types a file the engine classifies
/// by kind (an engine-schema file, its `type:` implicit) without a written key,
/// while a future explicit key stays honored. au-core is domain-pure: it stamps
/// whatever claim the caller supplies, it knows no engine-schema names.
///
/// With `stamp: None` this is exactly [`parse_instance`]. With a stamp and no
/// written key, `missing-type-claim` is suppressed and the stamp is the claim.
pub fn parse_instance_stamped(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    doc: &MarkedYaml<'_>,
    stamp: Option<TypeNameClaim>,
) -> InstanceParseResult {
    let mut diagnostics = Vec::new();
    // Navigational links captured from the instance's `#:` docstrings, filled by
    // the `attach_scope` walk below, empty on an early return before it runs.
    let mut doc_links: Vec<DocstringLink> = Vec::new();
    let source_span = span_to_byte_range(source, yaml_offset, doc.span);

    let mapping = match &doc.data {
        YamlData::Mapping(m) => m,
        _ => {
            diagnostics.push(diag(
                codes::INSTANCE_NOT_A_MAPPING,
                Severity::Error,
                path,
                source_span,
                "instance frontmatter must be a YAML mapping at the top level",
            ));
            return InstanceParseResult {
                instance: None,
                diagnostics,
                doc_links,
            };
        }
    };

    let mut type_claim: Option<TypeClaim> = None;
    let mut saw_type_key = false;
    let mut fields: Vec<InstanceField> = Vec::new();
    // The offset of the first frontmatter key, the boundary a leading `#:` head
    // block must sit before. Captured across every key (including `type:`), so a
    // block above the first key documents the instance. See [[type docstring::au-type-system]].
    let mut first_key_offset: Option<usize> = None;

    for (key, value) in mapping.iter() {
        let Some(key_str) = scalar_string(key) else {
            diagnostics.push(diag(
                codes::MAPPING_KEY_NOT_A_STRING,
                Severity::Warning,
                path,
                span_to_byte_range(source, yaml_offset, key.span),
                "instance top-level mapping key must be a string",
            ));
            continue;
        };
        let key_span = span_to_byte_range(source, yaml_offset, key.span);
        let value_span = span_to_byte_range(source, yaml_offset, value.span);
        if first_key_offset.is_none() {
            first_key_offset = Some(key_span.start);
        }

        match key_str.as_str() {
            "type" => {
                saw_type_key = true;
                type_claim = parse_type_claim(path, source, value, yaml_offset, &mut diagnostics);
            }
            "fields" | "sealed" | "abstract" | "meta" | "body" | "location" => {
                diagnostics.push(diag(
                    codes::RESERVED_KEY_ON_INSTANCE,
                    Severity::Error,
                    path,
                    key_span,
                    format!(
                        "`{}:` is a type-def-only key and cannot appear on an instance",
                        key_str
                    ),
                ));
            }
            // `^:` belongs on inline records ([[type block-id::au-type-system]]); the
            // file itself is addressable by name. Warn, drop the key.
            "^" => {
                diagnostics.push(diag(
                    codes::BLOCK_ID_ON_INSTANCE_ROOT,
                    Severity::Warning,
                    path,
                    key_span,
                    "`^:` on the instance root has no effect — the file is addressable by name; block-ids belong on inline records",
                ));
            }
            _ => {
                fields.push(InstanceField {
                    key: key_str.clone(),
                    key_span,
                    value: classify_value(path, source, yaml_offset, value, &mut diagnostics),
                    value_span,
                    nav_links: scalar_nav_links(source, yaml_offset, value),
                });
            }
        }
    }

    // Recover `#:` docstrings over the frontmatter and its nested records. Scan
    // from `yaml_offset`, not `source_span.start`: the mapping span starts at
    // the first key, so a leading head block would otherwise fall outside the
    // region. Extend the upper past the last field's line (the mapping span ends
    // at the value, before a trailing `#:` on the last field), stopping before
    // the next line (a closing `---` or the body). See [[type docstring::au-type-system]].
    let content_end = fields
        .iter()
        .map(|f| f.value_span.end)
        .chain(type_claim.as_ref().map(|c| c.span().end))
        .max()
        .unwrap_or(source_span.end);
    // A pure-yaml doc span reaches EOF (a trailing dangling `#:` is inside it); a
    // markdown mapping span ends at the last value, so the snap extends past a
    // trailing `#:` on the last field. Take the larger to cover both.
    let region_end = source_span.end.max(snap_to_next_line(source, content_end));
    let (doc, field_docs) = attach_scope(
        path,
        source,
        ByteRange::new(yaml_offset, region_end),
        first_key_offset,
        &mut fields,
        &mut diagnostics,
        &mut doc_links,
    );

    // A written `type:` wins; else stamp the caller's claim when the file wrote
    // no key (a malformed written key already erred, so it is not stamped over).
    let type_claim = type_claim.or_else(|| match stamp {
        Some(claim) if !saw_type_key => Some(TypeClaim::Bare(claim)),
        _ => None,
    });
    let Some(type_claim) = type_claim else {
        if !saw_type_key {
            // No written key and no stamp: a genuine missing claim.
            diagnostics.push(diag(
                codes::MISSING_TYPE_CLAIM,
                Severity::Error,
                path,
                source_span,
                "instance is missing required top-level `type:` key",
            ));
        }
        // If the key was seen but malformed, parse_type_claim already pushed
        // an `instance-claim-bad-shape` diagnostic — don't double-fire.
        return InstanceParseResult {
            instance: None,
            diagnostics,
            doc_links,
        };
    };

    InstanceParseResult {
        instance: Some(Instance {
            source_path: path.to_path_buf(),
            source_span,
            type_claim,
            fields,
            doc,
            field_docs,
        }),
        diagnostics,
        doc_links,
    }
}

/// A markdown file with frontmatter but no `type:` claim.
///
/// A note is a first-class graph citizen, it carries frontmatter fields and a
/// body with wikilinks, but it claims no type, so it is not validated. Only its
/// structure is extracted, the same `fields` shape an `Instance` carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub source_path: PathBuf,
    pub source_span: ByteRange,
    pub fields: Vec<InstanceField>,
}

/// Parse a note's frontmatter into its top-level fields.
///
/// No type semantics, no reserved-key checks, a note's frontmatter is plain
/// data. Diagnostics from value classification are discarded, a note is not
/// validated and emits none. A non-mapping frontmatter yields no fields.
pub fn parse_note(path: &Path, source: &str, yaml_offset: usize, doc: &MarkedYaml<'_>) -> Note {
    let source_span = span_to_byte_range(source, yaml_offset, doc.span);
    let mut fields = Vec::new();
    if let YamlData::Mapping(mapping) = &doc.data {
        let mut sink = Vec::new();
        for (key, value) in mapping.iter() {
            let Some(key_str) = scalar_string(key) else {
                continue;
            };
            fields.push(InstanceField {
                key: key_str,
                key_span: span_to_byte_range(source, yaml_offset, key.span),
                value: classify_value(path, source, yaml_offset, value, &mut sink),
                value_span: span_to_byte_range(source, yaml_offset, value.span),
                nav_links: scalar_nav_links(source, yaml_offset, value),
            });
        }
    }
    Note {
        source_path: path.to_path_buf(),
        source_span,
        fields,
    }
}

/// Recover `#:` docstrings for one mapping scope and its nested records.
///
/// Returns `(head_doc, field_docs)` for the scope: the head doc is a leading
/// `#:` block before `first_key_offset`; `field_docs` maps each documented
/// field key to its `#:` text. A `#:` binding to nothing fires
/// `dangling-doc-comment`. Recurses into every nested record so each carries
/// its own docs. See [[type docstring::au-type-system]].
///
/// The caller owns `region`, the byte range this scope's comments live in.
/// A record's leading head block sits BEFORE its value span (between the parent
/// key and the value), so the region must start at the parent key's end, not
/// the value's start. See the module's parse sites for how each region is set.
fn attach_scope(
    path: &Path,
    source: &str,
    region: ByteRange,
    first_key_offset: Option<usize>,
    fields: &mut [InstanceField],
    diagnostics: &mut Vec<Diagnostic>,
    doc_links: &mut Vec<DocstringLink>,
) -> (Option<String>, BTreeMap<String, String>) {
    let result = recover_scope(
        path,
        source,
        region,
        first_key_offset,
        fields,
        diagnostics,
        doc_links,
    );
    for field in fields.iter_mut() {
        let lower = field.key_span.end;
        let upper = field.value_span.end;
        recurse_docs(
            path,
            source,
            &mut field.value,
            lower,
            upper,
            diagnostics,
            doc_links,
        );
    }
    result
}

/// Scan one scope's `region` for `#:` comments and bind them: a leading block
/// before `first_key_offset` is the head doc, a leading block before a field
/// key or a trailing comment on a field's line is that field's doc, anything
/// else dangles. Skips each field's `[key_end .. value_end]` range, so a nested
/// record's own comments and any block-scalar content are left to the recursion
/// and not misread here.
///
/// The returned `field_docs` is keyed by field KEY (the authored string,
/// qualifier-form kept verbatim), so it assumes unique keys within the scope. A
/// duplicate key already fires `duplicate-key-in-mapping`, and its docs would
/// merge under the one entry; the ordered `fields` Vec keeps both values, this
/// map does not.
///
/// A trailing `#:` on a CONTAINER field's own key line (`m:  #: x`, `m` a
/// mapping/sequence) does not bind here: its value range is skipped, so the
/// parent never sees it, and the recursion starts mid-line and dangles it. This
/// mirrors the type-def head rule (a trailing `#:` on `type:` / `fields:`
/// dangles); a container's field doc uses a LEADING block before it, its record
/// head doc a leading block INSIDE it, both of which bind.
pub(crate) fn recover_scope(
    path: &Path,
    source: &str,
    region: ByteRange,
    first_key_offset: Option<usize>,
    fields: &[InstanceField],
    diagnostics: &mut Vec<Diagnostic>,
    doc_links: &mut Vec<DocstringLink>,
) -> (Option<String>, BTreeMap<String, String>) {
    // Skip a field's value range only when a `#:` there could be content rather
    // than a comment: a nested record or sequence (the recursion scans it), or a
    // multi-line value (a block scalar, where a `#:`-looking line is prose). A
    // single-line flow scalar is NOT skipped, so the scanner's own quote-lexing
    // handles a `#` inside a quoted value and still captures a trailing `#:`.
    let bytes = source.as_bytes();
    let skip: Vec<ByteRange> = fields
        .iter()
        .filter_map(|f| {
            let range = ByteRange::new(f.key_span.end, f.value_span.end.min(source.len()));
            let nested = matches!(
                f.value,
                InstanceValue::Mapping(_) | InstanceValue::Sequence(_)
            );
            let multiline = bytes[range.start..range.end].contains(&b'\n');
            (nested || multiline).then_some(range)
        })
        .collect();
    let comments = crate::typedef::scan_doc_comments(source, region.start, region.end, &skip);
    let mut head: Vec<String> = Vec::new();
    let mut field_parts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in &comments {
        let target = if c.own_line {
            if first_key_offset.is_some_and(|fk| c.hash_offset < fk) {
                head.push(c.text.clone());
                for link in nav_links_in_range(source, crate::typedef::doc_content_range(c)) {
                    doc_links.push(DocstringLink {
                        origin: DocOrigin::Head,
                        link,
                    });
                }
                continue;
            }
            // Leading block binds to the nearest following field.
            fields.iter().position(|f| f.key_span.start > c.hash_offset)
        } else {
            // Trailing comment binds to the field on its own line.
            fields
                .iter()
                .position(|f| f.key_span.start >= c.line_start && f.key_span.start < c.hash_offset)
        };
        match target {
            Some(idx) => {
                field_parts
                    .entry(fields[idx].key.clone())
                    .or_default()
                    .push(c.text.clone());
                for link in nav_links_in_range(source, crate::typedef::doc_content_range(c)) {
                    doc_links.push(DocstringLink {
                        origin: DocOrigin::Field(fields[idx].key.clone()),
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
    let field_docs = field_parts
        .into_iter()
        .map(|(k, parts)| (k, parts.join("\n")))
        .collect();
    let doc = (!head.is_empty()).then(|| head.join("\n"));
    (doc, field_docs)
}

/// Recurse the docstring pass into a value's nested records. `lower` is the
/// byte offset just past the enclosing key (a record's head block sits after
/// it), `upper` the value's end.
pub(crate) fn recurse_docs(
    path: &Path,
    source: &str,
    value: &mut InstanceValue,
    lower: usize,
    upper: usize,
    diagnostics: &mut Vec<Diagnostic>,
    doc_links: &mut Vec<DocstringLink>,
) {
    match value {
        InstanceValue::Mapping(inline) => {
            let first_key = inline_first_key(inline);
            let (doc, field_docs) = recover_scope(
                path,
                source,
                ByteRange::new(lower, upper),
                first_key,
                &inline.fields,
                diagnostics,
                doc_links,
            );
            inline.doc = doc;
            inline.field_docs = field_docs;
            for field in inline.fields.iter_mut() {
                let l = field.key_span.end;
                let u = field.value_span.end;
                recurse_docs(path, source, &mut field.value, l, u, diagnostics, doc_links);
            }
        }
        InstanceValue::Sequence(elements) => {
            // A list item has no key of its own, and saphyr's element span is
            // greedy: it runs up to the NEXT element's start, so it swallows the
            // next item's head comment. Bound each item by its own CONTENT end
            // (the max of its inner spans) instead, and start each item past the
            // line holding the previous item's last content, so an inter-item
            // `#:` head binds to the item below it, not the one above.
            let mut prev_end = lower;
            for element in elements.iter_mut() {
                let elem_lower = snap_to_next_line(source, prev_end.saturating_sub(1));
                let content_end = match &element.value {
                    InstanceValue::Mapping(inline) => inline_content_end(inline),
                    _ => element.span.end,
                };
                // Extend the upper to the end of the content's line, so a trailing
                // `#:` on the last field is included, but stop before later lines
                // that hold the next item's head comment.
                let elem_upper = snap_to_next_line(source, content_end);
                recurse_docs(
                    path,
                    source,
                    &mut element.value,
                    elem_lower,
                    elem_upper,
                    diagnostics,
                    doc_links,
                );
                prev_end = content_end;
            }
        }
        _ => {}
    }
}

/// The end of a record's own content: the max end across its `type:` claim,
/// `^:` block-id, and field values. A sequence element's saphyr span is greedy
/// (it reaches the next element), so this is the real upper bound for scanning
/// one list item's docs without swallowing the next item's head comment.
fn inline_content_end(inline: &InlineValue) -> usize {
    let mut end = 0;
    if let Some(claim) = &inline.type_claim {
        end = end.max(claim.span().end);
    }
    if let Some(block_id) = &inline.block_id {
        end = end.max(block_id.value_span.end);
    }
    for field in &inline.fields {
        end = end.max(field.value_span.end);
    }
    end
}

/// The offset of a record's first key, the boundary a leading head block sits
/// before. The `type:` claim carries only its value span, one line-position
/// late at worst, which never crosses a head block that precedes the record.
fn inline_first_key(inline: &InlineValue) -> Option<usize> {
    [
        inline.type_claim.as_ref().map(|c| c.span().start),
        inline.block_id.as_ref().map(|b| b.key_span.start),
        inline.fields.first().map(|f| f.key_span.start),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// Advance past the line containing `offset`: the byte after the next newline,
/// or the source end when none follows.
pub(crate) fn snap_to_next_line(source: &str, offset: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = offset.min(source.len());
    while i < source.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < source.len() {
        i + 1
    } else {
        i
    }
}

fn parse_type_claim(
    path: &Path,
    source: &str,
    value: &MarkedYaml<'_>,
    yaml_offset: usize,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TypeClaim> {
    let value_span = span_to_byte_range(source, yaml_offset, value.span);
    match &value.data {
        YamlData::Value(Scalar::String(s)) => {
            Some(TypeClaim::Bare(TypeNameClaim::parse(s, value_span)))
        }
        YamlData::Sequence(items) => {
            // Empty list claim (`type: []`) is rejected at parse time per
            // [[type list form::au-type-system]] — a list-form claim must name at least one
            // type. Reuses INSTANCE_CLAIM_BAD_SHAPE so callers see a single
            // claim-shape error rather than a downstream surprise.
            if items.is_empty() {
                diagnostics.push(diag(
                    codes::INSTANCE_CLAIM_BAD_SHAPE,
                    Severity::Error,
                    path,
                    value_span,
                    "`type:` list cannot be empty — a list-form claim must name at least one type",
                ));
                return None;
            }
            let mut out = Vec::with_capacity(items.len());
            let mut had_error = false;
            for item in items {
                let item_span = span_to_byte_range(source, yaml_offset, item.span);
                match &item.data {
                    YamlData::Value(Scalar::String(s)) => {
                        out.push(TypeNameClaim::parse(s, item_span))
                    }
                    _ => {
                        had_error = true;
                        diagnostics.push(diag(
                            codes::INSTANCE_CLAIM_BAD_SHAPE,
                            Severity::Error,
                            path,
                            item_span,
                            "`type:` list elements must be type-name strings",
                        ));
                    }
                }
            }
            if had_error {
                None
            } else {
                Some(TypeClaim::List {
                    items: out,
                    value_span,
                })
            }
        }
        _ => {
            diagnostics.push(diag(
                codes::INSTANCE_CLAIM_BAD_SHAPE,
                Severity::Error,
                path,
                value_span,
                "`type:` must be a type-name string or a list of type-name strings",
            ));
            None
        }
    }
}

/// Embedded navigational wikilinks of a scalar value node.
///
/// Empty for any non-string node: a number, a sequence, a mapping. Their
/// links, if any, live on the child holders the recursive parse builds. The
/// span covers the raw scalar token (quotes included), so each `[[...]]`
/// offset lands at its true file position regardless of how the scalar was
/// quoted.
pub(crate) fn scalar_nav_links(
    source: &str,
    yaml_offset: usize,
    node: &MarkedYaml<'_>,
) -> Vec<NavLink> {
    if !matches!(node.data, YamlData::Value(Scalar::String(_))) {
        return Vec::new();
    }
    let span = span_to_byte_range(source, yaml_offset, node.span);
    au_parser::scan_wikilink_spans(&source[span.start..span.end], &[])
        .into_iter()
        .map(|(raw, sub)| NavLink {
            span: ByteRange::new(span.start + sub.start, span.start + sub.end),
            raw: raw.to_string(),
        })
        .collect()
}

pub(crate) fn classify_value(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> InstanceValue {
    match &value.data {
        YamlData::Value(Scalar::String(s)) => InstanceValue::String(s.to_string()),
        YamlData::Value(Scalar::Integer(i)) => InstanceValue::Integer(*i),
        YamlData::Value(Scalar::FloatingPoint(f)) => InstanceValue::Float(f.into_inner()),
        YamlData::Value(Scalar::Boolean(b)) => InstanceValue::Boolean(*b),
        YamlData::Value(Scalar::Null) => InstanceValue::Null,
        YamlData::Sequence(items) => InstanceValue::Sequence(
            items
                .iter()
                .map(|item| SequenceElement {
                    value: classify_value(path, source, yaml_offset, item, diagnostics),
                    span: span_to_byte_range(source, yaml_offset, item.span),
                    nav_links: scalar_nav_links(source, yaml_offset, item),
                })
                .collect(),
        ),
        YamlData::Mapping(_) => InstanceValue::Mapping(parse_inline_value(
            path,
            source,
            yaml_offset,
            value,
            diagnostics,
        )),
        _ => InstanceValue::NotYetSupported,
    }
}

/// Parse an inline-value YAML mapping into an `InlineValue`. Same key
/// classification as `parse_instance`: `type:` populates the optional
/// claim; reserved keys (`fields:`, `sealed:`, `meta:`) emit
/// `reserved-key-on-instance`; other keys become fields. The classifier
/// recurses into nested mappings, so an inline value containing an
/// inline-record field falls out of the same walk.
///
/// Caller (`classify_value`) is responsible for ensuring `node.data` is
/// `YamlData::Mapping` before invoking this helper. The else-arm of the
/// pattern match below is `unreachable!` rather than a defensive empty
/// return — a non-mapping at this entry point would be a caller bug,
/// not a user error.
fn parse_inline_value(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    node: &MarkedYaml<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> InlineValue {
    let YamlData::Mapping(mapping) = &node.data else {
        unreachable!(
            "parse_inline_value invoked on non-mapping YAML node — classify_value's Mapping arm is the only call site"
        );
    };
    let mut type_claim: Option<TypeClaim> = None;
    let mut block_id: Option<BlockIdDecl> = None;
    let mut fields: Vec<InstanceField> = Vec::new();

    for (key, val) in mapping.iter() {
        // Docstrings attach to this record via the top-down `attach_scope` walk,
        // which owns the region boundaries; parse only builds the structure.
        let Some(key_str) = scalar_string(key) else {
            diagnostics.push(diag(
                codes::MAPPING_KEY_NOT_A_STRING,
                Severity::Warning,
                path,
                span_to_byte_range(source, yaml_offset, key.span),
                "inline-value mapping key must be a string",
            ));
            continue;
        };
        let key_span = span_to_byte_range(source, yaml_offset, key.span);
        let value_span = span_to_byte_range(source, yaml_offset, val.span);
        match key_str.as_str() {
            "type" => {
                type_claim = parse_type_claim(path, source, val, yaml_offset, diagnostics);
            }
            // `^:` is identity-layer ([[type block-id::au-type-system]]) — beside `type:`,
            // never a field. Field names can't be `^` (leading letter
            // required), so the key is structurally collision-free.
            "^" => match block_id_scalar(val) {
                Some(id) if au_parser::is_valid_block_id(&id) => {
                    block_id = Some(BlockIdDecl {
                        id,
                        key_span,
                        value_span,
                    });
                }
                Some(bad) => {
                    diagnostics.push(diag(
                        codes::BLOCK_ID_MALFORMED,
                        Severity::Error,
                        path,
                        value_span,
                        format!(
                            "`^:` value '{bad}' is not a valid block-id — ids are [A-Za-z0-9_-]+"
                        ),
                    ));
                }
                None => {
                    diagnostics.push(diag(
                        codes::BLOCK_ID_MALFORMED,
                        Severity::Error,
                        path,
                        value_span,
                        "`^:` value must be a scalar block-id ([A-Za-z0-9_-]+)",
                    ));
                }
            },
            "fields" | "sealed" | "abstract" | "meta" | "location" => {
                diagnostics.push(diag(
                    codes::RESERVED_KEY_ON_INSTANCE,
                    Severity::Error,
                    path,
                    key_span,
                    format!(
                        "`{}:` is a type-def-only key and cannot appear on an inline value",
                        key_str
                    ),
                ));
            }
            _ => {
                fields.push(InstanceField {
                    key: key_str.clone(),
                    key_span,
                    value: classify_value(path, source, yaml_offset, val, diagnostics),
                    value_span,
                    nav_links: scalar_nav_links(source, yaml_offset, val),
                });
            }
        }
    }

    InlineValue {
        type_claim,
        block_id,
        fields,
        // Filled by the top-down `attach_scope` walk, which knows this record's
        // region. Parse leaves them empty.
        doc: None,
        field_docs: BTreeMap::new(),
    }
}

/// Parse a marked fence's body (an inline record) into an [`InlineValue`]
/// with file-absolute spans, plus the navigational `[[...]]` links captured
/// from the record's `#:` docstrings (also file-absolute). `base_offset` is the
/// body's byte offset in the file, so the returned spans index the physical
/// file directly.
///
/// `None` when the body is not a top-level YAML mapping. Diagnostics are not
/// surfaced here: the build validates fence contributions on its own path, so
/// this is a read-side projection of already-checked content.
pub fn parse_block_record(
    path: &Path,
    body: &str,
    base_offset: usize,
) -> Option<(InlineValue, Vec<DocstringLink>)> {
    let parsed = au_parser::yaml::parse(body).ok()?;
    let doc = parsed.first()?;
    if !matches!(&doc.data, YamlData::Mapping(_)) {
        return None;
    }
    // Parse against the body alone (offset 0), then shift every span up to
    // the file. `span_to_byte_range` requires `source[yaml_offset..]` to be
    // the parsed text, so the file offset cannot be the yaml_offset here.
    let mut diagnostics = Vec::new();
    let mut inline = parse_inline_value(path, body, 0, doc, &mut diagnostics);
    // Recover the record's `#:` docstrings over the fence body (offset 0),
    // before the shift; docs are text, unaffected by it. The whole body is this
    // record's scope. A verbatim `String` / `any` fence never reaches here, it
    // is not a mapping, so it carries no docs by construction.
    let first_key = inline_first_key(&inline);
    let mut fence_doc_links = Vec::new();
    let (doc, field_docs) = attach_scope(
        path,
        body,
        ByteRange::new(0, body.len()),
        first_key,
        &mut inline.fields,
        &mut diagnostics,
        &mut fence_doc_links,
    );
    inline.doc = doc;
    inline.field_docs = field_docs;
    shift_inline_value(&mut inline, base_offset);
    // The links were captured over the offset-0 body, so shift their spans up to
    // the file like every other span in the record.
    for dl in fence_doc_links.iter_mut() {
        dl.link.span = shift_range(dl.link.span, base_offset);
    }
    Some((inline, fence_doc_links))
}

fn shift_range(r: ByteRange, delta: usize) -> ByteRange {
    ByteRange::new(r.start + delta, r.end + delta)
}

fn shift_inline_value(inline: &mut InlineValue, delta: usize) {
    if let Some(claim) = inline.type_claim.as_mut() {
        shift_type_claim(claim, delta);
    }
    if let Some(block_id) = inline.block_id.as_mut() {
        block_id.key_span = shift_range(block_id.key_span, delta);
        block_id.value_span = shift_range(block_id.value_span, delta);
    }
    for field in &mut inline.fields {
        field.key_span = shift_range(field.key_span, delta);
        field.value_span = shift_range(field.value_span, delta);
        shift_nav_links(&mut field.nav_links, delta);
        shift_value(&mut field.value, delta);
    }
}

/// Shift every embedded nav-link span. A fence record is parsed at offset 0 then
/// shifted to the file, so its nav-links must move with the rest, else a
/// consumer reading a link's span (the backlink index, a rename rewrite) points
/// at the wrong bytes.
fn shift_nav_links(links: &mut [NavLink], delta: usize) {
    for nl in links {
        nl.span = shift_range(nl.span, delta);
    }
}

fn shift_value(value: &mut InstanceValue, delta: usize) {
    match value {
        InstanceValue::Sequence(elements) => {
            for element in elements {
                element.span = shift_range(element.span, delta);
                shift_nav_links(&mut element.nav_links, delta);
                shift_value(&mut element.value, delta);
            }
        }
        InstanceValue::Mapping(inline) => shift_inline_value(inline, delta),
        _ => {}
    }
}

fn shift_type_claim(claim: &mut TypeClaim, delta: usize) {
    match claim {
        TypeClaim::Bare(name) => name.span = shift_range(name.span, delta),
        TypeClaim::List { items, value_span } => {
            *value_span = shift_range(*value_span, delta);
            for name in items {
                name.span = shift_range(name.span, delta);
            }
        }
    }
}

fn scalar_string(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// `^:` values accept string and integer scalars — a digit-only id
/// (`^: 123`) YAML-types as an integer, and rejecting it would force
/// quoting noise. Anything else is malformed.
fn block_id_scalar(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        YamlData::Value(Scalar::Integer(i)) => Some(i.to_string()),
        _ => None,
    }
}

fn diag(
    code: au_diagnostics::DiagnosticCode,
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

#[cfg(test)]
mod tests {
    use super::*;
    use au_parser::yaml::parse;

    fn parse_one(source: &str, path: &str) -> InstanceParseResult {
        let docs = parse(source).unwrap();
        parse_instance(Path::new(path), source, 0, &docs[0])
    }

    #[test]
    fn instance_docstring_wikilinks_are_captured_as_tagged_nav_links() {
        let src = "\
#: see [[the workflow]]
type: decision
owner: alice          #: signs off, see [[alice profile]]
status: made
";
        let res = parse_one(src, "/v/m.md");
        let head = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Head)
            .expect("head doc link");
        assert_eq!(head.link.raw, "the workflow");
        assert_eq!(
            &src[head.link.span.start..head.link.span.end],
            "[[the workflow]]"
        );
        let owner = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Field("owner".into()))
            .expect("owner field doc link");
        assert_eq!(owner.link.raw, "alice profile");
        assert_eq!(res.doc_links.len(), 2);
    }

    #[test]
    fn nested_record_field_docstring_link_is_captured() {
        // A `#:` on a field inside a nested inline record is captured with an
        // absolute span, tagged by that inner field's key.
        let src = "\
type: workflow
step:
  type: task
  gate: manual        #: reviewer confirms, see [[gate policy]]
";
        let res = parse_one(src, "/v/m.md");
        let gate = res
            .doc_links
            .iter()
            .find(|d| d.origin == DocOrigin::Field("gate".into()))
            .expect("nested gate doc link");
        assert_eq!(gate.link.raw, "gate policy");
        assert_eq!(
            &src[gate.link.span.start..gate.link.span.end],
            "[[gate policy]]"
        );
    }

    #[test]
    fn bare_type_claim_with_primitive_fields() {
        let src = "\
type: decision.decided
description: \"Migrate\"
status: made
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        match &inst.type_claim {
            TypeClaim::Bare(c) => assert_eq!(c.name.as_str(), "decision.decided"),
            _ => panic!("expected bare claim"),
        }
        assert_eq!(inst.fields.len(), 2);
        assert_eq!(inst.fields[0].key, "description");
        assert!(matches!(inst.fields[0].value, InstanceValue::String(ref s) if s == "Migrate"));
        assert_eq!(inst.fields[1].key, "status");
    }

    #[test]
    fn qualified_claim_carries_repo() {
        // `type: foo::other-repo` splits into base + peer qualifier. A type name
        // can never legally contain `:`, so the first `::` is unambiguous.
        let res = parse_one("type: foo::other-repo\n", "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        match &inst.type_claim {
            TypeClaim::Bare(c) => {
                assert_eq!(c.name.as_str(), "foo");
                assert_eq!(c.repo.as_deref(), Some("other-repo"));
                assert!(c.is_qualified());
            }
            _ => panic!("expected bare claim"),
        }
    }

    #[test]
    fn mixin_qualifies_per_element() {
        // `type: [a::r1, b]` — the peer qualifier is per element, an own claim
        // sits beside a qualified one.
        let res = parse_one("type: [a::r1, b]\n", "/v/m.md");
        let inst = res.instance.unwrap();
        match &inst.type_claim {
            TypeClaim::List { items, .. } => {
                assert_eq!(items[0].name.as_str(), "a");
                assert_eq!(items[0].repo.as_deref(), Some("r1"));
                assert_eq!(items[1].name.as_str(), "b");
                assert_eq!(items[1].repo, None);
            }
            _ => panic!("expected list claim"),
        }
    }

    #[test]
    fn embedded_wikilink_in_quoted_string_value_is_a_nav_link() {
        let src = "\
type: note
description: \"Runs the show at [[volvelle labs]].\"
";
        let res = parse_one(src, "/v/m.md");
        let inst = res.instance.unwrap();
        let field = &inst.fields[0];
        assert_eq!(field.key, "description");
        // The value stays a plain String, the link is navigational only.
        assert!(matches!(field.value, InstanceValue::String(_)));
        assert_eq!(field.nav_links.len(), 1);
        assert_eq!(field.nav_links[0].raw, "volvelle labs");
        // The span points at the embedded link in the source, brackets included.
        let s = &field.nav_links[0].span;
        assert_eq!(&src[s.start..s.end], "[[volvelle labs]]");
    }

    #[test]
    fn plain_string_value_has_no_nav_links() {
        let res = parse_one("type: note\ndescription: just text\n", "/v/m.md");
        let inst = res.instance.unwrap();
        assert!(inst.fields[0].nav_links.is_empty());
    }

    #[test]
    fn list_type_claim_one_element_is_single() {
        let res = parse_one("type: [decision]\n", "/v/m.md");
        let inst = res.instance.unwrap();
        match &inst.type_claim {
            TypeClaim::List { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].name.as_str(), "decision");
            }
            _ => panic!("expected list claim"),
        }
        assert!(inst.type_claim.is_single());
    }

    #[test]
    fn list_type_claim_multi_is_not_single() {
        let res = parse_one("type: [a, b]\n", "/v/m.md");
        let inst = res.instance.unwrap();
        assert_eq!(inst.type_claim.len(), 2);
        assert!(!inst.type_claim.is_single());
        // No parser-level diagnostic — N≥2 is structurally valid; the
        // validator's mixin handling kicks in at validate time.
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn missing_type_claim_emits_diagnostic() {
        let res = parse_one("description: x\n", "/v/m.md");
        assert!(res.instance.is_none());
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "missing-type-claim");
    }

    #[test]
    fn reserved_keys_on_instance_each_emit_diagnostic() {
        let src = "\
type: foo
fields: []
sealed: []
meta: []
";
        let res = parse_one(src, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes.len(), 3);
        assert!(codes.iter().all(|c| *c == "reserved-key-on-instance"));
        // Instance still parses successfully — reserved keys are ignored.
        assert!(res.instance.is_some());
    }

    #[test]
    fn non_string_top_level_key_on_instance_emits_warning() {
        // YAML allows non-string keys; the engine cannot model them. Pre-fix
        // the entry was silently dropped.
        let src = "\
type: foo
1: numeric-key
";
        let res = parse_one(src, "/v/m.md");
        let warns: Vec<_> = res
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == "mapping-key-not-a-string")
            .collect();
        assert_eq!(warns.len(), 1);
        assert_eq!(warns[0].severity, Severity::Warning);
        // Instance still parses; the recognized type-claim survives.
        assert!(res.instance.is_some());
    }

    #[test]
    fn type_claim_with_non_string_element_is_diagnosed() {
        let res = parse_one("type: [decision, 42]\n", "/v/m.md");
        assert!(res.instance.is_none());
        // Exactly one diagnostic — bad-shape, no double-fire of missing-type-claim.
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "instance-claim-bad-shape");
    }

    #[test]
    fn empty_type_list_is_diagnosed() {
        let res = parse_one("type: []\n", "/v/m.md");
        assert!(res.instance.is_none());
        // Exactly one diagnostic — bad-shape, no fall-through to validator.
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "instance-claim-bad-shape");
        assert!(res.diagnostics[0].message.contains("cannot be empty"));
    }

    #[test]
    fn type_claim_with_bad_shape_is_diagnosed() {
        let res = parse_one("type: 42\n", "/v/m.md");
        assert!(res.instance.is_none());
        // Bad-shape only — saw the key, so missing-type-claim is suppressed.
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "instance-claim-bad-shape");
    }

    #[test]
    fn top_level_not_mapping_is_diagnosed() {
        let res = parse_one("- foo\n", "/v/m.md");
        assert!(res.instance.is_none());
        assert_eq!(res.diagnostics[0].code.as_str(), "instance-not-a-mapping");
    }

    #[test]
    fn qualified_field_name_is_kept_verbatim() {
        // The parser doesn't resolve qualifiers; the name carries the
        // brace and the validator splits + resolves it via `parse_qualified_key`.
        let res = parse_one("type: foo\ntitle{note}: x\n", "/v/m.md");
        let inst = res.instance.unwrap();
        assert_eq!(inst.fields[0].key, "title{note}");
    }

    #[test]
    fn integer_float_bool_null_values_are_classified() {
        let src = "\
type: foo
i: 42
f: 3.14
b: true
n: null
";
        let res = parse_one(src, "/v/m.md");
        let inst = res.instance.unwrap();
        let by_key: std::collections::BTreeMap<&str, &InstanceValue> = inst
            .fields
            .iter()
            .map(|f| (f.key.as_str(), &f.value))
            .collect();
        assert!(matches!(by_key["i"], InstanceValue::Integer(42)));
        assert!(matches!(by_key["f"], InstanceValue::Float(_)));
        assert!(matches!(by_key["b"], InstanceValue::Boolean(true)));
        assert!(matches!(by_key["n"], InstanceValue::Null));
    }

    #[test]
    fn sequence_value_classifies_as_sequence() {
        let res = parse_one("type: foo\nxs: [1, 2]\n", "/v/m.md");
        let inst = res.instance.unwrap();
        let InstanceValue::Sequence(els) = &inst.fields[0].value else {
            panic!("expected Sequence, got {:?}", inst.fields[0].value);
        };
        assert_eq!(els.len(), 2);
        assert!(matches!(els[0].value, InstanceValue::Integer(1)));
        assert!(matches!(els[1].value, InstanceValue::Integer(2)));
        // Element spans must be set (non-trivially) — the validator uses them
        // to point per-element diagnostics at the right entry.
        assert!(els[0].span.start < els[1].span.start);
    }

    #[test]
    fn nested_sequence_value_classifies_recursively() {
        let res = parse_one("type: foo\nxss: [[1, 2], [3]]\n", "/v/m.md");
        let inst = res.instance.unwrap();
        let InstanceValue::Sequence(outer) = &inst.fields[0].value else {
            panic!("expected outer Sequence");
        };
        assert_eq!(outer.len(), 2);
        let InstanceValue::Sequence(inner) = &outer[0].value else {
            panic!("expected inner Sequence");
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn mapping_value_classifies_as_inline_value() {
        // [[type-def shape record::au-type-system]]: a YAML map at a record-typed slot is an inline value with
        // optional `type:` claim and arbitrary fields. The classifier
        // recurses, so nested maps come out as nested InlineValue.
        let res = parse_one("type: foo\nm: {a: 1}\n", "/v/m.md");
        let inst = res.instance.unwrap();
        let InstanceValue::Mapping(inline) = &inst.fields[0].value else {
            panic!("expected Mapping, got {:?}", inst.fields[0].value);
        };
        assert!(inline.type_claim.is_none());
        assert_eq!(inline.fields.len(), 1);
        assert_eq!(inline.fields[0].key, "a");
        assert!(matches!(inline.fields[0].value, InstanceValue::Integer(1)));
    }

    #[test]
    fn inline_mapping_with_type_claim_parses_claim() {
        let res = parse_one("type: outer\nm: {type: inner, a: 1}\n", "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        let InstanceValue::Mapping(inline) = &inst.fields[0].value else {
            panic!("expected Mapping");
        };
        match &inline.type_claim {
            Some(TypeClaim::Bare(c)) => assert_eq!(c.name.as_str(), "inner"),
            other => panic!("expected bare inline claim, got {other:?}"),
        }
        assert_eq!(inline.fields.len(), 1);
        assert_eq!(inline.fields[0].key, "a");
    }

    #[test]
    fn nested_inline_value_recurses() {
        let res = parse_one(
            "type: outer\nm: {type: middle, inner: {type: leaf, x: 7}}\n",
            "/v/m.md",
        );
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        let InstanceValue::Mapping(level1) = &inst.fields[0].value else {
            panic!("expected outer Mapping");
        };
        assert_eq!(level1.fields.len(), 1);
        let InstanceValue::Mapping(level2) = &level1.fields[0].value else {
            panic!("expected nested Mapping");
        };
        match &level2.type_claim {
            Some(TypeClaim::Bare(c)) => assert_eq!(c.name.as_str(), "leaf"),
            _ => panic!("expected nested inline claim"),
        }
        assert_eq!(level2.fields.len(), 1);
        assert!(matches!(level2.fields[0].value, InstanceValue::Integer(7)));
    }

    #[test]
    fn inline_value_reserved_key_emits_diagnostic() {
        // `fields:`, `sealed:`, `meta:` are type-def-only — they must not
        // appear on an inline value either (mirrors top-level rule).
        let res = parse_one("type: foo\nm: {fields: []}\n", "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["reserved-key-on-instance"]);
    }

    #[test]
    fn location_key_on_instance_is_reserved() {
        // `location:` is a type-def-only key, so it is reserved on an instance.
        let res = parse_one("type: foo\nlocation: {name: x}\n", "/v/m.md");
        assert!(res
            .diagnostics
            .iter()
            .any(|d| d.code.as_str() == "reserved-key-on-instance"));
    }

    #[test]
    fn key_and_value_spans_point_at_source_text() {
        let src = "type: foo\ndescription: \"hi\"\n";
        let res = parse_one(src, "/v/m.md");
        let inst = res.instance.unwrap();
        let f = &inst.fields[0];
        assert_eq!(&src[f.key_span.start..f.key_span.end], "description");
        // The value span covers the YAML scalar (quoted form included).
        let value_text = &src[f.value_span.start..f.value_span.end];
        assert!(value_text.contains("hi"));
    }

    // ----- `^:` block-ids on inline records ([[type block-id::au-type-system]]) -----

    fn inline_of(value: &InstanceValue) -> &InlineValue {
        match value {
            InstanceValue::Mapping(iv) => iv,
            other => panic!("expected inline value, got {other:?}"),
        }
    }

    #[test]
    fn inline_record_block_id_is_captured_and_never_a_field() {
        let src = "\
type: canvas
nodes:
  - ^: n1
    content: root
  - content: id-less
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        let InstanceValue::Sequence(els) = &inst.fields[0].value else {
            panic!("expected sequence");
        };
        let first = inline_of(&els[0].value);
        assert_eq!(first.block_id.as_ref().unwrap().id, "n1");
        // Identity-layer: `^` never lands in fields.
        assert_eq!(first.fields.len(), 1);
        assert_eq!(first.fields[0].key, "content");
        // An id-less record is fine — attaching is legal, never demanded.
        let second = inline_of(&els[1].value);
        assert!(second.block_id.is_none());
    }

    #[test]
    fn nested_inline_record_block_ids_capture_at_every_depth() {
        let src = "\
type: canvas
node:
  ^: outer
  inner:
    ^: inner-id
    content: x
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        let outer = inline_of(&inst.fields[0].value);
        assert_eq!(outer.block_id.as_ref().unwrap().id, "outer");
        let inner = inline_of(&outer.fields[0].value);
        assert_eq!(inner.block_id.as_ref().unwrap().id, "inner-id");
    }

    #[test]
    fn digit_only_block_id_needs_no_quoting() {
        // `^: 123` YAML-types as an integer; the grammar allows
        // digit-only ids, so quoting must not be required.
        let src = "\
type: canvas
node:
  ^: 123
  content: x
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty());
        let inst = res.instance.unwrap();
        let node = inline_of(&inst.fields[0].value);
        assert_eq!(node.block_id.as_ref().unwrap().id, "123");
    }

    #[test]
    fn block_id_violating_grammar_emits_block_id_malformed() {
        let src = "\
type: canvas
node:
  ^: \"bad id\"
  content: x
";
        let res = parse_one(src, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["block-id-malformed"]);
        let inst = res.instance.unwrap();
        assert!(inline_of(&inst.fields[0].value).block_id.is_none());
    }

    #[test]
    fn block_id_with_non_scalar_value_emits_block_id_malformed() {
        // A null `^:` (or any non-scalar) cannot carry an id.
        let src = "\
type: canvas
node:
  ^:
  content: x
";
        let res = parse_one(src, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["block-id-malformed"]);
        let inst = res.instance.unwrap();
        assert!(inline_of(&inst.fields[0].value).block_id.is_none());
    }

    #[test]
    fn block_id_on_instance_root_warns_and_drops() {
        let src = "\
type: canvas
^: rootId
title: x
";
        let res = parse_one(src, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["block-id-on-instance-root"]);
        assert_eq!(res.diagnostics[0].severity, Severity::Warning);
        let inst = res.instance.unwrap();
        // Dropped: neither a field nor an extra.
        assert!(inst.fields.iter().all(|f| f.key != "^"));
    }

    #[test]
    fn block_id_spans_point_at_source_text() {
        let src = "\
type: canvas
node:
  ^: n1
  content: x
";
        let res = parse_one(src, "/v/m.md");
        let inst = res.instance.unwrap();
        let decl = inline_of(&inst.fields[0].value).block_id.clone().unwrap();
        assert_eq!(&src[decl.key_span.start..decl.key_span.end], "^");
        assert_eq!(&src[decl.value_span.start..decl.value_span.end], "n1");
    }

    // ----- `#:` docstrings on instances ([[type docstring::au-type-system]]) -----

    #[test]
    fn instance_head_doc_and_frontmatter_field_docs() {
        let src = "\
#: the release checklist
#: human-gated
type: workflow
owner: alice          #: who signs off
#: leading doc for stage
stage: build
plain: x              # not a doc, incidental
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        // Leading block above the first key accumulates as the instance head doc.
        assert_eq!(
            inst.doc.as_deref(),
            Some("the release checklist\nhuman-gated")
        );
        // Trailing on a field line, and a leading block before a field, both bind.
        assert_eq!(
            inst.field_docs.get("owner").map(String::as_str),
            Some("who signs off")
        );
        assert_eq!(
            inst.field_docs.get("stage").map(String::as_str),
            Some("leading doc for stage")
        );
        // A plain `#` comment is incidental, never surfaced.
        assert!(!inst.field_docs.contains_key("plain"));
    }

    #[test]
    fn nested_record_head_and_field_docs() {
        let src = "\
type: outer
m:
  #: head of the record
  a: 1                #: doc for a
  b: 2
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        assert!(inst.doc.is_none());
        assert!(inst.field_docs.is_empty());
        let inline = inline_of(&inst.fields[0].value);
        assert_eq!(inline.doc.as_deref(), Some("head of the record"));
        assert_eq!(
            inline.field_docs.get("a").map(String::as_str),
            Some("doc for a")
        );
        assert!(!inline.field_docs.contains_key("b"));
    }

    #[test]
    fn list_item_record_head_and_field_docs() {
        // A step is a list item: its head doc sits before the item's first key,
        // and a trailing doc on a field line binds to that field.
        let src = "\
type: workflow
steps:
  #: build the signed image
  - ^: build
    gate: manual     #: reviewer confirms
    run: make image
  #: publish it
  - ^: publish
    gate: auto
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        let InstanceValue::Sequence(els) = &inst.fields[0].value else {
            panic!("expected sequence");
        };
        let step0 = inline_of(&els[0].value);
        assert_eq!(step0.doc.as_deref(), Some("build the signed image"));
        assert_eq!(
            step0.field_docs.get("gate").map(String::as_str),
            Some("reviewer confirms")
        );
        // The second item's head doc must NOT bleed from the first item's fields.
        let step1 = inline_of(&els[1].value);
        assert_eq!(step1.doc.as_deref(), Some("publish it"));
        assert!(step1.field_docs.is_empty());
    }

    #[test]
    fn trailing_field_doc_does_not_bleed_into_next_list_item() {
        // The regression the line-snap guards: item 0's trailing `#:` stays with
        // item 0's field, never read as item 1's head.
        let src = "\
type: workflow
steps:
  - gate: manual     #: for the first step only
  - gate: auto
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        let InstanceValue::Sequence(els) = &inst.fields[0].value else {
            panic!("expected sequence");
        };
        assert_eq!(
            inline_of(&els[0].value)
                .field_docs
                .get("gate")
                .map(String::as_str),
            Some("for the first step only")
        );
        let step1 = inline_of(&els[1].value);
        assert!(step1.doc.is_none());
        assert!(step1.field_docs.is_empty());
    }

    #[test]
    fn hash_inside_quoted_value_is_not_a_doc() {
        let src = "\
type: note
title: \"a # b :not a comment\"   #: real doc
";
        let res = parse_one(src, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        // The `#` inside the quoted value is content; only the trailing `#:` binds.
        assert_eq!(
            inst.field_docs.get("title").map(String::as_str),
            Some("real doc")
        );
    }

    #[test]
    fn dangling_doc_on_instance_warns() {
        let src = "\
type: note
title: x
#: documents nothing, no key follows
";
        let res = parse_one(src, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["dangling-doc-comment"]);
        assert_eq!(res.diagnostics[0].severity, Severity::Warning);
        let inst = res.instance.unwrap();
        assert!(inst.field_docs.is_empty());
        assert!(inst.doc.is_none());
    }

    #[test]
    fn instance_without_docs_carries_none() {
        let src = "type: note\ntitle: x\n";
        let res = parse_one(src, "/v/m.md");
        let inst = res.instance.unwrap();
        assert!(inst.doc.is_none());
        assert!(inst.field_docs.is_empty());
    }

    #[test]
    fn container_field_doc_forms_bind_but_trailing_on_its_key_dangles() {
        // A leading block BEFORE a container field documents the field (parent
        // field_docs); a leading block INSIDE documents the record (head doc).
        let ok = "\
type: outer
#: the m field
m:
  #: head of the m record
  a: 1
";
        let res = parse_one(ok, "/v/m.md");
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.unwrap();
        assert_eq!(
            inst.field_docs.get("m").map(String::as_str),
            Some("the m field")
        );
        assert_eq!(
            inline_of(&inst.fields[0].value).doc.as_deref(),
            Some("head of the m record")
        );

        // The ambiguous trailing form on the container's own key line dangles,
        // mirroring the type-def head rule. Not silently guessed.
        let ambiguous = "type: outer\nm:   #: doc for m\n  a: 1\n";
        let res = parse_one(ambiguous, "/v/m.md");
        let codes: Vec<&str> = res.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert_eq!(codes, vec!["dangling-doc-comment"]);
        let inst = res.instance.unwrap();
        assert!(inst.field_docs.get("m").is_none());
        assert!(inline_of(&inst.fields[0].value).doc.is_none());
    }

    #[test]
    fn body_fence_record_carries_head_and_field_docs() {
        // A marked fence's record body carries docs the same as a frontmatter
        // record. `base_offset` shifts spans but never the doc text.
        let body = "\
#: the build step
type: step
gate: manual     #: reviewer confirms
";
        let (inline, _links) = parse_block_record(Path::new("/v/m.md"), body, 100).unwrap();
        assert_eq!(inline.doc.as_deref(), Some("the build step"));
        assert_eq!(
            inline.field_docs.get("gate").map(String::as_str),
            Some("reviewer confirms")
        );
        // Spans are shifted into the file; docs are unaffected.
        assert!(inline.fields[0].key_span.start >= 100);
    }

    #[test]
    fn body_fence_record_docstring_link_is_shifted_into_the_file() {
        // A `#:` link inside a marked record fence is captured, and its span is
        // shifted by `base_offset` so it indexes the physical file.
        let body = "\
type: step
gate: manual     #: reviewer confirms, see [[gate policy]]
";
        let base = 100;
        let (_inline, links) = parse_block_record(Path::new("/v/m.md"), body, base).unwrap();
        let gate = links
            .iter()
            .find(|d| d.origin == DocOrigin::Field("gate".into()))
            .expect("fence gate doc link");
        assert_eq!(gate.link.raw, "gate policy");
        // The span is file-absolute: body-relative offset plus the base.
        let body_rel = body.find("[[gate policy]]").unwrap();
        assert_eq!(gate.link.span.start, base + body_rel);
    }
}
