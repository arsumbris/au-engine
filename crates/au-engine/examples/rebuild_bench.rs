//! Rebuild benchmark: from-scratch build vs the incremental fast path, across
//! knowledge base sizes and topologies.
//!
//! Run: `cargo run --release --example rebuild_bench -p au-engine -- [sizes...]`
//! The `sizes` argument drives the single-repo sweep; the multi-repo sweeps use
//! a fixed repo size and vary the repo count.
//!
//! Writes real files to a temp dir and builds through `RealFileSystem`, so disk
//! read cost is measured, not mocked. Knowledge-base shapes, each one row per edit class
//! (edit, add, delete), all medians over REPS:
//! - single: one repo, the whole knowledge base.
//! - multi-indep: an assembled workspace of N independent repos, fixed repo
//!   size, varied repo count. No cross-repo references.
//! - multi-xref: the same, but one repo imports another's type and references
//!   its instances through a typed slot. The realistic cross-repo case: it
//!   creates a compose site, so add/delete re-validate the cross-boundary typed
//!   referrers, and a claim edit ripples to its cross-boundary identity-dependents.
//! - indep-dirty / xref-dirty: the multi shapes, but every non-referrer instance
//!   carries a standing `reference-target-missing`, so the served diagnostic
//!   stream scales with the knowledge base. They exercise the served-stream maintenance a
//!   clean knowledge base leaves near-empty.
//!
//! Columns:
//! - diags: the served diagnostic count of the held knowledge base, the input size of the
//!   served-stream splice; ~0 on the clean shapes, ~knowledge base on the dirty ones.
//! - scratch: a full from-scratch build, the O(knowledge base) baseline.
//! - graph/val: a whole-knowledge-base rebuild reusing the parse layer with no edit, the
//!   O(knowledge base) resolved work the fast path replaces.
//! - fast/cmp: `recompute_dirty`, the compute half, bounded to the dirty set's
//!   blast radius. An edit recomputes one instance; an add or delete also patches
//!   the changed repo's reference index by delta. In the cross-repo case an
//!   add/delete re-validates the bounded cross-boundary referrer set, and a claim
//!   edit re-validates its inbound identity-dependents across the boundary.
//! - fast/clone: the bare `KnowledgeBase` clone alone. O(1): the held maps are persistent
//!   ordered structures and the invariant structs are `Arc`, so the clone shares,
//!   it does not copy.
//! - fast/apply: `apply_recompute`, the carrier, by subtraction (fast/edit -
//!   fast/cmp). The O(1) clone plus the per-source deltas (backlinks, closure,
//!   the served-stream splice), all O(changed).
//! - fast/edit: fast/cmp + fast/apply, the live per-edit cost of the fast path.
//!
//! Every per-edit term is O(changed): fast/cmp is flat against knowledge base size (the
//! cross-repo ripple is bounded by referrer count, not knowledge base), fast/clone is ~0,
//! and fast/apply does not scale with knowledge base size or, on the dirty shapes, with
//! the served diagnostic count. The fast path is orders of magnitude below a
//! scratch build and flat where the build grows O(knowledge base).
//!
//! Caveats:
//! - it times `recompute_dirty` + `apply_recompute`, not the lock, the
//!   fingerprint clone, and the version bump in the engine's
//!   `try_incremental_fast_path`, which are trivial next to the `KnowledgeBase` clone.
//! - reads are warm (OS page cache), so the read share is a lower bound.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use au_engine::{
    apply_recompute, build, build_reusing, recompute_dirty, ContentHash, KnowledgeBase,
};
use au_parser::RealFileSystem;

const TYPE_COUNT: usize = 8;
const REPS: usize = 5;
/// The multi-repo sweeps hold each repo at this many instances and vary the repo
/// count, so the changed repo's index rebuild is a fixed O(repo) while the knowledge base
/// (and the clone) grows.
const REPO_SIZE: usize = 1000;
const REPO_COUNTS: [usize; 3] = [1, 4, 16];
/// Per edit-class, the number of cross-repo typed referrers in the importing
/// repo. Fixed, so the cross-boundary re-validation term stays constant as the
/// knowledge base grows, demonstrating it is bounded by referrer count, not knowledge base size.
const CROSS_FANIN: usize = 64;

// --- single-repo knowledge base -----------------------------------------------------

