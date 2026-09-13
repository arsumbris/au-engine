//! YAML parser. Re-exports `saphyr`'s `MarkedYaml` and provides a thin `parse`
//! helper that wraps load errors in a typed `YamlError` carrying a byte range
//! suitable for diagnostics. Saphyr's `Marker::index()` counts Unicode scalars,
//! not bytes, so consumers of any span must go through `span_to_byte_range` to
//! get a slice-safe range.

use std::cell::RefCell;
use std::marker::PhantomData;

use au_diagnostics::ByteRange;
pub use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, YamlData};
use saphyr_parser::{Event, Parser};
pub use saphyr_parser::{Marker, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlError {
    pub message: String,
    pub range: ByteRange,
}

/// Parse YAML text into one or more documents. Spans on each node are
/// relative to `text` (i.e., to the start of the input string, not to any
/// enclosing source file). Callers that pass a frontmatter slice are
/// responsible for adjusting offsets if they need source-file-relative ranges.
pub fn parse(text: &str) -> Result<Vec<MarkedYaml<'_>>, YamlError> {
    MarkedYaml::load_from_str(text).map_err(|e| {
        let m = *e.marker();
        let byte = codepoint_to_byte(text, m.index());
        YamlError {
            message: e.to_string(),
            range: ByteRange::new(byte, byte),
        }
    })
}

/// Convert a saphyr `Span` into a file-relative `ByteRange`.
///
/// Saphyr's `Marker::index()` counts **Unicode scalar values** (`char` count),
/// not bytes. For pure-ASCII sources the two coincide, but multi-byte UTF-8
/// (`—`, `🎉`, accented letters) drifts the byte offset. This helper converts
/// codepoint offsets to byte offsets so downstream slices of the same source
/// string land in the right place.
///
/// `source` is the entire file content; `yaml_offset` is the byte offset
/// within `source` where the YAML body begins (0 for pure-YAML type-def
/// files; the frontmatter delimiter offset for markdown). The returned range
/// is file-relative — it has already been shifted by `yaml_offset`, so
/// callers don't need to combine with `shift`.
pub fn span_to_byte_range(source: &str, yaml_offset: usize, span: Span) -> ByteRange {
    let body = &source[yaml_offset..];
    let start = codepoint_to_byte(body, span.start.index()) + yaml_offset;
    let end = codepoint_to_byte(body, span.end.index()) + yaml_offset;
    ByteRange::new(start, end)
}

/// Saphyr reports span boundaries as codepoint offsets, but every consumer
/// needs byte offsets to slice the source. The naive conversion scans from the
/// start of the string per call, so a file with N spans costs O(N × length) —
/// quadratic on large instances (a session-log of thousands of records grinds
/// for minutes). [`index_source`] installs a checkpoint table for the source
/// being parsed so each conversion scans at most [`CHECKPOINT_STRIDE`] chars.
fn codepoint_to_byte(s: &str, codepoint_idx: usize) -> usize {
    CP_INDEX
        .with(|cell| {
            let cell = cell.borrow();
            match cell.as_ref() {
                Some(index) if index.matches(s) => Some(index.byte(s, codepoint_idx)),
                _ => None,
            }
        })
        .unwrap_or_else(|| {
            CP_FALLBACK_COUNT.with(|c| c.set(c.get() + 1));
            scan_codepoint_to_byte(s, codepoint_idx)
        })
}

