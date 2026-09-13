//! Body-typing validation per [[type-def body section::au-type-system]], [[type-def body fills::au-type-system]], [[type-def body::au-type-system]].
//!
//! Consumes a parsed `Instance` plus its raw body source and runs the
//! body-typing checks layered on top of the provenance value model.
//!
//! Public entry point: `validate_body(ctx, instance, body_source,
//! body_byte_offset, is_markdown_instance)`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span, SuggestedFix};
use au_grammar::Shape;
use au_parser::{scan_body, BodyEvent};
use au_references::{resolve_block_id, BlockResolutionError, WikilinkParseError};

use crate::body::{splice_effective_body, BodyItem, BodyTemplate, FillsContract};
use crate::candidates::{
    base_field_name, build_field_shape_map, claim_closure, is_opaque_slot, record_demanded_type,
    sequence_element_shape,
};
use crate::closure::{closure_of, EffectiveShape, EffectiveShapeError};
use crate::codes;
use crate::graph::TypeGraph;
use crate::instance::{InlineValue, Instance, InstanceValue, NavLink, TypeClaim};
use crate::provenance::{effective_values, ContributionValue, Surface, ValueContainer};
use crate::typedef::{FieldName, TypeName, TypeNameClaim};
use crate::validate::ValidateContext;

/// Source bytes for every parsed instance's body, keyed by absolute path.
/// Used by cross-file `^block-id` resolution. Empty for pure-YAML
/// instances (no body).
pub type BodySources = BTreeMap<PathBuf, String>;

/// Run the body-typing checks against `instance`. `body_source` is the
/// markdown body (may be empty); `body_byte_offset` is its absolute byte
/// position inside the source file (so contribution spans line up).
/// `is_markdown_instance` is `false` for pure-YAML instance files.
/// Cross-file `^block-id` references resolve through the context's
/// `body_sources` (re-scanned body events) and `record_targets`
/// (addressable inline records).
pub fn validate_body(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    body_source: &str,
    body_byte_offset: usize,
    is_markdown_instance: bool,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // Resolve the effective body template by walking the instance's claims
    // and splicing use:.
    let template = match build_effective_template(ctx, instance, &mut diags) {
        Some(t) => t,
        None => Vec::new(),
    };
    let body_declaring = !template.is_empty();

    // [[type-def body::au-type-system]] format coupling.
    if body_declaring && !is_markdown_instance {
        diags.push(Diagnostic {
            code: codes::BODY_REQUIRED_BUT_YAML_ONLY_INSTANCE,
            severity: Severity::Error,
            span: Span::new(instance.source_path.clone(), instance.source_span),
            message:
                "instance's type declares a `body:` template, but the instance is yaml-only (must be markdown)"
                    .to_string(),
            related: vec![],
            fix: None,
        });
    }

    let events = scan_body(body_source);

    // Effective closure of the instance. Routes through the resolution-aware
    // seam so an IMPORTED (`::repo`) claim resolves its folded shape, not the
    // empty own-graph shape. Otherwise an imported instance's `any` field is
    // unrecognized and its stored `[[...]]` text is wrongly scanned as
    // navigational — the own-graph-vs-fold class the subtypes fix closed.
    //
    // Computed BEFORE the value layer: `effective_values` needs it to decide
    // whether a whole-value wikilink is a reference or a literal string, which
    // is a per-SLOT question ([[type reference::au-type-system]]).
    let shape = match crate::validate::effective_shape_for(ctx, &instance.type_claim) {
        Ok(s) => Some(s),
        Err(EffectiveShapeError::UnknownType(_)) => None,
    };

    let values = effective_values(instance, &events, body_byte_offset, shape.as_ref());

    // [[type-def body section::au-type-system]] section presence + order (declared template only).
    if body_declaring && is_markdown_instance {
        check_section_presence(instance, &template, &events, body_byte_offset, &mut diags);
    }

    // [[type-def body fills::au-type-system]] fills contracts (declared template only).
    if body_declaring {
        check_fills_contracts(
            instance,
            &template,
            &events,
            body_byte_offset,
            &values,
            &mut diags,
        );
    }

    // [[type-instance body contribution::au-type-system]] frontmatter visibility — optional field with body contributions
    // but no frontmatter key.
    if let Some(shape) = &shape {
        check_frontmatter_visibility(instance, shape, &values, &mut diags);
        // The mirror direction: a required field whose null "filled by body"
        // anchor never received a contribution is as absent as an omitted key.
        check_null_anchor_without_body_fill(instance, shape, &values, &mut diags);
    }

    // [[type value container::au-type-system]] cardinality across surfaces — bare-T fields with > 1 distinct
    // ValueContainer.
    if let Some(shape) = &shape {
        check_cardinality_cross_surface(instance, shape, &values, &mut diags);
    }

    // Per-contribution checks (body contributions only).
    if let Some(shape) = &shape {
        check_per_contribution(ctx, instance, shape, &values, &mut diags);
        check_divergent_body_bare_use(instance, shape, &values, &mut diags);
    }

    // Malformed body content: inline-code markers and wikilinks that look
    // like contribution attempts but don't parse, plus unterminated
    // fenced code blocks.
    check_malformed_markers(instance, &events, body_byte_offset, &mut diags);
    check_malformed_wikilinks(instance, &events, body_byte_offset, &mut diags);
    check_prose_dangling_links(ctx, instance, &events, body_byte_offset, &mut diags);

    // Navigational links embedded in frontmatter string values, the value-side
    // mirror of the prose pass, see [[type reference::au-type-system]].
    check_frontmatter_navigational_links(ctx, instance, shape.as_ref(), &mut diags);
    check_unterminated_fences(instance, &events, body_byte_offset, &mut diags);
    check_duplicate_block_ids(instance, &events, body_byte_offset, &mut diags);

    // Shape conformance for scalars that arrive ONLY from the body. The
    // frontmatter pass walks `instance.fields`, so a value contributed by an
    // inline marker or a text fence was never compared to its slot.
    check_body_scalar_shapes(ctx, instance, shape.as_ref(), &values, &mut diags);

    // Marked-fence validation for body fence contributions.
    // Frontmatter `[[file^id]]` resolution lives in the slot-gated
    // reference checks (validate.rs), not here — resolving every
    // wikilink-shaped string regardless of slot was the bug.
    check_marked_fences(
        ctx,
        instance,
        shape.as_ref(),
        &events,
        body_source,
        body_byte_offset,
        &mut diags,
    );

    diags
}

// ----- effective template (recursive use: splice) -----

fn build_effective_template(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    _diags: &mut Vec<Diagnostic>,
) -> Option<BodyTemplate> {
    // Find the first body-declaring type in the instance's claim closure
    // and walk it. Mixin claims don't merge bodies (per [[type-def body::au-type-system]]) — we pick
    // the first claim whose type chain has a body. Multi-mixin with
    // multiple body-bearing chains is out of scope for v1.
    for claim in instance.type_claim.iter() {
        // A `::repo` claim's body lives in the PEER's graph, not the own graph
        // (the resolution graph holds ids/fields, not bodies). Resolve the peer
        // graph through the seam; an unresolvable peer is skipped, the crosstype
        // gate owns its diagnostic, mirroring the own-graph miss below.
        let graph = match &claim.repo {
            None => ctx.graph,
            Some(repo) => match ctx.cross_repo.and_then(|cr| cr.peer_graph(repo.as_str())) {
                Some(g) => g,
                None => continue,
            },
        };
        if let Some(t) = splice_effective_body(graph, &claim.name, ctx.cross_repo) {
            return Some(t);
        }
    }
    None
}

// ----- [[type-def body section::au-type-system]] section presence + order -----

fn check_section_presence(
    instance: &Instance,
    template: &BodyTemplate,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    // Walk top-level declared sections against top-level headings. Each
    // declared section consumes one matching heading (the first
    // unconsumed one with the matching name). This is robust to:
    //
    // - `[A?, B]` body `# B` — A is absent (optional, no diagnostic);
    //   B finds its match.
    // - `[A, B]` body `# B, # A` — A consumes the second heading
    //   (position 1), B the first (position 0). Both are present, but
    //   the located positions [1, 0] are not monotonic ↑ → fires
    //   `body-section-out-of-order` on B (the later-declared one whose
    //   actual position precedes a sibling declared before it).
    // - same-name siblings `[A, B, A]` body `# A # B # A` — each
    //   declared A consumes a distinct body heading in declaration order.
    let declared = declared_top_sections(template);
    let headings: Vec<(&str, ByteRange)> = events
        .iter()
        .filter_map(|e| match e {
            BodyEvent::Heading {
                level: 1,
                text,
                span,
            } => Some((*text, *span)),
            _ => None,
        })
        .collect();

    let mut consumed = vec![false; headings.len()];
    // Per-declared section: location index + heading span, OR None when
    // absent. Used in the second pass for monotonic-order verification.
    let mut located: Vec<Option<(usize, ByteRange)>> = Vec::with_capacity(declared.len());

    for entry in &declared {
        let found = headings
            .iter()
            .enumerate()
            .find(|(i, (name, _))| !consumed[*i] && *name == entry.name)
            .map(|(i, (_, span))| (i, *span));

        match found {
            Some((idx, span)) => {
                consumed[idx] = true;
                located.push(Some((idx, span)));
            }
            None => {
                if !entry.optional {
                    diags.push(Diagnostic {
                        code: codes::BODY_SECTION_MISSING,
                        severity: Severity::Error,
                        span: Span::new(instance.source_path.clone(), instance.source_span),
                        message: format!(
                            "required body section '# {}' is missing from the instance",
                            entry.name
                        ),
                        related: vec![Span::new(entry.source_path.clone(), entry.name_span)],
                        fix: None,
                    });
                }
                located.push(None);
            }
        }
    }

    // Second pass: among found sections, verify the located indices are
    // monotonically increasing. The "previous located" (not the running
    // max) is the comparator — a single swap fires once, doesn't
    // cascade.
    let mut prev: Option<(usize, &str, ByteRange)> = None;
    for (entry, slot) in declared.iter().zip(located.iter()) {
        let Some((idx, span)) = slot else {
            continue;
        };
        if let Some((prev_idx, prev_name, prev_span)) = prev {
            if *idx < prev_idx {
                let abs_span = absolute(*span, body_byte_offset);
                let prev_abs_span = absolute(prev_span, body_byte_offset);
                diags.push(Diagnostic {
                    code: codes::BODY_SECTION_OUT_OF_ORDER,
                    severity: Severity::Error,
                    span: Span::new(instance.source_path.clone(), abs_span),
                    message: format!(
                        "body section '# {}' appears before '# {prev_name}', but '{prev_name}' is declared earlier in the template",
                        entry.name
                    ),
                    related: vec![Span::new(instance.source_path.clone(), prev_abs_span)],
                    fix: None,
                });
            }
        }
        prev = Some((*idx, entry.name.as_str(), *span));
    }
}

/// Structured presence info for one declared section in the effective
/// template. Walks the template recursively so sub-sections appear too.
/// Consumers (the CLI introspect wire, downstream tooling) use this to
/// surface "where each declared section IS in the body" without
/// re-implementing the heading walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionPresenceInfo {
    /// The section's name as declared in the template.
    pub name: String,
    /// True for `section?:` declarations.
    pub optional: bool,
    /// 1-indexed depth — top-level = 1, sub-section under a top-level = 2, …
    pub depth: u8,
    /// True when a matching heading is present in the body.
    pub present: bool,
    /// Absolute body span of the matching heading (already shifted by
    /// `body_byte_offset`), when present.
    pub span: Option<ByteRange>,
    /// Stripped, name-only path from root to this section (e.g.
    /// `["Why", "Background"]`). Each entry is the declared section
    /// name without the index prefix that [[type value container::au-type-system]] uses internally.
    pub path: Vec<String>,
}

/// Walk a body template and report presence info for every declared
/// section at every depth. Driven by the same `present_sections` /
/// `heading_spans` data that the validator's section-missing /
/// fills-contract checks consume — there's exactly one heading-walk
/// algorithm across `body_validate` (this) and the CLI introspect
/// surface (which now calls into this).
pub fn compute_section_presence(
    template: &BodyTemplate,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
) -> Vec<SectionPresenceInfo> {
    let (present_sections, heading_spans) = collect_body_section_paths(events, body_byte_offset);
    let mut out = Vec::new();
    collect_presence_recursive(
        template,
        &[],
        1,
        &present_sections,
        &heading_spans,
        &mut out,
    );
    out
}

