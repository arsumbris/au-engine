//! Invariants for body-typing graph-load. Asserts that parsing and the
//! load-time `body:` checks produce deterministic output across runs and
//! input orderings.

use au_core::{
    build_graph, parse_type_def, run_body_typing_checks, run_graph_structure_checks,
    run_inheritance_checks,
};
use au_parser::yaml::parse;

fn parse_one(path: &str, src: &str) -> Option<au_core::TypeDef> {
    let docs = parse(src).expect("parse yaml");
    let doc = docs.first().expect("at least one doc");
    parse_type_def(std::path::Path::new(path), src, 0, doc).type_def
}

fn run_all_checks_diag_codes(srcs: &[(&str, &str)]) -> Vec<String> {
    let mut tds = Vec::new();
    for (path, src) in srcs {
        if let Some(td) = parse_one(path, src) {
            tds.push(td);
        }
    }
    let build = build_graph(tds);
    let mut diags = Vec::new();
    diags.extend(build.diagnostics);
    diags.extend(run_graph_structure_checks(&build.graph));
    diags.extend(run_inheritance_checks(&build.graph));
    diags.extend(run_body_typing_checks(&build.graph));
    diags.iter().map(|d| d.code.as_str().to_string()).collect()
}

#[test]
fn body_typing_checks_are_deterministic() {
    let sources = &[
        (
            "/v/note.type.yaml",
            "fields:\n  description: String\nbody:\n  - section: Body\n",
        ),
        (
            "/v/decision.type.yaml",
            "type: note\nfields:\n  rationale?: String\nbody:\n  - use: note\n  - section: Why\n    fills: rationale\n",
        ),
    ];
    let first = run_all_checks_diag_codes(sources);
    let second = run_all_checks_diag_codes(sources);
    assert_eq!(first, second);
}

#[test]
fn input_order_does_not_change_diag_set() {
    let a = (
        "/v/note.type.yaml",
        "fields:\n  description: String\nbody:\n  - section: Body\n",
    );
    let b = (
        "/v/decision.type.yaml",
        "type: note\nfields:\n  rationale?: String\nbody:\n  - use: note\n  - section: Why\n    fills: rationale\n",
    );
    let mut order_ab = run_all_checks_diag_codes(&[a, b]);
    let mut order_ba = run_all_checks_diag_codes(&[b, a]);
    order_ab.sort();
    order_ba.sort();
    assert_eq!(order_ab, order_ba);
}

#[test]
fn use_cycle_load_check_fires_consistently() {
    let src = "fields:\n  description: String\nbody:\n  - use: loop\n";
    let sources = &[("/v/loop.type.yaml", src)];
    let codes = run_all_checks_diag_codes(sources);
    assert!(
        codes.iter().any(|c| c == "body-use-cycle"),
        "expected body-use-cycle in {:?}",
        codes
    );
}

#[test]
fn use_out_of_closure_load_check_fires_consistently() {
    let sources = &[
        (
            "/v/note.type.yaml",
            "fields:\n  description: String\nbody:\n  - section: Body\n",
        ),
        (
            "/v/orphan.type.yaml",
            "fields:\n  title: String\nbody:\n  - use: note\n",
        ),
    ];
    let codes = run_all_checks_diag_codes(sources);
    assert!(
        codes.iter().any(|c| c == "body-use-out-of-closure"),
        "expected body-use-out-of-closure in {:?}",
        codes
    );
}

#[test]
fn fills_unknown_field_load_check_fires() {
    let src = "fields:\n  description: String\nbody:\n  - section: Bogus\n    fills: nonexistent\n";
    let sources = &[("/v/host.type.yaml", src)];
    let codes = run_all_checks_diag_codes(sources);
    assert!(
        codes.iter().any(|c| c == "fills-unknown-field"),
        "expected fills-unknown-field in {:?}",
        codes
    );
}

#[test]
fn body_template_round_trips_through_debug() {
    let src = "fields:\n  description: String\nbody:\n  - section: Why\n  - section?: Notes\n";
    let td = parse_one("/v/host.type.yaml", src).expect("parse host");
    let body = td.body.expect("body present");
    let dbg_first = format!("{body:?}");
    // Parse the same source again — the Debug representation of the parsed
    // body should be identical.
    let td2 = parse_one("/v/host.type.yaml", src).expect("parse host again");
    let body2 = td2.body.expect("body present");
    let dbg_second = format!("{body2:?}");
    assert_eq!(dbg_first, dbg_second);
}
