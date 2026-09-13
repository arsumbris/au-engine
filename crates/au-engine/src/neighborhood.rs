//! The bounded N-hop reference-graph walk behind the `neighborhood` read.
//!
//! One verb over the reference graph: direction (`out` / `in` / `both`) and
//! depth are arguments, so the whole axis is one walk rather than a verb per
//! shape. It returns a SUBGRAPH, the reachable nodes plus the edges traversed
//! between them, not an edge list.
//!
//! The traversal folds the two one-hop primitives, so it cannot disagree with
//! them: an outbound hop is [`crate::backlinks::walk_edges`] (the same source of
//! truth `references_out` projects), an inbound hop is the backlink index (the
//! same [`crate::ir::KnowledgeBase::backlinks`] `references_in` reads). Every edge is
//! classified by the shared coarse vocabulary,
//! [`crate::backlinks::coarse_edge_kind`], so a `kinds` filter means the same
//! thing whichever way the walk runs. The one outbound-only exception is a
//! commit-referent (`[[::@sha]]`), which reaches no node and carries its own
//! `commit-referent` kind rather than reading as a dangling edge. The filter
//! vocabulary is [`crate::backlinks::WALK_KINDS`].
//!
//! See [[spec - neighborhood read - a bounded n-hop reference walk returning a
//! subgraph of files and addressable blocks]].

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use au_diagnostics::ByteRange;
use au_parser::{scan_body, BodyEvent};

use crate::backlinks::{coarse_edge_kind, walk_edges, RefSurface};
use crate::ir::KnowledgeBase;
use crate::parse::{served_file_kind, FileParse};
use crate::repo::RepoName;

/// Which way the walk follows edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges the node authors, `references_out`'s axis.
    Out,
    /// Edges pointing AT the node, `references_in`'s axis.
    In,
    /// Either direction, per hop. A true undirected walk, so depth 2 reaches a
    /// co-cited sibling that neither directed walk alone reaches.
    Both,
}

/// A node identity in the walk: a file, or an addressable block within it.
///
/// A block node exists only as the resolved target of a `^^` block-referent
/// edge; a bare `^id` reaches the FILE node, its anchor riding the edge. So the
/// visited set keys on the pair, and a file node and a block node inside it are
/// distinct, never deduped against each other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId {
    pub path: PathBuf,
    /// `Some` for a block node, the `^^`-addressed block; `None` for a file node.
    pub block_id: Option<String>,
}

/// One reached node, plus the identity and size a consumer costs a fetch by.
#[derive(Debug, Clone)]
pub struct WalkNode {
    pub id: NodeId,
    /// The MINIMUM hop count from the seed.
    pub depth: usize,
    pub repo: Option<RepoName>,
    /// The parse kind, `instance` / `type-def` / `note` / `asset`, matching
    /// `hubs` and `files`. See [`crate::parse::served_file_kind`].
    pub file_kind: &'static str,
    /// Whole-file byte length, what `content` returns; `None` for an unread
    /// asset or an unheld file. For a block node it is the block's SPAN length,
    /// the size of the slice a `content` fetch returns.
    pub byte_len: Option<usize>,
    /// Prose-body byte length, what `body` returns; `None` for a pure-YAML or
    /// type-def file with no markdown body. For a block node it equals
    /// `byte_len`, the span length (a block has no frontmatter to strip).
    pub body_bytes: Option<usize>,
    /// The block's byte range in its file, `Some` only for a block node whose
    /// `^^` id resolved to a record or a fenced block. The enrichment phase
    /// slices `content` / `body` from it; sizing reads its length. `None` for a
    /// file node, or a block whose id did not resolve.
    pub block_span: Option<ByteRange>,
}

