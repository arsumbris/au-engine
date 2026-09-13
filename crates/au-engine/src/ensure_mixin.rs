//! The ensure-mixin write directive's DECISION: given the held knowledge base and
//! a file's would-be-written content, decide whether a `::repo` mixin can be added
//! to the file's `type:` claim, and if so produce the spliced content.
//!
//! The pure byte-splice lives in [`crate::mutate::splice_ensure_mixin`]; this layer
//! adds the resolved-graph semantics the spec requires: a closure no-op, the
//! cross-repo peer gate, and the no-new-error value gate.
//! See [[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]].
//!
//! Width-only subtyping is what makes a single-file gate sound: adding a claim only
//! WIDENS the target's closure, so it can introduce an error on the TARGET alone,
//! never on another file (a `T*` reference that resolved stays resolved). So the
//! gate validates the target instance twice — its baseline claim and its candidate
//! claim — and blocks only on an error the candidate gains.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

use au_core::{
    folded_closure_ids, parse_instance, validate, Instance, MetaMarker, RefData, TypeName,
    TypeNameClaim, ValidateContext,
};
use au_diagnostics::{ByteRange, Severity};

use crate::ir::KnowledgeBase;
use crate::mutate::{splice_ensure_mixin, MixinSpliceOutcome};

/// The decision for one mixin on one write.
#[derive(Debug)]
pub(crate) enum MixinDecision {
    /// The claim already resolves to include the mixin (directly or by closure), or
    /// the name is already authored on the claim. Nothing to write.
    NoOp,
    /// The mixin can be added cleanly. The spliced would-be content is carried, so
    /// the caller writes it into the same commit.
    Apply(String),
    /// The mixin cannot be added without introducing a new error (an unresolvable
    /// or non-dependency repo, a non-claimable target, a collision, a newly-unmet
    /// required field). The reason is caller-facing: strict rejects the write with
    /// it, lenient reports the skip.
    UnAppliable(String),
}

/// Decide the fate of one `mixin` (a `::repo`-qualified type name in authored form)
/// on the file `target`, whose would-be-written bytes are `content` (the primary
/// write plus any stamps, before the mixin).
pub(crate) fn decide(
    kb: &KnowledgeBase,
    target: &Path,
    content: &str,
    mixin: &str,
) -> MixinDecision {
    let Some(repo) = kb.repos.repo_of(target) else {
        return MixinDecision::UnAppliable(format!(
            "target {} is in no known repo, so its type vocabulary cannot be resolved",
            target.display()
        ));
    };
    let repo_name = repo.name.as_str().to_string();
    let claim = TypeNameClaim::parse(mixin, ByteRange::new(0, 0));
    let held_rg = kb.resolution_graphs.of(&repo.name);

    // Closure no-op: the mixin's identity is already in the current claim's closure
    // (it is claimed directly, or a claimed type extends it), so there is nothing to
    // add. A qualified mixin resolves over the HELD resolution graph (if it is
    // already reachable, its peer type is folded); a bare mixin resolves over the
    // repo's own graph, which a non-importing repo has when it has no resolution
    // graph at all.
    if let Some(current) = parse_content(target, content) {
        if let Some(rg) = held_rg {
            if let Some(id) = rg.resolve_authored(&claim.name, claim.repo.as_deref()) {
                if folded_closure_ids(rg, &current.type_claim).contains(id) {
                    return MixinDecision::NoOp;
                }
            }
        }
        if claim.repo.is_none() {
            let own_graph = kb.graph_for_repo(&repo_name);
            let in_closure = own_graph.is_some_and(|g| {
                current
                    .type_claim
                    .iter()
                    .any(|c| au_core::closure_of(g, &c.name).contains(&claim.name))
            });
            if in_closure {
                return MixinDecision::NoOp;
            }
        }
    }

    // The cross-repo peer gate (mode 1): a qualified mixin must resolve to a
    // declared, present dependency that owns the type. au-core's value gate below
    // DEFERS a qualified claim, so this engine-side gate owns the resolution
    // failures. A bare mixin has no `::repo` and falls to the value gate.
    if let Some(repo_q) = &claim.repo {
        if let Some(reason) = crate::crosstype::gate_mixin_repo(
            target,
            claim.name.as_str(),
            repo_q,
            &kb.repos,
            &kb.graphs,
            &kb.workspaces,
        ) {
            return MixinDecision::UnAppliable(reason);
        }
    }

    // Splice the mixin into the claim. `AlreadyPresent` is the literal-name no-op;
    // a structural reject (a non-mapping frontmatter) cannot carry a claim at all.
    let candidate = match splice_ensure_mixin(content, target, mixin) {
        Ok(MixinSpliceOutcome::AlreadyPresent) => return MixinDecision::NoOp,
        Ok(MixinSpliceOutcome::Applied(c)) => c,
        Err(reject) => return MixinDecision::UnAppliable(reject.message),
    };

    // The value gate (modes 2-5): validate the target instance under the baseline
    // and the candidate claim over the augmented graph, and block on an error the
    // candidate gains. The augmented graph folds the mixin in on demand, so a first
    // promotion resolves the peer the held graph never imported.
    let seed_repo = claim.repo.clone().unwrap_or_else(|| repo_name.clone());
    let seeds = [(claim.name.clone(), seed_repo)];
    let augmented = match held_rg {
        Some(rg) => crate::resolution_build::extend_resolution_graph(
            rg, &kb.graphs, &kb.repos, &repo_name, &seeds,
        ),
        None => {
            crate::resolution_build::fold_repo_with_seeds(&kb.graphs, &kb.repos, &repo_name, &seeds)
        }
    };

    let Some(candidate_inst) = parse_content(target, &candidate) else {
        // The splice's structural check already guaranteed a parse, so this is
        // unreachable; fail closed rather than apply an ungated mixin.
        return MixinDecision::UnAppliable(
            "internal: the spliced content did not parse as an instance".to_string(),
        );
    };

    let baseline_errors = match parse_content(target, content) {
        // A note (no claim) is not validated, so a promotion's baseline is empty:
        // every error the created claim brings is new.
        Some(base_inst) => validate_errors(kb, &repo_name, &augmented, &base_inst),
        None => HashSet::new(),
    };
    let candidate_errors = validate_errors(kb, &repo_name, &augmented, &candidate_inst);

    let mut new: Vec<&(String, String)> = candidate_errors.difference(&baseline_errors).collect();
    if new.is_empty() {
        MixinDecision::Apply(candidate)
    } else {
        // Stable order so the reason string is deterministic across runs.
        new.sort();
        let reasons: Vec<&str> = new.iter().map(|(_, msg)| msg.as_str()).collect();
        MixinDecision::UnAppliable(format!(
            "adding the mixin would introduce: {}",
            reasons.join("; ")
        ))
    }
}

