//! Markdown body scanner.
//!
//! Walks a markdown body (the bytes after the frontmatter split) and emits a
//! typed event stream with byte spans. Recognized event kinds:
//! - ATX headings (`#` ... `######`)
//! - fenced code blocks (``` ``` ``` with info string)
//! - inline code spans (backtick-delimited runs inside prose)
//! - wikilink occurrences (`[[…]]`)
//! - block-id markers (`^id` on their own line, typically after a fenced
//!   block close)
//!
//! The scanner is pure: bytes in, events out. No type-system knowledge — that
//! happens in `au-core`. The scanner never panics on malformed input; it emits
//! what it can and ignores the rest.

use au_diagnostics::ByteRange;
use std::collections::BTreeMap;

/// One token in the body scan.
///
/// Every variant carries a `span` covering the bytes the event represents.
/// Text slices borrow from the original source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyEvent<'a> {
    /// `#` ... `######` heading line. `text` is the trimmed heading text;
    /// `span` covers the full line including leading hashes and trailing
    /// newline (if any).
    Heading {
        level: u8,
        text: &'a str,
        span: ByteRange,
    },
    /// Fenced code block. `info` is the raw fence-info string (everything
    /// after the opening fence's backticks on the same line); `body` is the
    /// verbatim content between the fences; `span` covers both fences and
    /// the body. `trailing_block_id` is set when the line immediately after
    /// the closing fence is a bare `^id` marker.
    FencedBlock {
        info: &'a str,
        body: &'a str,
        span: ByteRange,
        trailing_block_id: Option<&'a str>,
    },
    /// Inline code span inside a prose run. `content` is the text between
    /// the delimiting backticks; `span` covers the full span including
    /// delimiters.
    InlineCode { content: &'a str, span: ByteRange },
    /// Wikilink occurrence. `raw` is the text between the `[[` and `]]`;
    /// `span` covers the full span including delimiters. Parsing the raw
    /// payload (target, fragments) belongs to `au-references`.
    Wikilink { raw: &'a str, span: ByteRange },
    /// Bare `^id` marker on its own line. `id` is the identifier; `span`
    /// covers the marker line.
    BlockIdMarker { id: &'a str, span: ByteRange },
    /// A fence-open line that the scanner couldn't pair with a closing
    /// fence anywhere later in the source. `info` is the raw fence-info
    /// string; `span` covers the fence-open line only.
    ///
    /// Emitted in lieu of a `FencedBlock` so downstream typing checks
    /// can fire `body-unterminated-fence`. Subsequent lines are scanned
    /// as ordinary content — the unterminated fence-open does NOT
    /// swallow events below it.
    UnterminatedFenceOpen { info: &'a str, span: ByteRange },
}

impl<'a> BodyEvent<'a> {
    /// The byte span this event covers in the source. Every variant
    /// carries a `span` field; this method de-duplicates the
    /// boilerplate match across the parser, testkit, and any
    /// downstream consumer that needs a uniform position handle.
    pub fn span(&self) -> ByteRange {
        match self {
            BodyEvent::Heading { span, .. }
            | BodyEvent::FencedBlock { span, .. }
            | BodyEvent::InlineCode { span, .. }
            | BodyEvent::Wikilink { span, .. }
            | BodyEvent::BlockIdMarker { span, .. }
            | BodyEvent::UnterminatedFenceOpen { span, .. } => *span,
        }
    }
}

/// Scan a markdown body and return its event stream.
///
/// Linear pass — never panics on malformed input. Headings, fenced code
/// blocks, and bare `^id` block-id markers are recognized. Fenced blocks
/// absorb everything between their open and close fences (so heading-shaped
/// content inside doesn't escape). Fences are variable-length per CommonMark:
/// a fence opened with N backticks closes only on a run of at least N, so a
/// longer fence wraps a shorter example (a quad-backtick fence around a
/// triple-backtick block). A fence-open with no matching close anywhere
/// later in the source emits `UnterminatedFenceOpen` on the open line;
/// subsequent lines continue to be scanned as ordinary content (headings,
/// wikilinks, inline code all still emit).
pub fn scan_body(src: &str) -> Vec<BodyEvent<'_>> {
    let mut events = Vec::new();
    let lines: Vec<(&str, ByteRange)> = iter_lines(src).collect();
    let mut i = 0;
    while i < lines.len() {
        let (line, span) = lines[i];

        if let Some((info, run_len)) = parse_fence_open(line) {
            // Scan ahead for a matching close fence without committing.
            // CommonMark variable-length fences: a fence opened with
            // `run_len` backticks closes only on a run of at least that
            // many. So a longer fence wraps a shorter one — a quad-backtick
            // fence can contain a triple-backtick example without the inner
            // close ending the outer block.
            let close_at = (i + 1..lines.len()).find(|&j| is_fence_close(lines[j].0, run_len));
            match close_at {
                Some(j) => {
                    let body_start = span.end;
                    let close_span = lines[j].1;
                    let body = &src[body_start..close_span.start];
                    let trailing_block_id =
                        lines.get(j + 1).and_then(|(l, _)| parse_block_id_marker(l));
                    let full_span = if trailing_block_id.is_some() {
                        ByteRange::new(span.start, lines[j + 1].1.end)
                    } else {
                        ByteRange::new(span.start, close_span.end)
                    };
                    events.push(BodyEvent::FencedBlock {
                        info,
                        body,
                        span: full_span,
                        trailing_block_id,
                    });
                    i = if trailing_block_id.is_some() {
                        j + 2
                    } else {
                        j + 1
                    };
                    continue;
                }
                None => {
                    // Unterminated — emit the marker, treat the
                    // fence-open line as ordinary text (scan any
                    // inline-code in it), continue to subsequent lines.
                    events.push(BodyEvent::UnterminatedFenceOpen { info, span });
                    scan_inline_code(line, span.start, &mut events);
                    i += 1;
                    continue;
                }
            }
        }

        // `^id` markers attach own-line (the whole line is the marker)
        // or trailing (the line's last whitespace-separated token), per
        // [[type block-id::au-type-system]]. The trailing token is excluded from the
        // content the rest of the line scanning sees, so a heading's
        // text never carries its marker.
        let (content, trailing) = split_trailing_block_id(line);
        if let Some((id, offset)) = trailing {
            if content.trim().is_empty() {
                // Own-line form: the whole line is the marker.
                events.push(BodyEvent::BlockIdMarker { id, span });
                i += 1;
                continue;
            }
            let marker_span =
                ByteRange::new(span.start + offset, span.start + offset + 1 + id.len());
            events.push(BodyEvent::BlockIdMarker {
                id,
                span: marker_span,
            });
        }

        if let Some((level, text)) = parse_atx_heading(content) {
            events.push(BodyEvent::Heading { level, text, span });
            i += 1;
            continue;
        }

        scan_inline_code(content, span.start, &mut events);
        i += 1;
    }

    let exclusions: Vec<ByteRange> = events
        .iter()
        .filter_map(|e| match e {
            BodyEvent::FencedBlock { span, .. }
            | BodyEvent::InlineCode { span, .. }
            | BodyEvent::Heading { span, .. } => Some(*span),
            _ => None,
        })
        .collect();
    scan_wikilinks(src, &exclusions, &mut events);

    events.sort_by_key(event_span_start);

    events
}

/// Pair each event with the root-to-leaf section path enclosing it, per [[type value container::au-type-system]].
///
/// Path elements are formatted as `"<1-based-index-at-level> <heading text>"`;
/// sibling indexing is scoped to the current parent and restarts under new
/// parents.
///
/// Body-preamble events (before any heading) carry the empty path. Heading
/// events themselves carry a path that INCLUDES the heading they introduce —
/// so a `# Why` heading at the top sees path `["1 Why"]`, and a `[:field]`
/// inside it sees the same.
///
/// Emits *raw* paths: every heading event contributes to the stack regardless
/// of whether it matches a declared template section. Filtering to declared
/// sections is the validator's job.
pub fn derive_section_paths<'a>(
    events: &'a [BodyEvent<'a>],
) -> Vec<(Vec<String>, &'a BodyEvent<'a>)> {
    struct Frame<'a> {
        level: u8,
        idx: u32,
        text: &'a str,
        child_counts: BTreeMap<u8, u32>,
    }

    let mut stack: Vec<Frame<'_>> = vec![Frame {
        level: 0,
        idx: 0,
        text: "",
        child_counts: BTreeMap::new(),
    }];
    let mut out = Vec::with_capacity(events.len());

    // Cache the rendered path across non-heading events. Headings
    // mutate the stack, so we recompute on those; non-heading events
    // under the same heading share the path string and avoid an
    // N×depth allocation per body event.
    let mut cached_path: Vec<String> = Vec::new();
    let mut path_dirty = true;
    for event in events {
        if let BodyEvent::Heading { level, text, .. } = event {
            while stack.last().unwrap().level >= *level {
                stack.pop();
            }
            let parent = stack.last_mut().unwrap();
            let counter = parent.child_counts.entry(*level).or_insert(0);
            *counter += 1;
            let self_idx = *counter;
            stack.push(Frame {
                level: *level,
                idx: self_idx,
                text,
                child_counts: BTreeMap::new(),
            });
            path_dirty = true;
        }
        if path_dirty {
            cached_path = stack
                .iter()
                .skip(1)
                .map(|f| format!("{} {}", f.idx, f.text))
                .collect();
            path_dirty = false;
        }
        out.push((cached_path.clone(), event));
    }

    out
}

