//! Scale timing harness over the shared `repogen` generator: whole-build and
//! incremental fast-path wall-clock across the named profiles and a repo-count
//! sweep. A smoke test for ~linear (O(V+E)) whole-build scaling, eyeballed.
//!
//! `cargo run --release --example scale_bench -p au-engine`
//!
//! Complements `rebuild_bench`, which measures the edit-class and dirty-served
//! stream over a hand-authored topology; this one drives the profile knobs of the
//! shared generator, so the two share no generation code once `repogen` gains a
//! dirty-ratio knob (deferred) and rebuild_bench can fold in.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use au_engine::wire::{
    introspect_instances_of, introspect_kb_instances, introspect_type_closure,
    introspect_type_tree, TypeScope,
};
use au_engine::{apply_recompute, build, recompute_dirty};
use au_parser::RealFileSystem;
use au_testkit::repogen::{generate, Knobs, Profile};

/// Timed repetitions per measurement (after one warm-up).
const REPS: usize = 5;

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

/// Whole-build + one-edit fast-path timing for a generated knowledge base. `label` names
/// the row; the printed `insts` is V, the read for the linearity smoke test.
fn bench(label: &str, knobs: &Knobs, fs: &RealFileSystem) {
    let tmp = tempfile::tempdir().unwrap();
    // The entry folder is named `bench` to match its declared repo name, so no
    // `repo-folder-name-mismatch` drift pollutes the run.
    let entry = tmp.path().join("bench");
    let out = generate(&entry, knobs, 0x5CA1E);
    let total = out.instances.len();

    let scratch = time(|| build(&out.entry, fs).unwrap());
    let base = build(&out.entry, fs).unwrap();

    // Fast path: edit repo0/n0's title (a clean instance edit that exists in every
    // profile, incl. deep-chain's single type), then time the incremental
    // recompute + apply. In the imported profiles n0 has cross-repo referrers, so
    // the recompute also scans those dependents.
    let n0 = entry.join("repo0/n0.md");
    std::fs::write(&n0, "---\ntype: t0\ntitle: edited\n---\n# edited\n").unwrap();
    let dirty: BTreeSet<PathBuf> = std::iter::once(n0).collect();
    let fast = time(|| {
        let rc = recompute_dirty(&base, &dirty, fs).expect("an instance edit is incremental");
        apply_recompute(&base, rc)
    });

    let per_inst = scratch.as_secs_f64() * 1e6 / total.max(1) as f64;
    println!(
        "{label:>12}  {total:>6}  {:>4}  {:>10}  {:>10}  {per_inst:>7.1}us",
        knobs.closure_depth,
        ms(scratch),
        ms(fast),
    );
}

/// Bulk co-dirtied hub measurement: `fanin` instances all reference one hub node
/// (`repo0/n0`), then all are dirtied at once. Prints the incremental recompute
/// time and the per-referrer cost, which GROWS with `fanin` if the per-hub
/// edge-set rebuild is O(N^2) (todo 2607051517).
fn bench_hub(fanin: usize, fs: &RealFileSystem) {
    let tmp = tempfile::tempdir().unwrap();
    let entry = tmp.path().join("bench");
    let knobs = Knobs {
        repo_count: 1,
        types_per_repo: 1,
        closure_depth: 1,
        xref_density: 0.0,
        instance_count: fanin + 1,
        hub_fanin: fanin,
        cotype_cohort: 0,
    };
    let out = generate(&entry, &knobs, 0x5CA1E);
    let base = build(&out.entry, fs).unwrap();

    // Rewrite every hub referrer once (a bulk edit), still referencing n0, then
    // time only the recompute + apply of the whole co-dirtied set.
    for p in &out.hub_referrers {
        std::fs::write(
            p,
            "---\ntype: t0\ntitle: touched\nlink: \"[[n0]]\"\n---\n# touched\n",
        )
        .unwrap();
    }
    let dirty: BTreeSet<PathBuf> = out.hub_referrers.iter().cloned().collect();
    let bulk = time(|| {
        let rc = recompute_dirty(&base, &dirty, fs).expect("a bulk instance edit is incremental");
        apply_recompute(&base, rc)
    });

    let per = bulk.as_secs_f64() * 1e6 / fanin.max(1) as f64;
    println!(
        "{fanin:>12}  {:>6}  {:>13}  {per:>9.1}us",
        out.instances.len(),
        ms(bulk),
    );
}