fn collect_presence_recursive(
    items: &[BodyItem],
    parent_path: &[String],
    depth: u8,
    present_sections: &BTreeSet<Vec<String>>,
    heading_spans: &BTreeMap<Vec<String>, ByteRange>,
    out: &mut Vec<SectionPresenceInfo>,
) {
    for item in items {
        if let BodyItem::Section {
            name,
            optional,
            body,
            ..
        } = item
        {
            let mut child_path = parent_path.to_vec();
            child_path.push(name.clone());
            let present = present_sections.contains(&child_path);
            let span = heading_spans.get(&child_path).copied();
            out.push(SectionPresenceInfo {
                name: name.clone(),
                optional: *optional,
                depth,
                present,
                span,
                path: child_path.clone(),
            });
            if let Some(nested) = body {
                collect_presence_recursive(
                    nested,
                    &child_path,
                    depth + 1,
                    present_sections,
                    heading_spans,
                    out,
                );
            }
        }
    }
}

/// One declared top-level section in a body template, carried with the
/// data needed to anchor a `body-section-missing` `related[]` span back
/// at the section's declaration in the type-def file (which may be a
/// spliced ancestor, not the instance's own claim).
struct DeclaredSection {
    name: String,
    optional: bool,
    name_span: ByteRange,
    source_path: std::path::PathBuf,
}

fn declared_top_sections(template: &BodyTemplate) -> Vec<DeclaredSection> {
    template
        .iter()
        .filter_map(|item| match item {
            BodyItem::Section {
                name,
                optional,
                name_span,
                source_path,
                ..
            } => Some(DeclaredSection {
                name: name.clone(),
                optional: *optional,
                name_span: *name_span,
                source_path: source_path.clone(),
            }),
            _ => None,
        })
        .collect()
}

// ----- [[type-def body fills::au-type-system]] contracts + exclusivity -----

fn check_fills_contracts(
    instance: &Instance,
    template: &BodyTemplate,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    // Build both: set of body section paths (for skip-when-absent), and
    // map of stripped-path → heading absolute span (so unmet-contract
    // diagnostics can anchor at the section heading in the instance).
    let (present_sections, heading_spans) = collect_body_section_paths(events, body_byte_offset);
    walk_scope(
        instance,
        template,
        &[],
        &[],
        &present_sections,
        &heading_spans,
        values,
        diags,
    );
}

/// Walk body events, returning:
/// - the set of section paths visible in the body (name-only, indices
///   stripped — `## Result` under `# Outcome` → `["Outcome", "Result"]`)
///   used by `walk_scope` to skip absent optional sections, and
/// - the absolute span of each section's heading line, keyed by the same
///   stripped path. Used by `enforce_contract` to anchor
///   `fills-contract-unmet` diagnostics at the violating heading.
fn collect_body_section_paths(
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
) -> (BTreeSet<Vec<String>>, BTreeMap<Vec<String>, ByteRange>) {
    let mut paths = BTreeSet::new();
    let mut spans: BTreeMap<Vec<String>, ByteRange> = BTreeMap::new();
    for (path, event) in au_parser::derive_section_paths(events) {
        let BodyEvent::Heading { span, .. } = event else {
            continue;
        };
        let stripped: Vec<String> = path
            .iter()
            .map(|s| strip_index_prefix(s).to_string())
            .collect();
        paths.insert(stripped.clone());
        // First heading wins when an instance has two headings with the
        // same stripped path (duplicate-name siblings) — diagnostic
        // anchoring on the first is a fine default; consumer can
        // disambiguate via related[] later if needed.
        spans
            .entry(stripped)
            .or_insert_with(|| absolute(*span, body_byte_offset));
    }
    (paths, spans)
}

fn walk_scope(
    instance: &Instance,
    items: &[BodyItem],
    path_prefix: &[String],
    stripped_prefix: &[String],
    present_sections: &BTreeSet<Vec<String>>,
    heading_spans: &BTreeMap<Vec<String>, ByteRange>,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    // Body-level fills: items at this level with discriminator Fills.
    // Body-level contracts always enforce — the body itself is always
    // "present" once we're validating it.
    for item in items {
        if let BodyItem::Fills { contract, .. } = item {
            enforce_contract(
                instance,
                contract,
                path_prefix,
                stripped_prefix,
                heading_spans,
                values,
                diags,
            );
        }
    }
    // Per-section recursion.
    let mut idx_at_level: std::collections::BTreeMap<usize, u32> =
        std::collections::BTreeMap::new();
    for item in items {
        if let BodyItem::Section {
            name,
            optional,
            fills,
            body,
            name_span,
            source_path,
            ..
        } = item
        {
            let level: usize = path_prefix.len() + 1;
            let counter = idx_at_level.entry(level).or_insert(0);
            *counter += 1;
            let label = format!("{} {}", counter, name);
            let mut child_path = path_prefix.to_vec();
            child_path.push(label);
            let mut child_stripped = stripped_prefix.to_vec();
            child_stripped.push(name.clone());
            let absent = !present_sections.contains(&child_stripped);
            // [[type-def body section::au-type-system]]: declared sections must be present at every nesting
            // depth. Top-level sections are checked separately by
            // `check_section_presence` (which also enforces top-level
            // ordering); this branch covers depth ≥ 1.
            if absent && !path_prefix.is_empty() && !*optional {
                let heading_marks = "#".repeat(level);
                diags.push(Diagnostic {
                    code: codes::BODY_SECTION_MISSING,
                    severity: Severity::Error,
                    span: Span::new(instance.source_path.clone(), instance.source_span),
                    message: format!(
                        "required body section '{heading_marks} {name}' is missing from the instance"
                    ),
                    related: vec![Span::new(source_path.clone(), *name_span)],
                    fix: None,
                });
            }
            // [[type-def body fills::au-type-system]]: `fills` on a `section?:` (or any nested section) that
            // didn't materialize in the body is a no-op — the contract
            // attaches to a scope that doesn't exist in this instance.
            // Skip enforcement AND skip recursion (a nested fills inside
            // an absent section is also moot).
            if absent {
                continue;
            }
            if let Some(contract) = fills {
                enforce_contract(
                    instance,
                    contract,
                    &child_path,
                    &child_stripped,
                    heading_spans,
                    values,
                    diags,
                );
            }
            if let Some(nested) = body {
                walk_scope(
                    instance,
                    nested,
                    &child_path,
                    &child_stripped,
                    present_sections,
                    heading_spans,
                    values,
                    diags,
                );
            }
        }
    }
}

fn enforce_contract(
    instance: &Instance,
    contract: &FillsContract,
    scope_prefix: &[String],
    stripped_scope: &[String],
    heading_spans: &BTreeMap<Vec<String>, ByteRange>,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    let declared: BTreeSet<&str> = contract.fields.iter().map(|f| f.name.as_str()).collect();

    // Anchor span for unmet diagnostics: the scope's heading in the
    // instance, when present; instance.source_span otherwise (body-level
    // contracts and degenerate cases). The contract's own declaration
    // travels via `related[]`.
    let unmet_anchor = heading_spans
        .get(stripped_scope)
        .copied()
        .unwrap_or(instance.source_span);

    // 1) Each declared field must have ≥1 BODY contribution whose
    //    section_path is under (or equal to) `scope_prefix`.
    for claim in &contract.fields {
        let mut satisfied = false;
        if let Some(containers) = values.get(&claim.name) {
            for c in containers {
                for contrib in &c.contributions {
                    if contrib.surface == Surface::Frontmatter {
                        continue;
                    }
                    if path_under(scope_prefix, &contrib.section_path) {
                        satisfied = true;
                        break;
                    }
                }
                if satisfied {
                    break;
                }
            }
        }
        if !satisfied {
            diags.push(Diagnostic {
                code: codes::FILLS_CONTRACT_UNMET,
                severity: Severity::Error,
                span: Span::new(instance.source_path.clone(), unmet_anchor),
                message: format!(
                    "fills contract for '{field}' not satisfied — no body contribution found in scope {scope}; contributions are self-tagged (`[[target:{field}]]`, `[:{field}] value`, or a ```[:{field}] fence`) — a bare `[[target]]` is navigational and does not count",
                    field = claim.name.as_str(),
                    scope = format_scope(scope_prefix)
                ),
                related: vec![Span::new(contract.source_path.clone(), claim.span)],
                fix: None,
            });
        }
    }

    // 2) Exclusivity: forbid body contributions to fields NOT in `declared`
    //    anywhere within `scope_prefix`.
    if contract.exclusive {
        for (field, containers) in values {
            if declared.contains(field.as_str()) {
                continue;
            }
            for c in containers {
                for contrib in &c.contributions {
                    if contrib.surface == Surface::Frontmatter {
                        continue;
                    }
                    if path_under(scope_prefix, &contrib.section_path) {
                        diags.push(Diagnostic {
                            code: codes::FILLS_CONTRACT_EXCEEDED,
                            severity: Severity::Error,
                            span: Span::new(
                                instance.source_path.clone(),
                                contrib.location.byte_range,
                            ),
                            message: format!(
                                "fills!: forbids field '{}' inside scope {} — only {} permitted",
                                field.as_str(),
                                format_scope(scope_prefix),
                                format_declared(&declared)
                            ),
                            related: vec![Span::new(
                                contract.source_path.clone(),
                                contract.fields_span,
                            )],
                            fix: None,
                        });
                    }
                }
            }
        }
    }
}

fn path_under(scope_prefix: &[String], contrib_path: &[String]) -> bool {
    if scope_prefix.is_empty() {
        return true;
    }
    if contrib_path.len() < scope_prefix.len() {
        return false;
    }
    // Compare by heading name only (strip the leading "<n> " index).
    // The template walker numbers its scopes by template position;
    // `derive_section_paths` numbers by body-occurrence position. When
    // a template-declared section is `section?:` and absent from the
    // body (or vice versa) those indices diverge, so strict element
    // equality wrongly says "not under". The wire-shape `section_path`
    // keeps its index for sibling disambiguation downstream.
    scope_prefix
        .iter()
        .zip(contrib_path.iter())
        .all(|(a, b)| strip_index_prefix(a) == strip_index_prefix(b))
}

/// `"3 Outcome"` → `"Outcome"`. Conservative: only strips when the
/// leading token is all-ASCII-digits followed by a single space, which
/// is the format `derive_section_paths` and `walk_scope` both emit.
fn strip_index_prefix(label: &str) -> &str {
    if let Some(space_idx) = label.find(' ') {
        if !label[..space_idx].is_empty() && label[..space_idx].bytes().all(|b| b.is_ascii_digit())
        {
            return &label[space_idx + 1..];
        }
    }
    label
}

fn format_scope(prefix: &[String]) -> String {
    if prefix.is_empty() {
        "<body-level>".to_string()
    } else {
        prefix.join(" > ")
    }
}

fn format_declared(declared: &BTreeSet<&str>) -> String {
    let names: Vec<String> = declared.iter().map(|n| format!("'{n}'")).collect();
    match names.len() {
        0 => "(none)".to_string(),
        1 => names.into_iter().next().unwrap(),
        _ => format!("[{}]", names.join(", ")),
    }
}

// ----- [[type-instance body contribution::au-type-system]] frontmatter visibility -----

