//! The cross-repo resolution graph: a repo R's own vocabulary plus the folded
//! claim / parent closures of every `foo::repo` it imports, keyed by `TypeId`
//! ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]).
//!
//! Where each repo's own [`TypeGraph`] is name-keyed and holds only that repo's
//! defs, the resolution graph is keyed by `TypeId = (name, ClosureHash)`, so it
//! can hold R's own `foo` and a peer's `foo::repo` at once (distinct ids), and
//! it dedups one identity reached two ways (same id) for free. The validator and
//! closure walk run against this graph; `duplicate-type-def`, `compose()`, and
//! drift stay on the own graph (the two-layer split).
//!
//! Scope, per the decision.
//! - the CLAIM / PARENT axis is folded here, the edges the closure walk follows
//!   to gather inherited fields and validate the identity closure.
//! - the field-SHAPE axis (`foo::repo*`) stays on the cross-repo reference seam
//!   (`target_closure_includes_cross_repo`), it is NOT folded into this graph, so
//!   a peer type reached only by a field shape does not enter here.
//! - sealed branches are local to their def's repo (a sealed leaf is always own),
//!   so they resolve within the same graph.
//!
//! Identity completeness, a named D1-vs-D4 boundary.
//! - the `TypeId` hash is the peer's OWN-graph `closure_id`, the referenced
//!   closure WITHIN its repo (parents plus own field-types, the Phase C hash).
//! - it does NOT yet fold in a peer type's OWN cross-repo dependencies, a type
//!   whose closure crosses a further `::repo` edge has an id over its in-repo
//!   part only. Sound for the common case (a peer type self-contained in its
//!   repo) and for parent-axis divergence; the cross-repo-complete Merkle id (the
//!   design's full referenced closure, folding peer ids across boundaries) is the
//!   D4 completion the diamond detection needs, a strict refinement behind the
//!   same `TypeId`, never a different key.
//!
//! Cross-repo cycles are real, the package manager supports mutual peers (A peers
//! B, B peers A), so the fold guards with a visited set over `TypeId` and
//! terminates on a cycle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};

use crate::closure_id::ClosureHash;
use crate::codes;
use crate::graph::TypeGraph;
use crate::typedef::{FieldDecl, MetaBlock, TypeName};

/// The import-tier identity of a folded type, `(name, closure-hash)`. The name
/// travels beside the hash so two cycle members (one shared hash) stay distinct,
/// and so a diamond surfaces as two ids sharing a name. The hash is the
/// referenced-closure [`ClosureHash`]; see the module note on its D1 completeness.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId {
    pub name: TypeName,
    pub hash: ClosureHash,
}

/// One folded type-def in a [`ResolutionGraph`]. Its parent and sealed edges are
/// resolved to `TypeId`s once, at fold time, so the closure walk reads ids with
/// no per-step origin resolution.
// Not `Eq`: `meta_blocks` carries `MetaBlock`, whose instance-value fields are
// only `PartialEq` (floats). Nothing needs `ResolvedNode: Eq`.
//
// Keep every field deterministically ordered (`Vec` / `BTreeMap`, never a
// `HashMap` / `HashSet`): the scale fuzzer `Debug`-diffs the folded graph to
// assert incremental-vs-full identity, so a non-ordered container here would make
// that diff nondeterministic (a spurious flake, or a masked real divergence).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedNode {
    pub id: TypeId,
    /// How this node was reached, the resolution edge that carries display:
    /// `None` is R's own def (bare `foo`), `Some(repo)` an imported peer def
    /// (`foo::repo`). Display recovers the authored form from here, never a side
    /// map, never the id.
    pub origin: Option<String>,
    pub source_path: PathBuf,
    /// The def's source span in its file, carried from the [`TypeDef`] at fold
    /// time (parity with `source_path`), so a resolution-graph load check can
    /// anchor a diagnostic at the def.
    pub source_span: ByteRange,
    /// Parent edges, the claim / parent axis, resolved to ids. A dangling parent
    /// (no id in its graph) is dropped, the load-time check owns it.
    pub parents: Vec<TypeId>,
    /// The def's own field declarations, carried verbatim. Their shapes are
    /// validated by the reference seam, not walked here.
    pub fields: Vec<FieldDecl>,
    /// Sealed branch ids, local to the def's repo.
    pub sealed: Vec<TypeId>,
    /// The raw `abstract: true` marker, carried from the def so a cross-repo
    /// claim on a peer's abstract type fires `abstract-type-claimed`, parity with
    /// the `sealed` carry. The DECLARED flag only; sealed-implies-abstract is
    /// composed at the use site, never baked in here.
    pub declared_abstract: bool,
    /// `required:` meta obligations resolved to peer identities, like `parents`:
    /// a bare target in the def's own repo, a `::repo` target in the named peer.
    /// Lets the required-subtype-meta satisfaction check walk a cross-repo base's
    /// obligation over folded ids ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
    pub required_meta: Vec<TypeId>,
    /// The def's `location:` block, carried verbatim (parity with `sealed` /
    /// `declared_abstract`), so a cross-repo claim on a peer's located type is
    /// placement-checked over the folded closure without the peer's graph. Out of
    /// identity (the hash excludes it), like the block itself.
    /// See [[spec - location constraints - a name template and path predicate as an advisory placement meet]].
    pub location: Option<crate::location::LocationSpec>,
    /// The def's `meta:` blocks, carried verbatim (parity with `location`), so the
    /// meta-surfacing walk inherits a peer parent's `meta:` through the fold
    /// without the peer's graph. `None` is no `meta:` key, `Some(vec![])` the
    /// `meta: []` suppression marker. Out of identity (the hash excludes it), like
    /// the blocks themselves. See [[type-def meta::au-type-system]].
    pub meta_blocks: Option<Vec<MetaBlock>>,
}