/// Write a synthetic single-repo knowledge base of `instances` instances into `dir`.
/// Each instance claims one of `TYPE_COUNT` types and links the next, a
/// navigational body link, so an edit has no inbound dependents.
fn write_repo(dir: &Path, instances: usize) {
    write_registry(dir, "name: bench\n");
    let type_dir = dir.join("type");
    std::fs::create_dir_all(&type_dir).unwrap();
    for t in 0..TYPE_COUNT {
        std::fs::write(
            type_dir.join(format!("t{t}.type.yaml")),
            b"fields:\n  title: String\n  n: Int\n",
        )
        .unwrap();
    }
    for i in 0..instances {
        let t = i % TYPE_COUNT;
        let link = (i + 1) % instances.max(1);
        std::fs::write(
            dir.join(format!("n{i}.md")),
            format!(
                "---\ntype: t{t}\ntitle: Note {i}\nn: {i}\n---\n# Note {i}\n\nbody {i}, see [[n{link}]].\n"
            ),
        )
        .unwrap();
    }
}

// --- multi-repo workspace --------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Cross {
    /// Independent repos with disjoint vocabularies: repo K defines its own
    /// `t{k}` / `o{k}` types, so each type name has a single cross-repo site.
    /// The clean baseline, no cross-repo references.
    None,
    /// A shared vocabulary imported across the boundary: every repo defines
    /// `note`, and repo1 imports repo0's `note` (`type: note::repo0`) and
    /// references repo0's instances through a typed slot. The realistic
    /// cross-repo case: repo1's importing instances recompute when repo0's
    /// `note` or the referenced targets change.
    Typed,
}

/// Write an assembled workspace of `repo_count` repos, each `repo_size`
/// instances, into `dir`. The entry is a content-free folder-repo; its
/// `.arsumbris/workspace.yaml` lists it plus each member in `edit`. Members are
/// co-present sibling subdirs, resolved by their `repo.yaml` markers, so no
/// per-machine location file is needed.
///
/// [`Cross::None`]: repo K owns disjoint `t{k}` / `o{k}` types; the measured
/// edit changes repo0/n0 from `t0` to `o0`. No cross-repo edges.
///
/// [`Cross::Typed`]: every repo defines `note` (and `other`); repo1 imports
/// repo0's `note` (`type: note::repo0`), and its first `3 * CROSS_FANIN`
/// instances fill the typed `link` slot with a cross-repo reference to repo0's
/// n0 (the edit target), n1 (the delete target), and `added` (the add target).
/// Those references resolve, so they form backlinks and conformance checks:
/// editing n0's claim re-validates the n0 group through inbound
/// identity-dependents, and the add and delete re-validate their groups through
/// the edge-flip set.
fn write_workspace(dir: &Path, repo_count: usize, repo_size: usize, cross: Cross, dirty: bool) {
    write_registry(dir, "name: bench\n");
    let mut edit = String::from("edit:\n  - bench\n");
    for k in 0..repo_count {
        edit.push_str(&format!("  - repo{k}\n"));
    }
    std::fs::write(dir.join(".arsumbris").join("workspace.yaml"), edit).unwrap();

    for k in 0..repo_count {
        let rdir = dir.join(format!("repo{k}"));
        let imports = cross == Cross::Typed && k == 1;
        if imports {
            // repo1 depends on repo0, a co-present sibling resolved by its marker.
            write_registry(&rdir, "name: repo1\ndeps:\n  - name: repo0\n");
        } else {
            write_registry(&rdir, &format!("name: repo{k}\n"));
        }

        let type_dir = rdir.join("type");
        std::fs::create_dir_all(&type_dir).unwrap();
        // The claimed type and the alternate the edit switches n0 to. Disjoint
        // per repo in the independent case, the shared `note` / `other` in the
        // cross-repo case.
        let (note, other) = match cross {
            Cross::None => (format!("t{k}"), format!("o{k}")),
            Cross::Typed => ("note".to_string(), "other".to_string()),
        };
        std::fs::write(
            type_dir.join(format!("{note}.type.yaml")),
            format!("fields:\n  title: String\n  link?: {note}*\n"),
        )
        .unwrap();
        std::fs::write(
            type_dir.join(format!("{other}.type.yaml")),
            "fields:\n  title: String\n",
        )
        .unwrap();

        for i in 0..repo_size {
            let body = if imports && i < 3 * CROSS_FANIN {
                // Three referrer groups, one per measured class, each importing
                // repo0's `note` type (`type: note::repo0`) and filling the typed
                // `link` slot with a cross-repo reference to the class's target in
                // repo0.
                let target = match i % 3 {
                    0 => "n0",    // the edit target: claim change flips conformance
                    1 => "n1",    // the delete target: vanishes
                    _ => "added", // the add target: appears
                };
                format!("---\ntype: note::repo0\ntitle: Ref {i}\nlink: \"[[{target}::repo0]]\"\n---\n# Ref {i}\n")
            } else if dirty {
                // A standing diagnostic per instance: the typed `link` slot
                // holds a reference to a target that does not exist, so each
                // instance carries a `reference-target-missing`. The served
                // diagnostic stream then scales with the knowledge base, exposing the
                // whole-stream re-derivation in apply that a clean knowledge base hides.
                let link = (i + 1) % repo_size.max(1);
                format!(
                    "---\ntype: {note}\ntitle: Note {i}\nlink: \"[[ghost{i}]]\"\n---\n# Note {i}\n\nbody {i}, see [[n{link}]].\n"
                )
            } else {
                let link = (i + 1) % repo_size.max(1);
                format!(
                    "---\ntype: {note}\ntitle: Note {i}\n---\n# Note {i}\n\nbody {i}, see [[n{link}]].\n"
                )
            };
            std::fs::write(rdir.join(format!("n{i}.md")), body).unwrap();
        }
    }
}

