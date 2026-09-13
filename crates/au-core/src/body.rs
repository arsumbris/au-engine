//! Type-def body template AST and parser.
//!
//! A type-def's optional `body:` key declares an authoring template for
//! markdown instances. Per [[type-def body::au-type-system]], the value is an ordered list of items;
//! each item has exactly one discriminating key:
//! - `use:` — splice another type-def's body inline (top-level only)
//! - `section:` / `section?:` — declare a named section
//! - `fills:` / `fills!:` — bind a contract to the enclosing scope
//!
//! This module owns the parse from `MarkedYaml` to `BodyTemplate`. Load-time
//! checks (use: closure / cycle, fills: shape extractability) live in
//! `load_checks`.

use std::collections::BTreeSet;
use std::path::Path;

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};
use au_parser::yaml::{span_to_byte_range, MarkedYaml, Scalar, YamlData};

use crate::codes;
use crate::graph::TypeGraph;
use crate::typedef::{FieldName, TypeName};

/// One item under `body:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyItem {
    /// `- use: T` — splice T's body inline at this position.
    Use {
        type_name: TypeName,
        /// The `::repo` peer qualifier, `None` for an own-body splice. A qualified
        /// `use:` splices a peer type's body via the fold; own-graph paths defer it.
        repo: Option<String>,
        type_name_span: ByteRange,
        item_span: ByteRange,
    },
    /// `- section: Name` or `- section?: Name` plus optional fills /
    /// guidance / nested body.
    ///
    /// `source_path` is the type-def file the section was parsed from —
    /// preserved across `use:` splices so diagnostics can point at the
    /// declaration site even when the section came from a spliced
    /// ancestor.
    Section {
        name: String,
        optional: bool,
        fills: Option<FillsContract>,
        guidance: Option<String>,
        body: Option<Vec<BodyItem>>,
        name_span: ByteRange,
        item_span: ByteRange,
        source_path: std::path::PathBuf,
    },
    /// `- fills:` / `- fills!:` as a bare item — body-level contract.
    Fills {
        contract: FillsContract,
        item_span: ByteRange,
    },
}

/// `fills:` or `fills!:` contract carried by either a section or a body-level
/// item. `exclusive` is `true` for the `!:` form.
///
/// `source_path` is the type-def file the contract was parsed from — the
/// host's file for own-body contracts, the spliced ancestor's file for
/// contracts that arrived through a top-level `use:`. Diagnostics use
/// this to anchor a `related: Vec<Span>` pointer at the contract's
/// declaration site without re-walking the graph at fire time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillsContract {
    pub fields: Vec<FieldClaim>,
    pub exclusive: bool,
    pub fields_span: ByteRange,
    pub source_path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldClaim {
    pub name: FieldName,
    pub span: ByteRange,
}

/// Parsed body template.
///
/// `None` means the `body:` key was absent. `Some(vec![])` means the key was
/// present but empty (treated like absent for behavior, per [[type-def body::au-type-system]]). A non-empty
/// vec is a declared template — and per [[type-def body::au-type-system]] makes the type-def markdown-only.
pub type BodyTemplate = Vec<BodyItem>;

/// Is the type-def body-declaring per [[type-def body::au-type-system]] — i.e. carries one or more items
/// (not just an empty `body: []`)?
pub fn is_body_declaring(body: Option<&BodyTemplate>) -> bool {
    body.map_or(false, |b| !b.is_empty())
}

/// Post-splice body for `name`: resolves every top-level `use: T` against
/// the graph, recursing through inner bodies. Returns `None` when the
/// type-def isn't body-declaring (no `body:` or `body: []`).
///
/// Two distinct silent skips:
/// - **Cycle skip.** When recursion would re-enter a host already on the
///   active splice stack, the `use:` is dropped. `load_checks` emits the
///   `body-use-cycle` diagnostic for the same edge.
/// - **Unknown-target skip.** When a `use:` references a type-def not in
///   the graph, the splice is dropped. `load_checks` emits
///   `body-use-out-of-closure`.
///
/// Repeated `use:` of the same target at sibling positions IS preserved —
/// per [[type-def body use::au-type-system]] "Repetition is literal." The active-stack semantics push on
/// entry into a target and pop on return, so a `use: A` followed by a
/// second `use: A` at the same level inlines A's body twice.
pub fn splice_effective_body(
    graph: &TypeGraph,
    name: &TypeName,
    peer: Option<&dyn crate::validate::CrossRepoResolver>,
) -> Option<BodyTemplate> {
    let td = graph.get(name)?;
    let body = td.body.as_ref().filter(|b| !b.is_empty())?;
    // The active set keys `(repo, name)`, so an own `H` and a peer `H::repo` never
    // collide and a cross-repo cycle terminates. `None` is the top-level graph R.
    let mut active: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    active.insert((None, name.as_str().to_string()));
    Some(splice_body_inner(graph, None, body, &mut active, peer))
}

