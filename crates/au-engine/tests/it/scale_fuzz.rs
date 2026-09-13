//! Correctness fuzzing over the seeded `repogen` generator: a generated
//! cross-repo knowledge base builds cleanly and deterministically (build-twice
//! byte-identical), and an incremental recompute of a representative edit set
//! is byte-identical to a full build. The large-graph pressure the determinism
//! and incremental invariants never had.

use std::collections::BTreeSet;
use std::path::PathBuf;

use au_diagnostics::Severity;
use au_engine::{apply_recompute, build, recompute_dirty, KnowledgeBase};
use au_parser::RealFileSystem;
use au_testkit::repogen::{generate, generate_profile, Knobs, Profile};

/// The closure ids reachable from each instance's per-repo graph, sorted, the
/// identity projection the harness exists to confirm is stable. Keyed off the
/// knowledge base's OWN instances, so it stays correct after an add / delete.
fn closure_ids(v: &KnowledgeBase) -> Vec<(String, String, Option<u64>)> {
    let mut ids = Vec::new();
    for path in v.instances.keys() {
        let g = v.graph_for_path(path);
        for name in g.names() {
            ids.push((
                path.display().to_string(),
                name.as_str().to_string(),
                g.closure_id(name).map(|h| h.0),
            ));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// The reverse-dependency indices as sorted per-entry facts: the backlink index
/// and the closure-membership index. These are the two maps the incremental
/// apply patches by delta, and the two `assert_same_projections` omits. Iterated
/// entry by entry (the `OrdMap`s yield sorted keys), never whole-map
/// `Debug`-rendered, so the comparison is canonical rather than sensitive to the
/// red-black tree's insertion-order shape (an incremental patch and a full build
/// insert in different orders). Mirrors `KnowledgeBase::parity_facts`, which is
/// crate-internal and so unreachable from this integration binary.
fn reverse_index_facts(v: &KnowledgeBase) -> Vec<String> {
    let mut facts = Vec::new();
    for (p, bl) in &v.backlinks {
        facts.push(format!("backlinks {}|{bl:?}", p.display()));
    }
    for ((repo, ty), members) in &v.closure_members {
        let members: Vec<String> = members.iter().map(|p| p.display().to_string()).collect();
        facts.push(format!("closure_member {repo:?}|{ty:?}|{members:?}"));
    }
    facts
}

/// Each instance's resolved effective-shape as a sorted per-entry fact, the
/// resolved-layer projection the closure-id diff does not cover. `instances` is a
/// sorted `OrdMap`, so `.iter()` yields canonical key order, and the shape's own
/// structure is `BTreeMap`-backed, so its `Debug` is insertion-order-independent.
fn instance_facts(v: &KnowledgeBase) -> Vec<String> {
    v.instances
        .iter()
        .map(|(p, inst)| format!("instance {}|{:?}", p.display(), inst.effective_shape))
        .collect()
}

/// Each repo's folded resolution graph as a sorted per-entry fact, the cross-repo
/// fold layer no other projection covers. `resolution_graphs` is a `BTreeMap`, and
/// a `ResolutionGraph` is `BTreeMap`-backed, so both render canonically.
fn resolution_facts(v: &KnowledgeBase) -> Vec<String> {
    v.resolution_graphs
        .iter()
        .map(|(repo, rg)| format!("resolution {repo:?}|{rg:?}"))
        .collect()
}

/// Assert two builds are identical on the observable projections: diagnostics,
/// per-path content hashes, closure ids, the reverse-dependency indices
/// (backlinks + closure membership), each instance's resolved effective-shape,
/// and each repo's folded resolution graph.
fn assert_same_projections(a: &KnowledgeBase, b: &KnowledgeBase, ctx: &str) {
    assert!(
        a.diagnostics().eq(b.diagnostics()),
        "{ctx}: diagnostics differ"
    );
    assert_eq!(
        a.catalog.size(),
        b.catalog.size(),
        "{ctx}: catalog size differs"
    );
    for (path, entry) in &a.catalog {
        let other = b.catalog.get(path).map(|e| e.hash);
        assert_eq!(
            Some(entry.hash),
            other,
            "{ctx}: content hash differs for {path:?}"
        );
    }
    assert_eq!(closure_ids(a), closure_ids(b), "{ctx}: closure ids differ");
    assert_eq!(
        reverse_index_facts(a),
        reverse_index_facts(b),
        "{ctx}: reverse-dependency indices (backlinks / closure membership) differ"
    );
    assert_eq!(
        instance_facts(a),
        instance_facts(b),
        "{ctx}: resolved instance effective-shapes differ"
    );
    assert_eq!(
        resolution_facts(a),
        resolution_facts(b),
        "{ctx}: folded resolution graphs differ"
    );
}

#[test]
fn a_generated_medium_kb_builds_twice_byte_identical_and_clean() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let out = generate_profile(&root, Profile::Medium, 0xA11CE);

    let a = build(&out.entry, &RealFileSystem).expect("build a");
    let b = build(&out.entry, &RealFileSystem).expect("build b");

    // Well-formed: the generator emits NO error-severity diagnostics. The
    // cross-repo imports resolve and conform; the local links carry no typed
    // constraint. This is the generator's real validation (au-testkit cannot
    // build a knowledge base itself).
    let errors: Vec<_> = a
        .diagnostics()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| (d.code.as_str().to_string(), d.message.clone()))
        .collect();
    assert!(
        errors.is_empty(),
        "generated MEDIUM knowledge base has error diagnostics: {errors:?}"
    );

    // Deterministic across builds.
    assert_same_projections(&a, &b, "build-twice");

    // The MEDIUM profile actually exercises cross-repo edges, else the fuzz is
    // single-repo and misses the point.
    assert!(
        !out.xref_referrers.is_empty(),
        "MEDIUM profile produced cross-repo referrers"
    );
}

#[test]
fn incremental_recompute_equals_a_full_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let out = generate_profile(&root, Profile::Medium, 0xB0B);
    assert!(
        !out.xref_referrers.is_empty(),
        "need cross-repo edges to pressure the incremental cross-repo path"
    );

    let fs = RealFileSystem;
    let mut held = build(&out.entry, &fs).expect("baseline build");

    // Each step edits the on-disk tree, recomputes the dirty set incrementally
    // (asserting the incremental path is actually taken), and asserts the result
    // equals a full rebuild of the post-edit tree.
    let step = |held: &KnowledgeBase, dirty: PathBuf, ctx: &str| -> KnowledgeBase {
        let set: BTreeSet<PathBuf> = std::iter::once(dirty).collect();
        let rc = recompute_dirty(held, &set, &fs)
            .unwrap_or_else(|| panic!("{ctx}: expected an incremental recompute, got NeedsFull"));
        let inc = apply_recompute(held, rc);
        let full = build(&out.entry, &fs).expect("full rebuild");
        assert_same_projections(&inc, &full, ctx);
        inc
    };

    // 1. Edit a local instance's title (no dependents). repo0/n2 is a local t2.
    let n2 = root.join("repo0/n2.md");
    std::fs::write(
        &n2,
        "---\ntype: t2\ntitle: Edited\n---\n# Edited\n\nbody, see [[n3]].\n",
    )
    .unwrap();
    held = step(&held, n2, "edit-local-instance");

    // 2. Add a new instance to repo0 (a fresh path, past the generated range).
    let added = root.join("repo0/n999.md");
    std::fs::write(&added, "---\ntype: t0\ntitle: Added\n---\n# Added\n").unwrap();
    held = step(&held, added, "add-instance");

    // 3. Cross-repo blast radius: flip repo0/n0's type from t0 to t1. The importers
    //    reference `[[n0::repo0]]` through a `t0*` slot, so n0 becoming a t1 makes
    //    those cross-repo references non-conforming — the dependents must be
    //    re-validated incrementally to match a full build.
    let n0 = root.join("repo0/n0.md");
    std::fs::write(
        &n0,
        "---\ntype: t1\ntitle: Note 0\n---\n# Note 0\n\nbody 0, see [[n1]].\n",
    )
    .unwrap();
    let _ = step(&held, n0, "cross-repo-referrer-flip");
}