fn check_frontmatter_visibility(
    instance: &Instance,
    shape: &crate::closure::EffectiveShape,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    let frontmatter_keys: BTreeSet<&str> = instance.fields.iter().map(|f| f.key.as_str()).collect();
    for (field, containers) in values {
        // Skip if the field has a frontmatter key already.
        if frontmatter_keys.contains(field.as_str()) {
            continue;
        }
        // Body contributions only.
        let has_body_contribution = containers
            .iter()
            .flat_map(|c| c.contributions.iter())
            .any(|c| c.surface != Surface::Frontmatter);
        if !has_body_contribution {
            continue;
        }
        // The field must be in the closure (otherwise it's an extra, advisory).
        let Some(origin) = shape.get(field) else {
            continue;
        };
        let first = containers
            .iter()
            .flat_map(|c| c.contributions.iter())
            .find(|c| c.surface != Surface::Frontmatter)
            .map(|c| c.location.byte_range)
            .unwrap_or(instance.source_span);
        let canonical = origin.canonical();
        diags.push(Diagnostic {
            code: codes::BODY_FILLS_WITHOUT_FRONTMATTER_KEY,
            severity: Severity::Error,
            span: Span::new(instance.source_path.clone(), first),
            message: format!(
                "body contributes to '{}' but frontmatter is missing the `{}:` key",
                field.as_str(),
                field.as_str()
            ),
            related: vec![Span::new(
                canonical.1.origin_path.clone(),
                canonical.1.decl.name_span,
            )],
            fix: Some(au_diagnostics::SuggestedFix {
                description: format!(
                    "add `{}:` to the frontmatter; an empty value (`{}: `) is fine — it marks the key as filled by the body",
                    field.as_str(),
                    field.as_str()
                ),
            }),
        });
    }
}

/// A required field written as a null frontmatter key (`field:`) is the
/// [[type-instance body contribution::au-type-system]] "filled by body" anchor, a promise the body will supply it.
/// The frontmatter-pass required-field check counts the key as provided by mere
/// presence, so it stays silent; if no body contribution ever arrives the
/// promise breaks and the field is as absent as an omitted key. Fire
/// `required-field-absent` here, the only pass that sees the merged body
/// `values`.
///
/// Mutually exclusive with the frontmatter-pass check (`validate`): that fires
/// only when the key is ABSENT, this only when the key is present but null. So a
/// truly-missing field fires once there, a null-anchored one fires once here.
/// A null anchor WITH a body contribution, or an optional field, stays clean.
///
/// The qualified form (`field{T}`) is left to the qualifier path; this covers
/// the bare key the finding names.
fn check_null_anchor_without_body_fill(
    instance: &Instance,
    shape: &crate::closure::EffectiveShape,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    // Bare frontmatter keys whose value is null — the filled-by-body anchors.
    let null_anchors: BTreeSet<&str> = instance
        .fields
        .iter()
        .filter(|f| matches!(f.value, InstanceValue::Null))
        .map(|f| f.key.as_str())
        .collect();
    if null_anchors.is_empty() {
        return;
    }
    for (field_name, field_origin) in shape.iter() {
        if !null_anchors.contains(field_name.as_str()) {
            continue;
        }
        // Any body contribution keeps the promise.
        let filled_by_body = values
            .get(field_name)
            .into_iter()
            .flatten()
            .flat_map(|c| c.contributions.iter())
            .any(|c| c.surface != Surface::Frontmatter);
        if filled_by_body {
            continue;
        }
        let anchor_span = instance
            .fields
            .iter()
            .find(|f| f.key.as_str() == field_name.as_str())
            .map(|f| f.key_span)
            .unwrap_or(instance.source_span);
        // Fire per required origin, mirroring the frontmatter-pass check
        // (auto-unify collapses shapes, not optional-ness).
        for (_origin_id, info) in field_origin.origins() {
            if info.decl.optional {
                continue;
            }
            diags.push(Diagnostic {
                code: codes::REQUIRED_FIELD_ABSENT,
                severity: Severity::Error,
                span: Span::new(instance.source_path.clone(), anchor_span),
                message: format!(
                    "instance is missing required field '{}' (declared on '{}')",
                    field_name.as_str(),
                    info.type_name.as_str()
                ),
                related: vec![Span::new(info.origin_path.clone(), info.decl.name_span)],
                fix: None,
            });
        }
    }
}

// ----- [[type value container::au-type-system]] cardinality across surfaces -----

fn check_cardinality_cross_surface(
    instance: &Instance,
    shape: &crate::closure::EffectiveShape,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    for (field, containers) in values {
        let Some(origin) = shape.get(field) else {
            continue;
        };
        let decl = origin.canonical_decl();
        let Ok(parsed) = decl.parsed_shape.as_ref() else {
            continue;
        };
        let is_bare = !matches!(parsed, Shape::List { .. });
        if is_bare && containers.len() > 1 {
            // [[type value container::au-type-system]]: the diagnostic includes the location of every
            // Contribution under every ValueContainer so the author can
            // see all duplication sites at once. Anchor at the first
            // container's earliest contribution (the canonical value);
            // every other Contribution — across ALL containers — goes
            // into related[].
            let primary_span = containers[0]
                .contributions
                .iter()
                .map(|c| c.location.byte_range)
                .next()
                .unwrap_or(instance.source_span);
            let mut related: Vec<Span> = Vec::new();
            // Skip the first contribution of the first container — that's
            // the primary span. Every other contribution (including
            // additional contributions to the first container, plus
            // every contribution to subsequent containers) lands in
            // related[].
            for (ci, container) in containers.iter().enumerate() {
                for (xi, contrib) in container.contributions.iter().enumerate() {
                    if ci == 0 && xi == 0 {
                        continue;
                    }
                    related.push(Span::new(
                        instance.source_path.clone(),
                        contrib.location.byte_range,
                    ));
                }
            }
            diags.push(Diagnostic {
                code: codes::FIELD_CARDINALITY_EXCEEDED,
                severity: Severity::Error,
                span: Span::new(instance.source_path.clone(), primary_span),
                message: format!(
                    "field '{}' has {} distinct ValueContainers but its shape is bare (cardinality 1)",
                    field.as_str(),
                    containers.len()
                ),
                related,
                fix: None,
            });
        }
    }
}

/// Build `related[]` spans pointing at the instance's `type:` claim.
/// For a bare claim, one span on the type name. For a list, one span
/// on the list value (since individual element spans rarely add signal
/// — the actionable fix is "add a type that declares this field").
/// Used by `unbound-field-binding` and `unknown-field-in-prose-contribution`
/// to give the author a jump target for the claim that needs editing.
fn type_claim_related(instance: &Instance) -> Vec<Span> {
    match &instance.type_claim {
        TypeClaim::Bare(c) => vec![Span::new(instance.source_path.clone(), c.span)],
        TypeClaim::List { value_span, .. } => {
            vec![Span::new(instance.source_path.clone(), *value_span)]
        }
    }
}

// ----- per-contribution checks -----