/// One traversed edge, stored in its NATURAL direction (`from` the referrer,
/// `to` the target). An inbound hop and an outbound hop of the same physical
/// link produce the same natural edge, so `both` reports it once.
#[derive(Debug, Clone)]
pub struct WalkEdge {
    pub from: NodeId,
    /// The target node; `None` for a dangling edge (an outbound link resolving
    /// to nothing). An inbound edge is always resolved, so never `None`.
    pub to: Option<NodeId>,
    pub kind: &'static str,
    pub surface: RefSurface,
    /// Byte range of the reference, in `from.path` (the referrer).
    pub span: ByteRange,
    /// The slot / `:field` attribution the edge fills.
    pub field: Option<String>,
    /// The `^:` id of the enclosing inline record the edge sits in, if any.
    pub source_block_id: Option<String>,
    /// The block-id fragment ON THE LINK, coupling id with `^` / `^^` mode.
    pub target_block_id: Option<au_references::BlockId>,
    /// The parsed link, present for an OUTBOUND-discovered edge (the rich
    /// `references_out` fragments: `repo`, `commit`, `anchor`, `target`).
    /// `None` for an inbound-discovered edge, whose index dropped the link;
    /// under `both` the outbound projection is kept when the two collide.
    pub link: Option<au_references::WikilinkRef>,
}

/// A node the walk reached but did not return, cut by `max_nodes`. Named, not
/// counted, so a consumer can surface or re-fetch exactly what was lost.
#[derive(Debug, Clone)]
pub struct DroppedNode {
    pub id: NodeId,
    /// The depth it would have carried.
    pub depth: usize,
    pub repo: Option<RepoName>,
}

/// The walk result: the reachable subgraph, plus the truncation report.
#[derive(Debug, Clone, Default)]
pub struct Neighborhood {
    /// Reached nodes, sorted by `(depth, path, block_id)`, so two builds of one
    /// knowledge base return one answer.
    pub nodes: Vec<WalkNode>,
    /// Traversed edges, sorted by `(from, span)`.
    pub edges: Vec<WalkEdge>,
    /// True when `max_nodes` cut an expansion that had more.
    pub truncated: bool,
    /// The depth at which cutting began; `None` when not truncated.
    pub truncated_at_depth: Option<usize>,
    /// The cut nodes, named. Every `edge.to` absent from `nodes` under
    /// truncation appears here.
    pub dropped: Vec<DroppedNode>,
}

/// The walk parameters, resolved from the wire args by the caller.
pub struct WalkParams<'a> {
    pub direction: Direction,
    /// Maximum hop count. Depth 1 is the seed plus its direct targets.
    pub depth: usize,
    /// The edge-kind include set; `None` means all kinds. The caller enforces
    /// "required past depth 1"; the walk just honors what it is given.
    pub kinds: Option<&'a BTreeSet<String>>,
    /// `true` prunes the walk at the repo boundary: a crossing edge is reported
    /// but its target, in a non-editable (dependency) repo, is not expanded.
    pub scope_own: bool,
    /// The node cap. The seed counts, so `max_nodes` 1 returns the seed alone.
    pub max_nodes: usize,
}

