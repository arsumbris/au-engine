//! Per-instance location-constraint resolution ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]).
//!
//! The RESOLUTION here is pure graph logic: which `location:` block(s) an
//! instance's claim is subject to, after whole-block closest-wins override and
//! token-equal auto-unify. The MATCHING against a file's actual placement takes
//! the repo-relative path as data (the engine supplies it), so au-core stays
//! I/O-free.

use std::collections::BTreeSet;
use std::path::Path;

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};

use crate::closure::{closure_of, folded_closure_ids};
use crate::codes;
use crate::graph::TypeGraph;
use crate::instance::{Instance, InstanceValue, TypeClaim};
use crate::location::{GlobSegment, LocationSpec, NameSegment, NameTemplate};
use crate::resolution::{ResolutionGraph, TypeId};
use crate::typedef::TypeName;

/// Resolve the effective location constraint(s) an instance's claim is subject
/// to.
///
/// Whole-block closest-wins ([[type-def meta::au-type-system]] model): a declared `location:` is
/// SHADOWED by any declared location on a strict descendant in the instance's
/// closure, so the declaration nearest the claimed leaf survives. Token-equal
/// survivors auto-unify to one.
///
/// - empty result, no location constraint.
/// - one, a single constraint, matched normally.
/// - more than one, a RAW MIXIN of unrelated declared locations, satisfy-any.
///
/// When `resolution` is present (R imports peers) the walk runs over the FOLDED
/// resolution graph, so a `type: foo::repo` claim (and a cross-repo `extends:`
/// parent) is subject to the peer type's location. Absent (a non-importing repo,
/// or a direct au-core caller), it runs over the own graph and a `::repo` claim
/// cannot resolve (nothing to import). The two paths agree for a purely-own
/// claim, the parity [`crate::closure::effective_shape_resolved`] holds.
pub fn effective_locations<'g>(
    graph: &'g TypeGraph,
    resolution: Option<&'g ResolutionGraph>,
    claim: &TypeClaim,
) -> Vec<(&'g TypeName, &'g LocationSpec)> {
    match resolution {
        Some(rg) => effective_locations_resolved(rg, claim),
        None => effective_locations_own(graph, claim),
    }
}

/// Own-graph resolution: a `::repo` claim cannot resolve here and is skipped.
fn effective_locations_own<'g>(
    graph: &'g TypeGraph,
    claim: &TypeClaim,
) -> Vec<(&'g TypeName, &'g LocationSpec)> {
    let mut closure: BTreeSet<TypeName> = BTreeSet::new();
    for c in claim.iter() {
        if c.is_qualified() {
            continue;
        }
        closure.extend(closure_of(graph, &c.name));
    }

    // Types in the closure that declare a location.
    let declared: Vec<&TypeName> = closure
        .iter()
        .filter(|n| graph.get(n).is_some_and(|td| td.location.is_some()))
        .collect();

    // A declared X survives iff no OTHER declared Y has X as a proper ancestor
    // (Y a strict descendant of X): a closer declaration overrides a farther one.
    let mut survivors: Vec<(&TypeName, &LocationSpec)> = Vec::new();
    for &x in &declared {
        let overridden = declared
            .iter()
            .any(|&y| y != x && closure_of(graph, y).contains(x));
        if !overridden {
            // Borrow the owner name from the GRAPH (`'g`), not the local closure set.
            if let Some(td) = graph.get(x) {
                if let Some(loc) = td.location.as_ref() {
                    survivors.push((&td.name, loc));
                }
            }
        }
    }
    auto_unify(survivors)
}

/// Folded-graph resolution: every claim (bare or `::repo`) resolves through the
/// fold, so a peer type's location is reached without the peer's graph. The
/// closest-wins and auto-unify logic mirror the own-graph path, over `TypeId`s.
fn effective_locations_resolved<'g>(
    rg: &'g ResolutionGraph,
    claim: &TypeClaim,
) -> Vec<(&'g TypeName, &'g LocationSpec)> {
    let closure = folded_closure_ids(rg, claim);
    let declared: Vec<&TypeId> = closure
        .iter()
        .filter(|tid| rg.get(tid).is_some_and(|n| n.location.is_some()))
        .collect();

    let mut survivors: Vec<(&'g TypeName, &'g LocationSpec)> = Vec::new();
    for &x in &declared {
        let overridden = declared
            .iter()
            .any(|&y| y != x && folded_ancestors(rg, y).contains(x));
        if !overridden {
            if let Some(node) = rg.get(x) {
                if let Some(loc) = node.location.as_ref() {
                    survivors.push((&node.id.name, loc));
                }
            }
        }
    }
    auto_unify(survivors)
}

