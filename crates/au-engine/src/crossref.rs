//! Cross-boundary reference resolution: `[[name::repo]]` crosses a repo
//! boundary.
//!
//! Unqualified links resolve repo-local (au-core, the per-repo index). A
//! `::repo` link names a target repo; the engine resolves it here, since only
//! the engine knows the workspace's repos. au-core skips `::repo` links, so
//! their resolution and diagnostics live here.
//!
//! Cross-repo backlink edges and the semantic-token styling of a `::repo` link
//! are a follow-on; this owns the forward resolution and the
//! `reference-repo-*` diagnostics.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use au_core::{
    closure_of, effective_shape, effective_shape_resolved, folded_closure_ids, CrossRepoResolver,
    CrossRepoTarget, EffectiveShape, InstanceValue, NavLink, PeerType, QualifiedDemand, TypeClaim,
    TypeGraph, TypeId, TypeName, TypeNameClaim,
};
use au_diagnostics::{ByteRange, Diagnostic, Severity, Span, SuggestedFix};
use au_parser::{scan_body, BodyEvent};
use au_references::{codes, parse_wikilink_inner, ResolutionError};

use crate::ir::{BuildOutcome, FileEntry, OrdMap, RepoGraphs, RepoIndexes, ResolutionGraphs};
use crate::parse::FileParse;
use crate::repo::{Repo, RepoMap, RepoName, Workspace};

/// A `path -> parsed qualified [`TypeClaim`]` lookup over whatever catalog view
/// the caller holds (the full build's whole-workspace catalog, or the
/// incremental patched view). The qualified-demand check needs a target's
/// STILL-QUALIFIED claims, which the bare `RefData::claims` drops (it maps to
/// names), so this is a separate seam.
pub(crate) trait TargetClaims: Sync {
    fn type_claim(&self, path: &Path) -> Option<&TypeClaim>;
}

/// [`TargetClaims`] over the full build's whole-workspace catalog.
pub(crate) struct CatalogTargetClaims<'a> {
    pub catalog: &'a crate::ir::OrdMap<PathBuf, FileEntry>,
}

impl TargetClaims for CatalogTargetClaims<'_> {
    fn type_claim(&self, path: &Path) -> Option<&TypeClaim> {
        match self.catalog.get(path)?.parse.as_ref() {
            FileParse::Instance {
                instance: Some(inst),
                ..
            } => Some(&inst.type_claim),
            _ => None,
        }
    }
}

/// The `TypeId`s a claim reaches over a name-keyed OWN graph, the no-import
/// fallback for a target repo that has no resolution graph. Mirrors the fold's
/// own-graph closure for a bare claim: each claim's parent closure, keyed by the
/// graph's precomputed `closure_id`. Agrees with `folded_closure_ids` for a bare
/// own claim (both key on `closure_id`), so a target repo with or without a
/// resolution graph yields the same folded ids for an own claim.
fn own_closure_ids(graph: &TypeGraph, claim: &TypeClaim) -> BTreeSet<TypeId> {
    let mut ids = BTreeSet::new();
    for c in claim.iter() {
        // A `::repo` claim makes its repo import (so it has a resolution graph and
        // never reaches this fallback); skip a qualified claim defensively.
        if c.repo.is_some() {
            continue;
        }
        for name in closure_of(graph, &c.name) {
            if let Some(hash) = graph.closure_id(&name) {
                ids.insert(TypeId { name, hash });
            }
        }
    }
    ids
}

