//! Immutable type-graph: type-defs keyed by name, plus reverse children index.
//!
//! One graph holds one repo's own vocabulary. Each repo resolves against its
//! own graph; cross-repo sameness and the per-name site set are a composition
//! over the per-repo graphs, not a property of any one graph. So a single graph
//! never compares same-name defs across a repo boundary — within a repo a name
//! resolves first-wins, and a second def of the same name is a
//! `duplicate-type-def`.
//!
//! Semantic load checks (regex, redeclare, sealed reachability) read from
//! this graph; see `load_checks`. Mutation after construction is unsupported
//! by design — edits produce a fresh graph.

use std::collections::BTreeMap;

use au_diagnostics::{Diagnostic, Severity, Span};

use crate::canonical::CanonicalHash;
use crate::closure_id::{compute_closure_ids, ClosureHash};
use crate::codes;
use crate::typedef::{TypeDef, TypeName, TypeNameClaim};

#[derive(Debug, Default, Clone)]
pub struct TypeGraph {
    type_defs: BTreeMap<TypeName, TypeDef>,
    children: BTreeMap<TypeName, Vec<TypeName>>,
    /// Each def's canonical hash, computed once at build. The hash is a pure
    /// function of the def, so caching it here is the memoization the canonical
    /// hash decision calls for.
    /// Recomputing it per call was a per-field-per-instance hot-path cost.
    canonical_hashes: BTreeMap<TypeName, CanonicalHash>,
    /// Each def's referenced-closure id, computed once at build, O(V+E)
    /// bottom-up over the SCC-condensed reference graph. The closure-level
    /// sibling of `canonical_hashes`: a pure function of the graph, so it caches
    /// here for the same reason. The import-tier identity.
    closure_ids: BTreeMap<TypeName, ClosureHash>,
}

impl TypeGraph {
    pub fn get(&self, name: &TypeName) -> Option<&TypeDef> {
        self.type_defs.get(name)
    }

    pub fn contains(&self, name: &TypeName) -> bool {
        self.type_defs.contains_key(name)
    }

    /// The def's canonical hash, memoized at build. `None` for a name absent
    /// from this graph (e.g. a dangling parent). Equal to
    /// `CanonicalHash::of(self.get(name)?)` by construction, but O(1).
    ///
    /// Keying by name is sound because one graph is one repo, where a name maps
    /// to exactly one def (`duplicate-type-def` forbids a second); the cross-repo
    /// same-name-different-shape case lives across separate per-repo graphs, each
    /// with its own cache, and is resolved by comparing the two caches.
    pub fn canonical_hash(&self, name: &TypeName) -> Option<CanonicalHash> {
        self.canonical_hashes.get(name).copied()
    }

    /// The def's referenced-closure id, memoized at build. `None` for a name
    /// absent from this graph. The hash half of the import-tier identity
    /// `(name, ClosureHash)`: equal ids mean the same type across a repo
    /// boundary, a divergence anywhere in the closure changes it. Reads the
    /// precomputed O(V+E) value, never re-walks the closure.
    pub fn closure_id(&self, name: &TypeName) -> Option<ClosureHash> {
        self.closure_ids.get(name).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TypeName, &TypeDef)> {
        self.type_defs.iter()
    }

    pub fn names(&self) -> impl Iterator<Item = &TypeName> {
        self.type_defs.keys()
    }

    pub fn len(&self) -> usize {
        self.type_defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.type_defs.is_empty()
    }

    /// Direct parents declared on the type-def. Returns an empty slice for
    /// missing or root-level types.
    pub fn parents_of(&self, name: &TypeName) -> &[TypeNameClaim] {
        self.type_defs
            .get(name)
            .map(|td| td.parents.as_slice())
            .unwrap_or(&[])
    }