impl ResolvedNode {
    /// This node's authored form from its STORED fold `origin`: bare for an own
    /// def (`origin` `None`), `name::repo` for an imported peer def. Recovered from
    /// the origin, never the id (the id excludes repo by design).
    ///
    /// The origin is the node's, not the edge THIS lookup traversed. For a diamond
    /// (divergent same-named defs) each is a distinct node, so this is exact. For
    /// an IN-SYNC same-name identity own and peer both define, the ONE folded node
    /// retains a processing-order-dependent origin (possibly the peer), so a caller
    /// wanting a form relative to a specific repo must not trust this for that
    /// identity, the wire closure projection does its own ownership check instead.
    pub fn authored(&self) -> String {
        match &self.origin {
            Some(repo) => format!("{}::{}", self.id.name.as_str(), repo),
            None => self.id.name.as_str().to_string(),
        }
    }
}

/// R's resolution graph, the own vocabulary plus folded peer closures, keyed by
/// `TypeId`. Built by [`fold`]; consumed by validation and the closure walk.
#[derive(Debug, Clone, Default)]
pub struct ResolutionGraph {
    nodes: BTreeMap<TypeId, ResolvedNode>,
    /// The resolution edge, an authored form to its `TypeId`. Keyed by the form
    /// a use site writes, `(name, None)` for bare / own, `(name, Some(repo))` for
    /// a peer. This is how a claim resolves to a node without the peer's graph,
    /// and how display recovers the authored form; one identity reached via two
    /// authored forms registers both keys to the same id (dedup-safe), so no side
    /// map is needed.
    by_authored: BTreeMap<(TypeName, Option<String>), TypeId>,
}

impl ResolutionGraph {
    pub fn get(&self, id: &TypeId) -> Option<&ResolvedNode> {
        self.nodes.get(id)
    }

    pub fn contains(&self, id: &TypeId) -> bool {
        self.nodes.contains_key(id)
    }