/// Walk the reference graph from `seed`, returning the reachable subgraph.
///
/// `seed` is always a FILE node. A missing or unheld seed yields an empty
/// neighborhood (the one seed node with null sizes), never an error.
pub fn walk(kb: &KnowledgeBase, seed: &std::path::Path, params: &WalkParams) -> Neighborhood {
    let seed_id = NodeId {
        path: seed.to_path_buf(),
        block_id: None,
    };

    // node -> minimum depth. Seeded at 0; the seed is never scope-pruned.
    let mut visited: BTreeMap<NodeId, usize> = BTreeMap::new();
    visited.insert(seed_id.clone(), 0);

    // Edges deduped by (from.path, span), so `both` reports one natural edge
    // once. On collision the richer outbound projection (link.is_some()) wins.
    let mut edges: BTreeMap<(PathBuf, usize, usize), WalkEdge> = BTreeMap::new();
    let mut dropped: BTreeMap<NodeId, usize> = BTreeMap::new();
    let mut truncated_at_depth: Option<usize> = None;

    let mut frontier = vec![seed_id];
    // Expand depth 0..depth-1; targets land at depth 1..depth. A node at
    // `depth` is a leaf, discovered but never expanded.
    for cur_depth in 0..params.depth {
        let mut next: Vec<NodeId> = Vec::new();
        for node in &frontier {
            for (cand, far) in edges_of(kb, node, params.direction) {
                // Kind filter. Absent set means all kinds.
                if let Some(set) = params.kinds {
                    if !set.contains(cand.kind) {
                        continue;
                    }
                }
                let key = (cand.from.path.clone(), cand.span.start, cand.span.end);
                // Record the edge, preferring the outbound (link-bearing) view.
                match edges.get(&key) {
                    Some(existing) if existing.link.is_some() => {}
                    _ => {
                        edges.insert(key, cand);
                    }
                }
                // The node reached by traversing this edge FROM the current one:
                // the target for an outbound edge, the referrer for an inbound
                // one. `None` is a dangling outbound edge, no node to advance to.
                let Some(far) = far else {
                    continue;
                };
                if visited.contains_key(&far) || dropped.contains_key(&far) {
                    continue; // already reached; keep its minimum depth
                }
                // Scope boundary: a crossing edge is reported, its peer target
                // not expanded. Not a budget cut, so not `dropped`.
                if params.scope_own && !node_in_own_scope(kb, &far) {
                    continue;
                }
                if visited.len() >= params.max_nodes {
                    dropped.entry(far.clone()).or_insert(cur_depth + 1);
                    truncated_at_depth.get_or_insert(cur_depth + 1);
                    continue;
                }
                visited.insert(far.clone(), cur_depth + 1);
                next.push(far);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    // Project the visited set and dropped set to sorted node lists.
    let mut nodes: Vec<WalkNode> = visited
        .iter()
        .map(|(id, &depth)| node_view(kb, id, depth))
        .collect();
    nodes.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.id.cmp(&b.id)));

    let mut edges: Vec<WalkEdge> = edges.into_values().collect();
    edges.sort_by(|a, b| {
        (&a.from, a.span.start, a.span.end).cmp(&(&b.from, b.span.start, b.span.end))
    });

    let mut dropped: Vec<DroppedNode> = dropped
        .into_iter()
        .map(|(id, depth)| DroppedNode {
            repo: kb.repos.repo_of(&id.path).map(|r| r.name.clone()),
            id,
            depth,
        })
        .collect();
    dropped.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.id.cmp(&b.id)));

    Neighborhood {
        nodes,
        edges,
        truncated: !dropped.is_empty(),
        truncated_at_depth,
        dropped,
    }
}

/// The candidate edges of one node, each paired with the FAR endpoint reached by
/// traversing it from this node: the target for an outbound edge, the referrer
/// for an inbound one, `None` for a dangling outbound edge. `Both` concatenates
/// the two; the caller dedups the edges by `(from, span)`.
fn edges_of(
    kb: &KnowledgeBase,
    node: &NodeId,
    direction: Direction,
) -> Vec<(WalkEdge, Option<NodeId>)> {
    match direction {
        Direction::Out => outbound_edges(kb, node),
        Direction::In => inbound_edges(kb, node),
        Direction::Both => {
            let mut out = outbound_edges(kb, node);
            out.extend(inbound_edges(kb, node));
            out
        }
    }
}

/// The edges `node` authors. A file node expands every edge of the file; a block
/// node only the edges that physically sit inside that block (their
/// `source_block_id` matches). A `^^` link mints a block target, a bare `^id`
/// resolves to the file node with its anchor on the edge.
fn outbound_edges(kb: &KnowledgeBase, node: &NodeId) -> Vec<(WalkEdge, Option<NodeId>)> {
    let Some(parse) = kb.file_parse(&node.path) else {
        return Vec::new();
    };
    walk_edges(&node.path, parse, &kb.indexes, &kb.repos, &kb.workspaces)
        .into_iter()
        .filter(|e| match &node.block_id {
            // Block node: only edges living inside this block.
            Some(bid) => e.source_block_id.as_deref() == Some(bid.as_str()),
            // File node: the whole file, blocks included.
            None => true,
        })
        .map(|e| {
            let to = e.resolved.as_ref().map(|tp| NodeId {
                path: tp.clone(),
                // A `^^` block-referent addresses the block; a bare `^id` (or none)
                // addresses the file.
                block_id: match &e.link.block_id {
                    Some(b) if b.referent => Some(b.id.clone()),
                    _ => None,
                },
            });
            let edge = WalkEdge {
                from: node.clone(),
                to: to.clone(),
                // A commit-referent (`[[::@sha]]`) names a commit, not a node: it
                // has no `to`, but it is NOT dangling. Its own kind keeps it from
                // reading as a broken outbound edge. Settled by the link, not by
                // surface + slot, so it is not a `coarse_edge_kind` output.
                kind: if e.link.is_commit_referent() {
                    "commit-referent"
                } else {
                    coarse_edge_kind(e.surface, e.slot.as_deref())
                },
                surface: e.surface,
                span: e.span,
                field: e.slot.clone(),
                source_block_id: e.source_block_id.clone(),
                target_block_id: e.link.block_id.clone(),
                link: Some(e.link),
            };
            // Advance to the target (the far endpoint of an outbound edge).
            (edge, to)
        })
        .collect()
}

