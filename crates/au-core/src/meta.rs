//! Consumer walk over the type-graph's `meta:` blocks ([[type-def meta::au-type-system]]).
//!
//! `lookup_meta` is the canonical helper: ask "does this host type-def
//! carry — directly or via ancestor inheritance — a `meta:` sub-region for
//! this named meta-type?" Returns the matching `MetaBlock` if any.
//!
//! Semantics summary (see [[type-def meta::au-type-system]] for the full statement):
//! - `meta:` is NOT inherited as data — each query walks ancestors at
//!   read time. The host's own metas are consulted first; missing
//!   queries fall through to parents.
//! - `meta: []` on any traversed type-def is a stop signal — walks
//!   originating at or below that node halt there and return `None`.
//! - The walk does NOT recurse into the metas of type-defs it
//!   encounters inside `meta:` ([[type-def meta::au-type-system]]). It targets the host's `type:`
//!   ancestor chain, not the chains of named meta-type-defs.
//! - Termination is bounded by the `cycle-in-type-chain` load check —
//!   the inheritance graph is acyclic at lookup time. A
//!   defensive `visited` set guards diamond ancestors so each is
//!   visited at most once (and would short-circuit any pathological
//!   cycle if one ever slipped past the load check).

use std::collections::HashSet;

use crate::graph::TypeGraph;
use crate::resolution::{ResolutionGraph, TypeId};
use crate::typedef::{MetaBlock, TypeName};

/// Resolve which `MetaBlock` answers a `meta_type` query at `host`.
///
/// Walks host's own metas first; on no-match (or no metas at all) walks
/// `host.parents` in claim-list order. For mixin parents the first
/// branch to yield a match wins. A `meta: []` (`Some(vec![])`)
/// encountered anywhere along the walk halts it and returns `None`.
///
/// `resolution` selects the graph the walk runs over, so surfacing is correct on
/// both surfaces (the parity `location`'s `effective_locations` already has):
/// - `Some(rg)`, walk the FOLDED resolution graph, so a `::repo` parent's meta is
///   inherited through the fold. Parents are resolved to ids at fold time, so a
///   peer parent is never confused for a local same-named type.
/// - `None`, walk the local `TypeGraph` only; a `::repo` parent cannot resolve
///   here, so it is skipped and its branch contributes nothing.
///
/// Returns `None` when:
/// - `host` is not in the graph, OR
/// - no traversed type-def carries a matching sub-region, OR
/// - a `meta: []` suppression marker was hit before any match.
///
/// Pure read; no diagnostics. `meta_repo` is the `::repo` qualifier of the SOUGHT
/// meta type, `None` for an own meta; an own `display-meta` and a peer
/// `display-meta::other` are distinct, so the match keys on (name, repo).
pub fn lookup_meta<'g>(
    graph: &'g TypeGraph,
    resolution: Option<&'g ResolutionGraph>,
    host: &TypeName,
    meta_type: &TypeName,
    meta_repo: Option<&str>,
) -> Option<&'g MetaBlock> {
    match resolution {
        // Folded path: resolve the host to its id, then walk parent ids the fold
        // already crossed `::repo` boundaries for.
        Some(rg) => {
            let host_id = rg.resolve_authored(host, None)?;
            let mut visited: HashSet<TypeId> = HashSet::new();
            visit_resolved(rg, host_id, meta_type, meta_repo, &mut visited)
        }
        // Own-graph path: no fold to resolve a peer, so a `::repo` parent is
        // skipped rather than mistaken for a local same-named type.
        None => {
            let mut visited: HashSet<TypeName> = HashSet::new();
            visit(graph, host, meta_type, meta_repo, &mut visited)
        }
    }
}

