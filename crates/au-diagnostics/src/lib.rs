//! Structured diagnostic records with stable codes, spans, and suggested-fix slots.
//!
//! `DiagnosticCode` is a newtype around `&'static str` so codes are added as
//! module-level constants from each producing crate without enum churn. Stable
//! string codes are kebab-case, e.g. `reserved-key-location`.

use std::borrow::Cow;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A stable diagnostic identifier. The backing string is the contract;
/// downstream consumers (LSP, CI gate, formatters) match on the string. Codes
/// are kebab-case.
///
/// Producers declare codes as module-level constants:
/// ```no_run
/// use au_diagnostics::DiagnosticCode;
/// pub const RESERVED_KEY_LOCATION: DiagnosticCode =
///     DiagnosticCode::from_static("reserved-key-location");
/// ```
///
/// Backed by `Cow<'static, str>` so producer-defined codes are zero-cost
/// (`Borrowed`) while deserialized codes from JSON allocate owned strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosticCode(pub Cow<'static, str>);

impl DiagnosticCode {
    pub const fn from_static(code: &'static str) -> Self {
        Self(Cow::Borrowed(code))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The weight of a diagnostic. Part of the wire contract.
///
/// `Error` is the only blocking level: it prevents a downstream stage from
/// running (graph-load errors block per-file validation; per-file errors stay
/// per-file). Every other level is advisory — the stage completes and the
/// issue is surfaced. Consumers match on the serialized string, not the
/// declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Blocks a downstream stage.
    Error,
    /// Advisory, high attention. Something has diverged from a version it was
    /// reconciled against, e.g. a pinned reference's live counterpart or a
    /// shadow of a hardwired engine type. Never blocks: the local copy still
    /// validates standalone. Its own tier so consumers can rank it above
    /// ordinary warnings; reusable for other staleness conditions.
    Drift,
    /// Advisory. The engine completes the stage and surfaces the issue.
    Warning,
    /// Advisory, a suggestion.
    Hint,
}

/// Half-open byte range into a single source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// A 1-based line/column position. `col` counts UTF-8 bytes from the line
/// start plus one, deterministic regardless of encoding width; consumers
/// needing character or UTF-16 columns convert within the one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineCol {
    pub line: usize,
    pub col: usize,
}

/// The line/column rendering of a `ByteRange`, both endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineColRange {
    pub start: LineCol,
    pub end: LineCol,
}

/// Byte-offset → line/column conversion table for one file's content.
///
/// Holds the byte offset of each line start (lines split at `\n` only, so a
/// `\r` counts into the column). Built once per read file, O(log lines) per
/// lookup. Works on raw bytes, so non-UTF-8 files index fine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset of each line's first byte; `[0]` is always 0.
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(bytes: &[u8]) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { line_starts }
    }

    /// The 1-based line/column of a byte offset. An offset past the end of
    /// the content clamps into the last line.
    pub fn line_col(&self, offset: usize) -> LineCol {
        let line_idx = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        LineCol {
            line: line_idx + 1,
            col: offset - self.line_starts[line_idx] + 1,
        }
    }

    /// Both endpoints of a byte range as line/column.
    pub fn line_col_range(&self, range: ByteRange) -> LineColRange {
        LineColRange {
            start: self.line_col(range.start),
            end: self.line_col(range.end),
        }
    }
}

/// A span pins a `ByteRange` to a specific source file. The byte range is
/// canonical; `line_col` is its derived rendering, attached by the engine
/// build once the file's `LineIndex` exists, so line-oriented consumers
/// don't re-read the file to correlate spans.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub file: PathBuf,
    pub range: ByteRange,
    /// `None` until attached, and for spans into files the build never read
    /// (assets, unreadable files). Always served for read files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_col: Option<LineColRange>,
}

impl Span {
    pub fn new(file: impl Into<PathBuf>, range: ByteRange) -> Self {
        Self {
            file: file.into(),
            range,
            line_col: None,
        }
    }