/// Engine-side [`CrossRepoResolver`]: resolves a `[[name::repo]]` reference into
/// the named repo and hands au-core that repo's graph, so the repo-agnostic
/// validator can type-check the slot by `(name, canonical-hash)` identity across
/// the boundary.
///
/// Only a present, resolving target feeds the typed check. The
/// `reference-repo-*` and cross-repo target existence diagnostics stay in
/// [`cross_repo_reference_diagnostics`]; an unresolvable reference returns
/// `None`, leaving that pass to own its diagnostics.
pub(crate) struct EngineCrossRepoResolver<'a> {
    pub repos: &'a RepoMap,
    pub indexes: &'a RepoIndexes,
    pub graphs: &'a RepoGraphs,
    pub workspaces: &'a [Workspace],
    /// Repos whose type graph aborted (a broken vocabulary). Their type closure
    /// is unreliable, so a cross-repo typed check into them is skipped, the
    /// target repo's own load diagnostics own the root cause. Without this a
    /// clean consumer would be blamed for the target's broken vocabulary.
    pub graph_aborted: &'a std::collections::BTreeSet<RepoName>,
    /// The per-repo resolution graphs, for a qualified DEMAND (`foo::repo*`): a
    /// target's folded closure is walked over ITS repo's resolution graph. Only
    /// importing repos have one; a non-importing target repo falls back to its
    /// own-graph closure (`own_closure_ids`).
    pub resolution_graphs: &'a ResolutionGraphs,
    /// A target path's parsed, still-qualified `type:` claims, for the folded
    /// closure of a qualified-demand target. The bare `RefData` claims drop the
    /// `::repo`, so the fold reads the parse directly.
    pub target_claims: &'a dyn TargetClaims,
}

