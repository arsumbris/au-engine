//! Cross-boundary TYPE-vocabulary resolution: a `::repo`-qualified type name —
//! an instance `type:` claim, a type-def parent, a field shape `foo::repo*`, a
//! meta sub-region `type: dm::repo`, or a body `use: parent::repo` — names a
//! peer's type, see
//! [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]].
//!
//! au-core resolves only OWN (unqualified) names; the cross-repo fold resolves a
//! valid `::repo` peer type. This gate is the use-site author feedback that
//! layers on top, the type-side sibling of [`crate::crossref`]: a `::repo` to an
//! undeclared peer, a declared-but-unmounted peer, an empty `::`, or a present
//! peer that lacks the named type. A resolvable `::repo` is silent here, the fold
//! type-checks it.
//!
//! Each diagnostic emits at its use site — a claim or parent span, or the
//! field's shape span. Distinct from the registry-level `peer-unmounted` /
//! `undeclared-peer` consistency family, which reports the declaration
//! topology once per peer, not per use.

use std::path::Path;

use au_core::{BodyItem, InlineValue, InstanceValue, TypeName, TypeNameClaim};
use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span};
use au_grammar::{DefBound, QualifiedName, RefMode, Shape};

use crate::crossref::{resolve_repo_scope, RepoScope};
use crate::ir::{RepoGraphs, ResolutionGraphs};
use crate::parse::FileParse;
use crate::repo::{RepoMap, Workspace};

/// `foo::` — a `::repo` type qualifier with an empty repo, the type-name sibling
/// of `wikilink-empty-repo`. Bare `foo` is the canonical own-repo form, so an
/// empty `::` adds nothing and is rejected. Error. See
/// [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]].
pub const TYPE_REPO_EMPTY: DiagnosticCode = DiagnosticCode::from_static("type-repo-empty");
/// A `::repo` type name names a repo that is neither a declared dependency of the
/// source's repo nor a known workspace member — the qualifier resolves to no
/// known repo, likely a typo. Error. The type-side sibling of
/// `reference-repo-unknown`.
pub const TYPE_REPO_UNKNOWN: DiagnosticCode = DiagnosticCode::from_static("type-repo-unknown");
/// A `::repo` type name names a declared dependency that is not present here (not
/// mounted, not cached), so its type cannot be folded. Warning — resolve / vendor
/// the closure is the fix that survives an absent peer. The type-side sibling of
/// `reference-repo-unavailable`.
pub const TYPE_REPO_UNAVAILABLE: DiagnosticCode =
    DiagnosticCode::from_static("type-repo-unavailable");
/// A `::repo` type name names a mounted workspace member (an `edit` or `discover`
/// member) that is NOT a declared dependency of the source's repo. A type
/// position requires a declared `dep` (the peer gate): a member is mounted for
/// discovery / editing, but crossing its VOCABULARY needs it declared as a
/// type-dependency, since `deps` folds into the closure-hash. Error. A plain
/// VALUE link into a member stays permissive; only a TYPE crossing gates. The fix
/// is to add the repo to this repo's `deps`. See [[repo yaml::au-type-system]].
pub const TYPE_REPO_NOT_A_DEPENDENCY: DiagnosticCode =
    DiagnosticCode::from_static("type-repo-not-a-dependency");
/// A `::repo` type name names a present peer that has no such type-def — the
/// repo resolved but the name is wrong (e.g. `mcp.tool::pkg` where the
/// peer exports `mcp.Tool`). Error. The type-side sibling of
/// `slot-references-absent-type`, distinct from `type-repo-unavailable` (peer
/// absent) and `type-repo-unknown` (peer undeclared).
pub const PEER_TYPE_NOT_FOUND: DiagnosticCode = DiagnosticCode::from_static("peer-type-not-found");
/// A `::repo` type name qualifies the source's OWN repo (`note::app` written
/// inside `app`). It resolves to the own type and validates, so it is not an
/// error, just redundant — bare `note` is the own form. Hint. Distinct from
/// every other `type-repo-*` code (those name a peer); this names yourself.
pub const TYPE_REPO_SELF: DiagnosticCode = DiagnosticCode::from_static("type-repo-self");

/// The outcome of gating one `::repo`-qualified type name.
enum TypeRepoRef {
    /// The repo is present and holds the named type-def. The fold type-checks it;
    /// the gate is satisfied, no diagnostic.
    Resolved,
    /// `foo::` — an empty repo qualifier.
    Empty,
    /// The named repo is neither a declared dependency nor a scoped workspace
    /// member.
    RepoUnknown,
    /// The named repo is a declared dependency but not present here.
    RepoUnavailable,
    /// The named repo is a mounted member (or scoped member) but not a declared
    /// dependency of the source's repo — a type crossing requires a `dep`.
    NotADependency,
    /// The named repo is present but has no such type-def.
    TypeMissing,
}