    /// Span targeting a file as a whole, with no specific byte position.
    /// Renders as `file:@0` in the human formatter; suitable for diagnostics
    /// that aren't tied to a particular location (knowledge-base-load advisories,
    /// "this file exists but..." references).
    pub fn for_file(file: impl Into<PathBuf>) -> Self {
        Self::new(file, ByteRange::new(0, 0))
    }

    /// Attach the line/column rendering derived from the file's index.
    pub fn attach_line_col(&mut self, index: &LineIndex) {
        self.line_col = Some(index.line_col_range(self.range));
    }
}

/// Currently advisory text only — never auto-applied. A future structured-edit
/// form may replace `description` once a mutation pipeline lands.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SuggestedFix {
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub span: Span,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<SuggestedFix>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CODE: DiagnosticCode = DiagnosticCode::from_static("test-code");

    #[test]
    fn round_trips_through_json() {
        let diag = Diagnostic {
            code: TEST_CODE.clone(),
            severity: Severity::Error,
            span: Span::new("foo.yaml", ByteRange::new(0, 5)),
            message: "bad".into(),
            related: vec![],
            fix: None,
        };

        let json = serde_json::to_string(&diag).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }

    #[test]
    fn omits_empty_related_and_fix_in_json() {
        let diag = Diagnostic {
            code: TEST_CODE.clone(),
            severity: Severity::Warning,
            span: Span::new("foo.yaml", ByteRange::new(0, 1)),
            message: "x".into(),
            related: vec![],
            fix: None,
        };
        let json = serde_json::to_string(&diag).unwrap();
        assert!(!json.contains("related"));
        assert!(!json.contains("fix"));
    }

    #[test]
    fn code_is_serialized_as_bare_string() {
        let json = serde_json::to_string(&TEST_CODE).unwrap();
        assert_eq!(json, "\"test-code\"");
    }

    #[test]
    fn code_round_trips_to_owned_string() {
        let json = serde_json::to_string(&TEST_CODE).unwrap();
        let back: DiagnosticCode = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "test-code");
    }

    #[test]
    fn line_index_maps_offsets_to_one_based_line_col() {
        let idx = LineIndex::new(b"ab\ncde\n\nf");
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(idx.line_col(2), LineCol { line: 1, col: 3 }); // the \n itself
        assert_eq!(idx.line_col(3), LineCol { line: 2, col: 1 });
        assert_eq!(idx.line_col(7), LineCol { line: 3, col: 1 }); // empty line
        assert_eq!(idx.line_col(8), LineCol { line: 4, col: 1 });
    }

    #[test]
    fn line_index_clamps_past_end_into_last_line() {
        let idx = LineIndex::new(b"ab\ncd");
        assert_eq!(idx.line_col(99), LineCol { line: 2, col: 97 });
    }

    #[test]
    fn line_index_counts_columns_in_bytes() {
        // 'é' is two UTF-8 bytes; col counts bytes, not chars.
        let idx = LineIndex::new("é x".as_bytes());
        assert_eq!(idx.line_col(2), LineCol { line: 1, col: 3 });
    }

    #[test]
    fn line_index_on_empty_content() {
        let idx = LineIndex::new(b"");
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 });
    }

    #[test]
    fn span_serializes_line_col_only_when_attached() {
        let mut span = Span::new("foo.md", ByteRange::new(3, 5));
        let json = serde_json::to_string(&span).unwrap();
        assert!(!json.contains("line_col"));

        span.attach_line_col(&LineIndex::new(b"ab\ncde"));
        let json = serde_json::to_string(&span).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["line_col"]["start"]["line"], 2);
        assert_eq!(v["line_col"]["start"]["col"], 1);
        assert_eq!(v["line_col"]["end"]["line"], 2);
        assert_eq!(v["line_col"]["end"]["col"], 3);
    }
}
