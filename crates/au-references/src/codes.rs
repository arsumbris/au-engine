//! Diagnostic codes emitted by au-references. Stable kebab-case strings.

use au_diagnostics::DiagnosticCode;

/// Two basenames in the repo collide under case-insensitive comparison.
/// Legal on Linux, illegal on macOS-default APFS — references to either are
/// ambiguous on case-insensitive filesystems. Spec [[type reference::au-type-system]].
///
/// **Severity is `Warning`, not `Error`.** The spec lists the diagnostic but
/// doesn't pin a severity; we picked Warning because the collision is a
/// knowledge-base-quality / portability concern, not a violation of the type system.
/// On Linux the repo still validates and references resolve unambiguously
/// when fully spelled (e.g. `[[notes/Foo.md]]`); the warning surfaces as a
/// "this will break if you migrate to macOS" advisory. Promote to Error if
/// real workflows show users routinely missing it.
pub const CASE_COLLISION_BASENAME: DiagnosticCode =
    DiagnosticCode::from_static("case-collision-basename");

/// Wikilink target did not resolve to any repo file. A `warning`, not an
/// error: a dangling typed reference is open-world growth (the target may be
/// authored next), mirroring the prose sibling `navigational-target-not-found`.
/// Distinct from `reference-target-type-mismatch`, where the target exists but
/// is the wrong type — a real mistake that stays an error. A consumer wanting
/// strictness gates on this code.
pub const REFERENCE_TARGET_MISSING: DiagnosticCode =
    DiagnosticCode::from_static("reference-target-missing");

/// Wikilink target matched two or more repo files (basename ambiguity).
/// Spec [[type reference::au-type-system]]: resolution requires explicit path or rename.
pub const REFERENCE_TARGET_AMBIGUOUS: DiagnosticCode =
    DiagnosticCode::from_static("reference-target-ambiguous");

/// A wikilink target names a path that leaves its own repo — absolute, or
/// climbing above the root with `..`. Warning. A wikilink is repo-scoped, and
/// `::repo` is how an edge crosses a boundary, so the address contradicts its
/// own scope and no later authoring makes it resolve. Distinct from
/// `reference-target-missing` (which is open-world growth) precisely so an
/// impossible address does not sit in the same bucket as one that is merely
/// absent. Carries the `::repo` spelling as its fix. See [[type reference::au-type-system]].
pub const REFERENCE_PATH_ESCAPES_REPO: DiagnosticCode =
    DiagnosticCode::from_static("reference-path-escapes-repo");

/// A `[[name::repo]]` link names a repo that is not a declared peer of the
/// source's repo and not a workspace member — the qualifier resolves to no
/// known repo, likely a typo. Error.
pub const REFERENCE_REPO_UNKNOWN: DiagnosticCode =
    DiagnosticCode::from_static("reference-repo-unknown");

/// A `[[name::repo]]` link names a declared peer or workspace member that is
/// not present in the loaded workspace on this machine. A legitimate state, the
/// repo is known but unmounted, so the reference cannot resolve here. Warning,
/// mirroring `peer-unmounted`.
pub const REFERENCE_REPO_UNAVAILABLE: DiagnosticCode =
    DiagnosticCode::from_static("reference-repo-unavailable");

// Note: `reference-target-type-mismatch` lives in `au-core::codes`. This crate
// owns resolution-side codes (target-missing, target-ambiguous, basename
// collisions); the type-closure check is au-core's concern, so its code lives
// where it's produced.

// ----- wikilink parse codes -----
//
// The canonical wikilink form is `[[target[#anchor][^block_id]]]` — `#`
// always precedes `^` when both appear. Malformed forms produce specific
// codes so the user can see exactly what's wrong instead of a generic
// "value is not a wikilink".

/// Wikilink has the `[[ … ]]` frame but the target portion is empty
/// without a locating fragment — `[[:field]]`. `#head` or `^block-id`
/// grant the empty name: `[[#head]]` / `[[^id]]` are the legal local
/// forms (current file) per [[type reference::au-type-system]]; a field alone has nothing
/// to contribute without a target.
pub const WIKILINK_EMPTY_TARGET: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-empty-target");

