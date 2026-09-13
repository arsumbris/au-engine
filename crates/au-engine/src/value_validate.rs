//! Validate a transient typed value against a named type-def.
//!
//! A value is not a file. The engine validates files. To give a value the
//! verdict a file would get, synthesize a frontmatter document from the value
//! plus the named type claim, then run the normal parse + validate pipeline.
//! Diagnostic spans index the synthesized document, not the caller's value.

use std::collections::BTreeSet;
use std::path::Path;

use au_core::{parse_instance, validate, Instance, InstanceParseResult};
use au_diagnostics::{Diagnostic, LineIndex, Span};
use au_parser::yaml::parse;
use au_references::RepoIndex;
use serde_json::Value;

use crate::build::ContextMaps;
use crate::diagnostics::{duplicate_key_diags, sort_diagnostics};
use crate::ir::KnowledgeBase;
use crate::parse::FileParse;

/// The synthetic path stamped on a value's diagnostics. Spans index a
/// synthesized frontmatter document, not a real file on disk.
pub(crate) const VALUE_PATH: &str = "<value>";

/// Synthesize a frontmatter document from a named type-def and a JSON value.
///
/// JSON is a subset of YAML, so each field renders as `key: <compact-json>`,
/// one line each, and the existing YAML parser reads back the same structure.
/// The claim is injected as a leading `type:` line for an object value.
///
/// A non-object value renders as-is, so `parse_instance` reports it is not a
/// mapping. A `type` key inside the value that MATCHES the claim is tolerated
/// and dropped, so pasting the on-disk instance shape (which carries its own
/// `type:`) does not manufacture a spurious duplicate key. A MISMATCHING `type`
/// is left in place, so a value contradicting the asserted type still surfaces
/// as the duplicate key it is.
fn synthesize_frontmatter(type_name: &str, value: &Value) -> String {
    let Value::Object(fields) = value else {
        return value.to_string();
    };
    let mut src = String::new();
    src.push_str("type: ");
    // JSON-encoding the name quotes and escapes it into a valid YAML scalar.
    src.push_str(&Value::String(type_name.to_string()).to_string());
    src.push('\n');
    for (key, val) in fields {
        // Tolerate a `type` key that equals the asserted claim: it is redundant
        // with the injected line, so emitting it would fire a spurious
        // `duplicate-key 'type'` ahead of the real diagnostic. A mismatching
        // `type` falls through and still collides, a real signal.
        if key == "type" && *val == Value::String(type_name.to_string()) {
            continue;
        }
        src.push_str(&Value::String(key.clone()).to_string());
        src.push_str(": ");
        src.push_str(&val.to_string());
        src.push('\n');
    }
    src
}