impl CrossRepoResolver for EngineCrossRepoResolver<'_> {
    fn resolve(&self, source: &Path, repo: &str, target: &str) -> Option<CrossRepoTarget<'_>> {
        match resolve_cross_repo(
            self.repos,
            self.indexes,
            self.workspaces,
            source,
            repo,
            target,
        ) {
            CrossRepoRef::Resolved(path) => {
                let repo_def = self.repos.by_name(repo)?;
                if self.graph_aborted.contains(&repo_def.name) {
                    // The target repo's vocabulary is broken: its type closure is
                    // unreliable, so skip the typed check rather than emit a
                    // mismatch the consumer cannot act on.
                    return None;
                }
                Some(CrossRepoTarget {
                    path,
                    graph: self.graphs.of(&repo_def.name),
                })
            }
            CrossRepoRef::TargetMissing
            | CrossRepoRef::TargetAmbiguous
            | CrossRepoRef::RepoUnavailable
            | CrossRepoRef::RepoUnknown => None,
        }
    }

    fn qualified_demand(
        &self,
        demand_base: &str,
        demand_repo: &str,
        target_path: &Path,
        target_claim: Option<&TypeClaim>,
    ) -> Option<QualifiedDemand> {
        // The demanded identity: the demand repo's `closure_id` for `demand_base`.
        // An unresolvable / graph-aborted demand repo is uncheckable here (the
        // `crosstype` gate owns its shape-span diagnostic), so skip.
        let dr = self.repos.by_name(demand_repo)?;
        if self.graph_aborted.contains(&dr.name) {
            return None;
        }
        let base = TypeName(demand_base.to_string());
        let hash = self.graphs.of(&dr.name).closure_id(&base)?;
        let demanded = TypeId { name: base, hash };

        // The target's FULL folded closure ids: its repo resolution graph (or its
        // own graph when it does not import) over the target's claims. The claim
        // is the override when given (a `^block-id` target's own block claim), else
        // the target FILE's frontmatter claim from the parse. A target with no
        // parse / no repo yields an empty set, so membership fails and the mismatch
        // fires (the target's own diagnostics own the cause); a graph-aborted
        // target repo is uncheckable, so skip.
        let Some(target_repo) = self.repos.repo_of(target_path) else {
            return Some(QualifiedDemand {
                demanded,
                target_folded: BTreeSet::new(),
            });
        };
        if self.graph_aborted.contains(&target_repo.name) {
            return None;
        }
        let claim = match target_claim {
            Some(c) => Some(c),
            None => self.target_claims.type_claim(target_path),
        };
        let Some(claim) = claim else {
            return Some(QualifiedDemand {
                demanded,
                target_folded: BTreeSet::new(),
            });
        };
        // A `::repo` claim (a `^^` block-referent's own claim naming a peer type)
        // is field-referenced from the target repo's side, so it is absent from
        // that repo's fold. Extend the fold on demand with the claim's `::repo`
        // seeds so a peer-typed block claim resolves OWNER-RELATIVE, parity with
        // the record-target and instances-of walks. Still unresolvable after
        // extension (an undeclared / absent peer) stays uncheckable, skip.
        let seeds: Vec<(TypeName, String)> = claim
            .iter()
            .filter_map(|c| c.repo.as_ref().map(|r| (c.name.clone(), r.clone())))
            .collect();
        let base_rg = self.resolution_graphs.of(&target_repo.name);
        let extended = match (base_rg, seeds.is_empty()) {
            (Some(rg), false) => Some(crate::resolution_build::extend_resolution_graph(
                rg,
                self.graphs,
                self.repos,
                target_repo.name.as_str(),
                &seeds,
            )),
            (None, false) => Some(crate::resolution_build::fold_repo_with_seeds(
                self.graphs,
                self.repos,
                target_repo.name.as_str(),
                &seeds,
            )),
            _ => None,
        };
        let target_folded = match extended.as_ref().or(base_rg) {
            Some(rg) => {
                for c in claim.iter() {
                    if c.repo.is_some() && rg.resolve_authored(&c.name, c.repo.as_deref()).is_none()
                    {
                        return None;
                    }
                }
                folded_closure_ids(rg, claim)
            }
            // No fold and no seeds to build one: only a bare own claim is checkable.
            None => {
                if claim.iter().any(|c| c.repo.is_some()) {
                    return None;
                }
                own_closure_ids(self.graphs.of(&target_repo.name), claim)
            }
        };
        Some(QualifiedDemand {
            demanded,
            target_folded,
        })
    }

    fn owner_effective_shape(
        &self,
        demand_base: &str,
        demand_repo: &str,
    ) -> Option<EffectiveShape> {
        // The demand repo owns the inline value's contract. An unresolvable /
        // aborted repo, an absent type, or a SEALED type (an inline at a sealed
        // peer slot needs an explicit descendant, deferred) yields `None`, so the
        // inline check skips (the `type-repo-*` gate owns the diagnostic).
        let dr = self.repos.by_name(demand_repo)?;
        if self.graph_aborted.contains(&dr.name) {
            return None;
        }
        let base = TypeName(demand_base.to_string());
        let graph = self.graphs.of(&dr.name);
        if !graph.contains(&base) || graph.is_sealed(&base) {
            return None;
        }
        let claim = TypeClaim::Bare(TypeNameClaim::own(base, ByteRange::new(0, 0)));
        // Import-aware, so an owner type whose own closure crosses a `::repo` edge
        // resolves through the owner's resolution graph, like validation does.
        let shape = match self.resolution_graphs.of(&dr.name) {
            Some(rg) => effective_shape_resolved(rg, graph, &claim).ok(),
            None => effective_shape(graph, &claim).ok(),
        }?;
        // The owner's own field names are BARE in the owner's graph, but this shape
        // is validated in the CONSUMER's scope (the source instance). Re-qualify
        // them to the owner repo so a nested reference / record field resolves
        // against the owner, not the source — the second occurrence of the folded-
        // field bug, which the source-side fold never touches (this shape is built
        // from the owner's graph). Mirrors the fold's peer-node re-qualification.
        Some(shape.qualified_to(demand_repo))
    }

    fn peer_type_id(&self, demand_base: &str, demand_repo: &str) -> Option<PeerType> {
        // The demanded identity: the demand repo's `closure_id` for `demand_base`,
        // the same `(name, closure-hash)` a reference demand compares. An
        // unresolvable / aborted repo, or an absent type, is uncheckable here (the
        // `type-repo-*` gate owns its shape-span diagnostic), so `None`.
        let dr = self.repos.by_name(demand_repo)?;
        if self.graph_aborted.contains(&dr.name) {
            return None;
        }
        let base = TypeName(demand_base.to_string());
        let graph = self.graphs.of(&dr.name);
        let hash = graph.closure_id(&base)?;
        let sealed = graph.is_sealed(&base);
        let declared_abstract = graph.declared_abstract_of(&base);
        Some(PeerType {
            id: TypeId { name: base, hash },
            sealed,
            declared_abstract,
        })
    }

    fn peer_graph(&self, repo: &str) -> Option<&TypeGraph> {
        // A cross-repo body `use: parent::repo` splices the peer's body from the
        // peer's own graph. A graph-aborted peer's graph is unreliable, so skip it
        // (the use stays unspliced), the target repo's own load diagnostics own it.
        let r = self.repos.by_name(repo)?;
        if self.graph_aborted.contains(&r.name) {
            return None;
        }
        Some(self.graphs.of(&r.name))
    }
}

