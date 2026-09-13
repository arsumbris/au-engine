//! Property + scenario tests for the implicit-identity candidate scan
//! (spec §10). Locks the rules that must hold across any knowledge base and any
//! claim/frontmatter combination:
//!
//! 1. Closure exclusion: no surfaced candidate is in the scope's closure
//!    (§10.1(b) / §10.2(b)).
//! 2. Required-set saturation: every surfaced candidate's
//!    `satisfied_required` equals T's full required-field set (§10.1(a)) —
//!    the discriminator is set *size*, not match count.
//! 3. Tag-type silence: types with zero required fields never surface
//!    (§10.1's vacuous-match exclusion).
//! 4. Sealed-parent silence: sealed type-defs never surface as candidates
//!    (matching §3.3's unactionability).
//! 5. Ranking monotonicity: `rank` produces a list where
//!    `satisfied_required.len()` is non-increasing (§10.4).
//! 6. Scope handle well-formedness: every `inline_path` is either `""`
//!    (top-level, RFC 6901 root) or starts with `/` (a non-trivial
//!    pointer).
//! 7. Supersedes well-formedness: every name listed in `supersedes` is a
//!    type-name actually claimed at the candidate's scope (sealed-family
//!    conflicts come from real claims, never invented).

use std::collections::BTreeSet;

use au_core::{
    build_graph, closure_of, rank, scan, scan_top_level, Candidate, Instance, InstanceValue,
    TypeClaim, TypeDef, TypeGraph, TypeName, TypeNameClaim,
};
use au_diagnostics::ByteRange;
use au_testkit::{
    instance_bare, instance_field, instance_list, type_def_with_fields, type_def_with_parents,
};

fn fields(keys: &[&str]) -> Vec<au_core::InstanceField> {
    keys.iter()
        .map(|k| instance_field(k, InstanceValue::String("v".into())))
        .collect()
}

/// Compose a `TypeDef` that is both sealed AND carries fields. `type_def_sealed`
/// in au-testkit is fields-empty; `type_def_with_fields` doesn't take a sealed
/// list. Stitched together inline here to exercise the supersedes path
/// (sealed parent with an inherited required field → leaves surface).
fn sealed_with_fields(name: &str, fields: &[&str], branches: &[&str]) -> TypeDef {
    TypeDef {
        sealed: branches
            .iter()
            .map(|b| TypeNameClaim::own(TypeName((*b).into()), ByteRange::new(0, 0)))
            .collect(),
        ..type_def_with_fields(name, &[], fields)
    }
}

/// Reusable graph: orthogonal-overlap (note, summary), extras-satisfy
/// (deliverable), tag (tag-only), sealed family (decision +
/// decision.pending / decision.decided — `status` inherited from sealed
/// parent so leaves can actually surface as candidates), disjoint
/// non-sealed (maturity).
fn invariants_graph() -> TypeGraph {
    build_graph(vec![
        type_def_with_fields("note", &[], &["description"]),
        type_def_with_fields("summary", &[], &["description"]),
        type_def_with_fields("deliverable", &[], &["description", "audience"]),
        type_def_with_fields("tag-only", &[], &[]),
        sealed_with_fields(
            "decision",
            &["status"],
            &["decision.pending", "decision.decided"],
        ),
        type_def_with_parents("decision.pending", &["decision"]),
        type_def_with_parents("decision.decided", &["decision"]),
        type_def_with_fields("maturity", &[], &["audience"]),
    ])
    .graph
}

// ---- 1. Closure exclusion ---------------------------------------------

#[test]
fn no_candidate_in_scope_closure() {
    let g = invariants_graph();
    for claim in ["note", "summary", "deliverable", "maturity"] {
        let inst = instance_bare(claim, fields(&["description", "audience"]));
        let closure = closure_of(&g, &TypeName(claim.into()));
        for c in scan(&g, &inst) {
            assert!(
                !closure.contains(&c.type_name),
                "candidate {} surfaced but is in {}'s closure",
                c.type_name.as_str(),
                claim
            );
        }
    }
}

// ---- 2. Required-set saturation ---------------------------------------

#[test]
fn satisfied_required_equals_full_required_set() {
    let g = invariants_graph();
    let inst = instance_bare("note", fields(&["description", "audience"]));
    for c in scan(&g, &inst) {
        let td_closure = closure_of(&g, &c.type_name);
        let mut expected: BTreeSet<&str> = BTreeSet::new();
        for tn in &td_closure {
            if let Some(td) = g.get(tn) {
                for f in &td.fields {
                    if !f.optional {
                        expected.insert(f.name.as_str());
                    }
                }
            }
        }
        let got: BTreeSet<&str> = c.satisfied_required.iter().map(|n| n.as_str()).collect();
        assert_eq!(
            got,
            expected,
            "candidate {}: satisfied_required must equal full required-set of T's closure",
            c.type_name.as_str()
        );
    }
}