fn event_span_start(e: &BodyEvent<'_>) -> usize {
    e.span().start
}

/// The end of the zone covering `pos`, or `None`. `zones` are sorted and
/// non-overlapping; `*idx` advances forward past zones ending at or before
/// `pos`, so it MUST be called with non-decreasing `pos` for a given `idx`.
/// O(1) amortized over a monotonic probe sequence.
fn zone_end_at(zones: &[ByteRange], idx: &mut usize, pos: usize) -> Option<usize> {
    while *idx < zones.len() && zones[*idx].end <= pos {
        *idx += 1;
    }
    zones.get(*idx).filter(|z| z.start <= pos).map(|z| z.end)
}

fn scan_wikilinks<'a>(src: &'a str, exclusions: &[ByteRange], out: &mut Vec<BodyEvent<'a>>) {
    for (raw, span) in scan_wikilink_spans(src, exclusions) {
        out.push(BodyEvent::Wikilink { raw, span });
    }
}

/// Scan `src` for `[[...]]` wikilink spans, skipping any byte position
/// inside an `exclusions` zone. Yields each link's inner text and its
/// full span (brackets included).
///
/// A wikilink can't straddle an exclusion zone, and an escaped `\[[` is
/// not a start. The body scanner passes its inline-code / heading /
/// fence zones; frontmatter value scanning passes no exclusions, so any
/// embedded `[[...]]` in a string value is found the same way.
pub fn scan_wikilink_spans<'a>(
    src: &'a str,
    exclusions: &[ByteRange],
) -> Vec<(&'a str, ByteRange)> {
    // INVARIANT: `exclusions` are sorted by start and non-overlapping. The body
    // scanner (`scan_body`) builds them that way — a heading line is pure prose
    // (no inline code within), a fenced block absorbs its content, and
    // inline-code zones are sequential, see [[type-instance body contribution::au-type-system]].
    // That lets a forward-only zone index replace a per-byte scan over every
    // zone (which was O(n*m) on a zone-dense body), with no O(n) allocation.
    // If you change how exclusion zones are built, KEEP THEM sorted and
    // disjoint or this silently mis-handles exclusions. The test
    // `body_scanner_exclusions_are_sorted_and_disjoint` guards the producer;
    // this asserts the contract here in debug builds.
    debug_assert!(
        exclusions.windows(2).all(|w| w[0].end <= w[1].start),
        "exclusion zones must be sorted and non-overlapping (see scan_body)"
    );

    let mut out = Vec::new();
    let bytes = src.as_bytes();
    // `zi` is the first zone whose end is past the outer cursor. The cursor only
    // advances, so `zi` only advances. The inner `]]`-search below uses a LOCAL
    // copy `si` (also forward-only over its own rising `search`), so the two
    // cursors never share one index.
    let mut zi = 0usize;
    let mut cursor = 0;
    while cursor + 1 < bytes.len() {
        if zone_end_at(exclusions, &mut zi, cursor).is_some() {
            cursor += 1;
            continue;
        }
        if bytes[cursor] != b'[' || bytes[cursor + 1] != b'[' {
            cursor += 1;
            continue;
        }
        if cursor > 0 && bytes[cursor - 1] == b'\\' {
            cursor += 2;
            continue;
        }
        let start = cursor;
        let content_start = cursor + 2;
        let mut search = content_start;
        let mut content_end: Option<usize> = None;
        // When the inner search enters an exclusion zone (inline-code
        // span, heading, fenced block) before finding `]]`, the
        // candidate isn't a wikilink — wikilinks can't straddle
        // exclusion zones. Record the zone end so the outer cursor
        // can jump past it instead of re-entering.
        let mut straddled_zone_end: Option<usize> = None;
        // Local zone index for the inner search, seeded from the outer `zi`
        // (zones before the cursor are irrelevant) and advanced over the rising
        // `search`. The inner search breaks at the first zone, so a non-overlap
        // means one covering zone, whose end is the jump target.
        let mut si = zi;
        while search + 1 < bytes.len() {
            if let Some(zone_end) = zone_end_at(exclusions, &mut si, search) {
                straddled_zone_end = Some(zone_end);
                break;
            }
            if bytes[search] == b']' && bytes[search + 1] == b']' {
                content_end = Some(search);
                break;
            }
            search += 1;
        }
        // Record the inner-scan distance (one accumulate per candidate, off the
        // per-byte path). The scanner is linear when this total stays O(body
        // length); a quadratic regression makes it O(length²), see
        // [`take_wikilink_scan_steps`].
        WIKILINK_SCAN_STEPS.with(|c| c.set(c.get() + (search - content_start)));
        match (content_end, straddled_zone_end) {
            (Some(end), _) => {
                let raw = &src[content_start..end];
                let close_end = end + 2;
                out.push((raw, ByteRange::new(start, close_end)));
                cursor = close_end;
            }
            (None, Some(zone_end)) => {
                // Skip past the zone (and past at least the `[[` we
                // tried, in case the zone is empty / coincident).
                cursor = zone_end.max(cursor + 2);
            }
            (None, None) => {
                // The inner search reached the end of the buffer without a
                // closing `]]` or a straddled zone, so no `]]` exists anywhere
                // in `[content_start, len)`. The outer cursor only advances, so
                // no later `[[` candidate can close either — stop scanning. This
                // is what keeps an adversarial body of unmatched `[` linear
                // instead of O(n²) (re-scanning the tail at every `[[`).
                break;
            }
        }
    }
    out
}