/// The folded ancestor set of `tid` (itself included), over the resolution
/// graph's parent-`TypeId` edges. `y`'s set containing `x` means `y` is a strict
/// descendant of `x`, so `x`'s location is overridden.
fn folded_ancestors(rg: &ResolutionGraph, tid: &TypeId) -> BTreeSet<TypeId> {
    let mut set = BTreeSet::new();
    let mut stack = vec![tid.clone()];
    while let Some(t) = stack.pop() {
        if !set.insert(t.clone()) {
            continue;
        }
        if let Some(n) = rg.get(&t) {
            for p in &n.parents {
                stack.push(p.clone());
            }
        }
    }
    set
}

/// Token-equal blocks auto-unify: one representative per distinct constraint,
/// keeping the first owner (deterministic, the survivor list is id/name-sorted).
fn auto_unify<'g>(
    survivors: Vec<(&'g TypeName, &'g LocationSpec)>,
) -> Vec<(&'g TypeName, &'g LocationSpec)> {
    let mut unified: Vec<(&'g TypeName, &'g LocationSpec)> = Vec::new();
    for (owner, s) in survivors {
        if !unified.iter().any(|(_, u)| u.constraint_eq(s)) {
            unified.push((owner, s));
        }
    }
    unified
}

/// Validate an instance's placement against its effective location(s).
///
/// `rel_path` is the file's REPO-RELATIVE path, supplied by the engine (au-core
/// is I/O-free). Severity model ([[spec - location constraints - a name template and path predicate as an advisory placement meet]]):
/// - a `strict` location unmet is `location-strict-violation` (error), mandatory.
/// - satisfy-any met, each unmet SOFT location is `location-partial-unmet` (hint).
/// - matched no claimed location, one `location-mismatch` (warning).
pub fn validate_location(
    graph: &TypeGraph,
    resolution: Option<&ResolutionGraph>,
    instance: &Instance,
    rel_path: &Path,
) -> Vec<Diagnostic> {
    let locs = effective_locations(graph, resolution, &instance.type_claim);
    if locs.is_empty() {
        return Vec::new();
    }
    let matched: Vec<bool> = locs
        .iter()
        .map(|(owner, spec)| location_matches(spec, owner, rel_path, instance))
        .collect();
    let any_matched = matched.iter().any(|&m| m);
    let mut diags = Vec::new();

    // Strict locations are mandatory: unmet is an error even inside a mixin.
    for ((owner, spec), &m) in locs.iter().zip(&matched) {
        if spec.strict && !m {
            diags.push(loc_diag(
                codes::LOCATION_STRICT_VIOLATION,
                Severity::Error,
                instance,
                format!(
                    "file does not satisfy the strict location of '{}'{}",
                    owner.as_str(),
                    expected_suffix(spec)
                ),
            ));
        }
    }

    if any_matched {
        // Satisfy-any met: each unmet SOFT location is a hint.
        for ((owner, spec), &m) in locs.iter().zip(&matched) {
            if !spec.strict && !m {
                diags.push(loc_diag(
                    codes::LOCATION_PARTIAL_UNMET,
                    Severity::Hint,
                    instance,
                    format!(
                        "file matches another claimed location but not '{}'s{}",
                        owner.as_str(),
                        expected_suffix(spec)
                    ),
                ));
            }
        }
    } else if locs.iter().any(|(_, spec)| !spec.strict) {
        // Matched no claimed location and at least one is soft: one warning.
        // (An all-strict no-match is fully covered by the strict errors above.)
        let names: Vec<&str> = locs.iter().map(|(o, _)| o.as_str()).collect();
        diags.push(loc_diag(
            codes::LOCATION_MISMATCH,
            Severity::Warning,
            instance,
            format!(
                "file does not match its type's location ({})",
                names.join(", ")
            ),
        ));
    }
    diags
}

fn loc_diag(
    code: au_diagnostics::DiagnosticCode,
    severity: Severity,
    instance: &Instance,
    message: String,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        span: Span::new(instance.source_path.clone(), ByteRange::new(0, 0)),
        message,
        related: vec![],
        fix: None,
    }
}