/// Value keys not declared by the identity's effective shape. Advisory:
/// undeclared fields are LEGAL under open-world validation, so this is not a
/// diagnostic, it only surfaces them in the response so a caller can spot a
/// typo'd extra. The injected `type` claim and qualified `field{origin}` keys are
/// not plain fields, so they are excluded (a qualified key carries a `{`). Sorted
/// for determinism.
fn undeclared_value_fields(value: &Value, declared: &BTreeSet<String>) -> Vec<String> {
    let Value::Object(fields) = value else {
        return Vec::new();
    };
    let mut out: Vec<String> = fields
        .keys()
        .filter(|k| k.as_str() != "type" && !k.contains('{') && !declared.contains(k.as_str()))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Parse a synthesized frontmatter document into an `Instance`.
///
/// The source is generated JSON, always valid YAML, so a parse failure is
/// unreachable and degrades to an empty result rather than a panic. Runs the
/// duplicate-key scan the file pipeline runs, so a duplicated key surfaces the
/// same way a file's would.
fn parse_synthesized(source: &str) -> InstanceParseResult {
    let path = Path::new(VALUE_PATH);
    let Ok(docs) = parse(source) else {
        return InstanceParseResult::default();
    };
    let Some(doc) = docs.first() else {
        return InstanceParseResult::default();
    };
    let mut diagnostics = duplicate_key_diags(path, source, source, 0);
    let parsed = parse_instance(path, source, 0, doc);
    diagnostics.extend(parsed.diagnostics);
    InstanceParseResult {
        instance: parsed.instance,
        diagnostics,
        doc_links: parsed.doc_links,
    }
}

/// Validate a transient value against a named type-def, as if it were a file's
/// frontmatter claiming `type_name`.
///
/// Returns the diagnostics a file with that frontmatter would get: same
/// catalog, same open-world stance. The named type-def resolves through the
/// held knowledge base's graph; an unknown name yields the unresolved-claim diagnostic
/// a file gets, not a transport error. Frontmatter only, a value has no body.
///
/// Spans on `<value>` diagnostics carry line/col into the synthesized
/// document; related spans into real knowledge base files carry their file's line/col.
pub(crate) fn validate_value_diagnostics(
    kb: &KnowledgeBase,
    type_name: &str,
    value: &Value,
    repo: Option<&str>,
) -> Vec<Diagnostic> {
    let source = synthesize_frontmatter(type_name, value);
    let parsed = parse_synthesized(&source);
    validate_scoped_instance(
        kb,
        parsed.instance,
        parsed.diagnostics,
        &source,
        Path::new(VALUE_PATH),
        repo,
    )
}

/// Validate a transient value against every mounted identity a type name
/// denotes, one verdict per identity.
///
/// MULTI-FIT: a BARE `type_name` conflates across mounted repos, so it returns
/// one verdict PER owning identity rather than guessing a winner. A `repo` arg,
/// or a `::repo` in the name, narrows to the 0-or-1 identity that repo owns.
/// Each fit validates the BARE name in its owning member's scope, so the shape
/// checked is the one that identity actually defines.
///
/// NO FIT yields ONE verdict with a null identity, carrying the
/// `unknown-type-claim` a file claiming an absent type gets — plus any
/// structural diagnostics the value itself has (a non-mapping value, a
/// duplicated key), so an unknown name never hides a malformed value. This
/// fails CLOSED; see [`crate::wire::ValueVerdictView`] for why an empty result
/// would not.
pub(crate) fn validate_value_verdicts(
    kb: &KnowledgeBase,
    type_name: &str,
    value: &Value,
    repo: Option<&str>,
) -> Vec<crate::wire::ValueVerdictView> {
    let fits = crate::wire::name_fits(kb, type_name, repo);
    if fits.is_empty() {
        // Scope the miss to the repo that was asked for, so an unmounted or
        // typo'd `repo` reports against that repo's (absent) vocabulary rather
        // than silently falling back to another's.
        let claim = au_core::TypeNameClaim::parse(type_name, au_diagnostics::ByteRange::new(0, 0));
        let want_repo = claim.repo.as_deref().or(repo);
        return vec![crate::wire::ValueVerdictView {
            identity: None,
            diagnostics: validate_value_diagnostics(kb, claim.name.as_str(), value, want_repo),
            // A null identity has no shape to compare against.
            undeclared_fields: Vec::new(),
        }];
    }
    fits.into_iter()
        .map(|fit| {
            // Declared field set for THIS identity, the same effective closure
            // `type_closure` reports, so a cross-repo folded parent field is not
            // mistaken for an undeclared extra.
            let undeclared = kb
                .graph_for_repo(fit.member.as_str())
                .map(|graph| {
                    let rg = kb.resolution_graphs.of(&fit.member);
                    let declared: BTreeSet<String> =
                        crate::wire::closure_fields(kb, graph, rg, fit.member.as_str(), &fit.name)
                            .into_iter()
                            .map(|cf| cf.field.name)
                            .collect();
                    undeclared_value_fields(value, &declared)
                })
                .unwrap_or_default();
            crate::wire::ValueVerdictView {
                diagnostics: validate_value_diagnostics(
                    kb,
                    fit.name.as_str(),
                    value,
                    Some(fit.member.as_str()),
                ),
                identity: Some(fit.identity),
                undeclared_fields: undeclared,
            }
        })
        .collect()
}

/// Field-shape validate a device-global-or-scoped file against a NAMED type,
/// parsing `bytes` as a typed instance with a BARE `type_name` claim stamped from
/// the caller (not a `type:` in the file), and resolving that name in
/// `repo_scope`'s graph.
///
/// Two callers, two scopes:
/// - the `device_config` read passes `Some(BUILTIN_ENGINE_REPO)`, so a hardwired
///   `au.engine.*` def (`repos.yaml` / `workspaces.yaml`) resolves in the builtin
///   graph.
/// - the scoped config channel passes the served entry (machine scope) or the
///   owning member (repo scope), so a CONSUMER type resolves in that graph; a
///   `foo::repo` type folds the peer as usual.
///
/// The file sits OUTSIDE the walked graph either way, so it is not a knowledge
/// base node, but it is a REAL file a consumer authors, so the diagnostics carry
/// REAL byte spans into the actual file (not the synthetic spans a transient value
/// gets). Field-shape only: the file has no reference fields and no repo index to
/// walk, so the graph-aware passes are inert. An unparseable or non-UTF-8 file
/// returns its structural diagnostics, the same verdict a walked instance file
/// gets. A `type_name` absent from `repo_scope`'s graph gets the usual
/// `unknown-type-claim`; the config channel pre-checks resolution and substitutes
/// its own advisory before reaching here, so this stays the generic verdict.
pub(crate) fn validate_device_file(
    kb: &KnowledgeBase,
    path: &Path,
    bytes: &[u8],
    type_name: &str,
    repo_scope: Option<&str>,
) -> Vec<Diagnostic> {
    // `String::from_utf8_lossy` is only for the line/col index; the parse below
    // reads the raw bytes and reports non-UTF-8 itself, so the lossy replacement
    // never reaches a diagnostic message.
    let source = String::from_utf8_lossy(bytes);
    match crate::parse::parse_engine_schema_instance(path, bytes, type_name, None) {
        FileParse::Instance {
            instance,
            diagnostics,
            ..
        } => validate_scoped_instance(kb, instance, diagnostics, &source, path, repo_scope),
        FileParse::Unparsed { mut diagnostics } => {
            attach_line_cols(&mut diagnostics, &source, path, kb);
            sort_diagnostics(&mut diagnostics);
            diagnostics
        }
        // `parse_engine_schema_instance` only ever yields `Instance` or
        // `Unparsed`; a type-def / note classification cannot arise for these
        // stamped pure-YAML files.
        _ => Vec::new(),
    }
}

/// Validate a candidate FILE content against the held knowledge base, for the
/// nested-record mutations' `on_invalid` gate. Parses `content` as a real
/// instance file at `path`, validates in the path's owning-repo scope, and
/// returns the diagnostics with real byte spans.
///
/// Frontmatter only: a `.md` body's diagnostics are not counted, so the gate's
/// error-count delta is a frontmatter delta. au-workflow's plans are bodyless
/// typed-YAML, so this is exact for them; a body-bearing editor is a follow-up.
pub(crate) fn validate_instance_content(
    kb: &KnowledgeBase,
    path: &Path,
    content: &str,
) -> Vec<Diagnostic> {
    let split = match au_parser::split_frontmatter(content) {
        Ok(Some(s)) => s,
        Ok(None) => au_parser::whole_as_frontmatter(content),
        // Unterminated frontmatter: a structural break the caller's own
        // re-parse already rejected, so an empty verdict here is fine.
        Err(_) => return Vec::new(),
    };
    let offset = split.frontmatter_range.start;
    let Ok(docs) = parse(split.frontmatter) else {
        return Vec::new();
    };
    let Some(doc) = docs.first() else {
        return Vec::new();
    };
    let mut structural = duplicate_key_diags(path, content, split.frontmatter, offset);
    let parsed = parse_instance(path, content, offset, doc);
    structural.extend(parsed.diagnostics);
    let repo = kb.repos.repo_of(path).map(|r| r.name.as_str().to_string());
    validate_scoped_instance(
        kb,
        parsed.instance,
        structural,
        content,
        path,
        repo.as_deref(),
    )
}

/// The count of `error`-severity diagnostics `content` would get as a file at
/// `path`. The `on_invalid` gate compares the count before and after a splice.
pub(crate) fn instance_error_count(kb: &KnowledgeBase, path: &Path, content: &str) -> usize {
    validate_instance_content(kb, path, content)
        .iter()
        .filter(|d| d.severity == au_diagnostics::Severity::Error)
        .count()
}

/// Validate an already-parsed instance in a repo's scope, the shared core of the
/// transient-value and device-global-file paths. `source` / `source_path` locate
/// the instance's own diagnostics (a synthesized document for a value, the real
/// file for a device-global config); related spans into knowledge base files keep their
/// own coordinates.
fn validate_scoped_instance(
    kb: &KnowledgeBase,
    instance: Option<Instance>,
    diagnostics: Vec<Diagnostic>,
    source: &str,
    source_path: &Path,
    repo: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = diagnostics;

    // A value validates against `type_name` in a repo's scope: `None` is the
    // root repo (the no-scope default), `Some(name)` that repo's graph. An
    // unknown repo resolves against the empty graph, so the type is absent and
    // the value gets `unknown-type-claim`, the same verdict a file claiming an
    // absent type gets.
    let graph = match repo {
        None => kb.root_graph(),
        Some(name) => kb.graph_for_repo(name).unwrap_or_else(|| kb.graphs.empty()),
    };
    // An unknown repo has no build outcome. `Complete` is the deliberate
    // default, not a fallback: it lets validation run against the empty graph
    // so the value gets `unknown-type-claim`, rather than `aborted()` skipping
    // the type verdict entirely. The empty graph above and `Complete` here are
    // the same "unknown repo, validate as if a healthy empty vocabulary" stance.
    let outcome = match repo {
        None => kb.root_outcome(),
        Some(name) => kb
            .outcome_for_repo(name)
            .unwrap_or(crate::ir::BuildOutcome::Complete),
    };

    // Per-instance validation runs only when the build reached it. An aborted
    // build (bad vocabulary or meta) leaves files unvalidated, so a value gets
    // the same treatment: only its own structural diagnostics, no type verdict
    // against an incomplete graph.
    if let (Some(instance), false) = (instance, outcome.aborted()) {
        let maps = ContextMaps::assemble(&kb.catalog, &kb.graphs, &kb.repos, &kb.resolution_graphs);
        // Reference targets resolve against the scoped repo's index; an unknown
        // repo gets the empty index, so cross-file references just don't
        // resolve, the value's shape still validates.
        let empty;
        let index = match repo {
            None => kb.root_index(),
            Some(name) => match kb.index_for_repo(name) {
                Some(ix) => ix,
                None => {
                    empty = RepoIndex::default();
                    &empty
                }
            },
        };
        // A `::repo` typed reference in the value is checked across the
        // boundary, the same as a file's frontmatter: the resolver reaches into
        // the named repo's graph for the `(name, canonical-hash)` identity check.
        // A target repo whose graph aborted is skipped, its closure is unreliable.
        let graph_aborted: std::collections::BTreeSet<crate::repo::RepoName> = kb
            .outcomes
            .iter()
            .filter(|(_, o)| **o == crate::ir::BuildOutcome::AbortedAtGraph)
            .map(|(name, _)| name.clone())
            .collect();
        let target_claims = crate::crossref::CatalogTargetClaims {
            catalog: &kb.catalog,
        };
        let cross_repo_resolver = crate::crossref::EngineCrossRepoResolver {
            repos: &kb.repos,
            indexes: &kb.indexes,
            graphs: &kb.graphs,
            workspaces: &kb.workspaces,
            graph_aborted: &graph_aborted,
            resolution_graphs: &kb.resolution_graphs,
            target_claims: &target_claims,
        };
        // The resolution graph for the scoped repo, so a value claiming a peer
        // type validates against the folded fields. Root-scope value validation
        // falls back to the own graph (import-aware root-scope values are a
        // follow-up, no consumer pulls them yet).
        let resolution = match repo {
            None => None,
            Some(name) => kb
                .resolution_graphs
                .of(&crate::repo::RepoName(name.to_string())),
        };
        let ctx = maps.context(graph, index, Some(&cross_repo_resolver), resolution);
        diagnostics.extend(validate(&ctx, &instance));
    }

    attach_line_cols(&mut diagnostics, source, source_path, kb);
    sort_diagnostics(&mut diagnostics);
    diagnostics
}

/// Attach line/col to an instance's diagnostics. Spans on `source_path` index
/// `source` (the synthesized value document, or the real device-global file);
/// related spans into knowledge base files index their own file.
fn attach_line_cols(
    diagnostics: &mut [Diagnostic],
    source: &str,
    source_path: &Path,
    kb: &KnowledgeBase,
) {
    let primary = LineIndex::new(source.as_bytes());
    let attach = |span: &mut Span| {
        if span.file == source_path {
            span.attach_line_col(&primary);
        } else if let Some(index) = kb
            .catalog
            .get(&span.file)
            .and_then(|e| e.line_index.as_deref())
        {
            span.attach_line_col(index);
        }
    };
    for d in diagnostics.iter_mut() {
        attach(&mut d.span);
        for r in d.related.iter_mut() {
            attach(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_core::{InstanceValue, TypeClaim};
    use serde_json::json;

    #[test]
    fn object_value_synthesizes_a_typed_mapping() {
        let src = synthesize_frontmatter("toolInput.read_file", &json!({ "file_path": "a.md" }));
        assert_eq!(
            src,
            "type: \"toolInput.read_file\"\n\"file_path\": \"a.md\"\n"
        );
    }

    #[test]
    fn parsed_instance_carries_the_named_claim_and_fields() {
        let src = synthesize_frontmatter("toolInput.read_file", &json!({ "file_path": "a.md" }));
        let res = parse_synthesized(&src);
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        let inst = res.instance.expect("instance");
        match &inst.type_claim {
            TypeClaim::Bare(c) => assert_eq!(c.name.0, "toolInput.read_file"),
            other => panic!("expected bare claim, got {other:?}"),
        }
        assert_eq!(inst.fields.len(), 1);
        assert_eq!(inst.fields[0].key, "file_path");
        assert_eq!(inst.fields[0].value, InstanceValue::String("a.md".into()));
    }

    #[test]
    fn nested_and_scalar_field_values_round_trip() {
        let src = synthesize_frontmatter(
            "t",
            &json!({ "n": 7, "flag": true, "list": [1, 2], "rec": { "a": "b" } }),
        );
        let inst = parse_synthesized(&src).instance.expect("instance");
        let by_key = |k: &str| {
            inst.fields
                .iter()
                .find(|f| f.key == k)
                .map(|f| &f.value)
                .unwrap()
        };
        assert_eq!(by_key("n"), &InstanceValue::Integer(7));
        assert_eq!(by_key("flag"), &InstanceValue::Boolean(true));
        assert!(matches!(by_key("list"), InstanceValue::Sequence(s) if s.len() == 2));
        assert!(matches!(by_key("rec"), InstanceValue::Mapping(_)));
    }

    #[test]
    fn non_object_value_is_not_a_mapping() {
        let res = parse_synthesized(&synthesize_frontmatter("t", &json!(42)));
        assert!(res.instance.is_none());
        assert!(
            res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "instance-not-a-mapping"),
            "{:?}",
            res.diagnostics
        );
    }

    #[test]
    fn a_mismatching_type_field_inside_the_value_still_duplicates_the_claim() {
        // A value asserting a DIFFERENT type than the one being validated
        // against is a real contradiction, so it still surfaces as a duplicate.
        let res = parse_synthesized(&synthesize_frontmatter(
            "t",
            &json!({ "type": "other", "x": 1 }),
        ));
        assert!(
            res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "duplicate-key-in-mapping"),
            "{:?}",
            res.diagnostics
        );
    }

    #[test]
    fn a_matching_type_field_inside_the_value_is_tolerated() {
        // Pasting the on-disk instance shape carries its own `type:`. A claim
        // that matches must not manufacture a spurious `duplicate-key 'type'`
        // ahead of the real diagnostic.
        let src = synthesize_frontmatter("t", &json!({ "type": "t", "x": 1 }));
        assert_eq!(src, "type: \"t\"\n\"x\": 1\n");
        let res = parse_synthesized(&src);
        assert!(
            !res.diagnostics
                .iter()
                .any(|d| d.code.as_str() == "duplicate-key-in-mapping"),
            "matching type claim must be tolerated, got {:?}",
            res.diagnostics
        );
        let inst = res.instance.expect("instance");
        assert_eq!(inst.fields.len(), 1);
        assert_eq!(inst.fields[0].key, "x");
    }
}