/// A minimal [`CrossRepoResolver`] for the wire read path's body splice: it only
/// resolves a peer repo's graph (for a `use: parent::repo`), the sole seam
/// [`splice_effective_body`] needs. `resolve` is unused there (the served
/// effective body splices, it does not type-check references), so it returns
/// `None`. Cheap to build per read from the held knowledge base's repos and graphs.
///
/// [`splice_effective_body`]: au_core::splice_effective_body
pub(crate) struct PeerBodyResolver<'a> {
    pub repos: &'a RepoMap,
    pub graphs: &'a RepoGraphs,
    /// Per-repo build outcome, so a graph-ABORTED peer is skipped like the
    /// validation seam does. A served body view must not splice sections from a
    /// peer whose graph validation deliberately left unspliced.
    pub outcomes: &'a OrdMap<RepoName, BuildOutcome>,
}

impl CrossRepoResolver for PeerBodyResolver<'_> {
    fn resolve(&self, _source: &Path, _repo: &str, _target: &str) -> Option<CrossRepoTarget<'_>> {
        None
    }

    fn peer_graph(&self, repo: &str) -> Option<&TypeGraph> {
        let r = self.repos.by_name(repo)?;
        // A graph-aborted peer's graph is unreliable, so skip it (the `use:` stays
        // unspliced), mirroring `EngineCrossRepoResolver::peer_graph`, so the served
        // body view stays consistent with the diagnostics.
        if self.outcomes.get(&r.name) == Some(&BuildOutcome::AbortedAtGraph) {
            return None;
        }
        Some(self.graphs.of(&r.name))
    }
}

/// The outcome of resolving a `::repo`-qualified reference.
pub(crate) enum CrossRepoRef {
    /// The named repo is present and holds the target.
    Resolved(PathBuf),
    /// The named repo is present but the target is absent from it.
    TargetMissing,
    /// The named repo is present but the target is ambiguous within it.
    TargetAmbiguous,
    /// The named repo is a declared peer or member but not present here.
    RepoUnavailable,
    /// The named repo is neither a declared peer nor a workspace member.
    RepoUnknown,
}

/// The repo-level outcome of a `::repo` qualifier, independent of what the
/// target is — a wikilink target resolved in the index ([`resolve_cross_repo`])
/// or a type name resolved in the graph (`crosstype`). The shared gate: which
/// repo a `::repo` names, and whether it is present, declared-but-absent, or
/// unknown. The target check layers on top per caller.
pub(crate) enum RepoScope<'a> {
    /// The named repo is present in the workspace; carries it for the target
    /// lookup (its index or its graph).
    Present(&'a Repo),
    /// The named repo is a declared peer of the source, or scoped as a member
    /// by the source's own workspace, but not present here.
    Unavailable,
    /// The named repo is neither a declared peer nor a scoped workspace member.
    Unknown,
}

/// Resolve the repo half of a `::repo` qualifier from `source`, independent of
/// the target. A present repo is returned for the caller's target lookup. An
/// absent repo is `Unavailable` when the source declares it as a peer or the
/// source's own workspace scopes it as a member, else `Unknown`.
pub(crate) fn resolve_repo_scope<'a>(
    repos: &'a RepoMap,
    workspaces: &[Workspace],
    source: &Path,
    repo_q: &str,
) -> RepoScope<'a> {
    if let Some(repo) = repos.by_name(repo_q) {
        return RepoScope::Present(repo);
    }
    let source_repo = repos.repo_of(source);
    let declared_peer = source_repo
        .map(|r| r.deps.iter().any(|p| p.name.as_str() == repo_q))
        .unwrap_or(false);
    // A member is scoped only by a workspace the SOURCE belongs to — one that
    // lists both the source's repo and `repo_q`. A member of some other
    // workspace is out of scope from here. (With at most one assembled
    // workspace today this matches the old scan; it diverges once multiple
    // workspaces coexist.)
    let scoped_member = source_repo.is_some_and(|sr| {
        workspaces.iter().any(|w| {
            let source_in_ws = w.members.iter().any(|m| m.name == sr.name);
            source_in_ws && w.members.iter().any(|m| m.name.as_str() == repo_q)
        })
    });
    if declared_peer || scoped_member {
        RepoScope::Unavailable
    } else {
        RepoScope::Unknown
    }
}

