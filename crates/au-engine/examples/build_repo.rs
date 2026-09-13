//! Time a full repo build. Used to check cold-load scaling on large
//! instances end-to-end (parse + validate + reference resolution + indexing).
//!
//! Run: `cargo run --release --example build_repo -p au-engine -- <repo-dir>`

use std::time::Instant;

use au_engine::build;
use au_parser::RealFileSystem;

fn main() {
    let repo = std::env::args()
        .nth(1)
        .expect("usage: build_repo <repo-dir>");
    let path = std::path::Path::new(&repo);

    let start = Instant::now();
    let result = build(path, &RealFileSystem);
    let elapsed = start.elapsed();

    match result {
        Ok(v) => println!(
            "built {repo} in {:.2}ms  diagnostics={}",
            elapsed.as_secs_f64() * 1000.0,
            v.diagnostics_len()
        ),
        Err(e) => println!(
            "build failed after {:.2}ms: {e}",
            elapsed.as_secs_f64() * 1000.0
        ),
    }
}
