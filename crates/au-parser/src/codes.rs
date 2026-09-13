//! Diagnostic codes emitted by `au-parser`. Stable kebab-case strings;
//! downstream consumers match on these.

use au_diagnostics::DiagnosticCode;

/// Frontmatter (or pure-YAML type-def) failed to parse as YAML.
pub const YAML_PARSE_ERROR: DiagnosticCode = DiagnosticCode::from_static("yaml-parse-error");

/// File opened with `---` but no closing `---` line was found before EOF.
/// Frontmatter is silently dropped if not terminated, so this surfaces the
/// failure mode loudly.
pub const FRONTMATTER_UNTERMINATED: DiagnosticCode =
    DiagnosticCode::from_static("frontmatter-unterminated");

/// Repo walker listed a file but `read_file` failed (permission denied,
/// mid-walk deletion, I/O error). The file is skipped, but the failure is
/// surfaced so the user knows their knowledge base is partially inaccessible.
pub const REPO_FILE_READ_ERROR: DiagnosticCode =
    DiagnosticCode::from_static("repo-file-read-error");

/// File bytes are not valid UTF-8. Skipped rather than lossily decoded —
/// silent replacement would skew byte offsets in every subsequent diagnostic
/// against the file.
///
/// Fires only for files the parser tries to read: type-defs (`*.type.yaml` /
/// under `type/`) and instance candidates (`*.md` / `*.yaml` / `*.yml`
/// elsewhere). Asset binaries (PDFs, images, archives) live in `RepoIndex`
/// for `file*` resolution but the parser never decodes their bytes, so they
/// don't trigger this code.
pub const REPO_FILE_NOT_UTF8: DiagnosticCode = DiagnosticCode::from_static("repo-file-not-utf8");

/// A file the parser would read is larger than the engine's read cap, so it is
/// skipped rather than pulled whole into memory. Catalogued by path and kind
/// (like an unread asset) so existence-based `file*` and navigational
/// references still resolve, but it carries no parse, no type claim, and no
/// validation. Surfaces a self-inflicted footgun (an oversized note or export
/// dropped into the vault) loudly and actionably rather than as an OOM. Fires
/// for the same read set as `repo-file-not-utf8`: type-defs and instance
/// candidates; an asset is never read, so its size is never capped.
pub const FILE_TOO_LARGE: DiagnosticCode = DiagnosticCode::from_static("file-too-large");

/// Repo walk hit a per-entry failure (unreadable subdirectory, broken
/// symlink, metadata error, canonicalize error). The walk skips the entry
/// and continues so a single bad path doesn't void the whole validation.
pub const REPO_WALK_ERROR: DiagnosticCode = DiagnosticCode::from_static("repo-walk-error");

/// A member's `.arsumbris/.auignore` exists but could not be applied (the read
/// failed, or a pattern is malformed). The member falls back to the default
/// excludes, as if the file were absent, so scoping degrades loudly rather
/// than silently dropping or over-including files.
pub const AUIGNORE_LOAD_ERROR: DiagnosticCode = DiagnosticCode::from_static("auignore-load-error");

/// A member's `.arsumbris/.auignore` applied cleanly but excluded every file
/// under the member, so it contributes nothing to the graph. Almost always an
/// over-broad pattern (`*`, `**`, a stray `/`); surfaced so an accidental
/// empty scope is loud rather than a silently missing subtree.
pub const AUIGNORE_EMPTY_SCOPE: DiagnosticCode =
    DiagnosticCode::from_static("auignore-empty-scope");

/// Same key appeared twice (or more) inside one YAML mapping. saphyr's
/// `LinkedHashMap` silently keeps the last occurrence and drops earlier
/// ones; without this diagnostic the user's earlier value vanishes with
/// no signal. Fires per second-and-later occurrence; `related` carries
/// the span of the first.
pub const DUPLICATE_KEY_IN_MAPPING: DiagnosticCode =
    DiagnosticCode::from_static("duplicate-key-in-mapping");