/// A human-readable "(expected …)" clause naming the sub-keys a location pins.
fn expected_suffix(spec: &LocationSpec) -> String {
    let mut parts = Vec::new();
    if let Some(p) = &spec.path {
        parts.push(format!("path `{}`", p.raw));
    }
    if let Some(n) = &spec.name {
        parts.push(format!("name `{}`", n.raw));
    }
    if let Some(ft) = spec.file_type {
        parts.push(format!("fileType `{}`", ft.extension()));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" (expected {})", parts.join(", "))
    }
}

/// Does the file at `rel_path` satisfy EVERY sub-key `spec` declares?
fn location_matches(
    spec: &LocationSpec,
    owner: &TypeName,
    rel_path: &Path,
    instance: &Instance,
) -> bool {
    if let Some(name) = &spec.name {
        match render_name(name, owner, instance) {
            Some(stem) if file_stem(rel_path) == Some(stem.as_str()) => {}
            _ => return false,
        }
    }
    if let Some(glob) = &spec.path {
        if !glob_matches(&glob.segments, &dir_segments(rel_path)) {
            return false;
        }
    }
    if let Some(ft) = spec.file_type {
        if rel_path.extension().and_then(|e| e.to_str()) != Some(ft.extension()) {
            return false;
        }
    }
    true
}

/// Render a `name` template. `${.type}` is the owning type's name; `${.field}`
/// is the field's canonical string value. `None` when a field is absent or its
/// value has no single-component rendering (the file then simply does not match).
fn render_name(t: &NameTemplate, owner: &TypeName, instance: &Instance) -> Option<String> {
    let mut out = String::new();
    for seg in &t.segments {
        match seg {
            NameSegment::Literal(s) => out.push_str(s),
            NameSegment::Type => out.push_str(owner.as_str()),
            NameSegment::Field(f) => {
                let field = instance.fields.iter().find(|fld| fld.key == *f)?;
                out.push_str(&render_value(&field.value)?);
            }
        }
    }
    Some(out)
}