thread_local! {
    /// Accumulates the total bytes the wikilink inner `]]`-search scans across
    /// all candidates since the last [`take_wikilink_scan_steps`]. A
    /// perf-regression hook, zero cost on the hot path (one accumulate per
    /// candidate, not per byte).
    static WIKILINK_SCAN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Total bytes scanned by the wikilink inner `]]`-search since the last call,
/// resetting the counter.
///
/// A deterministic proxy for the scan's time complexity. A body of N unmatched
/// `[` must stay O(N): the scanner stops once the tail is proven to hold no
/// `]]`, rather than restarting a full-length inner scan at every `[[` (which
/// was O(N²)). A count that grows quadratically with body length signals that
/// regression.
pub fn take_wikilink_scan_steps() -> usize {
    WIKILINK_SCAN_STEPS.with(|c| c.replace(0))
}

fn scan_inline_code<'a>(line: &'a str, line_start: usize, out: &mut Vec<BodyEvent<'a>>) {
    let bytes = line.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'`' {
            cursor += 1;
            continue;
        }
        let open_start = cursor;
        let open_end = run_end(bytes, open_start);
        let run_len = open_end - open_start;
        let mut search = open_end;
        let mut matched_close: Option<usize> = None;
        while search < bytes.len() {
            if bytes[search] != b'`' {
                search += 1;
                continue;
            }
            let close_end = run_end(bytes, search);
            if close_end - search == run_len {
                matched_close = Some(close_end);
                break;
            }
            search = close_end;
        }
        match matched_close {
            Some(close_end) => {
                let content_start = open_end;
                let content_end = close_end - run_len;
                let content = &line[content_start..content_end];
                let span = ByteRange::new(line_start + open_start, line_start + close_end);
                out.push(BodyEvent::InlineCode { content, span });
                cursor = close_end;
            }
            None => {
                cursor = open_end;
            }
        }
    }
}