fn write_registry(repo: &Path, body: &str) {
    let ars = repo.join(".arsumbris");
    std::fs::create_dir_all(&ars).unwrap();
    std::fs::write(ars.join("repo.yaml"), body).unwrap();
}

// --- timing ----------------------------------------------------------------

fn known_hashes(kb: &KnowledgeBase) -> BTreeMap<PathBuf, Option<ContentHash>> {
    kb.catalog
        .iter()
        .map(|(p, e)| (p.clone(), e.hash))
        .collect()
}

fn median(mut ds: Vec<Duration>) -> Duration {
    ds.sort();
    ds[ds.len() / 2]
}

/// Median wall-clock of `f` over `REPS` runs, after one warm-up.
fn time<R>(mut f: impl FnMut() -> R) -> Duration {
    std::hint::black_box(f());
    let mut ds = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        let r = f();
        ds.push(t.elapsed());
        std::hint::black_box(r);
    }
    median(ds)
}

fn ms(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}

fn one(path: PathBuf) -> BTreeSet<PathBuf> {
    std::iter::once(path).collect()
}

/// Median compute and compute+apply times for one already-staged dirty set: the
/// dirty paths are written or removed on disk, `base` is the held pre-change
/// knowledge base. Returns `(fast/cmp, fast/edit)`; apply is the difference.
fn measure(
    base: &KnowledgeBase,
    dirty: &BTreeSet<PathBuf>,
    fs: &RealFileSystem,
) -> (Duration, Duration) {
    let cmp = time(|| recompute_dirty(base, dirty, fs).expect("the dirty set takes the fast path"));
    let edit = time(|| {
        let rc = recompute_dirty(base, dirty, fs).expect("the dirty set takes the fast path");
        apply_recompute(base, rc)
    });
    (cmp, edit)
}

/// Build the held knowledge base, then stage and time the three edit classes against it.
/// `target` is the repo subdirectory the edit, add, and delete touch (the whole
/// knowledge base for single-repo, repo0 for the workspaces). `edit_body` rewrites n0 and
/// `add_body` writes a fresh `added.md`; the delete removes n1.
fn bench_one(
    variant: &str,
    total: usize,
    dir: &Path,
    target: &Path,
    edit_body: &[u8],
    add_body: &[u8],
    fs: &RealFileSystem,
) {
    let scratch = time(|| build(dir, fs).unwrap());
    let base = build(dir, fs).unwrap();
    // The served diagnostic count: the input size of the whole-stream
    // re-derivation apply runs, so a dirty knowledge base shows it grow with the knowledge base.
    let diags = base.diagnostics_len();
    let prior = base.parse_layer();
    let known = known_hashes(&base);
    let graph_val = time(|| {
        build_reusing(
            dir,
            fs,
            &au_engine::UserRegistry::new(),
            &prior,
            &known,
            None,
        )
        .unwrap()
    });
    // The bare `KnowledgeBase` clone, the part of apply the Arc-share target removes.
    // `apply` minus this is the post-clone re-derivation (the per-source deltas
    // and the served stream), which Arc-share does not remove.
    let clone = time(|| base.clone());

    // Each class is a disjoint change against the pristine `base`: edit n0, add a
    // fresh file, delete n1. They touch disjoint paths, and the recompute reads
    // only its dirty set and otherwise the held `base`, so staging one never
    // disturbs another.
    std::fs::write(target.join("n0.md"), edit_body).unwrap();
    let (e_cmp, e_edit) = measure(&base, &one(target.join("n0.md")), fs);

    std::fs::write(target.join("added.md"), add_body).unwrap();
    let (a_cmp, a_edit) = measure(&base, &one(target.join("added.md")), fs);

    std::fs::remove_file(target.join("n1.md")).unwrap();
    let (d_cmp, d_edit) = measure(&base, &one(target.join("n1.md")), fs);

    for (class, cmp, edit) in [
        ("edit", e_cmp, e_edit),
        ("add", a_cmp, a_edit),
        ("delete", d_cmp, d_edit),
    ] {
        println!(
            "{:>11}  {:>6}  {:<7}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}  {:>9}",
            variant,
            total,
            class,
            diags,
            ms(scratch),
            ms(graph_val),
            ms(cmp),
            ms(clone),
            ms(edit.saturating_sub(cmp)),
            ms(edit),
        );
    }
}