thread_local! {
    /// Counts the linear-scan fallbacks (the O(n) path) since the last
    /// [`take_cp_fallback_count`]. Incremented only when no matching index is
    /// installed, so it is zero on the hot indexed path and adds nothing there.
    static CP_FALLBACK_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The number of codepoint→byte conversions that took the O(n) linear-scan
/// fallback since the last call, resetting the counter.
///
/// A diagnostic hook for the perf-regression this guards: a per-file parse that
/// installs [`index_source`] over its body should convert every span through the
/// index, so a well-formed instance parses with ZERO fallbacks. A nonzero count
/// after a parse means a slice was left on the quadratic path (see
/// [`index_source`]'s slice-matching contract).
pub fn take_cp_fallback_count() -> usize {
    CP_FALLBACK_COUNT.with(|c| c.replace(0))
}

/// The fallback conversion: a linear scan from the start. Correct everywhere,
/// used when no matching index is installed (error markers, direct callers).
fn scan_codepoint_to_byte(s: &str, codepoint_idx: usize) -> usize {
    s.char_indices()
        .nth(codepoint_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// Chars between byte-offset checkpoints. A conversion scans at most this many
/// chars from the nearest checkpoint, so the table stays small (one entry per
/// stride) while keeping each lookup cheap.
const CHECKPOINT_STRIDE: usize = 64;

/// A codepoint→byte table for one source string. `checkpoints[i]` is the byte
/// offset of codepoint `i * CHECKPOINT_STRIDE`. Identity is the source's
/// pointer and length: a live `&str` of a given address and length is unique,
/// and [`SourceIndexGuard`] borrows it, so the table can never be matched
/// against a different source occupying a freed address.
struct CodepointIndex {
    ptr: *const u8,
    len: usize,
    checkpoints: Vec<usize>,
}

impl CodepointIndex {
    fn build(s: &str) -> Self {
        let mut checkpoints = Vec::with_capacity(s.len() / CHECKPOINT_STRIDE + 1);
        for (cp, (byte, _)) in s.char_indices().enumerate() {
            if cp % CHECKPOINT_STRIDE == 0 {
                checkpoints.push(byte);
            }
        }
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
            checkpoints,
        }
    }

    fn matches(&self, s: &str) -> bool {
        self.ptr == s.as_ptr() && self.len == s.len()
    }

    fn byte(&self, s: &str, codepoint_idx: usize) -> usize {
        let chunk = codepoint_idx / CHECKPOINT_STRIDE;
        let Some(&base) = self.checkpoints.get(chunk) else {
            return s.len();
        };
        let rem = codepoint_idx % CHECKPOINT_STRIDE;
        if rem == 0 {
            return base;
        }
        match s[base..].char_indices().nth(rem) {
            Some((byte, _)) => base + byte,
            None => s.len(),
        }
    }
}

thread_local! {
    /// The codepoint→byte table for the source currently being parsed, if any.
    static CP_INDEX: RefCell<Option<CodepointIndex>> = const { RefCell::new(None) };
}

/// Installs a codepoint→byte index over `source` for the lifetime of the
/// returned guard, making [`span_to_byte_range`] resolve offsets in
/// `O(CHECKPOINT_STRIDE)` instead of scanning from the start.
///
/// Call it once at the top of a per-file parse with the same slice the parse's
/// spans are relative to (the YAML body — `&full[yaml_offset..]` for markdown).
/// Nesting is supported: the previous index is restored on drop, so a nested
/// parse over a sub-slice installs its own index and cleanly restores the
/// outer one. The guard borrows `source`, tying the table's validity to the
/// source staying alive.
#[must_use]
pub fn index_source(source: &str) -> SourceIndexGuard<'_> {
    let index = CodepointIndex::build(source);
    let prev = CP_INDEX.with(|cell| cell.borrow_mut().replace(index));
    SourceIndexGuard {
        prev: Some(prev),
        _source: PhantomData,
    }
}

/// Restores the previously installed codepoint index when dropped. See
/// [`index_source`].
pub struct SourceIndexGuard<'a> {
    prev: Option<Option<CodepointIndex>>,
    _source: PhantomData<&'a str>,
}

impl Drop for SourceIndexGuard<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            CP_INDEX.with(|cell| *cell.borrow_mut() = prev);
        }
    }
}

/// One duplicate-key occurrence inside a YAML mapping. `first_span`
/// points at the earliest occurrence; `duplicate_span` at the
/// second-or-later one. Saphyr's `LinkedHashMap` silently keeps the
/// last value and drops earlier ones; this struct lets callers surface
/// the silent drop as a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateKey {
    pub key: String,
    pub first_span: Span,
    pub duplicate_span: Span,
}