fn check_per_contribution(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    shape: &crate::closure::EffectiveShape,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    for (field, containers) in values {
        for c in containers {
            for contrib in &c.contributions {
                if contrib.surface == Surface::Frontmatter {
                    continue;
                }
                // A divergent field lives in `shape.divergent`, not `fields`, so
                // check both — a qualified body contribution to a divergent field
                // is in-closure ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
                let in_closure = shape.get(field).is_some() || shape.get_divergent(field).is_some();
                if !in_closure {
                    // The instance's `type:` claim is the most actionable
                    // jump target — the author either misspelled the
                    // field name or needs to add a type that declares
                    // the field to the claim list.
                    let claim_related = type_claim_related(instance);
                    match contrib.surface {
                        Surface::BodyWikilink => {
                            diags.push(Diagnostic {
                                code: codes::UNBOUND_FIELD_BINDING,
                                severity: Severity::Error,
                                span: Span::new(
                                    instance.source_path.clone(),
                                    contrib.location.byte_range,
                                ),
                                message: format!(
                                    "wikilink :field references '{}' which is not in the instance's effective closure",
                                    field.as_str()
                                ),
                                related: claim_related.clone(),
                                fix: None,
                            });
                        }
                        Surface::BodyInlineCode => {
                            diags.push(Diagnostic {
                                code: codes::UNKNOWN_FIELD_IN_PROSE_CONTRIBUTION,
                                severity: Severity::Warning,
                                span: Span::new(
                                    instance.source_path.clone(),
                                    contrib.location.byte_range,
                                ),
                                message: format!(
                                    "inline `[:{}]` references a field absent from the instance's effective closure (advisory)",
                                    field.as_str()
                                ),
                                related: claim_related,
                                fix: None,
                            });
                        }
                        // A fence is the multi-line CARRIER of the same contribution
                        // ([[type-instance body contribution::au-type-system]], "the carriers differ only by
                        // EXTENT"), so an out-of-closure fence gets the same advisory
                        // the inline marker does. Without this a typo'd `` ```[:sumary] ``
                        // was completely silent where `` `[:sumary]` `` warned.
                        Surface::BodyFence => {
                            diags.push(Diagnostic {
                                code: codes::UNKNOWN_FIELD_IN_PROSE_CONTRIBUTION,
                                severity: Severity::Warning,
                                span: Span::new(
                                    instance.source_path.clone(),
                                    contrib.location.byte_range,
                                ),
                                message: format!(
                                    "marked fence `[:{}]` references a field absent from the instance's effective closure (advisory)",
                                    field.as_str()
                                ),
                                related: claim_related,
                                fix: None,
                            });
                        }
                        _ => {}
                    }
                    continue;
                }
                // A body qualifier `field{type}` is validated exactly as a
                // frontmatter key is: the qualifier must be in the closure, its
                // closure must declare the field, and an ambiguous descendant
                // qualifier is rejected. The shared `resolve_qualifier` keeps the
                // two surfaces identical — without it a bogus qualifier on an
                // auto-unified field was silently ignored on the body surface
                // only ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
                //
                // Fired PER SITE, deliberately not deduped. These diagnostics
                // anchor at the offending token's own span (unlike the
                // field-level `mixin-collision` / `required-field-absent`), so
                // each occurrence of a bad qualifier is its own fix-site. Flagging
                // every site at once lets an agent fix them in one pass rather
                // than rediscovering the next one on each re-validation.
                if let Some(q) = &contrib.qualifier {
                    if let Err(diag) = crate::validate::resolve_qualifier(
                        ctx,
                        shape,
                        &instance.source_path,
                        field,
                        &q.type_name,
                        q.repo.as_deref(),
                        contrib.location.byte_range,
                    ) {
                        diags.push(diag);
                    }
                }
                // Body wikilink reference checks, gated to slots that
                // admit a reference ([[type reference::au-type-system]]): block-id
                // existence (`block-id-not-found` / `block-id-not-typed`)
                // and the shape mismatch. A wikilink bound to a primitive
                // field is already a shape mismatch; it never block-id
                // resolves.
                if let (
                    Surface::BodyWikilink,
                    ContributionValue::Reference {
                        target,
                        block_id,
                        repo,
                        ..
                    },
                ) = (contrib.surface, &contrib.value)
                {
                    // Resolve the slot: a resolved field's canonical decl, or a
                    // divergent field's qualified origin ([[type-def fields collision - auto-unify and qualified field::au-type-system]]).
                    if let Some(info) = crate::provenance::contrib_target_decl(
                        shape,
                        field,
                        contrib.qualifier.as_ref(),
                    ) {
                        let decl = &info.decl;
                        if let Ok(parsed) = decl.parsed_shape.as_ref() {
                            if let Some(demands) = slot_required_demands(parsed) {
                                // Block-id existence + mode routing for a LOCAL
                                // value. A skip means the anchor is missing (nav
                                // growth) or the `^^` value is broken — there is
                                // no value to type-check.
                                let skip_value_check = if repo.is_none() {
                                    if let Some(block) = block_id {
                                        diagnose_block_id_resolution(
                                            ctx,
                                            &instance.source_path,
                                            contrib.location.byte_range,
                                            target,
                                            block,
                                            diags,
                                        )
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                };
                                // Only a `^^` block-referent checks the BLOCK's
                                // value; a bare `^` (or no `^`) contributes the FILE.
                                let value_block_id: Option<&str> = block_id
                                    .as_ref()
                                    .filter(|b| b.referent)
                                    .map(|b| b.id.as_str());
                                // Union over branches: any satisfying branch is
                                // enough. A BARE branch keeps the by-name closure
                                // check; a `::repo` branch goes through the
                                // `qualified_demand` TypeId-membership seam, the same
                                // the frontmatter path uses (the demand's `::repo`
                                // was previously dropped — finding 2.1).
                                let bare: Vec<String> = demands
                                    .iter()
                                    .filter(|d| d.repo.is_none())
                                    .map(|d| d.base.clone())
                                    .collect();
                                let bare_ok = !bare.is_empty()
                                    && if let Some(repo) = repo {
                                        cross_repo_body_target_satisfies(
                                            ctx,
                                            &instance.source_path,
                                            repo,
                                            target,
                                            value_block_id,
                                            &bare,
                                        )
                                    } else {
                                        match value_block_id {
                                            Some(id) => block_target_satisfies(
                                                ctx,
                                                &instance.source_path,
                                                target,
                                                id,
                                                &bare,
                                            ),
                                            None => target_type_satisfies(ctx, target, &bare),
                                        }
                                    };
                                let qual_ok = demands
                                    .iter()
                                    .filter_map(|d| d.repo.as_deref().map(|r| (d.base.as_str(), r)))
                                    .any(|(base, demand_repo)| {
                                        qualified_demand_satisfied(
                                            ctx,
                                            &instance.source_path,
                                            repo.as_deref(),
                                            target,
                                            value_block_id,
                                            base,
                                            demand_repo,
                                        )
                                    });
                                if !skip_value_check && !(bare_ok || qual_ok) {
                                    diags.push(Diagnostic {
                                        code: codes::BODY_SLOT_SHAPE_MISMATCH,
                                        severity: Severity::Error,
                                        span: Span::new(
                                            instance.source_path.clone(),
                                            contrib.location.byte_range,
                                        ),
                                        message: format!(
                                            "wikilink target '{}' does not satisfy slot '{}' for field '{}'",
                                            target,
                                            decl.raw_shape,
                                            field.as_str()
                                        ),
                                        related: vec![Span::new(
                                            info.origin_path.clone(),
                                            info.decl.shape_span,
                                        )],
                                        fix: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A BARE body contribution (no `field{type}` qualifier) to a DIVERGENT field is
/// a `mixin-collision`, the body twin of the frontmatter bare-use rule
/// ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). The frontmatter divergent pass sees only frontmatter
/// keys, so a body-only bare use would otherwise slip through unvalidated. Fires
/// once per divergent field with a bare body contribution, anchored at the
/// `type:` claim like the frontmatter case.
fn check_divergent_body_bare_use(
    instance: &Instance,
    shape: &crate::closure::EffectiveShape,
    values: &std::collections::BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    let claim_span = match &instance.type_claim {
        TypeClaim::Bare(c) => c.span,
        TypeClaim::List { value_span, .. } => *value_span,
    };
    for (field, containers) in values {
        let Some(fo) = shape.get_divergent(field) else {
            continue;
        };
        // The frontmatter divergent pass already fires the collision when a bare
        // frontmatter key is present; skip here so the two surfaces do not
        // double-fire for one field (a bare key is the exact field name, no `{`).
        if instance
            .fields
            .iter()
            .any(|f| f.key.as_str() == field.as_str())
        {
            continue;
        }
        let has_bare_body = containers
            .iter()
            .flat_map(|c| c.contributions.iter())
            .any(|c| c.surface != Surface::Frontmatter && c.qualifier.is_none());
        if has_bare_body {
            diags.push(crate::validate::mixin_collision_diag(
                &instance.source_path,
                claim_span,
                field,
                fo,
            ));
        }
    }
}

/// The demanded types for a slot's reference branch, each a `QualifiedName` so a
/// `::repo` demand keeps its repo (dropping it was finding 2.1). `Some` for
/// shapes that demand a `type:` closure check on the referenced file, `None` for
/// shapes that can't be referenced (primitives, enums, non-reference compounds).
fn slot_required_demands(shape: &Shape) -> Option<Vec<au_grammar::QualifiedName>> {
    match shape {
        Shape::Reference(name) | Shape::Record(name) | Shape::InlineOrReference(name) => {
            Some(vec![name.clone()])
        }
        Shape::List { inner, .. } => slot_required_demands(inner),
        Shape::CompoundReference { branches, .. } => Some(branches.clone()),
        _ => None,
    }
}

/// A qualified body-slot demand (`foo::repo*`): resolve the referenced target,
/// then check `demanded ∈ target_folded` over the `qualified_demand` seam, the
/// body sibling of the frontmatter cross-repo reference check. True (no mismatch)
/// when no resolver is wired, the demand or target is unresolvable, or the block
/// is untyped — mirroring the bare path's leniency, the `crosstype` gate and the
/// existence pass own those diagnostics.
fn qualified_demand_satisfied(
    ctx: &ValidateContext<'_>,
    source_path: &std::path::Path,
    value_repo: Option<&str>,
    target: &str,
    block_id: Option<&str>,
    demand_base: &str,
    demand_repo: &str,
) -> bool {
    let Some(resolver) = ctx.cross_repo else {
        return true;
    };
    // Resolve the target file: local form (empty name) is the host file, a
    // `::repo` value resolves into that peer, else the local repo.
    let target_path = if target.is_empty() {
        source_path.to_path_buf()
    } else if let Some(vrepo) = value_repo {
        match resolver.resolve(source_path, vrepo, target) {
            Some(t) => t.path,
            None => return true,
        }
    } else {
        match ctx.repo_index.resolve(target) {
            Ok(p) => p,
            Err(_) => return true,
        }
    };
    // A `^block-id` target's satisfaction is the BLOCK's own claim; a whole-file
    // target folds its frontmatter claim (override `None`, the seam engine-reads it).
    let block_claim = match block_id {
        Some(id) => match body_block_claim(ctx, &target_path, id) {
            Some(c) => Some(c),
            None => return true, // untyped block; its own diagnostic owns it
        },
        None => None,
    };
    match resolver.qualified_demand(demand_base, demand_repo, &target_path, block_claim.as_ref()) {
        Some(qd) => qd.target_folded.contains(&qd.demanded),
        None => true,
    }
}

/// The `type:` claim of a `^block-id` target in QUALIFIED form, for the
/// `qualified_demand` override: an addressable inline record's precomputed
/// qualified claim, else a fenced block parsed from the target body (the same
/// `::repo`-splitting `block_claims_of` uses on the frontmatter side). `None`
/// for an untyped block.
fn body_block_claim(
    ctx: &ValidateContext<'_>,
    target_path: &std::path::Path,
    block_id: &str,
) -> Option<TypeClaim> {
    if let Some(rec) = ctx
        .ref_data
        .record_targets(target_path)
        .as_ref()
        .and_then(|t| t.get(block_id))
    {
        if rec.qualified.is_empty() {
            return None;
        }
        return Some(TypeClaim::List {
            items: rec.qualified.clone(),
            value_span: ByteRange::new(0, 0),
        });
    }
    let target_body = ctx.ref_data.body(target_path)?;
    let events = au_parser::scan_body(target_body);
    let resolved = au_references::resolve_block_id(&events, block_id).ok()?;
    let docs = au_parser::yaml::parse(resolved.body).ok()?;
    let doc = docs.first()?;
    let raws = extract_block_type_claims(doc);
    if raws.is_empty() {
        return None;
    }
    let items: Vec<TypeNameClaim> = raws
        .iter()
        .map(|r| TypeNameClaim::parse(r, ByteRange::new(0, 0)))
        .collect();
    Some(TypeClaim::List {
        items,
        value_span: ByteRange::new(0, 0),
    })
}

/// `[[target^block_id:field]]` body wikilinks reference a specific
/// fenced block inside the target file; its inferred type for shape
/// purposes is the block's own `type:` claim, NOT the host file's.
///
/// Returns `true` (no mismatch fires) when the block resolves and its
/// type claim satisfies any of `required`. Falls back to `true` on
/// resolution failure — `BLOCK_ID_NOT_FOUND` / `BLOCK_ID_NOT_TYPED` are
/// separate diagnostics; don't double-fire.
fn block_target_satisfies(
    ctx: &ValidateContext<'_>,
    host_path: &std::path::Path,
    target: &str,
    block_id: &str,
    required: &[String],
) -> bool {
    // `file*` / `any*` are satisfied by mere existence; block-id resolution
    // isn't even needed for that case. The local form trivially exists.
    if is_existence_only_demand(required) {
        return target.is_empty() || ctx.repo_index.resolve(target).is_ok();
    }
    // Local form ([[type reference::au-type-system]]): empty name means the host file.
    let target_path = if target.is_empty() {
        host_path.to_path_buf()
    } else {
        match ctx.repo_index.resolve(target) {
            Ok(p) => p,
            Err(_) => return true,
        }
    };
    // Addressable inline record ([[type block-id::au-type-system]]): its precomputed
    // effective claim is checked, frontmatter precedes the body.
    let records = ctx.ref_data.record_targets(&target_path);
    if let Some(target) = records.as_ref().and_then(|targets| targets.get(block_id)) {
        if target.claims.is_empty() {
            return true; // claim-less record diagnosed at the target
        }
        return required_satisfied_by(ctx, target.claims.iter().map(|n| n.as_str()), required);
    }
    let Some(target_body) = ctx.ref_data.body(&target_path) else {
        return true;
    };
    let events = au_parser::scan_body(target_body);
    let Ok(resolved) = au_references::resolve_block_id(&events, block_id) else {
        return true;
    };
    let Ok(docs) = au_parser::yaml::parse(resolved.body) else {
        return true;
    };
    let Some(doc) = docs.first() else {
        return true;
    };
    let block_claims = extract_block_type_claims(doc);
    if block_claims.is_empty() {
        return true; // BLOCK_ID_NOT_TYPED handled elsewhere
    }
    required_satisfied_by(ctx, block_claims.iter().map(|s| s.as_str()), required)
}

/// Closure-walk each claim name; true when any required name is in the
/// union of the claims' closures (ancestors included).
fn required_satisfied_by<'a>(
    ctx: &ValidateContext<'_>,
    claims: impl Iterator<Item = &'a str>,
    required: &[String],
) -> bool {
    // Test each claim's closure directly rather than materializing the union as
    // a `BTreeSet<String>`. "any required in the union" is the same predicate as
    // "some claim's closure contains some required", so the early return yields
    // an identical boolean without cloning every closure member into a string.
    for claim_name in claims {
        let name = TypeName(claim_name.to_string());
        let closure = crate::closure::closure_of(ctx.graph, &name);
        if closure
            .iter()
            .any(|c| required.iter().any(|r| c.as_str() == r.as_str()))
        {
            return true;
        }
    }
    false
}

/// Every type claim a body's typed blocks (` ```[:field] ` fences) declare,
/// each in QUALIFIED form ([`TypeNameClaim`], `::repo` split) with the fence's
/// body-relative span. The body-contribution sibling of a frontmatter or
/// inline-record claim, so a body typed block claiming a peer type is a cross-repo
/// fold SEED and a `::repo` gate site, the same as those positions.
///
/// An unmarked fence is an ordinary code block and contributes none; a mixin
/// `type: [a, b]` yields one entry per name. The caller adds the body byte offset
/// to the span for an absolute diagnostic position.
pub fn collect_body_typed_block_claims(body: &str) -> Vec<(TypeNameClaim, ByteRange)> {
    let mut out = Vec::new();
    for event in scan_body(body) {
        let BodyEvent::FencedBlock {
            info,
            body: block,
            span,
            ..
        } = event
        else {
            continue;
        };
        if au_references::extract_field_marker(info).is_none() {
            continue;
        }
        let Ok(docs) = au_parser::yaml::parse(block) else {
            continue;
        };
        let Some(doc) = docs.first() else {
            continue;
        };
        for raw in extract_block_type_claims(doc) {
            out.push((TypeNameClaim::parse(&raw, span), span));
        }
    }
    out
}

/// Every `::repo`-qualified `type:` claim inside a body typed block, INCLUDING
/// nested inline records within the block content, as `(name, repo)` fold seeds.
///
/// The body sibling of frontmatter's recursive `collect_value_seeds`. The block's
/// top `type:` claim alone is not enough, a nested inline record `type: peer::repo`
/// inside the fence is also a claim that must fold, else it under-validates
/// (silently skips) exactly like an unfolded frontmatter claim would.
/// NOT slot-gated, and cannot be. Seeds drive the cross-repo FOLD, and the
/// effective shape is a RESULT of that fold, so no slot is known here. The safe
/// direction is over-seeding: folding a peer that turns out unneeded costs a
/// mount, while under-seeding silently skips validation. So a text fence whose
/// content happens to parse as a `type: x::repo` mapping seeds a fold it did not
/// need, which is accepted.
pub fn collect_body_typed_block_seeds(body: &str) -> Vec<(TypeName, String)> {
    let mut out = Vec::new();
    for event in scan_body(body) {
        let BodyEvent::FencedBlock {
            info, body: block, ..
        } = event
        else {
            continue;
        };
        if au_references::extract_field_marker(info).is_none() {
            continue;
        }
        let Ok(docs) = au_parser::yaml::parse(block) else {
            continue;
        };
        if let Some(doc) = docs.first() {
            collect_yaml_repo_claims(doc, &mut out);
        }
    }
    out
}

/// Recurse a parsed YAML node collecting every `::repo`-qualified `type:` claim,
/// at this level and in every nested mapping / sequence value.
fn collect_yaml_repo_claims(
    node: &au_parser::yaml::MarkedYaml<'_>,
    out: &mut Vec<(TypeName, String)>,
) {
    use au_parser::yaml::{Scalar, YamlData};
    match &node.data {
        YamlData::Mapping(map) => {
            for (k, v) in map.iter() {
                if let YamlData::Value(Scalar::String(key)) = &k.data {
                    if key == "type" {
                        match &v.data {
                            YamlData::Value(Scalar::String(s)) => push_repo_claim(s, out),
                            YamlData::Sequence(items) => {
                                for it in items {
                                    if let YamlData::Value(Scalar::String(s)) = &it.data {
                                        push_repo_claim(s, out);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                collect_yaml_repo_claims(v, out);
            }
        }
        YamlData::Sequence(items) => {
            for it in items {
                collect_yaml_repo_claims(it, out);
            }
        }
        _ => {}
    }
}

/// Push a `type:` scalar's `::repo` claim, dropping a bare (own) claim.
fn push_repo_claim(raw: &str, out: &mut Vec<(TypeName, String)>) {
    let claim = TypeNameClaim::parse(raw, ByteRange::new(0, 0));
    if let Some(repo) = claim.repo {
        out.push((claim.name, repo));
    }
}

/// Extract the block's `type:` claim list — both bare (`type: foo`) and
/// mixin (`type: [a, b]`) forms.
pub(crate) fn extract_block_type_claims(doc: &au_parser::yaml::MarkedYaml<'_>) -> Vec<String> {
    use au_parser::yaml::{Scalar, YamlData};
    let YamlData::Mapping(map) = &doc.data else {
        return Vec::new();
    };
    for (k, v) in map.iter() {
        let YamlData::Value(Scalar::String(key)) = &k.data else {
            continue;
        };
        if key != "type" {
            continue;
        }
        match &v.data {
            YamlData::Value(Scalar::String(s)) => return vec![s.to_string()],
            YamlData::Sequence(items) => {
                return items
                    .iter()
                    .filter_map(|item| match &item.data {
                        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect();
            }
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// `file*` and `any*` are the existence-only built-in reference targets: both
/// resolve by mere node existence, with no type-closure check
/// ([[type-def shape file::au-type-system]], [[type-def shape any::au-type-system]]). The frontmatter path
/// special-cases them in `validate_body_block_values`; the body-fill helpers
/// must too, otherwise a `file*` or `any*` body wikilink false-fires
/// `body-slot-shape-mismatch` since neither name is ever in a user closure.
fn is_existence_only_demand(required: &[String]) -> bool {
    required.iter().any(|r| r == "file" || r == "any")
}

fn target_type_satisfies(ctx: &ValidateContext<'_>, target: &str, required: &[String]) -> bool {
    if is_existence_only_demand(required) {
        return ctx.repo_index.resolve(target).is_ok();
    }
    // Resolve target via repo index.
    let resolved = match ctx.repo_index.resolve(target) {
        Ok(p) => p,
        Err(_) => return true, // target-missing is a separate diagnostic; don't double-fire.
    };
    let claims = match ctx.ref_data.claims(&resolved) {
        Some(c) => c,
        None => return true,
    };
    // Check each claim's closure directly for a required type, rather than
    // materializing the union as a `BTreeSet<String>`. Same predicate, no
    // per-member string clone.
    for claim in claims.iter() {
        let closure = crate::closure::closure_of(ctx.graph, claim);
        if closure
            .iter()
            .any(|c| required.iter().any(|r| c.as_str() == r.as_str()))
        {
            return true;
        }
    }
    false
}

/// Cross-repo body-slot satisfaction: resolve the `::repo` target in the named
/// repo and check its type against the slot by `(name, canonical-hash)` identity,
/// mirroring the frontmatter cross-repo check. Returns true (no mismatch) when no
/// resolver is wired, the target is unresolvable (the cross-repo existence pass
/// owns that), or the target is untyped (consistent with the repo-local body
/// leniency, which leaves an untyped target to its own diagnostic).
fn cross_repo_body_target_satisfies(
    ctx: &ValidateContext<'_>,
    source_path: &std::path::Path,
    repo: &str,
    target: &str,
    block_id: Option<&str>,
    required: &[String],
) -> bool {
    // `file*` / `any*` are satisfied by mere existence; the existence pass owns it.
    if is_existence_only_demand(required) {
        return true;
    }
    let Some(resolver) = ctx.cross_repo else {
        return true;
    };
    let Some(t) = resolver.resolve(source_path, repo, target) else {
        return true;
    };
    // Claims at the target: the block's `type:` for a `^block` reference, else
    // the target file's `type:` claim.
    let claims: Vec<TypeName> = match block_id {
        Some(id) => cross_repo_block_claims(ctx, &t.path, id),
        None => ctx
            .ref_data
            .claims(&t.path)
            .map(|c| c.to_vec())
            .unwrap_or_default(),
    };
    if claims.is_empty() {
        return true;
    }
    // Identity is over the target repo's graph, hashed against the source's copy.
    required.iter().any(|r| {
        crate::validate::target_closure_includes_cross_repo(ctx.graph, t.graph, &claims, r)
    })
}

/// The `type:` claims of a `^block-id` target in another repo, resolved against
/// the global record / body-source maps (both keyed by absolute path).
fn cross_repo_block_claims(
    ctx: &ValidateContext<'_>,
    target_path: &std::path::Path,
    block_id: &str,
) -> Vec<TypeName> {
    let records = ctx.ref_data.record_targets(target_path);
    if let Some(target) = records.as_ref().and_then(|targets| targets.get(block_id)) {
        return target.claims.clone();
    }
    let Some(target_body) = ctx.ref_data.body(target_path) else {
        return Vec::new();
    };
    let events = au_parser::scan_body(target_body);
    let Ok(resolved) = au_references::resolve_block_id(&events, block_id) else {
        return Vec::new();
    };
    let Ok(docs) = au_parser::yaml::parse(resolved.body) else {
        return Vec::new();
    };
    let Some(doc) = docs.first() else {
        return Vec::new();
    };
    extract_block_type_claims(doc)
        .into_iter()
        .map(TypeName)
        .collect()
}

// ----- malformed marker / wikilink checks -----

fn check_malformed_markers(
    instance: &Instance,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    for event in events {
        if let BodyEvent::InlineCode { content, span } = event {
            if !content.starts_with("[:") {
                continue;
            }
            // Well-formed `[:fieldName] value` is recognized as a contribution
            // in the provenance pipeline. Anything that starts with `[:` but
            // doesn't follow that shape is a malformed attribution attempt.
            if let Some(trimmed) = content.strip_prefix("[:") {
                if let Some(end) = trimmed.find(']') {
                    let field_name = trimmed[..end].trim();
                    let value_text = trimmed[end + 1..].trim();
                    // Check shape: field name non-empty AND no leading
                    // whitespace inside the brackets AND value non-empty.
                    let raw_field = &trimmed[..end];
                    let well_formed =
                        !field_name.is_empty() && !value_text.is_empty() && raw_field == field_name; // no inner whitespace
                    if well_formed {
                        continue;
                    }
                }
            }
            // Severity is Warning: `[:field]` (and other near-misses on the
            // canonical shape) IS reserved syntax in prose, but the author's
            // intent is ambiguous — bare `[:field]` is a common documentation
            // idiom referencing the field by name. Surface the noise without
            // failing validation.
            diags.push(Diagnostic {
                code: codes::MALFORMED_ATTRIBUTION_MARKER,
                severity: Severity::Warning,
                span: Span::new(instance.source_path.clone(), absolute(*span, body_byte_offset)),
                message: format!(
                    "inline code `{}` starts with `[:` but does not match the `[:fieldName] value` form",
                    content
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

/// Whether `anchor` matches a heading in the target, per the
/// [[type reference::au-type-system]] anchor-matching contract (`au_references::resolve_anchor`).
/// `None` when the engine cannot verify — no held body and the target
/// isn't a yaml-only instance (a plain note or asset may carry the
/// heading; a false warning is worse than a missed one). A yaml-only
/// instance has no headings, so the anchor is verifiably absent.
pub(crate) fn anchor_exists_in(
    ctx: &ValidateContext<'_>,
    target_path: &std::path::Path,
    anchor: &str,
) -> Option<bool> {
    match ctx.ref_data.body(target_path) {
        Some(body) => {
            let events = scan_body(body);
            Some(au_references::resolve_anchor(&events, anchor).is_some())
        }
        None => {
            if ctx.ref_data.claims(target_path).is_some() {
                Some(false)
            } else {
                None
            }
        }
    }
}

/// Surface dangling navigational wikilinks in body prose, as warnings —
/// a missing file target, or a `^block-id` absent from both of the
/// target's addressable surfaces ([[type block-id::au-type-system]]). Local forms
/// included: growth is the same story in the current file and across
/// files, so the severity is uniform.
///
/// Deliberately silent on:
/// - links with a `:field` fragment — contributions, the slot-gated
///   per-contribution path owns them.
/// - malformed links — `check_malformed_wikilinks` owns those.
/// - ambiguous targets — open question, decided when it bites.
/// - block-ids in targets whose body the engine doesn't hold (plain
///   notes, assets) — can't verify, a false warning is worse.
fn check_prose_dangling_links(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    for event in events {
        let BodyEvent::Wikilink { raw, span } = event else {
            continue;
        };
        let Ok(link) = au_references::parse_wikilink_inner(raw) else {
            continue;
        };
        if link.field.is_some() {
            continue;
        }
        // A `::repo` link is cross-repo; the engine's cross-repo layer resolves
        // it and owns its diagnostics, so repo-local dangling does not apply.
        if link.repo.is_some() {
            continue;
        }
        let diag_span = Span::new(
            instance.source_path.clone(),
            absolute(*span, body_byte_offset),
        );
        check_navigational_target(ctx, &instance.source_path, &link, raw, diag_span, diags);
    }
}

/// Emit the navigational dangling diagnostics for one parsed wikilink at a
/// file span: a missing target (`navigational-target-not-found`), a missing
/// anchor (`anchor-not-found`), a missing block-id
/// (`navigational-block-id-not-found`). All warnings, never errors.
///
/// Shared by body prose and frontmatter value links — a `[[...]]` is
/// navigational wherever it sits, see [[type reference::au-type-system]]. The slot-side
/// validated reference keeps its own error path.
fn check_navigational_target(
    ctx: &ValidateContext<'_>,
    source_path: &Path,
    link: &au_references::WikilinkRef,
    raw: &str,
    diag_span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    // A commit-referent (`[[::@sha]]` / `[[::repo@sha]]`) names a COMMIT, not a
    // file: no target to resolve and never dangling, so it emits no navigational
    // diagnostic. Anchor-only, see [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    if link.is_commit_referent() {
        return;
    }
    // Local form: the host file, no name lookup.
    let target_path = if link.is_local() {
        source_path.to_path_buf()
    } else {
        match ctx.repo_index.resolve(&link.target) {
            Ok(p) => p,
            Err(au_references::ResolutionError::Missing) => {
                // A target that LEAVES the repo is not the open-world case a
                // dangling prose link is: it contradicts its own scope and no
                // later authoring makes it resolve. Same severity, sharper code
                // and a fix, so it does not sit in the not-yet-written bucket.
                let escapes = au_references::target_escapes_repo(&link.target);
                diags.push(Diagnostic {
                    code: if escapes {
                        au_references::codes::REFERENCE_PATH_ESCAPES_REPO
                    } else {
                        codes::NAVIGATIONAL_TARGET_NOT_FOUND
                    },
                    severity: Severity::Warning,
                    span: diag_span,
                    message: if escapes {
                        format!(
                            "wikilink `[[{raw}]]` names a path that leaves this repo; a wikilink \
                             is repo-scoped, so it can never resolve"
                        )
                    } else {
                        format!("wikilink `[[{raw}]]` resolves to no repo file")
                    },
                    related: vec![],
                    fix: escapes.then(|| SuggestedFix {
                        description: au_references::ESCAPES_REPO_FIX.to_string(),
                    }),
                });
                return;
            }
            Err(au_references::ResolutionError::Ambiguous(matches)) => {
                // Symmetric with the Missing arm above: a navigational link
                // resolving to TWO OR MORE files lands on too much rather than
                // nothing, so the author is told, advisory. The validated twin
                // `reference-target-ambiguous` is an error; here it is a warning,
                // the same open-world stance as `navigational-target-not-found`.
                diags.push(Diagnostic {
                    code: codes::NAVIGATIONAL_TARGET_AMBIGUOUS,
                    severity: Severity::Warning,
                    span: diag_span,
                    message: format!(
                        "wikilink `[[{raw}]]` resolves ambiguously to {} repo files",
                        matches.len()
                    ),
                    related: matches.iter().map(|p| Span::for_file(p.clone())).collect(),
                    fix: Some(SuggestedFix {
                        description: "resolve by explicit extension, explicit path, or rename"
                            .to_string(),
                    }),
                });
                return;
            }
        }
    };
    let target_label = if link.is_local() {
        "this file".to_string()
    } else {
        link.target.clone()
    };
    // Anchor existence per the [[type reference::au-type-system]] matching contract,
    // independent of the block-id fragment.
    if let Some(anchor) = link.anchor.as_deref() {
        if anchor_exists_in(ctx, &target_path, anchor) == Some(false) {
            diags.push(Diagnostic {
                code: codes::ANCHOR_NOT_FOUND,
                severity: Severity::Warning,
                span: diag_span.clone(),
                message: format!(
                    "wikilink `[[{raw}]]` — heading `{anchor}` not found in {target_label}"
                ),
                related: vec![],
                fix: None,
            });
        }
    }
    let Some(block_id) = link.block_id_str() else {
        return;
    };
    // Navigation accepts any occurrence: record `^:` ids, bare
    // markers, fence ids — typed or not.
    if ctx
        .ref_data
        .record_targets(&target_path)
        .is_some_and(|targets| targets.contains_key(block_id))
    {
        return;
    }
    match ctx.ref_data.body(&target_path) {
        Some(target_body) => {
            let target_events = scan_body(target_body);
            if !matches!(
                resolve_block_id(&target_events, block_id),
                Err(BlockResolutionError::NotFound)
            ) {
                return; // exists, typed or navigational
            }
        }
        None => {
            // No body held. A yaml-only instance truly cannot carry
            // the id; anything else (note, asset) is unverifiable.
            if !ctx.ref_data.claims(&target_path).is_some() {
                return;
            }
        }
    }
    diags.push(Diagnostic {
        code: codes::NAVIGATIONAL_BLOCK_ID_NOT_FOUND,
        severity: Severity::Warning,
        span: diag_span,
        message: format!(
            "wikilink `[[{raw}]]` — block-id `{block_id}` not found in {target_label}"
        ),
        related: vec![],
        fix: None,
    });
}

/// Navigational dangling / ambiguous diagnostics for `[[...]]` in a file's `#:`
/// docstrings, on a type-def or an instance. A docstring link is navigational
/// only, so it warns and never errors, the same stance and the same
/// `check_navigational_target` path as a body prose link. The `:field` and `^^`
/// fragments carry no docstring meaning and are ignored by the resolution.
/// Spans are already file-absolute, so no shift. See
/// [[spec - docstring navigational links - a docstring's wikilinks resolve as navigational edges tagged to their declaration]].
pub fn validate_docstring_links(
    ctx: &ValidateContext<'_>,
    source_path: &Path,
    doc_links: &[crate::instance::DocstringLink],
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for dl in doc_links {
        let Ok(link) = au_references::parse_wikilink_inner(&dl.link.raw) else {
            continue;
        };
        // A `::repo` link is cross-repo; the engine's cross-repo layer resolves
        // it and owns its diagnostics, as with the prose pass.
        if link.repo.is_some() {
            continue;
        }
        let diag_span = Span::new(source_path.to_path_buf(), dl.link.span);
        check_navigational_target(ctx, source_path, &link, &dl.link.raw, diag_span, &mut diags);
    }
    diags
}

/// Navigational dangling diagnostics for wikilinks embedded in frontmatter
/// string values, descending into sequences and inline records. Mirrors the
/// prose pass: a value link is navigational, never a validated reference, so
/// a dangling one warns and never errors.
fn check_frontmatter_navigational_links(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    shape: Option<&EffectiveShape>,
    diags: &mut Vec<Diagnostic>,
) {
    for field in &instance.fields {
        let slot = top_level_field_shape(shape, &field.key);
        walk_value_navigational_links(
            ctx,
            &instance.source_path,
            &field.value,
            &field.nav_links,
            slot,
            diags,
        );
    }
}

/// The declared shape of a top-level instance field, looked up in the
/// effective shape by base name (prefix-stripped). `None` when the field is
/// an extra, hits a mixin collision, or the type is unknown — the walk then
/// scans the value without shape context, the prior behaviour.
fn top_level_field_shape<'a>(shape: Option<&'a EffectiveShape>, key: &str) -> Option<&'a Shape> {
    shape
        .and_then(|s| s.get(&FieldName(base_field_name(key).to_string())))
        .and_then(|fo| fo.canonical_decl().parsed_shape.as_ref().ok())
}

fn walk_value_navigational_links(
    ctx: &ValidateContext<'_>,
    source_path: &Path,
    value: &InstanceValue,
    nav_links: &[NavLink],
    slot: Option<&Shape>,
    diags: &mut Vec<Diagnostic>,
) {
    // An `opaque` slot ([[type-def shape opaque::au-type-system]]) holds content whose
    // `[[...]]`-shaped strings are uninterpreted data, so no navigational scan
    // descends into it. The interpreted top `any` is NOT skipped: its content is
    // scanned like any other value. The shape threads through records and lists
    // below, so a nested `opaque` field is skipped the same way.
    //
    // KNOWN LIMITATION: this suppresses the navigational DIAGNOSTIC only. The
    // backlink EDGE index is shape-blind, so an `opaque` field's `[[...]]` still
    // forms an edge and is still rename-rewritten, the deferred fourth axis, see
    // [[type-def shape opaque::au-type-system]].
    if is_opaque_slot(slot) {
        return;
    }
    match value {
        InstanceValue::String(s) => {
            // A whole-value `[[...]]` is the validated-or-reference case: the
            // slot's own checks own its diagnostics (a dangling reference is
            // an error, not a warning). Only links embedded in a longer
            // string are navigational-only and warn here.
            if au_references::parse_wikilink(s).is_ok() {
                return;
            }
            for nl in nav_links {
                let Ok(link) = au_references::parse_wikilink_inner(&nl.raw) else {
                    continue;
                };
                // Cross-repo links are resolved by the engine's cross-repo
                // layer, not repo-local here.
                if link.repo.is_some() {
                    continue;
                }
                let diag_span = Span::new(source_path.to_path_buf(), nl.span);
                check_navigational_target(ctx, source_path, &link, &nl.raw, diag_span, diags);
            }
        }
        InstanceValue::Sequence(elems) => {
            let inner = slot.and_then(sequence_element_shape);
            for e in elems {
                walk_value_navigational_links(
                    ctx,
                    source_path,
                    &e.value,
                    &e.nav_links,
                    inner,
                    diags,
                );
            }
        }
        InstanceValue::Mapping(inline) => {
            let field_shapes = mapping_field_shapes(ctx.graph, slot, inline);
            for f in &inline.fields {
                let fslot = field_shapes.as_ref().and_then(|m| {
                    m.get(&FieldName(base_field_name(&f.key).to_string()))
                        .copied()
                });
                walk_value_navigational_links(
                    ctx,
                    source_path,
                    &f.value,
                    &f.nav_links,
                    fslot,
                    diags,
                );
            }
        }
        _ => {}
    }
}

/// Field-name → declared-shape map for an inline record's own fields, so the
/// nav walk threads shapes one level deeper and skips a nested `any` field.
/// Resolves the record's identity the way the candidate scan does: the inline
/// value's explicit `type:` claim wins, else the slot's demanded record type.
/// `None` when neither resolves — the walk scans the nested fields without
/// shape context, the prior behaviour.
fn mapping_field_shapes<'a>(
    graph: &'a TypeGraph,
    slot: Option<&Shape>,
    inline: &InlineValue,
) -> Option<BTreeMap<FieldName, &'a Shape>> {
    if let Some(claim) = &inline.type_claim {
        return Some(build_field_shape_map(graph, &claim_closure(graph, claim)));
    }
    let demanded = slot
        .and_then(record_demanded_type)
        .filter(|t| graph.contains(t))?;
    Some(build_field_shape_map(graph, &closure_of(graph, &demanded)))
}

fn check_malformed_wikilinks(
    instance: &Instance,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    for event in events {
        if let BodyEvent::Wikilink { raw, span } = event {
            match au_references::parse_wikilink_inner(raw) {
                Ok(_) => {}
                Err(WikilinkParseError::FieldOutOfOrder) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_FRAGMENT_ORDER,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!(
                            "wikilink `[[{raw}]]` violates the strict `name / #head / ^block-id / :field` order"
                        ),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::InvalidFieldName) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_INVALID_FIELD_NAME,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!(
                            "wikilink `[[{raw}]]` has an invalid `:field` name (must match the field-name regex)"
                        ),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::EmptyField) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_EMPTY_FIELD,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!("wikilink `[[{}]]` ends with `:` but no field value", raw),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::EmptyRepo) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_EMPTY_REPO,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!("wikilink `[[{raw}]]` ends with `::` but no repo value"),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::EmptyCommit) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_EMPTY_COMMIT,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!("wikilink `[[{raw}]]` has `@` but no commit value"),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::CommitNotOid) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::PINNED_COMMIT_NOT_OID,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!(
                            "wikilink `[[{raw}]]` pins a non-oid commit; a pin's commit must be an immutable oid, not a branch, tag, or relative rev"
                        ),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::RepoOutOfOrder) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_FRAGMENT_ORDER,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!(
                            "wikilink `[[{raw}]]` has its `::repo` qualifier out of order; canonical order is `name ::repo #anchor ^block-id :field`, at most one `::repo`"
                        ),
                        related: vec![],
                        fix: None,
                    });
                }
                Err(WikilinkParseError::InvalidRepoName) => {
                    diags.push(Diagnostic {
                        code: au_references::codes::WIKILINK_INVALID_REPO_NAME,
                        severity: Severity::Error,
                        span: Span::new(
                            instance.source_path.clone(),
                            absolute(*span, body_byte_offset),
                        ),
                        message: format!(
                            "wikilink `[[{raw}]]` has an invalid `::repo` name (must match the repo-name regex)"
                        ),
                        related: vec![],
                        fix: None,
                    });
                }
                // Other parse errors fall through silently — they're
                // covered by the existing frontmatter wikilink validation
                // for frontmatter occurrences, and body navigational links
                // are open-world per [[type open-world validation::au-type-system]].
                Err(_) => {}
            }
        }
    }
}

/// Surface `block-id-duplicate` when two or more blocks in this
/// instance share the same `^block-id`. `resolve_block_id` returns the
/// first match and silently ignores duplicates — the diagnostic gives
/// authors the chance to disambiguate.
///
/// One namespace per file ([[type block-id::au-type-system]]). Sources counted: fenced
/// blocks with `trailing_block_id`, bare `BlockIdMarker` events, AND
/// inline-record `^:` ids from the frontmatter. Same-id occurrences
/// across any of the surfaces count toward the duplication.
fn check_duplicate_block_ids(
    instance: &Instance,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    // Spans collect file-absolute: frontmatter `^:` spans already are
    // (instance parse applies the yaml offset), body spans shift here.
    let mut record_ids: Vec<(&str, ByteRange)> = Vec::new();
    for field in &instance.fields {
        collect_record_block_ids(&field.value, &mut record_ids);
    }
    let mut by_id: BTreeMap<&str, Vec<ByteRange>> = BTreeMap::new();
    for (id, span) in record_ids {
        by_id.entry(id).or_default().push(span);
    }
    for event in events {
        match event {
            BodyEvent::FencedBlock {
                trailing_block_id: Some(id),
                span,
                ..
            } => {
                by_id
                    .entry(id)
                    .or_default()
                    .push(absolute(*span, body_byte_offset));
            }
            BodyEvent::BlockIdMarker { id, span } => {
                by_id
                    .entry(id)
                    .or_default()
                    .push(absolute(*span, body_byte_offset));
            }
            _ => {}
        }
    }
    for (id, mut spans) in by_id {
        if spans.len() < 2 {
            continue;
        }
        // Anchor at the first occurrence in document order; every later
        // occurrence into related[]. Frontmatter precedes the body, the
        // sort keeps that true across surfaces.
        spans.sort_by_key(|s| s.start);
        let related: Vec<Span> = spans[1..]
            .iter()
            .map(|s| Span::new(instance.source_path.clone(), *s))
            .collect();
        diags.push(Diagnostic {
            code: codes::BLOCK_ID_DUPLICATE,
            severity: Severity::Error,
            span: Span::new(instance.source_path.clone(), spans[0]),
            message: format!(
                "block-id `^{id}` appears {} times in this file; references resolve to the first only — disambiguate",
                spans.len()
            ),
            related,
            fix: None,
        });
    }
}

/// Collect every inline-record `^:` id under a frontmatter value,
/// depth-first in document order. Spans are file-absolute already.
fn collect_record_block_ids<'a>(value: &'a InstanceValue, out: &mut Vec<(&'a str, ByteRange)>) {
    match value {
        InstanceValue::Mapping(inline) => {
            if let Some(decl) = &inline.block_id {
                out.push((decl.id.as_str(), decl.value_span));
            }
            for field in &inline.fields {
                collect_record_block_ids(&field.value, out);
            }
        }
        InstanceValue::Sequence(elements) => {
            for el in elements {
                collect_record_block_ids(&el.value, out);
            }
        }
        _ => {}
    }
}

/// Surface `body-unterminated-fence` for every `UnterminatedFenceOpen`
/// event the parser emitted. Each event's span covers only the
/// fence-open line; subsequent lines parsed normally per the
/// parser's recovery semantics.
fn check_unterminated_fences(
    instance: &Instance,
    events: &[BodyEvent<'_>],
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    for event in events {
        if let BodyEvent::UnterminatedFenceOpen { info, span } = event {
            let info_label = if info.trim().is_empty() {
                "<unspecified>".to_string()
            } else {
                info.trim().to_string()
            };
            diags.push(Diagnostic {
                code: codes::BODY_UNTERMINATED_FENCE,
                severity: Severity::Warning,
                span: Span::new(instance.source_path.clone(), absolute(*span, body_byte_offset)),
                message: format!(
                    "fenced code block opens (`{info_label}`) but has no matching closing fence later in the body"
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

fn absolute(span: ByteRange, offset: usize) -> ByteRange {
    ByteRange::new(span.start + offset, span.end + offset)
}

// ----- cross-file ^block-id resolution + marked-fence validation -----

/// Resolve a body-contribution block-id fragment against the repo, keyed on the
/// sigil MODE ([[type block-id::au-type-system]]), and push at most one diagnostic. Returns
/// whether the caller should SKIP the value claim check — there is no valid
/// referent to check, or the file is being anchored (not asserted).
///
/// - a `^^id` block-referent contributes the BLOCK's value: a typed block /
///   inline record resolves (keep the check), a plain block is
///   `block-id-not-typed` and an absent id is `block-id-not-found`, both errors
///   that skip.
/// - a bare `^id` contributes the FILE, `^id` a navigational anchor: an existing
///   (or unverifiable no-body) anchor keeps the file check, an absent one is a
///   `navigational-block-id-not-found` warning that skips (no file-type
///   assertion on a missing anchor, matching the frontmatter path).
///
/// Silent when the target file itself isn't in the repo (`target-missing` is a
/// separate diagnostic). The frontmatter side resolves inside the slot-gated
/// reference checks (validate.rs).
fn diagnose_block_id_resolution(
    ctx: &ValidateContext<'_>,
    source_path: &std::path::Path,
    span: ByteRange,
    target: &str,
    block: &au_references::BlockId,
    diags: &mut Vec<Diagnostic>,
) -> bool {
    let block_id = block.id.as_str();
    let referent = block.referent;
    // Local form ([[type reference::au-type-system]]): empty name means the host file.
    let target_path = if target.is_empty() {
        source_path.to_path_buf()
    } else {
        match ctx.repo_index.resolve(target) {
            Ok(p) => p,
            Err(_) => return false,
        }
    };
    // An addressable inline record satisfies existence outright, in either mode.
    if ctx
        .ref_data
        .record_targets(&target_path)
        .is_some_and(|targets| targets.contains_key(block_id))
    {
        return false;
    }
    let target_label = if target.is_empty() {
        "this file"
    } else {
        target
    };
    let caret = if referent { "^^" } else { "^" };
    let mut push = |code, severity, message: String| {
        diags.push(Diagnostic {
            code,
            severity,
            span: Span::new(source_path.to_path_buf(), span),
            message,
            related: vec![],
            fix: None,
        });
    };
    let Some(target_body) = ctx.ref_data.body(&target_path) else {
        // No body and no record with this id.
        return if referent {
            // `^^` demanded a value from a block that does not exist.
            push(
                codes::BLOCK_ID_NOT_FOUND,
                Severity::Error,
                format!("wikilink `[[{target}{caret}{block_id}]]` — block-id `{block_id}` not found in {target_label} (no body, no record id)"),
            );
            true
        } else {
            // Bare `^` over a no-body target: the anchor is unverifiable, so the
            // FILE is the value, silently. Keep the file check.
            false
        };
    };
    let target_events = scan_body(target_body);
    match resolve_block_id(&target_events, block_id) {
        // Exists on the body surface: `^^` keeps the block check, bare `^`
        // resolves the anchor and keeps the file check.
        Ok(_) => false,
        Err(BlockResolutionError::NotTyped) => {
            if referent {
                // `^^` demanded a typed value, the block has none.
                push(
                    codes::BLOCK_ID_NOT_TYPED,
                    Severity::Error,
                    format!("wikilink `[[{target}{caret}{block_id}]]` — block `{block_id}` exists but isn't typed (no `[:field]` fence)"),
                );
                true
            } else {
                // Bare `^` over a plain block: the anchor resolves, the FILE is
                // the value. Keep the file check.
                false
            }
        }
        Err(BlockResolutionError::NotFound) => {
            let (code, severity) = if referent {
                (codes::BLOCK_ID_NOT_FOUND, Severity::Error)
            } else {
                (codes::NAVIGATIONAL_BLOCK_ID_NOT_FOUND, Severity::Warning)
            };
            push(
                code,
                severity,
                format!("wikilink `[[{target}{caret}{block_id}]]` — block-id `{block_id}` not found in {target_label}"),
            );
            // A missing anchor is advisory growth, not a file-type assertion, so
            // skip the value check in both modes.
            true
        }
    }
}

/// Shape-check every SCALAR value that reached a field only through the body.
///
/// [[type-instance body contribution::au-type-system]]: "a contribution whose value misses the slot
/// shape is an error". The frontmatter pass cannot deliver this — it iterates
/// `instance.fields`, where a body-filled field is either absent or a null
/// anchor, and a null returns early as a promise rather than a value.
///
/// Scoped deliberately, so this adds a missing check without duplicating an
/// existing one:
/// - only `Scalar`. A `Reference` is checked by the wikilink pass below and an
///   `InlineRecord` by `check_marked_fences`, both with better-targeted messages.
/// - only containers carrying NO frontmatter contribution. Equal values collapse
///   across surfaces ([[type value container::au-type-system]]), so a container holding one was
///   already checked by the frontmatter pass, and re-checking would double-report.
/// - against the slot's ELEMENT shape, since one contribution is one element.
/// - only where the slot is KNOWN. An unresolved claim yields no shape, and an
///   out-of-shape field is a [[type extras::au-type-system]], advisory by the open-world stance.
fn check_body_scalar_shapes(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    shape: Option<&EffectiveShape>,
    values: &BTreeMap<FieldName, Vec<ValueContainer>>,
    diags: &mut Vec<Diagnostic>,
) {
    let Some(effective) = shape else {
        return;
    };
    for (field, containers) in values {
        let Some(field_origin) = effective.get(field) else {
            continue; // an extra, advisory
        };
        let decl = field_origin.canonical_decl();
        let Ok(parsed) = &decl.parsed_shape else {
            continue; // a malformed shape is already diagnosed at load
        };
        let element = au_grammar::slot_element_shape(parsed);
        let origin_path = field_origin.canonical_origin_path();
        // A REFERENCE-ONLY slot admits no inline form, and `check_marked_fences`
        // already owns that verdict for a fence, with a message naming the real
        // problem. The generic scalar check would pile a second error on top and
        // dump the whole fence body into it, so leave the fence to that owner.
        // An INLINE-MARKER scalar at the same slot has no other owner, so it
        // still routes through here.
        let fence_is_owned_elsewhere = !au_grammar::slot_admits_record(element)
            && au_grammar::slot_text_form(element).is_none();
        for container in containers {
            if container
                .contributions
                .iter()
                .any(|c| c.surface == Surface::Frontmatter)
            {
                continue;
            }
            if fence_is_owned_elsewhere
                && container
                    .contributions
                    .iter()
                    .all(|c| c.surface == Surface::BodyFence)
            {
                continue;
            }
            // Anchor at the first contribution, the earliest occurrence in
            // source order, so the squiggle lands where the value was authored.
            let Some(first) = container.contributions.first() else {
                continue;
            };
            // A body-originated reference (an inline-code contribution the
            // elaborator classified into a Reference / MalformedReference node) is
            // validated with the FRONTMATTER reference codes it carried before
            // classification, via the shared reference arms, so no verdict
            // re-parses the surface (B1, [[spec - value model - one typed value produced once in effective_values and consumed by validation and reads]]).
            // Prose references (`BodyWikilink`) stay with `check_per_contribution`
            // and its `body-slot-shape-mismatch`, so gate to containers with NO
            // prose-wikilink contribution.
            if matches!(
                container.value,
                ContributionValue::Reference { .. } | ContributionValue::MalformedReference(..)
            ) && !container
                .contributions
                .iter()
                .any(|c| c.surface == Surface::BodyWikilink)
            {
                diags.extend(crate::validate::check_body_reference_container(
                    ctx,
                    &instance.source_path,
                    &container.value,
                    container.brand.as_deref(),
                    first.location.byte_range,
                    element,
                    field.as_str(),
                    origin_path,
                    decl.shape_span,
                ));
                continue;
            }
            // The single value-verdict walker, over the typed model. A non-scalar
            // kind returns no verdict here today (references and inline records
            // are owned by their own passes), so routing every container through
            // it is behaviour-identical to the earlier scalar-only path.
            diags.extend(crate::validate::check_contribution_value(
                ctx,
                &instance.source_path,
                &container.value,
                container.brand.as_deref(),
                first.location.byte_range,
                element,
                field.as_str(),
                // A body contribution carries no list-element index (the message
                // names the field, not a position), unchanged from before.
                None,
                origin_path,
                decl.shape_span,
            ));
        }
    }
}

fn check_marked_fences(
    ctx: &ValidateContext<'_>,
    instance: &Instance,
    shape: Option<&EffectiveShape>,
    events: &[BodyEvent<'_>],
    body_source: &str,
    body_byte_offset: usize,
    diags: &mut Vec<Diagnostic>,
) {
    for event in events {
        let BodyEvent::FencedBlock {
            info, body, span, ..
        } = event
        else {
            continue;
        };
        let Some(marker_field) = au_references::extract_field_marker(info) else {
            // Non-`[:field]` fences are ordinary CommonMark code blocks;
            // skip embedded-record validation for them.
            continue;
        };
        // A marked fence reads by its SLOT ([[type-instance body contribution::au-type-system]]),
        // so only a fence that reads as a RECORD is validated as one. A text
        // fence — a `String` slot, an `opaque` slot, a union whose content took
        // the text branch — carries no record contract to check, and validating
        // it as one would report a bad record against authored prose.
        //
        // The predicate is shared with the value layer so the two surfaces can
        // never disagree about what a given fence is.
        // Resolve the field's declaration ONCE: the parsed slot, its authored
        // form for messages, and where it was declared. Every branch below needs
        // some of these, and repeating the lookup drifts (an earlier cut fell
        // back to an empty string, rendering as `slot ''`).
        let field_decl = shape.and_then(|es| {
            es.get(&FieldName(base_field_name(marker_field).to_string()))
                .map(|fo| (fo.canonical_decl(), fo.canonical_origin_path()))
        });
        let slot = field_decl.and_then(|(d, _)| d.parsed_shape.as_ref().ok());
        let slot_raw = field_decl.map(|(d, _)| d.raw_shape.as_str()).unwrap_or("");
        let decl_origin = field_decl.map(|(_, p)| p);
        let decl_shape_span = field_decl.map(|(d, _)| d.shape_span);
        // A REFERENCE-ONLY slot (`T*`, `file*`, `any*`, `type<T>*`, `<A | B>*`)
        // carries no inline form at all, so a fence cannot fill it
        // ([[type-instance body contribution::au-type-system]]). The value layer still captures the
        // content, so the contribution exists and this anchors to it, rather than
        // the fence being silently accepted as text the slot cannot hold.
        //
        // Only when the slot is KNOWN: an unresolved claim gives no shape, and a
        // guess there would fire against a file whose contract is not yet known.
        if let Some(s) = slot {
            if !au_grammar::slot_admits_record(s) && au_grammar::slot_text_form(s).is_none() {
                diags.push(Diagnostic {
                    code: codes::BODY_SLOT_SHAPE_MISMATCH,
                    severity: Severity::Error,
                    span: Span::new(
                        instance.source_path.clone(),
                        absolute(*span, body_byte_offset),
                    ),
                    message: format!(
                        "marked fence cannot fill slot '{slot_raw}' for field '{marker_field}' — the slot admits only a reference, which has no inline form"
                    ),
                    related: Vec::new(),
                    fix: Some(au_diagnostics::SuggestedFix {
                        description:
                            "contribute a wikilink instead (`[[target:field]]`), or widen the slot to admit an inline value"
                                .to_string(),
                    }),
                });
                continue;
            }
        }
        if !crate::provenance::fence_reads_as_record(body, slot) {
            continue;
        }
        // A compound slot admitting BOTH forms had no single answer, so the
        // CONTENT chose the record branch. If that record then fails, the author
        // may have meant text and written prose whose leading line parses as a
        // `type:` key — a cause the failure itself cannot name.
        let slot_admits_both = slot.is_some_and(|s| {
            au_grammar::slot_admits_record(s) && au_grammar::slot_text_form(s).is_some()
        });
        // Parse the fence body into an inline record with file-absolute spans,
        // then validate it as an inline value of the type it claims — the same
        // resolution-aware, recursive path frontmatter inline records use. A
        // `::repo` block claim resolves against the folded peer shape, and
        // nested inline records inside the fence validate too.
        let fence_span = absolute(*span, body_byte_offset);
        let base_offset = body_byte_offset + offset_in(body_source, body);
        let Some((inline, _doc_links)) =
            crate::instance::parse_block_record(&instance.source_path, body, base_offset)
        else {
            // The slot demands a record (we passed the reference-only check and
            // `fence_reads_as_record` was true), but the content is not a mapping
            // — plain prose, or not valid YAML at all. The value layer captured
            // it, so it exists on the wire as an `inline_record` holding a scalar,
            // a kind the slot never admitted. Report it rather than continue in
            // silence. The third `body-slot-shape-mismatch` shape, beside the
            // reference-only one.
            diags.push(Diagnostic {
                code: codes::BODY_SLOT_SHAPE_MISMATCH,
                severity: Severity::Error,
                span: Span::new(instance.source_path.clone(), fence_span),
                message: format!(
                    "marked fence for '{marker_field}' does not fill record slot '{slot_raw}' — the content is not a record (needs a `type:` and fields)"
                ),
                related: Vec::new(),
                fix: Some(au_diagnostics::SuggestedFix {
                    description:
                        "write the fence as a YAML record with a `type:`, or change the slot to `String` for prose"
                            .to_string(),
                }),
            });
            continue;
        };
        // The block declares its own type via `type:`. Mixin `type: [a, b]`
        // inside a fence isn't a contemplated use, so render the first claim.
        //
        // A claim-LESS mapping is only meaningful at a slot that pins one type;
        // there the slot supplies the identity, exactly as it does for a
        // frontmatter inline record. At a union or a non-claimable ceiling the
        // frontmatter path fires `inline-value-missing-type`, and this path used
        // to `continue` in silence.
        let claim = match &inline.type_claim {
            Some(c) => c.clone(),
            None => match slot.map(au_grammar::slot_element_shape) {
                // Slot-pinned: synthesize the slot's own identity, as frontmatter does.
                Some(Shape::Record(n)) | Some(Shape::InlineOrReference(n)) => {
                    TypeClaim::Bare(crate::typedef::TypeNameClaim::own(
                        TypeName(n.as_str().to_string()),
                        fence_span,
                    ))
                }
                // Any other record-bearing slot cannot supply one identity, so
                // the author must declare it.
                Some(other) if au_grammar::slot_admits_record(other) => {
                    diags.push(Diagnostic {
                        code: codes::INLINE_VALUE_MISSING_TYPE,
                        severity: Severity::Error,
                        span: Span::new(instance.source_path.clone(), fence_span),
                        message: format!(
                            "marked fence for '{marker_field}' must declare `type:` identifying which branch of slot '{slot_raw}' the record satisfies"
                        ),
                        related: Vec::new(),
                        fix: None,
                    });
                    continue;
                }
                _ => continue,
            },
        };
        let claim = &claim;
        let claimed_type = claim
            .iter()
            .next()
            .map(|c| match &c.repo {
                Some(r) => format!("{}::{}", c.name.as_str(), r),
                None => c.name.as_str().to_string(),
            })
            .unwrap_or_default();
        // Resolution-aware effective shape of the block's claimed type. An
        // unresolvable claim (an unknown own type, or a `::repo` the gate owns)
        // contributes nothing.
        let shape = match crate::validate::effective_shape_for(ctx, claim) {
            Ok(s) => s,
            Err(err) => {
                // An unresolvable claim is a REAL failure and must be reported as
                // one. Every other `UnknownType` site in the validator emits
                // `unknown-type-claim`; this path used to emit nothing, so a
                // fence claiming a type that does not exist produced at most a
                // `hint`, and a consumer filtering hints saw silence.
                let EffectiveShapeError::UnknownType(name) = &err;
                diags.push(Diagnostic {
                    code: codes::UNKNOWN_TYPE_CLAIM,
                    severity: Severity::Error,
                    span: Span::new(
                        instance.source_path.clone(),
                        absolute(*span, body_byte_offset),
                    ),
                    message: format!(
                        "marked fence claims type '{}' which is not present in the type graph",
                        name.as_str()
                    ),
                    related: Vec::new(),
                    fix: None,
                });
                // The hint names the likely CAUSE of that error, and now rides
                // alongside it as its own spec entry promises.
                if slot_admits_both {
                    diags.push(fence_read_as_record_hint(
                        instance,
                        marker_field,
                        &claimed_type,
                        absolute(*span, body_byte_offset),
                    ));
                }
                continue;
            }
        };
        // Does the claim satisfy the SLOT? The frontmatter surface has always
        // checked this; the fence surface did not, so a union slot accepted a
        // record whose type it never admitted. Runs BEFORE the own-contract
        // checks: without a compatible identity those only add noise.
        if let Some(s) = slot {
            let incompatible = crate::validate::check_fence_record_against_slot(
                ctx,
                &instance.source_path,
                &shape,
                claim,
                au_grammar::slot_element_shape(s),
                marker_field,
                decl_origin.unwrap_or(&instance.source_path),
                decl_shape_span.unwrap_or(ByteRange::new(0, 0)),
            );
            if !incompatible.is_empty() {
                diags.extend(incompatible);
                continue;
            }
        }

        // Required-fields-present on the block's keys. Collect every missing
        // required field's declaration span for `related[]` so the author can
        // jump from the embedded block to the missing field's declaration site.
        let block_keys: BTreeSet<String> = inline.fields.iter().map(|f| f.key.clone()).collect();
        let mut missing_field_spans: Vec<Span> = Vec::new();
        let mut missing_field_names: Vec<String> = Vec::new();
        for (field_name, field_origin) in shape.iter() {
            if field_origin.canonical_decl().parsed_shape.is_err() {
                continue;
            }
            // Required at ANY origin makes the field required (auto-unify
            // collapses shapes, not optional-ness), mirroring the per-origin
            // frontmatter check. Point related[] at a required origin's decl,
            // the canonical (lex-min) origin may be the optional one under a
            // mixin.
            let Some((_, req)) = field_origin.origins().find(|(_, info)| !info.decl.optional)
            else {
                continue; // optional at every origin
            };
            if !block_keys.contains(field_name.as_str()) {
                missing_field_spans.push(Span::new(req.origin_path.clone(), req.decl.name_span));
                missing_field_names.push(field_name.as_str().to_string());
            }
        }
        let missing_field_spans_was_empty = missing_field_spans.is_empty();
        if !missing_field_spans.is_empty() {
            let names_list = missing_field_names
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ");
            diags.push(Diagnostic {
                code: codes::EMBEDDED_RECORD_VALIDATION_FAILURE,
                severity: Severity::Error,
                span: Span::new(instance.source_path.clone(), absolute(*span, body_byte_offset)),
                message: format!(
                    "embedded record (`type: {claimed_type}`) fails its type's contract — required field(s) absent: {names_list}"
                ),
                related: missing_field_spans,
                fix: None,
            });
        }
        // Per-field value-conformance (recursive, resolution-aware) — the half
        // the shallow presence check omitted, so a nested `type: peer::repo`
        // record's own contract is checked like a frontmatter inline record.
        let before_values = diags.len();
        diags.extend(crate::validate::validate_body_block_values(
            ctx,
            &instance.source_path,
            &inline,
            &shape,
        ));
        let record_failed = !missing_field_spans_was_empty || diags.len() > before_values;
        if slot_admits_both && record_failed {
            diags.push(fence_read_as_record_hint(
                instance,
                marker_field,
                &claimed_type,
                absolute(*span, body_byte_offset),
            ));
        }
    }
}

/// The [[spec - diagnostic codes::au-type-system^body-fence-read-as-record]] hint. Rides alongside
/// the record failure, never replaces it.
fn fence_read_as_record_hint(
    instance: &Instance,
    field: &str,
    claimed_type: &str,
    span: ByteRange,
) -> Diagnostic {
    Diagnostic {
        code: codes::BODY_FENCE_READ_AS_RECORD,
        severity: Severity::Hint,
        span: Span::new(instance.source_path.clone(), span),
        message: format!(
            "marked fence for '{field}' read as the record branch of its union slot, because the content parses as a mapping declaring `type: {claimed_type}`; if it was meant as text, reword it so the content does not parse as a yaml mapping"
        ),
        related: Vec::new(),
        fix: Some(au_diagnostics::SuggestedFix {
            description:
                "reword the content so it does not parse as a yaml mapping, or correct the record"
                    .to_string(),
        }),
    }
}

/// The byte offset of a sub-slice within its parent string. Both share the
/// backing buffer, as `scan_body`'s slices do.
fn offset_in(parent: &str, child: &str) -> usize {
    child.as_ptr() as usize - parent.as_ptr() as usize
}
