//! Load-time checks against an immutable `TypeGraph`.
//!
//! Graph-structure checks:
//! - type-name regex
//! - cycle in `type:` chains
//! - duplicate `meta:` sub-regions for the same named type-def on one host
//!
//! Inheritance checks: field-redeclaration (transitive) and sealed-family
//! reachability.
//!
//! Reserved-key-location for instances (`fields:` / `sealed:` / `meta:`
//! cannot appear on instance frontmatter) lives in `instance.rs` since the
//! check needs the instance parser.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span};
use au_grammar::{CompoundRefOp, Primitive, Shape};

use crate::closure::{
    closure_of, collect_referenced_type_names, effective_shape, effective_shape_resolved,
    folded_closure_ids, EffectiveShapeError,
};
use crate::codes;
use crate::graph::TypeGraph;
use crate::instance::TypeClaim;
use crate::location::{FileType, LocationSpec, NameSegment};
use crate::resolution::ResolutionGraph;
use crate::typedef::{FieldDecl, FieldName, TypeDef, TypeName, TypeNameClaim};
use crate::validate::CrossRepoResolver;

/// Primitive shape names (spec [[type-def shape primitive::au-type-system]]). Reserved as type-def names — claiming
/// one as a user type-def's name is a load error. Also referenced by
/// `primitive_typo_hint` for the lowercase-typo case in slot references.
const PRIMITIVE_NAMES: &[&str] = &["String", "Number", "Boolean", "Date", "DateTime", "Url"];

/// Built-in reference targets ([[type-def shape file::au-type-system]]) that resolve at validate time
/// without a corresponding type-def. Reserved as type-def names; slot-
/// reference graph-presence checks skip these too.
const BUILT_IN_REFERENCE_TARGETS: &[&str] = &["file", "any"];

/// Run every graph-structure load check against `graph`. Returns the flat
/// list of diagnostics. Order is deterministic across runs.
pub fn run_graph_structure_checks(graph: &TypeGraph) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (_, td) in graph.iter() {
        check_type_name_regex(td, &mut diags);
        check_brand_xor_record(td, &mut diags);
        check_duplicate_meta(td, &mut diags);
        check_reserved_type_name(td, &mut diags);
        check_parent_reference_targets(td, graph, &mut diags);
        check_required_meta_targets(td, graph, &mut diags);
        check_slot_reference_targets(td, graph, &mut diags);
        check_brand_not_referenceable(td, graph, &mut diags);
        check_slot_union_subsumption(td, graph, &mut diags);
        check_slot_intersection_subsumption(td, graph, &mut diags);
        check_slot_cardinality_subsumption(td, &mut diags);
        check_refinement_unsatisfiable(td, &mut diags);
        check_refinement_load_errors(td, &mut diags);
    }
    check_cycles(graph, &mut diags);
    for (_, td) in graph.iter() {
        check_redundant_abstract_on_sealed(graph, td, &mut diags);
    }
    diags
}

/// `redundant-abstract-on-sealed`: a sealed type also declaring `abstract:
/// true`. Sealed is abstract plus a closed branch set, so it already carries
/// non-claimability and the explicit marker adds nothing. See
/// [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
///
/// There is deliberately no "abstract type with no concrete descendant" check.
/// An abstract interface with no local subtype is a legitimate cross-repo pattern
/// (the subtype lives in another repo, or is authored later), not a defect.
fn check_redundant_abstract_on_sealed(
    graph: &TypeGraph,
    td: &TypeDef,
    diags: &mut Vec<Diagnostic>,
) {
    if td.declared_abstract && graph.is_sealed(&td.name) {
        diags.push(Diagnostic {
            code: codes::REDUNDANT_ABSTRACT_ON_SEALED,
            severity: Severity::Hint,
            span: Span::new(td.source_path.clone(), td.source_span),
            message: format!(
                "type-def '{}' is sealed, which already implies abstract; the explicit `abstract: true` is redundant",
                td.name.as_str()
            ),
            related: vec![],
            fix: None,
        });
    }
}

/// `brand-with-record-keys`: a type-def declares both `shape:` (a brand) and a
/// record key (`fields:` / `sealed:` / `body:`). A def is a brand XOR a record,
/// a brand names an underlying shape and has no fields. See
/// [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
fn check_brand_xor_record(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    if td.shape.is_none() {
        return;
    }
    let mut conflicts = Vec::new();
    if !td.fields.is_empty() {
        conflicts.push("fields");
    }
    if !td.sealed.is_empty() {
        conflicts.push("sealed");
    }
    if crate::body::is_body_declaring(td.body.as_ref()) {
        conflicts.push("body");
    }
    if conflicts.is_empty() {
        return;
    }
    diags.push(Diagnostic {
        code: codes::BRAND_WITH_RECORD_KEYS,
        severity: Severity::Error,
        span: Span::new(td.source_path.clone(), td.source_span),
        message: format!(
            "type-def '{}' declares both `shape:` (a brand) and record key(s) ({}); a def is a brand XOR a record",
            td.name.as_str(),
            conflicts
                .iter()
                .map(|k| format!("`{k}:`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        related: vec![],
        fix: Some(au_diagnostics::SuggestedFix {
            description: "keep `shape:` for a brand, or the record keys for a record, not both"
                .to_string(),
        }),
    });
}

/// Inheritance checks layered on top of the graph: redeclaration and
/// sealed-family reachability.
///
/// "Inherited-field removal" is listed in [[type validation::au-type-system]] as a load error but is
/// structurally moot in V1 — there is no syntactic mechanism for a subtype to
/// remove an inherited field, so the constraint is enforced by construction.
/// If a removal mechanism is introduced later, it lands here.
pub fn run_inheritance_checks(graph: &TypeGraph) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (_, td) in graph.iter() {
        check_field_redeclaration(graph, &td.name, &mut diags);
        check_sealed_reachability(graph, &td.name, &mut diags);
        check_parent_redundant_claims(graph, td, &mut diags);
    }
    diags
}

/// Body-typing load checks ([[type-def body use::au-type-system]], [[type-def body fills::au-type-system]]): every `use: T` target is in
/// the host's closure, splice graphs are acyclic, fills bind only to in-closure
/// fields that exist, nested fills don't conflict with
/// ancestor exclusivity.
pub fn run_body_typing_checks(graph: &TypeGraph) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (_, td) in graph.iter() {
        check_body_use_in_closure(graph, td, &mut diags);
        check_body_fills(graph, td, &mut diags);
    }
    check_body_use_cycles(graph, &mut diags);
    diags
}

/// Location-constraint load checks ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]):
/// a `name` template references only a REQUIRED SAFE SCALAR field of the type's
/// effective shape, and `fileType: yaml` does not sit beside a non-empty
/// `body:`. The block's structural parse fired `location-bad-shape` earlier;
/// these two need the type's effective FIELDS and BODY.
///
/// Both run over the FOLDED closure when `resolution` / `peer` are present, so a
/// subtype's own `location` over a field or body inherited from a cross-repo
/// parent (or spliced from a cross-repo `use:`) resolves the peer's contribution,
/// the parity of the per-instance side ([[crates/au-core/src/location_check.rs]]).
/// Absent (the builtin schema, a non-importing repo, a direct au-core test), they
/// run over the own graph, where a `::repo` claim cannot resolve anyway.
///
/// Runs POST-FOLD, so it does not gate the repo's abort: a broken `location`
/// block is an advisory-placement defect, and suppressing every instance's
/// validation over a filing-convention typo is disproportionate, per
/// [[type open-world validation::au-type-system]].
pub fn run_location_checks(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    peer: Option<&dyn CrossRepoResolver>,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (_, td) in graph.iter() {
        let Some(loc) = &td.location else {
            continue;
        };
        check_location_name_fields(graph, resolution, td, loc, &mut diags);
        check_location_filetype_body(graph, peer, td, loc, &mut diags);
    }
    diags
}

/// A `name` template renders one filesystem-safe path component, so each
/// `${.field}` must reference a REQUIRED field whose shape is a safe scalar:
/// `String` / `Number` / `Boolean` / `Date` / `DateTime` (the engine's
/// `DateTime` is the colon-free `YYYY-MM-DDThhmmssZ` form, so it renders one safe
/// component) / an `enum`, or a refinement over one. `Url`, a list, a reference,
/// a record, a compound, or a tuple render an unsafe or multi-part value.
fn check_location_name_fields(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    td: &TypeDef,
    loc: &LocationSpec,
    diags: &mut Vec<Diagnostic>,
) {
    let Some(name) = &loc.name else {
        return;
    };
    // The effective shape of an instance claiming this type; `${.type}` is
    // always safe, only `${.field}` segments are checked. Over the folded
    // resolution graph when the repo imports, so a field inherited from a
    // cross-repo parent is visible (else `closure_of` stops at the subtype and
    // the inherited field false-positives "not in the effective shape").
    let claim = TypeClaim::Bare(TypeNameClaim::own(td.name.clone(), ByteRange::new(0, 0)));
    let shape_result = match resolution {
        Some(rg) => effective_shape_resolved(rg, graph, &claim),
        None => effective_shape(graph, &claim),
    };
    let Ok(shape) = shape_result else {
        // An unresolvable claim is another gate's concern; skip here.
        return;
    };
    let bad = |msg: String, diags: &mut Vec<Diagnostic>| {
        diags.push(Diagnostic {
            code: codes::LOCATION_BAD_SHAPE,
            severity: Severity::Error,
            span: Span::new(&td.source_path, loc.name_span),
            message: msg,
            related: vec![],
            fix: None,
        });
    };
    for seg in &name.segments {
        let NameSegment::Field(f) = seg else {
            continue;
        };
        let fname = FieldName(f.clone());
        let Some(origin) = shape.get(&fname) else {
            if shape.get_divergent(&fname).is_some() {
                bad(
                    format!("`location.name` references divergent field '{f}'; a name template needs one unambiguous shape, qualify or rename it"),
                    diags,
                );
            } else {
                bad(
                    format!("`location.name` references field '{f}', which is not in the type's effective shape"),
                    diags,
                );
            }
            continue;
        };
        if !origin.is_required() {
            bad(
                format!("`location.name` references optional field '{f}'; a name template references required fields only"),
                diags,
            );
            continue;
        }
        match &origin.canonical_decl().parsed_shape {
            Ok(sh) if is_safe_name_shape(sh) => {}
            Ok(_) => bad(
                format!("`location.name` references field '{f}', whose shape does not render one filesystem-safe component; use a scalar String / Number / Boolean / Date / DateTime / enum field"),
                diags,
            ),
            // An unparseable shape is `shape-syntax-error`'s; do not double-report.
            Err(_) => {}
        }
    }
}

/// A safe `name`-slot shape: a scalar primitive other than `Url`, a refinement
/// over one, or an enum. Everything else renders unsafe or multi-part.
fn is_safe_name_shape(shape: &Shape) -> bool {
    match shape {
        Shape::Primitive(p) => !matches!(p, Primitive::Url),
        Shape::Refined { base, .. } => !matches!(base, Primitive::Url),
        Shape::Enum(_) => true,
        _ => false,
    }
}

/// `fileType: yaml` beside a non-empty `body:` is unsatisfiable: the body forces
/// markdown, the pin forces yaml, so no instance can be clean.
fn check_location_filetype_body(
    graph: &TypeGraph,
    peer: Option<&dyn CrossRepoResolver>,
    td: &TypeDef,
    loc: &LocationSpec,
    diags: &mut Vec<Diagnostic>,
) {
    if loc.file_type != Some(FileType::Yaml) {
        return;
    }
    // `peer` resolves a cross-repo `use:` splice, so a body composed from a peer
    // type is visible (else the conflict is a false negative cross-repo).
    if crate::body::splice_effective_body(graph, &td.name, peer).is_some() {
        diags.push(Diagnostic {
            code: codes::LOCATION_FILETYPE_BODY_CONFLICT,
            severity: Severity::Error,
            span: Span::new(&td.source_path, loc.file_type_span),
            message: format!(
                "`location.fileType: yaml` conflicts with a non-empty `body:` on '{}'; a body forces markdown, so no instance can be yaml",
                td.name.as_str()
            ),
            related: vec![],
            fix: None,
        });
    }
}

fn check_body_fills(graph: &TypeGraph, td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    let Some(body) = td.body.as_ref() else {
        return;
    };
    let shape = match effective_shape(
        graph,
        &TypeClaim::Bare(TypeNameClaim::own(td.name.clone(), td.source_span)),
    ) {
        Ok(s) => s,
        Err(EffectiveShapeError::UnknownType(_)) => return,
    };
    let mut exclusive_stack: Vec<ExclusiveFrame<'_>> = Vec::new();
    walk_fills(td, body, &shape, &mut exclusive_stack, diags);
}

/// One ancestor `fills!:` frame in the active descent. Carries the
/// allowed field-name set plus the declaration span so the descendant's
/// conflict diagnostic can anchor `related[]` at the ancestor's
/// declaration site.
struct ExclusiveFrame<'a> {
    fields: Vec<&'a str>,
    source_path: &'a std::path::Path,
    fields_span: ByteRange,
}