/// Walk a YAML source's event stream and report duplicate keys at every
/// mapping nesting level. Returns an empty vec on parse failure — the
/// AST parser surfaces parse errors separately, and there's no point
/// reporting duplicates inside a document that doesn't load.
///
/// Spans are saphyr's codepoint-indexed `Span`s; callers convert via
/// `span_to_byte_range` for diagnostic ranges.
pub fn scan_duplicate_keys(text: &str) -> Vec<DuplicateKey> {
    use std::collections::BTreeMap;

    enum Frame {
        Mapping {
            expecting_key: bool,
            seen: BTreeMap<String, Span>,
        },
        Sequence,
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut out: Vec<DuplicateKey> = Vec::new();

    let mut parser = Parser::new_from_str(text);
    loop {
        match parser.next_event() {
            Some(Ok((event, span))) => match event {
                Event::StreamEnd => break,
                Event::MappingStart(..) => {
                    stack.push(Frame::Mapping {
                        expecting_key: true,
                        seen: BTreeMap::new(),
                    });
                }
                Event::MappingEnd => {
                    stack.pop();
                    if let Some(Frame::Mapping { expecting_key, .. }) = stack.last_mut() {
                        *expecting_key = !*expecting_key;
                    }
                }
                Event::SequenceStart(..) => {
                    stack.push(Frame::Sequence);
                }
                Event::SequenceEnd => {
                    stack.pop();
                    if let Some(Frame::Mapping { expecting_key, .. }) = stack.last_mut() {
                        *expecting_key = !*expecting_key;
                    }
                }
                Event::Scalar(value, ..) => {
                    if let Some(Frame::Mapping {
                        expecting_key,
                        seen,
                    }) = stack.last_mut()
                    {
                        if *expecting_key {
                            let key = value.to_string();
                            match seen.get(&key) {
                                Some(first) => {
                                    out.push(DuplicateKey {
                                        key,
                                        first_span: *first,
                                        duplicate_span: span,
                                    });
                                }
                                None => {
                                    seen.insert(key, span);
                                }
                            }
                        }
                        *expecting_key = !*expecting_key;
                    }
                }
                Event::Alias(..) => {
                    // An alias node (`*anchor`) occupies one key-or-value slot
                    // in a mapping, exactly like a scalar — unlike a nested
                    // mapping/sequence, it has no End event to toggle the
                    // phase. We can't recover the resolved key string from the
                    // anchor id, so an alias in key position is not checked for
                    // duplication, but it MUST still flip the key/value phase.
                    // Without this, a `key: *anchor` value leaves the frame
                    // stuck in value-phase and every following key is read in
                    // the wrong slot, silently missing real duplicates.
                    if let Some(Frame::Mapping { expecting_key, .. }) = stack.last_mut() {
                        *expecting_key = !*expecting_key;
                    }
                }
                _ => {}
            },
            Some(Err(_)) => return Vec::new(),
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_mapping() {
        let docs = parse("type: foo\n").unwrap();
        let doc = &docs[0];
        match &doc.data {
            YamlData::Mapping(m) => assert_eq!(m.len(), 1),
            other => panic!("expected mapping, got {other:?}"),
        }
    }

    #[test]
    fn preserves_byte_spans() {
        let text = "type: [note]\nfields:\n  - foo: String\n";
        let docs = parse(text).unwrap();
        let map = match &docs[0].data {
            YamlData::Mapping(m) => m,
            _ => panic!(),
        };
        let (key, _) = map.iter().next().unwrap();
        let key_range = span_to_byte_range(text, 0, key.span);
        assert_eq!(&text[key_range.start..key_range.end], "type");
    }

    #[test]
    fn surfaces_parse_errors_with_span() {
        let err = parse("[unclosed").unwrap_err();
        assert!(!err.message.is_empty());
    }

    /// Saphyr indexes spans in codepoints; multi-byte UTF-8 chars before a
    /// span shift the byte offset away from the codepoint offset. The helper
    /// must convert. This test pins the behavior — break it and downstream
    /// source slicing will land mid-char or in the wrong place.
    #[test]
    fn span_to_byte_range_handles_multibyte_chars() {
        // em-dash `—` is 3 bytes UTF-8, 1 codepoint.
        let text = "# — comment\nk: [low, high]\n";
        let docs = parse(text).unwrap();
        let map = match &docs[0].data {
            YamlData::Mapping(m) => m,
            _ => panic!(),
        };
        let (_k, v) = map.iter().next().unwrap();
        let range = span_to_byte_range(text, 0, v.span);
        // Saphyr's end span sits ON the closing `]`, so the slice is the
        // open-bracket-and-contents (callers extend by 1 byte to include the
        // closer). The test here is that `start` lands on `[`, not earlier.
        assert_eq!(&text[range.start..=range.end], "[low, high]");
    }

    #[test]
    fn span_to_byte_range_handles_non_bmp() {
        // 🎉 (U+1F389) is 4 bytes UTF-8, 1 Unicode scalar, 2 UTF-16 code units.
        // Confirms saphyr counts Unicode scalars, not UTF-16 code units.
        let text = "# 🎉\nk: String\n";
        let docs = parse(text).unwrap();
        let map = match &docs[0].data {
            YamlData::Mapping(m) => m,
            _ => panic!(),
        };
        let (_k, v) = map.iter().next().unwrap();
        let range = span_to_byte_range(text, 0, v.span);
        assert_eq!(&text[range.start..range.end], "String");
    }

    #[test]
    fn indexed_conversion_matches_scan_across_checkpoints() {
        // A mixed ASCII / multibyte string long enough to cross many
        // CHECKPOINT_STRIDE boundaries. The fast path (index installed) must
        // agree with the linear scan at every codepoint, including past EOF.
        let unit = "aé🎉b—c"; // 1+2+4+1+3+1 bytes, 6 codepoints
        let text = unit.repeat(200); // 1200 codepoints, > 18 checkpoints
        let char_count = text.chars().count();

        let _index = index_source(&text);
        for cp in 0..=char_count + 2 {
            assert_eq!(
                codepoint_to_byte(&text, cp),
                scan_codepoint_to_byte(&text, cp),
                "mismatch at codepoint {cp}"
            );
        }
    }

    #[test]
    fn index_does_not_match_a_different_source() {
        // The index only applies to the exact source it was built over; an
        // unrelated string falls back to the scan (correctly).
        let indexed = "key: value\n".repeat(50);
        let other = "🎉🎉🎉other".to_string();
        let _index = index_source(&indexed);
        for cp in 0..=other.chars().count() {
            assert_eq!(
                codepoint_to_byte(&other, cp),
                scan_codepoint_to_byte(&other, cp)
            );
        }
    }

    #[test]
    fn scan_duplicate_keys_flags_top_level_duplicates() {
        // Saphyr's `LinkedHashMap` would silently drop the first `foo`;
        // the scanner sees both events and reports the second-occurrence
        // span with the first-occurrence span as related.
        let dups = scan_duplicate_keys("type: foo\nfoo: a\nfoo: b\n");
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].key, "foo");
        // Spans differ (different positions in source).
        assert_ne!(
            dups[0].first_span.start.index(),
            dups[0].duplicate_span.start.index()
        );
    }

    #[test]
    fn scan_duplicate_keys_flags_nested_mapping_duplicates() {
        let dups = scan_duplicate_keys("outer:\n  k: 1\n  k: 2\n");
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].key, "k");
    }

    #[test]
    fn scan_duplicate_keys_ignores_repeated_sequence_items() {
        // `foo` appears twice as a SEQUENCE ELEMENT (not a key); not a
        // YAML duplicate-key issue.
        let dups = scan_duplicate_keys("items:\n  - foo\n  - foo\n");
        assert!(dups.is_empty(), "got {:?}", dups);
    }

    #[test]
    fn scan_duplicate_keys_clean_mapping_fires_nothing() {
        let dups = scan_duplicate_keys("a: 1\nb: 2\nc: 3\n");
        assert!(dups.is_empty(), "got {:?}", dups);
    }

    #[test]
    fn scan_duplicate_keys_handles_repeated_keys_in_sibling_mappings() {
        // `k` in two different inline mappings — not a duplicate within
        // either mapping.
        let dups = scan_duplicate_keys("a: {k: 1}\nb: {k: 2}\n");
        assert!(dups.is_empty(), "got {:?}", dups);
    }

    #[test]
    fn scan_duplicate_keys_survives_an_alias_value() {
        // An alias value (`b: *a`) once left the key/value phase desynced, so
        // the real duplicate `c` after it was silently missed. The alias must
        // toggle the phase like a scalar.
        let dups = scan_duplicate_keys("x: &a 1\nb: *a\nc: 1\nc: 2\n");
        assert_eq!(dups.len(), 1, "got {:?}", dups);
        assert_eq!(dups[0].key, "c");
    }

    #[test]
    fn scan_duplicate_keys_clean_mapping_with_alias_fires_nothing() {
        // The phase toggle must not over-fire: a clean mapping that uses an
        // alias value stays clean.
        let dups = scan_duplicate_keys("x: &a 1\nb: *a\nc: 3\n");
        assert!(dups.is_empty(), "got {:?}", dups);
    }

    #[test]
    fn parse_error_range_is_byte_offset_not_codepoint() {
        // Em-dash before the unclosed bracket: any knowledge base with multi-byte
        // chars would point at the wrong byte if the error range were
        // codepoint-indexed. The diagnostic surface needs byte ranges.
        let text = "# — em-dash\n[unclosed";
        let err = parse(text).unwrap_err();
        // The marker index from saphyr is in codepoints; after conversion
        // it must land somewhere a slice can use.
        assert!(text.is_char_boundary(err.range.start));
        assert!(text.is_char_boundary(err.range.end));
    }
}
