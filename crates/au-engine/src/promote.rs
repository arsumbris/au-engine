//! The content move for `promote`: lift an inline `^:id` record out of its host
//! body into a standalone file, and leave a `[[newFile]]` reference where it sat.
//! See [[spec - reference-rewriting refactors - promote inline rename keep the
//! mounted-set graph consistent]].
//!
//! Span-located, source-rewritten — the parse locates the record's byte span,
//! the source on disk is the truth for its text. The record is YAML nested under
//! a slot, so extraction re-indents it to a top-level document and drops its
//! `^:` id line (the standalone file is addressable by name). The host keeps a
//! quoted `"[[newFile]]"` in the record's place.

use au_diagnostics::ByteRange;

use crate::mutate::MutationReject;

/// Strip up to `indent` leading spaces from `line`. A line with fewer (a blank
/// line, or shallower content) is stripped to its first non-space; never past it.
fn strip_indent(line: &str, indent: usize) -> &str {
    let bytes = line.as_bytes();
    let mut n = 0;
    while n < indent && bytes.get(n) == Some(&b' ') {
        n += 1;
    }
    &line[n..]
}

/// The standalone-file content for the inline record at `span` in `host`.
///
/// `span` covers the record's value (its first key through its last value), as
/// the parse reports it — it starts mid-line, at the first key, so the first
/// line carries no leading indent and the rest carry the record's indent. Each
/// following line is de-indented to column zero, yielding a top-level YAML
/// document. The record's own `^:` id line (column zero after de-indent) is
/// dropped; a nested record's `^:` stays indented, so it survives.
///
/// `inner_edits` are host-absolute (span, replacement) pairs that fall inside the
/// record — the record's references to its own block-id, redirected to the new
/// file so they do not dangle once the record moves. They are applied to the
/// slice first, in slice-relative coordinates, before de-indenting; the
/// replacements touch only `[[...]]` text, so they compose with the de-indent.
pub(crate) fn extract_record_file(
    host: &str,
    span: ByteRange,
    inner_edits: &[(ByteRange, String)],
) -> Result<String, MutationReject> {
    let raw = host.get(span.start..span.end).ok_or_else(|| {
        MutationReject::new("a record span is out of range — the file drifted from the index")
    })?;
    // Redirect the record's self-references before de-indenting. Span coordinates
    // are host-absolute; rebase them onto the slice.
    let rebased: Vec<(ByteRange, String)> = inner_edits
        .iter()
        .map(|(s, text)| {
            (
                ByteRange::new(s.start - span.start, s.end - span.start),
                text.clone(),
            )
        })
        .collect();
    let slice = crate::rename::apply_edits(raw, rebased)?;
    let line_start = host[..span.start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let indent = span.start - line_start;

    let mut out = String::with_capacity(slice.len());
    for (i, line) in slice.split('\n').enumerate() {
        let dedented = if i == 0 {
            line
        } else {
            strip_indent(line, indent)
        };
        // The record's own block-id, at column zero — dropped, the file is
        // addressable by name. A nested record's `^:` keeps its indent.
        if dedented.starts_with("^:") {
            continue;
        }
        out.push_str(dedented);
        out.push('\n');
    }
    // One trailing newline, regardless of where the span ended.
    Ok(format!("{}\n", out.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_of(content: &str, first_key: &str) -> ByteRange {
        let start = content.find(first_key).expect("first key present");
        // The record runs to end-of-content in these fixtures.
        ByteRange::new(start, content.trim_end().len())
    }

    #[test]
    fn extracts_a_sequence_element_record_dropping_its_id() {
        let host = "type: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n";
        let span = span_of(host, "^: rec");
        let file = extract_record_file(host, span, &[]).unwrap();
        assert_eq!(file, "type: node\ncontent: x\n");
    }

    #[test]
    fn extracts_a_record_whose_type_precedes_its_id() {
        let host = "type: canvas\nnodes:\n  - type: node\n    ^: rec\n    content: x\n";
        let span = span_of(host, "type: node");
        let file = extract_record_file(host, span, &[]).unwrap();
        assert_eq!(file, "type: node\ncontent: x\n");
    }

    #[test]
    fn preserves_nested_records_and_their_ids() {
        // The promoted record's own `^:` is dropped; a nested record's `^:`
        // keeps its (now relative) indent and survives.
        let host = "type: canvas\nroot:\n  ^: outer\n  type: node\n  child:\n    ^: inner\n    type: node\n";
        let span = span_of(host, "^: outer");
        let file = extract_record_file(host, span, &[]).unwrap();
        assert_eq!(file, "type: node\nchild:\n  ^: inner\n  type: node\n");
    }

    #[test]
    fn applies_inner_self_reference_edits_before_de_indenting() {
        // A host-absolute edit inside the record (a self-reference redirected to
        // the new file) lands in the extracted content, de-indented with the rest.
        let host =
            "type: canvas\nnodes:\n  - ^: rec\n    type: node\n    related: \"[[host^rec]]\"\n";
        let span = span_of(host, "^: rec");
        let link_start = host.find("[[host^rec]]").unwrap();
        let inner = [(
            ByteRange::new(link_start, link_start + "[[host^rec]]".len()),
            "[[promoted]]".to_string(),
        )];
        let file = extract_record_file(host, span, &inner).unwrap();
        assert_eq!(file, "type: node\nrelated: \"[[promoted]]\"\n");
    }
}