/// Gate one `::repo`-qualified base name from `source`. Mirrors
/// [`crate::crossref::resolve_cross_repo`] on the type axis: the repo half
/// reuses the shared `resolve_repo_scope`, the target half checks the peer's
/// type graph instead of its wikilink index.
fn resolve_type_repo(
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    source: &Path,
    repo_q: &str,
    base: &str,
) -> TypeRepoRef {
    if repo_q.is_empty() {
        return TypeRepoRef::Empty;
    }
    // The peer gate: crossing a peer's VOCABULARY needs it declared as a `dep`
    // (folded into the closure-hash), or it is the source's own repo (the
    // redundant self-qualifier). A mounted `edit` / `discover` member that is NOT
    // a declared dep is scoped for discovery / editing, but a TYPE crossing is
    // gated — a plain value link stays permissive (that path is `crossref`).
    let source_repo = repos.repo_of(source);
    let crossable = source_repo.is_some_and(|r| {
        r.name.as_str() == repo_q || r.deps.iter().any(|p| p.name.as_str() == repo_q)
    });
    match resolve_repo_scope(repos, workspaces, source, repo_q) {
        RepoScope::Present(repo) => {
            // The builtin `au-engine` repo is the universal peer: its `au.engine.*`
            // types cross without a dep declaration (every engine-schema file
            // carries an implicit `::au-engine` claim).
            if !repo.builtin && !crossable {
                TypeRepoRef::NotADependency
            } else if graphs.of(&repo.name).contains(&TypeName(base.to_string())) {
                TypeRepoRef::Resolved
            } else {
                TypeRepoRef::TypeMissing
            }
        }
        RepoScope::Unavailable => {
            if crossable {
                TypeRepoRef::RepoUnavailable
            } else {
                TypeRepoRef::NotADependency
            }
        }
        RepoScope::Unknown => TypeRepoRef::RepoUnknown,
    }
}

/// Resolve one `::repo`-qualified type name, push its gate diagnostic if any, and
/// return the outcome so a caller (the body `use:` closure check) can act on a
/// `Resolved` peer.
#[allow(clippy::too_many_arguments)]
fn emit(
    source: &Path,
    span: ByteRange,
    base: &str,
    repo_q: &str,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) -> TypeRepoRef {
    let outcome = resolve_type_repo(repos, graphs, workspaces, source, repo_q, base);
    let diag = match &outcome {
        TypeRepoRef::Resolved => None,
        TypeRepoRef::Empty => Some((
            TYPE_REPO_EMPTY,
            Severity::Error,
            format!("type '{base}::' has an empty repo qualifier; bare '{base}' is the own-repo form"),
        )),
        TypeRepoRef::RepoUnknown => Some((
            TYPE_REPO_UNKNOWN,
            Severity::Error,
            format!(
                "type '{base}::{repo_q}' names repo '{repo_q}', which is not a declared dependency or a known workspace member"
            ),
        )),
        TypeRepoRef::RepoUnavailable => Some((
            TYPE_REPO_UNAVAILABLE,
            Severity::Warning,
            format!(
                "type '{base}::{repo_q}' names repo '{repo_q}', which is declared but not present in this workspace"
            ),
        )),
        TypeRepoRef::NotADependency => Some((
            TYPE_REPO_NOT_A_DEPENDENCY,
            Severity::Error,
            format!(
                "type '{base}::{repo_q}' names repo '{repo_q}', a workspace member but not a declared dependency of this repo; add '{repo_q}' to this repo's deps to cross its types"
            ),
        )),
        TypeRepoRef::TypeMissing => Some((
            PEER_TYPE_NOT_FOUND,
            Severity::Error,
            format!("repo '{repo_q}' has no type-def '{base}'"),
        )),
    };
    if let Some((code, severity, message)) = diag {
        diags.push(Diagnostic {
            code,
            severity,
            span: Span::new(source.to_path_buf(), span),
            message,
            related: Vec::new(),
            fix: None,
        });
    }
    // A self-`::repo` qualifier resolves to the own type (the fold registers the
    // qualified alias), so validation holds, but the qualifier adds nothing over
    // bare `base`. Flag the redundancy, advisory.
    if matches!(outcome, TypeRepoRef::Resolved)
        && repos
            .repo_of(source)
            .is_some_and(|r| r.name.as_str() == repo_q)
    {
        diags.push(Diagnostic {
            code: TYPE_REPO_SELF,
            severity: Severity::Hint,
            span: Span::new(source.to_path_buf(), span),
            message: format!(
                "type '{base}::{repo_q}' qualifies the own repo; bare '{base}' is the own form"
            ),
            related: Vec::new(),
            fix: None,
        });
    }
    outcome
}