    /// Direct children — types that name `name` in their `type:` claim. Sorted
    /// by name for determinism.
    pub fn children_of(&self, name: &TypeName) -> &[TypeName] {
        self.children.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Direct sealed branches declared on the type-def. Empty if the type-def
    /// has no `sealed:` clause or is unknown.
    pub fn sealed_branches_of(&self, name: &TypeName) -> &[TypeNameClaim] {
        self.type_defs
            .get(name)
            .map(|td| td.sealed.as_slice())
            .unwrap_or(&[])
    }

    /// True if the type-def declares any `sealed:` branches.
    pub fn is_sealed(&self, name: &TypeName) -> bool {
        !self.sealed_branches_of(name).is_empty()
    }

    /// True if the type-def declares the raw `abstract: true` marker. This is
    /// the DECLARED flag only, not the derived non-claimable predicate. Use it
    /// where the two claim diagnostics must stay distinct (a declared-abstract
    /// non-sealed type fires `abstract-type-claimed`, a sealed one keeps firing
    /// the more specific `sealed-parent-claimed`).
    pub fn declared_abstract_of(&self, name: &TypeName) -> bool {
        self.type_defs
            .get(name)
            .is_some_and(|td| td.declared_abstract)
    }

    /// True if the type-def is NOT directly claimable: it declares `abstract:
    /// true`, OR it is sealed. Sealed implies abstract, and this predicate is
    /// where that invariant lives in code, so a slot ceiling or claim gate keyed
    /// on non-claimability calls this rather than `is_sealed` alone. See
    /// [[spec - abstract type-defs - a non-claimable open type-def, sealed is abstract plus closed]].
    pub fn is_abstract(&self, name: &TypeName) -> bool {
        self.declared_abstract_of(name) || self.is_sealed(name)
    }
}

#[derive(Debug, Default)]
pub struct GraphBuildResult {
    pub graph: TypeGraph,
    pub diagnostics: Vec<Diagnostic>,
}

/// Build a TypeGraph from one repo's parsed type-defs.
///
/// A name resolves first-wins within the repo. Two or more files deriving the
/// same name are competing claims, `duplicate-type-def`; the first wins.
/// Cross-repo sameness lives in the composition over the per-repo graphs, never
/// inside one graph.
pub fn build_graph(type_defs: Vec<TypeDef>) -> GraphBuildResult {
    let mut diagnostics = Vec::new();

    // Group by derived name, preserving input order within each group.
    let mut groups: BTreeMap<TypeName, Vec<TypeDef>> = BTreeMap::new();
    for td in type_defs {
        groups.entry(td.name.clone()).or_default().push(td);
    }

    let mut graph = TypeGraph::default();

    for (name, group) in groups {
        // First-wins within the repo; extra defs of the same name are
        // duplicates. A lone def simply wins as the sole copy.
        let mut it = group.into_iter();
        let canonical = it.next().unwrap();
        for extra in it {
            diagnostics.push(duplicate_type_def_diag(&extra, &canonical));
        }
        graph.type_defs.insert(name, canonical);
    }

    for (name, td) in &graph.type_defs {
        for parent in &td.parents {
            // A `::repo` parent is a peer type resolved by the cross-repo fold,
            // not an own-graph edge; it never seeds a child relationship here.
            if parent.is_qualified() {
                continue;
            }
            // Only index parents that name a known type-def. A dangling parent
            // reference (diagnosed elsewhere) must not seed a phantom
            // `children` key, so every key here stays a real type-def.
            if !graph.type_defs.contains_key(&parent.name) {
                continue;
            }
            graph
                .children
                .entry(parent.name.clone())
                .or_default()
                .push(name.clone());
        }
    }
    for children in graph.children.values_mut() {
        children.sort();
    }

    // Memoize each def's canonical hash once, here, so no read path recomputes
    // it. The hash is a pure function of the def alone.
    graph.canonical_hashes = graph
        .type_defs
        .iter()
        .map(|(name, td)| (name.clone(), CanonicalHash::of(td)))
        .collect();

    // Memoize each def's referenced-closure id once, here, over the canonical
    // hashes just computed. O(V+E), SCC-safe, a pure function of the graph, so
    // a read path never re-walks the closure.
    graph.closure_ids = compute_closure_ids(&graph.type_defs, &graph.canonical_hashes);

    GraphBuildResult { graph, diagnostics }
}

fn duplicate_type_def_diag(td: &TypeDef, existing: &TypeDef) -> Diagnostic {
    Diagnostic {
        code: codes::DUPLICATE_TYPE_DEF,
        severity: Severity::Error,
        span: Span::new(td.source_path.clone(), td.source_span),
        message: format!(
            "type-def '{}' is declared in multiple files",
            td.name.as_str()
        ),
        related: vec![Span::new(
            existing.source_path.clone(),
            existing.source_span,
        )],
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typedef::{ParentClaim, ParentClaimForm};
    use au_diagnostics::ByteRange;
    use std::path::PathBuf;

    fn td(name: &str, parents: &[&str], sealed: &[&str]) -> TypeDef {
        TypeDef {
            shape: None,
            name: TypeName(name.into()),
            source_path: PathBuf::from(format!("/v/{name}.type.yaml")),
            source_span: ByteRange::new(0, 0),
            parent_claim: if parents.is_empty() {
                None
            } else {
                Some(ParentClaim {
                    form: ParentClaimForm::List,
                    value_span: ByteRange::new(0, 0),
                })
            },
            parents: parents
                .iter()
                .map(|p| TypeNameClaim::own(TypeName((*p).into()), ByteRange::new(0, 0)))
                .collect(),
            fields: vec![],
            sealed: sealed
                .iter()
                .map(|s| TypeNameClaim::own(TypeName((*s).into()), ByteRange::new(0, 0)))
                .collect(),
            declared_abstract: false,
            meta_blocks: None,
            required_meta: Vec::new(),
            body: None,
            doc: None,
            ..Default::default()
        }
    }

    #[test]
    fn empty_graph() {
        let g = build_graph(vec![]).graph;
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
    }

    #[test]
    fn single_type_def() {
        let g = build_graph(vec![td("note", &[], &[])]).graph;
        assert_eq!(g.len(), 1);
        assert!(g.contains(&TypeName("note".into())));
        assert!(g.parents_of(&TypeName("note".into())).is_empty());
        assert!(g.children_of(&TypeName("note".into())).is_empty());
    }

    #[test]
    fn parent_child_indices() {
        let g = build_graph(vec![
            td("note", &[], &[]),
            td("decision", &["note"], &[]),
            td("source", &["note"], &[]),
        ])
        .graph;
        let note = TypeName("note".into());
        let kids = g.children_of(&note);
        // Sorted alphabetically.
        assert_eq!(
            kids.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
            vec!["decision", "source"]
        );
        let parents = g.parents_of(&TypeName("decision".into()));
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].name.as_str(), "note");
    }