/// Regression for the referenced-name index gap: a docstring's dangling target,
/// once added, clears the source's stale warning incrementally, matching a full
/// build. The source is an INSTANCE, so the add flips it via `edge_flip_sources`
/// on the incremental path; a type-def source would force a full rebuild, also
/// correct, through the dependent-revalidation `None` fallback. Guards the
/// incremental-equals-full invariant for the docstring edge class, which the
/// generated fuzz above does not exercise (it emits no docstrings).
#[test]
fn adding_a_docstring_target_clears_the_warning_incrementally() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let fs = RealFileSystem;

    std::fs::create_dir_all(root.join(".arsumbris")).unwrap();
    std::fs::write(root.join(".arsumbris/repo.yaml"), "name: v\n").unwrap();
    std::fs::create_dir_all(root.join("type")).unwrap();
    std::fs::write(root.join("type/note.type.yaml"), "fields:\n  x?: String\n").unwrap();
    // An instance whose head docstring points at a target that does not exist yet.
    std::fs::write(
        root.join("a.md"),
        "---\n#: see [[policy]]\ntype: note\n---\n",
    )
    .unwrap();

    let held = build(&root, &fs).expect("baseline build");
    assert!(
        held.diagnostics()
            .any(|d| d.code.as_str() == "navigational-target-not-found"),
        "the dangling docstring link should warn before the target exists"
    );

    // Add the target; recompute incrementally.
    let added = root.join("policy.md");
    std::fs::write(&added, "policy notes\n").unwrap();
    let set: BTreeSet<PathBuf> = std::iter::once(added).collect();
    let rc = recompute_dirty(&held, &set, &fs)
        .expect("adding a note is an incremental recompute, not NeedsFull");
    let inc = apply_recompute(&held, rc);
    let full = build(&root, &fs).expect("full rebuild");

    assert!(
        inc.diagnostics().eq(full.diagnostics()),
        "incremental diagnostics must equal a full build after the add"
    );
    assert!(
        !inc.diagnostics()
            .any(|d| d.code.as_str() == "navigational-target-not-found"),
        "the docstring warning must clear once its target is added"
    );
}