/// Gate a `::repo`-qualified mixin name for the ensure-mixin write directive: the
/// mixin can be appended to a `type:` claim only if it RESOLVES cleanly. Returns
/// `Some(reason)` when it does not — an undeclared repo, an unmounted dep, a
/// non-dependency member, or a missing type — the same peer gate the build's claim
/// check emits, surfaced as a decision reason rather than a standing diagnostic.
/// `None` means the mixin resolves; the caller proceeds to the value gate. A BARE
/// (own-repo) mixin has no `::repo` to gate, so its resolution is au-core's
/// `unknown-type-claim`, caught by the value gate instead.
/// See [[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]].
pub(crate) fn gate_mixin_repo(
    source: &Path,
    base: &str,
    repo_q: &str,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
) -> Option<String> {
    match resolve_type_repo(repos, graphs, workspaces, source, repo_q, base) {
        TypeRepoRef::Resolved => None,
        TypeRepoRef::Empty => Some(format!("mixin '{base}::' has an empty repo qualifier")),
        TypeRepoRef::RepoUnknown => Some(format!(
            "mixin repo '{repo_q}' is not a declared dependency or a known workspace member"
        )),
        TypeRepoRef::RepoUnavailable => Some(format!(
            "mixin repo '{repo_q}' is declared but not present in this workspace"
        )),
        TypeRepoRef::NotADependency => Some(format!(
            "mixin repo '{repo_q}' is a workspace member but not a declared dependency; add '{repo_q}' to this repo's deps"
        )),
        TypeRepoRef::TypeMissing => Some(format!("repo '{repo_q}' has no type-def '{base}'")),
    }
}

/// Gate a single claim or parent. An own (unqualified) claim is au-core's
/// concern and is skipped here.
#[allow(clippy::too_many_arguments)]
fn gate_claim(
    source: &Path,
    claim: &TypeNameClaim,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    if let Some(repo_q) = &claim.repo {
        emit(
            source,
            claim.span,
            claim.name.as_str(),
            repo_q,
            repos,
            graphs,
            workspaces,
            diags,
        );
    }
}

/// Collect every `::repo`-qualified name a field shape reaches, descending list,
/// pin, compound, and def-ref operands. Unqualified names are skipped — only a
/// peer reference is gated here.
fn collect_qualified<'a>(shape: &'a Shape, out: &mut Vec<&'a QualifiedName>) {
    match shape {
        Shape::Reference(q) | Shape::Record(q) | Shape::InlineOrReference(q) => out.push(q),
        Shape::List { inner, .. } | Shape::Pinned(inner) => collect_qualified(inner, out),
        Shape::Union(branches) | Shape::Intersection(branches) => {
            branches.iter().for_each(|s| collect_qualified(s, out))
        }
        Shape::CompoundReference { branches, .. } => out.extend(branches.iter()),
        Shape::DefReference(Some(DefBound::Single(q))) => out.push(q),
        Shape::DefReference(Some(DefBound::Compound { branches, .. })) => {
            out.extend(branches.iter())
        }
        // A tuple's element shapes may name peer types; gate each.
        Shape::Tuple(elements) => elements.iter().for_each(|s| collect_qualified(s, out)),
        Shape::Primitive(_)
        | Shape::Enum(_)
        | Shape::Any
        | Shape::Opaque
        | Shape::DefReference(None)
        | Shape::Refined { .. } => {}
    }
}

/// Collect every `::repo`-qualified name a field shape reaches in a REFERENCE
/// position (`*` / `&`), with its suffix char. The cross-repo sibling of
/// au-core's own-repo reference walk, for gating a peer brand's referenceability:
/// a nominal peer brand, or a peer union with a non-record member, cannot take a
/// `*` / `&`. Bare-record positions are excluded — those are inline, always fine.
fn collect_ref_qualified<'a>(shape: &'a Shape, out: &mut Vec<(&'a QualifiedName, char)>) {
    match shape {
        Shape::Reference(q) => out.push((q, '*')),
        Shape::InlineOrReference(q) => out.push((q, '&')),
        Shape::CompoundReference { branches, mode, .. } => {
            let c = match mode {
                RefMode::Star => '*',
                RefMode::Inline => '&',
            };
            for b in branches {
                out.push((b, c));
            }
        }
        Shape::List { inner, .. } | Shape::Pinned(inner) => collect_ref_qualified(inner, out),
        Shape::Union(branches) | Shape::Intersection(branches) | Shape::Tuple(branches) => {
            branches.iter().for_each(|s| collect_ref_qualified(s, out))
        }
        _ => {}
    }
}

