//! Engine-side construction of the per-repo cross-repo resolution graphs.
//!
//! au-core owns the [`fold`] algorithm (the `TypeId`-keyed claim / parent fold,
//! see [[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]),
//! but only the engine knows the workspace's repos, so the engine supplies the
//! [`PeerGraphResolver`] over [`RepoGraphs`] and discovers the import set from
//! the catalog. A repo's import set is its instance `::repo` claims (the seeds)
//! plus its own-def `::repo` parents (the fold walks those from the own graph).
//!
//! Only a repo that actually imports is folded; a repo with no `::repo` claim or
//! parent resolves against its own [`TypeGraph`] and gets no entry, so a
//! single-repo knowledge base engages none of this. The field-shape axis (`foo::repo*`)
//! is NOT a seed, it stays on the cross-repo reference seam.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_core::resolution::{fold, PeerGraphResolver};
use au_core::{
    EffectiveShape, InlineValue, Instance, InstanceValue, ResolutionGraph, TypeClaim, TypeGraph,
    TypeId, TypeName, TypeNameClaim,
};

use crate::ir::{FileEntry, KnowledgeBase, OrdMap, RepoGraphs, ResolutionGraphs};
use crate::parse::FileParse;
use crate::repo::{RepoMap, RepoName};

/// Resolves a repo's own [`TypeGraph`] by its global name, the fold's seam across
/// a `::repo` boundary. A repo not in the map (undeclared / unmounted peer)
/// yields `None`, so the fold drops that edge and the engine's gate owns the
/// diagnostic.
pub(crate) struct RepoGraphResolver<'a> {
    pub(crate) graphs: &'a RepoGraphs,
    pub(crate) repos: &'a RepoMap,
}

impl PeerGraphResolver for RepoGraphResolver<'_> {
    fn graph_of(&self, repo: &str) -> Option<&TypeGraph> {
        self.repos.by_name(repo).map(|r| self.graphs.of(&r.name))
    }
}

/// The addressable-record index for a file, resolved OWNER-RELATIVE.
///
/// The engine-side wrapper over [`au_core::collect_record_targets`]: it resolves
/// the host's effective shape over the fold (so a file claiming a peer type sees
/// its peer-typed slots) and hands the walk a [`RepoGraphResolver`] so a record
/// claiming a peer type reaches that type's slots in the owner repo. A slot-pinned
/// peer record therefore carries its owner-qualified claim, so a `[[file^^id]]`
/// block-referent type-checks across the boundary instead of silently skipping.
pub(crate) fn record_targets_of(
    graphs: &RepoGraphs,
    repos: &RepoMap,
    resolution_graphs: &ResolutionGraphs,
    path: &Path,
    inst: &Instance,
) -> au_core::RecordTargets {
    let (graph, res, own_repo) = match repos.repo_of(path) {
        Some(r) => (
            graphs.of(&r.name),
            resolution_graphs.of(&r.name),
            r.name.as_str().to_string(),
        ),
        None => (graphs.empty(), None, String::new()),
    };
    let resolver = RepoGraphResolver { graphs, repos };
    au_core::collect_record_targets(graph, res, Some(&resolver), &own_repo, inst)
}

/// [`record_targets_of`] over a held knowledge base, for the read and incremental
/// paths that already hold one.
pub(crate) fn record_targets_of_kb(
    kb: &crate::KnowledgeBase,
    path: &Path,
    inst: &Instance,
) -> au_core::RecordTargets {
    record_targets_of(&kb.graphs, &kb.repos, &kb.resolution_graphs, path, inst)
}

/// The occurrence list (duplicate `^:` ids included) for a file, resolved
/// owner-relative like [`record_targets_of_kb`]. Backs the block-id listing read.
pub(crate) fn record_target_occurrences_of_kb(
    kb: &crate::KnowledgeBase,
    path: &Path,
    inst: &Instance,
) -> Vec<(String, au_core::RecordTarget)> {
    let (graph, res, own_repo) = match kb.repos.repo_of(path) {
        Some(r) => (
            kb.graphs.of(&r.name),
            kb.resolution_graphs.of(&r.name),
            r.name.as_str().to_string(),
        ),
        None => (kb.graphs.empty(), None, String::new()),
    };
    let resolver = RepoGraphResolver {
        graphs: &kb.graphs,
        repos: &kb.repos,
    };
    au_core::collect_record_target_occurrences(graph, res, Some(&resolver), &own_repo, inst)
}

/// The declared shape governing the value at `span` in a file, resolved
/// OWNER-RELATIVE.
///
/// The engine-side wrapper over [`au_core::slot_shape_at`]: it resolves the host's
/// effective shape over the fold (so a file claiming a peer type sees its
/// peer-typed slots) and hands the walk a [`RepoGraphResolver`] so a nested record
/// claiming a peer type reaches that type's slots in the owner repo. The
/// promote / inline guards route through here, so a slot in a cross-repo-claiming
/// host is seen rather than skipped.
pub(crate) fn slot_shape_at_of_kb(
    kb: &crate::KnowledgeBase,
    path: &Path,
    inst: &Instance,
    span: au_diagnostics::ByteRange,
) -> Option<au_grammar::Shape> {
    let (graph, res, own_repo) = match kb.repos.repo_of(path) {
        Some(r) => (
            kb.graphs.of(&r.name),
            kb.resolution_graphs.of(&r.name),
            r.name.as_str().to_string(),
        ),
        None => (kb.graphs.empty(), None, String::new()),
    };
    let resolver = RepoGraphResolver {
        graphs: &kb.graphs,
        repos: &kb.repos,
    };
    au_core::slot_shape_at(graph, res, Some(&resolver), &own_repo, inst, span)
}

/// Whether the record at `span` sits in a reference-admitting slot, resolved
/// owner-relative like [`slot_shape_at_of_kb`]. The promote guard's input.
pub(crate) fn record_slot_admits_reference_at_of_kb(
    kb: &crate::KnowledgeBase,
    path: &Path,
    inst: &Instance,
    span: au_diagnostics::ByteRange,
) -> Option<bool> {
    slot_shape_at_of_kb(kb, path, inst, span).map(|s| au_core::shape_is_inline_or_reference(&s))
}

/// Fold `extra_seeds` into an already-built resolution graph over the workspace's
/// own graphs, the on-demand companion to the pre-built fold. Resolves a peer type
/// no file has yet imported (so it is absent from the pre-built graph), which the
/// ensure-mixin gate needs on a first promotion
/// ([[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]]).
pub(crate) fn extend_resolution_graph(
    existing: &ResolutionGraph,
    graphs: &RepoGraphs,
    repos: &RepoMap,
    own_repo: &str,
    extra_seeds: &[(TypeName, String)],
) -> ResolutionGraph {
    let resolver = RepoGraphResolver { graphs, repos };
    au_core::resolution::fold_extending(existing, own_repo, extra_seeds, &resolver)
}

/// A fresh resolution graph for `own_repo`, its own vocabulary plus `seeds`, for a
/// repo that imports nothing today (so it has no pre-built graph to extend). The
/// non-importing sibling of [`extend_resolution_graph`], used by the ensure-mixin
/// gate when the first import in a repo is the mixin itself.
pub(crate) fn fold_repo_with_seeds(
    graphs: &RepoGraphs,
    repos: &RepoMap,
    own_repo: &str,
    seeds: &[(TypeName, String)],
) -> ResolutionGraph {
    let resolver = RepoGraphResolver { graphs, repos };
    au_core::resolution::fold(own_repo, seeds, &resolver)
}