/// Wikilink ends with `#` and no anchor value — `[[note#]]`. Likely an
/// in-progress edit; reject so a downstream resolver doesn't have to guess.
pub const WIKILINK_EMPTY_ANCHOR: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-empty-anchor");

/// Wikilink ends with `^` and no block-id value — `[[note^]]`, or a doubled
/// caret with no id — `[[note^^]]`. Same rationale as `wikilink-empty-anchor`.
pub const WIKILINK_EMPTY_BLOCK_ID: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-empty-block-id");

/// Wikilink has `^` before `#` — `[[note^123#section]]`. Canonical order
/// is `target[#anchor][^block_id]`. Reverse order is rejected rather than
/// silently parsed because the value of either suffix would be ambiguous.
pub const WIKILINK_REVERSED_DELIMITERS: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-reversed-delimiters");

/// Wikilink ends with `:` and no field value — `[[note:]]`. Same
/// rationale as `wikilink-empty-anchor`.
pub const WIKILINK_EMPTY_FIELD: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-empty-field");

/// Wikilink fragments out of strict `name / #head / ^block-id / :field`
/// order — e.g. `[[note:f#h]]` (field before anchor). Spec [[type reference::au-type-system]].
pub const WIKILINK_FRAGMENT_ORDER: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-fragment-order");

/// Wikilink `:field` fragment whose value doesn't satisfy the field-name
/// grammar (lowercase identifier per [[type-def legal names::au-type-system]]). Distinct from fragment-order
/// errors so consumers can route name-vs-position issues separately.
/// Spec [[type reference::au-type-system]].
pub const WIKILINK_INVALID_FIELD_NAME: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-invalid-field-name");

/// Wikilink ends with the `::repo` qualifier but no repo value — `[[note::]]`.
/// Same rationale as the other empty-fragment codes. The `::repo` out-of-order
/// and at-most-one violations ride `wikilink-fragment-order`.
pub const WIKILINK_EMPTY_REPO: DiagnosticCode = DiagnosticCode::from_static("wikilink-empty-repo");

/// Wikilink carries the `@commit` pin but no commit value — `[[note::@]]` /
/// `[[note::base@]]`. Same rationale as the other empty-fragment codes. The
/// `@commit` binds to `::repo`; a bare `@` with no `::` is a literal filename
/// character, not an empty pin. Spec [[type reference::au-type-system]],
/// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
pub const WIKILINK_EMPTY_COMMIT: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-empty-commit");

/// A `@commit` pin whose value is not a hex oid — `[[note::@main]]`,
/// `[[note::@HEAD~2]]`. A pin is a coordinate into an immutable past, so its
/// commit must be an immutable oid (full or an abbreviated prefix), never a
/// mutable or relative rev (a branch, a tag, `HEAD`, `HEAD~2`), which can be
/// repointed or moves with HEAD. Checked at parse, no length floor, an
/// over-short prefix is accepted here and fails at resolution as ambiguous.
/// Spec [[type reference::au-type-system]],
/// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
pub const PINNED_COMMIT_NOT_OID: DiagnosticCode =
    DiagnosticCode::from_static("pinned-commit-not-oid");

/// Wikilink `::repo` value that violates the identifier regex ([[type-def legal names::au-type-system]])
/// — `[[note::b/c]]`, `[[note::1bad]]`. Repo names share the type/field-name
/// grammar; rejecting at parse gives a clear authoring error instead of the
/// misleading `reference-repo-unknown` a bad name would otherwise hit at lookup.
/// Distinct from `wikilink-empty-repo` and the order codes. Spec [[type reference::au-type-system]].
pub const WIKILINK_INVALID_REPO_NAME: DiagnosticCode =
    DiagnosticCode::from_static("wikilink-invalid-repo-name");