/// Gate every `::repo`-qualified body `use:` in a type-def's body, recursing into
/// section sub-bodies. Two layers, the cross-repo parity of the own-graph body-use
/// checks (`load_checks`, which DEFERS a qualified use):
/// - the repo gate (`emit`), a `use: parent::repo` to an undeclared / unmounted /
///   typo'd peer, like a claim / parent.
/// - when the peer RESOLVES, the folded-closure check (`use:` demands the host
///   EXTEND `parent::repo`, so the peer must be in the host's FOLDED closure) and
///   the no-body check (the peer type declares a body to splice).
#[allow(clippy::too_many_arguments)]
fn gate_body_uses(
    source: &Path,
    host: &TypeName,
    host_rg: Option<&au_core::ResolutionGraph>,
    body: &[BodyItem],
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    for item in body {
        match item {
            BodyItem::Use {
                type_name,
                repo: Some(repo_q),
                type_name_span,
                ..
            } => {
                let outcome = emit(
                    source,
                    *type_name_span,
                    type_name.as_str(),
                    repo_q,
                    repos,
                    graphs,
                    workspaces,
                    diags,
                );
                if matches!(outcome, TypeRepoRef::Resolved) {
                    check_folded_body_use(
                        source,
                        host,
                        host_rg,
                        *type_name_span,
                        type_name.as_str(),
                        repo_q,
                        repos,
                        graphs,
                        workspaces,
                        diags,
                    );
                }
            }
            BodyItem::Section {
                body: Some(sub), ..
            } => gate_body_uses(source, host, host_rg, sub, repos, graphs, workspaces, diags),
            _ => {}
        }
    }
}

/// The cross-repo `body-use-out-of-closure` / `body-use-target-has-no-body`
/// checks for a RESOLVED `use: base::repo_q` (the repo gate already passed).
/// `use:` demands the host EXTEND the peer, so the peer's `TypeId` must be in the
/// host's FOLDED parent closure (`folded_closure_ids` over the host's own name,
/// the def-axis pattern of D3-crossrepo-defref). In closure, the peer must declare
/// a body to splice.
#[allow(clippy::too_many_arguments)]
fn check_folded_body_use(
    source: &Path,
    host: &TypeName,
    host_rg: Option<&au_core::ResolutionGraph>,
    span: ByteRange,
    base: &str,
    repo_q: &str,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    let RepoScope::Present(peer) = resolve_repo_scope(repos, workspaces, source, repo_q) else {
        return;
    };
    let peer_graph = graphs.of(&peer.name);
    let base_name = TypeName(base.to_string());
    let Some(hash) = peer_graph.closure_id(&base_name) else {
        return;
    };
    let demanded = au_core::TypeId {
        name: base_name.clone(),
        hash,
    };
    // The host's folded parent closure, via its own name as the claim. No
    // resolution graph means the host imports nothing, so the peer cannot be in
    // its closure.
    let host_claim = au_core::TypeClaim::Bare(TypeNameClaim::own(host.clone(), span));
    let in_closure = host_rg
        .map(|rg| au_core::folded_closure_ids(rg, &host_claim).contains(&demanded))
        .unwrap_or(false);
    if !in_closure {
        diags.push(Diagnostic {
            code: au_core::codes::BODY_USE_OUT_OF_CLOSURE,
            severity: Severity::Error,
            span: Span::new(source.to_path_buf(), span),
            message: format!(
                "`use: {base}::{repo_q}` references a type-def not in '{}'s closure",
                host.as_str()
            ),
            related: Vec::new(),
            fix: None,
        });
        return;
    }
    // In closure: does the peer type actually declare a body to splice?
    let has_body = peer_graph
        .get(&base_name)
        .and_then(|t| t.body.as_ref())
        .is_some_and(|b| !b.is_empty());
    if !has_body {
        diags.push(Diagnostic {
            code: au_core::codes::BODY_USE_TARGET_HAS_NO_BODY,
            severity: Severity::Warning,
            span: Span::new(source.to_path_buf(), span),
            message: format!(
                "`use: {base}::{repo_q}` splices nothing — '{base}::{repo_q}' declares no `body:` template"
            ),
            related: Vec::new(),
            fix: None,
        });
    }
}