/// Bulk co-typed retype measurement: `cohort` link-free instances all claim `t0`
/// (sharing the closure key `(repo0, t0)`), then all are retyped at once (a `t1`
/// mixin) so every closure delta lands on `(repo0, t1)`. Link-free, so this
/// isolates the closure-membership apply from the backlink apply. Prints the
/// per-instance cost, which GROWS with `cohort` if the per-key membership rebuild
/// is O(N^2) (the sibling of the `bench_hub` quadratic, todo 2607051517).
fn bench_retype(cohort: usize, fs: &RealFileSystem) {
    let tmp = tempfile::tempdir().unwrap();
    let entry = tmp.path().join("bench");
    let knobs = Knobs {
        repo_count: 1,
        types_per_repo: 2,
        closure_depth: 1,
        xref_density: 0.0,
        instance_count: 1,
        hub_fanin: 0,
        cotype_cohort: cohort,
    };
    let out = generate(&entry, &knobs, 0x5CA1E);
    let base = build(&out.entry, fs).unwrap();

    // Retype every cohort member once (add the `t1` mixin), then time only the
    // recompute + apply of the whole co-dirtied set.
    for p in &out.cotype_cohort {
        std::fs::write(
            p,
            "---\ntype:\n  - t0\n  - t1\ntitle: Retyped\n---\n# Retyped\n",
        )
        .unwrap();
    }
    let dirty: BTreeSet<PathBuf> = out.cotype_cohort.iter().cloned().collect();
    let bulk = time(|| {
        let rc = recompute_dirty(&base, &dirty, fs).expect("a bulk instance edit is incremental");
        apply_recompute(&base, rc)
    });

    let per = bulk.as_secs_f64() * 1e6 / cohort.max(1) as f64;
    println!(
        "{cohort:>12}  {:>6}  {:>13}  {per:>9.1}us",
        out.instances.len(),
        ms(bulk),
    );
}

/// Read-path timing over a single Medium build. The whole-build and incremental
/// columns say nothing about per-query read cost, so this times the hot reads in
/// isolation, giving a before/after for read-side changes (the diagnostics
/// gather, `instances_of`, `type_tree`, `type_closure`, the hub scan). Same
/// median-per-call eyeball style, no criterion.
fn bench_reads(fs: &RealFileSystem) {
    let tmp = tempfile::tempdir().unwrap();
    let entry = tmp.path().join("bench");
    let out = generate(&entry, &Profile::Medium.knobs(), 0x5CA1E);
    let kb = build(&out.entry, fs).unwrap();
    let target = entry.join("repo0/n0.md");

    // The two diagnostics-gather forms, timed side by side over the same target.
    // The current form filters the whole served stream; the range form seeks the
    // file's contiguous key range. Both collect the same Vec, so this is a direct
    // before/after for the `resolved_view` gather.
    let diag_filter = time(|| {
        kb.diagnostics()
            .filter(|d| d.span.file == target)
            .cloned()
            .collect::<Vec<_>>()
    });
    let diag_range = time(|| {
        kb.diagnostics_for_file(&target)
            .cloned()
            .collect::<Vec<_>>()
    });

    // The public wire read builders over a broad base (`t0`, which every instance
    // claims), and the whole type tree.
    let tree = time(|| introspect_type_tree(&kb, TypeScope::all()));
    let insts = time(|| introspect_instances_of(&kb, "t0", None));
    let closure = time(|| introspect_type_closure(&kb, "t0", None));
    // The whole-corpus `instances` read: projects EVERY instance, including the
    // per-instance `closure` field, which re-walks `folded_closure_ids` per
    // instance (the owner-relative render). This is where the closure re-walk
    // cost lands; timing the whole read bounds that cost as a fraction of it.
    let all_insts = time(|| introspect_kb_instances(&kb));
    // Isolate JUST the added closure re-walk: the per-instance `folded_closure_ids`
    // the owner-relative projection runs, summed over every instance. This is the
    // exact work the `instances`-read closure field added over reading the
    // precomputed name-set; compare it against `instances(all,+closure)` above to
    // see what fraction of the read it is (the O(1)-vs-rewalk deferral question).
    let closure_rewalk = time(|| {
        let mut acc = 0usize;
        for (path, _r) in kb.instances.iter() {
            let Some(au_engine::FileParse::Instance {
                instance: Some(inst),
                ..
            }) = kb.file_parse(path)
            else {
                continue;
            };
            if let Some(rg) = kb
                .repos
                .repo_of(path)
                .and_then(|r| kb.resolution_graphs.of(&r.name))
            {
                acc += au_core::folded_closure_ids(rg, &inst.type_claim).len();
            }
        }
        acc
    });

    // `hub_ranking`'s core cost is scanning + sorting the whole backlink map per
    // call. The fn itself is private, so time that scan+sort directly over the
    // public `backlinks` field; a precompute-per-rebuild change removes exactly
    // this per-call work. A faithful proxy, not the fn itself.
    let hub_scan = time(|| {
        let mut hubs: Vec<(&PathBuf, usize)> =
            kb.backlinks.iter().map(|(p, bs)| (p, bs.len())).collect();
        hubs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        hubs
    });

    // Sizes computed outside the timed section.
    let diag_n = kb.diagnostics_for_file(&target).count();
    let tree_n = introspect_type_tree(&kb, TypeScope::all()).nodes.len();
    let inst_n = introspect_instances_of(&kb, "t0", None).len();
    let clos_n = introspect_type_closure(&kb, "t0", None).len();
    let all_inst_n = introspect_kb_instances(&kb).len();
    let bl_n = kb.backlinks.iter().count();

    println!();
    println!("{:>24}  {:>6}  {:>10}", "read", "size", "median");
    let row = |label: &str, size: usize, d: Duration| {
        println!("{label:>24}  {size:>6}  {:>10}", ms(d));
    };
    row("diagnostics(filter,cur)", diag_n, diag_filter);
    row("diagnostics(range,new)", diag_n, diag_range);
    row("type_tree", tree_n, tree);
    row("instances_of(t0)", inst_n, insts);
    row("type_closure(t0)", clos_n, closure);
    row("instances(all,+closure)", all_inst_n, all_insts);
    row("closure_rewalk(added)", all_inst_n, closure_rewalk);
    row("hub_scan(proxy)", bl_n, hub_scan);
}

