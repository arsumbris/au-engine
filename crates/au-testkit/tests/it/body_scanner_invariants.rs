//! Invariants asserted against `au_parser::scan_body` over arbitrary bytes.
//!
//! Guards three properties of the body scanner:
//! - never panics on any UTF-8 input
//! - events emit in source order (sorted by span.start)
//! - event spans never overlap
//!
//! Plus a deterministic property: scanning twice yields the same events.

use au_parser::{scan_body, BodyEvent};
use proptest::prelude::*;

fn event_span(e: &BodyEvent<'_>) -> (usize, usize) {
    let s = e.span();
    (s.start, s.end)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn scan_body_never_panics_on_arbitrary_utf8(src in any::<String>()) {
        let _ = scan_body(&src);
    }

    #[test]
    fn events_emit_in_source_order(src in any::<String>()) {
        let events = scan_body(&src);
        let mut prev_start: Option<usize> = None;
        for e in &events {
            let (start, _) = event_span(e);
            if let Some(p) = prev_start {
                prop_assert!(start >= p, "event starts out of order: {} < {}", start, p);
            }
            prev_start = Some(start);
        }
    }

    #[test]
    fn event_spans_do_not_overlap(src in any::<String>()) {
        let events = scan_body(&src);
        let mut prev_end: Option<usize> = None;
        for e in &events {
            let (start, end) = event_span(e);
            if let Some(p) = prev_end {
                prop_assert!(
                    start >= p,
                    "event span ({}, {}) overlaps prior end {}",
                    start,
                    end,
                    p
                );
            }
            prop_assert!(end <= src.len(), "event end {} exceeds src len {}", end, src.len());
            prev_end = Some(end);
        }
    }

    #[test]
    fn scan_is_deterministic(src in any::<String>()) {
        let first = scan_body(&src);
        let second = scan_body(&src);
        prop_assert_eq!(first, second);
    }
}

// ----- targeted regression assertions, picked up by the file -----

#[test]
fn realistic_body_yields_well_ordered_events() {
    let src = "# Why\n\nSee [[paper-a:sources]] for context.\n\n## Detail\n\n`[:field] value`\n\n```yaml [:assumptions]\ntype: assumption\ndescription: x\n```\n^id-x\n";
    let events = scan_body(src);
    assert!(!events.is_empty());
    let mut prev_end = 0usize;
    for e in &events {
        let (start, end) = event_span(e);
        assert!(start >= prev_end, "out of order at {:?}", e);
        prev_end = end;
    }
}