    /// Resolve an authored claim, `name` with an optional `::repo` qualifier, to
    /// its node's `TypeId` via the resolution edge. `repo` is the authored
    /// qualifier, `None` for a bare / own name. `None` return means no such
    /// authored form was folded (an unresolvable peer the gate owns, or a name
    /// absent here).
    pub fn resolve_authored(&self, name: &TypeName, repo: Option<&str>) -> Option<&TypeId> {
        self.by_authored
            .get(&(name.clone(), repo.map(|s| s.to_string())))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TypeId, &ResolvedNode)> {
        self.nodes.iter()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Names bound to more than one `TypeId` in this graph, the diamond
    /// CANDIDATES. A name with one id is unambiguous. A name with several is
    /// EITHER a legitimate side-by-side hold (`foo::A` and `foo::B`, distinct
    /// `::repo` you wrote) OR the diamond (one name reached transitively at two
    /// diverged closures). Distinguishing the two, and the `align-the-pins`
    /// error, is D4's job; this is the raw helper it reads.
    pub fn name_conflicts(&self) -> BTreeMap<TypeName, Vec<TypeId>> {
        let mut by_name: BTreeMap<TypeName, Vec<TypeId>> = BTreeMap::new();
        for id in self.nodes.keys() {
            by_name.entry(id.name.clone()).or_default().push(id.clone());
        }
        by_name.retain(|_, ids| ids.len() > 1);
        by_name
    }
}

/// Resolve a repo's own graph by its global name, the seam the fold crosses a
/// `::repo` boundary through. The engine owns the workspace's repos, so only it
/// can map a repo label to a graph; au-core stays repo-agnostic behind this.
/// `None` for an absent / undeclared repo, so an unresolvable `::repo` edge folds
/// to nothing and the engine's gate owns the diagnostic.
pub trait PeerGraphResolver {
    fn graph_of(&self, repo: &str) -> Option<&TypeGraph>;
}

/// The `TypeId` of `name` in `graph`, `None` when the name is absent (a dangling
/// edge, the load check owns it). The hash is the graph's precomputed
/// `closure_id`, so this is an O(1) lookup.
fn id_in(graph: &TypeGraph, name: &TypeName) -> Option<TypeId> {
    graph.closure_id(name).map(|hash| TypeId {
        name: name.clone(),
        hash,
    })
}

/// Build repo R's resolution graph.
///
/// `own_repo` is R's global name (its own graph is fetched through the resolver
/// like any peer's, so all graphs share one lifetime). `seeds` are the
/// `(base, repo)` of every `::repo` instance claim discovered in R's files, the
/// import set's instance-claim half; the own-def parent half is discovered by
/// walking R's own defs, whose `::repo` parents are followed here.
///
/// The fold seeds the worklist with every own def plus each seed claim, then
/// follows parent (and sealed) edges, crossing `::repo` edges via the resolver,
/// keying each node by `TypeId` and guarding revisits, so it dedups and
/// terminates on cross-repo cycles. The field-shape axis is not followed.
pub fn fold(
    own_repo: &str,
    seeds: &[(TypeName, String)],
    resolver: &dyn PeerGraphResolver,
) -> ResolutionGraph {
    // Worklist of (repo, name) to resolve. Every own def is in R's resolution
    // graph (own instances validate as before); each `::repo` instance claim
    // seeds its peer closure. Own-def `::repo` parents are followed below.
    let mut work: Vec<(String, TypeName)> = Vec::new();
    if let Some(own) = resolver.graph_of(own_repo) {
        for name in own.names() {
            work.push((own_repo.to_string(), name.clone()));
        }
    }
    for (base, repo) in seeds {
        work.push((repo.clone(), base.clone()));
    }
    drain_worklist(own_repo, resolver, BTreeMap::new(), BTreeMap::new(), work)
}

/// Fold `extra_seeds` INTO an already-built resolution graph, returning the
/// extended graph. The existing nodes and resolution edges carry over, so the
/// full own vocabulary and every prior import stay resolvable, and the extra
/// seeds' closures fold in beside them.
///
/// The on-demand companion to [`fold`]: a use site names a peer type that no
/// file has yet imported (so it is absent from the pre-built graph), and needs
/// it resolved WITHOUT rebuilding the whole graph. The extend is authoritative,
/// absence from the pre-built graph is never proof a peer type is unknown, only
/// that no file imports it yet ([[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]]).
///
/// Seeds already present carry over untouched, the dedup guard skips a re-fold.
pub fn fold_extending(
    existing: &ResolutionGraph,
    own_repo: &str,
    extra_seeds: &[(TypeName, String)],
    resolver: &dyn PeerGraphResolver,
) -> ResolutionGraph {
    let work: Vec<(String, TypeName)> = extra_seeds
        .iter()
        .map(|(base, repo)| (repo.clone(), base.clone()))
        .collect();
    drain_worklist(
        own_repo,
        resolver,
        existing.nodes.clone(),
        existing.by_authored.clone(),
        work,
    )
}

/// Drain a `(repo, name)` worklist into a resolution graph, following the claim /
/// parent / sealed / required-meta edges and crossing `::repo` boundaries via the
/// resolver. Seeded either empty (a full [`fold`]) or from an existing graph (a
/// [`fold_extending`]); the dedup guard keys on already-folded ids, so a
/// pre-populated `nodes` map is skipped and never re-folded.
fn drain_worklist(
    own_repo: &str,
    resolver: &dyn PeerGraphResolver,
    mut nodes: BTreeMap<TypeId, ResolvedNode>,
    mut by_authored: BTreeMap<(TypeName, Option<String>), TypeId>,
    mut work: Vec<(String, TypeName)>,
) -> ResolutionGraph {
    while let Some((repo, name)) = work.pop() {
        // An unresolvable peer or a dangling name folds to nothing; the gate /
        // load check owns that diagnostic.
        let Some(graph) = resolver.graph_of(&repo) else {
            continue;
        };
        let Some(id) = id_in(graph, &name) else {
            continue;
        };
        // Register the resolution edge BEFORE the dedup guard, so an identity
        // reached via two authored forms registers both. The authored form is
        // relative to R: `None` when the target lives in R's own repo, else the
        // peer repo. Every registration for a key yields the same id (it is a
        // pure function of `(repo, name)`), so first-write is deterministic.
        let authored_repo = if repo == own_repo {
            None
        } else {
            Some(repo.clone())
        };
        by_authored
            .entry((name.clone(), authored_repo))
            .or_insert_with(|| id.clone());
        // A self-`::repo` qualifier (`note::app` written inside `app`) names the
        // own form. Register the qualified alias too, so a self-qualified use
        // resolves to the own node instead of folding to an empty shape. Without
        // it the use site's `resolve_authored(name, Some(own))` misses (own is
        // authored `None`) and validation silently switches off. The gate's
        // `type-repo-self` lint flags the redundancy; validation still holds.
        if repo == own_repo {
            by_authored
                .entry((name.clone(), Some(own_repo.to_string())))
                .or_insert_with(|| id.clone());
        }
        // Dedup and cycle guard: one id is folded once.
        if nodes.contains_key(&id) {
            continue;
        }
        let Some(td) = graph.get(&name) else {
            continue;
        };

        // Parent edges, the claim / parent axis. A bare parent stays in this
        // repo's graph; a `::repo` parent crosses to the named peer.
        let mut parents = Vec::new();
        for p in &td.parents {
            let (p_repo, p_graph) = match &p.repo {
                Some(rq) => match resolver.graph_of(rq) {
                    Some(g) => (rq.clone(), g),
                    None => continue,
                },
                None => (repo.clone(), graph),
            };
            if let Some(pid) = id_in(p_graph, &p.name) {
                parents.push(pid);
                work.push((p_repo, p.name.clone()));
            }
        }

        // Sealed branches are local to the def's repo (a sealed leaf is always
        // an own type), so they resolve in the same graph.
        let mut sealed = Vec::new();
        for s in &td.sealed {
            if let Some(sid) = id_in(graph, &s.name) {
                sealed.push(sid);
                work.push((repo.clone(), s.name.clone()));
            }
        }

        // Required-meta obligations, resolved like parents: a bare target in this
        // def's repo, a `::repo` target in the named peer. An unresolvable target
        // folds to nothing (the gate / load check owns the diagnostic).
        let mut required_meta = Vec::new();
        for r in &td.required_meta {
            let (r_repo, r_graph) = match &r.repo {
                Some(rq) => match resolver.graph_of(rq) {
                    Some(g) => (rq.clone(), g),
                    None => continue,
                },
                None => (repo.clone(), graph),
            };
            if let Some(rid) = id_in(r_graph, &r.name) {
                required_meta.push(rid.clone());
                work.push((r_repo, r.name.clone()));
            }
        }

        let origin = if repo == own_repo {
            None
        } else {
            Some(repo.clone())
        };
        // A PEER node's field shapes name the peer's OWN types with BARE names
        // (a cross-repo reference is always authored `::repo`, so a bare name is
        // definitionally the peer's own). Re-qualify them to the origin repo so
        // every consumer of the folded shape (claim / parent / inline validation,
        // the served wire shape) resolves them against the peer, not R. An own
        // node keeps bare names, they resolve in R's own graph.
        let fields = match &origin {
            Some(peer) => td.fields.iter().map(|f| f.qualified_to(peer)).collect(),
            None => td.fields.clone(),
        };
        nodes.insert(
            id.clone(),
            ResolvedNode {
                id,
                origin,
                source_path: td.source_path.clone(),
                source_span: td.source_span,
                parents,
                fields,
                sealed,
                declared_abstract: td.declared_abstract,
                required_meta,
                location: td.location.clone(),
                meta_blocks: td.meta_blocks.clone(),
            },
        );
    }

    ResolutionGraph { nodes, by_authored }
}

/// Cross-repo `extends:` parent cycles in a resolution graph, the cross-repo sibling
/// of the own-graph `check_cycles`.
///
/// A cross-repo cycle (H in r1 `extends: B::r2`, B in r2 `extends: H::r1`) spans two
/// graphs, so the per-repo own-graph cycle check cannot see it; the fold resolves
/// the parent edges to `TypeId`s and terminates the cycle via its dedup guard, so
/// this walks those resolved edges to surface it.
///
/// A cycle is reported only when BOTH hold, so it never double-fires with the
/// own-graph `check_cycles` and each cycle is surfaced at a real owned file:
/// - it CROSSES a repo boundary (has a peer-origin member) — an all-own cycle is
///   the own graph's, already `cycle-in-type-chain` there.
/// - it has a member THIS repo owns (`origin` `None`) — the anchor. A cycle whose
///   members are all imported (a purely-transitive cycle) is left to the repos
///   that own its members; here it would have no local file to point at.
///
/// The `cycle-in-type-chain` code is reused: it is the same defect, a cyclic
/// `extends:` chain, spanning repos. The message renders the authored form of each
/// member (`name` for an own node, `name::repo` for a peer), so the loop reads
/// `H::r1 -> B::r2 -> H::r1` style.
pub fn cross_repo_type_chain_cycles(rg: &ResolutionGraph) -> Vec<Diagnostic> {
    let mut explored: BTreeSet<TypeId> = BTreeSet::new();
    let mut diags = Vec::new();
    for (start, _) in rg.iter() {
        if explored.contains(start) {
            continue;
        }
        let mut path: Vec<TypeId> = Vec::new();
        let mut on_path: BTreeSet<TypeId> = BTreeSet::new();
        cycle_dfs(
            rg,
            start,
            &mut path,
            &mut on_path,
            &mut explored,
            &mut diags,
        );
    }
    diags
}

/// Render a node's authored form for a cycle message: bare for an own node, a
/// `name::repo` for an imported peer node. Delegates to [`ResolvedNode::authored`],
/// the one authored-form renderer.
fn authored_form(node: &ResolvedNode) -> String {
    node.authored()
}

fn cycle_dfs(
    rg: &ResolutionGraph,
    id: &TypeId,
    path: &mut Vec<TypeId>,
    on_path: &mut BTreeSet<TypeId>,
    explored: &mut BTreeSet<TypeId>,
    diags: &mut Vec<Diagnostic>,
) {
    if on_path.contains(id) {
        // Back-edge: the cycle is `path[start..]` plus the re-entered id closing it.
        let start = path
            .iter()
            .position(|n| n == id)
            .expect("on_path implies id is in path");
        let members: Vec<&ResolvedNode> =
            path[start..].iter().filter_map(|tid| rg.get(tid)).collect();
        emit_cycle(&members, diags);
        return;
    }
    if explored.contains(id) {
        return;
    }
    if path.len() >= crate::load_checks::MAX_TYPE_CHAIN_DEPTH {
        // The FOLDED chain hit the depth bound. Mirror the single-repo
        // `type-chain-depth-exceeded`, so a deep cross-repo chain is caught at
        // parity. Gated (like `emit_cycle`) to a cross-boundary chain anchored at
        // an owned member, so it never double-fires with the per-repo own-graph
        // depth check.
        let members: Vec<&ResolvedNode> = path
            .iter()
            .chain(std::iter::once(id))
            .filter_map(|tid| rg.get(tid))
            .collect();
        emit_depth_exceeded(&members, diags);
        return;
    }
    on_path.insert(id.clone());
    path.push(id.clone());
    if let Some(node) = rg.get(id) {
        for parent in &node.parents {
            cycle_dfs(rg, parent, path, on_path, explored, diags);
        }
    }
    path.pop();
    on_path.remove(id);
    explored.insert(id.clone());
}

/// Emit `type-chain-depth-exceeded` for a folded chain that hit the depth bound,
/// gated like [`emit_cycle`]: a cross-boundary chain anchored at an owned member,
/// so it never double-fires with the per-repo own-graph depth check.
fn emit_depth_exceeded(members: &[&ResolvedNode], diags: &mut Vec<Diagnostic>) {
    if !members.iter().any(|n| n.origin.is_some()) {
        return; // an all-own deep chain is the own graph's depth check
    }
    let Some(anchor) = members.iter().find(|n| n.origin.is_none()) else {
        return; // purely-imported: the owning repos report it
    };
    diags.push(Diagnostic {
        code: codes::TYPE_CHAIN_DEPTH_EXCEEDED,
        severity: Severity::Error,
        span: Span::new(anchor.source_path.clone(), anchor.source_span),
        message: format!(
            "type chain reached the maximum depth of {} via '{}'; flatten the inheritance hierarchy, or check for a cross-repo cycle the load-time check missed",
            crate::load_checks::MAX_TYPE_CHAIN_DEPTH,
            authored_form(anchor)
        ),
        related: vec![],
        fix: None,
    });
}

/// Emit `cycle-in-type-chain` for a discovered cycle, iff it crosses a repo
/// boundary and this repo owns a member to anchor at.
fn emit_cycle(members: &[&ResolvedNode], diags: &mut Vec<Diagnostic>) {
    let crosses_boundary = members.iter().any(|n| n.origin.is_some());
    if !crosses_boundary {
        // An all-own cycle is the own graph's; `check_cycles` owns it.
        return;
    }
    let Some(anchor) = members.iter().find(|n| n.origin.is_none()) else {
        // Purely-imported cycle: no local file to point at, the owning repos report it.
        return;
    };
    let chain: Vec<String> = members
        .iter()
        .map(|n| authored_form(n))
        .chain(std::iter::once(authored_form(members[0])))
        .collect();
    diags.push(Diagnostic {
        code: codes::CYCLE_IN_TYPE_CHAIN,
        severity: Severity::Error,
        span: Span::new(anchor.source_path.clone(), anchor.source_span),
        message: format!("cycle in `extends:` chain: {}", chain.join(" -> ")),
        related: vec![],
        fix: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::typedef::parse_type_def;
    use std::path::Path;

    /// A resolver over a fixed `repo -> graph` map.
    struct MapResolver {
        graphs: BTreeMap<String, TypeGraph>,
    }
    impl PeerGraphResolver for MapResolver {
        fn graph_of(&self, repo: &str) -> Option<&TypeGraph> {
            self.graphs.get(repo)
        }
    }

    /// Build a `TypeGraph` from `(path, yaml)` pairs; the path derives the name.
    fn graph(defs: &[(&str, &str)]) -> TypeGraph {
        let tds = defs
            .iter()
            .map(|(path, src)| {
                let docs = au_parser::yaml::parse(src).unwrap();
                parse_type_def(Path::new(path), src, 0, &docs[0])
                    .type_def
                    .expect("parses")
            })
            .collect();
        build_graph(tds).graph
    }

    fn resolver(graphs: &[(&str, TypeGraph)]) -> MapResolver {
        MapResolver {
            graphs: graphs
                .iter()
                .map(|(n, g)| (n.to_string(), g.clone()))
                .collect(),
        }
    }

    /// The single node whose name matches, panicking otherwise.
    fn node<'a>(rg: &'a ResolutionGraph, name: &str) -> &'a ResolvedNode {
        let matches: Vec<_> = rg
            .iter()
            .filter(|(id, _)| id.name.as_str() == name)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one `{name}`, got {matches:?}"
        );
        matches[0].1
    }