/// Recurse an instance field value for inline-record `::repo` claims, descending
/// sequences and nested inline records, mirroring the cross-repo reference walk.
fn gate_inline_claims(
    source: &Path,
    value: &InstanceValue,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    match value {
        InstanceValue::Mapping(InlineValue {
            type_claim, fields, ..
        }) => {
            if let Some(tc) = type_claim {
                for claim in tc.iter() {
                    gate_claim(source, claim, repos, graphs, workspaces, diags);
                }
            }
            for f in fields {
                gate_inline_claims(source, &f.value, repos, graphs, workspaces, diags);
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                gate_inline_claims(source, &e.value, repos, graphs, workspaces, diags);
            }
        }
        _ => {}
    }
}

/// The byte offset of a sub-slice within its parent string. Both share the
/// backing buffer, as `scan_body`'s slices do.
fn offset_in(parent: &str, child: &str) -> usize {
    child.as_ptr() as usize - parent.as_ptr() as usize
}

/// The `::repo` type-name gating diagnostics for one source file's parse. A
/// type-def gates its parents and field shapes; an instance gates its identity
/// claim and any inline-record claims in its field values. A note carries no
/// type claim and contributes none.
///
/// Each diagnostic's `span.file` is the source. The whole-catalog pass routes a
/// type-def's diagnostics to the repo's vocabulary bucket and an instance's to
/// its own slice; the incremental path calls this for one changed instance.
pub(crate) fn cross_repo_type_diagnostics_for(
    parse: &FileParse,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    resolution_graphs: &ResolutionGraphs,
    workspaces: &[Workspace],
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    match parse {
        FileParse::TypeDef {
            type_def: Some(td), ..
        } => {
            for parent in &td.parents {
                gate_claim(
                    &td.source_path,
                    parent,
                    repos,
                    graphs,
                    workspaces,
                    &mut diags,
                );
            }
            for field in &td.fields {
                let Ok(shape) = &field.parsed_shape else {
                    continue;
                };
                let mut qns = Vec::new();
                collect_qualified(shape, &mut qns);
                for q in qns {
                    if let Some(repo_q) = &q.repo {
                        emit(
                            &td.source_path,
                            field.shape_span,
                            &q.base,
                            repo_q,
                            repos,
                            graphs,
                            workspaces,
                            &mut diags,
                        );
                    }
                }
                // A `*` / `&` on a peer brand that is not referenceable (a nominal
                // brand, or a union with a non-record member) is
                // `brand-not-referenceable`, the cross-repo sibling of the
                // own-repo load check. Silent in-repo skips a `::repo` target, so
                // this is where a peer brand's referenceability is gated.
                let mut ref_qns = Vec::new();
                collect_ref_qualified(shape, &mut ref_qns);
                for (q, suffix) in ref_qns {
                    let Some(repo_q) = &q.repo else { continue };
                    let RepoScope::Present(peer) =
                        resolve_repo_scope(repos, workspaces, &td.source_path, repo_q)
                    else {
                        continue;
                    };
                    let peer_graph = graphs.of(&peer.name);
                    let Some(brand) = peer_graph
                        .get(&TypeName(q.base.clone()))
                        .and_then(|t| t.shape.as_ref())
                    else {
                        continue;
                    };
                    if !au_core::load_checks::brand_admits_reference(brand, peer_graph) {
                        diags.push(Diagnostic {
                            code: au_core::codes::BRAND_NOT_REFERENCEABLE,
                            severity: Severity::Error,
                            span: Span::new(td.source_path.clone(), field.shape_span),
                            message: format!(
                                "field '{}' on type-def '{}': slot references peer brand '{}::{}' with '{}', but it is not referenceable (a nominal brand, or a union with a non-record member); use it by bare name '{}::{}'",
                                field.name.as_str(),
                                td.name.as_str(),
                                q.base,
                                repo_q,
                                suffix,
                                q.base,
                                repo_q,
                            ),
                            related: vec![],
                            fix: Some(au_diagnostics::SuggestedFix {
                                description: format!(
                                    "drop the '{}' suffix, write the slot as '{}::{}'",
                                    suffix, q.base, repo_q
                                ),
                            }),
                        });
                    }
                }
            }
            // A meta sub-region `type: dm::repo` names a peer meta-type; gate it at
            // the meta type span (single-name, no mixin), like a claim / parent.
            if let Some(blocks) = &td.meta_blocks {
                for block in blocks {
                    if let Some(repo_q) = &block.repo {
                        emit(
                            &td.source_path,
                            block.type_name_span,
                            block.type_name.as_str(),
                            repo_q,
                            repos,
                            graphs,
                            workspaces,
                            &mut diags,
                        );
                    }
                }
            }
            // A `required: X::repo` obligation names a peer meta-type; gate it at
            // its span, like a meta block's type, a claim, or a parent.
            for r in &td.required_meta {
                if let Some(repo_q) = &r.repo {
                    emit(
                        &td.source_path,
                        r.span,
                        r.name.as_str(),
                        repo_q,
                        repos,
                        graphs,
                        workspaces,
                        &mut diags,
                    );
                }
            }
            // A body `use: parent::repo` names a peer type whose body it splices:
            // gate the peer, and when it resolves, check the folded closure + that
            // it declares a body. The host's resolution graph is its own repo's.
            if let Some(body) = &td.body {
                let host_rg = repos
                    .repo_of(&td.source_path)
                    .and_then(|r| resolution_graphs.of(&r.name));
                gate_body_uses(
                    &td.source_path,
                    &td.name,
                    host_rg,
                    body,
                    repos,
                    graphs,
                    workspaces,
                    &mut diags,
                );
            }
        }
        FileParse::Instance {
            instance: Some(inst),
            body,
            body_offset,
            is_markdown,
            ..
        } => {
            for claim in inst.type_claim.iter() {
                gate_claim(
                    &inst.source_path,
                    claim,
                    repos,
                    graphs,
                    workspaces,
                    &mut diags,
                );
            }
            for field in &inst.fields {
                gate_inline_claims(
                    &inst.source_path,
                    &field.value,
                    repos,
                    graphs,
                    workspaces,
                    &mut diags,
                );
            }
            // A body typed-block (` ```yaml [:field] ` fence) is an inline record
            // contributed via the body. Gate its own claim AND every nested
            // inline `::repo` claim, the same recursion frontmatter's
            // `gate_inline_claims` runs, so a nested `type: peer::stranger` typo
            // inside a fence is not silent. Parsing with a file-absolute base
            // offset gives the claim spans directly.
            if *is_markdown {
                for event in au_parser::scan_body(body) {
                    let au_parser::BodyEvent::FencedBlock {
                        info,
                        body: fence_body,
                        ..
                    } = event
                    else {
                        continue;
                    };
                    if au_references::extract_field_marker(info).is_none() {
                        continue;
                    }
                    let base = body_offset + offset_in(body, fence_body);
                    if let Some((inline, _)) =
                        au_core::parse_block_record(&inst.source_path, fence_body, base)
                    {
                        gate_inline_claims(
                            &inst.source_path,
                            &InstanceValue::Mapping(inline),
                            repos,
                            graphs,
                            workspaces,
                            &mut diags,
                        );
                    }
                }
            }
        }
        _ => {}
    }
    diags
}