fn visit<'g>(
    graph: &'g TypeGraph,
    name: &TypeName,
    meta_type: &TypeName,
    meta_repo: Option<&str>,
    visited: &mut HashSet<TypeName>,
) -> Option<&'g MetaBlock> {
    // Diamond / cycle short-circuit. The load-time cycle check makes
    // the cycle case unreachable in practice, but visited-deduplication
    // is also a performance guard for mixin diamonds where two parent
    // branches meet at a shared ancestor — without it we'd traverse
    // the shared ancestor's subtree twice.
    if !visited.insert(name.clone()) {
        return None;
    }

    let td = graph.get(name)?;

    match &td.meta_blocks {
        // [[type-def meta::au-type-system]] suppression: stop the walk entirely. Ancestors are not
        // visited from this branch. Other branches (siblings in a mixin
        // chain) may still resolve via their own walks — that's by
        // design; suppression is per-chain, not global.
        Some(blocks) if blocks.is_empty() => return None,
        Some(blocks) => {
            if let Some(b) = blocks
                .iter()
                .find(|b| &b.type_name == meta_type && b.repo.as_deref() == meta_repo)
            {
                return Some(b);
            }
            // Has metas, but none for `meta_type` — fall through to
            // ancestors per [[type-def meta::au-type-system]] ("the consumer walks up the type
            // chain"). The suppression rule applies only to the empty-
            // list form, not to "has metas, different names."
        }
        None => {
            // No `meta:` key at all. Per [[type-def meta::au-type-system]]: "Absence of `meta:`
            // entirely means 'this type-def contributes nothing of its
            // own; the walk continues to ancestors as usual.'"
        }
    }

    // Walk parents in claim-list order. For mixin (`type: [a, b]`),
    // `a`'s chain is explored fully before `b`'s — per-claim
    // discipline. Order matters for diagnostics' determinism even
    // though both branches are mathematically equivalent paths.
    //
    // Walks do NOT recurse into the metas of the named meta-type-def
    // the host points at ([[type-def meta::au-type-system]]). We only traverse `td.parents` — the
    // host's `type:` chain — not the meta-type-defs encountered along
    // the way.
    for parent in &td.parents {
        // A `::repo` parent is a peer type the own graph cannot resolve; without
        // the fold (the `Some(rg)` path) its branch contributes nothing. Skipping
        // avoids re-entering a local same-named type and surfacing its meta.
        if parent.is_qualified() {
            continue;
        }
        if let Some(found) = visit(graph, &parent.name, meta_type, meta_repo, visited) {
            return Some(found);
        }
    }

    None
}