fn walk_fills<'a>(
    td: &TypeDef,
    items: &'a [crate::body::BodyItem],
    shape: &crate::closure::EffectiveShape,
    exclusive_stack: &mut Vec<ExclusiveFrame<'a>>,
    diags: &mut Vec<Diagnostic>,
) {
    // Two-pass: [[type-def body fills::au-type-system]] body-level `fills!:` constrains every section at
    // this level regardless of its position within `items`. So we push
    // body-level exclusive frames first, walk sections second, then
    // pop the body-level frames.
    //
    // Body-level Fills items are still checked against any inherited
    // ancestor stack BEFORE pushing their own frame — a body-level
    // `fills: c` nested inside a section whose parent `fills!: a`
    // would still fire `fills-contract-conflict-nested-exclusivity`.
    let mut body_level_pushed: usize = 0;
    for item in items {
        if let crate::body::BodyItem::Fills { contract, .. } = item {
            check_fills_contract(td, contract, shape, exclusive_stack, diags);
            if contract.exclusive {
                exclusive_stack.push(ExclusiveFrame {
                    fields: contract.fields.iter().map(|c| c.name.as_str()).collect(),
                    source_path: contract.source_path.as_path(),
                    fields_span: contract.fields_span,
                });
                body_level_pushed += 1;
            }
        }
    }

    for item in items {
        match item {
            crate::body::BodyItem::Section {
                fills,
                body: nested,
                ..
            } => {
                let pushed = if let Some(contract) = fills {
                    check_fills_contract(td, contract, shape, exclusive_stack, diags);
                    if contract.exclusive {
                        exclusive_stack.push(ExclusiveFrame {
                            fields: contract.fields.iter().map(|c| c.name.as_str()).collect(),
                            source_path: contract.source_path.as_path(),
                            fields_span: contract.fields_span,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if let Some(inner) = nested {
                    walk_fills(td, inner, shape, exclusive_stack, diags);
                }
                if pushed {
                    exclusive_stack.pop();
                }
            }
            crate::body::BodyItem::Fills { .. } | crate::body::BodyItem::Use { .. } => {}
        }
    }

    for _ in 0..body_level_pushed {
        exclusive_stack.pop();
    }
}

fn check_fills_contract(
    td: &TypeDef,
    contract: &crate::body::FillsContract,
    shape: &crate::closure::EffectiveShape,
    exclusive_stack: &[ExclusiveFrame<'_>],
    diags: &mut Vec<Diagnostic>,
) {
    for claim in &contract.fields {
        let field_name = claim.name.as_str();

        // Conflict with any ancestor `fills!:` whose allowed-field set
        // doesn't contain this claim. Per [[type-def body fills::au-type-system]]: a descendant claim is
        // valid iff it's in EVERY enclosing exclusive ancestor's
        // allowed set. First violating ancestor wins the diagnostic
        // (closest declaration site is most actionable).
        for frame in exclusive_stack {
            if !frame.fields.contains(&field_name) {
                let allowed = format_field_list(&frame.fields);
                diags.push(Diagnostic {
                    code: codes::FILLS_CONTRACT_CONFLICT_NESTED_EXCLUSIVITY,
                    severity: Severity::Error,
                    span: Span::new(td.source_path.clone(), claim.span),
                    message: format!(
                        "nested `fills:` requires '{field_name}', but an ancestor `fills!:` forbids any field other than {allowed}"
                    ),
                    related: vec![Span::new(
                        frame.source_path.to_path_buf(),
                        frame.fields_span,
                    )],
                    fix: None,
                });
                break;
            }
        }

        // Field must exist in the host type-def's effective closure. Existence is
        // the whole check: EVERY shape can be carried in prose, so a `fills:`
        // target is never rejected for its shape.
        if shape.get(&claim.name).is_none() {
            diags.push(Diagnostic {
                code: codes::FILLS_UNKNOWN_FIELD,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), claim.span),
                message: format!(
                    "`fills:` references field '{}' which is absent from '{}'s effective closure",
                    field_name,
                    td.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

/// Render a field-name list for diagnostic messages. Single field renders
/// as `'a'`; multiple as `['a', 'b']`. Matches what a reader would write.
fn format_field_list(names: &[&str]) -> String {
    if names.len() == 1 {
        format!("'{}'", names[0])
    } else {
        let joined: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
        format!("[{}]", joined.join(", "))
    }
}

/// [[type-def field shape::au-type-system]] prose-extractability for `fills:` bindings.
///
/// Every leaf shape is extractable via some surface — primitives via
/// inline-code, enums via inline-code, references via wikilink, records
/// and inline-or-reference via a marked fence.
///
/// Lists recurse on the inner shape.
///
/// Unions and intersections are extractable when every branch is
/// individually extractable AND at least one branch is non-primitive.
/// The mixed case `<String | T*>` is the [[aspiration - type system ideas]] carve-out — the
/// reference branch gives the contribution a clear surface (wikilink)
/// distinct from the primitive surface (inline-code), so authors can
/// disambiguate at the site. Compounds whose branches are entirely
/// primitives (e.g. `<String | Number>`) are rejected per [[type-def field shape::au-type-system]] —
/// inline-code can carry either, and the engine doesn't currently
/// dispatch on parsed value shape inside `fills:`.

fn check_body_use_in_closure(graph: &TypeGraph, td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    let Some(body) = td.body.as_ref() else {
        return;
    };
    let closure = closure_of(graph, &td.name);
    for item in body {
        let crate::body::BodyItem::Use {
            type_name,
            type_name_span,
            repo,
            ..
        } = item
        else {
            continue;
        };
        // A `::repo` use is in the FOLDED closure, checked via the fold (later
        // action). The own-graph closure walk cannot see it, so defer here rather
        // than false-fire `body-use-out-of-closure`.
        if repo.is_some() {
            continue;
        }
        if !closure.contains(type_name) {
            diags.push(Diagnostic {
                code: codes::BODY_USE_OUT_OF_CLOSURE,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), *type_name_span),
                message: format!(
                    "`use: {}` references a type-def not in '{}'s closure",
                    type_name.as_str(),
                    td.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
            continue;
        }
        // Target is in closure — does it actually carry a body?
        // `body: []` and `body: None` both leave nothing to splice; the
        // `use:` line is a no-op either way, almost always a typo on
        // the target name or a dangling `use:` left after the target
        // lost its body.
        let target_has_body = graph
            .get(type_name)
            .and_then(|t| t.body.as_ref())
            .is_some_and(|b| !b.is_empty());
        if !target_has_body {
            diags.push(Diagnostic {
                code: codes::BODY_USE_TARGET_HAS_NO_BODY,
                severity: Severity::Warning,
                span: Span::new(td.source_path.clone(), *type_name_span),
                message: format!(
                    "`use: {}` splices nothing — '{}' declares no `body:` template",
                    type_name.as_str(),
                    type_name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

/// Detect `use:` splice cycles. Each node on a cycle gets exactly one
/// `body-use-cycle` diagnostic; `related[]` lists every other cycle
/// member so the author can see the full loop from any entry point.
///
/// Three deterministic properties:
/// 1. Iteration order is sorted (TypeGraph's underlying BTreeMap).
/// 2. Edges are filtered to in-closure targets — a `use:` to a type
///    outside the host's closure is already diagnosed as
///    `body-use-out-of-closure`; counting it as a cycle edge would
///    double-fire on the same broken edge.
/// 3. Back-edge detection marks every node on the active stack slice
///    between the cycle target and the current top — not just the two
///    endpoints. Without this, a cycle member only reachable through
///    paths other than the one currently being walked would be missed.
fn check_body_use_cycles(graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    // Per-host filtered adjacency: only walk `use:` edges whose target
    // is in the host's closure. Out-of-closure edges are diagnosed
    // separately as `body-use-out-of-closure`.
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, td) in graph.iter() {
        let mut targets: Vec<&str> = Vec::new();
        if let Some(body) = &td.body {
            let closure = closure_of(graph, name);
            for item in body {
                // A qualified use crosses repos, not an own-graph cycle edge; the
                // fold's cross-repo cycle guard owns it (later action).
                if let crate::body::BodyItem::Use {
                    type_name,
                    repo: None,
                    ..
                } = item
                {
                    if closure.contains(type_name) {
                        targets.push(type_name.as_str());
                    }
                }
            }
        }
        adj.insert(name.as_str(), targets);
    }

    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<&str, Color> = BTreeMap::new();
    for name in adj.keys() {
        color.insert(*name, Color::White);
    }
    // node → other members of every cycle it participates in.
    let mut cycle_peers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for &start_name in adj.keys() {
        if matches!(color.get(start_name), Some(Color::Black)) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start_name, 0)];
        color.insert(start_name, Color::Gray);
        while let Some((name, child_idx)) = stack.last().copied() {
            let targets = adj.get(name).cloned().unwrap_or_default();
            if child_idx >= targets.len() {
                color.insert(name, Color::Black);
                stack.pop();
                continue;
            }
            let next = targets[child_idx];
            stack.last_mut().unwrap().1 = child_idx + 1;
            match color.get(next) {
                Some(Color::Gray) => {
                    // Back-edge into `next`. Every node from `next`'s
                    // stack position to the current top is on this
                    // cycle; capture all of them as peers of each
                    // other.
                    let cycle_start_idx = stack
                        .iter()
                        .position(|(n, _)| *n == next)
                        .expect("Gray means it's on the stack");
                    let members: Vec<&str> =
                        stack[cycle_start_idx..].iter().map(|(n, _)| *n).collect();
                    for m in &members {
                        let entry = cycle_peers.entry((*m).to_string()).or_default();
                        for other in &members {
                            if other != m {
                                entry.insert((*other).to_string());
                            }
                        }
                    }
                }
                Some(Color::White) => {
                    color.insert(next, Color::Gray);
                    stack.push((next, 0));
                }
                _ => {}
            }
        }
    }

    // Emit one diagnostic per cycle member, with `related[]` carrying
    // every other member's source-span. Sorted by member name for
    // deterministic diagnostic order.
    for (name, peers) in &cycle_peers {
        let Some(td) = graph.get(&TypeName(name.clone())) else {
            continue;
        };
        let mut related = Vec::with_capacity(peers.len());
        for peer in peers {
            if let Some(peer_td) = graph.get(&TypeName(peer.clone())) {
                related.push(Span::new(peer_td.source_path.clone(), peer_td.source_span));
            }
        }
        let peer_list = peers
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let message = if peers.is_empty() {
            format!("type-def '{name}' participates in a self-`use:` splice cycle")
        } else {
            format!("type-def '{name}' participates in a `use:` splice cycle with {peer_list}")
        };
        diags.push(Diagnostic {
            code: codes::BODY_USE_CYCLE,
            severity: Severity::Error,
            span: Span::new(td.source_path.clone(), td.source_span),
            message,
            related,
            fix: None,
        });
    }
}

// ----- type-def parent claim: redundancy + mixin-collision ([[type-instance type::au-type-system]], [[type-def fields collision - auto-unify and qualified field::au-type-system]]) -----

/// Type-def-level parallel of the validator's instance-claim redundant-
/// claim warnings. Emits `duplicate-claim` and `subsumption-in-mixin` at
/// load time when the type-def's `type:` parents list is redundant. Same
/// helper, same diagnostic codes, different fire site.
fn check_parent_redundant_claims(graph: &TypeGraph, td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    // Type-def parent list, checked at graph-load time before the cross-repo fold
    // exists, so subsumption is own-graph only (a `::repo` parent's ancestry is
    // the fold's, unavailable here). Instance mixins reach folded parity at
    // validate time.
    diags.extend(check_redundant_claims(
        graph,
        None,
        &td.parents,
        &td.source_path,
    ));
}

/// Type-def-level mixin-collision ([[type-def fields collision - auto-unify and qualified field::au-type-system]] — cannot be deferred to
/// instance time). When a type-def has `type: [a, b, ...]` parents, walk
/// the union of their closures and emit `mixin-collision` for any field
/// name whose origins disagree on shape.
///
// A divergent inherited field is LEGAL at a type-def ([[type-def fields collision - auto-unify and qualified field::au-type-system]]): the type is
// not broken, and the collision surfaces per-instance at a bare use, where it can
// actually be resolved by a qualifier. So there is no type-graph-load
// mixin-collision check; the validator emits it against an instance's claim list.

// ----- type-name regex -----

/// True if `s` matches `^[A-Za-z][A-Za-z0-9_-]*(\.[A-Za-z][A-Za-z0-9_-]*)*$`.
pub fn is_valid_type_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    for segment in s.split('.') {
        if segment.is_empty() {
            return false;
        }
        let mut chars = segment.chars();
        let first = match chars.next() {
            Some(c) => c,
            None => return false,
        };
        if !first.is_ascii_alphabetic() {
            return false;
        }
        for c in chars {
            if !c.is_ascii_alphanumeric() && c != '-' && c != '_' {
                return false;
            }
        }
    }
    true
}

fn check_type_name_regex(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    if !is_valid_type_name(td.name.as_str()) {
        diags.push(Diagnostic {
            code: codes::TYPE_NAME_VIOLATES_REGEX,
            severity: Severity::Error,
            span: Span::new(td.source_path.clone(), td.source_span),
            message: format!(
                "type name '{}' violates the type-name regex",
                td.name.as_str()
            ),
            related: vec![],
            fix: None,
        });
    }
    for parent in &td.parents {
        if !is_valid_type_name(parent.name.as_str()) {
            diags.push(Diagnostic {
                code: codes::TYPE_NAME_VIOLATES_REGEX,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), parent.span),
                message: format!(
                    "parent type name '{}' violates the type-name regex",
                    parent.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        }
    }
    for s in &td.sealed {
        if !is_valid_type_name(s.name.as_str()) {
            diags.push(Diagnostic {
                code: codes::TYPE_NAME_VIOLATES_REGEX,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), s.span),
                message: format!(
                    "sealed branch name '{}' violates the type-name regex",
                    s.name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

// ----- reserved type-def names -----

fn check_reserved_type_name(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    let name = td.name.as_str();
    // `any` is checked before the reference-target list so it keeps its own
    // detail even once it joins that list as a reference target ([[type-def shape any::au-type-system]]).
    let detail = if name == "any" {
        "no-type slot shape"
    } else if name == "opaque" {
        "uninterpreted slot shape"
    } else if BUILT_IN_REFERENCE_TARGETS.contains(&name) {
        "built-in any-repo-file reference"
    } else if PRIMITIVE_NAMES.contains(&name) {
        "primitive shape"
    } else {
        return;
    };
    diags.push(Diagnostic {
        code: codes::RESERVED_TYPE_NAME,
        severity: Severity::Error,
        span: Span::new(td.source_path.clone(), td.source_span),
        message: format!(
            "type-def name '{}' is reserved by the engine ({})",
            name, detail
        ),
        related: vec![],
        fix: None,
    });
}

// ----- slot-references-absent-type -----

/// A `type:` parent must name a present type-def. An absent parent truncates
/// the closure silently, the parent's fields vanish from every instance with no
/// signal, so this is an error. The parent sibling of
/// [`check_slot_reference_targets`]; fires for every type-def, a truncated
/// parent closure is unsound.
fn check_parent_reference_targets(td: &TypeDef, graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    for parent in &td.parents {
        // A `::repo` parent is a peer type resolved by the cross-repo fold, not
        // present in this own graph; the engine gates the peer reference, so it
        // is not an absent-parent error here.
        if parent.is_qualified() {
            continue;
        }
        let name = parent.name.as_str();
        if BUILT_IN_REFERENCE_TARGETS.contains(&name) {
            continue;
        }
        // A syntactically invalid parent name is `type-name-violates-regex`'s,
        // not an absent-type report: it never could name a real type-def.
        if !is_valid_type_name(name) {
            continue;
        }
        if !graph.contains(&parent.name) {
            diags.push(Diagnostic {
                code: codes::PARENT_REFERENCES_ABSENT_TYPE,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), parent.span),
                message: format!(
                    "type-def '{}' claims parent '{}', which is not in the type graph",
                    td.name.as_str(),
                    name
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

/// A bare `required:` obligation must name a present type-def. The meta sibling
/// of [`check_slot_reference_targets`] / [`check_parent_reference_targets`]; an
/// absent obligation could never be satisfied. A `::repo` target is a peer type
/// the engine's cross-repo gate owns, skipped here. Meta-ness of a present target
/// (`required-meta-not-a-meta-type`) is a resolution-graph check, not here.
fn check_required_meta_targets(td: &TypeDef, graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    for r in &td.required_meta {
        if r.is_qualified() {
            continue;
        }
        let name = r.name.as_str();
        if !is_valid_type_name(name) {
            continue;
        }
        if !graph.contains(&r.name) {
            diags.push(Diagnostic {
                code: codes::REQUIRED_META_ABSENT_TYPE,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), r.span),
                message: format!(
                    "type-def '{}' requires meta '{}', which is not in the type graph",
                    td.name.as_str(),
                    name
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

fn check_slot_reference_targets(td: &TypeDef, graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        for name in collect_referenced_type_names(shape) {
            if BUILT_IN_REFERENCE_TARGETS.contains(&name) {
                continue;
            }
            let target = TypeName(name.to_string());
            if !graph.contains(&target) {
                let mut message = format!(
                    "field '{}' on type-def '{}' references type-def '{}', which is not in the type graph",
                    field.name.as_str(),
                    td.name.as_str(),
                    name
                );
                if let Some(primitive) = primitive_typo_hint(name) {
                    // Most-common typo: lowercase form of a primitive.
                    // Append a hint, but keep severity::Error — a real
                    // missing type-def is still a real error, and we
                    // don't want to silently accept a load with a typo.
                    message.push_str(&format!(
                        " — did you mean '{}' (the {} primitive shape)?",
                        primitive, primitive
                    ));
                }
                diags.push(Diagnostic {
                    code: codes::SLOT_REFERENCES_ABSENT_TYPE,
                    severity: Severity::Error,
                    span: Span::new(td.source_path.clone(), field.shape_span),
                    message,
                    related: vec![],
                    fix: None,
                });
            }
        }
    }
}

/// A union brand member's kind, for referenceability gating and value
/// discrimination ([[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]]).
/// One classifier, shared by `load_checks` (referenceability) and `validate`
/// (discrimination), so the two never disagree about what a member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnionMemberKind {
    /// A record type — discriminates by inline `type:`, referenceable.
    Record,
    /// A nominal brand (scalar / enum / tuple) — selectable ONLY via its
    /// `Name(...)` constructor, never matched by a bare value, not referenceable.
    NominalBrand,
    /// A primitive or inline enum — matched directly by a bare value, not
    /// referenceable.
    Bare,
}

/// Classify one union-brand member shape against the graph. A bare name
/// (`Shape::Record`) resolving to a def with a nominal brand shape is a
/// `NominalBrand`; any other resolved name (a record type, an unresolved or
/// cross-repo name) is a `Record`; a primitive / enum is `Bare`. A per-branch
/// reference form (`<a* | b>`) reads as a `Record` — it already names a
/// referenceable type.
pub(crate) fn classify_union_member(member: &Shape, graph: &TypeGraph) -> UnionMemberKind {
    match member {
        Shape::Primitive(_) | Shape::Refined { .. } | Shape::Enum(_) => UnionMemberKind::Bare,
        Shape::Record(qn) | Shape::Reference(qn) | Shape::InlineOrReference(qn) => {
            if qn.repo.is_none() {
                if let Some(b) = graph
                    .get(&TypeName(qn.as_str().to_string()))
                    .and_then(|t| t.shape.as_ref())
                {
                    if b.is_nominal() {
                        return UnionMemberKind::NominalBrand;
                    }
                }
            }
            UnionMemberKind::Record
        }
        // Any other shape in a union member position is not bare-referenceable;
        // treat it as a record-ish reference target so a `*` gate is conservative.
        _ => UnionMemberKind::Record,
    }
}

/// A union brand is referenceable (its slot may carry `*` / `&`) iff EVERY member
/// is a record type. Any primitive or nominal-brand member makes the whole brand
/// inline-only. See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub(crate) fn union_brand_is_referenceable(members: &[Shape], graph: &TypeGraph) -> bool {
    members
        .iter()
        .all(|m| classify_union_member(m, graph) == UnionMemberKind::Record)
}

/// Whether a brand admits a `*` / `&` reference suffix on a slot demanding it:
/// a NOMINAL brand (scalar / enum / tuple) never does, a STRUCTURAL (union)
/// brand only when every member is a record type. `graph` is the graph the
/// brand's members resolve in — its own repo's, so a peer brand is checked
/// against the peer graph. The shared predicate behind the own-repo load check
/// and the cross-repo gate, so the two agree.
/// See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
pub fn brand_admits_reference(brand: &crate::typedef::BrandShape, graph: &TypeGraph) -> bool {
    match &brand.shape {
        Shape::Union(members) => union_brand_is_referenceable(members, graph),
        // A nominal brand (scalar / enum) and any other inline-only shape do not.
        _ => false,
    }
}

/// `brand-not-referenceable`: a field slot demands a brand with a `*` or `&`
/// reference suffix that the brand does not admit. A NOMINAL brand (scalar /
/// enum / tuple) is inline-only, the same as the primitive it wraps. A STRUCTURAL
/// (union) brand is referenceable ONLY when every member is a record type — a
/// union with a primitive or nominal-brand member (`<evidence | String>`) is
/// inline-only too. Own-repo targets only, a `::repo` brand ref resolves
/// cross-repo. See [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
fn check_brand_not_referenceable(td: &TypeDef, graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    fn ref_targets<'a>(shape: &'a Shape, out: &mut Vec<(&'a au_grammar::QualifiedName, char)>) {
        match shape {
            Shape::Reference(q) => out.push((q, '*')),
            Shape::InlineOrReference(q) => out.push((q, '&')),
            Shape::CompoundReference { branches, mode, .. } => {
                let c = mode.suffix_char();
                for b in branches {
                    out.push((b, c));
                }
            }
            Shape::List { inner, .. } => ref_targets(inner, out),
            Shape::Pinned(inner) => ref_targets(inner, out),
            Shape::Union(v) | Shape::Intersection(v) | Shape::Tuple(v) => {
                for b in v {
                    ref_targets(b, out);
                }
            }
            _ => {}
        }
    }
    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        let mut targets = Vec::new();
        ref_targets(shape, &mut targets);
        for (qn, suffix) in targets {
            if qn.repo.is_some() {
                continue;
            }
            let Some(brand) = graph
                .get(&TypeName(qn.as_str().to_string()))
                .and_then(|t| t.shape.as_ref())
            else {
                continue;
            };
            // A nominal brand is never referenceable. A structural (union) brand
            // is referenceable only when every member is a record type; a
            // primitive or nominal-brand member makes it inline-only too.
            let detail = if brand.is_nominal() {
                Some(format!(
                    "slot references nominal brand '{}' with '{}'; a nominal brand (scalar / enum / tuple) is inline-only, use it by bare name '{}'",
                    qn.as_str(),
                    suffix,
                    qn.as_str(),
                ))
            } else if let Shape::Union(members) = &brand.shape {
                if union_brand_is_referenceable(members, graph) {
                    None
                } else {
                    Some(format!(
                        "slot references union brand '{}' with '{}', but it has a non-record member; a union brand is referenceable only when every member is a record type, so use it by bare name '{}'",
                        qn.as_str(),
                        suffix,
                        qn.as_str(),
                    ))
                }
            } else {
                None
            };
            if let Some(detail) = detail {
                diags.push(Diagnostic {
                    code: codes::BRAND_NOT_REFERENCEABLE,
                    severity: Severity::Error,
                    span: Span::new(td.source_path.clone(), field.shape_span),
                    message: format!(
                        "field '{}' on type-def '{}': {}",
                        field.name.as_str(),
                        td.name.as_str(),
                        detail,
                    ),
                    related: vec![],
                    fix: Some(au_diagnostics::SuggestedFix {
                        description: format!(
                            "drop the '{}' suffix, write the slot as '{}'",
                            suffix,
                            qn.as_str()
                        ),
                    }),
                });
            }
        }
    }
}

/// If `name` is a case-insensitive match for a primitive name (but not
/// the canonical capitalized form), return the canonical form. Used by
/// `slot-references-absent-type` to suggest the primitive when the user
/// most likely typo'd a case (e.g. `string` for `String`).
fn primitive_typo_hint(name: &str) -> Option<&'static str> {
    for &p in PRIMITIVE_NAMES {
        if name != p && name.eq_ignore_ascii_case(p) {
            return Some(p);
        }
    }
    None
}

// ----- subsumption-in-slot-union / -intersection (spec [[type-def shape compound::au-type-system]]) -----

/// Slot-union subsumption check. Walks every field's shape; for each Union
/// (bare or `<...>*` / `<...>&` CompoundReference) collects the reference-
/// branch names and emits `subsumption-in-slot-union` for any pair where
/// one closure includes the other. Symmetric to `subsumption-in-mixin` —
/// reuses `closure_of` and the same pair-wise iteration pattern as
/// `check_redundant_claims`.
///
/// Skips primitive / enum branches in a union (no closure to compare).
fn check_slot_union_subsumption(td: &TypeDef, graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    check_slot_compound_subsumption(
        td,
        graph,
        CompoundRefOp::Union,
        &codes::SUBSUMPTION_IN_SLOT_UNION,
        "slot-union",
        diags,
    );
}

/// Slot-intersection subsumption check, mirror of the union case (spec
/// [[type-def shape compound::au-type-system]]): `<A & B>` where one branch's closure includes the other
/// collapses to the narrower branch. Same pairwise structure, same
/// "wider branch flagged as redundant" convention.
fn check_slot_intersection_subsumption(
    td: &TypeDef,
    graph: &TypeGraph,
    diags: &mut Vec<Diagnostic>,
) {
    check_slot_compound_subsumption(
        td,
        graph,
        CompoundRefOp::Intersection,
        &codes::SUBSUMPTION_IN_SLOT_INTERSECTION,
        "slot-intersection",
        diags,
    );
}

/// Shared body for slot-union and slot-intersection subsumption. `op`
/// selects which compounds to walk; `code` / `op_label` parameterize the
/// emitted diagnostic.
fn check_slot_compound_subsumption(
    td: &TypeDef,
    graph: &TypeGraph,
    op: CompoundRefOp,
    code: &DiagnosticCode,
    op_label: &'static str,
    diags: &mut Vec<Diagnostic>,
) {
    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        // `collect_slot_compound_branch_names` only yields compounds with
        // ≥ 2 reference branches matching `op`; consumer doesn't re-check.
        for compound_names in collect_slot_compound_branch_names(shape, op) {
            for (i, ni) in compound_names.iter().enumerate() {
                let target_i = TypeName((*ni).to_string());
                // `None` for the built-in targets `file` / `any`, which
                // aren't graph types; the `any`-as-wider rule below covers
                // them without a closure.
                let closure_i = graph
                    .contains(&target_i)
                    .then(|| closure_of(graph, &target_i));
                for (j, nj) in compound_names.iter().enumerate() {
                    if i == j || nj == ni {
                        continue;
                    }
                    // `nj` is the wider branch (it subsumes `ni`) when it is
                    // the universal `any` target, or it is an ancestor of
                    // `ni` in the graph. `any*` ([[type-def shape any::au-type-system]]) accepts every
                    // value any other reference branch would, including `file*`
                    // and any typed `T*`, so it subsumes them.
                    // Which branch is redundant depends on the operator:
                    //   Union:        narrower (`ni`) is redundant, the wider
                    //                 already accepts every value it would.
                    //   Intersection: wider (`nj`) is redundant, the narrower
                    //                 already implies it.
                    let nj_is_wider = *nj == "any"
                        || closure_i
                            .as_ref()
                            .is_some_and(|c| c.contains(&TypeName((*nj).to_string())));
                    if !nj_is_wider {
                        continue;
                    }
                    let (redundant_name, other_name) = match op {
                        CompoundRefOp::Union => (ni, nj),
                        CompoundRefOp::Intersection => (nj, ni),
                    };
                    let message = format_subsumption_message(
                        op,
                        field.name.as_str(),
                        td.name.as_str(),
                        op_label,
                        redundant_name,
                        other_name,
                    );
                    diags.push(Diagnostic {
                        code: code.clone(),
                        severity: Severity::Warning,
                        span: Span::new(td.source_path.clone(), field.shape_span),
                        message,
                        related: vec![],
                        fix: None,
                    });
                }
            }
        }
    }
}

/// Build the user-facing subsumption message. Both closure-inclusion
/// (named types) and cardinality-refinement (`T[]` vs `T[+]`) flow through
/// here so the wording is consistent. Frames *which* branch is redundant
/// and *why* in structural terms (wider/stricter), so the explanation
/// reads naturally even when the branches don't have a named identity
/// the user has a mental model for.
fn format_subsumption_message(
    op: CompoundRefOp,
    field_name: &str,
    type_def_name: &str,
    op_label: &str,
    redundant: &dyn std::fmt::Display,
    other: &dyn std::fmt::Display,
) -> String {
    match op {
        CompoundRefOp::Union => format!(
            "field '{}' on type-def '{}': {} branch '{}' adds nothing — '{}' is wider and accepts every '{}' value",
            field_name, type_def_name, op_label, redundant, other, redundant
        ),
        CompoundRefOp::Intersection => format!(
            "field '{}' on type-def '{}': {} branch '{}' adds nothing — '{}' is stricter and the intersection collapses to '{}'",
            field_name, type_def_name, op_label, redundant, other, other
        ),
    }
}

/// Cardinality-refinement subsumption (spec [[type-def shape compound::au-type-system]]): pairs of branches in
/// the same Union/Intersection that share an `inner` shape but differ on
/// `non_empty`. `T[+]` is a strict subset of `T[]`, so `<T[] | T[+]>`
/// warns (wider subsumes narrower) and `<T[] & T[+]>` collapses to
/// `T[+]` (the wider is redundant). Parallel to the closure-inclusion
/// check above; same diagnostic codes since spec [[type-def shape compound::au-type-system]] treats both as
/// one family ("strictly narrower constraint").
/// `refinement-unsatisfiable`: a field whose value refinement ([[type-def field shape::au-type-system]])
/// has a numeric meet that admits no value. A `Warning` on the field's shape;
/// the validator suppresses the per-value `value-out-of-refinement` for such a slot.
fn check_refinement_unsatisfiable(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    fn walk(shape: &Shape, found: &mut bool) {
        match shape {
            Shape::Refined { base, refinement } => {
                if crate::validate::refinement_unsatisfiable(*base, refinement) {
                    *found = true;
                }
            }
            Shape::List { inner, .. } | Shape::Pinned(inner) => walk(inner, found),
            Shape::Union(branches) | Shape::Intersection(branches) => {
                branches.iter().for_each(|b| walk(b, found))
            }
            _ => {}
        }
    }
    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        let mut found = false;
        walk(shape, &mut found);
        if found {
            diags.push(Diagnostic {
                code: codes::REFINEMENT_UNSATISFIABLE,
                severity: Severity::Warning,
                span: Span::new(td.source_path.clone(), field.shape_span),
                message: format!(
                    "field '{}' declares a value refinement '{}' that admits no value",
                    field.name.as_str(),
                    field.shape_display()
                ),
                related: vec![],
                fix: None,
            });
        }
    }
}

/// `refinement-bad-shape` at load: a value refinement whose regex does not
/// compile, or whose `Date` / `DateTime` bound literal is not a valid date.
/// The parse-time shape errors are separate ([[type-def field shape::au-type-system]]).
fn check_refinement_load_errors(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    fn walk(shape: &Shape, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        match shape {
            Shape::Refined { base, refinement } => {
                if let Some(msg) = crate::validate::refinement_load_error(*base, refinement) {
                    *out = Some(msg);
                }
            }
            Shape::List { inner, .. } | Shape::Pinned(inner) => walk(inner, out),
            Shape::Union(branches) | Shape::Intersection(branches) => {
                branches.iter().for_each(|b| walk(b, out))
            }
            _ => {}
        }
    }
    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        let mut err = None;
        walk(shape, &mut err);
        if let Some(msg) = err {
            diags.push(Diagnostic {
                code: au_grammar::REFINEMENT_BAD_SHAPE,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), field.shape_span),
                message: format!("field '{}': {}", field.name.as_str(), msg),
                related: vec![],
                fix: None,
            });
        }
    }
}

fn check_slot_cardinality_subsumption(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    fn walk(
        shape: &Shape,
        td: &TypeDef,
        field: &crate::typedef::FieldDecl,
        diags: &mut Vec<Diagnostic>,
    ) {
        match shape {
            Shape::Union(branches) => {
                emit_pairs(branches, CompoundRefOp::Union, td, field, diags);
                for b in branches {
                    walk(b, td, field, diags);
                }
            }
            Shape::Intersection(branches) => {
                emit_pairs(branches, CompoundRefOp::Intersection, td, field, diags);
                for b in branches {
                    walk(b, td, field, diags);
                }
            }
            Shape::List { inner, .. } => walk(inner, td, field, diags),
            Shape::Pinned(inner) => walk(inner, td, field, diags),
            // A tuple element may itself be a compound; descend to catch a
            // subsumption inside it.
            Shape::Tuple(elements) => {
                for el in elements {
                    walk(el, td, field, diags);
                }
            }
            Shape::Primitive(_)
            | Shape::Enum(_)
            | Shape::Any
            | Shape::Opaque
            | Shape::Reference(_)
            | Shape::Record(_)
            | Shape::InlineOrReference(_)
            | Shape::CompoundReference { .. }
            | Shape::DefReference(_)
            | Shape::Refined { .. } => {}
        }
    }

    fn emit_pairs(
        branches: &[Shape],
        op: CompoundRefOp,
        td: &TypeDef,
        field: &crate::typedef::FieldDecl,
        diags: &mut Vec<Diagnostic>,
    ) {
        for i in 0..branches.len() {
            for j in (i + 1)..branches.len() {
                let (a, b) = (&branches[i], &branches[j]);
                // Region subsumption over same-kind branches: a list count-range
                // (same inner shape) or a value refinement (same base primitive).
                // Equal or incomparable regions do not warn.
                let a_sub_b = branch_region_subset(a, b);
                let b_sub_a = branch_region_subset(b, a);
                if a_sub_b == b_sub_a {
                    continue;
                }
                let (wider, narrower) = if a_sub_b { (b, a) } else { (a, b) };
                let (code, op_label, redundant, other) = match op {
                    CompoundRefOp::Union => (
                        &codes::SUBSUMPTION_IN_SLOT_UNION,
                        "slot-union",
                        narrower,
                        wider,
                    ),
                    CompoundRefOp::Intersection => (
                        &codes::SUBSUMPTION_IN_SLOT_INTERSECTION,
                        "slot-intersection",
                        wider,
                        narrower,
                    ),
                };
                let message = format_subsumption_message(
                    op,
                    field.name.as_str(),
                    td.name.as_str(),
                    op_label,
                    redundant,
                    other,
                );
                diags.push(Diagnostic {
                    code: code.clone(),
                    severity: Severity::Warning,
                    span: Span::new(td.source_path.clone(), field.shape_span),
                    message,
                    related: vec![],
                    fix: None,
                });
            }
        }
    }

    /// Whether branch `a`'s region is a subset of `b`'s, for the two
    /// region-bearing branch kinds: a list count-range (same inner shape) or a
    /// value refinement (same base primitive). Any other pair is incomparable.
    fn branch_region_subset(a: &Shape, b: &Shape) -> bool {
        match (a, b) {
            (
                Shape::List {
                    inner: ia,
                    min: mina,
                    max: maxa,
                },
                Shape::List {
                    inner: ib,
                    min: minb,
                    max: maxb,
                },
            ) if ia == ib => crate::validate::cardinality_subset(*mina, *maxa, *minb, *maxb),
            (
                Shape::Refined {
                    base: ba,
                    refinement: ra,
                },
                Shape::Refined {
                    base: bb,
                    refinement: rb,
                },
            ) if ba == bb => crate::validate::refinement_region_subset(*ba, Some(ra), Some(rb)),
            _ => false,
        }
    }

    for field in &td.fields {
        let Ok(shape) = &field.parsed_shape else {
            continue;
        };
        walk(shape, td, field, diags);
    }
}

/// Yield the reference-branch names of every compound in `shape` whose
/// operator matches `op`. Descends through `Shape::List` wrappers and
/// recurses into Union / Intersection branches regardless of op match —
/// nesting may carry compounds of either flavor.
///
/// `Shape::CompoundReference { op, .. }` is treated as a compound of
/// reference names; only branches whose op matches `op` are collected.
///
/// Non-reference branches in a matching compound (primitives, enums,
/// lists, nested compounds) are skipped — they have no closure to compare.
fn collect_slot_compound_branch_names(shape: &Shape, op: CompoundRefOp) -> Vec<Vec<&str>> {
    fn collect_ref_names<'a>(branches: &'a [Shape]) -> Vec<&'a str> {
        branches
            .iter()
            .filter_map(|b| match b {
                Shape::Reference(n) | Shape::Record(n) | Shape::InlineOrReference(n) => {
                    Some(n.as_str())
                }
                _ => None,
            })
            .collect()
    }

    fn walk<'a>(shape: &'a Shape, op: CompoundRefOp, out: &mut Vec<Vec<&'a str>>) {
        match shape {
            Shape::Union(branches) => {
                if op == CompoundRefOp::Union {
                    let names = collect_ref_names(branches);
                    if names.len() >= 2 {
                        out.push(names);
                    }
                }
                for branch in branches {
                    walk(branch, op, out);
                }
            }
            Shape::Intersection(branches) => {
                if op == CompoundRefOp::Intersection {
                    let names = collect_ref_names(branches);
                    if names.len() >= 2 {
                        out.push(names);
                    }
                }
                for branch in branches {
                    walk(branch, op, out);
                }
            }
            Shape::List { inner, .. } => walk(inner, op, out),
            Shape::Pinned(inner) => walk(inner, op, out),
            Shape::CompoundReference {
                op: cop, branches, ..
            } if *cop == op => {
                if branches.len() >= 2 {
                    let names: Vec<&'a str> = branches.iter().map(|s| s.as_str()).collect();
                    out.push(names);
                }
            }
            Shape::Tuple(elements) => {
                for el in elements {
                    walk(el, op, out);
                }
            }
            Shape::CompoundReference { .. }
            | Shape::Primitive(_)
            | Shape::Enum(_)
            | Shape::Any
            | Shape::Opaque
            | Shape::Reference(_)
            | Shape::Record(_)
            | Shape::InlineOrReference(_)
            | Shape::DefReference(_)
            | Shape::Refined { .. } => {}
        }
    }
    let mut out = Vec::new();
    walk(shape, op, &mut out);
    out
}

/// Walk a `Shape` and yield every type-def name a `Shape::Reference` targets,
/// descending through `Shape::List` wrappers and into `Shape::Union` /
/// `Shape::Intersection` branches. Returns names in source order so
/// diagnostics are stable.
// ----- duplicate meta sub-regions -----

fn check_duplicate_meta(td: &TypeDef, diags: &mut Vec<Diagnostic>) {
    // Keyed on (type_name, repo): an own meta and a same-named PEER meta
    // (`display-meta` and `display-meta::other`) are DISTINCT types, so both may
    // coexist. Only a same-(name, repo) pair is a real duplicate.
    let mut first_seen: HashMap<(&TypeName, Option<&str>), &crate::typedef::MetaBlock> =
        HashMap::new();
    // `Option<Vec<MetaBlock>>::iter().flatten()` yields `&MetaBlock`. The
    // None case (no `meta:` key) and the Some(vec![]) case (suppression
    // marker per [[type-def meta::au-type-system]]) both flatten to an empty iterator — no duplicates.
    for block in td.meta_blocks.iter().flatten() {
        let key = (&block.type_name, block.repo.as_deref());
        let shown = match &block.repo {
            Some(r) => format!("{}::{}", block.type_name.as_str(), r),
            None => block.type_name.as_str().to_string(),
        };
        if let Some(prev) = first_seen.get(&key) {
            diags.push(Diagnostic {
                code: codes::DUPLICATE_META_BLOCK,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), block.block_span),
                message: format!(
                    "duplicate `meta:` sub-region for '{}' on type-def '{}'",
                    shown,
                    td.name.as_str()
                ),
                related: vec![Span::new(td.source_path.clone(), prev.block_span)],
                fix: None,
            });
        } else {
            first_seen.insert(key, block);
        }
    }
}

// ----- field redeclaration -----

/// Emit `field-redeclaration` for any field a type-def declares that an
/// ancestor already declares (width-only subtyping, [[type subtyping width-only::au-type-system]]).
///
/// This can co-fire with `mixin-collision` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]) on one
/// type-def: the parents disagree on a field's shape AND the type-def
/// redeclares it. The two are independent problems with independent fixes —
/// removing the redeclaration leaves the parent disagreement, and reconciling
/// the parents leaves the redeclaration. Suppressing either would hide a
/// cascading error that only surfaces after the other is fixed, so both fire.
fn check_field_redeclaration(graph: &TypeGraph, name: &TypeName, diags: &mut Vec<Diagnostic>) {
    let Some(td) = graph.get(name) else { return };
    if td.fields.is_empty() {
        return;
    }
    let mut ancestors: Vec<TypeName> = closure_of(graph, name)
        .into_iter()
        .filter(|n| n != name)
        .collect();
    ancestors.sort();

    for f in &td.fields {
        // Collect every ancestor that declares the same field name.
        // Diamond mixin can produce multiple origins for one field; the
        // diagnostic must surface them all so a fix isn't hidden behind
        // the lex-first ancestor.
        let mut origins: Vec<(&TypeName, &FieldDecl, &TypeDef)> = Vec::new();
        for ancestor_name in &ancestors {
            let Some(ancestor) = graph.get(ancestor_name) else {
                continue;
            };
            if let Some(origin_field) = ancestor.fields.iter().find(|af| af.name == f.name) {
                origins.push((ancestor_name, origin_field, ancestor));
            }
        }
        if origins.is_empty() {
            continue;
        }
        let origin_names: Vec<String> = origins
            .iter()
            .map(|(n, _, _)| format!("'{}'", n.as_str()))
            .collect();
        let inherited_from = if origin_names.len() == 1 {
            origin_names[0].clone()
        } else {
            format!(
                "{} and {}",
                origin_names[..origin_names.len() - 1].join(", "),
                origin_names[origin_names.len() - 1]
            )
        };
        diags.push(Diagnostic {
            code: codes::FIELD_REDECLARATION,
            severity: Severity::Error,
            span: Span::new(td.source_path.clone(), f.name_span),
            message: format!(
                "subtype '{}' redeclares field '{}' inherited from {}",
                name.as_str(),
                f.name.as_str(),
                inherited_from
            ),
            related: origins
                .iter()
                .map(|(_, decl, ancestor)| Span::new(ancestor.source_path.clone(), decl.name_span))
                .collect(),
            fix: None,
        });
    }
}

// ----- sealed-family reachability -----

fn check_sealed_reachability(graph: &TypeGraph, name: &TypeName, diags: &mut Vec<Diagnostic>) {
    let closure = closure_of(graph, name);
    for ancestor_name in &closure {
        if ancestor_name == name {
            continue;
        }
        let Some(ancestor) = graph.get(ancestor_name) else {
            continue;
        };
        if ancestor.sealed.is_empty() {
            continue;
        }
        // Type-def must be reachable through one of ancestor's listed branches:
        // some name X in ancestor.sealed must appear in closure(self) (excluding
        // the ancestor itself).
        let through_branch = ancestor
            .sealed
            .iter()
            .any(|branch| branch.name != *ancestor_name && closure.contains(&branch.name));
        if !through_branch {
            let Some(td) = graph.get(name) else { continue };
            diags.push(Diagnostic {
                code: codes::SEALED_NO_SURPRISE_CHILDREN,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), td.source_span),
                message: format!(
                    "type-def '{}' crosses sealed parent '{}' without going through any of its listed branches",
                    name.as_str(),
                    ancestor_name.as_str()
                ),
                related: vec![Span::new(
                    ancestor.source_path.clone(),
                    ancestor.source_span,
                )],
                fix: None,
            });
        }
    }
}

// ----- cycle detection -----

/// Defensive cap on `type:` chain depth during the recursive DFS that
/// detects cycles. Realistic knowledge bases stay well under 50 levels; the cap is
/// orders of magnitude above that, sized only to keep adversarial content
/// from blowing the thread stack. Hitting it surfaces as a distinct
/// diagnostic so the user sees the actionable bound, not a panic.
pub(crate) const MAX_TYPE_CHAIN_DEPTH: usize = 256;

fn check_cycles(graph: &TypeGraph, diags: &mut Vec<Diagnostic>) {
    let mut explored: HashSet<TypeName> = HashSet::new();
    for start in graph.names() {
        if explored.contains(start) {
            continue;
        }
        let mut path: Vec<TypeName> = Vec::new();
        let mut on_path: HashSet<TypeName> = HashSet::new();
        dfs(start, graph, &mut path, &mut on_path, &mut explored, diags);
    }
}

fn dfs(
    name: &TypeName,
    graph: &TypeGraph,
    path: &mut Vec<TypeName>,
    on_path: &mut HashSet<TypeName>,
    explored: &mut HashSet<TypeName>,
    diags: &mut Vec<Diagnostic>,
) {
    if on_path.contains(name) {
        if let Some(td) = graph.get(name) {
            let cycle_start = path
                .iter()
                .position(|n| n == name)
                .expect("on_path implies `name` is in path");
            let chain: Vec<&str> = path[cycle_start..]
                .iter()
                .chain(std::iter::once(name))
                .map(|n| n.as_str())
                .collect();
            diags.push(Diagnostic {
                code: codes::CYCLE_IN_TYPE_CHAIN,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), td.source_span),
                message: format!("cycle in `type:` chain: {}", chain.join(" -> ")),
                related: vec![],
                fix: None,
            });
        }
        return;
    }
    if explored.contains(name) {
        return;
    }
    if path.len() >= MAX_TYPE_CHAIN_DEPTH {
        if let Some(td) = graph.get(name) {
            diags.push(Diagnostic {
                code: codes::TYPE_CHAIN_DEPTH_EXCEEDED,
                severity: Severity::Error,
                span: Span::new(td.source_path.clone(), td.source_span),
                message: format!(
                    "type chain reached the maximum depth of {} via '{}'; flatten the inheritance hierarchy or check for a cycle the load-time check missed",
                    MAX_TYPE_CHAIN_DEPTH,
                    name.as_str()
                ),
                related: vec![],
                fix: None,
            });
        }
        return;
    }

    on_path.insert(name.clone());
    path.push(name.clone());

    if let Some(td) = graph.get(name) {
        for parent in &td.parents {
            // A `::repo` parent is a peer type resolved by the cross-repo fold,
            // not a node in this graph. Following it by its bare name would
            // re-enter a local same-named type and report a phantom cycle (a
            // legal `note` extends `note::base`). The sibling walkers skip
            // qualified refs for the same reason; a genuine cross-repo cycle is
            // the fold's concern, not this per-repo check.
            if parent.is_qualified() {
                continue;
            }
            dfs(&parent.name, graph, path, on_path, explored, diags);
        }
    }

    path.pop();
    on_path.remove(name);
    explored.insert(name.clone());
}

/// [[type-instance type::au-type-system]] redundant-claims rule.
///
/// Scans a `type:` claim list for two redundancy patterns:
/// 1. **Duplicate claim** — repeated name in the list. Emits one
///    `duplicate-claim` per occurrence beyond the first; primary span on
///    the duplicate, related span on the first occurrence.
/// 2. **Subsumption** — one claim's closure includes another. Emits one
///    `subsumption-in-mixin` per redundant (wider) claim, naming the
///    narrower claim that implies it. Pure-duplicate pairs are skipped
///    (the duplicate-claim diag covers them); pairs involving an unknown
///    name are skipped (`unknown-type-claim` covers those).
///
/// Both diagnostics are `Severity::Warning` — closure dedupes silently;
/// the warning surfaces likely authoring mistakes. Single-claim and
/// 1-element lists are no-ops.
///
/// Used by both validate (instance `TypeClaim::List`) and the type-def
/// load check (parents list). The `file_path` argument carries whichever
/// file the claims live in.
/// Render a claim's source form for a diagnostic message, `name` or
/// `name::repo`.
fn claim_display(c: &TypeNameClaim) -> String {
    match &c.repo {
        Some(repo) => format!("{}::{}", c.name.as_str(), repo),
        None => c.name.as_str().to_string(),
    }
}

pub fn check_redundant_claims(
    graph: &TypeGraph,
    resolution: Option<&crate::resolution::ResolutionGraph>,
    claims: &[TypeNameClaim],
    file_path: &Path,
) -> Vec<Diagnostic> {
    if claims.len() < 2 {
        return Vec::new();
    }
    let mut diags = Vec::new();

    // Duplicate detection — independent of graph state. The key is the full
    // `(name, repo)` identity, so an own `foo` and a peer `foo::repo` are
    // distinct claims (not a duplicate), while two `foo::repo` are.
    let mut first_seen: BTreeMap<(&str, Option<&str>), au_diagnostics::ByteRange> = BTreeMap::new();
    for c in claims {
        let key = (c.name.as_str(), c.repo.as_deref());
        let shown = claim_display(c);
        if let Some(&first_span) = first_seen.get(&key) {
            diags.push(Diagnostic {
                code: codes::DUPLICATE_CLAIM,
                severity: Severity::Warning,
                span: Span::new(file_path.to_path_buf(), c.span),
                message: format!("claim '{shown}' is repeated"),
                related: vec![Span::new(file_path.to_path_buf(), first_span)],
                fix: None,
            });
        } else {
            first_seen.insert(key, c.span);
        }
    }

    // Subsumption detection — an ancestor claim in the same mixin is redundant.
    // An importing instance (resolution present) resolves every claim, own or
    // peer, to a folded `TypeId` and tests ancestry over the resolution graph,
    // at parity with the own path and subsuming it (every own def is a fold
    // seed). A single-repo instance walks the own graph by name. This closes
    // finding 3.6: `type: [child::base, parent::base]` was silently unwarned,
    // where single-repo `type: [child, parent]` warns.
    match resolution {
        Some(rg) => {
            for (i, ci) in claims.iter().enumerate() {
                let Some(ci_id) = rg.resolve_authored(&ci.name, ci.repo.as_deref()) else {
                    continue; // unresolvable — the crosstype gate owns it
                };
                let closure_i = folded_closure_ids(rg, &TypeClaim::Bare(ci.clone()));
                for (j, cj) in claims.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    let Some(cj_id) = rg.resolve_authored(&cj.name, cj.repo.as_deref()) else {
                        continue;
                    };
                    // Same identity is `duplicate-claim`'s (keyed on (name, repo)),
                    // or two authored forms of one id — not an ancestor subsumption.
                    if cj_id == ci_id {
                        continue;
                    }
                    // ci's folded closure includes cj → cj is an ancestor of ci,
                    // so cj is the wider/redundant claim and ci implies it.
                    if closure_i.contains(cj_id) {
                        diags.push(Diagnostic {
                            code: codes::SUBSUMPTION_IN_MIXIN,
                            severity: Severity::Warning,
                            span: Span::new(file_path.to_path_buf(), cj.span),
                            message: format!(
                                "claim '{}' is implied by '{}' and is redundant",
                                claim_display(cj),
                                claim_display(ci)
                            ),
                            related: vec![Span::new(file_path.to_path_buf(), ci.span)],
                            fix: None,
                        });
                    }
                }
            }
        }
        None => {
            // Single-repo own-graph walk, keyed by name. Skip a `::repo` peer claim
            // (its closure is the peer graph's, resolved by the fold — but with no
            // resolution graph the repo does not import, so a `::repo` claim here is
            // the engine's gate's), and pairs where either claim is absent
            // (`unknown-type-claim` covers those) or the names are identical
            // (`duplicate-claim` covers those).
            for (i, ci) in claims.iter().enumerate() {
                if ci.is_qualified() || !graph.contains(&ci.name) {
                    continue;
                }
                let closure_i = closure_of(graph, &ci.name);
                for (j, cj) in claims.iter().enumerate() {
                    if i == j || cj.is_qualified() || cj.name == ci.name {
                        continue;
                    }
                    if !graph.contains(&cj.name) {
                        continue;
                    }
                    // ci's closure includes cj → cj is an ancestor of ci, so cj
                    // is the wider/redundant claim and ci implies it.
                    if closure_i.contains(&cj.name) {
                        diags.push(Diagnostic {
                            code: codes::SUBSUMPTION_IN_MIXIN,
                            severity: Severity::Warning,
                            span: Span::new(file_path.to_path_buf(), cj.span),
                            message: format!(
                                "claim '{}' is implied by '{}' and is redundant",
                                cj.name.as_str(),
                                ci.name.as_str()
                            ),
                            related: vec![Span::new(file_path.to_path_buf(), ci.span)],
                            fix: None,
                        });
                    }
                }
            }
        }
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::typedef::{
        FieldDecl, FieldName, MetaBlock, ParentClaim, ParentClaimForm, TypeDef, TypeName,
        TypeNameClaim,
    };
    use au_diagnostics::{ByteRange, DiagnosticCode};
    use au_grammar::DefBound;
    use std::path::PathBuf;

    fn empty_td(name: &str) -> TypeDef {
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim: None,
            parents: vec![],
            fields: vec![],
            sealed: vec![],
            declared_abstract: false,
            meta_blocks: None,
            required_meta: Vec::new(),
            body: None,
            doc: None,
            ..Default::default()
        }
    }

    fn td_parents(name: &str, parents: &[&str]) -> TypeDef {
        TypeDef {
            shape: None,
            parent_claim: Some(ParentClaim {
                form: ParentClaimForm::List,
                value_span: ByteRange::new(0, 0),
            }),
            parents: parents
                .iter()
                .map(|p| TypeNameClaim::own(TypeName((*p).into()), ByteRange::new(0, 0)))
                .collect(),
            ..empty_td(name)
        }
    }

    fn run(defs: Vec<TypeDef>) -> Vec<Diagnostic> {
        let g = build_graph(defs).graph;
        run_graph_structure_checks(&g)
    }

    fn brand(shape: Shape) -> crate::typedef::BrandShape {
        crate::typedef::BrandShape {
            shape,
            member_docs: std::collections::BTreeMap::new(),
            span: ByteRange::new(0, 0),
        }
    }

    #[test]
    fn brand_with_fields_fires_brand_with_record_keys() {
        let mut td = td_with_fields("bad", &[], &["a"]);
        td.shape = Some(brand(Shape::Enum(vec!["x".into()])));
        let diags = run(vec![td]);
        assert!(diags
            .iter()
            .any(|d| d.code == codes::BRAND_WITH_RECORD_KEYS));
    }

    #[test]
    fn a_plain_brand_fires_no_brand_conflict() {
        let mut td = empty_td("meter");
        td.shape = Some(brand(Shape::Primitive(au_grammar::Primitive::Number)));
        let diags = run(vec![td]);
        assert!(!diags
            .iter()
            .any(|d| d.code == codes::BRAND_WITH_RECORD_KEYS));
    }

    fn parse_def(path: &str, src: &str) -> TypeDef {
        let docs = au_parser::yaml::parse(src).unwrap();
        crate::typedef::parse_type_def(std::path::Path::new(path), src, 0, &docs[0])
            .type_def
            .unwrap()
    }

    #[test]
    fn a_star_reference_to_a_nominal_enum_brand_is_not_referenceable() {
        let ir = parse_def("/v/type/ir.type.yaml", "shape:\n  - a\n  - b\n");
        let host = parse_def("/v/type/host.type.yaml", "fields:\n  f: ir*\n");
        let diags = run(vec![ir, host]);
        assert!(diags
            .iter()
            .any(|d| d.code == codes::BRAND_NOT_REFERENCEABLE));
    }

    #[test]
    fn a_bare_name_slot_to_a_nominal_brand_is_not_flagged() {
        let ir = parse_def("/v/type/ir.type.yaml", "shape:\n  - a\n  - b\n");
        let host = parse_def("/v/type/host.type.yaml", "fields:\n  f: ir\n");
        let diags = run(vec![ir, host]);
        assert!(!diags
            .iter()
            .any(|d| d.code == codes::BRAND_NOT_REFERENCEABLE));
    }

    fn run_location(defs: Vec<TypeDef>) -> Vec<Diagnostic> {
        // Own-graph path (no imports); the cross-repo path is exercised by an
        // au-engine two-repo integration test.
        super::run_location_checks(&build_graph(defs).graph, None, None)
    }

    #[test]
    fn location_name_over_required_safe_scalars_is_clean() {
        let td = parse_def(
            "/v/type/plan.type.yaml",
            "fields:\n  createdAt: Date\n  count: Number\nlocation:\n  name: \"${.type} - ${.createdAt} - ${.count}\"\n",
        );
        assert!(run_location(vec![td]).is_empty());
    }

    #[test]
    fn location_name_over_url_field_is_bad_shape() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields:\n  site: Url\nlocation:\n  name: \"${.site}\"\n",
        );
        assert!(run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_BAD_SHAPE));
    }

    #[test]
    fn location_name_over_list_field_is_bad_shape() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields:\n  tags: \"String[]\"\nlocation:\n  name: \"${.tags}\"\n",
        );
        assert!(run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_BAD_SHAPE));
    }

    #[test]
    fn location_name_over_optional_field_is_bad_shape() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields:\n  slug?: String\nlocation:\n  name: \"${.slug}\"\n",
        );
        assert!(run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_BAD_SHAPE));
    }

    #[test]
    fn location_name_over_absent_field_is_bad_shape() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields:\n  a: String\nlocation:\n  name: \"${.ghost}\"\n",
        );
        assert!(run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_BAD_SHAPE));
    }

    #[test]
    fn location_name_inherited_field_resolves() {
        let base = parse_def("/v/type/base.type.yaml", "fields:\n  slug: String\n");
        let child = parse_def(
            "/v/type/child.type.yaml",
            "extends: base\nlocation:\n  name: \"${.slug}\"\n",
        );
        assert!(run_location(vec![base, child]).is_empty());
    }

    #[test]
    fn location_filetype_yaml_with_body_conflicts() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields: {}\nbody:\n  - section: S\nlocation:\n  fileType: yaml\n",
        );
        assert!(run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_FILETYPE_BODY_CONFLICT));
    }

    #[test]
    fn location_filetype_md_with_body_is_clean() {
        let td = parse_def(
            "/v/type/x.type.yaml",
            "fields: {}\nbody:\n  - section: S\nlocation:\n  fileType: md\n",
        );
        assert!(!run_location(vec![td])
            .iter()
            .any(|d| d.code == codes::LOCATION_FILETYPE_BODY_CONFLICT));
    }

    #[test]
    fn a_nominal_brand_reference_inside_a_tuple_element_is_flagged() {
        // A `*` on a nominal brand escapes into a tuple element; the
        // referenceability walker must descend into the tuple (code-review 2.3).
        let ir = parse_def("/v/type/ir.type.yaml", "shape:\n  - a\n  - b\n");
        let host = parse_def("/v/type/host.type.yaml", "fields:\n  f: (ir*, String)\n");
        let diags = run(vec![ir, host]);
        assert!(
            diags
                .iter()
                .any(|d| d.code == codes::BRAND_NOT_REFERENCEABLE),
            "a nominal-brand `*` inside a tuple element must be flagged"
        );
    }

    #[test]
    fn a_star_on_an_all_record_union_brand_is_referenceable() {
        // Every member is a record type, so the union brand is referenceable.
        let paper = parse_def("/v/type/paper.type.yaml", "fields:\n  t: String\n");
        let obs = parse_def("/v/type/observation.type.yaml", "fields:\n  t: String\n");
        let ek = parse_def(
            "/v/type/evidence-kind.type.yaml",
            "shape: <paper | observation>\n",
        );
        let host = parse_def("/v/type/host.type.yaml", "fields:\n  e: evidence-kind*\n");
        let diags = run(vec![paper, obs, ek, host]);
        assert!(
            !diags
                .iter()
                .any(|d| d.code == codes::BRAND_NOT_REFERENCEABLE),
            "an all-record union brand takes '*': {:?}",
            diags.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_star_on_a_mixed_member_union_brand_is_not_referenceable() {
        // A primitive member (`String`) makes the whole union brand inline-only.
        let evidence = parse_def("/v/type/evidence.type.yaml", "fields:\n  t: String\n");
        let ek = parse_def(
            "/v/type/evidence-kind.type.yaml",
            "shape: <evidence | String>\n",
        );
        let host = parse_def("/v/type/host.type.yaml", "fields:\n  e: evidence-kind*\n");
        let diags = run(vec![evidence, ek, host]);
        assert!(
            diags
                .iter()
                .any(|d| d.code == codes::BRAND_NOT_REFERENCEABLE),
            "a mixed-member union brand cannot take '*'"
        );
    }

    #[test]
    fn type_name_regex_accepts_valid() {
        assert!(is_valid_type_name("note"));
        assert!(is_valid_type_name("decision"));
        assert!(is_valid_type_name("decision.decided"));
        assert!(is_valid_type_name("source.url.canonical"));
        assert!(is_valid_type_name("a-b_c"));
        assert!(is_valid_type_name("a1.b2"));
    }

    #[test]
    fn type_name_regex_rejects_invalid() {
        assert!(!is_valid_type_name(""));
        assert!(!is_valid_type_name("1note"));
        assert!(!is_valid_type_name("note*"));
        assert!(!is_valid_type_name("note bar"));
        assert!(!is_valid_type_name(".decision"));
        assert!(!is_valid_type_name("decision."));
        assert!(!is_valid_type_name("a..b"));
        assert!(!is_valid_type_name("a.1b"));
    }

    #[test]
    fn flags_invalid_type_name() {
        let bad = empty_td("note*");
        let diags = run(vec![bad]);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "type-name-violates-regex"));
    }

    #[test]
    fn bare_name_parent_claim_no_longer_flagged() {
        // Rule 3 ([[type list form::au-type-system]]): bare scalar is the natural single-claim form on
        // type-defs as well; no load error.
        let mut td = td_parents("decision", &["note"]);
        td.parent_claim = Some(ParentClaim {
            form: ParentClaimForm::BareName,
            value_span: ByteRange::new(0, 0),
        });
        let diags = run(vec![empty_td("note"), td]);
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "a bare-name parent claim is accepted without error, got {diags:?}"
        );
    }

    #[test]
    fn qualified_parent_is_deferred_not_absent() {
        // `type: base::peer` is a peer parent resolved by the cross-repo fold;
        // au-core defers it, so `parent-references-absent-type` must not fire
        // even though `base` is absent from this own graph.
        let mut td = empty_td("child");
        td.parent_claim = Some(ParentClaim {
            form: ParentClaimForm::BareName,
            value_span: ByteRange::new(0, 0),
        });
        td.parents = vec![TypeNameClaim::parse("base::peer", ByteRange::new(0, 0))];
        let diags = run(vec![td]);
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() != "parent-references-absent-type"),
            "qualified parent must not fire absent-type: {diags:?}"
        );
    }

    #[test]
    fn duplicate_meta_blocks_emit_one_diag_per_dup() {
        let mut td = empty_td("decision");
        td.meta_blocks = Some(vec![
            MetaBlock {
                type_name: TypeName("display-meta".into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                block_span: ByteRange::new(10, 20),
                fields: vec![],
                body_span: ByteRange::new(10, 20),
                doc: None,
                field_docs: Default::default(),
            },
            MetaBlock {
                type_name: TypeName("display-meta".into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                block_span: ByteRange::new(30, 40),
                fields: vec![],
                body_span: ByteRange::new(30, 40),
                doc: None,
                field_docs: Default::default(),
            },
            MetaBlock {
                type_name: TypeName("runtime-meta".into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                block_span: ByteRange::new(50, 60),
                fields: vec![],
                body_span: ByteRange::new(50, 60),
                doc: None,
                field_docs: Default::default(),
            },
        ]);
        let diags = run(vec![td]);
        let dups: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "duplicate-meta-block")
            .collect();
        assert_eq!(dups.len(), 1);
        assert!(!dups[0].related.is_empty());
    }

    #[test]
    fn detects_self_cycle() {
        let td = td_parents("self", &["self"]);
        let diags = run(vec![td]);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.code.as_str() == "cycle-in-type-chain")
                .count(),
            1
        );
    }

    #[test]
    fn detects_two_node_cycle() {
        let diags = run(vec![td_parents("a", &["b"]), td_parents("b", &["a"])]);
        // One cycle, one diagnostic — explored set prevents double-emit.
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.code.as_str() == "cycle-in-type-chain")
                .count(),
            1
        );
    }

    #[test]
    fn acyclic_graph_emits_no_cycle_diag() {
        let diags = run(vec![
            empty_td("note"),
            td_parents("decision", &["note"]),
            td_parents("decision.decided", &["decision"]),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "cycle-in-type-chain"));
    }

    #[test]
    fn qualified_parent_sharing_a_bare_name_is_not_a_cycle() {
        // A legal vendored same-name type: local `note` mixes in the peer
        // `note::base`. The cross-repo fold owns the peer parent, so the local
        // cycle DFS must not follow it by its bare name `note`, re-enter the
        // on-path local `note`, and report a phantom `note -> note` cycle.
        let mut td = empty_td("note");
        td.parent_claim = Some(ParentClaim {
            form: ParentClaimForm::List,
            value_span: ByteRange::new(0, 0),
        });
        td.parents = vec![TypeNameClaim::parse("note::base", ByteRange::new(0, 0))];
        let diags = run(vec![td]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "cycle-in-type-chain"));
    }

    #[test]
    fn deep_chain_exceeding_max_depth_fires_type_chain_depth_exceeded() {
        // Build a linear chain one node deeper than the cap. Names are
        // zero-padded so lex-order (the order `check_cycles` iterates) puts
        // the deepest leaf first — otherwise the `explored` set short-
        // circuits ancestors before the recursion approaches the cap.
        let depth = MAX_TYPE_CHAIN_DEPTH + 1;
        let name = |i: usize| format!("t{i:06}");
        let mut defs = Vec::with_capacity(depth);
        defs.push(empty_td(&name(depth - 1))); // root has no parents
        for i in 0..depth - 1 {
            defs.push(td_parents(&name(i), &[&name(i + 1)]));
        }
        let diags = run(defs);
        let depth_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "type-chain-depth-exceeded")
            .collect();
        assert!(
            !depth_diags.is_empty(),
            "expected at least one type-chain-depth-exceeded diag, got codes: {:?}",
            diags.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
        // No cycle diag should fire — this is a depth issue, not a cycle.
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "cycle-in-type-chain"));
    }

    #[test]
    fn diamond_inheritance_does_not_trigger_cycle() {
        // Diamond: d → b, d → c, b → a, c → a.
        let diags = run(vec![
            empty_td("a"),
            td_parents("b", &["a"]),
            td_parents("c", &["a"]),
            td_parents("d", &["b", "c"]),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "cycle-in-type-chain"));
    }

    #[test]
    fn parent_with_bad_name_flagged() {
        let td = td_parents("decision", &["note*"]);
        let diags = run(vec![td]);
        // The graph also reports the same diag transitively through td's parent
        // claim — at least one regex diagnostic must surface.
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "type-name-violates-regex"));
    }

    #[test]
    fn sealed_branch_with_bad_name_flagged() {
        let mut td = empty_td("source");
        td.sealed = vec![TypeNameClaim::own(
            TypeName("source.url*".into()),
            ByteRange::new(0, 0),
        )];
        let diags = run(vec![td]);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "type-name-violates-regex"));
    }

    fn td_with_fields(name: &str, parents: &[&str], field_names: &[&str]) -> TypeDef {
        TypeDef {
            shape: None,
            fields: field_names
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
            ..td_parents(name, parents)
        }
    }

    fn td_sealed(name: &str, sealed_branches: &[&str]) -> TypeDef {
        TypeDef {
            shape: None,
            sealed: sealed_branches
                .iter()
                .map(|s| TypeNameClaim::own(TypeName((*s).into()), ByteRange::new(0, 0)))
                .collect(),
            ..empty_td(name)
        }
    }

    fn td_abstract(name: &str) -> TypeDef {
        TypeDef {
            shape: None,
            declared_abstract: true,
            ..empty_td(name)
        }
    }

    fn count_code(diags: &[Diagnostic], code: &str) -> usize {
        diags.iter().filter(|d| d.code.as_str() == code).count()
    }

    #[test]
    fn required_meta_absent_type_fires() {
        // A bare `required:` naming a type absent from the graph is an error.
        let mut base = empty_td("base");
        base.required_meta = vec![TypeNameClaim::own(
            TypeName("ghost-meta".into()),
            ByteRange::new(0, 0),
        )];
        let diags = run(vec![base]);
        let hits: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "required-meta-absent-type")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].severity, Severity::Error);
    }

    #[test]
    fn required_meta_present_type_fires_no_absent() {
        // A present required target does not fire absent-type (meta-ness is a
        // separate resolution-graph check).
        let mut base = empty_td("base");
        base.required_meta = vec![TypeNameClaim::own(
            TypeName("pm".into()),
            ByteRange::new(0, 0),
        )];
        let diags = run(vec![base, empty_td("pm")]);
        assert_eq!(count_code(&diags, "required-meta-absent-type"), 0);
    }

    #[test]
    fn redundant_abstract_on_sealed_fires_as_hint() {
        let diags = run(vec![
            TypeDef {
                shape: None,
                declared_abstract: true,
                ..td_sealed("decision", &["decision.pending"])
            },
            td_parents("decision.pending", &["decision"]),
        ]);
        let hits: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "redundant-abstract-on-sealed")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].severity, Severity::Hint);
    }

    #[test]
    fn an_abstract_base_with_no_descendant_is_not_diagnosed() {
        // A cross-repo abstract interface with no local subtype is legitimate,
        // so no diagnostic fires. A plain concrete sealed type also stays clean.
        let diags = run(vec![td_abstract("pane")]);
        assert_eq!(count_code(&diags, "redundant-abstract-on-sealed"), 0);
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() != "abstract-type-unrealizable"),
            "no unrealizable code exists any more"
        );
    }

    fn run_inheritance(defs: Vec<TypeDef>) -> Vec<Diagnostic> {
        let g = build_graph(defs).graph;
        run_inheritance_checks(&g)
    }

    #[test]
    fn redeclare_at_one_level_is_flagged() {
        let diags = run_inheritance(vec![
            td_with_fields("note", &[], &["description"]),
            td_with_fields("decision", &["note"], &["description"]),
        ]);
        let dups: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "field-redeclaration")
            .collect();
        assert_eq!(dups.len(), 1);
        assert!(dups[0].message.contains("note"));
    }

    #[test]
    fn redeclare_at_grandparent_is_flagged() {
        let diags = run_inheritance(vec![
            td_with_fields("a", &[], &["x"]),
            td_with_fields("b", &["a"], &[]),
            td_with_fields("c", &["b"], &["x"]),
        ]);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "field-redeclaration"));
    }

    #[test]
    fn redeclare_at_diamond_mixin_lists_every_ancestor() {
        // D claims `type: [b, c]`; both b and c declare field `x`; D
        // also declares `x`. The redeclare diagnostic must name BOTH
        // ancestor origins so a fix isn't hidden behind the lex-first
        // ancestor — user shouldn't fix b, re-validate, then discover c.
        let diags = run_inheritance(vec![
            td_with_fields("b", &[], &["x"]),
            td_with_fields("c", &[], &["x"]),
            td_with_fields("d", &["b", "c"], &["x"]),
        ]);
        let redecls: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "field-redeclaration")
            .collect();
        assert_eq!(redecls.len(), 1, "expected one diag, got {redecls:?}");
        assert_eq!(
            redecls[0].related.len(),
            2,
            "expected both ancestor spans in related, got {:?}",
            redecls[0].related
        );
        assert!(
            redecls[0].message.contains("'b'") && redecls[0].message.contains("'c'"),
            "expected both ancestor names in message, got {:?}",
            redecls[0].message
        );
    }

    #[test]
    fn no_redeclare_means_no_diagnostic() {
        let diags = run_inheritance(vec![
            td_with_fields("note", &[], &["description"]),
            td_with_fields("decision", &["note"], &["status"]),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "field-redeclaration"));
    }

    #[test]
    fn sealed_listed_branch_passes() {
        let diags = run_inheritance(vec![
            td_sealed("source", &["source.url", "source.path"]),
            td_parents("source.url", &["source"]),
            td_parents("source.path", &["source"]),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "sealed-no-surprise-children"));
    }

    #[test]
    fn sealed_unlisted_descendant_is_flagged() {
        let diags = run_inheritance(vec![
            td_sealed("source", &["source.url", "source.path"]),
            td_parents("source.malformed", &["source"]),
        ]);
        let surprise: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "sealed-no-surprise-children")
            .collect();
        assert_eq!(surprise.len(), 1);
        assert!(surprise[0].message.contains("source.malformed"));
    }

    #[test]
    fn nested_sealed_chain_is_reachable_through_branch() {
        let diags = run_inheritance(vec![
            td_sealed("source", &["source.url", "source.path"]),
            td_parents("source.url", &["source"]),
            td_parents("source.url.canonical", &["source.url"]),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "sealed-no-surprise-children"));
    }

    // ----- reserved type-def names + slot reference targets -----

    fn td_with_reference_field(name: &str, field: &str, target: &str) -> TypeDef {
        TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName(field.into()),
                optional: false,
                raw_shape: format!("{}*", target),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Reference(target.into())),
                doc: None,
            }],
            ..empty_td(name)
        }
    }

    fn td_with_list_of_reference(name: &str, field: &str, target: &str) -> TypeDef {
        TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName(field.into()),
                optional: false,
                raw_shape: format!("{}*[]", target),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::List {
                    inner: Box::new(au_grammar::Shape::Reference(target.into())),
                    min: 0,
                    max: None,
                }),
                doc: None,
            }],
            ..empty_td(name)
        }
    }

    fn td_with_def_ref_field(name: &str, field: &str, bound: Option<&str>) -> TypeDef {
        let (raw, shape) = match bound {
            Some(t) => (
                format!("type<{}>*", t),
                Shape::DefReference(Some(DefBound::Single(t.into()))),
            ),
            None => ("type*".to_string(), Shape::DefReference(None)),
        };
        TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName(field.into()),
                optional: false,
                raw_shape: raw,
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(shape),
                doc: None,
            }],
            ..empty_td(name)
        }
    }

    #[test]
    fn def_ref_bound_to_absent_type_fires_slot_references_absent_type() {
        // `type<mcp.tool>*` names `mcp.tool` as a ceiling; if it's absent the
        // bound is a dangling slot ([[type-def shape def-ref::au-type-system]]).
        let diags = run(vec![td_with_def_ref_field(
            "mode",
            "propose_tool",
            Some("mcp.tool"),
        )]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("mcp.tool"));
    }

    #[test]
    fn pinned_reference_to_absent_type_fires_slot_references_absent_type() {
        // `T*@` ([[type-def shape suffixes::au-type-system]]) wraps a reference; its bound type must
        // exist, so the absent-type check descends through the pin.
        let pinned_field = TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName("touched".into()),
                optional: false,
                raw_shape: "absent*@".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Pinned(Box::new(
                    au_grammar::Shape::Reference("absent".into()),
                ))),
                doc: None,
            }],
            ..empty_td("log")
        };
        let diags = run(vec![pinned_field]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("absent"));
    }

    #[test]
    fn def_ref_bound_to_present_type_passes() {
        let diags = run(vec![
            empty_td("mcp.tool"),
            td_with_def_ref_field("mode", "propose_tool", Some("mcp.tool")),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "slot-references-absent-type"));
    }

    #[test]
    fn unconstrained_def_ref_names_no_type() {
        // `type*` has no ceiling, so it never fires slot-references-absent-type.
        let diags = run(vec![td_with_def_ref_field("mode", "any_def", None)]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "slot-references-absent-type"));
    }

    #[test]
    fn reserved_name_file_is_flagged() {
        let diags = run(vec![empty_td("file")]);
        let reserved: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "reserved-type-name")
            .collect();
        assert_eq!(reserved.len(), 1);
        assert!(reserved[0].message.contains("file"));
        assert!(reserved[0].message.contains("any-repo-file"));
    }

    #[test]
    fn reserved_name_any_is_flagged() {
        let diags = run(vec![empty_td("any")]);
        let reserved: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "reserved-type-name")
            .collect();
        assert_eq!(reserved.len(), 1);
        assert!(reserved[0].message.contains("any"));
        assert!(reserved[0].message.contains("no-type slot shape"));
    }

    #[test]
    fn reserved_name_opaque_is_flagged() {
        let diags = run(vec![empty_td("opaque")]);
        let reserved: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "reserved-type-name")
            .collect();
        assert_eq!(reserved.len(), 1);
        assert!(reserved[0].message.contains("opaque"));
        assert!(reserved[0].message.contains("uninterpreted slot shape"));
    }

    #[test]
    fn reserved_name_primitives_are_flagged() {
        let diags = run(vec![
            empty_td("String"),
            empty_td("Number"),
            empty_td("Boolean"),
            empty_td("Date"),
            empty_td("DateTime"),
            empty_td("Url"),
        ]);
        let reserved: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "reserved-type-name")
            .collect();
        assert_eq!(reserved.len(), 6);
        for primitive in ["String", "Number", "Boolean", "Date", "DateTime", "Url"] {
            let hit = reserved
                .iter()
                .find(|d| d.message.contains(&format!("'{}'", primitive)));
            assert!(
                hit.is_some(),
                "no reserved-type-name diagnostic mentions '{}'",
                primitive
            );
            assert!(
                hit.unwrap().message.contains("primitive shape"),
                "diagnostic for '{}' doesn't say 'primitive shape': {}",
                primitive,
                hit.unwrap().message
            );
        }
    }

    #[test]
    fn non_reserved_names_pass() {
        // Names that resemble reserved ones but aren't exact matches stay
        // claimable. Reservation is case-sensitive and exact.
        let diags = run(vec![
            empty_td("file_thing"),
            empty_td("Files"),
            empty_td("myfile"),
            empty_td("MyString"),
            empty_td("Strings"),
            empty_td("string"),
            empty_td("MyDateTime"),
            empty_td("File"),
            empty_td("MyUrl"),
            empty_td("Urls"),
            empty_td("url"),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "reserved-type-name"));
    }

    #[test]
    fn slot_references_absent_type_fires() {
        // `note` doesn't exist; `link-card.target: note*` should flag.
        let diags = run(vec![td_with_reference_field("link-card", "target", "note")]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("note"));
        assert!(absent[0].message.contains("target"));
    }

    #[test]
    fn slot_references_existing_type_passes() {
        let diags = run(vec![
            empty_td("note"),
            td_with_reference_field("link-card", "target", "note"),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "slot-references-absent-type"));
    }

    #[test]
    fn slot_references_builtin_file_passes() {
        // `file*` is the engine built-in; no closure check, no absent diagnostic.
        let diags = run(vec![td_with_reference_field("attachment", "blob", "file")]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "slot-references-absent-type"));
    }

    #[test]
    fn parent_references_absent_type_fires() {
        // `note` claims parent `thing`, which is not defined; the closure
        // truncates silently today, so this must flag.
        let diags = run(vec![td_parents("note", &["thing"])]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "parent-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("thing"));
        assert!(absent[0].message.contains("note"));
    }

    #[test]
    fn parent_references_existing_type_passes() {
        let diags = run(vec![empty_td("thing"), td_parents("note", &["thing"])]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "parent-references-absent-type"));
    }

    #[test]
    fn an_invalid_parent_name_is_the_regex_checks_not_an_absent_type() {
        // `foo*bar` is not a valid type name; that is the regex check's report,
        // not a misleading absent-type one.
        let diags = run(vec![td_parents("note", &["foo*bar"])]);
        assert!(diags
            .iter()
            .any(|d| d.code.as_str() == "type-name-violates-regex"));
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "parent-references-absent-type"));
    }

    #[test]
    fn list_of_absent_type_fires() {
        // The check descends through `Shape::List` wrappers.
        let diags = run(vec![td_with_list_of_reference(
            "collection",
            "items",
            "note",
        )]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
    }

    #[test]
    fn list_of_builtin_file_passes() {
        let diags = run(vec![td_with_list_of_reference("gallery", "images", "file")]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "slot-references-absent-type"));
    }

    #[test]
    fn lowercase_primitive_typo_appends_hint() {
        // `string` parses as `Shape::Record("string")`.
        // No type-def named "string" → slot-references-absent-type
        // fires AND the message hints at the primitive String form.
        let td = TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName("body".into()),
                optional: false,
                raw_shape: "string".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Record("string".into())),
                doc: None,
            }],
            ..empty_td("host")
        };
        let diags = run(vec![td]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        // Severity stays Error — load still aborts.
        assert_eq!(absent[0].severity, Severity::Error);
        // Hint names the canonical primitive form.
        assert!(
            absent[0].message.contains("did you mean 'String'"),
            "expected 'did you mean String' hint; got: {}",
            absent[0].message
        );
    }

    #[test]
    fn each_primitive_lowercase_form_gets_hint() {
        for (typo, canon) in [
            ("string", "String"),
            ("number", "Number"),
            ("boolean", "Boolean"),
            ("date", "Date"),
            ("datetime", "DateTime"),
        ] {
            let td = TypeDef {
                shape: None,
                fields: vec![FieldDecl {
                    name: FieldName("body".into()),
                    optional: false,
                    raw_shape: typo.into(),
                    name_span: ByteRange::new(0, 0),
                    shape_span: ByteRange::new(0, 0),
                    entry_span: ByteRange::new(0, 0),
                    parsed_shape: Ok(au_grammar::Shape::Record(typo.into())),
                    doc: None,
                }],
                ..empty_td("host")
            };
            let diags = run(vec![td]);
            let msg = diags
                .iter()
                .find(|d| d.code.as_str() == "slot-references-absent-type")
                .map(|d| d.message.as_str())
                .unwrap_or("");
            assert!(
                msg.contains(&format!("did you mean '{}'", canon)),
                "for typo '{}', expected hint mentioning '{}'; got: {}",
                typo,
                canon,
                msg
            );
        }
    }

    #[test]
    fn truly_unknown_name_gets_no_primitive_hint() {
        // `foo` doesn't lowercase-match any primitive — bare diagnostic
        // without the hint.
        let td = TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName("body".into()),
                optional: false,
                raw_shape: "foo".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Record("foo".into())),
                doc: None,
            }],
            ..empty_td("host")
        };
        let diags = run(vec![td]);
        let msg = diags
            .iter()
            .find(|d| d.code.as_str() == "slot-references-absent-type")
            .map(|d| d.message.as_str())
            .unwrap_or("");
        assert!(
            !msg.contains("did you mean"),
            "non-primitive typo should NOT get a hint; got: {}",
            msg
        );
    }

    #[test]
    fn canonical_primitive_form_does_not_get_hint() {
        // The canonical form (`String`) is a recognized primitive in
        // au-grammar — so `target: String` parses as
        // `Shape::Primitive(String)`, NOT `Shape::Record("String")`.
        // The slot-references-absent-type check never sees "String"
        // as a referenced name; the test is a regression lock against
        // a future regression where String falls through to Record.
        // Constructing the Record-of-"String" case manually:
        let td = TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName("body".into()),
                optional: false,
                raw_shape: "String".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Record("String".into())),
                doc: None,
            }],
            ..empty_td("host")
        };
        let diags = run(vec![td]);
        let absent = diags
            .iter()
            .find(|d| d.code.as_str() == "slot-references-absent-type");
        // If a Record("String") ever does reach this check (which the
        // parser shouldn't produce today), no hint should fire — the
        // user already wrote the canonical form.
        if let Some(d) = absent {
            assert!(
                !d.message.contains("did you mean"),
                "exact-canonical form should not get a hint; got: {}",
                d.message
            );
        }
    }

    #[test]
    fn union_of_absent_types_fires_per_branch() {
        // `walk_refs` descends into Union/Intersection branches; each
        // missing target produces its own diagnostic in source order.
        let diags = run(vec![td_with_typed_field(
            "evidence",
            &[],
            "support",
            au_grammar::Shape::Union(vec![
                au_grammar::Shape::Reference("rationale".into()),
                au_grammar::Shape::Reference("thesis".into()),
            ]),
        )]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 2);
        assert!(absent[0].message.contains("rationale"));
        assert!(absent[1].message.contains("thesis"));
    }

    #[test]
    fn intersection_descent_finds_present_and_absent_branches() {
        // Existing branch (`rationale`) passes; missing branch (`thesis`)
        // fires once.
        let diags = run(vec![
            empty_td("rationale"),
            td_with_typed_field(
                "combo",
                &[],
                "joint",
                au_grammar::Shape::Intersection(vec![
                    au_grammar::Shape::Reference("rationale".into()),
                    au_grammar::Shape::Reference("thesis".into()),
                ]),
            ),
        ]);
        let absent: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "slot-references-absent-type")
            .collect();
        assert_eq!(absent.len(), 1);
        assert!(absent[0].message.contains("thesis"));
    }

    // ----- subsumption-in-slot-union (spec [[type-def shape compound::au-type-system]]) -----
    //
    // Symmetric to `subsumption-in-mixin` but on slot-union branches.
    // Fires only when both endpoints are reference branches whose closures
    // are comparable. Intersection slots are silent per [[type-def shape compound::au-type-system]].

    fn ref_(name: &str) -> au_grammar::Shape {
        au_grammar::Shape::Reference(name.into())
    }

    fn run_struct(defs: Vec<TypeDef>) -> Vec<Diagnostic> {
        let g = build_graph(defs).graph;
        run_graph_structure_checks(&g)
    }

    #[test]
    fn slot_union_with_subsumption_fires_warning() {
        // `decision.decided`'s closure includes `decision`, so `decision`
        // is the wider branch and is redundant.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "support",
                au_grammar::Shape::Union(vec![ref_("decision"), ref_("decision.decided")]),
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].severity, Severity::Warning);
        // Wider branch named in the redundant slot; narrower as the implier.
        assert!(subs[0].message.contains("'decision'"));
        assert!(subs[0].message.contains("'decision.decided'"));
    }

    #[test]
    fn slot_union_any_star_subsumes_file_star() {
        // `any*` ([[type-def shape any::au-type-system]]) is the universal reference target, wider than
        // `file*`, so `<file* | any*>` fires with `file*` redundant.
        let diags = run_struct(vec![td_with_typed_field(
            "ev",
            &[],
            "attachment",
            au_grammar::Shape::Union(vec![ref_("file"), ref_("any")]),
        )]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1, "exactly one subsumption, got {subs:?}");
        // `file` is the redundant (narrower) branch, `any` the wider.
        assert!(subs[0].message.contains("'file'"));
        assert!(subs[0].message.contains("'any'"));
    }

    #[test]
    fn slot_union_any_star_subsumes_typed_reference() {
        // `any*` subsumes any typed `T*` too.
        let diags = run_struct(vec![
            empty_td("note"),
            td_with_typed_field(
                "ev",
                &[],
                "link",
                au_grammar::Shape::Union(vec![ref_("note"), ref_("any")]),
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1, "exactly one subsumption, got {subs:?}");
        assert!(subs[0].message.contains("'note'"));
        assert!(subs[0].message.contains("'any'"));
    }

    #[test]
    fn slot_union_with_disjoint_branches_is_silent() {
        let diags = run_struct(vec![
            empty_td("rationale"),
            empty_td("thesis"),
            td_with_typed_field(
                "ev",
                &[],
                "support",
                au_grammar::Shape::Union(vec![ref_("rationale"), ref_("thesis")]),
            ),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"));
    }

    #[test]
    fn ternary_union_fires_only_for_subsuming_pair() {
        // `<decision.pending | decision.decided | decision>` — pending and
        // decided are siblings (no subsumption), but `decision` is implied
        // by both. We expect one warning per (narrower, decision) pair.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.pending", &["decision"]),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "support",
                au_grammar::Shape::Union(vec![
                    ref_("decision.pending"),
                    ref_("decision.decided"),
                    ref_("decision"),
                ]),
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        // Two narrower branches each imply `decision`, so two warnings fire
        // (mirrors `subsumption-in-mixin`'s behavior on the symmetric case).
        assert_eq!(subs.len(), 2);
        for s in &subs {
            assert!(s.message.contains("'decision'"));
        }
    }

    #[test]
    fn slot_intersection_with_subsumption_fires_warning() {
        // `<decision & decision.decided>` collapses to `decision.decided`
        // per spec [[type-def shape compound::au-type-system]]; the wider `decision` branch is redundant.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "joint",
                au_grammar::Shape::Intersection(vec![ref_("decision"), ref_("decision.decided")]),
            ),
        ]);
        // Sanity: the symmetric union code did NOT fire (intersection-only).
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"));
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-intersection")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].severity, Severity::Warning);
        assert!(subs[0].message.contains("'decision'"));
        assert!(subs[0].message.contains("'decision.decided'"));
        assert!(subs[0].message.contains("slot-intersection"));
    }

    #[test]
    fn slot_intersection_with_disjoint_branches_is_silent() {
        // `<decision & maturity>` — disjoint closures, intersection may
        // be uninhabited in practice but spec [[type-def shape compound::au-type-system]] explicitly leaves
        // uninhabitability unenforced. Only subsumption fires.
        let diags = run_struct(vec![
            empty_td("decision"),
            empty_td("maturity"),
            td_with_typed_field(
                "ev",
                &[],
                "joint",
                au_grammar::Shape::Intersection(vec![ref_("decision"), ref_("maturity")]),
            ),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-intersection"));
    }

    #[test]
    fn ternary_intersection_fires_per_subsuming_pair() {
        // `<decision.pending & decision.decided & decision>` — pending
        // and decided are siblings (no subsumption between them), but
        // each implies `decision`. Two pairs fire.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.pending", &["decision"]),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "joint",
                au_grammar::Shape::Intersection(vec![
                    ref_("decision.pending"),
                    ref_("decision.decided"),
                    ref_("decision"),
                ]),
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-intersection")
            .collect();
        assert_eq!(subs.len(), 2);
        for s in &subs {
            assert!(s.message.contains("'decision'"));
        }
    }

    #[test]
    fn slot_union_with_cardinality_refinement_fires_warning() {
        // [[type-def shape compound::au-type-system]]: `<T[] | T[+]>` — T[+] is a strict subset of T[]; the
        // wider branch subsumes the narrower. One warning per pair, same
        // code as closure-inclusion subsumption.
        let str_list = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
            min: 0,
            max: None,
        };
        let str_list_ne = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
            min: 1,
            max: None,
        };
        let diags = run_struct(vec![td_with_typed_field(
            "ev",
            &[],
            "tags",
            au_grammar::Shape::Union(vec![str_list, str_list_ne]),
        )]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].severity, Severity::Warning);
        // Union: narrower `String[+]` is the redundant branch; the
        // message names it as adding nothing because `String[]` is wider.
        assert!(subs[0].message.contains("'String[+]' adds nothing"));
        assert!(subs[0].message.contains("'String[]' is wider"));
    }

    #[test]
    fn slot_intersection_with_cardinality_refinement_fires_warning() {
        // [[type-def shape compound::au-type-system]] mirror: `<T[] & T[+]>` collapses to T[+]; T[] is redundant.
        let str_list = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
            min: 0,
            max: None,
        };
        let str_list_ne = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
            min: 1,
            max: None,
        };
        let diags = run_struct(vec![td_with_typed_field(
            "ev",
            &[],
            "tags",
            au_grammar::Shape::Intersection(vec![str_list, str_list_ne]),
        )]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-intersection")
            .collect();
        assert_eq!(subs.len(), 1);
        // Intersection: wider `String[]` is the redundant branch; the
        // intersection collapses to the stricter `String[+]`.
        assert!(subs[0].message.contains("'String[]' adds nothing"));
        assert!(subs[0].message.contains("'String[+]' is stricter"));
        assert!(subs[0].message.contains("collapses to 'String[+]'"));
    }

    #[test]
    fn slot_union_disjoint_inner_does_not_fire_cardinality_subsumption() {
        // `<String[] | Number[+]>` — different inner shapes, no
        // cardinality-subsumption relationship.
        let str_list = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
            min: 0,
            max: None,
        };
        let num_list_ne = au_grammar::Shape::List {
            inner: Box::new(au_grammar::Shape::Primitive(au_grammar::Primitive::Number)),
            min: 1,
            max: None,
        };
        let diags = run_struct(vec![td_with_typed_field(
            "ev",
            &[],
            "tags",
            au_grammar::Shape::Union(vec![str_list, num_list_ne]),
        )]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"));
    }

    #[test]
    fn slot_union_and_intersection_subsumption_are_independent() {
        // Same closure shape, two different consumer type-defs: one with a
        // union slot, one with an intersection slot. Each fires its own
        // code; neither bleeds into the other.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev_union",
                &[],
                "support",
                au_grammar::Shape::Union(vec![ref_("decision"), ref_("decision.decided")]),
            ),
            td_with_typed_field(
                "ev_intersection",
                &[],
                "joint",
                au_grammar::Shape::Intersection(vec![ref_("decision"), ref_("decision.decided")]),
            ),
        ]);
        let union_subs = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .count();
        let intersection_subs = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-intersection")
            .count();
        assert_eq!(union_subs, 1);
        assert_eq!(intersection_subs, 1);
    }

    #[test]
    fn list_of_subsuming_union_fires_warning() {
        // `<decision | decision.decided>[]` — the List wrapper is
        // transparent; subsumption walks descend into it.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "items",
                au_grammar::Shape::List {
                    inner: Box::new(au_grammar::Shape::Union(vec![
                        ref_("decision"),
                        ref_("decision.decided"),
                    ])),
                    min: 0,
                    max: None,
                },
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn compound_reference_union_with_subsumption_fires_warning() {
        // `<decision | decision.decided>*` — same rule applies to the
        // CompoundReference's branch list.
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "target",
                au_grammar::Shape::CompoundReference {
                    mode: au_grammar::RefMode::Star,
                    op: au_grammar::CompoundRefOp::Union,
                    branches: vec!["decision".into(), "decision.decided".into()],
                },
            ),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-slot-union")
            .collect();
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn compound_reference_intersection_is_silent_per_4_7() {
        let diags = run_struct(vec![
            empty_td("decision"),
            td_parents("decision.decided", &["decision"]),
            td_with_typed_field(
                "ev",
                &[],
                "target",
                au_grammar::Shape::CompoundReference {
                    mode: au_grammar::RefMode::Star,
                    op: au_grammar::CompoundRefOp::Intersection,
                    branches: vec!["decision".into(), "decision.decided".into()],
                },
            ),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"));
    }

    #[test]
    fn primitive_in_union_does_not_fire_subsumption() {
        // `<String | rationale>` — String has no closure to compare,
        // so the rule cannot apply. No diagnostic.
        let diags = run_struct(vec![
            empty_td("rationale"),
            td_with_typed_field(
                "ev",
                &[],
                "v",
                au_grammar::Shape::Union(vec![
                    au_grammar::Shape::Primitive(au_grammar::Primitive::String),
                    ref_("rationale"),
                ]),
            ),
        ]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-slot-union"));
    }

    // ----- type-def parent: redundancy + mixin-collision -----

    fn td_with_typed_field(
        name: &str,
        parents: &[&str],
        field_name: &str,
        shape: au_grammar::Shape,
    ) -> TypeDef {
        TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName(field_name.into()),
                optional: false,
                raw_shape: format!("{shape:?}"),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(shape),
                doc: None,
            }],
            ..td_parents(name, parents)
        }
    }

    #[test]
    fn type_def_with_duplicate_parent_fires_warning() {
        let diags = run_inheritance(vec![empty_td("note"), td_parents("c", &["note", "note"])]);
        let dups: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "duplicate-claim")
            .collect();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].severity, Severity::Warning);
    }

    #[test]
    fn type_def_with_subsumed_parent_fires_warning() {
        // c's parents are [note, decision]. decision's closure includes
        // note → note is the wider, redundant parent.
        let diags = run_inheritance(vec![
            empty_td("note"),
            td_parents("decision", &["note"]),
            td_parents("c", &["note", "decision"]),
        ]);
        let subs: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "subsumption-in-mixin")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].severity, Severity::Warning);
        assert!(subs[0].message.contains("'note'"));
        assert!(subs[0].message.contains("'decision'"));
    }

    #[test]
    fn type_def_with_token_equal_parents_passes() {
        // Two parents declaring same-named field with the same shape →
        // auto-unify, no collision diagnostic at load.
        let diags = run_inheritance(vec![
            td_with_typed_field(
                "a",
                &[],
                "f",
                au_grammar::Shape::Primitive(au_grammar::Primitive::String),
            ),
            td_with_typed_field(
                "b",
                &[],
                "f",
                au_grammar::Shape::Primitive(au_grammar::Primitive::String),
            ),
            td_parents("c", &["a", "b"]),
        ]);
        assert!(!diags.iter().any(|d| d.code.as_str() == "mixin-collision"));
    }

    /// Build a TypeDef whose single field has an `Err` parsed_shape carrying
    /// a synthetic diagnostic. Used to prove auto-unify equality is on
    /// `raw_shape`, not on full Diagnostic equality (whose spans differ
    /// across origins).
    fn td_with_unparsed_field(name: &str, field_name: &str, raw_shape: &str) -> TypeDef {
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
            fields: vec![FieldDecl {
                name: FieldName(field_name.into()),
                optional: false,
                raw_shape: raw_shape.into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(name.len(), name.len() + 1),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Err(diag),
                doc: None,
            }],
            ..empty_td(name)
        }
    }

    #[test]
    fn type_def_with_parents_sharing_unparsed_shape_does_not_fire_mixin_collision() {
        // Two parents both declare `summary: rationale` (deferred bare-name
        // shape — Err parsed_shape). Pre-fix the load-time check fired
        // mixin-collision because Diagnostic equality embeds per-decl spans.
        let diags = run_inheritance(vec![
            td_with_unparsed_field("note", "summary", "rationale"),
            td_with_unparsed_field("deliverable", "summary", "rationale"),
            td_parents("c", &["note", "deliverable"]),
        ]);
        assert!(
            !diags.iter().any(|d| d.code.as_str() == "mixin-collision"),
            "identical raw_shape across parents must auto-unify, got: {:?}",
            diags
                .iter()
                .filter(|d| d.code.as_str() == "mixin-collision")
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn type_def_with_diverging_parents_loads_clean() {
        // A divergent inherited field is LEGAL at the type-def ([[type-def fields collision - auto-unify and qualified field::au-type-system]]):
        // the type is not broken, and the collision can only be resolved
        // per-instance at a bare use. So the type-graph load fires nothing; the
        // validator emits `mixin-collision` against an instance's claim list.
        let diags = run_inheritance(vec![
            td_with_typed_field(
                "a",
                &[],
                "f",
                au_grammar::Shape::Primitive(au_grammar::Primitive::String),
            ),
            td_with_typed_field(
                "b",
                &[],
                "f",
                au_grammar::Shape::Primitive(au_grammar::Primitive::Number),
            ),
            td_parents("c", &["a", "b"]),
        ]);
        assert!(
            !diags.iter().any(|d| d.code.as_str() == "mixin-collision"),
            "a divergent inherited field must load clean, got: {:?}",
            diags
                .iter()
                .filter(|d| d.code.as_str() == "mixin-collision")
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn type_def_with_single_parent_does_not_fire_mixin_checks() {
        let diags = run_inheritance(vec![empty_td("note"), td_parents("c", &["note"])]);
        assert!(!diags.iter().any(|d| d.code.as_str() == "mixin-collision"));
        assert!(!diags.iter().any(|d| d.code.as_str() == "duplicate-claim"));
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "subsumption-in-mixin"));
    }

    // ----- [[type-def field shape::au-type-system]] / [[aspiration - type system ideas]] prose-extractability for `fills:` bindings -----

    fn td_with_body_fills(
        name: &str,
        field_name: &str,
        shape: au_grammar::Shape,
        fills_field: &str,
    ) -> TypeDef {
        let path = PathBuf::from(format!("/v/{name}.type.yaml"));
        TypeDef {
            shape: None,
            fields: vec![FieldDecl {
                name: FieldName(field_name.into()),
                optional: false,
                raw_shape: format!("{shape}"),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(shape),
                doc: None,
            }],
            body: Some(vec![crate::body::BodyItem::Section {
                name: "Why".into(),
                optional: false,
                fills: Some(crate::body::FillsContract {
                    fields: vec![crate::body::FieldClaim {
                        name: FieldName(fills_field.into()),
                        span: ByteRange::new(0, 0),
                    }],
                    exclusive: false,
                    fields_span: ByteRange::new(0, 0),
                    source_path: path.clone(),
                }),
                guidance: None,
                body: None,
                name_span: ByteRange::new(0, 0),
                item_span: ByteRange::new(0, 0),
                source_path: path,
            }]),
            ..empty_td(name)
        }
    }

    /// EVERY shape is carryable in prose, so a `fills:` target is never rejected
    /// for its shape — only for not existing. The old
    /// `fills-shape-not-prose-extractable` rule conflated "can a contribution
    /// exist" (always yes) with "will the author get the branch they meant" (a
    /// value-layer disambiguation question, not a `fills:` one).
    #[test]
    fn fills_accepts_every_shape() {
        use au_grammar::{Primitive, Shape};
        let shapes = vec![
            ("bare primitive", Shape::Primitive(Primitive::String)),
            (
                "all-primitive union",
                Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Primitive(Primitive::Number),
                ]),
            ),
            (
                "mixed union",
                Shape::Union(vec![
                    Shape::Primitive(Primitive::String),
                    Shape::Reference("rationale".into()),
                ]),
            ),
            ("reference", Shape::Reference("rationale".into())),
            ("record", Shape::Record("rationale".into())),
            ("any", Shape::Any),
        ];
        for (label, shape) in shapes {
            let diags = run_body(vec![td_with_body_fills("decision", "f", shape, "f")]);
            let shape_errors: Vec<_> = diags
                .iter()
                .filter(|d| d.code.as_str().starts_with("fills-shape"))
                .collect();
            assert!(
                shape_errors.is_empty(),
                "{label} must be a valid fills target; got {shape_errors:?}"
            );
        }
    }

    fn run_body(defs: Vec<TypeDef>) -> Vec<Diagnostic> {
        let g = build_graph(defs).graph;
        run_body_typing_checks(&g)
    }

    // ----- [[type-def body fills::au-type-system]] nested-exclusivity propagation for multi-field `fills!:` -----

    /// Build a type-def with a single String field `name` and a body
    /// shaped `[Section parent (fills! parent_fields) { Section child
    /// (fills child_fields, optionally exclusive) }]`. Used to drive
    /// the nested-exclusivity check directly.
    fn td_with_nested_fills(
        td_name: &str,
        fields: &[&str],
        parent_section: &str,
        parent_fills: &[&str],
        parent_exclusive: bool,
        child_section: &str,
        child_fills: &[&str],
        child_exclusive: bool,
    ) -> TypeDef {
        let path = PathBuf::from(format!("/v/{td_name}.type.yaml"));
        let field_decls = fields
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
            .collect();
        let make_contract = |fs: &[&str], excl: bool| crate::body::FillsContract {
            fields: fs
                .iter()
                .map(|f| crate::body::FieldClaim {
                    name: FieldName((*f).into()),
                    span: ByteRange::new(0, 0),
                })
                .collect(),
            exclusive: excl,
            fields_span: ByteRange::new(0, 0),
            source_path: path.clone(),
        };
        let child = crate::body::BodyItem::Section {
            name: child_section.into(),
            optional: false,
            fills: Some(make_contract(child_fills, child_exclusive)),
            guidance: None,
            body: None,
            name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
            source_path: path.clone(),
        };
        let parent = crate::body::BodyItem::Section {
            name: parent_section.into(),
            optional: false,
            fills: Some(make_contract(parent_fills, parent_exclusive)),
            guidance: None,
            body: Some(vec![child]),
            name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
            source_path: path,
        };
        TypeDef {
            shape: None,
            fields: field_decls,
            body: Some(vec![parent]),
            ..empty_td(td_name)
        }
    }

    /// Multi-field `fills!:` on parent forbids a child fills to a
    /// field outside the parent's allowed set.
    #[test]
    fn multi_field_fills_exclusive_propagates_to_descendants() {
        let diags = run_body(vec![td_with_nested_fills(
            "decision",
            &["a", "b", "c"],
            "Outer",
            &["a", "b"],
            true, // fills!: [a, b]
            "Inner",
            &["c"],
            false, // fills: c
        )]);
        let conflicts: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity")
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "child fills:c must conflict with parent fills![a, b]; diags: {diags:?}"
        );
        assert!(conflicts[0].message.contains("'c'"));
        assert!(conflicts[0].message.contains("'a'"));
        assert!(conflicts[0].message.contains("'b'"));
        // related[] anchors at the ancestor's fields_span — confirms
        // the cross-site span is wired.
        assert_eq!(
            conflicts[0].related.len(),
            1,
            "diagnostic must carry the ancestor's declaration site in related[]"
        );
    }

    /// Multi-field `fills!:` accepts a child whose field IS in the
    /// allowed set — no false-positive.
    #[test]
    fn multi_field_fills_exclusive_allows_subset_descendant() {
        let diags = run_body(vec![td_with_nested_fills(
            "decision",
            &["a", "b"],
            "Outer",
            &["a", "b"],
            true,
            "Inner",
            &["a"], // valid subset
            false,
        )]);
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity"),
            "child fills:a is in parent fills![a, b]; no conflict"
        );
    }

    /// Regression: single-field `fills!:` still propagates correctly
    /// (the original behavior the broken `len() == 1` gate covered).
    #[test]
    fn single_field_fills_exclusive_still_propagates() {
        let diags = run_body(vec![td_with_nested_fills(
            "decision",
            &["rationale", "other"],
            "Why",
            &["rationale"],
            true, // fills!: rationale
            "Inner",
            &["other"], // conflicts
            false,
        )]);
        let conflicts: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity")
            .collect();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].message.contains("'other'"));
        assert!(conflicts[0].message.contains("'rationale'"));
    }

    /// Non-exclusive parent doesn't constrain children at all.
    #[test]
    fn non_exclusive_parent_lets_child_diverge() {
        let diags = run_body(vec![td_with_nested_fills(
            "decision",
            &["a", "b"],
            "Outer",
            &["a"],
            false, // plain fills:
            "Inner",
            &["b"],
            false,
        )]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity"));
    }

    // ----- `use:` cycle detection ([[type-def body use::au-type-system]] acyclicity) -----

    /// TypeDef builder: name has body `[use: t1, use: t2, ...]`. Parent
    /// links each target so `use:` falls within the host's closure (a
    /// closure-filter precondition for the cycle walk).
    fn td_use_chain(name: &str, targets: &[&str]) -> TypeDef {
        let path = PathBuf::from(format!("/v/{name}.type.yaml"));
        let body = targets
            .iter()
            .map(|t| crate::body::BodyItem::Use {
                type_name: TypeName((*t).into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                item_span: ByteRange::new(0, 0),
            })
            .collect();
        TypeDef {
            shape: None,
            parent_claim: Some(ParentClaim {
                form: ParentClaimForm::List,
                value_span: ByteRange::new(0, 0),
            }),
            parents: targets
                .iter()
                .map(|t| TypeNameClaim::own(TypeName((*t).into()), ByteRange::new(0, 0)))
                .collect(),
            body: Some(body),
            source_path: path,
            ..empty_td(name)
        }
    }

    fn cycle_diags(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| d.code.as_str() == "body-use-cycle")
            .collect()
    }

    /// Two-node cycle A→B→A. Both members must be diagnosed.
    #[test]
    fn two_node_cycle_diagnoses_both_members() {
        // A's body has `use: b`; A claims B as parent (so B is in A's closure).
        // B's body has `use: a`; B claims A as parent (so A is in B's closure).
        let diags = run_body(vec![td_use_chain("a", &["b"]), td_use_chain("b", &["a"])]);
        let cycles = cycle_diags(&diags);
        assert_eq!(
            cycles.len(),
            2,
            "both A and B participate; diags: {diags:?}"
        );
        // Each names the other in related[].
        assert_eq!(cycles[0].related.len(), 1);
        assert_eq!(cycles[1].related.len(), 1);
    }

    /// Three-node cycle A→B→C→A. All three members must be diagnosed
    /// and each should name the other two in related[].
    #[test]
    fn three_node_cycle_diagnoses_all_members() {
        let diags = run_body(vec![
            td_use_chain("a", &["b"]),
            td_use_chain("b", &["c"]),
            td_use_chain("c", &["a"]),
        ]);
        let cycles = cycle_diags(&diags);
        assert_eq!(
            cycles.len(),
            3,
            "all of A, B, C participate; diags: {diags:?}"
        );
        for d in &cycles {
            assert_eq!(
                d.related.len(),
                2,
                "each cycle member names the other two: {}",
                d.message
            );
        }
    }

    /// Pre-fix regression case: a cycle member only reachable through
    /// a path NOT taken from the alphabetically-first start node would
    /// be missed (old `on_cycle` only captured back-edge endpoints).
    /// With `D→B` and a `B↔C` cycle, B was on the stack when the
    /// back-edge fired but C was — old code missed B. Trace today:
    /// alphabetical start is B, B→C→B fires back-edge with stack
    /// slice {B, C}, both captured.
    #[test]
    fn cycle_member_reached_via_alternative_path_is_still_diagnosed() {
        let diags = run_body(vec![
            td_use_chain("b", &["c"]),
            td_use_chain("c", &["b"]),
            // D is not on the cycle, just reaches into it.
            td_use_chain("d", &["b"]),
        ]);
        let cycles = cycle_diags(&diags);
        let on_cycle_names: std::collections::BTreeSet<_> = cycles
            .iter()
            .map(|d| {
                d.message
                    .split('\'')
                    .nth(1)
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            on_cycle_names,
            ["b".to_string(), "c".to_string()].into_iter().collect(),
            "B and C are both on cycle; D is not. diags: {diags:?}"
        );
    }

    /// Deep linear chain (no cycle). The iterative DFS must terminate
    /// without stack-overflow or false cycle diagnostics.
    #[test]
    fn deep_linear_chain_does_not_false_fire() {
        let n = 200;
        let mut defs = Vec::with_capacity(n);
        for i in 0..n {
            let next: Vec<&'static str> = if i + 1 < n {
                vec![Box::leak(format!("n{:03}", i + 1).into_boxed_str())]
            } else {
                vec![]
            };
            let name: &'static str = Box::leak(format!("n{:03}", i).into_boxed_str());
            defs.push(td_use_chain(name, &next));
        }
        let diags = run_body(defs);
        assert!(
            cycle_diags(&diags).is_empty(),
            "linear chain shouldn't fire cycle diags"
        );
    }

    /// Out-of-closure `use:` targets should NOT count as cycle edges —
    /// they're already diagnosed as `body-use-out-of-closure`. A graph
    /// where A's body has `use: B` but A doesn't claim B (so B isn't in
    /// A's closure) and B's body has `use: A` should produce only
    /// `body-use-out-of-closure` per edge, NOT a cycle diagnostic.
    #[test]
    fn out_of_closure_use_edges_dont_count_as_cycle() {
        // Construct without parent claims — A and B don't include each
        // other in their closures.
        let a = TypeDef {
            shape: None,
            body: Some(vec![crate::body::BodyItem::Use {
                type_name: TypeName("b".into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                item_span: ByteRange::new(0, 0),
            }]),
            ..empty_td("a")
        };
        let b = TypeDef {
            shape: None,
            body: Some(vec![crate::body::BodyItem::Use {
                type_name: TypeName("a".into()),
                repo: None,
                type_name_span: ByteRange::new(0, 0),
                item_span: ByteRange::new(0, 0),
            }]),
            ..empty_td("b")
        };
        let diags = run_body(vec![a, b]);
        assert!(
            cycle_diags(&diags).is_empty(),
            "out-of-closure edges must not count as cycle; diags: {diags:?}"
        );
        let oc: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "body-use-out-of-closure")
            .collect();
        assert_eq!(
            oc.len(),
            2,
            "each broken edge fires body-use-out-of-closure"
        );
    }

    // ----- [[type-def body fills::au-type-system]] body-level `fills!:` propagation -----

    /// Build a type-def whose body has both a body-level Fills item and
    /// a single Section with its own fills. Used to drive the [[type-def body fills::au-type-system]]
    /// body-scope exclusivity check.
    fn td_with_body_level_fills_and_section(
        td_name: &str,
        fields: &[&str],
        body_fills: &[&str],
        body_exclusive: bool,
        section_fills: &[&str],
        section_exclusive: bool,
        body_fills_first: bool, // controls source-position ordering
    ) -> TypeDef {
        let path = PathBuf::from(format!("/v/{td_name}.type.yaml"));
        let field_decls = fields
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
            .collect();
        let make_contract = |fs: &[&str], excl: bool| crate::body::FillsContract {
            fields: fs
                .iter()
                .map(|f| crate::body::FieldClaim {
                    name: FieldName((*f).into()),
                    span: ByteRange::new(0, 0),
                })
                .collect(),
            exclusive: excl,
            fields_span: ByteRange::new(0, 0),
            source_path: path.clone(),
        };
        let body_fills_item = crate::body::BodyItem::Fills {
            contract: make_contract(body_fills, body_exclusive),
            item_span: ByteRange::new(0, 0),
        };
        let section_item = crate::body::BodyItem::Section {
            name: "S".into(),
            optional: false,
            fills: Some(make_contract(section_fills, section_exclusive)),
            guidance: None,
            body: None,
            name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
            source_path: path.clone(),
        };
        let body = if body_fills_first {
            vec![body_fills_item, section_item]
        } else {
            vec![section_item, body_fills_item]
        };
        TypeDef {
            shape: None,
            fields: field_decls,
            body: Some(body),
            ..empty_td(td_name)
        }
    }

    /// Body `[fills!: a, section: S (fills: b)]` — section's fills
    /// conflicts with the body-level `fills!: a`.
    #[test]
    fn body_level_fills_exclusive_constrains_section_after_it() {
        let diags = run_body(vec![td_with_body_level_fills_and_section(
            "decision",
            &["a", "b"],
            &["a"],
            true, // body-level fills!: a
            &["b"],
            false, // section fills: b
            true,  // body-fills appears first
        )]);
        let conflicts: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity")
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "section's `fills: b` must conflict with body-level `fills!: a`; diags: {diags:?}"
        );
        assert!(conflicts[0].message.contains("'b'"));
        assert!(conflicts[0].message.contains("'a'"));
    }

    /// Body `[section: S (fills: b), fills!: a]` — body-level Fills
    /// declared AFTER the section must still constrain it, since [[type-def body fills::au-type-system]]
    /// applies the constraint to the whole body regardless of position.
    #[test]
    fn body_level_fills_exclusive_constrains_section_before_it() {
        let diags = run_body(vec![td_with_body_level_fills_and_section(
            "decision",
            &["a", "b"],
            &["a"],
            true,
            &["b"],
            false,
            false, // section appears FIRST in body items
        )]);
        let conflicts: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity")
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "body-level `fills!:` must constrain sections preceding it in source order"
        );
    }

    /// Body `[fills!: a, section: S (fills: a)]` — section matches the
    /// body-level constraint, no conflict.
    #[test]
    fn body_level_fills_exclusive_accepts_matching_section() {
        let diags = run_body(vec![td_with_body_level_fills_and_section(
            "decision",
            &["a"],
            &["a"],
            true,
            &["a"],
            false,
            true,
        )]);
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity"),
            "section's fills:a matches body-level fills!:a; no conflict"
        );
    }

    /// Body `[fills: a, section: S (fills: b)]` — non-exclusive
    /// body-level fills does NOT propagate; the section is free to
    /// declare a different field.
    #[test]
    fn body_level_fills_non_exclusive_does_not_constrain_sections() {
        let diags = run_body(vec![td_with_body_level_fills_and_section(
            "decision",
            &["a", "b"],
            &["a"],
            false, // body-level plain fills:
            &["b"],
            false,
            true,
        )]);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "fills-contract-conflict-nested-exclusivity"));
    }

    /// Self-cycle (single node uses itself, transitively in-closure).
    /// One diagnostic, empty related[], with the "self-`use:`" wording.
    #[test]
    fn self_use_fires_self_cycle_diagnostic() {
        let mut self_td = td_use_chain("h", &["h"]);
        // Add self to closure: claim self as parent. (The type-cycle
        // check is separate from body-use-cycle; this just makes the
        // closure_of include "h".)
        self_td.parents = vec![TypeNameClaim::own(
            TypeName("h".into()),
            ByteRange::new(0, 0),
        )];
        let diags = run_body(vec![self_td]);
        let cycles = cycle_diags(&diags);
        assert_eq!(cycles.len(), 1);
        assert!(
            cycles[0].message.contains("self-`use:`"),
            "self-cycle wording, got: {}",
            cycles[0].message
        );
        assert!(cycles[0].related.is_empty());
    }
}