fn main() {
    let sizes: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let sizes = if sizes.is_empty() {
        vec![200, 1000, 4000]
    } else {
        sizes
    };

    let fs = RealFileSystem;
    println!(
        "{:>11}  {:>6}  {:<7}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}  {:>9}",
        "variant",
        "files",
        "class",
        "diags",
        "scratch",
        "graph/val",
        "fast/cmp",
        "fast/clone",
        "fast/apply",
        "fast/edit"
    );

    // single-repo: edit changes n0's claim t0 -> t1 (no typed dependents).
    let single_edit =
        b"---\ntype: t1\ntitle: Note 0 edited\nn: 0\n---\n# Note 0 edited\n\nsee [[n1]].\n";
    let single_add = b"---\ntype: t0\ntitle: Added\nn: 0\n---\n# Added\n";
    for &n in &sizes {
        let tmp = tempfile::tempdir().unwrap();
        // The entry folder is named `bench` to match its declared repo name, so
        // no `repo-folder-name-mismatch` drift pollutes the measured diag stream.
        let dir = tmp.path().join("bench");
        write_repo(&dir, n);
        bench_one("single", n, &dir, &dir, single_edit, single_add, &fs);
    }

    // multi-repo: the edit changes repo0/n0's claim to the repo's alternate type
    // (t0 -> o0 independent, note -> other cross-repo). In multi-xref that flips
    // the conformance of the cross-repo referrers pointing at n0.
    // The `dirty` variants give every non-referrer instance a standing
    // `reference-target-missing`, so the served diagnostic stream scales with the
    // knowledge base. They isolate the whole-stream re-derivation apply runs, which a clean
    // knowledge base hides (its served stream is near-empty). Same topologies otherwise.
    let variants: [(&str, Cross, bool, &[u8], &[u8]); 4] = [
        (
            "multi-indep",
            Cross::None,
            false,
            b"---\ntype: o0\ntitle: Note 0 edited\n---\n# Note 0 edited\n",
            b"---\ntype: t0\ntitle: Added\n---\n# Added\n",
        ),
        (
            "multi-xref",
            Cross::Typed,
            false,
            b"---\ntype: other\ntitle: Note 0 edited\n---\n# Note 0 edited\n",
            b"---\ntype: note\ntitle: Added\n---\n# Added\n",
        ),
        (
            "indep-dirty",
            Cross::None,
            true,
            b"---\ntype: o0\ntitle: Note 0 edited\n---\n# Note 0 edited\n",
            b"---\ntype: t0\ntitle: Added\n---\n# Added\n",
        ),
        (
            "xref-dirty",
            Cross::Typed,
            true,
            b"---\ntype: other\ntitle: Note 0 edited\n---\n# Note 0 edited\n",
            b"---\ntype: note\ntitle: Added\n---\n# Added\n",
        ),
    ];
    for (variant, cross, dirty, edit_body, add_body) in variants {
        for &repos in &REPO_COUNTS {
            let total = repos * REPO_SIZE;
            let tmp = tempfile::tempdir().unwrap();
            // Entry folder named `bench` to match its declared name (no drift).
            let dir = tmp.path().join("bench");
            write_workspace(&dir, repos, REPO_SIZE, cross, dirty);
            let target = dir.join("repo0");
            bench_one(variant, total, &dir, &target, edit_body, add_body, &fs);
        }
    }
}