/// Build the per-repo resolution graphs from the own graphs plus the catalog's
/// discovered import set. A pure function of `(graphs, repos, catalog)`, so the
/// full build and the incremental apply compute identical graphs from identical
/// inputs.
pub(crate) fn build_resolution_graphs(
    graphs: &RepoGraphs,
    repos: &RepoMap,
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> ResolutionGraphs {
    build_resolution_graphs_from(
        graphs,
        repos,
        catalog.iter().map(|(p, e)| (p.as_path(), e.parse.as_ref())),
    )
}

/// The [`build_resolution_graphs`] core over a `(path, parse)` iterator rather
/// than a materialized catalog, so the incremental path can feed it the patched
/// catalog view (held entries with the dirty parses overlaid) without building a
/// whole `FileEntry` catalog first, the Option-3 up-front fold recompute.
pub(crate) fn build_resolution_graphs_from<'a>(
    graphs: &RepoGraphs,
    repos: &RepoMap,
    instance_parses: impl Iterator<Item = (&'a std::path::Path, &'a FileParse)>,
) -> ResolutionGraphs {
    let resolver = RepoGraphResolver { graphs, repos };

    // `::repo` fold seeds, grouped by the CONSUMING repo (where the file lives).
    // An instance contributes its `::repo` claims (frontmatter, inline-record,
    // body typed-block); a type-def contributes its meta sub-region `::repo` types
    // (a meta `type: dm::repo` validates against the folded peer meta type, the
    // meta-position sibling of an instance claim seed).
    let mut seeds_by_repo: BTreeMap<RepoName, Vec<(TypeName, String)>> = BTreeMap::new();
    for (path, parse) in instance_parses {
        let Some(repo) = repos.repo_of(path) else {
            continue;
        };
        let mut seeds = Vec::new();
        match parse {
            FileParse::Instance {
                instance: Some(inst),
                body,
                is_markdown,
                ..
            } => {
                collect_instance_seeds(inst, &mut seeds);
                // A body typed-block claim (` ```yaml [:field] ` fence) claiming a
                // peer type is a fold seed too, the claim-position sibling of the
                // frontmatter and inline-record claims. Recurses nested inline
                // records inside the fence, mirroring `collect_value_seeds` for
                // frontmatter, so a nested `type: peer::repo` folds and validates.
                if *is_markdown {
                    seeds.extend(au_core::collect_body_typed_block_seeds(body));
                }
            }
            FileParse::TypeDef {
                type_def: Some(td), ..
            } => {
                if let Some(blocks) = &td.meta_blocks {
                    for b in blocks {
                        if let Some(repo_q) = &b.repo {
                            seeds.push((b.type_name.clone(), repo_q.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
        if !seeds.is_empty() {
            seeds_by_repo
                .entry(repo.name.clone())
                .or_default()
                .extend(seeds);
        }
    }

    // Fold a repo iff it imports: it has an instance `::repo` claim seed, or an
    // own-def `::repo` parent. Otherwise it resolves against its own graph and
    // gets no entry.
    let mut out: BTreeMap<RepoName, au_core::ResolutionGraph> = BTreeMap::new();
    for repo in repos.repos() {
        // Borrow the seed bucket rather than cloning it. `fold` takes a slice,
        // and a borrow reads the same merged bucket a clone would, so the
        // outcome is identical even if two repos share a name (removing the
        // entry would empty it for the second same-named repo).
        let seeds: &[(TypeName, String)] = seeds_by_repo
            .get(&repo.name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let has_qualified_parent = graphs
            .of(&repo.name)
            .iter()
            .any(|(_, td)| td.parents.iter().any(|p| p.is_qualified()));
        if seeds.is_empty() && !has_qualified_parent {
            continue;
        }
        out.insert(
            repo.name.clone(),
            fold(repo.name.as_str(), seeds, &resolver),
        );
    }
    ResolutionGraphs::new(out)
}

/// Compute an instance's effective shape over the resolution graph when its repo
/// imports, else the own graph, the resolved-layer sibling of the validator's
/// `effective_shape_for` seam. Routing the SERVED shape through the same layer as
/// the diagnostics keeps them consistent: an importing instance's served shape
/// carries the folded peer fields, not an own-only (empty) shape.
pub(crate) fn resolved_effective_shape(
    own_graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    claim: &TypeClaim,
) -> Option<EffectiveShape> {
    match resolution {
        Some(rg) => au_core::effective_shape_resolved(rg, own_graph, claim).ok(),
        None => au_core::effective_shape(own_graph, claim).ok(),
    }
}

/// A nested record's effective shape resolved OWNER-RELATIVE, the value-layer
/// parallel of [`crate::wire::nested_closure_tids`].
///
/// A `::repo` claim the SOURCE repo never imported — a slot-pinned peer record,
/// whose owner-qualified identity is not a node in the source fold (a
/// field-shape reference is not folded in) — resolves in the OWNER repo's own
/// graph instead, the same `kb.graphs.of(repo)` lookup the closure side uses. So
/// the nested record's fields elaborate against the peer shape rather than
/// degrading to untyped. A claim the source fold DOES resolve is left to it, so
/// a legitimately-imported peer stays unchanged.
///
/// The owner-relative branch is a single-name `::repo` claim only; a MIXIN
/// claim keeps the fold result, and this is complete rather than a narrowing. A
/// non-seeded peer identity only ever arises from a SLOT DEMAND (a claim-less
/// nested record takes the enclosing slot's pinned type, [`demand_claim`]), and
/// a slot demand is always ONE name. A nested record with an EXPLICIT mixin
/// `type: [own, peer::repo]` instead SEEDS each `::repo` member into the source
/// fold ([`collect_value_seeds`] recurses into nested-record claims), so the
/// fold already resolves every member — verified by
/// `cross_repo_claim_reads::value_layer_resolves_every_member_of_a_cross_repo_mixin_nested_record`.
/// So the two non-seeding-vs-seeding cases partition cleanly: single here,
/// mixin at the fold. Purely additive over [`resolved_effective_shape`].
pub(crate) fn owner_relative_effective_shape(
    kb: &KnowledgeBase,
    source_own: &TypeGraph,
    source_res: Option<&ResolutionGraph>,
    claim: &TypeClaim,
) -> Option<EffectiveShape> {
    // A single `::repo` claim the source fold never imported resolves in its
    // owner's graph. The `resolve_authored` gate mirrors `nested_closure_tids`:
    // a claim the fold DOES import is left to the fold below, and the gate avoids
    // the trap where `resolved_effective_shape` returns a shape MISSING the peer
    // type's fields (a field-shape reference is not a fold node), which would
    // otherwise mask the fallback.
    if let TypeClaim::Bare(c) = claim {
        if let Some(owner) = c.repo.as_deref() {
            let imported = source_res
                .and_then(|rg| rg.resolve_authored(&c.name, Some(owner)))
                .is_some();
            if !imported {
                if let Some(owner_repo) = kb.repos.by_name(owner) {
                    let owner_own = kb.graphs.of(&owner_repo.name);
                    let owner_res = kb.resolution_graphs.of(&owner_repo.name);
                    let bare = TypeClaim::Bare(TypeNameClaim::own(c.name.clone(), c.span));
                    return resolved_effective_shape(owner_own, owner_res, &bare);
                }
            }
        }
    }
    // An own type, a legitimately-imported peer, or a mixin: the source fold.
    resolved_effective_shape(source_own, source_res, claim)
}

/// Collect every `::repo`-qualified claim an instance makes, its frontmatter
/// identity claim plus any inline-record claim in its field values. Unqualified
/// claims are au-core's own concern and contribute no seed.
fn collect_instance_seeds(inst: &Instance, out: &mut Vec<(TypeName, String)>) {
    for claim in inst.type_claim.iter() {
        if let Some(repo) = &claim.repo {
            out.push((claim.name.clone(), repo.clone()));
        }
    }
    for f in &inst.fields {
        collect_value_seeds(&f.value, out);
    }
}

/// Every directly-authored `::repo` import a repo makes, deduped, as
/// `(importer, name, owner)` triples.
///
/// An import is a peer type folded INTO the repo's graph, on the fold axis.
/// Those are the positions the fold seeds from or walks: an instance `::repo`
/// claim (frontmatter, inline-record, body typed-block), a type-def `::repo`
/// parent, a meta `type: dm::repo`, and a body `use: t::repo`. A field-shape
/// `foo::repo*` is a cross-repo REFERENCE on the seam, not an import, so it is
/// excluded. Only R's OWN authored uses count, a transitive peer ancestor a
/// folded type pulls in is not R's import. The identity resolution (the hash) is
/// the caller's; this collects the authored `(name, owner)` set.
pub(crate) fn collect_imports(
    repos: &RepoMap,
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> BTreeSet<(RepoName, TypeName, String)> {
    let mut out: BTreeSet<(RepoName, TypeName, String)> = BTreeSet::new();
    for (path, entry) in catalog.iter() {
        let Some(repo) = repos.repo_of(path) else {
            continue;
        };
        match entry.parse.as_ref() {
            FileParse::Instance {
                instance: Some(inst),
                body,
                is_markdown,
                ..
            } => {
                let mut seeds = Vec::new();
                collect_instance_seeds(inst, &mut seeds);
                if *is_markdown {
                    seeds.extend(au_core::collect_body_typed_block_seeds(body));
                }
                for (name, owner) in seeds {
                    out.insert((repo.name.clone(), name, owner));
                }
            }
            FileParse::TypeDef {
                type_def: Some(td), ..
            } => {
                for p in &td.parents {
                    if let Some(repo_q) = &p.repo {
                        out.insert((repo.name.clone(), p.name.clone(), repo_q.clone()));
                    }
                }
                if let Some(blocks) = &td.meta_blocks {
                    for b in blocks {
                        if let Some(repo_q) = &b.repo {
                            out.insert((repo.name.clone(), b.type_name.clone(), repo_q.clone()));
                        }
                    }
                }
                // A `required: X::repo` obligation references a peer meta type, so
                // it seeds the fold like a `::repo` parent / meta block, letting
                // the obligation resolve and its meta-legality be checked.
                for r in &td.required_meta {
                    if let Some(repo_q) = &r.repo {
                        out.insert((repo.name.clone(), r.name.clone(), repo_q.clone()));
                    }
                }
                if let Some(body) = &td.body {
                    collect_body_use_imports(body, &repo.name, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

/// An instance's closure as two `TypeId` sets: the identities it directly
/// CLAIMS, and the ancestor identities it INHERITS (reached by walking the
/// parents of a claimed type). The two can overlap, a mixin that claims `note`
/// and also claims `card` extending `note` has `note` in BOTH.
///
/// Over the repo's resolution graph when it imports (so a `::repo` claim resolves
/// to its folded peer identity), else its own graph (a non-importing repo, whose
/// claims and parents are all bare).
pub(crate) fn instance_closure_tids(
    own_graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    claim: &TypeClaim,
) -> (BTreeSet<TypeId>, BTreeSet<TypeId>) {
    match resolution {
        Some(rg) => {
            let mut claimed = BTreeSet::new();
            let mut ancestors = BTreeSet::new();
            for c in claim.iter() {
                let Some(tid) = rg.resolve_authored(&c.name, c.repo.as_deref()) else {
                    continue;
                };
                claimed.insert(tid.clone());
                let mut stack: Vec<TypeId> =
                    rg.get(tid).map(|n| n.parents.clone()).unwrap_or_default();
                while let Some(p) = stack.pop() {
                    if ancestors.insert(p.clone()) {
                        if let Some(n) = rg.get(&p) {
                            stack.extend(n.parents.clone());
                        }
                    }
                }
            }
            (claimed, ancestors)
        }
        None => {
            let mut claimed = BTreeSet::new();
            let mut ancestors = BTreeSet::new();
            let tid = |n: &TypeName| {
                own_graph.closure_id(n).map(|hash| TypeId {
                    name: n.clone(),
                    hash,
                })
            };
            let bare_parents = |n: &TypeName| -> Vec<TypeName> {
                own_graph
                    .get(n)
                    .map(|td| {
                        td.parents
                            .iter()
                            .filter(|p| p.repo.is_none())
                            .map(|p| p.name.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for c in claim.iter() {
                if c.repo.is_some() {
                    continue; // a `::repo` claim never resolves against the own graph
                }
                if let Some(t) = tid(&c.name) {
                    claimed.insert(t);
                }
                let mut stack = bare_parents(&c.name);
                while let Some(pn) = stack.pop() {
                    if let Some(t) = tid(&pn) {
                        if ancestors.insert(t) {
                            stack.extend(bare_parents(&pn));
                        }
                    }
                }
            }
            (claimed, ancestors)
        }
    }
}

/// Collect the `::repo` targets of every body `use:` in a type-def body, into
/// `out` keyed by `importer`. `use:` parses only at the top level, but section
/// sub-bodies are walked defensively, mirroring the cross-repo body-use gate.
fn collect_body_use_imports(
    items: &[au_core::BodyItem],
    importer: &RepoName,
    out: &mut BTreeSet<(RepoName, TypeName, String)>,
) {
    for item in items {
        match item {
            au_core::BodyItem::Use {
                type_name,
                repo: Some(repo_q),
                ..
            } => {
                out.insert((importer.clone(), type_name.clone(), repo_q.clone()));
            }
            au_core::BodyItem::Section {
                body: Some(sub), ..
            } => collect_body_use_imports(sub, importer, out),
            _ => {}
        }
    }
}

/// Recurse a field value for inline-record `::repo` claims, descending sequences
/// and nested inline records, mirroring the cross-repo type-gating walk.
fn collect_value_seeds(value: &InstanceValue, out: &mut Vec<(TypeName, String)>) {
    match value {
        InstanceValue::Mapping(InlineValue {
            type_claim, fields, ..
        }) => {
            if let Some(tc) = type_claim {
                for claim in tc.iter() {
                    if let Some(repo) = &claim.repo {
                        out.push((claim.name.clone(), repo.clone()));
                    }
                }
            }
            for f in fields {
                collect_value_seeds(&f.value, out);
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                collect_value_seeds(&e.value, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use crate::ir::KnowledgeBase;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// Build a two-repo workspace: `base` owns `note { title }`, `app` peers base
    /// and carries `app_files`.
    fn build_two_repo(app_files: &[(&str, &str)]) -> KnowledgeBase {
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
        for (rel, content) in app_files {
            fs.insert(format!("/v/app/{rel}"), *content);
        }
        build(Path::new("/v"), &fs).unwrap()
    }

    fn app(v: &KnowledgeBase) -> Option<&au_core::ResolutionGraph> {
        v.resolution_graphs.of(&RepoName("app".into()))
    }

    #[test]
    fn a_repo_with_no_user_imports_folds_only_the_builtin_engine_schema() {
        // app owns its own `card` and authors no `::repo`. But its `repo.yaml` is
        // a typed instance claiming `au.engine.repo::au-engine`, so app folds the
        // builtin engine schema (visible, not filtered) and thus HAS a resolution
        // graph — carrying au.engine.repo, but no user peer type.
        let v = build_two_repo(&[("type/card.type.yaml", "fields:\n  n: Number\n")]);
        let rg = app(&v).expect("repo.yaml imports au.engine.repo, so app folds it");
        assert!(
            rg.resolve_authored(
                &au_core::TypeName("au.engine.repo".into()),
                Some("au-engine")
            )
            .is_some(),
            "the builtin engine schema is folded"
        );
        assert!(
            rg.resolve_authored(&au_core::TypeName("note".into()), Some("base"))
                .is_none(),
            "no user peer type is imported"
        );
    }

    #[test]
    fn an_instance_claim_builds_a_resolution_graph_with_the_peer_type() {
        // app's instance claims `note::base`: app gets a resolution graph that
        // folds `note` from base.
        let v = build_two_repo(&[("n.md", "---\ntype: note::base\ntitle: x\n---\n")]);
        let rg = app(&v).expect("an importing repo has a resolution graph");
        let note = rg
            .iter()
            .find(|(id, _)| id.name.as_str() == "note")
            .expect("note folded in");
        assert_eq!(note.1.origin, Some("base".into()));
    }

    #[test]
    fn an_instance_claiming_a_peer_type_validates_against_its_folded_fields() {
        // base owns `note { title: String }`, title required. An instance
        // `type: note::base` with no `title` must fire required-field-absent, the
        // headline payoff, the folded peer field is enforced.
        let missing = build_two_repo(&[("n.md", "---\ntype: note::base\n---\n")]);
        assert!(
            missing
                .diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"),
            "the folded peer field `title` must be required: {:?}",
            missing
                .diagnostics()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );

        // Supplying the field clears it.
        let ok = build_two_repo(&[("n.md", "---\ntype: note::base\ntitle: hi\n---\n")]);
        assert!(
            !ok.diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"),
            "a satisfied peer field must not error: {:?}",
            ok.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_served_shape_of_an_importer_carries_the_peer_fields() {
        // The resolved layer (served on the wire) must be consistent with
        // validation: an instance claiming `note::base` has `title` in its served
        // effective shape, not an own-only empty shape.
        let v = build_two_repo(&[("n.md", "---\ntype: note::base\ntitle: hi\n---\n")]);
        let shape = v
            .instances
            .get(&std::path::PathBuf::from("/v/app/n.md"))
            .and_then(|ri| ri.effective_shape.as_ref())
            .expect("the importing instance has a resolved shape");
        assert!(
            shape.get(&au_core::FieldName("title".into())).is_some(),
            "the served shape must carry the folded peer field `title`, got {:?}",
            shape.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_subtype_extending_a_peer_parent_inherits_its_required_fields() {
        // app's own `card` extends `note::base`; an instance `type: card` must
        // still supply `title`, the inherited peer field, proving the parent fold.
        let v = build_two_repo(&[
            (
                "type/card.type.yaml",
                "extends: note::base\nfields:\n  n: Number\n",
            ),
            ("c.md", "---\ntype: card\nn: 1\n---\n"),
        ]);
        assert!(
            v.diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"),
            "the inherited peer field `title` must be required: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_imported_type_does_not_leak_into_the_own_graph() {
        // app owns `note { a }` AND imports `note::base` (base owns `note { title }`).
        // The two-layer split: the OWN graph keeps only app's note, so
        // `duplicate-type-def` (an own-graph rule) never fires and compose / drift
        // (own-graph passes) never see the import; both notes coexist only in the
        // RESOLUTION graph, as distinct ids.
        let v = build_two_repo(&[
            ("type/note.type.yaml", "fields:\n  a: String\n"),
            ("n.md", "---\ntype: note::base\ntitle: x\n---\n"),
        ]);

        // Own graph: app's note only, carrying app's own field `a`, not base's.
        let own = v.graphs.of(&RepoName("app".into()));
        let note = own
            .get(&TypeName("note".into()))
            .expect("app owns its own note");
        assert!(
            note.fields.iter().any(|f| f.name.as_str() == "a"),
            "the own graph holds app's note, not the imported one"
        );

        // Resolution graph: both notes coexist, one name bound to two ids.
        let rg = app(&v).expect("app imports, so it has a resolution graph");
        assert_eq!(
            rg.name_conflicts()
                .get(&TypeName("note".into()))
                .map(|ids| ids.len()),
            Some(2),
            "own note and peer note::base are distinct nodes in the resolution graph"
        );
    }

    #[test]
    fn a_qualified_parent_builds_a_resolution_graph() {
        // app's own `card` extends `note::base`: the own-def qualified parent
        // triggers a resolution graph that folds note.
        let v = build_two_repo(&[(
            "type/card.type.yaml",
            "extends: note::base\nfields:\n  n: Number\n",
        )]);
        let rg = app(&v).expect("a repo with a qualified parent has a resolution graph");
        assert!(rg.iter().any(|(id, _)| id.name.as_str() == "note"));
        assert!(rg.iter().any(|(id, _)| id.name.as_str() == "card"));
    }

    /// Two repos that peer each other, with mutual `::repo` parents forming a
    /// cross-repo `type:` cycle: `a` (app) extends `b::base`, `b` (base) extends
    /// `a::app`. D3-crossrepo-cycle: this must fire `cycle-in-type-chain`, the
    /// cross-repo sibling of the single-repo cycle error. The RED probe asserts
    /// the current (undiagnosed) state so the fix flips it GREEN.
    fn build_mutual_cycle() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert(
            "/v/base/.arsumbris/repo.yaml",
            "name: base\ndeps:\n  - name: app\n",
        );
        fs.insert("/v/base/type/b.type.yaml", "extends: a::app\n");
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        fs.insert("/v/app/type/a.type.yaml", "extends: b::base\n");
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn a_cross_repo_parent_cycle_is_diagnosed() {
        let v = build_mutual_cycle();
        let cycle: Vec<String> = v
            .diagnostics()
            .filter(|d| d.code.as_str() == "cycle-in-type-chain")
            .map(|d| d.message.clone())
            .collect();
        assert!(
            !cycle.is_empty(),
            "a cross-repo `type:` parent cycle must fire cycle-in-type-chain, got: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
        // The message renders the authored form, so the peer member carries `::`.
        assert!(
            cycle.iter().any(|m| m.contains("::")),
            "the cycle message renders the peer member qualified: {cycle:?}"
        );
    }

    #[test]
    fn a_non_cyclic_cross_repo_workspace_has_no_cycle() {
        // app extends note::base (a clean cross-repo parent, no cycle).
        let v = build_two_repo(&[(
            "type/card.type.yaml",
            "extends: note::base\nfields:\n  n: Number\n",
        )]);
        assert!(
            !v.diagnostics()
                .any(|d| d.code.as_str() == "cycle-in-type-chain"),
            "a non-cyclic cross-repo graph must be silent: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_in_repo_cycle_is_not_double_fired_when_the_repo_imports() {
        // app has an OWN `type:` cycle (c -> d -> c) AND imports note::base, so it
        // gets a resolution graph that folds in the own cycle too. The own-graph
        // `check_cycles` owns the in-repo cycle; the cross-repo check must SKIP it
        // (all-own, no boundary crossing), so the count matches a repo with the
        // same own cycle and NO import.
        let own_cycle = &[
            ("type/c.type.yaml", "extends: d\n"),
            ("type/d.type.yaml", "extends: c\n"),
        ];
        let no_import = build_two_repo(own_cycle);
        let mut with_import: Vec<(&str, &str)> = own_cycle.to_vec();
        with_import.push(("n.md", "---\ntype: note::base\ntitle: x\n---\n"));
        let with_import = build_two_repo(&with_import);

        let count = |v: &KnowledgeBase| {
            v.diagnostics()
                .filter(|d| d.code.as_str() == "cycle-in-type-chain")
                .count()
        };
        assert!(
            count(&no_import) > 0,
            "the own cycle must fire at least once"
        );
        assert_eq!(
            count(&no_import),
            count(&with_import),
            "importing must not add a second cycle diagnostic for the same in-repo cycle"
        );
    }

    /// A cross-repo `type:` cycle with instances in each repo. Parity item: the
    /// cross-repo cycle must ABORT the owning repos' instance validation, like the
    /// single-repo `cycle-in-type-chain` graph-load abort, not merely emit the
    /// error and proceed. Each repo owns a cyclic member and imports (a `::repo`
    /// parent), so each aborts its own.
    fn build_mutual_cycle_with_instances() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert(
            "/v/base/.arsumbris/repo.yaml",
            "name: base\ndeps:\n  - name: app\n",
        );
        fs.insert(
            "/v/base/type/b.type.yaml",
            "extends: a::app\nfields:\n  bf: String\n",
        );
        fs.insert("/v/base/inst-b.md", "---\ntype: b\n---\n");
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        fs.insert(
            "/v/app/type/a.type.yaml",
            "extends: b::base\nfields:\n  af: String\n",
        );
        fs.insert("/v/app/inst-a.md", "---\ntype: a\n---\n");
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn a_cross_repo_type_cycle_aborts_instance_validation() {
        let v = build_mutual_cycle_with_instances();

        // The cycle error still fires (unchanged).
        assert!(
            v.diagnostics()
                .any(|d| d.code.as_str() == "cycle-in-type-chain"),
            "the cross-repo cycle is still reported"
        );

        // Both repos own a cyclic member, so both abort at the graph level,
        // matching the single-repo `cycle-in-type-chain` gate.
        assert_eq!(
            v.outcome_for_repo("app"),
            Some(crate::ir::BuildOutcome::AbortedAtGraph),
            "the app repo aborts on the cross-repo cycle"
        );
        assert_eq!(
            v.outcome_for_repo("base"),
            Some(crate::ir::BuildOutcome::AbortedAtGraph),
            "the base repo aborts on the cross-repo cycle"
        );

        // Instance validation is suppressed: no secondary `required-field-absent`
        // over the merged cyclic closure (the single-repo abort suppresses it too).
        assert!(
            !v.diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"),
            "aborted repos do not validate instances against the cyclic closure: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    /// `base` owns `note { title }` (required). `app` peers base, owns
    /// `outer { inner: note::base }`. An instance's `slot: outer` is supplied
    /// either in frontmatter or via a body typed-block; either way its `inner`
    /// is a nested inline record `type: note::base` with the required title
    /// OMITTED. Verifies whether the body form seeds/validates the nested peer
    /// claim like the frontmatter form (review 2.2).
    fn build_nested_peer_fixture(body_form: bool) -> KnowledgeBase {
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
        fs.insert(
            "/v/app/type/outer.type.yaml",
            "fields:\n  inner: note::base\n",
        );
        fs.insert("/v/app/type/holder.type.yaml", "fields:\n  slot: outer\n");
        if body_form {
            fs.insert(
                "/v/app/h.md",
                "---\ntype: holder\nslot:\n---\n\n```yaml [:slot]\ntype: outer\ninner:\n  type: note::base\n```\n",
            );
        } else {
            fs.insert(
                "/v/app/h.md",
                "---\ntype: holder\nslot:\n  type: outer\n  inner:\n    type: note::base\n---\n",
            );
        }
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn body_block_nested_peer_claim_validates_like_frontmatter() {
        let fires = |v: &KnowledgeBase| {
            v.diagnostics().any(|d| {
                let c = d.code.as_str();
                c == "required-field-absent" || c == "embedded-record-validation-failure"
            })
        };
        let front = build_nested_peer_fixture(false);
        let body = build_nested_peer_fixture(true);
        // Diagnose the baseline: whether the frontmatter form validates the nested
        // peer at all (it should, seeds recurse via collect_value_seeds).
        let front_fires = fires(&front);
        let body_fires = fires(&body);
        // Both forms validate the nested peer record's missing required field.
        // The body form must not silently under-validate relative to frontmatter.
        assert!(
            front_fires,
            "baseline: frontmatter must validate the nested peer: {:?}",
            front
                .diagnostics()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            body_fires,
            "the body-block form must validate the nested peer like frontmatter: {:?}",
            body.diagnostics()
                .map(|d| d.code.as_str())
                .collect::<Vec<_>>()
        );
    }
    fn build_self_repo_fixture() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert("/v/.arsumbris/workspace.yaml", "edit:\n  - v\n  - app\n");
        fs.insert("/v/app/.arsumbris/repo.yaml", "name: app\n");
        fs.insert("/v/app/type/note.type.yaml", "fields:\n  title: String\n");
        fs.insert("/v/app/n.md", "---\ntype: note::app\n---\n");
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn a_self_repo_qualifier_validates_against_the_own_type() {
        let v = build_self_repo_fixture();
        // `note::app` inside `app` is the own `note`; the missing required title
        // must fire, not fold to an empty shape with zero validation.
        assert!(
            v.diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"),
            "a self-qualified claim must validate against the own type: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
        // ...and carries the redundancy hint, not silence.
        assert!(
            v.diagnostics().any(|d| d.code.as_str() == "type-repo-self"),
            "a self-qualified claim carries the type-repo-self hint: {:?}",
            v.diagnostics().map(|d| d.code.as_str()).collect::<Vec<_>>()
        );
    }

    /// `base` owns note / tag / dm / layout; `app` peers base and imports on every
    /// fold-axis position (a claim, a parent, a meta, a body-use) plus references
    /// `tag::base` from a field-shape slot, which is NOT an import.
    fn build_imports_fixture() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        fs.insert("/v/base/type/note.type.yaml", "fields:\n  title: String\n");
        fs.insert("/v/base/type/tag.type.yaml", "fields:\n  label: String\n");
        fs.insert("/v/base/type/dm.type.yaml", "fields: {}\n");
        fs.insert(
            "/v/base/type/layout.type.yaml",
            "fields: {}\nbody:\n  - section: S\n",
        );
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        // claim import
        fs.insert("/v/app/n.md", "---\ntype: note::base\ntitle: x\n---\n");
        // parent import + meta import
        fs.insert(
            "/v/app/type/card.type.yaml",
            "extends: note::base\nfields:\n  n: Number\nmeta:\n  - type: dm::base\n",
        );
        // body-use import
        fs.insert(
            "/v/app/type/doc.type.yaml",
            "fields: {}\nbody:\n  - use: layout::base\n",
        );
        // field-shape REFERENCE, not an import
        fs.insert("/v/app/type/holder.type.yaml", "fields:\n  t: tag::base*\n");
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn collect_imports_gathers_fold_axis_and_excludes_field_shape_references() {
        let v = build_imports_fixture();
        let imports = collect_imports(&v.repos, &v.catalog);
        let app = RepoName("app".into());
        let has =
            |n: &str| imports.contains(&(app.clone(), TypeName(n.into()), "base".to_string()));
        // The fold-axis imports: claim + parent (note), meta (dm), body-use (layout).
        assert!(
            has("note"),
            "claim/parent note::base is an import: {imports:?}"
        );
        assert!(has("dm"), "meta dm::base is an import: {imports:?}");
        assert!(
            has("layout"),
            "body-use layout::base is an import: {imports:?}"
        );
        // The field-shape reference tag::base* is NOT an import.
        assert!(
            !has("tag"),
            "field-shape tag::base* is a reference, not an import: {imports:?}"
        );
        // app's stamped `repo.yaml` claims `au.engine.repo::au-engine`, so the
        // config-schema dependency surfaces as an import too — the builtin peer
        // is visible, not filtered (see the plan's visible-vs-prelude decision).
        assert!(
            imports.contains(&(
                app.clone(),
                TypeName("au.engine.repo".into()),
                "au-engine".to_string()
            )),
            "the stamped repo.yaml surfaces au.engine.repo::au-engine as an import: {imports:?}"
        );
        // Three user imports from base, note::base deduped across its two sites,
        // plus the one builtin au.engine.repo import: four total.
        assert_eq!(
            imports.iter().filter(|(r, _, _)| r == &app).count(),
            4,
            "{imports:?}"
        );
    }

    /// `base` owns `note { title }` with an own instance. `app` peers base, owns
    /// a DIVERGED own `note { headline }` (a distinct identity), imports
    /// `note::base` directly (an instance) and via a subtype (`card` extends it).
    fn build_instances_of_fixture() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        fs.insert("/v/base/type/note.type.yaml", "fields:\n  title: String\n");
        fs.insert("/v/base/bn.md", "---\ntype: note\ntitle: x\n---\n");
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        // app's OWN note, diverged fields → a distinct identity from base's note.
        fs.insert(
            "/v/app/type/note.type.yaml",
            "fields:\n  headline: String\n",
        );
        fs.insert(
            "/v/app/type/card.type.yaml",
            "extends: note::base\nfields:\n  n: Number\n",
        );
        fs.insert("/v/app/an.md", "---\ntype: note\nheadline: y\n---\n"); // app's own note
        fs.insert("/v/app/n.md", "---\ntype: note::base\ntitle: z\n---\n"); // imports peer note
        fs.insert("/v/app/c.md", "---\ntype: card\nn: 1\ntitle: w\n---\n"); // inherits peer note
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn instances_of_qualified_matches_one_identity_split_claimed_and_inherited() {
        let v = build_instances_of_fixture();
        let recs = crate::wire::introspect_instances_of(
            &v,
            "note::base",
            Some(&[crate::wire::Origin::File]),
        );
        let by = |p: &str| recs.iter().find(|r| r.path.ends_with(p));
        // base's note identity: bn.md (base own), n.md (import) are claimed; c.md
        // (card extends note::base) is inherited. app's own note (an.md) is a
        // DIFFERENT identity and is excluded.
        assert_eq!(
            recs.len(),
            3,
            "{:?}",
            recs.iter()
                .map(|r| (r.path.clone(), r.claimed, r.inherited))
                .collect::<Vec<_>>()
        );
        assert!(recs
            .iter()
            .all(|r| r.name == "note" && r.type_owners == vec!["base".to_string()]));
        assert!(by("bn.md").unwrap().claimed && !by("bn.md").unwrap().inherited);
        assert!(by("n.md").unwrap().claimed && !by("n.md").unwrap().inherited);
        let c = by("c.md").unwrap();
        assert!(
            !c.claimed && c.inherited,
            "card extends note::base → inherited"
        );
        assert!(
            by("an.md").is_none(),
            "app's own note is a different identity"
        );
    }

    #[test]
    fn instances_of_bare_matches_every_identity_named_note() {
        let v = build_instances_of_fixture();
        let recs =
            crate::wire::introspect_instances_of(&v, "note", Some(&[crate::wire::Origin::File]));
        // Both identities: base's note (bn / n / c) plus app's own note (an).
        assert_eq!(
            recs.len(),
            4,
            "{:?}",
            recs.iter()
                .map(|r| (r.path.clone(), r.type_owners.clone()))
                .collect::<Vec<_>>()
        );
        let an = recs.iter().find(|r| r.path.ends_with("an.md")).unwrap();
        assert_eq!(
            an.type_owners,
            vec!["app".to_string()],
            "app's own note is owned by app, a distinct identity"
        );
        assert!(an.claimed && !an.inherited);
    }

    #[test]
    fn list_imports_read_resolves_identities_and_excludes_references() {
        let v = build_imports_fixture();
        let imports = crate::wire::introspect_list_imports(&v, crate::wire::TypeScope::all());
        // Three USER imports (app → base), plus the config-schema import each
        // repo.yaml surfaces (`v`, `app`, `base` → au.engine.repo), plus the entry
        // `v`'s workspace.yaml (→ au.engine.workspace): seven total. The builtin
        // peer is VISIBLE, not filtered (the plan's decision).
        assert_eq!(
            imports.len(),
            7,
            "{:?}",
            imports
                .iter()
                .map(|i| (i.importer.clone(), i.name.clone(), i.owner.clone()))
                .collect::<Vec<_>>()
        );
        // The user imports: app → base, all with a resolved identity hash.
        let user: Vec<_> = imports.iter().filter(|i| i.owner == "base").collect();
        assert_eq!(user.len(), 3, "{user:?}");
        assert!(user
            .iter()
            .all(|i| i.importer == "app" && !i.hash.is_empty()));
        let names: Vec<&str> = user.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"note") && names.contains(&"dm") && names.contains(&"layout"));
        assert!(
            !names.contains(&"tag"),
            "the field-shape reference is excluded"
        );
        // The config-schema imports: every repo.yaml surfaces au.engine.repo (the
        // entry `v`, `app`, `base`), and the entry's workspace.yaml surfaces
        // au.engine.workspace.
        let repo_schema: Vec<_> = imports
            .iter()
            .filter(|i| i.name == "au.engine.repo")
            .collect();
        assert_eq!(
            repo_schema.len(),
            3,
            "the entry `v`, `app`, and `base` each import au.engine.repo"
        );
        assert!(repo_schema
            .iter()
            .all(|i| i.owner == "au-engine" && !i.hash.is_empty()));
        assert!(
            imports
                .iter()
                .any(|i| i.name == "au.engine.workspace" && i.importer == "v"),
            "the entry `v`'s workspace.yaml imports au.engine.workspace"
        );
    }
}

/// D3-bodyuse-splice: a `use: parent::repo` splices the peer type's body sections
/// across the boundary, and the folded-closure / no-body checks hold cross-repo.
#[cfg(test)]
mod bodyuse_tests {
    use crate::build::build;
    use crate::ir::KnowledgeBase;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// `base` (present, owns `base_files`) and `app` (peers base, owns `app_files`).
    fn build_repos(base_files: &[(&str, &str)], app_files: &[(&str, &str)]) -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        for (rel, c) in base_files {
            fs.insert(format!("/v/base/{rel}"), *c);
        }
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        for (rel, c) in app_files {
            fs.insert(format!("/v/app/{rel}"), *c);
        }
        build(Path::new("/v"), &fs).unwrap()
    }

    fn codes_on<'a>(v: &'a KnowledgeBase, abs: &str) -> Vec<&'a str> {
        v.diagnostics()
            .filter(|d| d.span.file == Path::new(abs))
            .map(|d| d.code.as_str())
            .collect()
    }

    // base owns `note { title }` with a body section `Detail`; app's `card` extends
    // AND `use:`s note::base, so an app instance of card must carry `# Detail`.
    const NOTE_WITH_BODY: &str = "fields:\n  title: String\nbody:\n  - section: Detail\n";
    const CARD_USES_NOTE: &str = "extends: note::base\nbody:\n  - use: note::base\n";

    #[test]
    fn a_direct_peer_claim_validates_the_peer_body_sections() {
        // The instance's OWN claim IS the peer type `note::base`, which carries a
        // body section — no own wrapper type, no explicit `use:`. The peer body
        // template must still be enforced (the hole was build_effective_template
        // splicing from the own graph with the bare name, missing a `::repo`
        // claim). Omitting `# Detail` fires body-section-missing.
        let v = build_repos(
            &[("type/note.type.yaml", NOTE_WITH_BODY)],
            &[("n.md", "---\ntype: note::base\ntitle: hi\n---\n")],
        );
        assert!(
            codes_on(&v, "/v/app/n.md").contains(&"body-section-missing"),
            "a direct peer claim must enforce the peer's body template: {:?}",
            codes_on(&v, "/v/app/n.md")
        );
    }

    #[test]
    fn a_direct_peer_claim_is_clean_when_the_peer_section_is_present() {
        let v = build_repos(
            &[("type/note.type.yaml", NOTE_WITH_BODY)],
            &[(
                "n.md",
                "---\ntype: note::base\ntitle: hi\n---\n\n# Detail\n\nprose\n",
            )],
        );
        assert!(
            !codes_on(&v, "/v/app/n.md").contains(&"body-section-missing"),
            "the present peer section satisfies the template: {:?}",
            codes_on(&v, "/v/app/n.md")
        );
    }

    #[test]
    fn peer_body_resolver_skips_a_graph_aborted_peer() {
        // base's graph aborts on a cycle. The served body-splice seam must skip it,
        // like the validation seam, so a served view does not splice sections from a
        // peer whose graph validation deliberately left unspliced (finding 3.5).
        use au_core::CrossRepoResolver;
        let v = build_repos(
            &[
                ("type/x.type.yaml", "extends: y\nfields: {}\n"),
                ("type/y.type.yaml", "extends: x\nfields: {}\n"),
                (
                    "type/note.type.yaml",
                    "fields: {}\nbody:\n  - section: Detail\n",
                ),
            ],
            &[],
        );
        let peer_body = crate::crossref::PeerBodyResolver {
            repos: &v.repos,
            graphs: &v.graphs,
            outcomes: &v.outcomes,
        };
        assert!(
            peer_body.peer_graph("base").is_none(),
            "a graph-aborted peer's graph must be skipped by the body-splice seam"
        );
    }

    #[test]
    fn a_cross_repo_use_splices_the_peer_body_sections() {
        // The instance omits `# Detail`; after the splice its type demands it.
        let v = build_repos(
            &[("type/note.type.yaml", NOTE_WITH_BODY)],
            &[
                ("type/card.type.yaml", CARD_USES_NOTE),
                ("c.md", "---\ntype: card\ntitle: hi\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "/v/app/c.md").contains(&"body-section-missing"),
            "a spliced peer section must be required: {:?}",
            codes_on(&v, "/v/app/c.md")
        );
    }

    #[test]
    fn a_cross_repo_use_is_clean_when_the_spliced_section_is_present() {
        let v = build_repos(
            &[("type/note.type.yaml", NOTE_WITH_BODY)],
            &[
                ("type/card.type.yaml", CARD_USES_NOTE),
                (
                    "c.md",
                    "---\ntype: card\ntitle: hi\n---\n\n# Detail\n\nprose\n",
                ),
            ],
        );
        assert!(
            !codes_on(&v, "/v/app/c.md").contains(&"body-section-missing"),
            "the present spliced section must satisfy the template: {:?}",
            codes_on(&v, "/v/app/c.md")
        );
    }

    #[test]
    fn a_body_use_not_in_the_folded_closure_is_out_of_closure() {
        // `loose` uses note::base but does NOT extend it, so the peer body is not
        // in its folded closure — the cross-repo sibling of body-use-out-of-closure.
        let v = build_repos(
            &[("type/note.type.yaml", NOTE_WITH_BODY)],
            &[("type/loose.type.yaml", "body:\n  - use: note::base\n")],
        );
        assert!(
            codes_on(&v, "/v/app/type/loose.type.yaml").contains(&"body-use-out-of-closure"),
            "a use of a peer not in the folded closure must fire: {:?}",
            codes_on(&v, "/v/app/type/loose.type.yaml")
        );
    }

    #[test]
    fn a_cross_repo_use_of_a_bodiless_peer_warns() {
        // base's `plain` has no body; extending + using it splices nothing.
        let v = build_repos(
            &[("type/plain.type.yaml", "fields:\n  a: String\n")],
            &[(
                "type/wrap.type.yaml",
                "extends: plain::base\nbody:\n  - use: plain::base\n",
            )],
        );
        assert!(
            codes_on(&v, "/v/app/type/wrap.type.yaml").contains(&"body-use-target-has-no-body"),
            "a use of a bodiless peer must warn: {:?}",
            codes_on(&v, "/v/app/type/wrap.type.yaml")
        );
    }
}

/// D5, the candidate scan stays scoped to the own graph, it never leaks a peer's
/// vocabulary even when peer types are folded into the resolution graph.
#[cfg(test)]
mod candidate_scope_tests {
    use crate::build::build;
    use crate::RepoName;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    #[test]
    fn the_candidate_scan_stays_scoped_to_the_own_graph() {
        // base owns `note { title }`. app owns `titled { title }` and `card { n }`,
        // and IMPORTS note::base (imp.md), so `note` IS folded into app's
        // resolution graph. cand.md claims `card` and carries `title`, which
        // satisfies BOTH the own `titled` and the peer `note::base`. The candidate
        // scan must suggest the OWN `titled` (proving it runs) and must NOT suggest
        // the peer `note` (proving it stays own-graph-scoped, never the peer vocab).
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
        fs.insert("/v/app/type/titled.type.yaml", "fields:\n  title: String\n");
        fs.insert("/v/app/type/card.type.yaml", "fields:\n  n: Number\n");
        fs.insert("/v/app/imp.md", "---\ntype: note::base\ntitle: x\n---\n");
        fs.insert("/v/app/cand.md", "---\ntype: card\nn: 1\ntitle: x\n---\n");
        let v = build(Path::new("/v"), &fs).unwrap();

        // Precondition: `note` is folded into app's resolution graph (the import is real).
        let rg = v.resolution_graphs.of(&RepoName("app".into()));
        assert!(
            rg.is_some_and(|rg| rg.iter().any(|(id, _)| id.name.as_str() == "note")),
            "the peer note must be folded, so the scoping is meaningful"
        );

        // Candidates are computed on demand from the file's own graph, so this
        // also proves the lazy read stays own-graph-scoped.
        let names: Vec<String> =
            crate::wire::candidates_for(&v, &std::path::PathBuf::from("/v/app/cand.md"))
                .into_iter()
                .map(|c| c.type_name.0)
                .collect();
        assert!(
            names.iter().any(|n| n == "titled"),
            "the own same-shaped type is suggested (the scan runs): {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "note"),
            "the peer type must NOT be suggested (own-graph scoped, no peer-vocab leak): {names:?}"
        );
    }
}

/// D3b, a qualified DEMANDED type (`foo::repo*`) validating a reference value,
/// the fold-versus-demand rule's demand half
/// ([[example - cross-repo type fold versus field demand, a worked verification trace]]).
#[cfg(test)]
mod qualified_demand_tests {
    use crate::build::build;
    use crate::ir::KnowledgeBase;
    use au_parser::MemoryFileSystem;
    use std::path::{Path, PathBuf};

    /// A two-repo workspace: `base` (present, owns the given files) and `app`
    /// (peers base, owns the given files). `app` demands `base`'s types via
    /// `::repo` field shapes.
    fn build_repos(base_files: &[(&str, &str)], app_files: &[(&str, &str)]) -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        for (rel, c) in base_files {
            fs.insert(format!("/v/base/{rel}"), *c);
        }
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        for (rel, c) in app_files {
            fs.insert(format!("/v/app/{rel}"), *c);
        }
        build(Path::new("/v"), &fs).unwrap()
    }

    /// Reference-target-type-mismatch diagnostics on `/v/app/<rel>`.
    fn mismatches_on(v: &KnowledgeBase, rel: &str) -> Vec<String> {
        let path = PathBuf::from(format!("/v/app/{rel}"));
        v.diagnostics()
            .filter(|d| d.code.as_str() == "reference-target-type-mismatch" && d.span.file == path)
            .map(|d| d.message.clone())
            .collect()
    }

    /// All diagnostic codes on `/v/app/<rel>`.
    fn codes_on(v: &KnowledgeBase, rel: &str) -> Vec<String> {
        let path = PathBuf::from(format!("/v/app/{rel}"));
        v.diagnostics()
            .filter(|d| d.span.file == path)
            .map(|d| d.code.as_str().to_string())
            .collect()
    }

    const THING: (&str, &str) = ("type/thing.type.yaml", "fields:\n  t: String\n");

    // Finding 2.3: multi-leaf-in-sealed-family across the boundary.
    const K_FAMILY: &[(&str, &str)] = &[
        ("type/k.type.yaml", "sealed:\n  - k.a\n  - k.b\n"),
        ("type/k.a.type.yaml", "extends: k\nfields: {}\n"),
        ("type/k.b.type.yaml", "extends: k\nfields: {}\n"),
    ];

    #[test]
    fn multi_leaf_of_a_peer_sealed_family_fires() {
        // Two leaves of base's sealed family `k`, claimed cross-repo. Single-repo
        // this is an Error; it was silently skipped for `::repo` claims.
        let v = build_repos(
            K_FAMILY,
            &[("n.md", "---\ntype:\n  - k.a::base\n  - k.b::base\n---\n")],
        );
        assert!(
            codes_on(&v, "n.md").contains(&"multi-leaf-in-sealed-family".to_string()),
            "two leaves of a peer sealed family must fire: {:?}",
            codes_on(&v, "n.md")
        );
    }

    #[test]
    fn a_single_peer_sealed_leaf_is_clean() {
        let v = build_repos(K_FAMILY, &[("n.md", "---\ntype: k.a::base\n---\n")]);
        assert!(
            !codes_on(&v, "n.md").contains(&"multi-leaf-in-sealed-family".to_string()),
            "a single peer leaf is a valid claim: {:?}",
            codes_on(&v, "n.md")
        );
    }

    #[test]
    fn two_own_sealed_leaves_still_fire_when_the_repo_imports() {
        // app imports (the `trigger` instance claims a peer type), so validation
        // runs the FOLDED multi-leaf check; two OWN sealed leaves must still fire,
        // at parity with the single-repo path the fold subsumes.
        let v = build_repos(
            &[("type/thing.type.yaml", "fields:\n  t: String\n")],
            &[
                ("type/m.type.yaml", "sealed:\n  - m.a\n  - m.b\n"),
                ("type/m.a.type.yaml", "extends: m\nfields: {}\n"),
                ("type/m.b.type.yaml", "extends: m\nfields: {}\n"),
                ("trigger.md", "---\ntype: thing::base\nt: x\n---\n"),
                ("own-multi.md", "---\ntype:\n  - m.a\n  - m.b\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "own-multi.md").contains(&"multi-leaf-in-sealed-family".to_string()),
            "two own leaves fire under the folded path too: {:?}",
            codes_on(&v, "own-multi.md")
        );
    }

    #[test]
    fn a_peer_sealed_parent_claimed_on_an_inline_value_fires() {
        // An inline value that claims a peer's SEALED parent directly must fire
        // `sealed-parent-claimed`, at parity with the file-level, qualified-inline-
        // demand, and meta checks. The slot is a UNION (`<box | k::base>`), so the
        // qualified-inline-demand early-return does not intercept (that path only
        // fires for a `SingleDemand` with a `::repo` demand); the inline value's own
        // `::repo` claim was the silently-skipped position. The `k::base` branch is
        // satisfied by the claim's folded closure, so the compat check passes and
        // the sealed check downstream is actually reached.
        let v = build_repos(
            K_FAMILY,
            &[
                ("type/box.type.yaml", "fields:\n  b: String\n"),
                ("type/host.type.yaml", "fields:\n  x: <box | k::base>\n"),
                ("inst.md", "---\ntype: host\nx:\n  type: k::base\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "inst.md").contains(&"sealed-parent-claimed".to_string()),
            "a peer sealed parent claimed on an inline value must fire: {:?}",
            codes_on(&v, "inst.md")
        );
    }

    #[test]
    fn a_peer_sealed_leaf_on_an_inline_value_is_clean() {
        // The parity sibling: a non-sealed peer LEAF (`k.a::base`) claimed on the
        // same inline slot is a valid drill-down and must NOT fire.
        let v = build_repos(
            K_FAMILY,
            &[
                ("type/box.type.yaml", "fields:\n  b: String\n"),
                ("type/host.type.yaml", "fields:\n  x: <box | k::base>\n"),
                ("inst.md", "---\ntype: host\nx:\n  type: k.a::base\n---\n"),
            ],
        );
        assert!(
            !codes_on(&v, "inst.md").contains(&"sealed-parent-claimed".to_string()),
            "a peer non-sealed leaf on an inline value is a valid claim: {:?}",
            codes_on(&v, "inst.md")
        );
    }

    // Finding 3.6: subsumption-in-mixin across the fold. `child` extends `parent`.
    const PARENT_CHILD: &[(&str, &str)] = &[
        ("type/parent.type.yaml", "fields: {}\n"),
        ("type/child.type.yaml", "extends: parent\nfields: {}\n"),
    ];

    #[test]
    fn subsumption_of_a_peer_ancestor_fires() {
        // `child::base` extends `parent::base`, so `parent::base` is redundant in
        // the mixin. Single-repo `type: [child, parent]` warns; the `::repo` form
        // was silently skipped (the subsumption walk dropped qualified claims).
        let v = build_repos(
            PARENT_CHILD,
            &[(
                "n.md",
                "---\ntype:\n  - child::base\n  - parent::base\n---\n",
            )],
        );
        assert!(
            codes_on(&v, "n.md").contains(&"subsumption-in-mixin".to_string()),
            "a redundant peer ancestor claim must warn: {:?}",
            codes_on(&v, "n.md")
        );
    }

    #[test]
    fn a_non_subsuming_peer_pair_is_clean() {
        // Two unrelated peer types — no ancestry, no redundancy warning.
        let v = build_repos(
            &[
                ("type/parent.type.yaml", "fields: {}\n"),
                ("type/other.type.yaml", "fields: {}\n"),
            ],
            &[(
                "n.md",
                "---\ntype:\n  - parent::base\n  - other::base\n---\n",
            )],
        );
        assert!(
            !codes_on(&v, "n.md").contains(&"subsumption-in-mixin".to_string()),
            "two unrelated peer claims are not redundant: {:?}",
            codes_on(&v, "n.md")
        );
    }

    #[test]
    fn two_own_claims_still_subsume_when_the_repo_imports() {
        // app imports (the `trigger` instance claims a peer type), so validation
        // runs the FOLDED subsumption check; two OWN claims where one extends the
        // other must still warn, at parity with the single-repo path the fold
        // subsumes.
        let v = build_repos(
            &[("type/thing.type.yaml", "fields:\n  t: String\n")],
            &[
                ("type/pp.type.yaml", "fields: {}\n"),
                ("type/cc.type.yaml", "extends: pp\nfields: {}\n"),
                ("trigger.md", "---\ntype: thing::base\nt: x\n---\n"),
                ("own.md", "---\ntype:\n  - cc\n  - pp\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "own.md").contains(&"subsumption-in-mixin".to_string()),
            "own subsumption still fires under the folded path: {:?}",
            codes_on(&v, "own.md")
        );
    }

    #[test]
    fn a_same_name_diamond_with_diverged_fields_is_a_mixin_collision() {
        // app owns `note` (title: String) AND imports base's `note` (title: Number).
        // An instance mixing `[note, note::base]` reaches two DISTINCT identities of
        // one name; the field-origin map keyed by bare name silently picked one
        // (finding 1.2). Now it surfaces as a `mixin-collision`, and the message
        // names the two identities in authored form (`note` vs `note::base`), not a
        // confusing `note` vs `note`.
        // A BARE use of the divergent `title` is the collision (an untouched
        // divergent field would be clean); qualifying every use resolves it.
        let v = build_repos(
            &[("type/note.type.yaml", "fields:\n  title: Number\n")],
            &[
                ("type/note.type.yaml", "fields:\n  title: String\n"),
                (
                    "n.md",
                    "---\ntype:\n  - note\n  - note::base\ntitle: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "n.md").contains(&"mixin-collision".to_string()),
            "a bare use of a diverged same-name diamond must fire mixin-collision: {:?}",
            codes_on(&v, "n.md")
        );
        let path = PathBuf::from("/v/app/n.md");
        let msg = v
            .diagnostics()
            .find(|d| d.code.as_str() == "mixin-collision" && d.span.file == path)
            .map(|d| d.message.clone())
            .unwrap_or_default();
        assert!(
            msg.contains("note::base"),
            "the message must name the peer identity in authored form: {msg}"
        );
    }

    #[test]
    fn a_divergent_field_keeps_a_third_non_diverging_origin() {
        // The diamond (`note` String vs `note::base` Number) plus a THIRD origin
        // of the same field from a differently-named type (`deliverable.title`,
        // required). The third origin must survive into the divergent field so its
        // `required-field-absent` still fires — the origin-drop the review caught.
        let v = build_repos(
            &[("type/note.type.yaml", "fields:\n  title: Number\n")],
            &[
                ("type/note.type.yaml", "fields:\n  title: String\n"),
                ("type/deliverable.type.yaml", "fields:\n  title: String\n"),
                (
                    "n.md",
                    "---\ntype:\n  - note\n  - note::base\n  - deliverable\ntitle{note}: x\ntitle{note::base}: 42\n---\n",
                ),
            ],
        );
        let path = PathBuf::from("/v/app/n.md");
        let missing: Vec<String> = v
            .diagnostics()
            .filter(|d| d.span.file == path && d.code.as_str() == "required-field-absent")
            .map(|d| d.message.clone())
            .collect();
        assert!(
            missing.iter().any(|m| m.contains("deliverable")),
            "the third origin (`deliverable`) must still be required, not dropped: {missing:?}"
        );
    }

    #[test]
    fn a_same_name_pair_agreeing_on_the_shared_field_is_clean() {
        // Two distinct identities of `note` (base adds `extra`, so the ids differ)
        // but the SHARED field `title` is token-equal, so it auto-unifies and no
        // collision fires — only a genuine field conflict is an error.
        let v = build_repos(
            &[(
                "type/note.type.yaml",
                "fields:\n  title: String\n  extra: Bool\n",
            )],
            &[
                ("type/note.type.yaml", "fields:\n  title: String\n"),
                (
                    "n.md",
                    "---\ntype:\n  - note\n  - note::base\ntitle: t\nextra: true\n---\n",
                ),
            ],
        );
        assert!(
            !codes_on(&v, "n.md").contains(&"mixin-collision".to_string()),
            "a same-name pair agreeing on the shared field is clean: {:?}",
            codes_on(&v, "n.md")
        );
    }

    #[test]
    fn a_cross_name_conflict_under_the_fold_names_the_peer_origin_qualified() {
        // Two DIFFERENTLY-named types (`x` own, `y::base` peer) collide on field `f`.
        // It is a mixin-collision as always; the improvement is that the peer origin
        // renders qualified (`y::base`, not bare `y`) so the message is navigable.
        let v = build_repos(
            &[("type/y.type.yaml", "fields:\n  f: Number\n")],
            &[
                ("type/x.type.yaml", "fields:\n  f: String\n"),
                ("n.md", "---\ntype:\n  - x\n  - y::base\nf: bare\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "n.md").contains(&"mixin-collision".to_string()),
            "a bare use of a cross-name field conflict is a mixin-collision: {:?}",
            codes_on(&v, "n.md")
        );
        let path = PathBuf::from("/v/app/n.md");
        let msg = v
            .diagnostics()
            .find(|d| d.code.as_str() == "mixin-collision" && d.span.file == path)
            .map(|d| d.message.clone())
            .unwrap_or_default();
        assert!(
            msg.contains("y::base"),
            "the peer origin must render qualified in the message: {msg}"
        );
    }

    #[test]
    fn a_deep_cross_repo_type_chain_hits_the_depth_bound() {
        // base b1..b130 (own chain), app a1..a130 with a130 extending b1::base.
        // The FOLDED chain from a1 is ~260 deep, crossing the boundary once;
        // neither repo's own chain reaches 256, so only the fold catches it
        // (finding 3.7). The fold terminates safely regardless; this is the
        // missing advisory at parity with the single-repo depth check.
        let n = 130;
        let mut base_files: Vec<(String, String)> = Vec::new();
        for i in 1..=n {
            let content = if i < n {
                format!("extends: b{}\nfields: {{}}\n", i + 1)
            } else {
                "fields: {}\n".to_string()
            };
            base_files.push((format!("type/b{i}.type.yaml"), content));
        }
        let mut app_files: Vec<(String, String)> = Vec::new();
        for i in 1..=n {
            let content = if i < n {
                format!("extends: a{}\nfields: {{}}\n", i + 1)
            } else {
                "extends: b1::base\nfields: {}\n".to_string()
            };
            app_files.push((format!("type/a{i}.type.yaml"), content));
        }
        let base_refs: Vec<(&str, &str)> = base_files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let app_refs: Vec<(&str, &str)> = app_files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let v = build_repos(&base_refs, &app_refs);
        assert!(
            v.diagnostics()
                .any(|d| d.code.as_str() == "type-chain-depth-exceeded"),
            "a ~260-deep folded cross-repo chain must hit the depth bound"
        );
    }

    // Finding 3.4a: an own meta and a same-named PEER meta are distinct types.
    #[test]
    fn an_own_and_a_peer_meta_of_the_same_name_are_not_a_duplicate() {
        let v = build_repos(
            &[("type/dm.type.yaml", "fields: {}\n")],
            &[
                ("type/dm.type.yaml", "fields: {}\n"),
                (
                    "type/card.type.yaml",
                    "meta:\n  - type: dm\n  - type: dm::base\n",
                ),
            ],
        );
        assert!(
            !codes_on(&v, "type/card.type.yaml").contains(&"duplicate-meta-block".to_string()),
            "own dm and peer dm::base are distinct, not a duplicate: {:?}",
            codes_on(&v, "type/card.type.yaml")
        );
    }

    #[test]
    fn two_own_metas_of_the_same_name_still_duplicate() {
        let v = build_repos(
            &[],
            &[
                ("type/dm.type.yaml", "fields: {}\n"),
                ("type/card.type.yaml", "meta:\n  - type: dm\n  - type: dm\n"),
            ],
        );
        assert!(
            codes_on(&v, "type/card.type.yaml").contains(&"duplicate-meta-block".to_string()),
            "two own dm blocks are a duplicate: {:?}",
            codes_on(&v, "type/card.type.yaml")
        );
    }

    // Finding 2.4: a `::repo` field qualifier resolves a cross-repo mixin collision.
    // own `note.summary: String` collides with peer `deliverable::base.summary: Number`.
    const COLLIDING_DELIVERABLE: &[(&str, &str)] =
        &[("type/deliverable.type.yaml", "fields:\n  summary: Number\n")];
    const OWN_NOTE: (&str, &str) = ("type/note.type.yaml", "fields:\n  summary: String\n");

    #[test]
    fn a_qualified_brace_prefix_resolves_a_cross_repo_collision() {
        let v = build_repos(
            COLLIDING_DELIVERABLE,
            &[
                OWN_NOTE,
                (
                    "n.md",
                    "---\ntype:\n  - note\n  - deliverable::base\nsummary{note}: hi\nsummary{deliverable::base}: 42\n---\n",
                ),
            ],
        );
        let codes = codes_on(&v, "n.md");
        assert!(
            !codes.contains(&"malformed-qualifier-key".to_string()),
            "the `field{{type::repo}}` qualifier form must parse, not malform: {codes:?}"
        );
        assert!(
            !codes.contains(&"qualifier-does-not-declare-field".to_string()),
            "a qualified peer qualifier must resolve, not misfire: {codes:?}"
        );
        assert!(
            !codes.contains(&"field-shape-mismatch".to_string()),
            "correct values at both qualifiers validate: {codes:?}"
        );
    }

    #[test]
    fn a_qualified_brace_prefix_validates_the_value_shape() {
        // A wrong-typed value at the qualified field must fire, proving the value
        // is validated (before the fix it misfired and dropped validation).
        let v = build_repos(
            COLLIDING_DELIVERABLE,
            &[
                OWN_NOTE,
                (
                    "n.md",
                    "---\ntype:\n  - note\n  - deliverable::base\nsummary{note}: hi\nsummary{deliverable::base}: not-a-number\n---\n",
                ),
            ],
        );
        let codes = codes_on(&v, "n.md");
        assert!(
            codes.contains(&"field-shape-mismatch".to_string()),
            "a non-Number at summary{{deliverable::base}} must mismatch: {codes:?}"
        );
    }

    #[test]
    fn case1_unqualified_value_to_a_local_target_claiming_the_peer_type() {
        // The worked example's case 1: the target lives in app and CLAIMS the peer
        // type (so app folded `thing`), the value is unqualified. Membership reads
        // app's FOLDED closure; a plain own-graph walk would miss it.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("t.md", "---\ntype: thing::base\nt: x\n---\n"),
                ("h.md", "---\ntype: holder\nref: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "h.md").is_empty(),
            "a local target claiming the peer type satisfies `thing::base*`: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn case1_negative_own_target_does_not_satisfy_the_peer_demand() {
        // The target claims app's OWN `other`, not the peer type. app imports
        // nothing (the demand is a field shape, never a seed), so this exercises
        // the own-graph fallback for the target's folded ids. It must mismatch,
        // and the message names the authored `thing::base`.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("type/other.type.yaml", "fields:\n  o: String\n"),
                ("t.md", "---\ntype: other\no: y\n---\n"),
                ("h.md", "---\ntype: holder\nref: \"[[t]]\"\n---\n"),
            ],
        );
        let ms = mismatches_on(&v, "h.md");
        assert_eq!(ms.len(), 1, "an own target must not satisfy a peer demand");
        assert!(
            ms[0].contains("thing::base"),
            "the message shows the authored `thing::base`: {}",
            ms[0]
        );
    }

    #[test]
    fn case2_qualified_value_to_a_target_in_the_peer_repo() {
        // The worked example's case 2: the value is `::base`-qualified, the target
        // lives in base and claims bare `thing` (its own). Membership reads base's
        // own-graph closure (base imports nothing). Same demanded and target id as
        // case 1, reached through the other repo.
        let v = build_repos(
            &[THING, ("bt.md", "---\ntype: thing\nt: z\n---\n")],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("h.md", "---\ntype: holder\nref: \"[[bt::base]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "h.md").is_empty(),
            "a peer-repo target claiming its own `thing` satisfies `thing::base*`: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_peer_demand_inherits_the_targets_parent_closure() {
        // base's `thing` extends `root`; app demands `root::base*`. A value whose
        // target claims `thing::base` satisfies it, because the folded closure
        // crosses the parent edge to `root`.
        let v = build_repos(
            &[
                ("type/root.type.yaml", "fields:\n  r: String\n"),
                (
                    "type/thing.type.yaml",
                    "extends: root\nfields:\n  t: String\n",
                ),
            ],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: root::base*\n"),
                ("t.md", "---\ntype: thing::base\nr: a\nt: x\n---\n"),
                ("h.md", "---\ntype: holder\nref: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "h.md").is_empty(),
            "a target claiming `thing::base` satisfies `root::base*` via the parent fold: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_list_of_peer_demands_checks_each_element() {
        // `refs: thing::base*[]`: a good element (target claims thing::base) and a
        // bad one (target claims own `other`). Exactly one mismatch, on the bad
        // element.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  refs: thing::base*[]\n"),
                ("type/other.type.yaml", "fields:\n  o: String\n"),
                ("t.md", "---\ntype: thing::base\nt: x\n---\n"),
                ("o.md", "---\ntype: other\no: y\n---\n"),
                (
                    "h.md",
                    "---\ntype: holder\nrefs:\n  - \"[[t]]\"\n  - \"[[o]]\"\n---\n",
                ),
            ],
        );
        let ms = mismatches_on(&v, "h.md");
        assert_eq!(ms.len(), 1, "only the `other` element mismatches: {ms:?}");
    }

    #[test]
    fn a_compound_demand_is_satisfied_by_either_branch() {
        // `k: <thing::base | localx>*`, a Union of a peer and an own branch. A
        // value claiming the peer type satisfies the peer branch; a value claiming
        // the own type satisfies the own branch; a value claiming neither
        // mismatches.
        let v = build_repos(
            &[THING],
            &[
                ("type/localx.type.yaml", "fields:\n  x: String\n"),
                ("type/other.type.yaml", "fields:\n  o: String\n"),
                (
                    "type/holder.type.yaml",
                    "fields:\n  k: <thing::base | localx>*\n",
                ),
                ("t.md", "---\ntype: thing::base\nt: x\n---\n"),
                ("lx.md", "---\ntype: localx\nx: y\n---\n"),
                ("u.md", "---\ntype: other\no: z\n---\n"),
                ("peer.md", "---\ntype: holder\nk: \"[[t]]\"\n---\n"),
                ("own.md", "---\ntype: holder\nk: \"[[lx]]\"\n---\n"),
                ("bad.md", "---\ntype: holder\nk: \"[[u]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "peer.md").is_empty(),
            "the peer branch is satisfied: {:?}",
            mismatches_on(&v, "peer.md")
        );
        assert!(
            mismatches_on(&v, "own.md").is_empty(),
            "the own branch is satisfied: {:?}",
            mismatches_on(&v, "own.md")
        );
        assert_eq!(
            mismatches_on(&v, "bad.md").len(),
            1,
            "neither branch is satisfied"
        );
    }

    #[test]
    fn an_inline_record_block_id_claiming_the_peer_type_satisfies_the_demand() {
        // A `^block-id` target's satisfaction is its BLOCK's own claim, folded over
        // the target repo's resolution graph. Here `t.md` carries an addressable
        // inline record (`^: blk`) claiming `thing::base` (an inline-record claim
        // is a fold seed, so app folds `thing`), referenced via `[[t^^blk]]` against
        // `thing::base*`. The block's claim, not the file's, drives the check.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("type/thost.type.yaml", "fields: {}\n"),
                (
                    "t.md",
                    "---\ntype: thost\nstuff:\n  ^: blk\n  type: thing::base\n  t: x\n---\n",
                ),
                ("h.md", "---\ntype: holder\nref: \"[[t^^blk]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "h.md").is_empty(),
            "the block's own `thing::base` claim satisfies `thing::base*`: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_inline_record_block_id_claiming_an_own_type_mismatches() {
        // The block claims app's OWN `other`, not the peer type, so the demand
        // `thing::base*` is not satisfied and the mismatch fires — the block claim
        // is genuinely checked, not skipped.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("type/thost.type.yaml", "fields: {}\n"),
                ("type/other.type.yaml", "fields:\n  o: String\n"),
                (
                    "t.md",
                    "---\ntype: thost\nstuff:\n  ^: blk\n  type: other\n  o: y\n---\n",
                ),
                ("h.md", "---\ntype: holder\nref: \"[[t^^blk]]\"\n---\n"),
            ],
        );
        assert_eq!(
            mismatches_on(&v, "h.md").len(),
            1,
            "the block's own claim does not satisfy the peer demand"
        );
    }

    // ----- D3b-ii, the inline-record demand delegation (`foo::repo`, `foo::repo&`
    // inline branch): the inline map validates against the OWNER repo's shape. -----

    const REC: (&str, &str) = ("type/rec.type.yaml", "fields:\n  r: String\n  n: Number\n");

    #[test]
    fn an_inline_value_at_a_qualified_amp_slot_validates_against_the_owner_shape() {
        // `m: rec::base&` with an inline map that supplies base's `rec` fields is
        // clean — the contract comes from the OWNER (base), not app (which has no
        // `rec`).
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                ("h.md", "---\ntype: holder\nm:\n  r: hi\n  n: 3\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "a satisfying inline value at a peer-typed slot is clean: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_inline_value_missing_an_owner_required_field_fires_required_absent() {
        // The inline map omits base's required `n`, so the owner's contract fires
        // required-field-absent, proving the delegation enforces the peer fields.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                ("h.md", "---\ntype: holder\nm:\n  r: hi\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "the owner's required `n` must fire: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_inline_value_violating_an_owner_field_shape_fires_mismatch() {
        // `n` is a Number in base's `rec`; a string value fails the owner's shape.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  r: hi\n  n: notanumber\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"field-shape-mismatch".to_string()),
            "the owner's `n: Number` shape must reject a string: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_bare_record_slot_also_delegates_to_the_owner() {
        // The `Record` form `m: rec::base` (no `&`) delegates the same way as the
        // `&` inline branch.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base\n"),
                ("h.md", "---\ntype: holder\nm:\n  r: hi\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "the bare record slot enforces the owner's required `n`: {:?}",
            codes_on(&v, "h.md")
        );
    }

    // base owns `rec` and a subtype `recSub` extending it with an own required
    // field, for the explicit-inline-claim membership cases.
    const REC_SUB: (&str, &str) = (
        "type/recSub.type.yaml",
        "extends: rec\nfields:\n  s: String\n",
    );

    #[test]
    fn an_explicit_inline_claim_that_is_a_peer_subtype_validates_against_it() {
        // The inline declares `type: recSub::base`, a peer subtype of the demanded
        // `rec::base`. Its folded closure includes the demanded peer id, so it
        // satisfies the slot AND its own required fields are enforced: omitting
        // `recSub`'s `s` (and `rec`'s `n`) fires required-field-absent, no longer
        // skipped.
        let v = build_repos(
            &[REC, REC_SUB],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recSub::base\n  r: hi\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "the peer subtype's required fields are enforced, not skipped: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_explicit_inline_claim_that_is_a_peer_subtype_is_clean_when_satisfied() {
        // The same peer subtype, with every folded field supplied, validates clean.
        let v = build_repos(
            &[REC, REC_SUB],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recSub::base\n  r: hi\n  n: 3\n  s: y\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "a fully-supplied peer subtype inline value is clean: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_explicit_inline_claim_that_does_not_reach_the_peer_is_not_compatible() {
        // The inline declares app's own `local`, which does not reach the demanded
        // peer type — its closure excludes it, so `inline-value-type-not-compatible`
        // fires. Formerly this was silently skipped, an unvalidated typed slot.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                ("type/local.type.yaml", "fields:\n  a: String\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: local\n  a: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-type-not-compatible".to_string()),
            "a claim that does not reach the peer demand fires not-compatible: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_explicit_inline_claim_that_is_a_source_subtype_of_the_peer_validates() {
        // The inline declares app's own `mine`, which EXTENDS the peer type
        // (`type: rec::base`). Its qualified parent folds `rec` into app, so its
        // folded closure includes the demanded peer id and it satisfies the slot;
        // its inherited `rec` fields and own `k` are enforced.
        let v = build_repos(
            &[REC],
            &[
                (
                    "type/mine.type.yaml",
                    "extends: rec::base\nfields:\n  k: String\n",
                ),
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: mine\n  r: hi\n  n: 3\n  k: y\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "a source subtype extending the peer validates clean: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_explicit_inline_claim_of_the_exact_demand_validates_against_it() {
        // The inline restates the demand exactly (`type: rec::base` at a `rec::base&`
        // slot). It routes through the membership path and validates against the
        // peer shape: omitting `rec`'s required `n` fires required-field-absent.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: rec::base\n  r: hi\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "the exact-demand explicit claim still enforces the peer shape: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_explicit_inline_claim_of_an_absent_type_fires_unknown_type_claim() {
        // The inline declares a bare type absent from every graph, so the qualified
        // inline path fires `unknown-type-claim`, mirroring the file-level rule.
        let v = build_repos(
            &[REC],
            &[
                ("type/holder.type.yaml", "fields:\n  m: rec::base&\n"),
                ("h.md", "---\ntype: holder\nm:\n  type: ghost\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"unknown-type-claim".to_string()),
            "an absent inline claim fires unknown-type-claim: {:?}",
            codes_on(&v, "h.md")
        );
    }

    // base owns a sealed family `sh` (parent field `p`) with a non-sealed leaf
    // `sh.leaf` (own field `q`), for the sealed-peer-demand cases.
    const SEALED: [(&str, &str); 2] = [
        (
            "type/sh.type.yaml",
            "fields:\n  p: String\nsealed:\n  - sh.leaf\n",
        ),
        (
            "type/sh.leaf.type.yaml",
            "extends: sh\nfields:\n  q: String\n",
        ),
    ];

    #[test]
    fn a_sealed_peer_demand_with_an_omitted_type_fires_missing_type() {
        // `m: sh::base&` demands a sealed peer parent; an inline value with no
        // `type:` cannot default to a sealed parent, so it must declare a
        // non-sealed descendant — `inline-value-missing-type`.
        let v = build_repos(
            &SEALED,
            &[
                ("type/holder.type.yaml", "fields:\n  m: sh::base&\n"),
                ("h.md", "---\ntype: holder\nm:\n  p: x\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-missing-type".to_string()),
            "an omitted `type:` at a sealed peer slot fires missing-type: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_sealed_peer_demand_with_an_explicit_leaf_validates() {
        // An explicit non-sealed descendant `sh.leaf::base` satisfies the sealed
        // demand and validates against its folded shape (parent `p` + own `q`).
        let v = build_repos(
            &SEALED,
            &[
                ("type/holder.type.yaml", "fields:\n  m: sh::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: sh.leaf::base\n  p: x\n  q: y\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "an explicit non-sealed leaf validates clean at a sealed peer slot: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_sealed_peer_demand_with_an_explicit_leaf_enforces_its_fields() {
        // The same leaf, omitting `sh`'s required `p`, fires required-field-absent,
        // proving the sealed descendant's folded contract is enforced.
        let v = build_repos(
            &SEALED,
            &[
                ("type/holder.type.yaml", "fields:\n  m: sh::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: sh.leaf::base\n  q: y\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "the sealed leaf's inherited `p` is enforced: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_sealed_peer_parent_claimed_inline_fires_sealed_parent_claimed() {
        // Claiming the sealed peer parent `sh::base` directly (not a descendant) is
        // `sealed-parent-claimed`, even though its own id is trivially in its
        // folded closure.
        let v = build_repos(
            &SEALED,
            &[
                ("type/holder.type.yaml", "fields:\n  m: sh::base&\n"),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: sh::base\n  p: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"sealed-parent-claimed".to_string()),
            "claiming a sealed peer parent directly fires sealed-parent-claimed: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_typed_fence_block_id_claiming_the_peer_type_satisfies() {
        // A body typed-fence claim is now a fold seed (the D3-body-claim position),
        // so a fence claiming `thing::base` folds `thing` into app, and a reference
        // `[[t^^blk]]` against `thing::base*` resolves the block's own claim and is
        // satisfied — a REAL check, no longer skipped.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("type/thost.type.yaml", "fields: {}\n"),
                (
                    "t.md",
                    "---\ntype: thost\n---\n\n# S\n\n```yaml [:x]\ntype: thing::base\nt: y\n```\n^blk\n",
                ),
                ("h.md", "---\ntype: holder\nref: \"[[t^^blk]]\"\n---\n"),
            ],
        );
        assert!(
            mismatches_on(&v, "h.md").is_empty(),
            "the fence's own `thing::base` claim satisfies `thing::base*`: {:?}",
            v.diagnostics()
                .map(|d| (d.code.as_str(), d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_typed_fence_block_id_claiming_a_different_type_mismatches() {
        // The fence claims app's own `other`, not the peer type, so the demand
        // `thing::base*` is not satisfied — the block claim is genuinely checked
        // (not skipped), and the mismatch fires.
        let v = build_repos(
            &[THING],
            &[
                ("type/holder.type.yaml", "fields:\n  ref: thing::base*\n"),
                ("type/thost.type.yaml", "fields: {}\n"),
                ("type/other.type.yaml", "fields:\n  o: String\n"),
                (
                    "t.md",
                    "---\ntype: thost\n---\n\n# S\n\n```yaml [:x]\ntype: other\no: y\n```\n^blk\n",
                ),
                ("h.md", "---\ntype: holder\nref: \"[[t^^blk]]\"\n---\n"),
            ],
        );
        assert_eq!(
            mismatches_on(&v, "h.md").len(),
            1,
            "the fence's own claim does not satisfy the peer demand"
        );
    }

    // ----- D3-inline-compound: a compound qualified inline slot checks each
    // branch by identity, not by name. -----

    // base owns `recA` (a) and `recB` (b), for compound-branch cases.
    const REC_A: (&str, &str) = ("type/recA.type.yaml", "fields:\n  a: String\n");
    const REC_B: (&str, &str) = ("type/recB.type.yaml", "fields:\n  b: String\n");

    #[test]
    fn a_compound_union_qualified_inline_is_satisfied_by_the_peer_branch() {
        // `m: <recA::base | recB::base>&`, inline `type: recA::base` supplies recA's
        // `a` — clean, the peer branch is satisfied and its fields enforced.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recA::base\n  a: hi\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "the peer branch recA::base is satisfied: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_union_qualified_inline_enforces_the_claimed_branchs_fields() {
        // The same slot, omitting recA's required `a`, fires required-field-absent —
        // the claimed branch's folded contract is enforced.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>&\n",
                ),
                ("h.md", "---\ntype: holder\nm:\n  type: recA::base\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"required-field-absent".to_string()),
            "recA::base's required `a` is enforced: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_union_qualified_inline_rejects_an_own_same_named_type() {
        // THE SOUNDNESS FIX: app owns its OWN `recA` (field `z`), distinct from the
        // peer `recA::base`. An inline `type: recA` claims the OWN type, which does
        // NOT satisfy the demand `<recA::base | recB::base>&`. A by-name compat
        // would false-pass; the TypeId membership rejects it as not-compatible.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                ("type/recA.type.yaml", "fields:\n  z: String\n"),
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recA\n  z: hi\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-type-not-compatible".to_string()),
            "an own same-named type does not satisfy a qualified branch: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_union_qualified_inline_rejects_a_value_matching_no_branch() {
        // An unrelated own type reaches neither peer branch — not-compatible.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                ("type/unrelated.type.yaml", "fields:\n  u: String\n"),
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: unrelated\n  u: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-type-not-compatible".to_string()),
            "a value reaching no branch fires not-compatible: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_intersection_qualified_inline_needs_every_branch() {
        // `m: <recA::base & recB::base>&`, inline mixin `type: [recA::base,
        // recB::base]` reaches BOTH peers, so the intersection is satisfied and both
        // folded field sets are enforced (a + b supplied → clean).
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base & recB::base>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type:\n    - recA::base\n    - recB::base\n  a: x\n  b: y\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "a mixin reaching both branches satisfies the intersection: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_intersection_qualified_inline_rejects_a_single_branch_claim() {
        // The same intersection, an inline claiming only `recA::base` reaches recA
        // but not recB, so the intersection is unsatisfied — not-compatible.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base & recB::base>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recA::base\n  a: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-type-not-compatible".to_string()),
            "a single-branch claim does not satisfy the intersection: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_with_a_bare_branch_is_satisfied_by_that_own_type() {
        // A compound mixing a qualified and a BARE branch (`<recA::base | localBare>&`):
        // an inline claiming the own `localBare` satisfies via the bare branch (name
        // in closure) and its fields are enforced — the bare-branch path still works
        // beside the qualified one.
        let v = build_repos(
            &[REC_A],
            &[
                ("type/localBare.type.yaml", "fields:\n  c: String\n"),
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | localBare>&\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: localBare\n  c: x\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "the bare branch localBare is satisfied by the own type: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_bare_union_shape_qualified_inline_rejects_an_own_same_named_type() {
        // The bare `<recA::base | recB::base>` slot (no `&`, the `Shape::Union`
        // dispatch, distinct from the `&` `CompoundReference` path). An own
        // same-named `recA` still does not satisfy the qualified branch — the
        // identity fix holds on both dispatch paths.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                ("type/recA.type.yaml", "fields:\n  z: String\n"),
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>\n",
                ),
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: recA\n  z: hi\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-type-not-compatible".to_string()),
            "the bare-union-shape path also rejects an own same-named type: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn a_compound_qualified_inline_with_omitted_type_fires_missing_type() {
        // A compound slot always requires an explicit `type:` (which branch); an
        // omitted claim at `<recA::base | recB::base>&` fires missing-type.
        let v = build_repos(
            &[REC_A, REC_B],
            &[
                (
                    "type/holder.type.yaml",
                    "fields:\n  m: <recA::base | recB::base>&\n",
                ),
                ("h.md", "---\ntype: holder\nm:\n  a: x\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").contains(&"inline-value-missing-type".to_string()),
            "an omitted type at a compound slot fires missing-type: {:?}",
            codes_on(&v, "h.md")
        );
    }

    // ----- D3-nested-owner-fields: a folded peer type's FIELD shapes name the
    // peer's OWN types (bare), so they must be validated against the OWNER repo,
    // not the source. Every consumer of the folded effective shape is affected
    // (the claim path, parent extension, explicit + omitted inline delegation),
    // across every name-bearing shape (reference / record / list / compound). A
    // source-local target CLAIMING the peer type satisfies (worked-example case 1).
    // These are RED until the fold-time + owner-shape re-qualification lands. -----

    // base owns `baz` (required `v`) referenced by peer field shapes, `qux` for a
    // compound branch, and holder types carrying each name-bearing field kind.
    const NPF_BASE: [(&str, &str); 6] = [
        ("type/baz.type.yaml", "fields:\n  v: String\n"),
        ("type/qux.type.yaml", "fields:\n  w: String\n"),
        ("type/foo.type.yaml", "fields:\n  child: baz*\n"),
        ("type/fooRec.type.yaml", "fields:\n  child: baz\n"),
        ("type/fooList.type.yaml", "fields:\n  kids: baz*[]\n"),
        ("type/fooC.type.yaml", "fields:\n  child: <baz | qux>*\n"),
    ];

    // A source-local target claiming the peer type `baz::base` (case 1).
    const NPF_T: (&str, &str) = ("t.md", "---\ntype: baz::base\nv: hi\n---\n");

    #[test]
    fn a_claim_with_a_peer_reference_field_accepts_a_valid_target() {
        // `type: foo::base`, foo's folded `child: baz*` names a BASE type. A
        // source-local target claiming `baz::base` satisfies it — currently a false
        // reference-target-type-mismatch (baz resolved in the source graph).
        let v = build_repos(
            &NPF_BASE,
            &[
                NPF_T,
                ("i.md", "---\ntype: foo::base\nchild: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a valid peer-typed target satisfies the folded reference field: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_claim_with_a_peer_reference_field_rejects_a_wrong_target() {
        // The negative guard: a target claiming an unrelated type does NOT satisfy
        // the folded `child: baz*`, so the mismatch fires (for the right reason).
        let v = build_repos(
            &NPF_BASE,
            &[
                ("type/unrelated.type.yaml", "fields:\n  u: String\n"),
                ("w.md", "---\ntype: unrelated\nu: x\n---\n"),
                ("i.md", "---\ntype: foo::base\nchild: \"[[w]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").contains(&"reference-target-type-mismatch".to_string()),
            "a wrong target is rejected: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_claim_with_a_peer_record_field_validates_the_inline_map() {
        // foo's folded `child: baz` (record) validates an inline map against BASE's
        // `baz` shape: supplying `v` is clean.
        let v = build_repos(
            &NPF_BASE,
            &[("i.md", "---\ntype: fooRec::base\nchild:\n  v: hi\n---\n")],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a peer record field validates the inline map against the owner: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_claim_with_a_peer_record_field_enforces_the_owner_required_field() {
        // The same record field, omitting baz's required `v`, fires
        // required-field-absent against the OWNER's contract.
        let v = build_repos(
            &NPF_BASE,
            &[("i.md", "---\ntype: fooRec::base\nchild:\n  x: 1\n---\n")],
        );
        assert!(
            codes_on(&v, "i.md").contains(&"required-field-absent".to_string()),
            "the owner's required `v` is enforced in the nested record: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_claim_with_a_peer_list_reference_field_accepts_valid_targets() {
        // foo's folded `kids: baz*[]` (list of references) accepts a valid target.
        let v = build_repos(
            &NPF_BASE,
            &[
                NPF_T,
                (
                    "i.md",
                    "---\ntype: fooList::base\nkids:\n  - \"[[t]]\"\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a peer list-reference field accepts a valid target: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_claim_with_a_peer_compound_reference_field_accepts_a_branch_target() {
        // foo's folded `child: <baz | qux>*` accepts a target claiming either
        // branch (here `baz::base`).
        let v = build_repos(
            &NPF_BASE,
            &[
                NPF_T,
                ("i.md", "---\ntype: fooC::base\nchild: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a peer compound-reference field accepts a branch target: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_source_subtype_of_a_peer_inherits_the_peer_reference_field_correctly() {
        // Parent-extension path: app's own `card` extends `foo::base`, inheriting
        // `child: baz*` through the fold. A valid target satisfies it.
        let v = build_repos(
            &NPF_BASE,
            &[
                ("type/card.type.yaml", "extends: foo::base\nfields: {}\n"),
                NPF_T,
                ("i.md", "---\ntype: card\nchild: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "an inherited peer reference field validates correctly: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn an_explicit_inline_delegation_validates_a_nested_peer_reference_field() {
        // Explicit-type inline: `m: foo::base&`, inline `type: foo::base` with a
        // nested `child: [[t]]`. The folded foo's `child: baz*` validates correctly.
        let v = build_repos(
            &NPF_BASE,
            &[
                ("type/holder.type.yaml", "fields:\n  m: foo::base&\n"),
                NPF_T,
                (
                    "h.md",
                    "---\ntype: holder\nm:\n  type: foo::base\n  child: \"[[t]]\"\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "an explicit inline delegation validates a nested peer reference: {:?}",
            codes_on(&v, "h.md")
        );
    }

    #[test]
    fn an_omitted_type_inline_delegation_validates_a_nested_peer_reference_field() {
        // Omitted-type inline (the `owner_effective_shape` bypass): `m: foo::base&`,
        // inline with no `type:` and a nested `child: [[t]]`. The owner shape's
        // `child: baz*` must re-qualify to base, or this false-fires.
        let v = build_repos(
            &NPF_BASE,
            &[
                ("type/holder.type.yaml", "fields:\n  m: foo::base&\n"),
                NPF_T,
                ("h.md", "---\ntype: holder\nm:\n  child: \"[[t]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "h.md").is_empty(),
            "an omitted-type inline delegation validates a nested peer reference: {:?}",
            codes_on(&v, "h.md")
        );
    }

    // ----- D3-metabody-carry: a qualified meta `type:` / body `use:` parses,
    // carries the `::repo`, and DEFERS own-graph resolution so neither
    // false-errors. Was RED (meta fired `unknown-type-claim`, body-use fired
    // `body-use-out-of-closure`); groundwork, the gate + fold close the holes. -----

    #[test]
    fn a_qualified_meta_type_no_longer_false_errors() {
        // `meta: - type: dm::base` on a source type-def. Before carry the whole
        // `dm::base` string was looked up in the own graph and fired
        // unknown-type-claim; now the `::repo` is split off and deferred.
        let v = build_repos(
            &[("type/dm.type.yaml", "fields:\n  k: String\n")],
            &[(
                "type/holder.type.yaml",
                "meta:\n  - type: dm::base\n    k: hi\nfields: {}\n",
            )],
        );
        assert!(
            !codes_on(&v, "type/holder.type.yaml").contains(&"unknown-type-claim".to_string()),
            "a qualified meta type is deferred, not false-errored: {:?}",
            codes_on(&v, "type/holder.type.yaml")
        );
    }

    #[test]
    fn a_qualified_body_use_no_longer_false_errors() {
        // `body: - use: bt::base` on a source type-def extending `bt::base`. Before
        // carry the own-graph closure walk missed the qualified use and fired
        // body-use-out-of-closure; now it is deferred.
        let v = build_repos(
            &[("type/bt.type.yaml", "body:\n  - section: S\n")],
            &[(
                "type/card.type.yaml",
                "extends: bt::base\nbody:\n  - use: bt::base\n",
            )],
        );
        assert!(
            !codes_on(&v, "type/card.type.yaml").contains(&"body-use-out-of-closure".to_string()),
            "a qualified body use is deferred, not false-errored: {:?}",
            codes_on(&v, "type/card.type.yaml")
        );
    }

    // ----- D3-meta-fold: a meta `type: dm::repo` is an import seed, and the meta
    // block's fields validate against the FOLDED peer meta type, exactly as a
    // single-repo meta block validates against its own meta type. -----

    // base owns a meta type `dm` with two required fields.
    const DM: (&str, &str) = (
        "type/dm.type.yaml",
        "extends: au.engine.meta::au-engine\nfields:\n  k: String\n  n: Number\n",
    );

    #[test]
    fn a_qualified_meta_block_enforces_the_peer_required_fields() {
        // holder's meta claims `dm::base`; omitting dm's required `n` fires
        // required-field-absent against the FOLDED peer meta type (was silent
        // after carry).
        let v = build_repos(
            &[DM],
            &[(
                "type/holder.type.yaml",
                "meta:\n  - type: dm::base\n    k: hi\nfields: {}\n",
            )],
        );
        assert!(
            codes_on(&v, "type/holder.type.yaml").contains(&"required-field-absent".to_string()),
            "the peer meta type's required `n` is enforced: {:?}",
            codes_on(&v, "type/holder.type.yaml")
        );
    }

    #[test]
    fn a_qualified_meta_block_is_clean_when_satisfied() {
        let v = build_repos(
            &[DM],
            &[(
                "type/holder.type.yaml",
                "meta:\n  - type: dm::base\n    k: hi\n    n: 3\nfields: {}\n",
            )],
        );
        assert!(
            codes_on(&v, "type/holder.type.yaml").is_empty(),
            "a fully-supplied peer meta block is clean: {:?}",
            codes_on(&v, "type/holder.type.yaml")
        );
    }

    #[test]
    fn a_qualified_meta_block_field_shape_is_enforced() {
        // dm's `n: Number` rejects a string value, against the folded peer shape.
        let v = build_repos(
            &[DM],
            &[(
                "type/holder.type.yaml",
                "meta:\n  - type: dm::base\n    k: hi\n    n: notanumber\nfields: {}\n",
            )],
        );
        assert!(
            codes_on(&v, "type/holder.type.yaml").contains(&"field-shape-mismatch".to_string()),
            "the peer meta type's `n: Number` shape is enforced: {:?}",
            codes_on(&v, "type/holder.type.yaml")
        );
    }

    // ----- D3-crossrepo-defref: a `::repo` def-reference ceiling
    // (`type<baz::base>*`), the def-axis sibling of the reference demand. The
    // target DEF's folded PARENT closure must include the ceiling's peer id.
    // Covers an authored qualified ceiling AND a folded peer def-ref field
    // (`type<baz>*` re-qualified at the fold), same-repo + cross-repo target defs.
    // RED until check_def_reference is repo-aware on the ceiling. -----

    // base owns a tag `baz` (a valid def-ref target / ceiling) and `basecard`, a
    // base def whose parent closure includes baz (a cross-repo target def).
    const DEFREF_BASE: [(&str, &str); 2] = [
        ("type/baz.type.yaml", "fields: {}\n"),
        ("type/basecard.type.yaml", "extends: baz\nfields: {}\n"),
    ];

    #[test]
    fn an_authored_qualified_ceiling_accepts_a_same_repo_target_def() {
        // `d: type<baz::base>*`, value `[[mycard]]` where mycard is a SOURCE def
        // extending the peer `baz::base`, so its folded parent closure includes the
        // ceiling id — clean. Currently a false def-ref-closure-mismatch (baz
        // resolved in the source graph).
        let v = build_repos(
            &DEFREF_BASE,
            &[
                ("type/holder.type.yaml", "fields:\n  d: type<baz::base>*\n"),
                ("type/mycard.type.yaml", "extends: baz::base\nfields: {}\n"),
                ("i.md", "---\ntype: holder\nd: \"[[mycard]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a same-repo def extending the peer ceiling satisfies it: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn an_authored_qualified_ceiling_rejects_a_non_descendant_def() {
        // A source def NOT extending the ceiling fires def-ref-closure-mismatch.
        let v = build_repos(
            &DEFREF_BASE,
            &[
                ("type/holder.type.yaml", "fields:\n  d: type<baz::base>*\n"),
                ("type/plain.type.yaml", "fields: {}\n"),
                ("i.md", "---\ntype: holder\nd: \"[[plain]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").contains(&"def-ref-closure-mismatch".to_string()),
            "a def not extending the peer ceiling is rejected: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn an_authored_qualified_ceiling_accepts_a_cross_repo_target_def() {
        // The target def lives in base and extends baz there; `[[basecard::base]]`
        // resolves in base, its folded closure includes the ceiling id — clean.
        let v = build_repos(
            &DEFREF_BASE,
            &[
                ("type/holder.type.yaml", "fields:\n  d: type<baz::base>*\n"),
                (
                    "i.md",
                    "---\ntype: holder\nd: \"[[basecard::base]]\"\n---\n",
                ),
            ],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a cross-repo target def extending the ceiling satisfies it: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_folded_peer_def_ref_field_accepts_a_valid_target_def() {
        // A PEER type `foo` (in base) has a def-ref field `dd: type<baz>*` (baz is
        // base's own). Folded into source, `type<baz>*` must re-qualify to
        // `type<baz::base>*`, so a target def reaching baz satisfies it.
        let v = build_repos(
            &[
                ("type/baz.type.yaml", "fields: {}\n"),
                ("type/basecard.type.yaml", "extends: baz\nfields: {}\n"),
                ("type/foo.type.yaml", "fields:\n  dd: type<baz>*\n"),
            ],
            &[(
                "i.md",
                "---\ntype: foo::base\ndd: \"[[basecard::base]]\"\n---\n",
            )],
        );
        assert!(
            codes_on(&v, "i.md").is_empty(),
            "a folded peer def-ref field accepts a valid target def: {:?}",
            codes_on(&v, "i.md")
        );
    }

    #[test]
    fn a_folded_peer_def_ref_field_rejects_a_non_descendant_def() {
        // The same folded field, a target def not reaching baz, mismatches.
        let v = build_repos(
            &[
                ("type/baz.type.yaml", "fields: {}\n"),
                ("type/foo.type.yaml", "fields:\n  dd: type<baz>*\n"),
            ],
            &[
                ("type/plain.type.yaml", "fields: {}\n"),
                ("i.md", "---\ntype: foo::base\ndd: \"[[plain]]\"\n---\n"),
            ],
        );
        assert!(
            codes_on(&v, "i.md").contains(&"def-ref-closure-mismatch".to_string()),
            "a folded peer def-ref field rejects a non-descendant def: {:?}",
            codes_on(&v, "i.md")
        );
    }
}