fn render_value(v: &InstanceValue) -> Option<String> {
    match v {
        InstanceValue::String(s) => Some(s.clone()),
        InstanceValue::Integer(i) => Some(i.to_string()),
        InstanceValue::Float(f) => Some(f.to_string()),
        InstanceValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The file's basename stem, its name with the last extension stripped.
fn file_stem(rel_path: &Path) -> Option<&str> {
    rel_path.file_stem().and_then(|s| s.to_str())
}

/// The repo-relative directory segments the file sits in.
fn dir_segments(rel_path: &Path) -> Vec<&str> {
    match rel_path.parent() {
        Some(p) => p
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect(),
        None => Vec::new(),
    }
}

/// Match a static glob against directory segments, `*` one segment, `**` zero or
/// more, anchored at both ends (the glob names the directory the file sits in).
fn glob_matches(glob: &[GlobSegment], dir: &[&str]) -> bool {
    match glob.split_first() {
        None => dir.is_empty(),
        Some((GlobSegment::DoubleStar, rest)) => {
            (0..=dir.len()).any(|i| glob_matches(rest, &dir[i..]))
        }
        Some((GlobSegment::Star, rest)) => !dir.is_empty() && glob_matches(rest, &dir[1..]),
        Some((GlobSegment::Literal(lit), rest)) => {
            !dir.is_empty() && dir[0] == lit && glob_matches(rest, &dir[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_graph;
    use crate::instance::TypeClaim;
    use crate::typedef::{parse_type_def, TypeDef, TypeNameClaim};
    use au_diagnostics::ByteRange;
    use std::path::Path;

    fn def(name: &str, src: &str) -> TypeDef {
        let docs = au_parser::yaml::parse(src).unwrap();
        parse_type_def(
            Path::new(&format!("/v/type/{name}.type.yaml")),
            src,
            0,
            &docs[0],
        )
        .type_def
        .unwrap()
    }

    fn claim(name: &str) -> TypeClaim {
        TypeClaim::Bare(TypeNameClaim::own(
            TypeName(name.into()),
            ByteRange::new(0, 0),
        ))
    }

    #[test]
    fn single_declared_location_resolves() {
        let g = build_graph(vec![def("plan", "location:\n  path: \"p/\"\n")]).graph;
        let locs = effective_locations(&g, None, &claim("plan"));
        assert_eq!(locs.len(), 1);
    }

    #[test]
    fn subtype_overrides_ancestor_whole_block() {
        // base declares one, leaf declares another; leaf (closest) wins, one result.
        let g = build_graph(vec![
            def("base", "location:\n  path: \"base/\"\n"),
            def("leaf", "extends: base\nlocation:\n  path: \"leaf/\"\n"),
        ])
        .graph;
        let locs = effective_locations(&g, None, &claim("leaf"));
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].1.path.as_ref().unwrap().raw, "leaf/");
    }

    #[test]
    fn inherited_location_surfaces_when_subtype_declares_none() {
        let g = build_graph(vec![
            def("base", "location:\n  path: \"base/\"\n"),
            def("leaf", "extends: base\nfields: {}\n"),
        ])
        .graph;
        let locs = effective_locations(&g, None, &claim("leaf"));
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].1.path.as_ref().unwrap().raw, "base/");
    }

    #[test]
    fn empty_block_suppresses_inherited() {
        // `location: {}` is a declared empty block: it shadows the ancestor and
        // constrains nothing.
        let g = build_graph(vec![
            def("base", "location:\n  path: \"base/\"\n"),
            def("leaf", "extends: base\nlocation: {}\n"),
        ])
        .graph;
        let locs = effective_locations(&g, None, &claim("leaf"));
        assert_eq!(locs.len(), 1);
        assert!(locs[0].1.path.is_none() && locs[0].1.name.is_none());
    }

    #[test]
    fn raw_mixin_of_unrelated_locations_returns_all() {
        let g = build_graph(vec![
            def("a", "location:\n  path: \"a/\"\n"),
            def("b", "location:\n  path: \"b/\"\n"),
        ])
        .graph;
        let mixin = TypeClaim::List {
            items: vec![
                TypeNameClaim::own(TypeName("a".into()), ByteRange::new(0, 0)),
                TypeNameClaim::own(TypeName("b".into()), ByteRange::new(0, 0)),
            ],
            value_span: ByteRange::new(0, 0),
        };
        assert_eq!(effective_locations(&g, None, &mixin).len(), 2);
    }

    #[test]
    fn token_equal_blocks_auto_unify() {
        let g = build_graph(vec![
            def("a", "location:\n  path: \"same/\"\n"),
            def("b", "location:\n  path: \"same/\"\n"),
        ])
        .graph;
        let mixin = TypeClaim::List {
            items: vec![
                TypeNameClaim::own(TypeName("a".into()), ByteRange::new(0, 0)),
                TypeNameClaim::own(TypeName("b".into()), ByteRange::new(0, 0)),
            ],
            value_span: ByteRange::new(0, 0),
        };
        assert_eq!(effective_locations(&g, None, &mixin).len(), 1);
    }

    #[test]
    fn no_location_is_empty() {
        let g = build_graph(vec![def("plain", "fields: {}\n")]).graph;
        assert!(effective_locations(&g, None, &claim("plain")).is_empty());
    }

    fn inst(path: &str, src: &str) -> crate::instance::Instance {
        let docs = au_parser::yaml::parse(src).unwrap();
        crate::instance::parse_instance(Path::new(path), src, 0, &docs[0])
            .instance
            .unwrap()
    }

    fn codes_of(d: &[au_diagnostics::Diagnostic]) -> Vec<&str> {
        d.iter().map(|x| x.code.as_str()).collect()
    }

    #[test]
    fn clean_placement_emits_nothing() {
        let g = build_graph(vec![def(
            "plan",
            "fields:\n  slug: String\nlocation:\n  name: \"${.type} - ${.slug}\"\n  path: \"plans/\"\n  fileType: md\n",
        )])
        .graph;
        let i = inst("/v/plans/plan - hello.md", "type: plan\nslug: hello\n");
        let d = validate_location(&g, None, &i, Path::new("plans/plan - hello.md"));
        assert!(d.is_empty(), "{:?}", codes_of(&d));
    }

    #[test]
    fn wrong_path_is_a_soft_mismatch_warning() {
        let g = build_graph(vec![def("plan", "location:\n  path: \"plans/\"\n")]).graph;
        let i = inst("/v/wrong/x.md", "type: plan\n");
        let d = validate_location(&g, None, &i, Path::new("wrong/x.md"));
        assert_eq!(codes_of(&d), vec!["location-mismatch"]);
        assert_eq!(d[0].severity, au_diagnostics::Severity::Warning);
    }

    #[test]
    fn wrong_name_render_is_a_mismatch() {
        let g = build_graph(vec![def(
            "note",
            "fields:\n  slug: String\nlocation:\n  name: \"${.slug}\"\n",
        )])
        .graph;
        let i = inst("/v/other.md", "type: note\nslug: hello\n");
        let d = validate_location(&g, None, &i, Path::new("other.md")); // stem "other" != "hello"
        assert_eq!(codes_of(&d), vec!["location-mismatch"]);
    }

    #[test]
    fn strict_violation_is_an_error() {
        let g = build_graph(vec![def(
            "gov",
            "location:\n  path: \"config/\"\n  strict: true\n",
        )])
        .graph;
        let i = inst("/v/elsewhere/gov.yaml", "type: gov\n");
        let d = validate_location(&g, None, &i, Path::new("elsewhere/gov.yaml"));
        assert_eq!(codes_of(&d), vec!["location-strict-violation"]);
        assert_eq!(d[0].severity, au_diagnostics::Severity::Error);
    }

    #[test]
    fn deep_glob_matches_nested_dir() {
        let g = build_graph(vec![def("plan", "location:\n  path: \"**/plan/\"\n")]).graph;
        let i = inst("/v/a/b/plan/x.md", "type: plan\n");
        let d = validate_location(&g, None, &i, Path::new("a/b/plan/x.md"));
        assert!(d.is_empty(), "{:?}", codes_of(&d));
    }

    #[test]
    fn mixin_partial_match_is_a_hint() {
        let g = build_graph(vec![
            def("a", "location:\n  path: \"aa/\"\n"),
            def("b", "location:\n  path: \"bb/\"\n"),
        ])
        .graph;
        // claims both; file sits in aa/, so it matches a, not b.
        let i = inst("/v/aa/x.md", "type:\n  - a\n  - b\n");
        let d = validate_location(&g, None, &i, Path::new("aa/x.md"));
        assert_eq!(codes_of(&d), vec!["location-partial-unmet"]);
        assert_eq!(d[0].severity, au_diagnostics::Severity::Hint);
    }

    #[test]
    fn mixin_matching_none_is_one_warning() {
        let g = build_graph(vec![
            def("a", "location:\n  path: \"aa/\"\n"),
            def("b", "location:\n  path: \"bb/\"\n"),
        ])
        .graph;
        let i = inst("/v/cc/x.md", "type:\n  - a\n  - b\n");
        let d = validate_location(&g, None, &i, Path::new("cc/x.md"));
        assert_eq!(codes_of(&d), vec!["location-mismatch"]);
    }

    #[test]
    fn cross_repo_claim_is_subject_to_the_peer_location() {
        use crate::resolution::{fold, PeerGraphResolver};
        use std::collections::BTreeMap;

        // base owns `note` with a location; app owns nothing and claims `note::base`.
        struct R {
            g: BTreeMap<String, crate::graph::TypeGraph>,
        }
        impl PeerGraphResolver for R {
            fn graph_of(&self, repo: &str) -> Option<&crate::graph::TypeGraph> {
                self.g.get(repo)
            }
        }
        let base = build_graph(vec![def(
            "note",
            "location:\n  path: \"notes/\"\n  name: \"${.type}\"\n",
        )])
        .graph;
        let app = build_graph(vec![]).graph;
        let r = R {
            g: BTreeMap::from([("app".into(), app.clone()), ("base".into(), base)]),
        };
        let rg = fold("app", &[(TypeName("note".into()), "base".into())], &r);

        let claim = TypeClaim::Bare(TypeNameClaim::parse("note::base", ByteRange::new(0, 0)));

        // The peer location resolves over the fold, `${.type}` renders the peer's name.
        let locs = effective_locations(&app, Some(&rg), &claim);
        assert_eq!(locs.len(), 1, "peer location resolved: {locs:?}");
        assert_eq!(locs[0].1.path.as_ref().unwrap().raw, "notes/");

        // Placement is checked relative to the CLAIMING repo's root.
        let i = inst("/app/notes/note.md", "type: note::base\n");
        let clean = validate_location(&app, Some(&rg), &i, Path::new("notes/note.md"));
        assert!(clean.is_empty(), "{:?}", codes_of(&clean));
        let wrong = validate_location(&app, Some(&rg), &i, Path::new("elsewhere/note.md"));
        assert_eq!(codes_of(&wrong), vec!["location-mismatch"]);

        // Without a resolution graph the `::repo` claim cannot resolve, so it is skipped.
        assert!(effective_locations(&app, None, &claim).is_empty());
    }
}