fn run_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && bytes[end] == b'`' {
        end += 1;
    }
    end
}

/// Block-id grammar per [[type block-id::au-type-system]]: `[A-Za-z0-9_-]+`. One rule
/// across both attachment surfaces — body `^id` markers (this module)
/// and inline-record `^:` keys (au-core's instance parse).
pub fn is_valid_block_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parse_block_id_marker(line: &str) -> Option<&str> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_end();
    let id = trimmed.strip_prefix('^')?;
    if is_valid_block_id(id) {
        Some(id)
    } else {
        None
    }
}

/// Split a trailing `^id` marker off a line ([[type block-id::au-type-system]]).
/// Returns the content before the marker plus `(id, byte offset of the
/// `^`)`. The marker must be the line's last whitespace-separated token
/// — `word^id` glued to text is prose, not a marker. Empty content is
/// the own-line form.
fn split_trailing_block_id(line: &str) -> (&str, Option<(&str, usize)>) {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_end();
    let token_start = match trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
    {
        Some((i, c)) => i + c.len_utf8(),
        None => 0,
    };
    let token = &trimmed[token_start..];
    let Some(id) = token.strip_prefix('^') else {
        return (line, None);
    };
    if !is_valid_block_id(id) {
        return (line, None);
    }
    (&trimmed[..token_start], Some((id, token_start)))
}

/// An opening code fence, per CommonMark's variable-length rule. Returns
/// the info string plus the backtick-run length, so the matching close can
/// require at least that many backticks. A run shorter than three is not a
/// fence. A backtick fence's info string may not contain a backtick — that
/// rule is what makes a bare ```` ```` ```` a length-4 open with empty info,
/// not a length-3 open whose info is a stray backtick.
fn parse_fence_open(line: &str) -> Option<(&str, usize)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let run_len = line.bytes().take_while(|&b| b == b'`').count();
    if run_len < 3 {
        return None;
    }
    let info = line[run_len..].trim();
    if info.contains('`') {
        return None;
    }
    Some((info, run_len))
}

/// CommonMark ATX headings are `#` to `######` followed by a space.
/// More than six leading `#` is not a heading at all.
const ATX_MAX_LEVEL: u8 = 6;

fn parse_atx_heading(line: &str) -> Option<(u8, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let bytes = line.as_bytes();
    // Count leading `#` up to ATX_MAX_LEVEL + 1 — one more than the
    // valid maximum, so we can detect "too many hashes" by checking
    // for an extra `#` past the legal range.
    let mut hashes = 0u8;
    while hashes <= ATX_MAX_LEVEL && bytes.get(hashes as usize) == Some(&b'#') {
        hashes += 1;
    }
    if hashes == 0 || hashes > ATX_MAX_LEVEL {
        return None;
    }
    let after = &line[hashes as usize..];
    let stripped = after.strip_prefix(|c: char| c == ' ' || c == '\t')?;
    Some((hashes, stripped.trim()))
}

/// A closing code fence for an open of `min_len` backticks: a line of only
/// backticks (trailing whitespace allowed), at least `min_len` of them.
fn is_fence_close(line: &str, min_len: usize) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = line.trim_end();
    let run_len = trimmed.bytes().take_while(|&b| b == b'`').count();
    run_len >= min_len && run_len == trimmed.len()
}

fn iter_lines(src: &str) -> LineIter<'_> {
    LineIter { src, cursor: 0 }
}

struct LineIter<'a> {
    src: &'a str,
    cursor: usize,
}