/// Folded-graph sibling of [`visit`], over `TypeId` ids. Parents are resolved
/// cross-repo at fold time, so a peer parent's `meta:` is inherited without the
/// peer's graph and a `::repo` parent is never mistaken for a local same-named
/// type. Own-first, `meta: []` suppression, first-match in claim order, and the
/// no-recurse-into-encountered-metas rule all mirror [`visit`].
fn visit_resolved<'g>(
    rg: &'g ResolutionGraph,
    id: &TypeId,
    meta_type: &TypeName,
    meta_repo: Option<&str>,
    visited: &mut HashSet<TypeId>,
) -> Option<&'g MetaBlock> {
    if !visited.insert(id.clone()) {
        return None;
    }
    let node = rg.get(id)?;
    match &node.meta_blocks {
        Some(blocks) if blocks.is_empty() => return None,
        Some(blocks) => {
            if let Some(b) = blocks
                .iter()
                .find(|b| &b.type_name == meta_type && b.repo.as_deref() == meta_repo)
            {
                return Some(b);
            }
        }
        None => {}
    }
    for parent in &node.parents {
        if let Some(found) = visit_resolved(rg, parent, meta_type, meta_repo, visited) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::typedef::{
        FieldDecl, FieldName, MetaBlock, ParentClaim, ParentClaimForm, TypeDef, TypeName,
        TypeNameClaim,
    };
    use au_diagnostics::ByteRange;
    use std::path::PathBuf;

    fn td(name: &str, parents: &[&str]) -> TypeDef {
        let parent_claim = if parents.is_empty() {
            None
        } else {
            Some(ParentClaim {
                form: if parents.len() == 1 {
                    ParentClaimForm::BareName
                } else {
                    ParentClaimForm::List
                },
                value_span: ByteRange::new(0, 0),
            })
        };
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim,
            parents: parents
                .iter()
                .map(|p| TypeNameClaim::own(TypeName((*p).into()), ByteRange::new(0, 0)))
                .collect(),
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

    fn with_meta(mut t: TypeDef, blocks: Vec<MetaBlock>) -> TypeDef {
        t.meta_blocks = Some(blocks);
        t
    }

    /// Mark `meta: []` (the [[type-def meta::au-type-system]] suppression form) on a type-def.
    fn with_suppressed_meta(mut t: TypeDef) -> TypeDef {
        t.meta_blocks = Some(vec![]);
        t
    }

    fn meta_block(type_name: &str) -> MetaBlock {
        MetaBlock {
            type_name: TypeName(type_name.into()),
            repo: None,
            type_name_span: ByteRange::new(0, 0),
            block_span: ByteRange::new(0, 0),
            fields: vec![],
            body_span: ByteRange::new(0, 0),
            doc: None,
            field_docs: Default::default(),
        }
    }

    /// Build a single-field meta-type-def. Just needs to exist so the host's
    /// `meta:` entry resolves to something nameable.
    fn meta_typedef(name: &str) -> TypeDef {
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim: None,
            parents: vec![],
            fields: vec![FieldDecl {
                name: FieldName("tldr".into()),
                optional: true,
                raw_shape: "String".into(),
                name_span: ByteRange::new(0, 0),
                shape_span: ByteRange::new(0, 0),
                entry_span: ByteRange::new(0, 0),
                parsed_shape: Ok(au_grammar::Shape::Primitive(au_grammar::Primitive::String)),
                doc: None,
            }],
            sealed: vec![],
            declared_abstract: false,
            meta_blocks: None,
            required_meta: Vec::new(),
            body: None,
            doc: None,
            ..Default::default()
        }
    }

    fn tn(s: &str) -> TypeName {
        TypeName(s.into())
    }

    #[test]
    fn host_own_meta_wins() {
        // Host declares its own display-meta — `lookup_meta` returns it
        // directly, no ancestor walk.
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            with_meta(td("decision", &[]), vec![meta_block("display-meta")]),
        ])
        .graph;
        let found = lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None);
        assert!(found.is_some());
        assert_eq!(found.unwrap().type_name.as_str(), "display-meta");
    }

    fn peer_meta_block(type_name: &str, repo: &str) -> MetaBlock {
        MetaBlock {
            type_name: TypeName(type_name.into()),
            repo: Some(repo.into()),
            type_name_span: ByteRange::new(0, 0),
            block_span: ByteRange::new(0, 0),
            fields: vec![],
            body_span: ByteRange::new(0, 0),
            doc: None,
            field_docs: Default::default(),
        }
    }

    #[test]
    fn lookup_distinguishes_own_from_a_same_named_peer_meta() {
        // A host with both `display-meta` (own) and `display-meta::other` (peer),
        // distinct types. lookup_meta disambiguates by repo (finding 3.4b).
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            with_meta(
                td("host", &[]),
                vec![
                    meta_block("display-meta"),
                    peer_meta_block("display-meta", "other"),
                ],
            ),
        ])
        .graph;
        let own = lookup_meta(&g, None, &tn("host"), &tn("display-meta"), None).unwrap();
        assert!(own.repo.is_none(), "bare lookup returns the own meta");
        let peer = lookup_meta(&g, None, &tn("host"), &tn("display-meta"), Some("other")).unwrap();
        assert_eq!(
            peer.repo.as_deref(),
            Some("other"),
            "qualified lookup returns the peer meta"
        );
    }

    #[test]
    fn parent_fallback_when_host_absent() {
        // Host has no `meta:` of its own; parent declares display-meta.
        // Walk falls through to parent and returns parent's block.
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            with_meta(td("note", &[]), vec![meta_block("display-meta")]),
            td("decision", &["note"]),
        ])
        .graph;
        let found = lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None);
        assert!(found.is_some());
    }

    #[test]
    fn returns_none_when_no_ancestor_declares() {
        // Neither host nor any ancestor declares the requested meta.
        // Walk exhausts → None.
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            td("note", &[]),
            td("decision", &["note"]),
        ])
        .graph;
        assert!(lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None).is_none());
    }

    #[test]
    fn suppression_marker_stops_walk() {
        // Host declares `meta: []` ([[type-def meta::au-type-system]] suppression). Parent declares
        // display-meta. Walk halts at host's suppression → None.
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            with_meta(td("note", &[]), vec![meta_block("display-meta")]),
            with_suppressed_meta(td("decision", &["note"])),
        ])
        .graph;
        assert!(lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None).is_none());
    }

    #[test]
    fn host_with_partial_meta_resolves_others_via_walk() {
        // Host declares ONLY runtime-meta; parent declares display-meta.
        // Querying for runtime-meta → host's; querying for display-meta
        // walks up → parent's. Locks the "has metas, different names"
        // case from suppression (which would stop the walk).
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            meta_typedef("runtime-meta"),
            with_meta(td("note", &[]), vec![meta_block("display-meta")]),
            with_meta(td("decision", &["note"]), vec![meta_block("runtime-meta")]),
        ])
        .graph;
        let host_owned = lookup_meta(&g, None, &tn("decision"), &tn("runtime-meta"), None);
        let inherited = lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None);
        assert!(host_owned.is_some());
        assert_eq!(host_owned.unwrap().type_name.as_str(), "runtime-meta");
        assert!(inherited.is_some());
        assert_eq!(inherited.unwrap().type_name.as_str(), "display-meta");
    }

    #[test]
    fn descendant_redeclares_after_suppressing_ancestor() {
        // Chain: note(display-meta) → decision(meta:[]) → committed(display-meta).
        // Query at `committed` for display-meta → committed's own; for
        // runtime-meta → walks up, hits decision's `meta:[]`, returns
        // None. Mirrors [[type-def meta::au-type-system]] "Other names remain suppressed for that
        // subtree until something further down re-declares them."
        let mut display_for_committed = meta_block("display-meta");
        display_for_committed.block_span = ByteRange::new(100, 200);
        let g = build_graph(vec![
            meta_typedef("display-meta"),
            meta_typedef("runtime-meta"),
            with_meta(td("note", &[]), vec![meta_block("display-meta")]),
            with_suppressed_meta(td("decision", &["note"])),
            with_meta(td("committed", &["decision"]), vec![display_for_committed]),
        ])
        .graph;
        let display_at_committed =
            lookup_meta(&g, None, &tn("committed"), &tn("display-meta"), None);
        assert!(display_at_committed.is_some());
        // Verify we got committed's block specifically — span is 100..200,
        // not 0..0 like note's.
        assert_eq!(display_at_committed.unwrap().block_span.start, 100);

        let runtime_at_committed =
            lookup_meta(&g, None, &tn("committed"), &tn("runtime-meta"), None);
        assert!(runtime_at_committed.is_none());
    }

    #[test]
    fn mixin_parents_walk_in_claim_list_order() {
        // Host has `type: [a, b]`; a declares display-meta with span
        // 10..20, b declares display-meta with span 30..40. Per the
        // claim-list-order convention, a's block wins.
        let mut a_block = meta_block("display-meta");
        a_block.block_span = ByteRange::new(10, 20);
        let mut b_block = meta_block("display-meta");
        b_block.block_span = ByteRange::new(30, 40);

        let g = build_graph(vec![
            meta_typedef("display-meta"),
            with_meta(td("a", &[]), vec![a_block]),
            with_meta(td("b", &[]), vec![b_block]),
            td("host", &["a", "b"]),
        ])
        .graph;
        let found = lookup_meta(&g, None, &tn("host"), &tn("display-meta"), None);
        assert!(found.is_some());
        // First parent claim's chain wins — `a`'s span, not `b`'s.
        assert_eq!(found.unwrap().block_span.start, 10);
    }

    #[test]
    fn walk_does_not_recurse_into_encountered_metas() {
        // [[type-def meta::au-type-system]] lock: if the meta-type-def `display-meta` itself carries
        // a `meta:` block for `runtime-meta`, querying the host for
        // `runtime-meta` must NOT return display-meta's runtime-meta.
        // The walk targets the host's TYPE chain, not the meta-type-def's
        // own meta chain.
        let g = build_graph(vec![
            // `display-meta` itself carries a `runtime-meta` sub-region.
            // If the walk wrongly recursed into encountered metas, this
            // would be returned when querying the host for runtime-meta.
            with_meta(
                meta_typedef("display-meta"),
                vec![meta_block("runtime-meta")],
            ),
            meta_typedef("runtime-meta"),
            // Host declares only display-meta; no ancestor declares
            // runtime-meta.
            with_meta(td("decision", &[]), vec![meta_block("display-meta")]),
        ])
        .graph;
        assert!(lookup_meta(&g, None, &tn("decision"), &tn("runtime-meta"), None).is_none());
        // And display-meta should still resolve.
        assert!(lookup_meta(&g, None, &tn("decision"), &tn("display-meta"), None).is_some());
    }

    #[test]
    fn unknown_host_returns_none() {
        // Defensive: looking up a host that isn't in the graph returns
        // None. Callers can pass any TypeName without pre-checking.
        let g = build_graph(vec![meta_typedef("display-meta")]).graph;
        assert!(lookup_meta(&g, None, &tn("nonexistent"), &tn("display-meta"), None).is_none());
    }
}
