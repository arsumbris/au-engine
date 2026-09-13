//! Diagnostics the orchestration emits directly, plus the canonical sort.
//!
//! These wrap I/O and parser failures the build hits while walking and reading
//! the knowledge base. The pure crates emit their own diagnostics; these cover the
//! seams between them, file reads, encoding, and the duplicate-key surface.

use std::path::Path;

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};
use au_parser::{
    scan_duplicate_keys, span_to_byte_range, DuplicateKey, WalkError, AUIGNORE_EMPTY_SCOPE,
    AUIGNORE_LOAD_ERROR, DUPLICATE_KEY_IN_MAPPING, FILE_TOO_LARGE, REPO_FILE_NOT_UTF8,
    REPO_FILE_READ_ERROR, REPO_WALK_ERROR,
};

/// Total diagnostic order: by file, then start offset, then code, then a
/// content tiebreaker. Every build sorts its merged stream through this so the
/// output never depends on the order the passes ran in.
///
/// The content tiebreaker is load-bearing for incremental recompute, not
/// cosmetic. Two diagnostics can tie on `(file, start, code)`, e.g. two
/// `peer-unmounted` on one registry at span `(file, 0..0)`. A stable sort would
/// then keep them in insertion order, which is pass order. Incremental
/// recompute re-merges diagnostics in held-bucket order, not pass order, so an
/// insertion-order-dependent tie would diverge from a full build and break the
/// identical-rebuild contract. Resolving ties by content makes the order
/// independent of how the stream was assembled. The tiebreaker is computed only
/// on a tie, so the common path is unchanged.
pub fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        a.span
            .file
            .cmp(&b.span.file)
            .then(a.span.range.start.cmp(&b.span.range.start))
            .then(a.code.as_str().cmp(b.code.as_str()))
            .then_with(|| format!("{a:?}").cmp(&format!("{b:?}")))
    });
}

/// Diagnostic for a `walk_files` entry whose `read_file` then failed.
pub fn read_error_diag(file: &Path, err: &std::io::Error) -> Diagnostic {
    Diagnostic {
        code: REPO_FILE_READ_ERROR,
        severity: Severity::Error,
        span: Span::for_file(file.to_path_buf()),
        message: format!("cannot read repo file: {err}"),
        related: vec![],
        fix: None,
    }
}

/// Diagnostic for a file the parser would read but that exceeds the read cap,
/// so it is skipped rather than pulled whole into memory. The skip-at-read
/// sibling of [`read_error_diag`]: same `Error` severity, same "file is
/// skipped" outcome, catalogued as an unread entry so existence-based
/// references still resolve. The message names the size and the cap, and the
/// fix, so the author can act (split it, or exclude it via `.auignore`).
pub fn file_too_large_diag(file: &Path, size: u64, cap: u64) -> Diagnostic {
    Diagnostic {
        code: FILE_TOO_LARGE,
        severity: Severity::Error,
        span: Span::for_file(file.to_path_buf()),
        message: format!(
            "file is {size} bytes, over the {cap}-byte read cap; it is skipped and not analysed"
        ),
        related: vec![],
        fix: Some(au_diagnostics::SuggestedFix {
            description: "split the file, or exclude it from the graph via `.arsumbris/.auignore`"
                .to_string(),
        }),
    }
}

/// Diagnostic for a per-entry walk failure (unreadable subdir, broken
/// symlink, metadata or canonicalize error). The walk skipped the entry
/// and continued; this surfaces what was missed.
pub fn walk_error_diag(err: &WalkError) -> Diagnostic {
    Diagnostic {
        code: REPO_WALK_ERROR,
        severity: Severity::Error,
        span: Span::for_file(err.path.clone()),
        message: format!("cannot walk repo entry: {}", err.source),
        related: vec![],
        fix: None,
    }
}

/// Diagnostic for an `.arsumbris/.auignore` that exists but could not be
/// applied (read failed, or a pattern is malformed). The member falls back to
/// the default excludes; scoping degrades loudly, never silently.
pub fn auignore_load_error_diag(file: &Path, reason: &str) -> Diagnostic {
    Diagnostic {
        code: AUIGNORE_LOAD_ERROR,
        severity: Severity::Warning,
        span: Span::for_file(file.to_path_buf()),
        message: format!(
            "cannot apply `.auignore`, scoping falls back to the default excludes: {reason}"
        ),
        related: vec![],
        fix: None,
    }
}

/// Diagnostic for an `.arsumbris/.auignore` that applied cleanly but excluded
/// every file under its member, so the member contributes nothing. Almost
/// always an over-broad pattern; surfaced so an accidental empty scope is loud.
/// `root` is the member root; the span points at the `.auignore` itself.
pub fn auignore_empty_scope_diag(root: &Path) -> Diagnostic {
    Diagnostic {
        code: AUIGNORE_EMPTY_SCOPE,
        severity: Severity::Warning,
        span: Span::for_file(root.join(".arsumbris").join(".auignore")),
        message: format!(
            "`.auignore` excluded every file under {}; the member contributes nothing",
            root.display()
        ),
        related: vec![],
        fix: None,
    }
}

/// Diagnostic for a file whose bytes aren't valid UTF-8. The span points at
/// the first invalid byte so the user knows where the encoding broke.
pub fn not_utf8_diag(file: &Path, err: &std::string::FromUtf8Error) -> Diagnostic {
    let bad_byte = err.utf8_error().valid_up_to();
    Diagnostic {
        code: REPO_FILE_NOT_UTF8,
        severity: Severity::Error,
        span: Span::new(file.to_path_buf(), ByteRange::new(bad_byte, bad_byte)),
        message: format!("file is not valid UTF-8 (first invalid byte at offset {bad_byte})"),
        related: vec![],
        fix: None,
    }
}

/// Build diagnostics for every duplicate key found in `yaml_text`. saphyr's
/// `LinkedHashMap` silently keeps the last value and drops earlier ones; this
/// surface lets the user see the silent drop. `yaml_offset` lifts saphyr's
/// YAML-relative spans into file-relative byte ranges.
pub fn duplicate_key_diags(
    file: &Path,
    source: &str,
    yaml_text: &str,
    yaml_offset: usize,
) -> Vec<Diagnostic> {
    scan_duplicate_keys(yaml_text)
        .iter()
        .map(|d| duplicate_key_diag(file, source, yaml_offset, d))
        .collect()
}

fn duplicate_key_diag(
    file: &Path,
    source: &str,
    yaml_offset: usize,
    dup: &DuplicateKey,
) -> Diagnostic {
    let primary = span_to_byte_range(source, yaml_offset, dup.duplicate_span);
    let related = span_to_byte_range(source, yaml_offset, dup.first_span);
    Diagnostic {
        code: DUPLICATE_KEY_IN_MAPPING,
        // Advisory: the mapping still parses (saphyr keeps the last value), so a
        // duplicate key surfaces the silent drop without aborting the file or,
        // for a type-def, its repo graph (see `build.rs` parse-error gate).
        severity: Severity::Warning,
        span: Span::new(file.to_path_buf(), primary),
        message: format!(
            "duplicate key '{}' in YAML mapping; saphyr keeps only the last occurrence and silently drops earlier values",
            dup.key
        ),
        related: vec![Span::new(file.to_path_buf(), related)],
        fix: None,
    }
}