impl<'a> Iterator for LineIter<'a> {
    type Item = (&'a str, ByteRange);

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.src.len() {
            return None;
        }
        let bytes = self.src.as_bytes();
        let start = self.cursor;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let span_end = if end < bytes.len() { end + 1 } else { end };
        self.cursor = span_end;
        Some((&self.src[start..end], ByteRange::new(start, span_end)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headings(src: &str) -> Vec<(u8, &str, ByteRange)> {
        scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::Heading { level, text, span } => Some((level, text, span)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_input_yields_no_events() {
        assert!(scan_body("").is_empty());
    }

    #[test]
    fn scan_wikilink_spans_finds_embedded_links_without_exclusions() {
        let src = "Runs the show at [[volvelle labs]] and [[other]].";
        let spans = scan_wikilink_spans(src, &[]);
        let raws: Vec<&str> = spans.iter().map(|(raw, _)| *raw).collect();
        assert_eq!(raws, vec!["volvelle labs", "other"]);
        // span covers the brackets and slices back to the link.
        let (raw0, span0) = spans[0];
        assert_eq!(&src[span0.start..span0.end], "[[volvelle labs]]");
        assert_eq!(raw0, "volvelle labs");
    }

    #[test]
    fn scan_wikilink_spans_skips_escaped_open() {
        let spans = scan_wikilink_spans("literal \\[[not a link]] here", &[]);
        assert!(spans.is_empty());
    }

    #[test]
    fn scan_wikilink_spans_is_linear_on_unmatched_open_brackets() {
        // An adversarial body of unmatched `[` must not restart a full-length
        // inner `]]`-search at every `[[`. Once the tail is proven `]]`-free the
        // scan stops, so the total inner-scan work stays linear in body length
        // (was O(n²), the DoS cliff). The step counter is the deterministic
        // proxy: ~n with the fix, ~n²/4 without it.
        let n = 50_000;
        let src = "[".repeat(n);
        let _ = take_wikilink_scan_steps(); // zero any prior accumulation
        let spans = scan_wikilink_spans(&src, &[]);
        assert!(spans.is_empty(), "no closing `]]`, so no wikilinks");
        let steps = take_wikilink_scan_steps();
        assert!(
            steps <= 8 * n,
            "wikilink scan went quadratic: {steps} inner-scan steps for a \
             {n}-byte body of `[` (linear bound {})",
            8 * n
        );
    }

    #[test]
    fn scan_wikilink_spans_finds_a_link_before_unmatched_open_brackets() {
        // The linear-scan stop only triggers when the remaining buffer holds no
        // `]]`, so a real link before an unmatched run is still captured — the
        // output is byte-identical to the pre-optimization scan.
        let src = "[[real]] then [[[[";
        let spans = scan_wikilink_spans(src, &[]);
        let raws: Vec<&str> = spans.iter().map(|(raw, _)| *raw).collect();
        assert_eq!(raws, vec!["real"]);
    }

    #[test]
    fn body_event_carries_byte_range() {
        let event = BodyEvent::Heading {
            level: 1,
            text: "Why",
            span: ByteRange::new(0, 5),
        };
        match event {
            BodyEvent::Heading { span, .. } => {
                assert_eq!(span.start, 0);
                assert_eq!(span.end, 5);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parses_atx_heading_levels_1_to_6() {
        let src = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n";
        let hs = headings(src);
        assert_eq!(hs.len(), 6);
        for (i, (level, text, _)) in hs.iter().enumerate() {
            assert_eq!(*level as usize, i + 1);
            assert_eq!(*text, format!("H{}", i + 1));
        }
    }

    #[test]
    fn rejects_seven_hashes() {
        let src = "####### too many\n";
        assert!(headings(src).is_empty());
    }

    #[test]
    fn requires_space_after_hashes() {
        let src = "#NoSpace\n";
        assert!(headings(src).is_empty());
    }

    #[test]
    fn trims_text_whitespace() {
        let src = "##    spaced text   \n";
        let hs = headings(src);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].1, "spaced text");
    }

    #[test]
    fn span_covers_full_line_including_newline() {
        let src = "# Why\n# Then\n";
        let hs = headings(src);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].2, ByteRange::new(0, 6));
        assert_eq!(hs[1].2, ByteRange::new(6, 13));
    }

    #[test]
    fn ignores_headings_inside_fenced_blocks() {
        let src = "# Real\n```\n# fake-heading-in-fence\n```\n# After\n";
        let hs = headings(src);
        let texts: Vec<_> = hs.iter().map(|h| h.1).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let src = "# Why\r\n## Then\r\n";
        let hs = headings(src);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].1, "Why");
        assert_eq!(hs[1].1, "Then");
    }

    #[test]
    fn handles_final_line_without_newline() {
        let src = "# Only";
        let hs = headings(src);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].1, "Only");
        assert_eq!(hs[0].2, ByteRange::new(0, 6));
    }

    #[test]
    fn does_not_match_setext_underline() {
        // Setext-style is intentionally unsupported; the line of `=` is not a heading.
        let src = "Why\n===\n";
        assert!(headings(src).is_empty());
    }

    fn fences(src: &str) -> Vec<(&str, &str, ByteRange, Option<&str>)> {
        scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::FencedBlock {
                    info,
                    body,
                    span,
                    trailing_block_id,
                } => Some((info, body, span, trailing_block_id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_fenced_block_with_info() {
        let src = "```yaml [:field]\nkey: value\n```\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].0, "yaml [:field]");
        assert_eq!(fs[0].1, "key: value\n");
        assert_eq!(fs[0].3, None);
    }

    #[test]
    fn parses_fenced_block_with_trailing_block_id() {
        let src = "```yaml [:assumptions]\ntype: assumption\n```\n^pdf-pagination\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].3, Some("pdf-pagination"));
    }

    #[test]
    fn parses_fenced_block_without_info() {
        let src = "```\nplain content\n```\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].0, "");
    }

    #[test]
    fn variable_length_fence_wraps_shorter_fence() {
        // A quad-backtick fence contains a triple-backtick example; the
        // inner triple close must NOT end the outer block.
        let src = "````markdown\n```yaml\nkey: value\n```\n````\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1, "one block, not a mispaired pair: {fs:?}");
        assert_eq!(fs[0].0, "markdown");
        assert_eq!(fs[0].1, "```yaml\nkey: value\n```\n");
        // The real quad-close leaks no unterminated marker.
        let events = scan_body(src);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, BodyEvent::UnterminatedFenceOpen { .. })),
            "no unterminated marker: {events:?}"
        );
    }

    #[test]
    fn shorter_fence_inside_longer_is_absorbed() {
        // A bare triple line inside a quad fence is content, not a close.
        let src = "````\ncontent\n```\n````\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1, "{fs:?}");
        assert_eq!(fs[0].1, "content\n```\n");
    }

