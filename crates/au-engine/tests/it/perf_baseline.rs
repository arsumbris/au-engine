//! Large-knowledge-base read-latency baseline, the instrument for the read-path work.
//!
//! Not a correctness test, an `#[ignore]`'d benchmark. It drives the real
//! daemon read path (framing, the state lock, per-connection servicing), so it
//! captures the effects the read-path fixes target, not just the read compute.
//!
//! Run it, sized by `AU_BASELINE_N` (default 2000):
//!   cargo test -p au-engine --test it perf_baseline -- --ignored --nocapture
//!
//! Re-run after each read-path action and compare. The numbers to watch:
//! - `children` and per-file `diagnostics`, the O(knowledge base) reads, should fall to
//!   O(answer) once the range-scans land.
//! - the concurrent-vs-sequential ratio, should approach 1.0 once reads stop
//!   serializing on the state lock.

#![cfg(unix)]

use std::fs;
use std::time::{Duration, Instant};

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _server: ServeHandle,
    socket: std::path::PathBuf,
    client: Client,
}

const FILES_PER_DIR: usize = 50;

/// Write `n` typed notes spread across `n / FILES_PER_DIR` subdirs, so the
/// catalog is large but any one directory is small. This is the shape the
/// `children` fix targets: listing a small dir must not scan the whole knowledge base.
///
/// Each note is a `note` instance linking a shared hub (`note-0`), its
/// neighbour, and a dangling body target. Half also carry a dangling typed
/// `rel`, an advisory dangling-reference diagnostic, so the served diagnostic
/// stream is large (~`n / 2`) and the per-file `diagnostics` scan has a whole
/// stream to wade through for one file's few.
fn gen_repo(root: &std::path::Path, n: usize) {
    crate::seed_repo(root);
    fs::create_dir_all(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  title: String\n  rel: note*\n",
    )
    .unwrap();
    for i in 0..n {
        let d = i / FILES_PER_DIR;
        fs::create_dir_all(root.join(format!("content/d{d}"))).unwrap();
        let next = (i + 1) % n;
        // Half the notes point `rel` at a missing target, a dangling-reference
        // diagnostic each; the rest resolve to the hub.
        let rel = if i % 2 == 0 {
            format!("[[gone-{i}]]")
        } else {
            "[[note-0]]".to_string()
        };
        let body = format!(
            "---\ntype: note\ntitle: n{i}\nrel: {rel}\n---\n\n# note {i}\n\n\
             see [[note-0]] and [[note-{next}]].\n\nmissing [[gone-body-{i}]].\n"
        );
        fs::write(root.join(format!("content/d{d}/note-{i}.md")), body).unwrap();
    }
    // A real knowledge base is a git repo, so `content` exercises the direct HEAD read
    // (the production path), not the non-git shortcut.
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "seed"]);
}

fn boot(root: &std::path::Path) -> Harness {
    let dir_keep = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(root, ConfigSource::Empty);
    engine.rebuild();
    let server = serve(engine.handle(), &socket).expect("serve");
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir_keep,
        _sock_dir: sock_dir,
        _server: server,
        socket,
        client,
    }
}

/// The reads a consumer fires on file open, against `content/note-1.md`.
fn open_batch(target: &str, dir: &str) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("content", json!({ "read": "content", "path": target })),
        (
            "frontmatter",
            json!({ "read": "frontmatter", "path": target }),
        ),
        (
            "semantic_tokens",
            json!({ "read": "semantic_tokens", "path": target }),
        ),
        (
            "references_out",
            json!({ "read": "references_out", "path": target }),
        ),
        (
            "backlinks",
            json!({ "read": "references_in", "path": target }),
        ),
        (
            "diagnostics(file)",
            json!({ "read": "diagnostics", "path": target }),
        ),
        ("children", json!({ "read": "dir_entries", "dir": dir })),
    ]
}

fn median(mut ds: Vec<Duration>) -> Duration {
    ds.sort();
    ds[ds.len() / 2]
}