/// The edges pointing AT `node`. The backlink index keeps only resolved edges,
/// so an inbound edge is never dangling. A backlink attaches to the block node
/// when it named that block with `^^`; a bare `^id` (or no `^`) attaches to the
/// file node. The source of an inbound edge is always a file node.
fn inbound_edges(kb: &KnowledgeBase, node: &NodeId) -> Vec<(WalkEdge, Option<NodeId>)> {
    kb.backlinks(&node.path)
        .iter()
        .filter(|bl| match &node.block_id {
            // Block node: only `^^` links naming this block.
            Some(bid) => bl
                .block_id
                .as_ref()
                .is_some_and(|b| b.referent && b.id == *bid),
            // File node: links with no `^`, or a bare `^id` whose referent is
            // the file. A `^^` link belongs to the block node, not here.
            None => bl.block_id.as_ref().map_or(true, |b| !b.referent),
        })
        .map(|bl| {
            let from = NodeId {
                path: bl.source.clone(),
                block_id: None,
            };
            let edge = WalkEdge {
                from: from.clone(),
                to: Some(node.clone()),
                kind: coarse_edge_kind(bl.surface, bl.slot.as_deref()),
                surface: bl.surface,
                span: bl.span,
                field: bl.slot.clone(),
                source_block_id: bl.source_block_id.clone(),
                target_block_id: bl.block_id.clone(),
                link: None,
            };
            // Advance to the referrer (the far endpoint of an inbound edge).
            (edge, Some(from))
        })
        .collect()
}

/// The reached-node projection: identity, depth, owner, kind, and the two sizes
/// a consumer costs a fetch by. A block node's sizes are its resolved span
/// length; a file node's are the recorded byte length and the parse body's.
fn node_view(kb: &KnowledgeBase, id: &NodeId, depth: usize) -> WalkNode {
    let parse = kb.file_parse(&id.path);
    // File-node sizes come free from the catalog and the parse: the recorded
    // byte length, and the markdown body's length (what `content` / `body`
    // return). A block node measures its resolved SPAN, in-memory, no disk read.
    let (byte_len, body_bytes, block_span) = match &id.block_id {
        Some(bid) => {
            let span = block_span(kb, &id.path, bid);
            let len = span.map(|s| s.end - s.start);
            (len, len, span)
        }
        None => {
            let byte_len = kb.catalog.get(&id.path).and_then(|e| e.byte_len);
            let body_bytes = parse
                .and_then(|p| p.markdown_body())
                .map(|(body, _)| body.len());
            (byte_len, body_bytes, None)
        }
    };
    WalkNode {
        id: id.clone(),
        depth,
        repo: kb.repos.repo_of(&id.path).map(|r| r.name.clone()),
        file_kind: served_file_kind(parse),
        byte_len,
        body_bytes,
        block_span,
    }
}

/// The byte range of a `^^`-addressed block in its file: an inline record's
/// span (frontmatter, checked first, mirroring `resolve_block_id_view`), else a
/// fenced block's span in the body. `None` when the id resolves to neither (an
/// unresolved or bare-marker id). In-memory over the parse, no disk read.
pub(crate) fn block_span(
    kb: &KnowledgeBase,
    path: &std::path::Path,
    block_id: &str,
) -> Option<ByteRange> {
    if let Some(FileParse::Instance {
        instance: Some(inst),
        ..
    }) = kb.file_parse(path)
    {
        if let Some(record) =
            crate::resolution_build::record_targets_of_kb(kb, path, inst).get(block_id)
        {
            return Some(record.span);
        }
    }
    let (body, body_offset) = kb.file_parse(path)?.markdown_body()?;
    for ev in scan_body(body) {
        if let BodyEvent::FencedBlock {
            span,
            trailing_block_id: Some(id),
            ..
        } = ev
        {
            if id == block_id {
                return Some(ByteRange::new(
                    span.start + body_offset,
                    span.end + body_offset,
                ));
            }
        }
    }
    None
}