/// The ERROR-severity `(code, message)` set the target instance validates to over
/// `rg`. Keyed by `(code, message)` so a per-field error is distinguished from a
/// same-code error on another field, which the diff needs to attribute correctly.
/// Warnings, hints, and drift are advisory and excluded, per the open-world stance.
fn validate_errors(
    kb: &KnowledgeBase,
    repo_name: &str,
    rg: &au_core::ResolutionGraph,
    instance: &Instance,
) -> HashSet<(String, String)> {
    let Some(repo_index) = kb.index_for_repo(repo_name) else {
        return HashSet::new();
    };
    let ctx = ValidateContext {
        graph: kb
            .graph_for_repo(repo_name)
            .unwrap_or_else(|| kb.graph_for_path(&instance.source_path)),
        repo_index,
        // References do not change between baseline and candidate (only the claim
        // does), so a null reference resolver only yields advisory warnings that
        // cancel in the diff. The mixin's errors are all claim-level, resolved over
        // `resolution` below, not through this seam.
        ref_data: &NullRefData,
        cross_repo: None,
        resolution: Some(rg),
        meta_marker: Some(MetaMarker {
            name: crate::engine_schema::ENGINE_META_TYPE,
            repo: crate::engine_schema::BUILTIN_ENGINE_REPO,
        }),
    };
    validate(&ctx, instance)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| (d.code.as_str().to_string(), d.message))
        .collect()
}

/// Parse a content blob into an `Instance`, `None` when it carries no valid `type:`
/// claim (a note, an empty file) or does not parse. The gate treats a claimless
/// baseline as un-validated, which is what a note is.
fn parse_content(path: &Path, content: &str) -> Option<Instance> {
    let split = match au_parser::split_frontmatter(content) {
        Ok(Some(s)) => s,
        Ok(None) if au_parser::is_pure_yaml_instance_path(path) => {
            au_parser::whole_as_frontmatter(content)
        }
        _ => return None,
    };
    let offset = split.frontmatter_range.start;
    let docs = au_parser::yaml::parse(split.frontmatter).ok()?;
    let doc = docs.first()?;
    parse_instance(path, content, offset, doc).instance
}

/// A [`RefData`] that resolves nothing. The gate validates one instance in
/// isolation; reference targets do not change between the baseline and the
/// candidate, so a null resolver's advisory warnings cancel in the diff.
///
/// LOAD-BEARING ASSUMPTION: no reference-target-resolution diagnostic is
/// `Severity::Error` today (a dangling or wrong-typed ref target is a `warning` /
/// `drift`, open-world). So a mixin whose newly-required ref-typed field the file
/// mis-satisfies is not caught here — correctly, because it never gated anyway.
/// If a ref-target mismatch ever becomes an ERROR, this null resolver would let
/// such a mixin through; re-check the gate then (back the `ref_data` by the held
/// catalog, as the incremental path does).
struct NullRefData;