/// Resolve a `::repo`-qualified reference from `source` into the named repo.
///
/// A present repo resolves the target against its own index. An absent repo is
/// `RepoUnavailable` when the source declares it as a peer or the source's own
/// workspace scopes it as a member, else `RepoUnknown`.
pub(crate) fn resolve_cross_repo(
    repos: &RepoMap,
    indexes: &RepoIndexes,
    workspaces: &[Workspace],
    source: &Path,
    repo_q: &str,
    target: &str,
) -> CrossRepoRef {
    match resolve_repo_scope(repos, workspaces, source, repo_q) {
        RepoScope::Present(repo) => match indexes.of(&repo.name).resolve(target) {
            Ok(p) => CrossRepoRef::Resolved(p),
            Err(ResolutionError::Missing) => CrossRepoRef::TargetMissing,
            Err(ResolutionError::Ambiguous(_)) => CrossRepoRef::TargetAmbiguous,
        },
        RepoScope::Unavailable => CrossRepoRef::RepoUnavailable,
        RepoScope::Unknown => CrossRepoRef::RepoUnknown,
    }
}

/// Diagnostics for every `::repo` reference in the workspace: the
/// `reference-repo-*` codes, plus the cross-repo target-missing / ambiguous
/// codes when the named repo is present but the target does not resolve there.
/// Unqualified links are au-core's concern and are not revisited here.
pub(crate) fn cross_repo_reference_diagnostics(
    catalog: &crate::ir::OrdMap<PathBuf, FileEntry>,
    repos: &RepoMap,
    indexes: &RepoIndexes,
    workspaces: &[Workspace],
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for entry in catalog.values() {
        diags.extend(cross_repo_reference_diagnostics_for(
            entry.parse.as_ref(),
            repos,
            indexes,
            workspaces,
        ));
    }
    diags
}

/// The cross-boundary `::repo` reference diagnostics for one source file's
/// parse. The per-source unit the whole-catalog pass loops, and the one the
/// incremental path calls for a single changed instance, so its `::repo`
/// diagnostics recompute without a whole-catalog walk. A non-instance,
/// non-note parse contributes none.
pub(crate) fn cross_repo_reference_diagnostics_for(
    parse: &FileParse,
    repos: &RepoMap,
    indexes: &RepoIndexes,
    workspaces: &[Workspace],
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let (source, fields, body, body_offset, is_markdown) = match parse {
        FileParse::Instance {
            instance: Some(inst),
            body,
            body_offset,
            is_markdown,
            ..
        } => (
            &inst.source_path,
            &inst.fields,
            body,
            *body_offset,
            *is_markdown,
        ),
        FileParse::Note {
            source_path,
            fields,
            body,
            body_offset,
            ..
        } => (source_path, fields, body, *body_offset, true),
        _ => return diags,
    };

    for field in fields {
        collect_field(
            source,
            &field.value,
            &field.nav_links,
            repos,
            indexes,
            workspaces,
            &mut diags,
        );
    }

    if is_markdown {
        for ev in scan_body(body) {
            let BodyEvent::Wikilink { raw, span } = ev else {
                continue;
            };
            let Ok(w) = parse_wikilink_inner(raw) else {
                continue;
            };
            if let Some(repo_q) = &w.repo {
                // A commit-pinned reference is INERT: it resolves against its
                // commit's tree, not the peer's live state, so the cross-repo
                // existence check does not apply. This covers both the named-target
                // pin (`[[note::repo@sha]]`, whose live counterpart may be gone) and
                // the commit-referent (`[[::repo@sha]]`, which names no file). See
                // the pinned-references spec.
                if w.is_inert_pin() {
                    continue;
                }
                emit(
                    source,
                    ByteRange::new(span.start + body_offset, span.end + body_offset),
                    repo_q,
                    &w.target,
                    repos,
                    indexes,
                    workspaces,
                    &mut diags,
                );
            }
        }
    }
    diags
}

