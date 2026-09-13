//! The content fold for `inline`: replace a `[[file]]` reference in a host with
//! the referenced file's content as an inline `^:id` record. The inverse of
//! [`crate::promote`]'s extract. See [[spec - reference-rewriting refactors -
//! promote inline rename keep the mounted-set graph consistent]].
//!
//! Span-located, source-rewritten. The referenced file's frontmatter is the
//! record body; it is re-indented from a top-level document into the host's
//! slot, with an engine-assigned `^:` id prepended. A `.md` file's body must be
//! empty — a record has no body, folding one would drop the prose.

use au_diagnostics::ByteRange;

use crate::mutate::MutationReject;

/// The referenced file's frontmatter (the record body), plus whether the file
/// carries a non-empty markdown body. `is_md` files fence the frontmatter in
/// `---`; `.yaml`/`.yml` files are bare frontmatter with no body.
pub(crate) fn frontmatter_and_has_body(
    content: &str,
    is_md: bool,
) -> Result<(String, bool), MutationReject> {
    if !is_md {
        return Ok((content.trim_end().to_string(), false));
    }
    // A `.md` instance opens with a `---` fence; the frontmatter runs to the
    // next line that is exactly `---`.
    let rest = content.strip_prefix("---\n").ok_or_else(|| {
        MutationReject::new("the file to inline is not a frontmatter document (no leading `---`)")
    })?;
    let close = rest
        .find("\n---")
        .ok_or_else(|| MutationReject::new("the file to inline has no closing `---` fence"))?;
    let frontmatter = &rest[..close];
    // Everything after the closing fence line is the body.
    let after = &rest[close + "\n---".len()..];
    let after = after
        .strip_prefix('\n')
        .unwrap_or(after.trim_start_matches('-'));
    let has_body = !after.trim().is_empty();
    Ok((frontmatter.trim_end().to_string(), has_body))
}

/// The (span, replacement) edit that folds `frontmatter` into `host` at the
/// `[[file]]` reference occupying `value_span`, as a `^:id` record.
///
/// The host's source before the value decides the shape:
/// - a sequence element (`- "[[file]]"`) keeps its `- ` marker; the record's
///   first key rides inline after it, the rest indent to the value column.
/// - a field (`key: "[[file]]"`) keeps its `key:`; the whole record moves to
///   the following lines, indented two past the key.
///
/// Each frontmatter line keeps its own relative indentation, so nested records
/// survive. Returns the edit, not the applied result, so the caller can merge it
/// with the host's other reference rewrites in one last-first pass.
pub(crate) fn fold_edit(
    host: &str,
    value_span: ByteRange,
    frontmatter: &str,
    id: &str,
) -> Result<(ByteRange, String), MutationReject> {
    if value_span.end > host.len()
        || !host.is_char_boundary(value_span.start)
        || !host.is_char_boundary(value_span.end)
    {
        return Err(MutationReject::new(
            "a reference span is out of range — the file drifted from the index",
        ));
    }
    let line_start = host[..value_span.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = &host[line_start..value_span.start];

    // The record's lines: the `^:` id, then the frontmatter verbatim.
    let mut lines: Vec<&str> = vec![];
    let id_line = format!("^: {id}");
    lines.push(&id_line);
    for l in frontmatter.trim_end().split('\n') {
        lines.push(l);
    }

    let is_seq_element = prefix.trim_end().ends_with('-');
    let replacement = if is_seq_element {
        // First key inline after `- `; continuation at the value column.
        let indent = " ".repeat(prefix.len());
        let mut out = String::new();
        for (i, l) in lines.iter().enumerate() {
            if i == 0 {
                out.push_str(l);
            } else {
                out.push('\n');
                out.push_str(&indent);
                out.push_str(l);
            }
        }
        out
    } else {
        // A field: the whole record on the following lines, indented two past
        // the key. The `key:` and its trailing space stay; the value is replaced
        // by a newline-led block.
        let key_indent = prefix.len() - prefix.trim_start().len();
        let indent = " ".repeat(key_indent + 2);
        let mut out = String::new();
        for l in &lines {
            out.push('\n');
            out.push_str(&indent);
            out.push_str(l);
        }
        out
    };
    Ok((value_span, replacement))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rename::apply_edits;

    fn span_of(host: &str, value: &str) -> ByteRange {
        let start = host.find(value).expect("value present");
        ByteRange::new(start, start + value.len())
    }

    #[test]
    fn md_frontmatter_splits_from_body() {
        let (fm, body) =
            frontmatter_and_has_body("---\ntype: node\ncontent: x\n---\n", true).unwrap();
        assert_eq!(fm, "type: node\ncontent: x");
        assert!(!body);
        let (_, body) =
            frontmatter_and_has_body("---\ntype: node\n---\nprose here\n", true).unwrap();
        assert!(body, "a non-empty body is detected");
    }

    #[test]
    fn yaml_file_is_bare_frontmatter_no_body() {
        let (fm, body) = frontmatter_and_has_body("type: node\ncontent: x\n", false).unwrap();
        assert_eq!(fm, "type: node\ncontent: x");
        assert!(!body);
    }

    #[test]
    fn folds_into_a_sequence_element() {
        let host = "type: canvas\nnodes:\n  - \"[[child]]\"\n";
        let span = span_of(host, "\"[[child]]\"");
        let (sp, repl) = fold_edit(host, span, "type: node\ncontent: x", "b-1").unwrap();
        let out = apply_edits(host, vec![(sp, repl)]).unwrap();
        assert_eq!(
            out,
            "type: canvas\nnodes:\n  - ^: b-1\n    type: node\n    content: x\n"
        );
    }

    #[test]
    fn folds_into_a_field_value() {
        let host = "type: canvas\nroot: \"[[child]]\"\n";
        let span = span_of(host, "\"[[child]]\"");
        let (sp, repl) = fold_edit(host, span, "type: node\ncontent: x", "b-1").unwrap();
        let out = apply_edits(host, vec![(sp, repl)]).unwrap();
        assert_eq!(
            out,
            "type: canvas\nroot: \n  ^: b-1\n  type: node\n  content: x\n"
        );
    }

    #[test]
    fn nested_record_indentation_is_preserved() {
        let host = "type: canvas\nnodes:\n  - \"[[child]]\"\n";
        let span = span_of(host, "\"[[child]]\"");
        let (sp, repl) = fold_edit(host, span, "type: node\nchild:\n  type: node", "b-1").unwrap();
        let out = apply_edits(host, vec![(sp, repl)]).unwrap();
        assert_eq!(
            out,
            "type: canvas\nnodes:\n  - ^: b-1\n    type: node\n    child:\n      type: node\n"
        );
    }
}