/// Whether a node sits in the user's OWN (editable) repo, the `scope: own`
/// predicate. A node whose path belongs to no repo is treated as own (a
/// degenerate case), never silently pruned.
fn node_in_own_scope(kb: &KnowledgeBase, node: &NodeId) -> bool {
    match kb.repos.repo_of(&node.path) {
        Some(r) => crate::wire::member_role(kb, &r.name).editable(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Params with sane defaults: out, the given depth, all kinds, all repos, no
    /// practical node cap.
    fn params(direction: Direction, depth: usize) -> WalkParams<'static> {
        WalkParams {
            direction,
            depth,
            kinds: None,
            scope_own: false,
            max_nodes: 10_000,
        }
    }

    /// The reached node paths as basenames, for compact assertions.
    fn node_names(n: &Neighborhood) -> Vec<String> {
        n.nodes
            .iter()
            .map(|w| {
                let base = w.id.path.file_name().unwrap().to_string_lossy().to_string();
                match &w.id.block_id {
                    Some(b) => format!("{base}^^{b}"),
                    None => base,
                }
            })
            .collect()
    }

    fn depth_of(n: &Neighborhood, base: &str) -> usize {
        n.nodes
            .iter()
            .find(|w| w.id.path.file_name().unwrap() == base)
            .unwrap_or_else(|| panic!("{base} in nodes: {:?}", node_names(n)))
            .depth
    }

    /// A knowledge base whose notes link in a chain a → b → c, a diamond, a cycle, and a
    /// hub, plus a dangling link and a typed field edge. One `note` type with a
    /// `ref` slot so a frontmatter edge is `field`.
    fn linked_kb() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  ref?: note*\n".to_vec(),
        );
        let note = |body: &str| format!("---\ntype: note\n---\n{body}\n").into_bytes();
        // Chain: a -> b -> c (body prose links, navigational).
        fs.insert("/v/a.md", note("See [[b]]."));
        fs.insert("/v/b.md", note("See [[c]]."));
        fs.insert("/v/c.md", note("A leaf."));
        // Diamond: d -> e, d -> f, e -> g, f -> g.
        fs.insert("/v/d.md", note("[[e]] and [[f]]."));
        fs.insert("/v/e.md", note("[[g]]."));
        fs.insert("/v/f.md", note("[[g]]."));
        fs.insert("/v/g.md", note("A sink."));
        // Cycle: p -> q -> p.
        fs.insert("/v/p.md", note("[[q]]."));
        fs.insert("/v/q.md", note("[[p]]."));
        // Dangling: dl -> nowhere.
        fs.insert("/v/dl.md", note("[[nowhere]]."));
        // A frontmatter field edge: fr.ref -> a (a `field` kind).
        fs.insert(
            "/v/fr.md",
            b"---\ntype: note\nref: \"[[a]]\"\n---\nAlso [[b]] in prose.\n".to_vec(),
        );
        build(Path::new("/v"), &fs).unwrap()
    }

    #[test]
    fn depth_bounds_the_chain() {
        let v = linked_kb();
        let d1 = walk(&v, Path::new("/v/a.md"), &params(Direction::Out, 1));
        assert_eq!(node_names(&d1), vec!["a.md", "b.md"], "depth 1 stops at b");

        let d2 = walk(&v, Path::new("/v/a.md"), &params(Direction::Out, 2));
        let mut got = node_names(&d2);
        got.sort();
        assert_eq!(got, vec!["a.md", "b.md", "c.md"], "depth 2 reaches c");
        assert_eq!(depth_of(&d2, "c.md"), 2, "c is two hops out");
    }

    #[test]
    fn a_diamond_visits_the_sink_once_at_min_depth() {
        let v = linked_kb();
        let n = walk(&v, Path::new("/v/d.md"), &params(Direction::Out, 2));
        let g_count = n
            .nodes
            .iter()
            .filter(|w| w.id.path.ends_with("g.md"))
            .count();
        assert_eq!(g_count, 1, "the sink appears once: {:?}", node_names(&n));
        assert_eq!(depth_of(&n, "g.md"), 2, "g's minimum depth is 2");
        // Both edges into g are reported (e->g and f->g).
        let into_g = n
            .edges
            .iter()
            .filter(|e| e.to.as_ref().is_some_and(|t| t.path.ends_with("g.md")))
            .count();
        assert_eq!(into_g, 2, "both diamond edges into g are traversed");
    }

    #[test]
    fn a_cycle_terminates_and_reports_its_closing_edge() {
        let v = linked_kb();
        let n = walk(&v, Path::new("/v/p.md"), &params(Direction::Out, 10));
        let mut got = node_names(&n);
        got.sort();
        assert_eq!(got, vec!["p.md", "q.md"], "the cycle terminates");
        // The closing edge q -> p is present though p is already visited.
        assert!(
            n.edges.iter().any(|e| e.from.path.ends_with("q.md")
                && e.to.as_ref().is_some_and(|t| t.path.ends_with("p.md"))),
            "q -> p closing edge is reported"
        );
    }

    #[test]
    fn a_dangling_edge_is_reported_with_no_target() {
        let v = linked_kb();
        let n = walk(&v, Path::new("/v/dl.md"), &params(Direction::Out, 1));
        assert_eq!(
            node_names(&n),
            vec!["dl.md"],
            "no node for the missing target"
        );
        let dangling: Vec<_> = n.edges.iter().filter(|e| e.to.is_none()).collect();
        assert_eq!(dangling.len(), 1, "one dangling edge: {:?}", n.edges.len());
    }

    #[test]
    fn a_commit_referent_edge_carries_its_own_kind_not_a_dangling_coarse_one() {
        // `[[::@sha]]` names a commit, not a node: the outbound edge reaches no
        // target (`to` is None) but carries kind `commit-referent`, so a
        // consumer does not read it as the broken link a coarse `navigational`
        // + null `to` would suggest.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  ref?: note*\n".to_vec(),
        );
        fs.insert(
            "/v/span.md",
            b"---\ntype: note\n---\nProduced [[::@a1b2c3d]].\n".to_vec(),
        );
        let v = build(Path::new("/v"), &fs).unwrap();

        let n = walk(&v, Path::new("/v/span.md"), &params(Direction::Out, 1));
        assert_eq!(node_names(&n), vec!["span.md"], "no node for a commit");
        let cr: Vec<_> = n
            .edges
            .iter()
            .filter(|e| e.kind == "commit-referent")
            .collect();
        assert_eq!(cr.len(), 1, "one commit-referent edge: {:?}", n.edges);
        assert!(cr[0].to.is_none(), "a commit-referent reaches no node");
    }

    #[test]
    fn inbound_finds_referrers() {
        let v = linked_kb();
        // Who points at b? a (prose) and fr (prose "Also [[b]]").
        let n = walk(&v, Path::new("/v/b.md"), &params(Direction::In, 1));
        let mut got = node_names(&n);
        got.sort();
        assert_eq!(got, vec!["a.md", "b.md", "fr.md"], "b's referrers: {got:?}");
    }

    #[test]
    fn both_reaches_a_co_cited_sibling_at_depth_two() {
        let v = linked_kb();
        // e and f both point at g. From e: out to g (depth 1), then IN from f
        // (depth 2). A directed out-walk never reaches f.
        let n = walk(&v, Path::new("/v/e.md"), &params(Direction::Both, 2));
        assert!(
            n.nodes.iter().any(|w| w.id.path.ends_with("f.md")),
            "both-walk reaches co-cited f: {:?}",
            node_names(&n)
        );
        let out_only = walk(&v, Path::new("/v/e.md"), &params(Direction::Out, 2));
        assert!(
            !out_only.nodes.iter().any(|w| w.id.path.ends_with("f.md")),
            "a directed out-walk does NOT reach f"
        );
    }

    #[test]
    fn the_kind_filter_selects_edges() {
        let v = linked_kb();
        // fr authors a `field` edge (ref -> a) and a `navigational` edge (-> b).
        let mut only_field = BTreeSet::new();
        only_field.insert("field".to_string());
        let p = WalkParams {
            kinds: Some(&only_field),
            ..params(Direction::Out, 1)
        };
        let n = walk(&v, Path::new("/v/fr.md"), &p);
        let mut got = node_names(&n);
        got.sort();
        assert_eq!(
            got,
            vec!["a.md", "fr.md"],
            "only the field edge to a: {got:?}"
        );
        assert!(
            n.edges.iter().all(|e| e.kind == "field"),
            "every kept edge is `field`"
        );
    }

    #[test]
    fn max_nodes_truncates_loudly_and_names_the_cut() {
        let v = linked_kb();
        // d -> e, d -> f at depth 1. Cap at 2 keeps seed + one target, drops the other.
        let p = WalkParams {
            max_nodes: 2,
            ..params(Direction::Out, 1)
        };
        let n = walk(&v, Path::new("/v/d.md"), &p);
        assert_eq!(n.nodes.len(), 2, "seed plus one target");
        assert!(n.truncated, "truncated flag set");
        assert_eq!(n.truncated_at_depth, Some(1), "cut began at depth 1");
        assert_eq!(n.dropped.len(), 1, "one node named as dropped");
        // The dropped node's edge is still reported: a `to` absent from nodes.
        let dropped_id = &n.dropped[0].id;
        assert!(
            !n.nodes.iter().any(|w| &w.id == dropped_id),
            "the dropped node is not in nodes"
        );
        assert!(
            n.edges.iter().any(|e| e.to.as_ref() == Some(dropped_id)),
            "an edge names the dropped node as its target"
        );
    }

    #[test]
    fn reaching_depth_is_not_truncation() {
        let v = linked_kb();
        let n = walk(&v, Path::new("/v/a.md"), &params(Direction::Out, 1));
        assert!(!n.truncated, "a full depth-1 walk is not truncated");
        assert_eq!(n.truncated_at_depth, None);
        assert!(n.dropped.is_empty());
    }

    #[test]
    fn a_file_node_carries_its_content_length() {
        let v = linked_kb();
        let n = walk(&v, Path::new("/v/c.md"), &params(Direction::Out, 0));
        let seed = &n.nodes[0];
        assert_eq!(seed.id.path.file_name().unwrap(), "c.md");
        let disk = b"---\ntype: note\n---\nA leaf.\n".len();
        assert_eq!(seed.byte_len, Some(disk), "byte_len is the whole file");
        assert_eq!(
            seed.body_bytes,
            Some("A leaf.\n".len()),
            "body_bytes is the prose"
        );
        assert_eq!(seed.file_kind, "instance", "a typed note is an instance");
    }

    #[test]
    fn the_walk_is_deterministic() {
        let v = linked_kb();
        let a = walk(&v, Path::new("/v/d.md"), &params(Direction::Both, 3));
        let b = walk(&v, Path::new("/v/d.md"), &params(Direction::Both, 3));
        assert_eq!(node_names(&a), node_names(&b), "node order is stable");
        assert_eq!(a.edges.len(), b.edges.len(), "edge set is stable");
    }

    /// `block_span` resolves both addressable surfaces: an inline record's `^:`
    /// id (frontmatter) and a fenced block's trailing `^id` (body). The socket
    /// tests cover the fence path end-to-end; this covers the record path.
    #[test]
    fn block_span_resolves_a_record_and_a_fence() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  inner?: assumption[]\n".to_vec(),
        );
        fs.insert(
            "/v/type/assumption.type.yaml",
            b"fields:\n  what: String\n".to_vec(),
        );
        // An inline record carrying `^: rec` in frontmatter, plus a fenced block.
        fs.insert(
            "/v/host.md",
            b"---\ntype: note\ninner:\n  - ^: rec\n    type: assumption\n    what: x\n---\n\n```yaml [:inner]\ntype: assumption\nwhat: y\n```\n^fen\n".to_vec(),
        );
        let v = build(Path::new("/v"), &fs).unwrap();

        let rec = block_span(&v, Path::new("/v/host.md"), "rec").expect("record span");
        assert!(rec.end > rec.start, "the record span is non-empty");
        let fen = block_span(&v, Path::new("/v/host.md"), "fen").expect("fence span");
        assert!(fen.end > fen.start, "the fence span is non-empty");
        // Distinct blocks, distinct spans.
        assert_ne!((rec.start, rec.end), (fen.start, fen.end));
        // An absent id resolves to nothing.
        assert!(block_span(&v, Path::new("/v/host.md"), "ghost").is_none());
    }
}
