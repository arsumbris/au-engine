//! Timing harness for instance parsing over a large record sequence.
//!
//! Generates a synthetic instance whose body is a sequence of N event
//! mappings, then times `parse` + `parse_instance` across growing N. The
//! `ms/record` column exposes superlinear scaling: flat means linear, rising
//! means worse.
//!
//! Run: `cargo run --release --example span_scaling -p au-core`

use std::path::Path;
use std::time::Instant;

use au_core::instance::parse_instance;
use au_parser::yaml::parse;

/// Build an instance document with `n` event records. A multibyte char rides
/// in each record so the input is non-ASCII, matching real session-log traces.
fn synth(n: usize) -> String {
    let mut s = String::from("type: session-log\nevents:\n");
    for i in 0..n {
        s.push_str(&format!(
            "  - kind: toolUse\n    seq: {i}\n    tool: edit_file\n    \
             text: \"line {i} with a unicode é and an ellipsis … to force multibyte\"\n    \
             at: {ts}\n",
            ts = 1_700_000_000 + i
        ));
    }
    s
}

fn time_one(n: usize) {
    let source = synth(n);
    let bytes = source.len();
    let docs = parse(&source).expect("parse");

    let start = Instant::now();
    let result = parse_instance(Path::new("session.yaml"), &source, 0, &docs[0]);
    let elapsed = start.elapsed();

    // Touch the result so the work can't be optimized away.
    let field_count = result.instance.map(|i| i.fields.len()).unwrap_or(0);
    let ms = elapsed.as_secs_f64() * 1000.0;
    let us_per_record = elapsed.as_micros() as f64 / n as f64;
    println!(
        "n={n:>6}  bytes={bytes:>9}  fields={field_count:>3}  total={ms:>10.2}ms  per-record={us_per_record:>8.2}us"
    );
}

fn main() {
    for n in [500usize, 1_000, 2_000, 4_000, 8_000, 16_000] {
        time_one(n);
    }
}