    #[test]
    fn unterminated_quad_fence_still_marks() {
        // A genuinely unterminated fence of any length still recovers.
        let src = "````\nstuff\n";
        assert!(fences(src).is_empty());
        let unterm = scan_body(src)
            .iter()
            .filter(|e| matches!(e, BodyEvent::UnterminatedFenceOpen { .. }))
            .count();
        assert_eq!(unterm, 1, "quad-open is unterminated");
    }

    #[test]
    fn double_tick_inline_code_is_one_span_not_a_marker() {
        // The docs-about-syntax convention: a marker shown as an example is
        // double-ticked, so its inline-code content starts with a backtick
        // and dodges the `[:` contribution/malformed check downstream. The
        // scanner matches backtick runs by length, so this is ONE span.
        let src = "An example: `` `[:field]` `` here.\n";
        let codes: Vec<&str> = scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::InlineCode { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(codes.len(), 1, "one span, not three: {codes:?}");
        assert!(
            !codes[0].starts_with("[:"),
            "content must not trip the `[:` marker check: {:?}",
            codes[0]
        );
    }

    #[test]
    fn unterminated_fence_emits_marker_not_block() {
        let src = "```yaml\nkey: value\n";
        // No matched FencedBlock.
        assert!(fences(src).is_empty());
        // But an UnterminatedFenceOpen DOES appear, with the info
        // string from the open line. Span covers the fence-open line
        // only.
        let events = scan_body(src);
        let unterm: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BodyEvent::UnterminatedFenceOpen { info, span } => Some((*info, *span)),
                _ => None,
            })
            .collect();
        assert_eq!(unterm.len(), 1);
        assert_eq!(unterm[0].0, "yaml");
        // Span ends at byte 8 (the newline after ```yaml is included).
        assert_eq!(unterm[0].1.end, 8);
    }

    #[test]
    fn unterminated_fence_does_not_swallow_later_events() {
        // The fix for the parser-recovery bug: subsequent headings,
        // wikilinks, inline-code, and block-id markers continue to
        // emit even after an unterminated fence-open.
        let src = "```yaml\n# Why\nThis is `[:rationale] X` text.\n[[note:assumption]]\n^marker\n";
        let events = scan_body(src);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                BodyEvent::Heading { .. } => "heading",
                BodyEvent::FencedBlock { .. } => "fenced",
                BodyEvent::InlineCode { .. } => "inline-code",
                BodyEvent::Wikilink { .. } => "wikilink",
                BodyEvent::BlockIdMarker { .. } => "block-id-marker",
                BodyEvent::UnterminatedFenceOpen { .. } => "unterm",
            })
            .collect();
        assert!(kinds.contains(&"unterm"), "unterminated marker: {kinds:?}");
        assert!(
            kinds.contains(&"heading"),
            "heading must still emit: {kinds:?}"
        );
        assert!(
            kinds.contains(&"inline-code"),
            "inline-code must still emit: {kinds:?}"
        );
        assert!(
            kinds.contains(&"wikilink"),
            "wikilink must still emit: {kinds:?}"
        );
        assert!(
            kinds.contains(&"block-id-marker"),
            "block-id-marker must still emit: {kinds:?}"
        );
    }

    #[test]
    fn tilde_fences_are_not_recognized() {
        let src = "~~~yaml\nkey: value\n~~~\n";
        assert!(fences(src).is_empty());
    }

    #[test]
    fn fence_close_with_info_is_treated_as_body() {
        // Per CommonMark, closing fence must have no info string.
        let src = "```yaml\n```bogus\nstill in body\n```\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1);
        assert!(fs[0].1.contains("```bogus"));
        assert!(fs[0].1.contains("still in body"));
    }

    #[test]
    fn standalone_block_id_marker_emits_event() {
        let src = "Some paragraph.\n^paragraph-id\n";
        let events = scan_body(src);
        let markers: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BodyEvent::BlockIdMarker { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec!["paragraph-id"]);
    }

    #[test]
    fn fence_consumes_its_trailing_marker_no_duplicate_event() {
        let src = "```yaml [:f]\nx: 1\n```\n^used\n";
        let events = scan_body(src);
        // FencedBlock captures the trailing block-id; no standalone BlockIdMarker emitted for it.
        let marker_count = events
            .iter()
            .filter(|e| matches!(e, BodyEvent::BlockIdMarker { .. }))
            .count();
        assert_eq!(marker_count, 0);
    }

    #[test]
    fn block_id_rejects_invalid_chars() {
        let src = "^bad id with space\n^bad/slash\n";
        let events = scan_body(src);
        let marker_count = events
            .iter()
            .filter(|e| matches!(e, BodyEvent::BlockIdMarker { .. }))
            .count();
        assert_eq!(marker_count, 0);
    }

    #[test]
    fn trailing_block_id_on_a_paragraph_line() {
        // The same-line form per [[type block-id::au-type-system]] — the marker is the
        // line's last whitespace-separated token, span covers the token.
        let src = "A paragraph, addressable. ^para-1\n";
        let events = scan_body(src);
        let (id, span) = events
            .iter()
            .find_map(|e| match e {
                BodyEvent::BlockIdMarker { id, span } => Some((*id, *span)),
                _ => None,
            })
            .expect("trailing marker emitted");
        assert_eq!(id, "para-1");
        assert_eq!(&src[span.start..span.end], "^para-1");
    }

    #[test]
    fn trailing_block_id_on_a_heading_keeps_the_text_clean() {
        // Section matching must see the heading text without its marker.
        let src = "# My Section ^sec-1\n";
        let events = scan_body(src);
        let text = events
            .iter()
            .find_map(|e| match e {
                BodyEvent::Heading { text, .. } => Some(*text),
                _ => None,
            })
            .expect("heading emitted");
        assert_eq!(text, "My Section");
        let id = events
            .iter()
            .find_map(|e| match e {
                BodyEvent::BlockIdMarker { id, .. } => Some(*id),
                _ => None,
            })
            .expect("marker emitted");
        assert_eq!(id, "sec-1");
    }

    #[test]
    fn glued_caret_token_is_not_a_marker() {
        // `word^id` inside prose has no whitespace before the caret.
        let src = "see foo^bar in prose\n";
        let events = scan_body(src);
        assert!(!events
            .iter()
            .any(|e| matches!(e, BodyEvent::BlockIdMarker { .. })));
    }

    #[test]
    fn mid_line_caret_token_is_not_a_marker() {
        // Only the trailing token attaches; a caret token mid-line is prose.
        let src = "a ^id1 then more words\n";
        let events = scan_body(src);
        assert!(!events
            .iter()
            .any(|e| matches!(e, BodyEvent::BlockIdMarker { .. })));
    }

    #[test]
    fn fence_span_includes_open_close_and_trailing_marker() {
        let src = "```\nx\n```\n^id\n";
        let fs = fences(src);
        assert_eq!(fs.len(), 1);
        let (_, _, span, id) = &fs[0];
        assert_eq!(*id, Some("id"));
        assert_eq!(span.start, 0);
        assert_eq!(span.end, src.len());
    }

    fn inline_codes(src: &str) -> Vec<(&str, ByteRange)> {
        scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::InlineCode { content, span } => Some((content, span)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_single_backtick_span() {
        let src = "before `code` after\n";
        let codes = inline_codes(src);
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].0, "code");
        // span covers the backticks too
        assert_eq!(codes[0].1, ByteRange::new(7, 13));
    }

    #[test]
    fn parses_multi_backtick_span() {
        let src = "before ``co`de`` after\n";
        let codes = inline_codes(src);
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].0, "co`de");
    }

    #[test]
    fn parses_multiple_inline_codes_on_one_line() {
        let src = "a `one` b `two` c\n";
        let codes = inline_codes(src);
        assert_eq!(
            codes.iter().map(|c| c.0).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn unterminated_inline_code_is_dropped() {
        let src = "a `code without close\n";
        let codes = inline_codes(src);
        assert!(codes.is_empty());
    }

    #[test]
    fn inline_code_not_emitted_inside_fence() {
        let src = "```\n`not-a-code-span` because we're inside\n```\n";
        let codes = inline_codes(src);
        assert!(codes.is_empty());
    }

    #[test]
    fn inline_code_does_not_match_across_lines() {
        let src = "a `unmatched\nnext line ` other\n";
        let codes = inline_codes(src);
        assert!(codes.is_empty());
    }

    #[test]
    fn matches_attribution_marker_shape() {
        let src = "Decided on `[:decided_at] 2026-04-15` after review.\n";
        let codes = inline_codes(src);
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].0, "[:decided_at] 2026-04-15");
    }

    fn wikilinks(src: &str) -> Vec<(&str, ByteRange)> {
        scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::Wikilink { raw, span } => Some((raw, span)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn body_scanner_exclusions_are_sorted_and_disjoint() {
        // `scan_wikilink_spans` walks exclusion zones with a forward-only index,
        // which is correct only if the zones are sorted and non-overlapping.
        // This pins the producer: a heading line is pure prose (its backticks
        // are NOT an inline-code zone), a fence absorbs its content, inline-code
        // zones are sequential. A change that scanned inline code inside a
        // heading would nest a zone in the heading zone and break this.
        let src = "# A `not code` heading\n\ntext `code1` and `code2` here\n\n```rust\nfn x() {}\n```\n\n## Another: heading\n";
        let zones: Vec<ByteRange> = scan_body(src)
            .iter()
            .filter_map(|e| match e {
                BodyEvent::Heading { span, .. }
                | BodyEvent::FencedBlock { span, .. }
                | BodyEvent::InlineCode { span, .. } => Some(*span),
                _ => None,
            })
            .collect();
        assert!(
            zones.windows(2).all(|w| w[0].start <= w[1].start),
            "exclusion zones must be sorted by start: {zones:?}"
        );
        assert!(
            zones.windows(2).all(|w| w[0].end <= w[1].start),
            "exclusion zones must be non-overlapping: {zones:?}"
        );
        // The heading's backticks did not produce an inline-code zone.
        let inline = scan_body(src)
            .into_iter()
            .filter_map(|e| match e {
                BodyEvent::InlineCode { content, .. } => Some(content),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            inline,
            vec!["code1", "code2"],
            "heading backticks must not be inline code"
        );
    }

    #[test]
    fn parses_wikilink_in_prose() {
        let src = "See [[my-rationale]] for context.\n";
        let ws = wikilinks(src);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].0, "my-rationale");
    }

    #[test]
    fn parses_wikilink_with_field_fragment_raw() {
        let src = "Cite [[paper-a:sources]] here.\n";
        let ws = wikilinks(src);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].0, "paper-a:sources");
    }

    #[test]
    fn parses_multiple_wikilinks_on_one_line() {
        let src = "Use [[a:f]] and [[b:f]] together.\n";
        let ws = wikilinks(src);
        assert_eq!(
            ws.iter().map(|w| w.0).collect::<Vec<_>>(),
            vec!["a:f", "b:f"]
        );
    }

    #[test]
    fn does_not_emit_wikilink_inside_fence() {
        let src = "```\n[[inside-fence:f]]\n```\n";
        let ws = wikilinks(src);
        assert!(ws.is_empty());
    }

    #[test]
    fn does_not_emit_wikilink_inside_inline_code() {
        let src = "Sample: `[[inside-code:f]]` only.\n";
        let ws = wikilinks(src);
        assert!(ws.is_empty());
    }

    #[test]
    fn wikilink_straddling_inline_code_is_not_emitted() {
        // `[[ ... ]]` whose interior spans across an inline-code span
        // is NOT a wikilink — the backticks are exclusion zones, and a
        // wikilink can't span across them. Pre-fix the parser produced
        // a Wikilink whose raw text included the literal backticks.
        let src = "Bad: [[foo `code` bar]] here.\n";
        let ws = wikilinks(src);
        assert!(
            ws.is_empty(),
            "wikilink straddling backtick span must not be emitted; got: {ws:?}"
        );
    }

    #[test]
    fn wikilink_after_straddled_candidate_still_parses() {
        // The cursor should advance past the exclusion zone, not
        // re-enter it, so a subsequent valid wikilink later in the
        // source still emits.
        let src = "Bad: [[foo `code` bar]] and good: [[real-target]].\n";
        let ws = wikilinks(src);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].0, "real-target");
    }

    #[test]
    fn escaped_wikilink_is_skipped() {
        let src = "Literal: \\[[not-a-wikilink]] here.\n";
        let ws = wikilinks(src);
        assert!(ws.is_empty());
    }

    #[test]
    fn wikilink_can_straddle_line_breaks() {
        let src = "Spread [[multi\nline-target]] across.\n";
        let ws = wikilinks(src);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].0, "multi\nline-target");
    }

    #[test]
    fn unterminated_wikilink_is_dropped() {
        let src = "Open [[but-never-closed in prose.\n";
        let ws = wikilinks(src);
        assert!(ws.is_empty());
    }

    #[test]
    fn events_sort_in_source_order_across_kinds() {
        let src = "# Why\n\nCite [[paper]] and `[:rationale] x` here.\n";
        let events = scan_body(src);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                BodyEvent::Heading { .. } => "heading",
                BodyEvent::Wikilink { .. } => "wikilink",
                BodyEvent::InlineCode { .. } => "code",
                BodyEvent::FencedBlock { .. } => "fence",
                BodyEvent::BlockIdMarker { .. } => "marker",
                BodyEvent::UnterminatedFenceOpen { .. } => "unterm",
            })
            .collect();
        assert_eq!(kinds, vec!["heading", "wikilink", "code"]);
    }

    fn paths_of(src: &str) -> Vec<Vec<String>> {
        let events = scan_body(src);
        derive_section_paths(&events)
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }

    #[test]
    fn empty_events_yields_empty_paths() {
        assert!(derive_section_paths(&[]).is_empty());
    }

    #[test]
    fn body_preamble_event_has_empty_path() {
        let src = "Cite [[paper]] before any heading.\n";
        let paths = paths_of(src);
        assert_eq!(paths, vec![Vec::<String>::new()]);
    }

    #[test]
    fn heading_path_includes_itself() {
        let src = "# Why\n";
        let paths = paths_of(src);
        assert_eq!(paths, vec![vec!["1 Why".to_string()]]);
    }

    #[test]
    fn sibling_index_increments_under_same_parent() {
        let src = "# A\n## A.1\n## A.2\n";
        let paths = paths_of(src);
        assert_eq!(
            paths,
            vec![
                vec!["1 A".to_string()],
                vec!["1 A".to_string(), "1 A.1".to_string()],
                vec!["1 A".to_string(), "2 A.2".to_string()],
            ]
        );
    }

    #[test]
    fn sibling_counter_restarts_under_new_parent() {
        let src = "# A\n## child\n# B\n## child\n";
        let paths = paths_of(src);
        assert_eq!(
            paths,
            vec![
                vec!["1 A".to_string()],
                vec!["1 A".to_string(), "1 child".to_string()],
                vec!["2 B".to_string()],
                vec!["2 B".to_string(), "1 child".to_string()],
            ]
        );
    }

    #[test]
    fn deep_nesting_and_pop_back_to_root() {
        let src = "# A\n## B\n### C\n# D\n";
        let paths = paths_of(src);
        assert_eq!(
            paths,
            vec![
                vec!["1 A".to_string()],
                vec!["1 A".to_string(), "1 B".to_string()],
                vec!["1 A".to_string(), "1 B".to_string(), "1 C".to_string()],
                vec!["2 D".to_string()],
            ]
        );
    }

    #[test]
    fn contributions_inherit_enclosing_heading_path() {
        let src = "# Why\nCite [[a]] in prose.\n## Detail\nUse `[:f] v` here.\n";
        let events = scan_body(src);
        let paired = derive_section_paths(&events);
        // Heading "Why", Wikilink under Why, Heading "Detail", InlineCode under Detail.
        let kinds: Vec<&str> = paired
            .iter()
            .map(|(_, e)| match e {
                BodyEvent::Heading { .. } => "heading",
                BodyEvent::Wikilink { .. } => "wikilink",
                BodyEvent::InlineCode { .. } => "code",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["heading", "wikilink", "heading", "code"]);
        assert_eq!(paired[1].0, vec!["1 Why".to_string()]);
        assert_eq!(
            paired[3].0,
            vec!["1 Why".to_string(), "1 Detail".to_string()]
        );
    }
}