// ---- 3. Tag-type silence ----------------------------------------------

#[test]
fn tag_only_never_surfaces() {
    let g = invariants_graph();
    for claim in [
        "note",
        "summary",
        "deliverable",
        "maturity",
        "decision.pending",
    ] {
        let inst = instance_bare(claim, fields(&["description", "audience"]));
        for c in scan(&g, &inst) {
            assert_ne!(
                c.type_name.as_str(),
                "tag-only",
                "tag-only would match every file vacuously (§10.1 exclusion)"
            );
        }
    }
}

// ---- 4. Sealed-parent silence -----------------------------------------

#[test]
fn sealed_parents_never_surface() {
    let g = invariants_graph();
    for claim in ["note", "decision.pending", "maturity"] {
        let inst = instance_bare(claim, fields(&["description", "audience", "summary"]));
        for c in scan(&g, &inst) {
            assert!(
                !g.is_sealed(&c.type_name),
                "sealed type-def {} surfaced — unactionable per §3.3",
                c.type_name.as_str()
            );
        }
    }
}

// ---- 5. Ranking monotonicity ------------------------------------------

#[test]
fn rank_produces_non_increasing_required_size() {
    let g = invariants_graph();
    // Any-fields instance to maximize candidate density.
    let inst = instance_bare("maturity", fields(&["description", "audience", "summary"]));
    let mut cands = scan_top_level(&g, &inst);
    rank(&mut cands);
    for window in cands.windows(2) {
        assert!(
            window[0].satisfied_required.len() >= window[1].satisfied_required.len(),
            "ranking violates non-increasing required-set size: {} ({}) before {} ({})",
            window[0].type_name.as_str(),
            window[0].satisfied_required.len(),
            window[1].type_name.as_str(),
            window[1].satisfied_required.len()
        );
    }
}

// ---- 6. Scope handle well-formedness ----------------------------------

#[test]
fn every_inline_path_is_root_or_starts_with_slash() {
    let g = invariants_graph();
    let inst = instance_bare("maturity", fields(&["description", "audience", "summary"]));
    for c in scan(&g, &inst) {
        let p = c.scope.inline_path.as_str();
        assert!(
            p.is_empty() || p.starts_with('/'),
            "inline_path '{p}' violates RFC 6901: must be '' (root) or start with '/'"
        );
    }
}

// ---- 7. Supersedes well-formedness ------------------------------------

#[test]
fn supersedes_names_are_actually_claimed() {
    // Instance claims a leaf under sealed `decision`. Provide the
    // inherited `status` so sibling leaves actually surface.
    // The sibling candidate must list the actual claimed leaf in its
    // `supersedes` — never invent a name.
    let g = invariants_graph();
    let claim_names: BTreeSet<&str> = ["decision.pending"].into_iter().collect();
    let inst = instance_bare("decision.pending", fields(&["status"]));
    let cands = scan(&g, &inst);
    // Sanity: at least one candidate should surface here (decision.decided).
    assert!(cands
        .iter()
        .any(|c| c.type_name.as_str() == "decision.decided"));
    let mut saw_supersedes = false;
    for c in &cands {
        for sup in &c.supersedes {
            saw_supersedes = true;
            assert!(
                claim_names.contains(sup.as_str()),
                "supersedes entry '{}' is not actually claimed at the scope",
                sup.as_str()
            );
        }
    }
    assert!(
        saw_supersedes,
        "expected at least one non-empty supersedes entry to exercise the invariant"
    );
}

#[test]
fn supersedes_is_empty_in_disjoint_families() {
    // A candidate that doesn't share any sealed ancestor with the
    // scope's claims must have empty supersedes.
    let g = invariants_graph();
    // Instance claims `maturity` (non-sealed); candidate `deliverable`
    // also non-sealed. No sealed family in common → empty supersedes.
    let inst = instance_bare("maturity", fields(&["description", "audience"]));
    for c in scan(&g, &inst) {
        assert!(
            c.supersedes.is_empty(),
            "candidate {} unexpectedly has supersedes {:?} from a non-sealed scope",
            c.type_name.as_str(),
            c.supersedes
        );
    }
}

// ---- Catalog cross-check (exhaustive expected output) -----------------