/// A BULK co-dirtied edit is byte-identical to a full build, over the two shared-
/// target reverse indices the incremental apply patches by delta. This is the
/// correctness net for the per-hub dedup: it exercises N deltas all landing on
/// ONE shared target in a single apply pass, which `assert_same_projections`
/// alone would not catch (it omits `backlinks` and `closure_members`).
///
/// Two shapes, isolated:
/// - the HUB set: N referrers of one node `n0`, all re-touched at once, so every
///   backlink delta lands on `n0`'s inbound list.
/// - the COHORT set: N link-free instances sharing the closure key `(repo0, t0)`,
///   all retyped at once (a `t1` mixin), so every closure delta lands on
///   `(repo0, t1)`.
///
/// It passes with the sequential apply AND the batched apply, so it guards the
/// dedup refactor rather than merely measuring it.
#[test]
fn bulk_co_dirtied_recompute_equals_a_full_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // A single repo carrying both stressors: a hub (`n0` + 30 referrers) and a
    // link-free co-typed cohort (30 `c{i}` claiming `t0`). `types_per_repo: 2` so
    // the `t1` retype target exists.
    let knobs = Knobs {
        repo_count: 1,
        types_per_repo: 2,
        closure_depth: 1,
        xref_density: 0.0,
        instance_count: 31,
        hub_fanin: 30,
        cotype_cohort: 30,
    };
    let out = generate(&root, &knobs, 0xC0DED);
    assert!(!out.hub_referrers.is_empty() && !out.cotype_cohort.is_empty());

    let fs = RealFileSystem;
    let held = build(&out.entry, &fs).expect("baseline build");

    let bulk = |held: &KnowledgeBase, dirty: BTreeSet<PathBuf>, ctx: &str| -> KnowledgeBase {
        let rc = recompute_dirty(held, &dirty, &fs)
            .unwrap_or_else(|| panic!("{ctx}: expected an incremental recompute, got NeedsFull"));
        let inc = apply_recompute(held, rc);
        let full = build(&out.entry, &fs).expect("full rebuild");
        assert_same_projections(&inc, &full, ctx);
        assert_eq!(
            reverse_index_facts(&inc),
            reverse_index_facts(&full),
            "{ctx}: reverse indices (backlinks / closure_members) differ from a full build"
        );
        inc
    };

    // Step A — the hub: re-touch every referrer of `n0` at once (title change,
    // link preserved), so all N backlink deltas land on `n0`'s inbound list.
    for p in &out.hub_referrers {
        std::fs::write(
            p,
            "---\ntype: t0\ntitle: touched\nlink: \"[[n0]]\"\n---\n# touched\n",
        )
        .unwrap();
    }
    let hub_set: BTreeSet<PathBuf> = out.hub_referrers.iter().cloned().collect();
    let held = bulk(&held, hub_set, "bulk-hub-backlinks");

    // Step B — the cohort: retype every `c{i}` at once (add the `t1` mixin), so
    // all N closure deltas land on `(repo0, t1)`. Link-free, so this leaves the
    // backlink index untouched and isolates the closure-membership apply.
    for p in &out.cotype_cohort {
        std::fs::write(
            p,
            "---\ntype:\n  - t0\n  - t1\ntitle: Retyped\n---\n# Retyped\n",
        )
        .unwrap();
    }
    let cohort_set: BTreeSet<PathBuf> = out.cotype_cohort.iter().cloned().collect();
    let _ = bulk(&held, cohort_set, "bulk-cohort-closure-membership");
}