    #[test]
    fn own_only_fold_holds_every_own_def_as_origin_none() {
        // No imports: the resolution graph is R's own vocabulary, every node own.
        let app = graph(&[
            ("/v/app/type/note.type.yaml", "fields:\n  title: String\n"),
            (
                "/v/app/type/card.type.yaml",
                "extends: note\nfields:\n  n: Number\n",
            ),
        ]);
        let r = resolver(&[("app", app)]);
        let rg = fold("app", &[], &r);
        assert_eq!(rg.len(), 2);
        assert_eq!(node(&rg, "note").origin, None);
        let card = node(&rg, "card");
        assert_eq!(card.origin, None);
        // card's parent edge resolves to note's id.
        assert_eq!(card.parents, vec![node(&rg, "note").id.clone()]);
    }

    #[test]
    fn an_instance_claim_seed_folds_the_peer_closure() {
        // app imports `note::base` via an instance claim; base owns note. The
        // fold pulls note in as a peer node (origin Some("base")).
        let app = graph(&[]); // app owns nothing
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  title: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);
        let note = node(&rg, "note");
        assert_eq!(note.origin, Some("base".into()));
    }

    #[test]
    fn meta_surfaces_from_a_cross_repo_parent_not_a_local_same_name() {
        // base owns `note` carrying display-meta. app owns its OWN `note` carrying
        // runtime-meta (a legal same-name vendor), plus `card` extending the peer
        // `note::base`. Surfacing card's meta over the fold must reach the PEER
        // note's display-meta, never app's local same-named note's runtime-meta.
        let base = graph(&[(
            "/v/base/type/note.type.yaml",
            "fields:\n  title: String\nmeta:\n  - type: display-meta\n",
        )]);
        let app = graph(&[
            (
                "/v/app/type/note.type.yaml",
                "fields: {}\nmeta:\n  - type: runtime-meta\n",
            ),
            (
                "/v/app/type/card.type.yaml",
                "extends: note::base\nfields:\n  n: Number\n",
            ),
        ]);
        let r = resolver(&[("app", app.clone()), ("base", base)]);
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);

        // The peer parent's meta is inherited across the repo boundary.
        let display = crate::meta::lookup_meta(
            &app,
            Some(&rg),
            &TypeName("card".into()),
            &TypeName("display-meta".into()),
            None,
        );
        assert!(
            display.is_some(),
            "card inherits the peer note::base's display-meta over the fold"
        );

        // The local same-named `note`'s meta is NOT surfaced in the peer's place.
        let runtime = crate::meta::lookup_meta(
            &app,
            Some(&rg),
            &TypeName("card".into()),
            &TypeName("runtime-meta".into()),
            None,
        );
        assert!(
            runtime.is_none(),
            "card's parent is the peer note, so app's local note's runtime-meta must not surface"
        );
    }

    #[test]
    fn effective_shape_resolved_equals_the_own_shape_for_an_own_claim() {
        // The load-bearing equivalence: in an importing repo, a bare OWN claim
        // gets the SAME effective shape whether resolved over the fold or the own
        // graph. This is what lets the resolved layer switch layers freely, and a
        // repo flip None<->Some (first import added / last removed) leave its
        // other own-only instances byte-identical, no sibling re-validation.
        use crate::closure::{effective_shape, effective_shape_resolved};
        use crate::instance::TypeClaim;
        use crate::typedef::TypeNameClaim;
        use au_diagnostics::ByteRange;

        let app = graph(&[
            ("/v/app/type/note.type.yaml", "fields:\n  title: String\n"),
            (
                "/v/app/type/card.type.yaml",
                "extends: note\nfields:\n  n: Number\n",
            ),
        ]);
        let base = graph(&[("/v/base/type/thing.type.yaml", "fields:\n  x: String\n")]);
        // app imports `thing::base`, so it HAS a resolution graph, but the claim
        // under test is the bare own `card`.
        let r = resolver(&[("app", app.clone()), ("base", base)]);
        let rg = fold("app", &[(TypeName("thing".into()), "base".into())], &r);

        let claim = TypeClaim::Bare(TypeNameClaim::own(
            TypeName("card".into()),
            ByteRange::new(0, 0),
        ));
        let own = effective_shape(&app, &claim).unwrap();
        let resolved = effective_shape_resolved(&rg, &app, &claim).unwrap();
        assert_eq!(
            own, resolved,
            "an own claim's shape must be identical across the own and resolved layers"
        );
    }

    #[test]
    fn resolve_authored_maps_own_and_peer_forms_to_their_ids() {
        // app owns `card`, imports `note::base`. The resolution edge resolves the
        // bare own form and the qualified peer form to their nodes.
        let app = graph(&[("/v/app/type/card.type.yaml", "fields:\n  n: Number\n")]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  title: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);

        let card = rg.resolve_authored(&TypeName("card".into()), None);
        assert_eq!(card, Some(&node(&rg, "card").id));
        let note = rg.resolve_authored(&TypeName("note".into()), Some("base"));
        assert_eq!(note, Some(&node(&rg, "note").id));
        // A bare `note` is not R's own type, so it does not resolve own.
        assert_eq!(rg.resolve_authored(&TypeName("note".into()), None), None);
    }

    #[test]
    fn a_qualified_parent_is_folded_across_the_boundary() {
        // app's `card` extends `note::base`; base owns note. Walking app's own
        // `card` follows the `::repo` parent into base.
        let app = graph(&[(
            "/v/app/type/card.type.yaml",
            "extends: note::base\nfields:\n  n: Number\n",
        )]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  title: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[], &r);
        let card = node(&rg, "card");
        assert_eq!(card.origin, None);
        let note = node(&rg, "note");
        assert_eq!(note.origin, Some("base".into()));
        assert_eq!(card.parents, vec![note.id.clone()]);
    }

    #[test]
    fn the_same_identity_reached_two_ways_dedups() {
        // Two instance claims for the same peer type fold to one node.
        let app = graph(&[]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  title: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold(
            "app",
            &[
                (TypeName("note".into()), "base".into()),
                (TypeName("note".into()), "base".into()),
            ],
            &r,
        );
        assert_eq!(rg.len(), 1);
    }

    #[test]
    fn own_and_peer_same_name_are_distinct_nodes() {
        // app owns `note { a: String }`; base owns a DIFFERENT `note { a: Number }`.
        // Both held, distinct ids, surfaced as a name conflict (D4 adjudicates).
        let app = graph(&[("/v/app/type/note.type.yaml", "fields:\n  a: String\n")]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  a: Number\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);
        assert_eq!(rg.len(), 2);
        let conflicts = rg.name_conflicts();
        assert_eq!(
            conflicts.get(&TypeName("note".into())).map(|v| v.len()),
            Some(2)
        );
    }

    #[test]
    fn identical_peer_def_dedups_with_own_chain() {
        // app owns `note { a: String }` AND imports `note::base` whose def is
        // byte-identical. Same closure id, so they are the SAME type, one node.
        let app = graph(&[("/v/app/type/note.type.yaml", "fields:\n  a: String\n")]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  a: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);
        assert_eq!(rg.len(), 1, "identical defs across repos share one id");
        assert!(rg.name_conflicts().is_empty());
    }

    // D4a, the diamond's two NON-error cases over the fold's identity keying.

    #[test]
    fn a_diamond_dag_dedups_the_shared_ancestor() {
        // The worked diamond: `T4 extends T2::r2, T3::r3`; r2's `T2 extends T1::r1`
        // and r3's `T3 extends T1::r1`. `T1` is reached TWO WAYS through the parent
        // axis, but it is one `r1.T1`, one id, so it dedups to a single node — the
        // equal-closure diamond, not a conflict.
        let r1 = graph(&[("/v/r1/type/T1.type.yaml", "fields:\n  a: String\n")]);
        let r2 = graph(&[("/v/r2/type/T2.type.yaml", "extends: T1::r1\n")]);
        let r3 = graph(&[("/v/r3/type/T3.type.yaml", "extends: T1::r1\n")]);
        let r4 = graph(&[(
            "/v/r4/type/T4.type.yaml",
            "extends:\n  - T2::r2\n  - T3::r3\n",
        )]);
        let r = resolver(&[("r1", r1), ("r2", r2), ("r3", r3), ("r4", r4)]);
        let rg = fold("r4", &[], &r);

        let t1s = rg.iter().filter(|(id, _)| id.name.as_str() == "T1").count();
        assert_eq!(t1s, 1, "the shared ancestor T1 dedups to one node");
        assert!(
            rg.name_conflicts().is_empty(),
            "an equal-closure diamond is not a conflict: {:?}",
            rg.name_conflicts()
        );
        assert_eq!(rg.len(), 4, "the DAG folds to T4, T2, T3, T1");
    }

    #[test]
    fn two_peers_same_name_are_held_side_by_side() {
        // R imports `foo::a` and `foo::b` — two DIFFERENT repos each with their own
        // `foo`, like two crates each with a `Config`. Distinct authored keys, both
        // held, a name conflict but NOT a diamond (you addressed them apart).
        let a = graph(&[("/v/a/type/foo.type.yaml", "fields:\n  x: String\n")]);
        let b = graph(&[("/v/b/type/foo.type.yaml", "fields:\n  y: Number\n")]);
        let app = graph(&[]);
        let r = resolver(&[("app", app), ("a", a), ("b", b)]);
        let rg = fold(
            "app",
            &[
                (TypeName("foo".into()), "a".into()),
                (TypeName("foo".into()), "b".into()),
            ],
            &r,
        );
        assert_eq!(rg.len(), 2, "foo::a and foo::b are distinct nodes");
        assert!(rg
            .resolve_authored(&TypeName("foo".into()), Some("a"))
            .is_some());
        assert!(rg
            .resolve_authored(&TypeName("foo".into()), Some("b"))
            .is_some());
        assert_eq!(
            rg.name_conflicts()
                .get(&TypeName("foo".into()))
                .map(|v| v.len()),
            Some(2),
            "surfaced as a name conflict for D4 to classify (here, legitimate side-by-side)"
        );
    }

    #[test]
    fn a_cross_repo_parent_cycle_terminates() {
        // app's `x` extends `y::base`; base's `y` extends `x::app`. Mutual peers,
        // a parent cycle across the boundary. The visited guard halts it.
        let app = graph(&[(
            "/v/app/type/x.type.yaml",
            "extends: y::base\nfields:\n  a: String\n",
        )]);
        let base = graph(&[(
            "/v/base/type/y.type.yaml",
            "extends: x::app\nfields:\n  b: String\n",
        )]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[], &r);
        // Both folded, the walk terminated.
        assert!(rg.iter().any(|(id, _)| id.name.as_str() == "x"));
        assert!(rg.iter().any(|(id, _)| id.name.as_str() == "y"));
    }

    #[test]
    fn cross_repo_type_chain_cycles_reports_a_folded_cycle() {
        // The same mutual-peer parent cycle: x (app) extends y::base, y (base)
        // extends x::app. The detector surfaces it, anchored at app's own member,
        // with the peer member rendered qualified.
        let app = graph(&[("/v/app/type/x.type.yaml", "extends: y::base\n")]);
        let base = graph(&[("/v/base/type/y.type.yaml", "extends: x::app\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[], &r);
        let diags = cross_repo_type_chain_cycles(&rg);
        assert_eq!(diags.len(), 1, "one cross-repo cycle reported: {diags:?}");
        assert_eq!(diags[0].code, codes::CYCLE_IN_TYPE_CHAIN);
        // Anchored at app's own file (origin None member), peer rendered `y::base`.
        assert!(
            diags[0].span.file.ends_with("app/type/x.type.yaml"),
            "anchored at the own member's file: {:?}",
            diags[0].span.file
        );
        assert!(
            diags[0].message.contains("y::base"),
            "the peer member renders qualified: {}",
            diags[0].message
        );
    }

    #[test]
    fn cross_repo_type_chain_cycles_is_empty_without_a_cycle() {
        // app's card extends note::base, a clean cross-repo parent, no cycle.
        let app = graph(&[("/v/app/type/card.type.yaml", "extends: note::base\n")]);
        let base = graph(&[("/v/base/type/note.type.yaml", "fields:\n  t: String\n")]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[], &r);
        assert!(cross_repo_type_chain_cycles(&rg).is_empty());
    }

    #[test]
    fn an_unresolvable_peer_folds_to_nothing() {
        // A seed naming an absent repo contributes no node; the gate owns the
        // diagnostic. The own graph still folds.
        let app = graph(&[("/v/app/type/note.type.yaml", "fields:\n  title: String\n")]);
        let r = resolver(&[("app", app)]);
        let rg = fold("app", &[(TypeName("ghost".into()), "stranger".into())], &r);
        assert_eq!(rg.len(), 1);
        assert_eq!(node(&rg, "note").origin, None);
    }

    #[test]
    fn folded_closure_ids_reaches_a_peer_claim_and_its_parents() {
        // The reference-seam helper: a claim's folded parent-closure ids. app
        // imports thing::base; base's thing extends root. A qualified claim
        // `thing::base` folds to thing's id AND its parent root's id (the walk
        // crosses parents), so a demand `thing::base*` or `root::base*` is a
        // member of a value claiming thing::base.
        use crate::closure::folded_closure_ids;
        use crate::instance::TypeClaim;
        use crate::typedef::TypeNameClaim;
        use au_diagnostics::ByteRange;

        let app = graph(&[]);
        let base = graph(&[
            ("/v/base/type/root.type.yaml", "fields:\n  r: String\n"),
            (
                "/v/base/type/thing.type.yaml",
                "extends: root\nfields:\n  t: String\n",
            ),
        ]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("thing".into()), "base".into())], &r);

        let claim = TypeClaim::Bare(TypeNameClaim::parse("thing::base", ByteRange::new(0, 0)));
        let ids = folded_closure_ids(&rg, &claim);
        let names: BTreeMap<&str, &TypeId> = ids.iter().map(|id| (id.name.as_str(), id)).collect();
        assert_eq!(names.len(), 2, "thing and its parent root: {names:?}");
        assert_eq!(names.get("thing"), Some(&&node(&rg, "thing").id));
        assert_eq!(names.get("root"), Some(&&node(&rg, "root").id));
    }

    #[test]
    fn folded_closure_ids_of_an_unresolvable_claim_is_empty() {
        // A claim that does not resolve (an absent peer the gate owns) contributes
        // nothing, so the demand is simply unsatisfied, never a panic.
        use crate::closure::folded_closure_ids;
        use crate::instance::TypeClaim;
        use crate::typedef::TypeNameClaim;
        use au_diagnostics::ByteRange;

        let app = graph(&[("/v/app/type/note.type.yaml", "fields:\n  a: String\n")]);
        let r = resolver(&[("app", app)]);
        let rg = fold("app", &[], &r);
        let claim = TypeClaim::Bare(TypeNameClaim::parse(
            "ghost::stranger",
            ByteRange::new(0, 0),
        ));
        assert!(folded_closure_ids(&rg, &claim).is_empty());
    }

    #[test]
    fn fold_extending_resolves_a_not_yet_imported_peer_type() {
        // app imports nothing, so its pre-built graph has no `provenance::base`.
        // fold_extending seeds it on demand, leaving the own vocabulary intact —
        // the first-promotion path the ensure-mixin gate relies on.
        use crate::closure::folded_closure_ids;
        use crate::instance::TypeClaim;
        use crate::typedef::TypeNameClaim;
        use au_diagnostics::ByteRange;

        let app = graph(&[("/v/app/type/note.type.yaml", "fields:\n  a: String\n")]);
        let base = graph(&[(
            "/v/base/type/provenance.type.yaml",
            "fields:\n  p: String\n",
        )]);
        let r = resolver(&[("app", app), ("base", base)]);

        let pre = fold("app", &[], &r);
        assert!(
            pre.resolve_authored(&TypeName("provenance".into()), Some("base"))
                .is_none(),
            "the peer is absent from the pre-built graph"
        );
        assert!(
            pre.resolve_authored(&TypeName("note".into()), None)
                .is_some(),
            "the own vocabulary is present"
        );

        let ext = fold_extending(
            &pre,
            "app",
            &[(TypeName("provenance".into()), "base".into())],
            &r,
        );
        assert!(
            ext.resolve_authored(&TypeName("provenance".into()), Some("base"))
                .is_some(),
            "the extended peer now resolves"
        );
        assert!(
            ext.resolve_authored(&TypeName("note".into()), None)
                .is_some(),
            "the own vocabulary still resolves"
        );

        let claim = TypeClaim::Bare(TypeNameClaim::parse(
            "provenance::base",
            ByteRange::new(0, 0),
        ));
        let ids = folded_closure_ids(&ext, &claim);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids.iter().next().unwrap().name.as_str(), "provenance");
    }

    #[test]
    fn sealed_branches_resolve_in_the_peer_graph() {
        // Importing a sealed family pulls its branches (local to the peer).
        let app = graph(&[]);
        let base = graph(&[
            ("/v/base/type/k.type.yaml", "sealed:\n  - k.a\n  - k.b\n"),
            (
                "/v/base/type/k.a.type.yaml",
                "extends: k\nfields:\n  a: String\n",
            ),
            (
                "/v/base/type/k.b.type.yaml",
                "extends: k\nfields:\n  b: String\n",
            ),
        ]);
        let r = resolver(&[("app", app), ("base", base)]);
        let rg = fold("app", &[(TypeName("k".into()), "base".into())], &r);
        let k = node(&rg, "k");
        assert_eq!(k.sealed.len(), 2);
        // The branches are folded too (origin base).
        assert_eq!(node(&rg, "k.a").origin, Some("base".into()));
    }
}