#[test]
fn fires_iff_required_set_satisfied_and_not_in_closure() {
    // Exhaustive cross-check: for a known instance claim and field set,
    // the candidate set must equal the manually-computed expected set.
    // This catches accidental filter additions (or removals) in the
    // detector that the property invariants might miss.
    let g = invariants_graph();
    let inst = instance_bare("note", fields(&["description", "audience"]));
    let cands = scan(&g, &inst);
    let got: BTreeSet<&str> = cands
        .iter()
        .filter(|c| c.scope.is_top_level())
        .map(|c| c.type_name.as_str())
        .collect();
    // Top-level frontmatter has `description` + `audience`.
    // - `note` in closure → skip
    // - `summary` (req: description) → satisfied, not in closure → surface
    // - `deliverable` (req: description, audience) → satisfied → surface
    // - `tag-only` → vacuous → skip
    // - `decision` → sealed → skip
    // - `decision.pending` (req: status via parent — NOT satisfied) → skip
    // - `decision.decided` → same as decision.pending → skip
    // - `maturity` (req: audience) → satisfied → surface
    let expected: BTreeSet<&str> = ["summary", "deliverable", "maturity"].into_iter().collect();
    assert_eq!(got, expected);
}

// ---- Determinism proptest ---------------------------------------------

use proptest::prelude::*;

proptest! {
    /// Scan output is a pure function of (graph, instance). Two calls
    /// on the same input must produce byte-identical Vec<Candidate>.
    #[test]
    fn scan_is_deterministic(
        field_keys in prop::collection::vec(
            prop::sample::select(vec!["description", "audience", "summary", "other"]),
            0..6,
        ),
        claim in prop::sample::select(vec!["note", "summary", "deliverable", "maturity"]),
    ) {
        let g = invariants_graph();
        let inst = instance_bare(claim, fields(&field_keys));
        let a = scan(&g, &inst);
        let b = scan(&g, &inst);
        prop_assert_eq!(a, b);
    }

    /// Mixin claim equivalence: scanning `type: [a]` and `type: a` must
    /// produce candidates with the same type-names + scopes (modulo
    /// claim-side path differences, which we don't compare). Locks the
    /// "1-element list == bare" semantic from spec §1.3.
    #[test]
    fn singleton_list_and_bare_claim_surface_same_types(
        field_keys in prop::collection::vec(
            prop::sample::select(vec!["description", "audience", "summary"]),
            0..4,
        ),
        claim in prop::sample::select(vec!["note", "summary", "deliverable", "maturity"]),
    ) {
        let g = invariants_graph();
        let inst_bare = instance_bare(claim, fields(&field_keys));
        let inst_list = instance_list(&[claim], fields(&field_keys));
        let cands_bare = scan(&g, &inst_bare);
        let cands_list = scan(&g, &inst_list);
        let names_bare: Vec<&str> = cands_bare.iter().map(|c| c.type_name.as_str()).collect();
        let names_list: Vec<&str> = cands_list.iter().map(|c| c.type_name.as_str()).collect();
        prop_assert_eq!(names_bare, names_list);
    }

    /// Adding an extra field to the frontmatter can only ever expand
    /// the candidate set (never shrink it) — extras-satisfy is
    /// monotonic. If T's required set was satisfied without the extra,
    /// it stays satisfied with it.
    #[test]
    fn adding_a_field_is_monotone(
        base_keys in prop::collection::vec(
            prop::sample::select(vec!["description", "audience", "summary"]),
            0..3,
        ),
        extra in prop::sample::select(vec!["description", "audience", "summary", "extra"]),
        claim in prop::sample::select(vec!["note", "maturity"]),
    ) {
        let g = invariants_graph();
        let inst_base = instance_bare(claim, fields(&base_keys));
        let mut with_extra = base_keys.clone();
        with_extra.push(extra);
        let inst_with = instance_bare(claim, fields(&with_extra));
        let cands_base = scan(&g, &inst_base);
        let cands_with = scan(&g, &inst_with);
        let base_names: BTreeSet<&str> = cands_base.iter().map(|c| c.type_name.as_str()).collect();
        let with_names: BTreeSet<&str> = cands_with.iter().map(|c| c.type_name.as_str()).collect();
        prop_assert!(
            base_names.is_subset(&with_names),
            "adding field '{extra}' to {claim} fixture dropped candidate(s); base={base_names:?}, with={with_names:?}"
        );
    }
}

// Silence unused imports for `Candidate` / `Instance` when proptest! is
// the only consumer of certain types.
#[allow(dead_code)]
fn _force_use(_: Candidate, _: Instance, _: TypeClaim) {}