/// Recurse a frontmatter field value for `::repo` links, descending sequences
/// and inline records, mirroring the backlink walk.
#[allow(clippy::too_many_arguments)]
fn collect_field(
    source: &Path,
    value: &InstanceValue,
    nav_links: &[NavLink],
    repos: &RepoMap,
    indexes: &RepoIndexes,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    match value {
        InstanceValue::String(_) => {
            for nl in nav_links {
                let Ok(w) = parse_wikilink_inner(&nl.raw) else {
                    continue;
                };
                if let Some(repo_q) = &w.repo {
                    // A commit-pinned reference is INERT, resolved against its
                    // commit's tree, not the peer's live state, so the cross-repo
                    // existence check does not apply. Covers the named-target pin
                    // and the commit-referent alike. See the pinned-references spec.
                    if w.is_inert_pin() {
                        continue;
                    }
                    emit(
                        source, nl.span, repo_q, &w.target, repos, indexes, workspaces, diags,
                    );
                }
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                collect_field(
                    source,
                    &e.value,
                    &e.nav_links,
                    repos,
                    indexes,
                    workspaces,
                    diags,
                );
            }
        }
        InstanceValue::Mapping(inline) => {
            for f in &inline.fields {
                collect_field(
                    source,
                    &f.value,
                    &f.nav_links,
                    repos,
                    indexes,
                    workspaces,
                    diags,
                );
            }
        }
        _ => {}
    }
}