fn main() {
    let fs = RealFileSystem;
    println!(
        "{:>12}  {:>6}  {:>4}  {:>10}  {:>10}  {:>9}",
        "profile/n", "insts", "dep", "whole", "fast/edit", "per-inst"
    );

    // Named profiles.
    for (label, p) in [
        ("small", Profile::Small),
        ("medium", Profile::Medium),
        ("large", Profile::Large),
        ("deep-chain", Profile::DeepChain),
        ("workspace", Profile::Workspace),
    ] {
        bench(label, &p.knobs(), &fs);
    }
    // The degenerate extreme (~150 repos, ~30k instances) is opt-in: it takes a
    // while and writes ~130 MiB. `SCALE_EXTREME=1 cargo run --release ...`.
    if std::env::var_os("SCALE_EXTREME").is_some() {
        bench("wksp-extreme", &Profile::WorkspaceExtreme.knobs(), &fs);
    }

    // Repo-count sweep at fixed per-repo size: total V scales with repo_count, so
    // whole-build should track ~linearly (the O(V+E) closure-id smoke test).
    let base = Profile::Medium.knobs();
    for &repos in &[1usize, 2, 4, 8, 16] {
        let knobs = Knobs {
            repo_count: repos,
            ..base
        };
        bench(&format!("sweep-{repos}"), &knobs, &fs);
    }

    // Hub sweep: `fanin` instances all reference one hub node, dirtied all at once.
    // A per-referrer cost that GROWS with fanin is the O(N^2)-per-hub recompute
    // (todo 2607051517); a flat one is linear.
    println!();
    println!(
        "{:>12}  {:>6}  {:>13}  {:>11}",
        "hub-fanin", "insts", "bulk-recompute", "us/referrer"
    );
    for &fanin in &[250usize, 500, 1000, 2000] {
        bench_hub(fanin, &fs);
    }

    // Retype sweep: `cohort` link-free instances sharing one closure key, retyped
    // all at once. A per-instance cost that GROWS with cohort is the O(N^2)-per-key
    // closure-membership rebuild (the `bench_hub` sibling, todo 2607051517); a flat
    // one is linear.
    println!();
    println!(
        "{:>12}  {:>6}  {:>13}  {:>11}",
        "cohort", "insts", "bulk-recompute", "us/instance"
    );
    for &cohort in &[250usize, 500, 1000, 2000] {
        bench_retype(cohort, &fs);
    }

    // Read-path timing: per-query hot reads over a single Medium build, the
    // before/after surface the build/incremental columns cannot give.
    bench_reads(&fs);
}