/// Splice `body`, whose items live in `graph` (repo `repo`, `None` for the
/// top-level graph R). A bare `use: T` splices T's body within `graph`; a
/// `use: T::peer` fetches the peer's graph through `peer` and recurses there, so
/// the peer's own nested bare `use:`s resolve in the peer's graph. An unresolvable
/// peer (or a `None` seam) leaves the qualified `use:` unspliced.
fn splice_body_inner(
    graph: &TypeGraph,
    repo: Option<&str>,
    body: &BodyTemplate,
    active: &mut BTreeSet<(Option<String>, String)>,
    peer: Option<&dyn crate::validate::CrossRepoResolver>,
) -> BodyTemplate {
    let mut out = Vec::with_capacity(body.len());
    for item in body {
        match item {
            BodyItem::Use {
                type_name,
                repo: use_repo,
                ..
            } => {
                // Where the target's body lives: a peer graph for `T::peer`, else
                // the current graph for a bare `use:`.
                let (target_graph, target_repo): (&TypeGraph, Option<&str>) = match use_repo {
                    Some(r) => match peer.and_then(|p| p.peer_graph(r)) {
                        Some(g) => (g, Some(r.as_str())),
                        // Unresolvable peer: keep unspliced (the gate owns it), as
                        // when no seam is supplied.
                        None => {
                            out.push(item.clone());
                            continue;
                        }
                    },
                    None => (graph, repo),
                };
                let key = (
                    target_repo.map(|s| s.to_string()),
                    type_name.as_str().to_string(),
                );
                if active.contains(&key) {
                    continue;
                }
                let Some(td) = target_graph.get(type_name) else {
                    continue;
                };
                let Some(inner_body) = &td.body else {
                    continue;
                };
                active.insert(key.clone());
                out.extend(splice_body_inner(
                    target_graph,
                    target_repo,
                    inner_body,
                    active,
                    peer,
                ));
                active.remove(&key);
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// Parse the `body:` value into a `Vec<BodyItem>`. Caller has already
/// confirmed the host key is `body:`. Used both at the top-level and
/// recursively for nested `body:` inside section items.
pub fn parse_body_value(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    nested: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<BodyItem> {
    let value_span = span_to_byte_range(source, yaml_offset, value.span);
    let seq = match &value.data {
        YamlData::Sequence(items) => items,
        _ => {
            diagnostics.push(simple_diag(
                codes::BODY_NOT_A_LIST,
                path,
                value_span,
                "`body:` must be a YAML list",
            ));
            return Vec::new();
        }
    };
    let mut items = Vec::with_capacity(seq.len());
    for entry in seq.iter() {
        if let Some(item) = parse_body_item(path, source, yaml_offset, entry, nested, diagnostics) {
            items.push(item);
        }
    }
    items
}

fn parse_body_item(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    entry: &MarkedYaml<'_>,
    nested: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<BodyItem> {
    let item_span = span_to_byte_range(source, yaml_offset, entry.span);
    let mapping = match &entry.data {
        YamlData::Mapping(m) => m,
        _ => {
            diagnostics.push(simple_diag(
                codes::BODY_ITEM_NOT_A_MAPPING,
                path,
                item_span,
                "each `body:` item must be a YAML mapping with a discriminator key (`use:`, `section:`, `section?:`, `fills:`, or `fills!:`)",
            ));
            return None;
        }
    };

    // Pull discriminator first. Strict precedence: use > section / section? > fills / fills!.
    let mut use_target: Option<(TypeName, ByteRange)> = None;
    let mut section_name: Option<(String, bool, ByteRange)> = None;
    let mut fills: Option<(Vec<FieldClaim>, bool, ByteRange)> = None;
    let mut nested_body_value: Option<&MarkedYaml<'_>> = None;
    let mut guidance: Option<String> = None;

    for (key, value) in mapping.iter() {
        let Some(key_str) = scalar_string(key) else {
            continue;
        };
        match key_str.as_str() {
            "use" => {
                if let Some(name) = scalar_string(value) {
                    use_target = Some((
                        TypeName(name),
                        span_to_byte_range(source, yaml_offset, value.span),
                    ));
                }
            }
            "section" => {
                if let Some(name) = scalar_string(value) {
                    section_name = Some((
                        name,
                        false,
                        span_to_byte_range(source, yaml_offset, value.span),
                    ));
                }
            }
            "section?" => {
                if let Some(name) = scalar_string(value) {
                    section_name = Some((
                        name,
                        true,
                        span_to_byte_range(source, yaml_offset, value.span),
                    ));
                }
            }
            "fills" | "fills!" => {
                let exclusive = key_str == "fills!";
                let claims = parse_fills_value(path, source, yaml_offset, value, diagnostics);
                let span = span_to_byte_range(source, yaml_offset, value.span);
                if let Some((_, prior_exclusive, _)) = &fills {
                    if *prior_exclusive != exclusive {
                        diagnostics.push(simple_diag(
                            codes::FILLS_DOUBLE_FORM_DECLARATION,
                            path,
                            item_span,
                            "scope carries both `fills:` and `fills!:` — pick one",
                        ));
                    }
                }
                fills = Some((claims, exclusive, span));
            }
            "body" => {
                nested_body_value = Some(value);
            }
            "guidance" => {
                if let Some(text) = scalar_string(value) {
                    guidance = Some(text);
                }
            }
            _ => {
                // unknown keys are advisory per [[type-def body::au-type-system]] — silently ignored.
            }
        }
    }

    if let Some((type_name_raw, type_name_span)) = use_target {
        if nested {
            diagnostics.push(simple_diag(
                codes::BODY_USE_NESTED,
                path,
                item_span,
                "`use:` is valid only at the top of the outermost `body:`",
            ));
        }
        // Split the `::repo` peer qualifier, mirroring a `::repo` claim.
        let (type_name, repo) = match type_name_raw.as_str().split_once("::") {
            Some((base, r)) => (TypeName(base.to_string()), Some(r.to_string())),
            None => (type_name_raw, None),
        };
        return Some(BodyItem::Use {
            type_name,
            repo,
            type_name_span,
            item_span,
        });
    }

    if let Some((name, optional, name_span)) = section_name {
        let fills_contract = fills.map(|(fields, exclusive, span)| FillsContract {
            fields,
            exclusive,
            fields_span: span,
            source_path: path.to_path_buf(),
        });
        let nested = nested_body_value
            .map(|v| parse_body_value(path, source, yaml_offset, v, true, diagnostics));
        return Some(BodyItem::Section {
            name,
            optional,
            fills: fills_contract,
            guidance,
            body: nested,
            name_span,
            item_span,
            source_path: path.to_path_buf(),
        });
    }

    if let Some((fields, exclusive, fields_span)) = fills {
        return Some(BodyItem::Fills {
            contract: FillsContract {
                fields,
                exclusive,
                fields_span,
                source_path: path.to_path_buf(),
            },
            item_span,
        });
    }

    diagnostics.push(simple_diag(
        codes::BODY_ITEM_MISSING_DISCRIMINATOR,
        path,
        item_span,
        "`body:` item is missing a discriminator (`use:`, `section:`, `section?:`, `fills:`, or `fills!:`)",
    ));
    None
}

fn parse_fills_value(
    path: &Path,
    source: &str,
    yaml_offset: usize,
    value: &MarkedYaml<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<FieldClaim> {
    match &value.data {
        YamlData::Value(Scalar::String(s)) => vec![FieldClaim {
            name: FieldName(s.to_string()),
            span: span_to_byte_range(source, yaml_offset, value.span),
        }],
        YamlData::Sequence(items) => items
            .iter()
            .filter_map(|entry| {
                scalar_string(entry).map(|s| FieldClaim {
                    name: FieldName(s),
                    span: span_to_byte_range(source, yaml_offset, entry.span),
                })
            })
            .collect(),
        _ => {
            diagnostics.push(simple_diag(
                codes::FILLS_VALUE_BAD_SHAPE,
                path,
                span_to_byte_range(source, yaml_offset, value.span),
                "`fills:` value must be a field name or a list of field names",
            ));
            Vec::new()
        }
    }
}

fn scalar_string(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn simple_diag(
    code: au_diagnostics::DiagnosticCode,
    path: &Path,
    span: ByteRange,
    message: &str,
) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Error,
        span: Span::new(path.to_path_buf(), span),
        message: message.to_string(),
        related: vec![],
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::typedef::TypeDef;
    use std::path::PathBuf;

    fn td_with_body(name: &str, body: Vec<BodyItem>) -> TypeDef {
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
            body: Some(body),
            doc: None,
            ..Default::default()
        }
    }

    fn use_item(target: &str) -> BodyItem {
        BodyItem::Use {
            type_name: TypeName(target.into()),
            repo: None,
            type_name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
        }
    }

    fn use_item_q(target: &str, repo: &str) -> BodyItem {
        BodyItem::Use {
            type_name: TypeName(target.into()),
            repo: Some(repo.into()),
            type_name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
        }
    }

    /// A `CrossRepoResolver` that only maps a repo name to a graph, the sole seam
    /// the body splice uses. `resolve` is unused here.
    struct MapPeer {
        graphs: std::collections::BTreeMap<String, TypeGraph>,
    }
    impl crate::validate::CrossRepoResolver for MapPeer {
        fn resolve(
            &self,
            _source: &std::path::Path,
            _repo: &str,
            _target: &str,
        ) -> Option<crate::validate::CrossRepoTarget<'_>> {
            None
        }
        fn peer_graph(&self, repo: &str) -> Option<&TypeGraph> {
            self.graphs.get(repo)
        }
    }

    fn section_item(name: &str) -> BodyItem {
        BodyItem::Section {
            name: name.into(),
            optional: false,
            fills: None,
            guidance: None,
            body: None,
            name_span: ByteRange::new(0, 0),
            item_span: ByteRange::new(0, 0),
            source_path: PathBuf::from("/v/h.type.yaml"),
        }
    }

    fn section_names(template: &BodyTemplate) -> Vec<String> {
        template
            .iter()
            .map(|item| match item {
                BodyItem::Section { name, .. } => name.clone(),
                BodyItem::Use { type_name, .. } => format!("use:{}", type_name.as_str()),
                BodyItem::Fills { .. } => "fills".to_string(),
            })
            .collect()
    }

    /// [[type-def body use::au-type-system]] "Repetition is literal." Two `use: A` at sibling positions
    /// must splice A's body twice, not collapse to once.
    #[test]
    fn repeated_use_at_same_level_splices_twice() {
        let a = td_with_body("a", vec![section_item("FromA")]);
        let h = td_with_body(
            "h",
            vec![use_item("a"), section_item("Middle"), use_item("a")],
        );
        let graph = build_graph(vec![a, h]).graph;

        let effective = splice_effective_body(&graph, &TypeName("h".into()), None)
            .expect("h declares a non-empty body");

        assert_eq!(
            section_names(&effective),
            vec!["FromA", "Middle", "FromA"],
            "second `use: a` must re-splice A's body (no silent dedup)"
        );
    }

    /// A `use: p::peer` splices the PEER's body from the peer's graph, and the
    /// peer's own nested bare `use:` resolves within the peer's graph.
    #[test]
    fn qualified_use_splices_the_peer_body() {
        let own = build_graph(vec![td_with_body(
            "h",
            vec![
                section_item("Top"),
                use_item_q("p", "peer"),
                section_item("Bottom"),
            ],
        )])
        .graph;
        // peer's `p` uses its own `sub` (bare, in the peer graph).
        let sub = td_with_body("sub", vec![section_item("FromSub")]);
        let p = td_with_body("p", vec![use_item("sub"), section_item("FromP")]);
        let peer = build_graph(vec![sub, p]).graph;
        let resolver = MapPeer {
            graphs: [("peer".to_string(), peer)].into_iter().collect(),
        };

        let effective = splice_effective_body(&own, &TypeName("h".into()), Some(&resolver))
            .expect("h declares a body");
        assert_eq!(
            section_names(&effective),
            vec!["Top", "FromSub", "FromP", "Bottom"],
            "the peer body (with its own nested bare use) splices in order"
        );
    }

    /// Without a resolver, a `::repo` use stays unspliced — today's behavior for
    /// au-core's own (repo-agnostic) callers, no regression.
    #[test]
    fn qualified_use_without_a_resolver_stays_unspliced() {
        let own = build_graph(vec![td_with_body(
            "h",
            vec![use_item_q("p", "peer"), section_item("Own")],
        )])
        .graph;
        let effective =
            splice_effective_body(&own, &TypeName("h".into()), None).expect("h declares a body");
        assert_eq!(section_names(&effective), vec!["use:p", "Own"]);
    }

    /// A cross-repo body-use cycle (`h` uses `p::peer`, `p` uses `h::own`)
    /// terminates via the `(repo, name)` active guard — no infinite recursion.
    #[test]
    fn cross_repo_body_use_cycle_terminates() {
        let own = build_graph(vec![td_with_body(
            "h",
            vec![use_item_q("p", "peer"), section_item("H")],
        )])
        .graph;
        let p = td_with_body("p", vec![use_item_q("h", "own"), section_item("P")]);
        let peer = build_graph(vec![p]).graph;
        // `own` maps back to h's graph so the peer's `h::own` resolves and closes
        // the loop; the guard stops it.
        let resolver = MapPeer {
            graphs: [("peer".to_string(), peer), ("own".to_string(), own.clone())]
                .into_iter()
                .collect(),
        };

        let effective = splice_effective_body(&own, &TypeName("h".into()), Some(&resolver))
            .expect("h declares a body");
        assert_eq!(
            section_names(&effective),
            vec!["H", "P", "H"],
            "the cycle terminates with a bounded splice"
        );
    }

    /// Self-cycle on a single host: H's body has `use: H`. Cycle guard
    /// drops the inner splice, no infinite recursion.
    #[test]
    fn self_cycle_is_silently_dropped() {
        let h = td_with_body("h", vec![use_item("h"), section_item("Plain")]);
        let graph = build_graph(vec![h]).graph;

        let effective = splice_effective_body(&graph, &TypeName("h".into()), None)
            .expect("h declares a non-empty body");

        assert_eq!(
            section_names(&effective),
            vec!["Plain"],
            "self-`use:` is cycle-skipped; only the literal section remains"
        );
    }

    /// Two-node cycle A↔B reached from a third host. The cycle guard
    /// drops the back-edge but lets the forward `use` succeed once.
    #[test]
    fn two_node_cycle_drops_back_edge_only() {
        let a = td_with_body("a", vec![section_item("FromA"), use_item("b")]);
        let b = td_with_body("b", vec![section_item("FromB"), use_item("a")]);
        let h = td_with_body("h", vec![use_item("a")]);
        let graph = build_graph(vec![a, b, h]).graph;

        let effective = splice_effective_body(&graph, &TypeName("h".into()), None)
            .expect("h declares a non-empty body");

        // h → a (active: h, a). a's body: FromA, then use b (active: h, a, b).
        // b's body: FromB, then use a (active contains a) → dropped.
        // Pop b, pop a. Final: [FromA, FromB].
        assert_eq!(section_names(&effective), vec!["FromA", "FromB"]);
    }

    /// `use:` of the same target at different nesting levels: A's body
    /// has `use: C`, then B's body has `use: C`, and H uses both A and B.
    /// C's body must appear twice (once under each splice).
    #[test]
    fn same_target_at_different_splice_depths_preserves_both() {
        let c = td_with_body("c", vec![section_item("FromC")]);
        let a = td_with_body("a", vec![section_item("FromA"), use_item("c")]);
        let b = td_with_body("b", vec![use_item("c"), section_item("FromB")]);
        let h = td_with_body("h", vec![use_item("a"), use_item("b")]);
        let graph = build_graph(vec![a, b, c, h]).graph;

        let effective = splice_effective_body(&graph, &TypeName("h".into()), None)
            .expect("h declares a non-empty body");

        assert_eq!(
            section_names(&effective),
            vec!["FromA", "FromC", "FromC", "FromB"],
            "C must be spliced once under A and once under B; pop-on-return semantics preserve both"
        );
    }

    /// Unknown `use:` target is silently dropped (load_checks fires
    /// body-use-out-of-closure separately).
    #[test]
    fn unknown_use_target_is_dropped() {
        let h = td_with_body(
            "h",
            vec![
                section_item("First"),
                use_item("nonexistent"),
                section_item("Second"),
            ],
        );
        let graph = build_graph(vec![h]).graph;

        let effective = splice_effective_body(&graph, &TypeName("h".into()), None)
            .expect("h declares a non-empty body");

        assert_eq!(section_names(&effective), vec!["First", "Second"]);
    }
}