/// Fire `reqs` from `k` clients at once, each on its own connection, and return
/// the wall-clock for all to finish. Compared against `k x` a single run, the
/// ratio shows how much the reads parallelize.
fn probe(
    socket: &std::path::Path,
    reqs: &[(&'static str, serde_json::Value)],
    k: usize,
) -> Duration {
    let socket = socket.to_path_buf();
    let payloads: Vec<serde_json::Value> = reqs.iter().map(|(_, r)| r.clone()).collect();
    let start = Instant::now();
    let handles: Vec<_> = (0..k)
        .map(|_| {
            let socket = socket.clone();
            let payloads = payloads.clone();
            std::thread::spawn(move || {
                let mut c = Client::connect(&socket).expect("connect");
                for req in &payloads {
                    let _ = c.query(req).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    start.elapsed()
}

#[test]
#[ignore = "benchmark, run explicitly with --ignored --nocapture"]
fn read_latency_baseline() {
    let n: usize = std::env::var("AU_BASELINE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let reps: usize = 9;

    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    let t0 = Instant::now();
    gen_repo(&root, n);
    let gen = t0.elapsed();

    let t1 = Instant::now();
    let mut h = boot(&root);
    let build_and_serve = t1.elapsed();

    // A note in a small leaf dir (FILES_PER_DIR entries) inside a large
    // catalog, so `children` on its dir is the O(knowledge base)-vs-O(answer) case.
    let target = "content/d0/note-1.md";
    let dir = "content/d0";
    let batch = open_batch(target, dir);

    // Workload facts, so the numbers below read against a known shape.
    let diag_total = h
        .client
        .query(&json!({ "read": "diagnostic_counts" }))
        .unwrap()["result"]["diagnostic_counts"]["total"]
        .as_u64()
        .unwrap_or(0);

    println!("\n=== read-latency baseline (N={n} files, reps={reps}) ===");
    println!("gen_repo:        {gen:?}");
    println!("build+serve:      {build_and_serve:?}");
    println!(
        "workload:         {n} files across {} dirs, {diag_total} total diagnostics",
        n.div_ceil(FILES_PER_DIR)
    );
    println!("target dir holds: {FILES_PER_DIR} entries (children answer), vs {n} catalog keys (current scan)");
    println!("\nper-read median (sequential, warm):");

    let mut batch_once = Duration::ZERO;
    let mut lock_held_once = Duration::ZERO;
    for (label, req) in &batch {
        // warm
        let _ = h.client.query(req).unwrap();
        let mut samples = Vec::with_capacity(reps);
        for _ in 0..reps {
            let s = Instant::now();
            let _ = h.client.query(req).unwrap();
            samples.push(s.elapsed());
        }
        let m = median(samples);
        batch_once += m;
        // `content` shells out to git (head_commit) and runs on the blocking
        // pool, off the state lock already. The lock-free-read win shows in the
        // reads that go through `handle.read`, so track them apart.
        if *label != "content" {
            lock_held_once += m;
        }
        println!("  {label:<20} {m:?}");
    }
    println!("  {:<20} {batch_once:?}", "batch total");
    println!("  {:<20} {lock_held_once:?}", "lock-held subtotal");

    // Concurrency: K clients fire a batch at once. Under a shared-lock serial
    // read path the wall-clock is ~K x one batch; once reads run off the lock
    // it falls toward the worker-count floor. Reported for the full batch and
    // for the lock-held reads alone, where the lock-free change actually lands
    // (the full batch is dominated by `content`'s git shell-out).
    let k = 8usize;
    let full = open_batch(target, dir);
    let lock_held: Vec<_> = full
        .iter()
        .filter(|(l, _)| *l != "content")
        .cloned()
        .collect();

    let full_wall = probe(&h.socket, &full, k);
    let held_wall = probe(&h.socket, &lock_held, k);
    let full_seq = batch_once * k as u32;
    let held_seq = lock_held_once * k as u32;

    let ratio = |wall: Duration, seq: Duration| {
        wall.as_secs_f64() / seq.as_secs_f64().max(f64::MIN_POSITIVE)
    };
    println!("\nconcurrency ({k} clients):");
    println!(
        "  full batch    seq-est {full_seq:?}  wall {full_wall:?}  ratio {:.2}",
        ratio(full_wall, full_seq)
    );
    println!(
        "  lock-held     seq-est {held_seq:?}  wall {held_wall:?}  ratio {:.2}  (1.0 = serial, ~0.5 = 2-worker parallel)",
        ratio(held_wall, held_seq)
    );
    println!();
}