/// Resolve one `::repo` reference and push its diagnostic, if any.
#[allow(clippy::too_many_arguments)]
fn emit(
    source: &Path,
    span: ByteRange,
    repo_q: &str,
    target: &str,
    repos: &RepoMap,
    indexes: &RepoIndexes,
    workspaces: &[Workspace],
    diags: &mut Vec<Diagnostic>,
) {
    let (code, severity, message) =
        match resolve_cross_repo(repos, indexes, workspaces, source, repo_q, target) {
            CrossRepoRef::Resolved(_) => return,
            // An IMPOSSIBLE address must not sit in the bucket that means "not
            // written yet". The qualifier says which repo to resolve in; a target
            // that climbs out of that repo contradicts its own scope, so no file
            // authored later makes it resolve. The unqualified surfaces already
            // draw this distinction, and a consumer matching on the code must not
            // see it appear and disappear with the spelling.
            CrossRepoRef::TargetMissing if au_references::target_escapes_repo(target) => (
                au_references::codes::REFERENCE_PATH_ESCAPES_REPO,
                Severity::Warning,
                format!(
                    "reference '{target}::{repo_q}' names a path that leaves repo '{repo_q}'; a \
                     wikilink is repo-scoped, so it can never resolve"
                ),
            ),
            CrossRepoRef::TargetMissing => (
                codes::REFERENCE_TARGET_MISSING,
                Severity::Warning,
                format!("reference '{target}::{repo_q}' does not exist in repo '{repo_q}'"),
            ),
            CrossRepoRef::TargetAmbiguous => (
                codes::REFERENCE_TARGET_AMBIGUOUS,
                Severity::Error,
                format!("reference '{target}::{repo_q}' is ambiguous in repo '{repo_q}'"),
            ),
            CrossRepoRef::RepoUnavailable => (
                codes::REFERENCE_REPO_UNAVAILABLE,
                Severity::Warning,
                format!("reference repo '{repo_q}' is declared but not present in this workspace"),
            ),
            CrossRepoRef::RepoUnknown => (
                codes::REFERENCE_REPO_UNKNOWN,
                Severity::Error,
                format!("reference repo '{repo_q}' is not a declared peer or workspace member"),
            ),
        };
    let escapes = code == au_references::codes::REFERENCE_PATH_ESCAPES_REPO;
    diags.push(Diagnostic {
        code,
        severity,
        span: Span::new(source.to_path_buf(), span),
        message,
        related: Vec::new(),
        fix: escapes.then(|| SuggestedFix {
            description: au_references::ESCAPES_REPO_FIX.to_string(),
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{discover_repos, RepoEntry};
    use std::collections::BTreeMap;

    #[test]
    fn per_source_cross_repo_equals_the_whole_catalog_slice() {
        // An instance with a `::repo` reference to an undeclared repo produces a
        // cross-repo diagnostic. The per-source function over that instance's
        // parse must equal the instance's slice of the whole-catalog pass, so
        // the incremental path can recompute one instance's `::repo` diagnostics
        // alone.
        use crate::build::build;
        use au_parser::MemoryFileSystem;
        use std::path::Path;

        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  link?: note*\n".to_vec(),
        );
        fs.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[target::other]]\"\n---\n".to_vec(),
        );
        let kb = build(Path::new("/v"), &fs).unwrap();

        let whole =
            cross_repo_reference_diagnostics(&kb.catalog, &kb.repos, &kb.indexes, &kb.workspaces);
        assert!(
            !whole.is_empty(),
            "the `::repo` reference to an undeclared repo should diagnose"
        );

        let a = Path::new("/v/a.md");
        let per = cross_repo_reference_diagnostics_for(
            kb.catalog.get(a).unwrap().parse.as_ref(),
            &kb.repos,
            &kb.indexes,
            &kb.workspaces,
        );
        let slice: Vec<Diagnostic> = whole.iter().filter(|d| d.span.file == a).cloned().collect();
        assert_eq!(
            per, slice,
            "per-source equals the instance's whole-catalog slice"
        );
    }

    fn repo_map_with_app() -> RepoMap {
        // One declared repo `app` at /v/app. `discover_repos` takes pre-read
        // registry bytes, so no filesystem walk is needed.
        let registries = vec![(
            PathBuf::from("/v/app/.arsumbris/repo.yaml"),
            b"name: app\n".to_vec(),
        )];
        let roots = vec![PathBuf::from("/v/app")];
        discover_repos(&roots, &registries, &BTreeMap::new(), None).0
    }

    fn workspace(name: &str, members: &[&str]) -> Workspace {
        Workspace {
            manifest_path: PathBuf::from(format!("/v/{name}/.arsumbris/workspace.yaml")),
            name: name.to_string(),
            members: members
                .iter()
                .map(|m| RepoEntry {
                    name: RepoName((*m).to_string()),
                    description: None,
                    remote: None,
                    git_ref: None,
                    path: None,
                })
                .collect(),
            member_paths: BTreeMap::new(),
            member_roles: BTreeMap::new(),
            edit: members.iter().map(|m| RepoName((*m).to_string())).collect(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
        }
    }

    /// A member is scoped only by the SOURCE's own workspace. A member of a
    /// different workspace the source does not belong to is out of scope, so it
    /// is `RepoUnknown` (an error), not the declared-but-absent `RepoUnavailable`
    /// (a warning). Today the build assembles at most one workspace, so this only
    /// bites once multi-workspace assembly lands — the unit input exercises it
    /// directly.
    #[test]
    fn only_the_sources_own_workspace_scopes_a_member() {
        let repos = repo_map_with_app();
        let indexes = RepoIndexes::default();
        let source = PathBuf::from("/v/app/note.md");

        // app belongs to w1; `far` is a member of w2 only. From app, `far`
        // shares no workspace, so it is RepoUnknown.
        let two = vec![workspace("w1", &["app"]), workspace("w2", &["far"])];
        let r = resolve_cross_repo(&repos, &indexes, &two, &source, "far", "x");
        assert!(
            matches!(r, CrossRepoRef::RepoUnknown),
            "a member of a workspace the source does not belong to is not scoped"
        );

        // A member of the source's OWN workspace is scoped (declared-but-absent).
        let one = vec![workspace("w1", &["app", "near"])];
        let r2 = resolve_cross_repo(&repos, &indexes, &one, &source, "near", "x");
        assert!(
            matches!(r2, CrossRepoRef::RepoUnavailable),
            "a member of the source's own workspace is scoped"
        );
    }
}
