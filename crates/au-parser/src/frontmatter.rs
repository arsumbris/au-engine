//! Splits a markdown/yaml file into its YAML frontmatter and body.
//!
//! Recognized form: file starts with a line containing exactly `---`, ends the
//! frontmatter at the next line containing exactly `---`. Bytes between are the
//! frontmatter; bytes after are the body. Files with no leading `---` have an
//! empty frontmatter and the entire content as body — this is how pure-YAML
//! type-def files (`*.type.yaml`) flow through: the parser will call
//! `whole_as_frontmatter` instead.
//!
//! A leading UTF-8 BOM (`EF BB BF`) is stripped before the `---` check so
//! files saved by BOM-emitting editors aren't silently classified as
//! frontmatter-less.

use au_diagnostics::ByteRange;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterSplit<'a> {
    pub frontmatter: &'a str,
    pub frontmatter_range: ByteRange,
    pub body: &'a str,
    pub body_range: ByteRange,
}

/// Why `split_frontmatter` could not honor a frontmatter-shaped file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterError {
    /// File began with a `---` open line but reached EOF without a matching
    /// `---` close line. `open_range` points at the opening `---` so callers
    /// can render a span pointing at the cause.
    Unterminated { open_range: ByteRange },
}

/// Try to split a markdown file with `---`-delimited YAML frontmatter.
///
/// Returns:
/// - `Ok(Some(_))` — frontmatter present and well-formed.
/// - `Ok(None)` — no frontmatter (no leading `---`, or `---` not followed by
///   a newline). The whole text is body.
/// - `Err(Unterminated)` — file opened with `---\n` but never closed. Pre-fix
///   this collapsed into `Ok(None)`, which silently dropped frontmatter the
///   user clearly intended.
pub fn split_frontmatter(text: &str) -> Result<Option<FrontmatterSplit<'_>>, FrontmatterError> {
    let bytes = text.as_bytes();
    let bom_len = if bytes.starts_with(UTF8_BOM) { 3 } else { 0 };
    let after_bom = &bytes[bom_len..];
    if !after_bom.starts_with(b"---") {
        return Ok(None);
    }
    // Confirm `---` is followed by newline (not by other characters on the line).
    let after_open = match after_bom.get(3) {
        Some(b'\n') => bom_len + 4,
        Some(b'\r') if after_bom.get(4) == Some(&b'\n') => bom_len + 5,
        _ => return Ok(None),
    };

    let close = match find_close_marker(text, after_open) {
        Some(c) => c,
        None => {
            return Err(FrontmatterError::Unterminated {
                open_range: ByteRange::new(bom_len, bom_len + 3),
            });
        }
    };
    let frontmatter_start = after_open;
    let frontmatter_end = close.frontmatter_end;
    let body_start = close.body_start;

    Ok(Some(FrontmatterSplit {
        frontmatter: &text[frontmatter_start..frontmatter_end],
        frontmatter_range: ByteRange::new(frontmatter_start, frontmatter_end),
        body: &text[body_start..],
        body_range: ByteRange::new(body_start, text.len()),
    }))
}

/// Treat the whole file as frontmatter (no body). Used for pure-YAML files like
/// `*.type.yaml` where there's never a markdown body.
pub fn whole_as_frontmatter(text: &str) -> FrontmatterSplit<'_> {
    let len = text.len();
    FrontmatterSplit {
        frontmatter: text,
        frontmatter_range: ByteRange::new(0, len),
        body: "",
        body_range: ByteRange::new(len, len),
    }
}

struct CloseMarker {
    frontmatter_end: usize,
    body_start: usize,
}

fn find_close_marker(text: &str, search_from: usize) -> Option<CloseMarker> {
    let bytes = text.as_bytes();
    let mut line_start = search_from;
    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| line_start + p)
            .unwrap_or(bytes.len());
        // Trim a trailing \r for CRLF.
        let trim_end = if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        let line = &bytes[line_start..trim_end];
        if line == b"---" {
            // Frontmatter ends at the start of this `---` line — i.e. it
            // includes the trailing newline of the previous content line, but
            // not the `---` itself.
            let fm_end = line_start;
            // Body starts after the newline that follows this `---` line.
            let body_start = if line_end < bytes.len() {
                line_end + 1
            } else {
                line_end
            };
            return Some(CloseMarker {
                frontmatter_end: fm_end,
                body_start,
            });
        }
        if line_end >= bytes.len() {
            break;
        }
        line_start = line_end + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_ok_none() {
        assert_eq!(split_frontmatter("just a body\n"), Ok(None));
        assert_eq!(split_frontmatter("---no-newline-after"), Ok(None));
    }

    #[test]
    fn splits_minimal_frontmatter() {
        let text = "---\ntype: foo\n---\nbody\n";
        let split = split_frontmatter(text).unwrap().unwrap();
        assert_eq!(split.frontmatter, "type: foo\n");
        assert_eq!(split.body, "body\n");
    }

    #[test]
    fn splits_empty_frontmatter() {
        let text = "---\n---\nbody\n";
        let split = split_frontmatter(text).unwrap().unwrap();
        assert_eq!(split.frontmatter, "");
        assert_eq!(split.body, "body\n");
    }

    #[test]
    fn splits_with_trailing_no_body() {
        let text = "---\ntype: foo\n---\n";
        let split = split_frontmatter(text).unwrap().unwrap();
        assert_eq!(split.frontmatter, "type: foo\n");
        assert_eq!(split.body, "");
    }

    #[test]
    fn handles_crlf() {
        let text = "---\r\ntype: foo\r\n---\r\nbody\r\n";
        let split = split_frontmatter(text).unwrap().unwrap();
        assert_eq!(split.frontmatter, "type: foo\r\n");
        assert_eq!(split.body, "body\r\n");
    }

    #[test]
    fn whole_file_as_frontmatter() {
        let split = whole_as_frontmatter("type: foo\n");
        assert_eq!(split.frontmatter, "type: foo\n");
        assert_eq!(split.body, "");
        assert_eq!(split.frontmatter_range, ByteRange::new(0, 10));
    }

    #[test]
    fn strips_leading_bom_and_recognizes_frontmatter() {
        // Pre-fix this returned `Ok(None)` — the BOM kept `---` from being
        // detected at byte 0, silently classifying the file as frontmatter-less.
        let text = "\u{FEFF}---\ntype: foo\n---\nbody\n";
        let split = split_frontmatter(text).unwrap().unwrap();
        assert_eq!(split.frontmatter, "type: foo\n");
        assert_eq!(split.body, "body\n");
        // Frontmatter starts after BOM (3 bytes) + open marker (4 bytes) = 7.
        assert_eq!(split.frontmatter_range.start, 7);
    }

    #[test]
    fn unterminated_frontmatter_surfaces_as_err() {
        // Pre-fix `Ok(None)` (silently no-frontmatter); now distinguishable.
        let err = split_frontmatter("---\ntype: foo\nno closer here\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::Unterminated {
                open_range: ByteRange::new(0, 3)
            }
        );
    }

    #[test]
    fn unterminated_frontmatter_with_bom_points_past_bom() {
        let err = split_frontmatter("\u{FEFF}---\ntype: foo\n").unwrap_err();
        assert_eq!(
            err,
            FrontmatterError::Unterminated {
                open_range: ByteRange::new(3, 6)
            }
        );
    }
}
