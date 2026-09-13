//! Eager per-node referenced-closure identity, the import-tier `TypeId`'s hash
//! half ([[design - cross-repo type vocabulary - reference import and vendor as one spectrum over the repo qualifier]]).
//!
//! Where [`crate::canonical::CanonicalHash`] is a def-LOCAL fingerprint (one
//! type's own declaration), a [`ClosureHash`] folds in the type's WHOLE
//! referenced closure, every transitive parent AND field-referenced type. Two
//! defs with the same `(name, ClosureHash)` are the same type across a repo
//! boundary; a same-named field type that diverges anywhere in the closure
//! changes the hash, so the divergence the parent-only walk missed
//! ([[solved - 2606292147 - cross-repo reference satisfaction misses field-type divergence, the closure is parent-only]])
//! is now caught by id-equality.
//!
//! The identity SEMANTICS equal the flattened referenced-closure signature, the
//! set `{name -> canonical-hash}` over [`crate::closure::referenced_closure_of`].
//! This computes a fingerprint of that set in O(V+E) instead of O(V^2):
//!
//! - the referenced closure can CYCLE (`A.field -> B`, `B.field -> A`; or a
//!   self-reference `person { manager: person* }`), so a strict Merkle fold has
//!   no topological order.
//! - so condense the cycles to a DAG (Tarjan), then hash each component
//!   bottom-up over the order-independent SET of its members' def-local hashes
//!   plus the already-computed closure-ids of its edges to LOWER components.
//! - every member of one component shares the component hash, exactly as the
//!   flattened signature gives every cycle member the same closure set; the
//!   `(name, ClosureHash)` pair still tells them apart.
//!
//! Cross-repo (`::repo`) edges are NOT followed here. `referenced_closure_of`'s
//! collectors skip qualified names, so the own-graph id is over own types only;
//! the fold resolves a peer edge to the peer's id at fold time.

use std::collections::{BTreeMap, BTreeSet};

use crate::canonical::{fnv1a, lp, CanonicalHash};
use crate::closure::collect_referenced_type_names;
use crate::typedef::{TypeDef, TypeName};

/// FNV-1a 64-bit fingerprint of a type-def's REFERENCED CLOSURE.
///
/// Identity is the pair `(name, ClosureHash)`: the name travels beside the hash.
/// Distinct from [`CanonicalHash`] (def-local); this folds in the whole closure.
/// 64-bit and FNV-1a, matching the engine's other provisional content hash, so
/// it is swappable without changing the closure-set semantics it fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClosureHash(pub u64);

/// The own-graph type-defs a single def references: unqualified parents plus
/// every type-def named by a field shape ([`collect_referenced_type_names`]).
/// A `::repo`-qualified parent or shape ref is a peer type the fold resolves,
/// not an own-graph edge, so both collectors drop it. Names that resolve to no
/// def (a dangling ref, or the `file` / `any` sentinels) are still returned, as
/// sink nodes, so the id reflects them exactly as the flattened signature does.
fn referenced_neighbors(td: &TypeDef) -> Vec<TypeName> {
    let mut out = Vec::new();
    for p in &td.parents {
        if !p.is_qualified() {
            out.push(p.name.clone());
        }
    }
    for f in &td.fields {
        if let Ok(shape) = &f.parsed_shape {
            for n in collect_referenced_type_names(shape) {
                out.push(TypeName(n.to_string()));
            }
        }
    }
    // A `required:` obligation references a meta type, folded like a field ref so
    // a divergence in the required meta type's closure changes this def's id
    // ([[spec - required subtype meta - a base obligates every concrete subtype to carry a named meta]]).
    // A `::repo`-qualified target is a peer type the fold resolves, dropped here.
    for r in &td.required_meta {
        if !r.is_qualified() {
            out.push(r.name.clone());
        }
    }
    // A brand's shape references types, a structural union's members or a tuple's
    // record elements, so a member divergence changes the brand's closure id,
    // exactly like a field-referenced type. An enum or scalar shape references
    // none. A `::repo`-qualified member is a peer type the fold resolves, dropped
    // here by `collect_referenced_type_names` like any qualified name. See
    // [[spec - branded types - the shape key names a reusable scalar, enum, union, or tuple]].
    if let Some(brand) = &td.shape {
        for n in collect_referenced_type_names(&brand.shape) {
            out.push(TypeName(n.to_string()));
        }
    }
    out
}