/// Malformed shapes (an unresolvable type claim, a dangling body wikilink)
/// exercise the identity invariants on the DIAGNOSTIC-bearing path the clean fuzz
/// above avoids. The generated tree stays clean; this test INJECTS malformed
/// instances, so it asserts identity (build-twice, incremental == full) rather
/// than cleanliness. Guards the reference-layer bug class (an unresolved claim, a
/// stranded link) that lived exactly in these shapes.
#[test]
fn malformed_shapes_recompute_equals_a_full_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    generate_profile(&root, Profile::Medium, 0xBAD);
    let fs = RealFileSystem;

    // Inject malformed instances into repo0: an unresolvable type claim and a
    // dangling body wikilink. Both produce diagnostics, so the knowledge base is
    // no longer clean, which is the point.
    std::fs::write(
        root.join("repo0/bad_type.md"),
        "---\ntype: t_missing\ntitle: Bad\n---\n# Bad\n",
    )
    .unwrap();
    std::fs::write(
        root.join("repo0/bad_link.md"),
        "---\ntype: t0\ntitle: Bad link\n---\n# Bad link\n\nsee [[does-not-exist]].\n",
    )
    .unwrap();

    // Build-twice identical WITH the malformed shapes present (their diagnostics
    // and projections must be deterministic).
    let a = build(&root, &fs).expect("build a");
    let b = build(&root, &fs).expect("build b");
    assert_same_projections(&a, &b, "malformed-build-twice");

    // Add a further malformed instance incrementally; the result must equal a full
    // rebuild of the post-add tree.
    let held = build(&root, &fs).expect("baseline build");
    let added = root.join("repo0/bad_added.md");
    std::fs::write(
        &added,
        "---\ntype: also_missing\ntitle: Added bad\n---\n# Added bad\n\nsee [[also-nope]].\n",
    )
    .unwrap();
    let set: BTreeSet<PathBuf> = std::iter::once(added).collect();
    let rc = recompute_dirty(&held, &set, &fs)
        .expect("adding a malformed instance recomputes incrementally, not NeedsFull");
    let inc = apply_recompute(&held, rc);
    let full = build(&root, &fs).expect("full rebuild");
    assert_same_projections(&inc, &full, "malformed-add");
}