    #[test]
    fn dangling_parent_does_not_seed_a_children_key() {
        // A parent naming a type-def that does not exist must not create a
        // `children` entry: every key in the index is a known type.
        let g = build_graph(vec![td("decision", &["ghost"], &[])]).graph;
        assert!(
            g.children_of(&TypeName("ghost".into())).is_empty(),
            "a dangling parent must not appear as a children key"
        );
        assert!(!g.contains(&TypeName("ghost".into())));
    }

    #[test]
    fn sealed_accessors() {
        let g = build_graph(vec![
            td("source", &[], &["source.url", "source.path"]),
            td("source.url", &["source"], &[]),
            td("source.path", &["source"], &[]),
        ])
        .graph;
        assert!(g.is_sealed(&TypeName("source".into())));
        assert_eq!(g.sealed_branches_of(&TypeName("source".into())).len(), 2);
        assert!(!g.is_sealed(&TypeName("source.url".into())));
    }

    #[test]
    fn duplicate_type_defs_emit_diagnostic_and_first_wins() {
        let first = TypeDef {
            shape: None,
            source_path: PathBuf::from("/v/a.type.yaml"),
            ..td("decision", &["note"], &[])
        };
        let second = TypeDef {
            shape: None,
            source_path: PathBuf::from("/v/type/decision.yaml"),
            ..td("decision", &[], &[])
        };
        let res = build_graph(vec![first, second]);
        assert_eq!(res.diagnostics.len(), 1);
        assert_eq!(res.diagnostics[0].code.as_str(), "duplicate-type-def");
        assert!(!res.diagnostics[0].related.is_empty());
        // First-wins: the kept type-def has the parent claim from `first`.
        let kept = res.graph.get(&TypeName("decision".into())).unwrap();
        assert_eq!(kept.parents.len(), 1);
    }

    #[test]
    fn iteration_is_sorted_by_name() {
        let g = build_graph(vec![
            td("zebra", &[], &[]),
            td("alpha", &[], &[]),
            td("mango", &[], &[]),
        ])
        .graph;
        let names: Vec<_> = g.names().map(|n| n.as_str().to_string()).collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }

    #[test]
    fn graph_build_does_not_surface_shape_errors() {
        // A type-def with a malformed shape parses into the graph; the
        // shape Err is held on the FieldDecl until validation. The graph
        // build itself emits zero diagnostics.
        use crate::typedef::parse_type_def;
        use au_parser::yaml::parse;
        use std::path::Path;

        let src = "fields:\n  r: '@bad'\n";
        let docs = parse(src).unwrap();
        let path = Path::new("/v/x.type.yaml");
        let parsed = parse_type_def(path, src, 0, &docs[0]);
        // No parser-level diagnostics either.
        assert!(parsed.diagnostics.is_empty());

        let res = build_graph(vec![parsed.type_def.unwrap()]);
        assert!(res.diagnostics.is_empty());

        // The Err is still reachable on the FieldDecl for the validator.
        let td = res.graph.get(&TypeName("x".into())).unwrap();
        assert!(td.fields[0].parsed_shape.is_err());
    }

    #[test]
    fn canonical_hash_cache_equals_recompute() {
        use crate::canonical::CanonicalHash;
        // `foo` is a parent, `bar` declares it; `baz` claims an absent parent.
        let graph = build_graph(vec![
            td("foo", &[], &[]),
            td("bar", &["foo"], &[]),
            td("baz", &["missing"], &[]),
        ])
        .graph;

        // The memoized hash matches a fresh recompute for every present def.
        for (name, def) in graph.iter() {
            assert_eq!(
                graph.canonical_hash(name),
                Some(CanonicalHash::of(def)),
                "cache must equal recompute for {name:?}"
            );
        }

        // An absent name (here a dangling parent) has no cached hash.
        assert_eq!(graph.canonical_hash(&TypeName("missing".into())), None);
    }
}