/// Compute every present type-def's [`ClosureHash`], O(V+E), at build.
///
/// The node set is every present def plus every name any def references (sink
/// nodes for danglers / sentinels); edges are [`referenced_neighbors`]. Tarjan
/// condenses cycles and yields components sink-first, so each component's
/// out-edges to lower components are already hashed when it is processed. The
/// returned map holds an entry only for PRESENT defs, mirroring
/// `canonical_hash` (a dangling name has no id), though danglers are hashed
/// internally as the leaves the present ids fold over.
pub(crate) fn compute_closure_ids(
    type_defs: &BTreeMap<TypeName, TypeDef>,
    canonical_hashes: &BTreeMap<TypeName, CanonicalHash>,
) -> BTreeMap<TypeName, ClosureHash> {
    // Adjacency over the full node set. Every present def gets its real
    // neighbor list; a referenced name with no def gets an empty list (a sink).
    // `BTreeMap` keeps a deterministic node order; the result is order-free
    // anyway (the per-component hash sorts its inputs).
    let mut adj: BTreeMap<TypeName, Vec<TypeName>> = BTreeMap::new();
    for (name, td) in type_defs {
        let neighbors = referenced_neighbors(td);
        for n in &neighbors {
            adj.entry(n.clone()).or_default();
        }
        adj.insert(name.clone(), neighbors);
    }

    let sccs = tarjan_sccs(&adj);

    // Hash each component sink-first. A member's id is its component's hash.
    let mut ids: BTreeMap<TypeName, ClosureHash> = BTreeMap::new();
    for scc in &sccs {
        let members: BTreeSet<&TypeName> = scc.iter().collect();

        // Local content: each member's (name, def-local hash or absent),
        // sorted, order-independent within the cycle.
        let mut local: Vec<(&str, Option<u64>)> = scc
            .iter()
            .map(|m| (m.as_str(), canonical_hashes.get(m).map(|h| h.0)))
            .collect();
        local.sort_unstable();

        // Edges leaving the component: the already-computed ids of out-neighbors
        // in lower components. Intra-component edges (the cycle itself) are
        // skipped, the cycle's content lives in `local`. Sorted + deduped, so
        // the set of reachable subtrees is what counts, not which member or how
        // many edges reach each.
        let mut external: Vec<u64> = Vec::new();
        for m in scc {
            for nb in adj.get(m).into_iter().flatten() {
                if !members.contains(nb) {
                    if let Some(id) = ids.get(nb) {
                        external.push(id.0);
                    }
                }
            }
        }
        external.sort_unstable();
        external.dedup();

        let hash = ClosureHash(hash_component(&local, &external));
        for m in scc {
            ids.insert(m.clone(), hash);
        }
    }

    // Keep only present defs, mirroring `canonical_hash`'s None-for-absent.
    ids.retain(|name, _| type_defs.contains_key(name));
    ids
}

/// FNV-1a over a deterministic serialization of one component's identity: its
/// sorted member `(name, def-local-hash?)` pairs and its sorted-deduped set of
/// external out-edge ids. Length-prefixed names and explicit counts keep the
/// framing unambiguous.
fn hash_component(local: &[(&str, Option<u64>)], external: &[u64]) -> u64 {
    let mut out = String::from("cid;M");
    out.push_str(&local.len().to_string());
    out.push(';');
    for (name, hash) in local {
        lp(&mut out, name);
        out.push('=');
        match hash {
            Some(h) => out.push_str(&h.to_string()),
            None => out.push('-'),
        }
        out.push(';');
    }
    out.push('E');
    out.push_str(&external.len().to_string());
    out.push(';');
    for id in external {
        out.push_str(&id.to_string());
        out.push(';');
    }
    fnv1a(out.as_bytes())
}