impl RefData for NullRefData {
    fn claims(&self, _: &Path) -> Option<Cow<'_, [TypeName]>> {
        None
    }
    fn body(&self, _: &Path) -> Option<&str> {
        None
    }
    fn record_targets(&self, _: &Path) -> Option<Cow<'_, au_core::RecordTargets>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;

    /// One repo. `note` (title), `marker` (a no-field tag), `tagged` (a required
    /// `tag`), `sub` (extends `marker`), and a colliding `x` pair.
    fn kb() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/v/type/marker.type.yaml", b"fields: {}\n".to_vec());
        fs.insert(
            "/v/type/tagged.type.yaml",
            b"fields:\n  tag: String\n".to_vec(),
        );
        fs.insert("/v/type/sub.type.yaml", b"extends: marker\n".to_vec());
        fs.insert(
            "/v/type/noteX.type.yaml",
            b"fields:\n  x: String\n".to_vec(),
        );
        fs.insert(
            "/v/type/colliderX.type.yaml",
            b"fields:\n  x: Number\n".to_vec(),
        );
        fs.insert("/v/a.md", b"---\ntype: note\ntitle: A\n---\n".to_vec());
        build(std::path::Path::new("/v"), &fs).unwrap()
    }

    fn md(name: &str) -> std::path::PathBuf {
        std::path::Path::new("/v").join(name)
    }

    #[test]
    fn clean_append_of_a_no_field_mixin_applies() {
        let kb = kb();
        let content = "---\ntype: note\ntitle: A\n---\n";
        match decide(&kb, &md("a.md"), content, "marker") {
            MixinDecision::Apply(c) => {
                assert!(c.contains("type: [note, marker]"), "got: {c}")
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn a_mixin_with_an_unmet_required_field_is_unappliable() {
        let kb = kb();
        // `tagged` requires `tag`, which the file does not supply.
        let content = "---\ntype: note\ntitle: A\n---\n";
        match decide(&kb, &md("a.md"), content, "tagged") {
            MixinDecision::UnAppliable(reason) => {
                assert!(reason.contains("tag"), "got: {reason}")
            }
            other => panic!("expected UnAppliable, got {other:?}"),
        }
    }

    #[test]
    fn a_mixin_supplying_its_required_field_applies() {
        let kb = kb();
        let content = "---\ntype: note\ntitle: A\ntag: t\n---\n";
        match decide(&kb, &md("a.md"), content, "tagged") {
            MixinDecision::Apply(c) => assert!(c.contains("type: [note, tagged]"), "got: {c}"),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn a_colliding_mixin_is_unappliable() {
        let kb = kb();
        // noteX declares `x: String`, colliderX declares `x: Number`: a collision.
        let content = "---\ntype: noteX\nx: s\n---\n";
        match decide(&kb, &md("b.md"), content, "colliderX") {
            MixinDecision::UnAppliable(_) => {}
            other => panic!("expected UnAppliable, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_own_mixin_is_unappliable() {
        let kb = kb();
        let content = "---\ntype: note\ntitle: A\n---\n";
        match decide(&kb, &md("a.md"), content, "ghost") {
            MixinDecision::UnAppliable(_) => {}
            other => panic!("expected UnAppliable, got {other:?}"),
        }
    }

    #[test]
    fn a_literally_present_mixin_is_a_noop() {
        let kb = kb();
        let content = "---\ntype: [note, marker]\ntitle: A\n---\n";
        assert!(matches!(
            decide(&kb, &md("a.md"), content, "marker"),
            MixinDecision::NoOp
        ));
    }

    #[test]
    fn a_mixin_reachable_through_a_parent_is_a_noop() {
        let kb = kb();
        // `sub` extends `marker`, so `marker` is already in the closure.
        let content = "---\ntype: sub\n---\n";
        assert!(matches!(
            decide(&kb, &md("a.md"), content, "marker"),
            MixinDecision::NoOp
        ));
    }

    #[test]
    fn creates_the_claim_on_a_claimless_note() {
        let kb = kb();
        // A note (no `type:`), promoted by a no-field mixin. `title` is an advisory
        // extra, not an error, so the promotion is clean.
        let content = "---\ntitle: A\n---\nbody\n";
        match decide(&kb, &md("n.md"), content, "marker") {
            MixinDecision::Apply(c) => {
                assert!(c.contains("type: marker"), "got: {c}");
                assert!(c.contains("title: A"), "the note's field survives: {c}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }
}