/// The `::repo` type-name gating diagnostics over the whole catalog, flattened.
/// The build routes each source's diagnostics by kind (a type-def to its repo
/// bucket, an instance to its own slice), so it loops the per-parse form
/// directly; this convenience form drops the routing and returns them all, for
/// tests.
#[cfg(test)]
pub(crate) fn cross_repo_type_diagnostics(
    catalog: &crate::ir::OrdMap<std::path::PathBuf, crate::ir::FileEntry>,
    repos: &RepoMap,
    graphs: &RepoGraphs,
    resolution_graphs: &ResolutionGraphs,
    workspaces: &[Workspace],
) -> Vec<Diagnostic> {
    catalog
        .values()
        .flat_map(|e| {
            cross_repo_type_diagnostics_for(
                e.parse.as_ref(),
                repos,
                graphs,
                resolution_graphs,
                workspaces,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// Build a two-repo workspace and return only its `::repo` type-gating
    /// diagnostics. `base` is present and owns `note { title }`; `ghost` is a
    /// declared peer with no repo in the tree (unmounted). `app` declares both
    /// and carries the given files. au-core defers every qualified claim, so the
    /// only diagnostics over these files are this pass's.
    fn type_diags(app_files: &[(&str, &str)]) -> Vec<Diagnostic> {
        let mut fs = MemoryFileSystem::new();
        // The entry is a content-free folder-repo composing base + app.
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        fs.insert("/v/base/type/note.type.yaml", "fields:\n  title: String\n");
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n  - name: ghost\n",
        );
        for (rel, content) in app_files {
            fs.insert(format!("/v/app/{rel}"), *content);
        }
        let kb = build(Path::new("/v"), &fs).unwrap();
        cross_repo_type_diagnostics(
            &kb.catalog,
            &kb.repos,
            &kb.graphs,
            &kb.resolution_graphs,
            &kb.workspaces,
        )
    }

    fn only(diags: &[Diagnostic]) -> &Diagnostic {
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one type-gating diagnostic, got {diags:?}"
        );
        &diags[0]
    }

    #[test]
    fn present_peer_type_resolves_silently() {
        // `note::base` resolves against base's graph, which owns `note`. The fold
        // type-checks it; the gate is satisfied, so no diagnostic.
        let diags = type_diags(&[("n.md", "---\ntype: note::base\ntitle: x\n---\n")]);
        assert!(
            diags.is_empty(),
            "a resolvable peer type is silent: {diags:?}"
        );
    }

    #[test]
    fn undeclared_peer_is_type_repo_unknown() {
        let diags = type_diags(&[("n.md", "---\ntype: note::stranger\n---\n")]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
        assert_eq!(d.severity, Severity::Error);
    }

    /// Build a workspace where `a` and `b` are both `edit` members and `b` owns
    /// `thing`; `a` claims `thing::b`. `a`'s repo.yaml is given verbatim so a test
    /// can toggle whether it declares `b` as a dep.
    fn a_claims_thing_from_b(a_repo_yaml: &str) -> Vec<Diagnostic> {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - a\n  - b\n",
        );
        fs.insert("/v/a/.arsumbris/repo.yaml", a_repo_yaml);
        fs.insert("/v/a/n.md", "---\ntype: thing::b\n---\n");
        fs.insert("/v/b/.arsumbris/repo.yaml", "name: b\n");
        fs.insert("/v/b/type/thing.type.yaml", "fields: {}\n");
        let kb = build(Path::new("/v"), &fs).unwrap();
        cross_repo_type_diagnostics(
            &kb.catalog,
            &kb.repos,
            &kb.graphs,
            &kb.resolution_graphs,
            &kb.workspaces,
        )
    }

    #[test]
    fn a_member_type_claim_without_a_dep_is_not_a_dependency() {
        // `b` is a mounted `edit` member but `a` does not declare it as a dep, so
        // crossing `b`'s vocabulary is gated: the peer gate requires a `dep`.
        let diags = a_claims_thing_from_b("name: a\n");
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_NOT_A_DEPENDENCY);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn a_member_type_claim_with_a_dep_resolves() {
        // The same claim, but `a` now declares `b` as a dep: the type crossing is
        // allowed and resolves silently.
        let diags = a_claims_thing_from_b("name: a\ndeps:\n  - name: b\n");
        assert!(
            diags.is_empty(),
            "a declared dep makes the member's types crossable: {diags:?}"
        );
    }

    #[test]
    fn declared_unmounted_peer_is_type_repo_unavailable() {
        // `ghost` is a declared peer with no repo present: declared-but-absent.
        let diags = type_diags(&[("n.md", "---\ntype: note::ghost\n---\n")]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNAVAILABLE);
        assert_eq!(d.severity, Severity::Warning);
    }

    #[test]
    fn present_peer_missing_type_is_peer_type_not_found() {
        // `base` is present but owns no `gone`: the repo resolved, the name is wrong.
        let diags = type_diags(&[("n.md", "---\ntype: gone::base\n---\n")]);
        let d = only(&diags);
        assert_eq!(d.code, PEER_TYPE_NOT_FOUND);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn empty_repo_qualifier_is_type_repo_empty() {
        // Quoted: an unquoted `note::` is a YAML mapping-value error, so the
        // empty `::` only reaches the claim parse when quoted. At the field-shape
        // position au-grammar rejects `foo::` as `shape-syntax-error`, so this
        // code is a claim / parent concern.
        let diags = type_diags(&[("n.md", "---\ntype: \"note::\"\n---\n")]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_EMPTY);
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn parent_position_gates() {
        // A type-def extending an undeclared peer's type gates at the parent.
        let diags = type_diags(&[(
            "type/sub.type.yaml",
            "extends: note::stranger\nfields:\n  x: String\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
    }

    #[test]
    fn field_shape_position_gates() {
        // A slot referencing a present peer's missing type gates at the shape.
        let diags = type_diags(&[("type/holder.type.yaml", "fields:\n  ref: gone::base*\n")]);
        let d = only(&diags);
        assert_eq!(d.code, PEER_TYPE_NOT_FOUND);
    }

    #[test]
    fn inline_record_claim_gates() {
        // A `::repo` claim nested in an inline-record value is gated too.
        let diags = type_diags(&[(
            "n.md",
            "---\ntype: note::base\ntitle: x\nextra:\n  type: gone::base\n---\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, PEER_TYPE_NOT_FOUND);
    }

    #[test]
    fn body_typed_block_claim_gates() {
        // A body typed-block (` ```yaml [:field] ` fence) claiming a `::repo` type
        // is gated at its fence, the same as a frontmatter / inline-record claim.
        // The frontmatter `note::base` resolves silently; the body block's
        // `gone::base` is `peer-type-not-found`.
        let diags = type_diags(&[(
            "n.md",
            "---\ntype: note::base\ntitle: x\n---\n\n# S\n\n```yaml [:x]\ntype: gone::base\ny: z\n```\n^blk\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, PEER_TYPE_NOT_FOUND);
    }

    #[test]
    fn body_typed_block_undeclared_peer_gates() {
        // The body position joins the full gate family: an undeclared peer is
        // `type-repo-unknown`, not silent.
        let diags = type_diags(&[(
            "n.md",
            "---\ntype: note\n---\n\n# S\n\n```yaml [:x]\ntype: note::stranger\n```\n^blk\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
    }

    #[test]
    fn body_typed_block_nested_inline_claim_gates() {
        // A NESTED inline record inside a fence, claiming an undeclared peer, is
        // gated like the same nested claim in frontmatter (review 2.3). Before
        // the gate recursed, the fence's top claim was gated but a nested
        // `note::stranger` was validated-by-skip yet ungated → silent.
        let diags = type_diags(&[(
            "n.md",
            "---\ntype: note\n---\n\n# S\n\n```yaml [:x]\ntype: note\ninner:\n  type: note::stranger\n```\n^blk\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
    }

    #[test]
    fn meta_type_position_gates() {
        // A meta sub-region naming an undeclared peer's type gates at the meta type.
        let diags = type_diags(&[(
            "type/h.type.yaml",
            "meta:\n  - type: note::stranger\nfields: {}\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
    }

    #[test]
    fn meta_present_peer_missing_type_gates() {
        // A meta type naming a present peer that lacks it is peer-type-not-found.
        let diags = type_diags(&[(
            "type/h.type.yaml",
            "meta:\n  - type: gone::base\nfields: {}\n",
        )]);
        let d = only(&diags);
        assert_eq!(d.code, PEER_TYPE_NOT_FOUND);
    }

    #[test]
    fn meta_resolvable_peer_type_is_silent() {
        // `note::base` resolves against base's graph; the gate is satisfied.
        let diags = type_diags(&[(
            "type/h.type.yaml",
            "meta:\n  - type: note::base\nfields: {}\n",
        )]);
        assert!(
            diags.is_empty(),
            "a resolvable meta peer type is silent: {diags:?}"
        );
    }

    #[test]
    fn body_use_position_gates() {
        // A body `use:` naming an undeclared peer gates at the use.
        let diags = type_diags(&[("type/h.type.yaml", "body:\n  - use: note::stranger\n")]);
        let d = only(&diags);
        assert_eq!(d.code, TYPE_REPO_UNKNOWN);
    }

    #[test]
    fn body_use_not_extending_the_peer_is_out_of_closure() {
        // `note::base` resolves, but `h` does NOT extend it, so the peer body is
        // not in `h`'s folded closure — the cross-repo `body-use-out-of-closure`.
        // (The genuinely-silent and splice cases, where the host extends a
        // body-bearing peer, live in `resolution_build::bodyuse_tests`, whose
        // fixture can give the peer a body.)
        let diags = type_diags(&[("type/h.type.yaml", "body:\n  - use: note::base\n")]);
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_str() == "body-use-out-of-closure"),
            "a body-use of a peer the host does not extend is out of closure: {diags:?}"
        );
    }

    #[test]
    fn own_claims_are_not_gated() {
        // A bare claim and a bare field shape are au-core's concern, untouched here.
        let diags = type_diags(&[
            ("n.md", "---\ntype: note\n---\n"),
            ("type/h.type.yaml", "fields:\n  r: note*\n"),
        ]);
        assert!(diags.is_empty(), "own names are not gated: {diags:?}");
    }

    #[test]
    fn per_source_equals_the_whole_catalog_slice() {
        // The per-source function over one instance's parse equals that
        // instance's slice of the whole-catalog pass, so the incremental path
        // recomputes one instance's `::repo` claim gating alone.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        fs.insert("/v/base/type/note.type.yaml", "fields:\n  title: String\n");
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        fs.insert("/v/app/n.md", "---\ntype: note::stranger\n---\n");
        let kb = build(Path::new("/v"), &fs).unwrap();

        let whole = cross_repo_type_diagnostics(
            &kb.catalog,
            &kb.repos,
            &kb.graphs,
            &kb.resolution_graphs,
            &kb.workspaces,
        );
        assert!(!whole.is_empty(), "the undeclared peer should diagnose");

        let n = Path::new("/v/app/n.md");
        let per = cross_repo_type_diagnostics_for(
            kb.catalog.get(n).unwrap().parse.as_ref(),
            &kb.repos,
            &kb.graphs,
            &kb.resolution_graphs,
            &kb.workspaces,
        );
        let slice: Vec<Diagnostic> = whole.iter().filter(|d| d.span.file == n).cloned().collect();
        assert_eq!(
            per, slice,
            "per-source equals the instance's whole-catalog slice"
        );
    }
}