/// Tarjan's strongly-connected-components over `adj`, returning the components
/// SINK-FIRST: a component is emitted only after every component it has an edge
/// to, so a bottom-up hash can read its out-neighbors' ids.
///
/// Recursive, so DFS depth is bounded by the graph's longest simple path. Type
/// graphs are shallow in practice; a pathologically deep chain would want an
/// explicit-stack rewrite, the swap that does not change the SCC result.
fn tarjan_sccs(adj: &BTreeMap<TypeName, Vec<TypeName>>) -> Vec<Vec<TypeName>> {
    struct State<'a> {
        adj: &'a BTreeMap<TypeName, Vec<TypeName>>,
        index: BTreeMap<TypeName, u32>,
        lowlink: BTreeMap<TypeName, u32>,
        on_stack: BTreeSet<TypeName>,
        stack: Vec<TypeName>,
        next: u32,
        sccs: Vec<Vec<TypeName>>,
    }

    fn strongconnect(st: &mut State, v: &TypeName) {
        st.index.insert(v.clone(), st.next);
        st.lowlink.insert(v.clone(), st.next);
        st.next += 1;
        st.stack.push(v.clone());
        st.on_stack.insert(v.clone());

        for w in st
            .adj
            .get(v)
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
        {
            if !st.index.contains_key(&w) {
                strongconnect(st, &w);
                let low_w = st.lowlink[&w];
                let low_v = st.lowlink[v];
                st.lowlink.insert(v.clone(), low_v.min(low_w));
            } else if st.on_stack.contains(&w) {
                let idx_w = st.index[&w];
                let low_v = st.lowlink[v];
                st.lowlink.insert(v.clone(), low_v.min(idx_w));
            }
        }

        // `v` roots a component: pop the stack down to it.
        if st.lowlink[v] == st.index[v] {
            let mut component = Vec::new();
            loop {
                let w = st.stack.pop().expect("stack holds the component");
                st.on_stack.remove(&w);
                let done = &w == v;
                component.push(w);
                if done {
                    break;
                }
            }
            st.sccs.push(component);
        }
    }

    let mut st = State {
        adj,
        index: BTreeMap::new(),
        lowlink: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        sccs: Vec::new(),
    };
    for v in adj.keys() {
        if !st.index.contains_key(v) {
            strongconnect(&mut st, v);
        }
    }
    st.sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{build_graph, TypeGraph};
    use crate::typedef::parse_type_def;
    use std::path::Path;

    /// Build a graph from `(path, yaml)` pairs; the path derives the type name.
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

    fn cid(g: &TypeGraph, name: &str) -> ClosureHash {
        g.closure_id(&TypeName(name.into()))
            .expect("a present def has a closure id")
    }

    #[test]
    fn present_defs_get_ids_and_a_chain_distinguishes() {
        let g = graph(&[
            ("/v/type/note.type.yaml", "fields:\n  title: String\n"),
            (
                "/v/type/card.type.yaml",
                "type: note\nfields:\n  n: Number\n",
            ),
        ]);
        // Distinct types, distinct ids; the parent extends the closure.
        assert_ne!(cid(&g, "note"), cid(&g, "card"));
        // An absent name has no id, mirroring `canonical_hash`.
        assert_eq!(g.closure_id(&TypeName("ghost".into())), None);
    }

    #[test]
    fn a_divergent_field_type_changes_the_id_though_the_def_local_hash_is_equal() {
        // The soundness property the closure id exists for. Two repos write the
        // same `holder { r: item* }`, but `item` diverges in content. The
        // def-local hash of `holder` is IDENTICAL (its own bytes are the same),
        // yet the closure id MUST differ, the divergence the parent-only walk
        // missed, see [[solved - 2606292147 - ...]].
        let g1 = graph(&[
            ("/v/type/holder.type.yaml", "fields:\n  r: item*\n"),
            ("/v/type/item.type.yaml", "fields:\n  a: String\n"),
        ]);
        let g2 = graph(&[
            ("/v/type/holder.type.yaml", "fields:\n  r: item*\n"),
            ("/v/type/item.type.yaml", "fields:\n  a: Number\n"),
        ]);
        let holder = TypeName("holder".into());
        assert_eq!(
            g1.canonical_hash(&holder),
            g2.canonical_hash(&holder),
            "holder's own def is byte-identical, so its def-local hash matches"
        );
        assert_ne!(
            cid(&g1, "holder"),
            cid(&g2, "holder"),
            "but the divergent `item` must change holder's closure id"
        );
    }

    #[test]
    fn the_abstract_marker_changes_the_closure_id() {
        // `abstract` folds through the def-local canonical form into the closure
        // id, so two same-name defs differing only in the marker are distinct
        // identities, the drift the closure id exists to catch. A concrete def
        // keeps its exact id (append-only-when-declared), covered by canonical.rs.
        let concrete = graph(&[("/v/type/pane.type.yaml", "fields:\n  a: String\n")]);
        let abstract_ = graph(&[(
            "/v/type/pane.type.yaml",
            "abstract: true\nfields:\n  a: String\n",
        )]);
        assert_ne!(cid(&concrete, "pane"), cid(&abstract_, "pane"));
    }

    #[test]
    fn a_required_meta_obligation_folds_into_the_closure_id() {
        // The obligation enters the base's id two ways: its own canonical form
        // (present vs absent), and the referenced meta type's closure folding in
        // (a divergence in the required meta type changes the base's id, though
        // the base's own bytes are unchanged).
        let plain = graph(&[("/v/type/base.type.yaml", "fields:\n  a: String\n")]);
        let obligated = graph(&[
            (
                "/v/type/base.type.yaml",
                "fields:\n  a: String\nmeta:\n  - required: pm\n",
            ),
            ("/v/type/pm.type.yaml", "fields:\n  k: String\n"),
        ]);
        assert_ne!(cid(&plain, "base"), cid(&obligated, "base"));

        // Same base bytes, but the required meta type `pm` diverges.
        let pm_str = graph(&[
            (
                "/v/type/base.type.yaml",
                "fields:\n  a: String\nmeta:\n  - required: pm\n",
            ),
            ("/v/type/pm.type.yaml", "fields:\n  k: String\n"),
        ]);
        let pm_num = graph(&[
            (
                "/v/type/base.type.yaml",
                "fields:\n  a: String\nmeta:\n  - required: pm\n",
            ),
            ("/v/type/pm.type.yaml", "fields:\n  k: Number\n"),
        ]);
        assert_eq!(
            pm_str.canonical_hash(&TypeName("base".into())),
            pm_num.canonical_hash(&TypeName("base".into())),
            "base's own def bytes are identical"
        );
        assert_ne!(
            cid(&pm_str, "base"),
            cid(&pm_num, "base"),
            "but the divergent required meta type must change base's id"
        );
    }

    #[test]
    fn identical_defs_across_graphs_share_a_closure_id() {
        // The dedup positive: the same `holder { r: item* }` over the same
        // `item` yields the same id in two separate graphs. This is the
        // cross-repo "same type" the fold dedups on.
        let defs: &[(&str, &str)] = &[
            ("/v/type/holder.type.yaml", "fields:\n  r: item*\n"),
            ("/v/type/item.type.yaml", "fields:\n  a: String\n"),
        ];
        assert_eq!(cid(&graph(defs), "holder"), cid(&graph(defs), "holder"));
    }

    #[test]
    fn a_mutual_cycle_terminates_and_members_share_an_id() {
        // `pa.field -> pb`, `pb.field -> pa`. The referenced closure of each is
        // the same set {pa, pb}, so the flattened signatures are equal and the
        // ids match, even though pa and pb are distinct types. The `(name, id)`
        // pair still tells them apart. The build terminating proves cycle-safety.
        let g = graph(&[
            ("/v/type/pa.type.yaml", "fields:\n  o: pb*\n"),
            ("/v/type/pb.type.yaml", "fields:\n  o: pa*\n"),
        ]);
        assert_eq!(
            cid(&g, "pa"),
            cid(&g, "pb"),
            "both cycle members share the component closure id"
        );
    }

    #[test]
    fn a_self_reference_terminates_and_has_an_id() {
        // `node { next: node* }` is a singleton SCC with a self-edge; the build
        // must not loop, and the id exists.
        let g = graph(&[("/v/type/node.type.yaml", "fields:\n  next: node*\n")]);
        assert!(g.closure_id(&TypeName("node".into())).is_some());
    }

    #[test]
    fn a_self_reference_id_reflects_its_own_field_divergence() {
        // Two `node` defs that both self-reference but differ in another field
        // must get different ids; the self-edge does not mask the divergence.
        let g1 = graph(&[(
            "/v/type/node.type.yaml",
            "fields:\n  next: node*\n  a: String\n",
        )]);
        let g2 = graph(&[(
            "/v/type/node.type.yaml",
            "fields:\n  next: node*\n  a: Number\n",
        )]);
        assert_ne!(cid(&g1, "node"), cid(&g2, "node"));
    }

    #[test]
    fn an_id_depends_only_on_its_own_closure_not_the_rest_of_the_graph() {
        // A type's closure id is a function of its OWN referenced closure, not
        // the whole graph: adding an unrelated type must not change it. This is
        // what lets the fold dedup a peer's type by id regardless of what else
        // each repo holds, and it underpins the incremental-safety argument, a
        // graph that grows elsewhere leaves an untouched closure's id stable.
        let lean = graph(&[
            ("/v/type/holder.type.yaml", "fields:\n  r: item*\n"),
            ("/v/type/item.type.yaml", "fields:\n  a: String\n"),
        ]);
        let plus_unrelated = graph(&[
            ("/v/type/holder.type.yaml", "fields:\n  r: item*\n"),
            ("/v/type/item.type.yaml", "fields:\n  a: String\n"),
            ("/v/type/unrelated.type.yaml", "fields:\n  z: Number\n"),
        ]);
        assert_eq!(
            cid(&lean, "holder"),
            cid(&plus_unrelated, "holder"),
            "an unrelated type must not perturb holder's closure id"
        );
    }

    #[test]
    fn a_structural_brand_member_divergence_changes_its_closure_id() {
        // A brand `shape: <a | b>` references record types `a` and `b`, so a
        // divergence in a member changes the brand's closure id, though the
        // brand's own def bytes are unchanged. The soundness the closure id gives
        // fields, extended to a brand's shape.
        let g1 = graph(&[
            ("/v/type/ek.type.yaml", "shape: <a | b>\n"),
            ("/v/type/a.type.yaml", "fields:\n  x: String\n"),
            ("/v/type/b.type.yaml", "fields:\n  y: String\n"),
        ]);
        let g2 = graph(&[
            ("/v/type/ek.type.yaml", "shape: <a | b>\n"),
            ("/v/type/a.type.yaml", "fields:\n  x: Number\n"),
            ("/v/type/b.type.yaml", "fields:\n  y: String\n"),
        ]);
        assert_eq!(
            g1.canonical_hash(&TypeName("ek".into())),
            g2.canonical_hash(&TypeName("ek".into())),
            "ek's own def bytes are identical"
        );
        assert_ne!(
            cid(&g1, "ek"),
            cid(&g2, "ek"),
            "but a divergent union member must change ek's closure id"
        );
    }

    #[test]
    fn a_tuple_of_records_brand_folds_its_members() {
        // A tuple `(a, b)` references record types `a` and `b`, so a member
        // divergence changes the brand's closure id, like a union.
        let g1 = graph(&[
            ("/v/type/rect.type.yaml", "shape: (a, b)\n"),
            ("/v/type/a.type.yaml", "fields:\n  x: String\n"),
            ("/v/type/b.type.yaml", "fields:\n  y: String\n"),
        ]);
        let g2 = graph(&[
            ("/v/type/rect.type.yaml", "shape: (a, b)\n"),
            ("/v/type/a.type.yaml", "fields:\n  x: Number\n"),
            ("/v/type/b.type.yaml", "fields:\n  y: String\n"),
        ]);
        assert_eq!(
            g1.canonical_hash(&TypeName("rect".into())),
            g2.canonical_hash(&TypeName("rect".into())),
            "rect's own def bytes are identical"
        );
        assert_ne!(
            cid(&g1, "rect"),
            cid(&g2, "rect"),
            "but a divergent tuple member must change rect's closure id"
        );
    }

    #[test]
    fn an_enum_brand_references_no_types() {
        // A scalar or enum brand references no type-defs, so its closure id is a
        // function of its own canonical form only, not the rest of the graph.
        let lean = graph(&[("/v/type/q.type.yaml", "shape:\n  - a\n  - b\n")]);
        let plus = graph(&[
            ("/v/type/q.type.yaml", "shape:\n  - a\n  - b\n"),
            ("/v/type/unrelated.type.yaml", "fields:\n  z: Number\n"),
        ]);
        assert_eq!(cid(&lean, "q"), cid(&plus, "q"));
    }

    #[test]
    fn build_is_deterministic_and_input_order_independent() {
        // The id is a pure function of the graph: the same defs in either input
        // order yield the same ids (the byte-identity property C4 leans on).
        let a = ("/v/type/note.type.yaml", "fields:\n  title: String\n");
        let b = (
            "/v/type/card.type.yaml",
            "type: note\nfields:\n  r: note*\n",
        );
        let g_ab = graph(&[a, b]);
        let g_ba = graph(&[b, a]);
        assert_eq!(cid(&g_ab, "note"), cid(&g_ba, "note"));
        assert_eq!(cid(&g_ab, "card"), cid(&g_ba, "card"));
    }
}
