//! Wikilink parsing + repo file index + reference resolution per [[type reference::au-type-system]].
//!
//! Three pieces:
//! - [`parse_wikilink`] — `"[[target[#anchor][^block_id]]]"` → [`WikilinkRef`].
//!   Strict canonical order; malformations surface as [`WikilinkParseError`]
//!   variants that callers map to specific diagnostic codes.
//! - [`looks_like_wikilink`] — cheap `[[ … ]]` shape sniff, used by callers
//!   that want to route wikilink-shaped strings to wikilink-handling code
//!   paths (and surface a precise parse error there) rather than treating
//!   them as plain primitives.
//! - [`RepoIndex`] — basename + relative-path index over a repo's files.
//!   Construction surfaces `case-collision-basename` diagnostics (two basenames
//!   colliding under case-insensitive comparison; legal on Linux, illegal on
//!   macOS-default APFS).
//! - [`RepoIndex::resolve`] — wikilink target → absolute path. Implements
//!   the [[type reference::au-type-system]] algorithm: `/`-bearing targets resolve as repo-relative paths;
//!   bare targets match basenames case-insensitively; an extensionless target
//!   matches any file's stem, whatever the extension; multiple matches are
//!   ambiguous, none is missing.
//!
//! No filesystem I/O — the index is built from a caller-supplied list of
//! paths. The au-parser crate owns walking; this crate owns indexing +
//! resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, Severity, Span};

/// The persistent ordered map / set backing [`RepoIndex`]'s buckets. Clone is
/// O(1) and a patch is O(log n) with structural sharing, so an engine cloning
/// the held indices shares an unchanged repo's buckets by pointer, and an
/// incremental [`RepoIndex::insert`] / [`RepoIndex::remove`] patches only the
/// changed path. Iteration is key-ordered, matching `BTreeMap` / `BTreeSet`.
type OrdMap<K, V> = rpds::RedBlackTreeMapSync<K, V>;
type OrdSet<T> = rpds::RedBlackTreeSetSync<T>;

pub mod codes;

/// Parsed wikilink: `[[target[#anchor][^block_id][:field]]]`. `target` is
/// the resolution key; `anchor` and `block_id` parse-and-strip but
/// must conform to the canonical form. A block-id delimiter is either a
/// bare `^id` (navigational) or a doubled `^^id` (block-referent); the id
/// and its mode ride together in [`BlockId`]. `field` is the body-typing
/// contribution attribution per [[type reference::au-type-system]] — present only when the wikilink
/// participates in a body contribution.
///
/// An empty `target` with a locating fragment (`anchor` or `block_id`) is
/// the local form per [[type reference::au-type-system]]: `[[^id]]` / `[[#head]]` resolve
/// within the host file. See [`WikilinkRef::is_local`]. An empty `target`
/// with a `commit` is the commit-referent form (`[[::@sha]]`), which names a
/// commit rather than a file. See [`WikilinkRef::is_commit_referent`].
///
/// Implements `Serialize` under the `serde` feature, so the au-cli
/// introspect surface can emit `WikilinkRef` directly on the wire
/// without owning a duplicate DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct WikilinkRef {
    pub target: String,
    /// The `::repo` qualifier, the target repo crossed into. `None` is an
    /// unqualified link, resolved repo-local.
    pub repo: Option<String>,
    /// The `@commit` pin, the commit-ish whose tree the reference resolves
    /// against. Binds to `::repo`, written `::repo@commit` or the this-repo
    /// `::@commit`. `None` is an unpinned link, resolved against the working
    /// tree. Verbatim commit-ish, validity is a resolution-time concern. See
    /// [[type reference::au-type-system]] and [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    pub commit: Option<String>,
    pub anchor: Option<String>,
    /// The block-id fragment, coupling the id with the mode its sigil selected.
    /// `None` when the link carries no `^`. See [`BlockId`].
    pub block_id: Option<BlockId>,
    pub field: Option<String>,
}

impl WikilinkRef {
    /// True for the local forms (`[[^id]]` / `[[#head]]`): empty target,
    /// no commit, locating fragment present. Resolution targets the host file;
    /// name lookup is skipped entirely, so no missing/ambiguous outcome exists
    /// for local references. An empty target WITH a commit is not local, it is
    /// a commit-referent (see [`WikilinkRef::is_commit_referent`]).
    pub fn is_local(&self) -> bool {
        self.target.is_empty() && self.commit.is_none()
    }

    /// True for a COMMIT-only reference (`[[::@sha]]` / `[[::repo@sha]]`): an
    /// empty target plus a commit. It names a commit, not a file, so it resolves
    /// to no path, forms no backlink, and is never dangling. The commit stays
    /// readable because git history is append-only. Parse guarantees a
    /// commit-referent carries no locating fragment. See
    /// [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    pub fn is_commit_referent(&self) -> bool {
        self.target.is_empty() && self.commit.is_some()
    }

    /// True for ANY commit-pinned link, a named-target pin (`[[file::@sha]]`) or
    /// the empty-target commit-referent (`[[::@sha]]`). A pin is an inert snapshot
    /// into an immutable past: it forms no live inbound backlink, is not
    /// re-resolved against the live graph, and a refactor never rewrites it. The
    /// superset of [`WikilinkRef::is_commit_referent`], which is the empty-target
    /// case alone. See [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
    pub fn is_inert_pin(&self) -> bool {
        self.commit.is_some()
    }

    /// The block-id string, mode-agnostic. `None` when the link has no `^`.
    /// Locating a block by id is the same in either mode, so callers that only
    /// need the id use this instead of matching on [`BlockId`].
    pub fn block_id_str(&self) -> Option<&str> {
        self.block_id.as_ref().map(|b| b.id.as_str())
    }
}

/// A wikilink's block-id fragment: the id, and the mode the sigil selected.
/// The two ride together so a mode never exists without an id, and the mode is
/// the link's own (decided locally), never the target's typed-ness. See
/// [[type block-id::au-type-system]], [[type reference::au-type-system]].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BlockId {
    pub id: String,
    /// `true` for a `^^id` block-referent (the block's typed value fills the
    /// slot). `false` for a bare `^id` navigational anchor (the file is the
    /// referent, `^id` a jump anchor into it).
    pub referent: bool,
}

/// Reasons a wikilink string failed to parse. Each variant maps to a
/// specific `wikilink-*` diagnostic code; callers in au-core build the
/// user-facing diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikilinkParseError {
    /// Missing `[[` prefix or `]]` suffix.
    NotAWikilink,
    /// `[[]]` or `[[   ]]` — no content inside the brackets.
    EmptyInner,
    /// Target portion is empty without a locating fragment: `[[:field]]`.
    /// `#head` or `^block-id` grant the empty name — `[[#head]]` / `[[^id]]`
    /// are the legal local forms per [[type reference::au-type-system]]; a field alone has
    /// nothing to contribute without a target. Also the commit-referent with a
    /// stray fragment: `[[::@sha^id]]` names a commit, which has no addressable
    /// file interior, so a `#`/`^`/`:` fragment on it is rejected here.
    EmptyTarget,
    /// `[[note#]]` — anchor delimiter with no value.
    EmptyAnchor,
    /// `[[note^]]` — block-id delimiter with no value.
    EmptyBlockId,
    /// `[[note:]]` — field delimiter with no value.
    EmptyField,
    /// `[[note^block#anchor]]` — `^` appears before `#`. Canonical order
    /// is `target[#anchor][^block_id][:field]`; reverse order is rejected
    /// rather than silently disambiguated.
    ReversedDelimiters,
    /// `:field` appears before `#anchor` or `^block_id`. Per [[type reference::au-type-system]] the
    /// strict parse order is name / #head / ^block-id / :field.
    FieldOutOfOrder,
    /// `:field` value doesn't conform to the type-name regex ([[type-def legal names::au-type-system]]).
    InvalidFieldName,
    /// `[[note::]]` — `::repo` qualifier with no repo value.
    EmptyRepo,
    /// `[[note::@]]` / `[[note::base@]]` — `@commit` pin with no commit value.
    EmptyCommit,
    /// `[[note::@main]]` / `[[note::@HEAD~2]]` — a `@commit` pin whose value is not
    /// a hex oid. A pin is a coordinate into an immutable past, so its commit must
    /// be an immutable oid (full or an abbreviated prefix), never a mutable or
    /// relative rev (a branch, a tag, `HEAD`, `HEAD~2`), which can be repointed or
    /// move with HEAD. See [[type reference::au-type-system]].
    CommitNotOid,
    /// `::repo` out of canonical order: a `#`/`^`/`:` fragment precedes it, or
    /// more than one `::repo` appears. Order is `name ::repo #head ^block :field`.
    RepoOutOfOrder,
    /// `[[note::b/c]]` — the `::repo` value violates the identifier regex
    /// ([[type-def legal names::au-type-system]]). Rejected at parse, rather than degrading to a
    /// misleading `reference-repo-unknown` at lookup.
    InvalidRepoName,
}

/// Cheap `[[ … ]]` shape sniff. True for any trimmed string that starts
/// with `[[`, ends with `]]`, and has at least one character between
/// them. Used by the [[type reference::au-type-system]] precheck to route wikilink-shaped inputs to
/// wikilink-handling code paths regardless of whether they parse cleanly
/// — so a malformed wikilink surfaces as a precise parse error rather
/// than a generic "doesn't match shape".
pub fn looks_like_wikilink(s: &str) -> bool {
    let trimmed = s.trim();
    trimmed.len() > 4 && trimmed.starts_with("[[") && trimmed.ends_with("]]")
}

/// Parse a wikilink string in canonical order
/// (`target[#anchor][^block_id][:field]`). Whitespace inside the brackets
/// is trimmed. Anchors / block-ids / fields that appear must carry
/// non-empty values; reverse delimiter order is rejected; field-name
/// regex per [[type-def legal names::au-type-system]] enforced.
pub fn parse_wikilink(s: &str) -> Result<WikilinkRef, WikilinkParseError> {
    let trimmed = s.trim();
    let Some(inner) = trimmed
        .strip_prefix("[[")
        .and_then(|i| i.strip_suffix("]]"))
    else {
        return Err(WikilinkParseError::NotAWikilink);
    };
    parse_wikilink_inner(inner)
}

/// Parse a wikilink's inner form (the bytes BETWEEN `[[` and `]]`).
/// Used by the body scanner — `BodyEvent::Wikilink.raw` already
/// carries the inner-form text, so wrapping it back into `[[…]]` just
/// to call `parse_wikilink` is a wasted allocation. Same grammar
/// enforcement as `parse_wikilink`.
pub fn parse_wikilink_inner(inner: &str) -> Result<WikilinkRef, WikilinkParseError> {
    let inner = inner.trim();
    if inner.is_empty() {
        return Err(WikilinkParseError::EmptyInner);
    }

    // Split off the `::repo@commit` resolution-scope qualifiers first, canonical
    // order is `name ::repo @commit #head ^block :field`. `::` is unambiguous:
    // repo and field names are letter-first, so a `:` never abuts another in a
    // valid link. `@` binds to `::`, so it is a commit delimiter only here, in
    // the repo-scope position; a bare `@` with no preceding `::` is a literal
    // filename character (handled in the no-`::` branch below). See [[type reference::au-type-system]].
    let (name_part, repo, commit, frag) = if let Some(r) = find_repo_delim(inner) {
        // Anything before `::` other than the name means the qualifier is out
        // of order (a fragment, or a stray `:`, precedes it).
        if inner[..r].contains(['#', '^', ':']) {
            return Err(WikilinkParseError::RepoOutOfOrder);
        }
        let after = &inner[r + 2..];
        // At most one `::repo`. A `::` inside a `:field{type::repo}` qualifier is
        // not a second repo qualifier, so scan at brace depth 0.
        if find_repo_delim(after).is_some() {
            return Err(WikilinkParseError::RepoOutOfOrder);
        }
        // The repo value runs to the first fragment delimiter, the `@commit`
        // pin, or end. The first `@` after the repo scope is the commit
        // delimiter; repo names are identifier-only and never carry one.
        let repo_end = after.find(['#', '^', ':', '@']).unwrap_or(after.len());
        let repo_val = after[..repo_end].trim();
        // A commit pin, `::repo@commit` or the this-repo `::@commit`. The
        // commit-ish runs to the next fragment delimiter, taken verbatim;
        // its validity is a resolution-time concern.
        let (commit, rest) = if after.as_bytes().get(repo_end) == Some(&b'@') {
            let after_at = &after[repo_end + 1..];
            let commit_end = after_at.find(['#', '^', ':']).unwrap_or(after_at.len());
            let commit_val = after_at[..commit_end].trim();
            if commit_val.is_empty() {
                return Err(WikilinkParseError::EmptyCommit);
            }
            // A pin's commit must be an immutable oid, hex, full or an abbreviated
            // prefix. A mutable or relative rev (a branch, a tag, `HEAD~2`) can be
            // repointed or moves with HEAD, defeating the immutable past a pin
            // records. No length floor: an over-short prefix is syntactically fine
            // and fails at resolution as ambiguous. See [[type reference::au-type-system]].
            if !commit_val.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(WikilinkParseError::CommitNotOid);
            }
            (Some(commit_val.to_string()), &after_at[commit_end..])
        } else {
            (None, &after[repo_end..])
        };
        // Empty repo is the this-repo pin only when a commit follows
        // (`::@commit`); a bare `::` or `::#frag` is a missing repo. The `::`
        // is conceptually always present, so `@` always has its `::` anchor.
        let repo = if repo_val.is_empty() {
            if commit.is_none() {
                return Err(WikilinkParseError::EmptyRepo);
            }
            None
        } else if !is_valid_repo_name(repo_val) {
            return Err(WikilinkParseError::InvalidRepoName);
        } else {
            Some(repo_val.to_string())
        };
        (&inner[..r], repo, commit, rest)
    } else {
        let name_end = inner.find(['#', '^', ':']).unwrap_or(inner.len());
        (&inner[..name_end], None, None, &inner[name_end..])
    };

    // Locate the fragment delimiters within `frag`. Canonical order: `#`
    // (anchor), `^` (block_id), `:` (field). Reverse-pair and
    // field-misplacement violations are rejected, not silently disambiguated.
    let hash_idx = frag.find('#');
    let caret_idx = frag.find('^');
    if let (Some(h), Some(c)) = (hash_idx, caret_idx) {
        if c < h {
            return Err(WikilinkParseError::ReversedDelimiters);
        }
    }

    // The field `:` is the contribution delimiter, distinct from a `:` that is
    // literal heading text. A `:field` never follows a bare `#head`: a heading
    // is navigational, not an addressable value, so it can not be a
    // contribution source, see [[type reference::au-type-system]]. A heading therefore runs to
    // the `^block-id` or to the end, and a `:` inside it stays in the anchor.
    // - with a `^block-id`, the field `:` is the first `:` after the caret (a
    //   block-id carries no `:`).
    // - with a `#head` and no block, there is no field, every `:` is heading text.
    // - with neither, the first `:` is the field.
    let field_idx = match (hash_idx, caret_idx) {
        (_, Some(c)) => frag[c + 1..].find(':').map(|rel| c + 1 + rel),
        (Some(_), None) => None,
        (None, None) => frag.find(':'),
    };

    // A `:` that is neither the field delimiter nor heading text is a field
    // placed before its locators, out of order.
    if let Some(rc) = frag.find(':') {
        let anchor_end = caret_idx.unwrap_or(frag.len());
        let is_heading_text = hash_idx.is_some_and(|h| rc > h && rc < anchor_end);
        if !is_heading_text && Some(rc) != field_idx {
            return Err(WikilinkParseError::FieldOutOfOrder);
        }
    }

    let target_part = name_part.trim();
    if target_part.is_empty() {
        // `#head` / `^block-id` grant the empty name (the local forms
        // `[[#head]]` / `[[^id]]` per [[type reference::au-type-system]]); `:field` does not,
        // it needs a target to attribute a contribution to.
        let locating = caret_idx.is_some() || hash_idx.is_some();
        let any_fragment = locating || field_idx.is_some();
        if commit.is_some() {
            // An empty name plus a commit is a COMMIT-only reference: it names a
            // commit, not a file. It resolves to no file and is never backlinked
            // or diagnosed as dangling; the commit stays readable because git
            // history is append-only. See
            // [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
            // A commit has no addressable file interior, so a locating fragment
            // on one is rejected rather than silently dropped. A bare
            // commit-referent (`[[::@sha]]` / `[[::repo@sha]]`) falls through.
            if any_fragment {
                return Err(WikilinkParseError::EmptyTarget);
            }
        } else if repo.is_some() || !locating {
            // `[[:field]]` needs a target; `[[::repo]]` (repo-root) is the
            // deferred error per the qualifier decision; both reject here.
            return Err(WikilinkParseError::EmptyTarget);
        }
    }

    let anchor = if let Some(h) = hash_idx {
        // The heading runs to the block delimiter or the end, never to a `:`,
        // so a colon in heading text is preserved as part of the anchor.
        let anchor_end = caret_idx.unwrap_or(frag.len());
        let a = frag[h + 1..anchor_end].trim();
        if a.is_empty() {
            return Err(WikilinkParseError::EmptyAnchor);
        }
        Some(a.to_string())
    } else {
        None
    };

    // A doubled caret `^^id` is the block-referent mode (the block's typed
    // value fills the slot); a bare `^id` is navigational (the file is the
    // referent, `^id` a jump anchor). The mode is the sigil's, decided
    // locally, never the target's typed-ness. See [[type block-id::au-type-system]].
    let block_id = if let Some(c) = caret_idx {
        let referent = frag.as_bytes().get(c + 1) == Some(&b'^');
        let id_start = c + if referent { 2 } else { 1 };
        let block_end = field_idx.unwrap_or(frag.len());
        let id = frag[id_start..block_end].trim();
        if id.is_empty() {
            return Err(WikilinkParseError::EmptyBlockId);
        }
        Some(BlockId {
            id: id.to_string(),
            referent,
        })
    } else {
        None
    };

    let field = if let Some(f) = field_idx {
        let value = frag[f + 1..].trim();
        if value.is_empty() {
            return Err(WikilinkParseError::EmptyField);
        }
        // The `:field` may carry a collision qualifier, `field{type}` /
        // `field{type::repo}` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). The raw form is kept
        // verbatim; au-core parses the qualifier out. A bad field name or
        // qualifier is `wikilink-invalid-field-name`.
        if !is_valid_field_attribution(value) {
            return Err(WikilinkParseError::InvalidFieldName);
        }
        Some(value.to_string())
    } else {
        None
    };

    Ok(WikilinkRef {
        target: target_part.to_string(),
        repo,
        commit,
        anchor,
        block_id,
        field,
    })
}

/// `:field` value regex per [[type-def legal names::au-type-system]] — same shape used for type names and
/// enum literals. Lifted into au-references to avoid a dep on au-grammar
/// just for this predicate.
/// A `::repo` value follows the same identifier grammar as type and field
/// names ([[type-def legal names::au-type-system]]). Repo names in practice (`au-engine`,
/// `my-notes`) are identifier-shaped, so a value that breaks the regex
/// (a slash, a space, a leading digit) is a malformed reference, surfaced at
/// parse as `wikilink-invalid-repo-name` rather than a misleading unknown-repo
/// error at lookup.
fn is_valid_repo_name(s: &str) -> bool {
    is_valid_field_name(s)
}

fn is_valid_field_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    for segment in s.split('.') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_ascii_alphabetic() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return false;
        }
    }
    true
}

/// The byte offset of the first `::` resolution-scope delimiter at brace depth
/// zero, or `None`. A `::` inside a `:field{type::repo}` collision qualifier sits
/// at depth > 0 and is NOT a repo delimiter ([[type-def fields collision - auto-unify and qualified field::au-type-system]]); this is what
/// keeps the field qualifier's `::` from being misread as the repo scope. `::` is
/// ASCII, so byte scanning is char-safe.
fn find_repo_delim(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            // Clamp at zero so an unmatched `}` (a pathological `}` in a target
            // basename) cannot drive depth negative and hide a later real `::repo`.
            b'}' => depth = (depth - 1).max(0),
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// A `:field` attribution value: a plain field name, or a collision-qualified
/// `field{type}` / `field{type::repo}` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]). The field name and the
/// qualifier's type-name / `::repo` each follow the identifier grammar. The
/// braces are the only permitted extra punctuation, and exactly one `{...}` may
/// appear, closing at the end.
pub fn is_valid_field_attribution(s: &str) -> bool {
    match s.split_once('{') {
        None => is_valid_field_name(s),
        Some((field, rest)) => {
            let Some(qualifier) = rest.strip_suffix('}') else {
                return false; // unclosed or trailing junk after `}`
            };
            if qualifier.contains('{') || qualifier.contains('}') {
                return false; // a second brace group
            }
            let (type_name, repo) = match qualifier.split_once("::") {
                Some((t, r)) => (t, Some(r)),
                None => (qualifier, None),
            };
            is_valid_field_name(field)
                && is_valid_field_name(type_name)
                && repo.is_none_or(is_valid_repo_name)
        }
    }
}

/// Repo-wide file index.
///
/// All paths stored are absolute. `paths_by_basename` is keyed by
/// lowercased basename for case-insensitive lookup; `paths_by_stem` by
/// lowercased stem (basename minus the final extension), serving
/// extensionless targets; `paths_by_type_name` by a `*.type.yaml` def's
/// type-name, serving a def-ref `[[type-name]]` whose dots would otherwise read
/// as an extension; `relpaths_set` holds repo-relative paths for `/`-bearing
/// wikilink targets.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RepoIndex {
    root: PathBuf,
    paths_by_basename: OrdMap<String, Vec<PathBuf>>,
    paths_by_stem: OrdMap<String, Vec<PathBuf>>,
    paths_by_type_name: OrdMap<String, Vec<PathBuf>>,
    relpaths_set: OrdSet<PathBuf>,
    relpath_to_absolute: OrdMap<PathBuf, PathBuf>,
}

/// Whether a wikilink target names a path that leaves its own repo.
///
/// A wikilink is repo-scoped by construction: a bare name resolves in the
/// source's repo, and `::repo` is how an edge crosses a boundary. So a target
/// that is absolute, or that climbs above the repo root, contradicts its own
/// scope and can never resolve — no file authored later makes it valid.
///
/// Checked LEXICALLY, never against the filesystem. Resolution is a lookup over
/// indexed repo-relative paths and never joins a target onto a root, so this is
/// not a traversal guard; nothing escapes today. It exists so an impossible
/// address is REPORTED as impossible rather than as merely absent, which is the
/// bucket a legitimately renamed or not-yet-written target falls in.
///
/// Only a path-mode target can escape. A bare name has no components to climb
/// with, and interior `..` that stays inside the repo (`a/../b`) is fine.
///
/// The fix a diagnostic should carry is [`ESCAPES_REPO_FIX`].
pub fn target_escapes_repo(target: &str) -> bool {
    if !target.contains('/') {
        return false;
    }
    if target.starts_with('/') {
        return true;
    }
    let mut depth: i32 = 0;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// How to address a node in another repo, the fix for a target that
/// [`target_escapes_repo`].
///
/// It lives beside the predicate rather than at the diagnostic sites because it
/// is a statement about what the predicate MEANS, and it has one right answer on
/// every surface. The surfaces word their MESSAGES differently, correctly so — a
/// typed slot names its field, a prose link quotes its raw text — but the fix is
/// the same sentence, and kept twice it would drift.
pub const ESCAPES_REPO_FIX: &str =
    "name the target in its own repo with the `::repo` qualifier, `[[name::repo]]`, rather than \
     reaching across with a relative path";

/// Failure to resolve a wikilink target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionError {
    Missing,
    /// Two or more files share the target's basename under case-insensitive
    /// match. Carries every match for diagnostics.
    Ambiguous(Vec<PathBuf>),
}

impl RepoIndex {
    /// Build an index. `files` is the repo's file set as absolute paths;
    /// each must live under `root`. Files outside `root` are silently
    /// ignored — the walker is the right place to filter.
    ///
    /// Returns the index plus any `case-collision-basename` diagnostics
    /// detected during construction.
    pub fn build(
        root: impl Into<PathBuf>,
        files: impl IntoIterator<Item = PathBuf>,
    ) -> (Self, Vec<Diagnostic>) {
        let root = root.into();
        // Accumulate into plain ordered maps, then collect into the persistent
        // form once. The buckets come out in file-iteration order, which the
        // caller sorts, so they match the order incremental insert/remove keep.
        let mut paths_by_basename: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut paths_by_stem: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut paths_by_type_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut relpaths_set: BTreeSet<PathBuf> = BTreeSet::new();
        let mut relpath_to_absolute: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        let mut diags: Vec<Diagnostic> = Vec::new();

        for absolute in files {
            let Ok(relative) = absolute.strip_prefix(&root) else {
                continue;
            };
            let relative = relative.to_path_buf();

            // Relative path index — supports `/`-bearing wikilink targets.
            relpaths_set.insert(relative.clone());
            relpath_to_absolute.insert(relative.clone(), absolute.clone());

            // Basename index — supports bare wikilink targets.
            let Some(basename) = absolute.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let lower = basename.to_ascii_lowercase();
            let bucket = paths_by_basename.entry(lower).or_default();

            // Detect case-collision: same lowercased basename but a different
            // exact basename string already in the bucket.
            for existing in bucket.iter() {
                let Some(prev_basename) = existing.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if prev_basename != basename {
                    diags.push(case_collision_diag(
                        &absolute,
                        existing,
                        basename,
                        prev_basename,
                    ));
                    break;
                }
            }

            bucket.push(absolute.clone());

            // Stem index — supports extensionless bare targets across every
            // file kind. An extension is only required when the stem alone
            // is ambiguous; the ambiguity surfaces at reference time.
            if let Some(stem) = absolute.file_stem().and_then(|s| s.to_str()) {
                paths_by_stem
                    .entry(stem.to_ascii_lowercase())
                    .or_default()
                    .push(absolute.clone());
            }
            // Type-name index — a `*.type.yaml` def is reachable by its
            // type-name, not only its `.type`-tailed file stem.
            if let Some(name) = type_name_alias(&absolute) {
                paths_by_type_name.entry(name).or_default().push(absolute);
            }
        }

        let index = RepoIndex {
            root,
            paths_by_basename: paths_by_basename.into_iter().collect(),
            paths_by_stem: paths_by_stem.into_iter().collect(),
            paths_by_type_name: paths_by_type_name.into_iter().collect(),
            relpaths_set: relpaths_set.into_iter().collect(),
            relpath_to_absolute: relpath_to_absolute.into_iter().collect(),
        };
        (index, diags)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.relpaths_set.size()
    }

    pub fn is_empty(&self) -> bool {
        self.relpaths_set.is_empty()
    }

    /// A canonical, order-independent rendering of the index content, for
    /// identical-build parity assertions.
    ///
    /// The persistent maps iterate in sorted key order, so this compares the
    /// logical content, not the internal tree shape. A red-black tree's shape
    /// depends on its insertion order, so two equal indices built differently
    /// (an incremental patch versus a from-scratch build) render different
    /// whole-map `Debug` strings while comparing equal under `PartialEq`.
    pub fn parity_repr(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = write!(s, "root={:?}", self.root);
        for (k, v) in self.paths_by_basename.iter() {
            let _ = write!(s, "|bn {k:?}={v:?}");
        }
        for (k, v) in self.paths_by_stem.iter() {
            let _ = write!(s, "|st {k:?}={v:?}");
        }
        for (k, v) in self.paths_by_type_name.iter() {
            let _ = write!(s, "|tn {k:?}={v:?}");
        }
        for p in self.relpaths_set.iter() {
            let _ = write!(s, "|rp {p:?}");
        }
        for (k, v) in self.relpath_to_absolute.iter() {
            let _ = write!(s, "|ra {k:?}={v:?}");
        }
        s
    }

    /// Insert one file incrementally, the dual of a single [`build`] iteration,
    /// so an add patches the index in O(its keys) instead of rebuilding it.
    ///
    /// Returns `true` when the insert changes the case-collision set: the file's
    /// basename case-collides with one already indexed. The collision
    /// diagnostics a build emits are not maintained here, so the caller must
    /// fall back to a full rebuild. Returns `false` on a clean insert.
    ///
    /// Every bucket stays path-sorted, the order [`build`]'s sorted-file
    /// iteration produces, so a clean insert leaves the index byte-identical to
    /// a from-scratch build over the same set. A path outside the root is
    /// ignored, as [`build`] ignores it.
    ///
    /// [`build`]: RepoIndex::build
    pub fn insert(&mut self, absolute: PathBuf) -> bool {
        let Ok(relative) = absolute.strip_prefix(&self.root) else {
            return false;
        };
        let relative = relative.to_path_buf();
        let Some(basename) = absolute.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        let lower = basename.to_ascii_lowercase();
        // A different exact basename already under this lowercased key is a
        // case-collision a build would diagnose; fall back rather than splice it.
        if let Some(bucket) = self.paths_by_basename.get(&lower) {
            if bucket_has_case_variant(bucket, basename, None) {
                return true;
            }
        }
        self.relpaths_set.insert_mut(relative.clone());
        self.relpath_to_absolute
            .insert_mut(relative, absolute.clone());
        bucket_insert(&mut self.paths_by_basename, lower, absolute.clone());
        if let Some(stem) = absolute.file_stem().and_then(|s| s.to_str()) {
            bucket_insert(
                &mut self.paths_by_stem,
                stem.to_ascii_lowercase(),
                absolute.clone(),
            );
        }
        if let Some(name) = type_name_alias(&absolute) {
            bucket_insert(&mut self.paths_by_type_name, name, absolute);
        }
        false
    }

    /// Remove one file incrementally, the dual of [`insert`]. Returns `true`
    /// when the removal changes the case-collision set (the file was part of a
    /// collision), the caller falls back. Returns `false` on a clean removal.
    ///
    /// An emptied bucket is dropped, [`build`] never holds one, so a clean
    /// removal leaves the index byte-identical to a from-scratch build.
    ///
    /// [`insert`]: RepoIndex::insert
    /// [`build`]: RepoIndex::build
    pub fn remove(&mut self, absolute: &Path) -> bool {
        let Ok(relative) = absolute.strip_prefix(&self.root) else {
            return false;
        };
        let Some(basename) = absolute.file_name().and_then(|s| s.to_str()) else {
            return false;
        };
        let lower = basename.to_ascii_lowercase();
        // Removing a file that shared its lowercased basename with a case-variant
        // resolves (or alters) a collision a build diagnosed; fall back.
        if let Some(bucket) = self.paths_by_basename.get(&lower) {
            if bucket_has_case_variant(bucket, basename, Some(absolute)) {
                return true;
            }
        }
        self.relpaths_set.remove_mut(relative);
        self.relpath_to_absolute.remove_mut(relative);
        remove_from_bucket(&mut self.paths_by_basename, &lower, absolute);
        if let Some(stem) = absolute.file_stem().and_then(|s| s.to_str()) {
            remove_from_bucket(
                &mut self.paths_by_stem,
                &stem.to_ascii_lowercase(),
                absolute,
            );
        }
        if let Some(name) = type_name_alias(absolute) {
            remove_from_bucket(&mut self.paths_by_type_name, &name, absolute);
        }
        false
    }

    /// Resolve a wikilink target string to an absolute path per [[type reference::au-type-system]].
    ///
    /// An exact basename match wins. Otherwise the target matches any file
    /// whose stem equals it, whatever the file's real extension — `[[s-001]]`
    /// reaches `s-001.yaml`, `[[photo]]` reaches `photo.png`, and a dotted name
    /// like `[[my.file.name]]` reaches `my.file.name.md`. Typing an extension
    /// only matters to disambiguate a shared stem, where the exact basename
    /// wins first.
    pub fn resolve(&self, target: &str) -> Result<PathBuf, ResolutionError> {
        if target.contains('/') {
            // Repo-relative path mode. Exact relpath first, then the bare-stem
            // rule within the same directory. The stem filter keys on the whole
            // final segment, so `dir/my.file.name` reaches `dir/my.file.name.md`
            // (stem `my.file.name`), while a wrong-extension target like
            // `dir/note.pdf` still misses (no file there stems to `note.pdf`).
            let rel = PathBuf::from(target);
            if let Some(abs) = self.relpath_to_absolute.get(&rel) {
                return Ok(abs.clone());
            }
            let matches: Vec<PathBuf> = self
                .relpath_to_absolute
                .iter()
                .filter(|(r, _)| r.parent() == rel.parent() && r.file_stem() == rel.file_name())
                .map(|(_, abs)| abs.clone())
                .collect();
            return match matches.len() {
                0 => Err(ResolutionError::Missing),
                1 => Ok(matches.into_iter().next().unwrap()),
                _ => Err(ResolutionError::Ambiguous(matches)),
            };
        }

        // Basename mode (case-insensitive). The literal basename first, then the
        // stem index keyed by the whole target, then the type-name alias. Select
        // the winning bucket as a borrow, then clone only the single winner or the
        // ambiguous set — the common single-match and `.is_ok()` existence checks
        // stop allocating a throwaway `Vec` per resolve.
        //
        // An empty bucket is treated as no match and falls through, so the
        // outcome is identical to cloning-then-testing `is_empty()`.
        //
        // The stem lookup keys on the WHOLE target, not a stem-of-target, so a
        // dotted name like `[[my.file.name]]` reaches `my.file.name.md` (stem
        // `my.file.name`), while a wrong-extension target like `[[note.pdf]]`
        // still misses (nothing stems to `note.pdf`). Exact basename is tried
        // first, so `[[note.yaml]]` disambiguates a shared `note` stem to the
        // yaml file.
        //
        // The type-name fallback (`paths_by_type_name`) trails the stem lookup:
        // a def-ref `[[mcp.tool.propose]]` is a type-name, and the file's stem
        // carries the `.type` tail (`mcp.tool.propose.type`), so the stem lookup
        // misses and the alias resolves it. A real file always wins because this
        // bucket is only consulted when basename and stem found nothing.
        // [[type-def shape def-ref::au-type-system]].
        let key = target.to_ascii_lowercase();
        let bucket: Option<&Vec<PathBuf>> = self
            .paths_by_basename
            .get(&key)
            .filter(|b| !b.is_empty())
            .or_else(|| self.paths_by_stem.get(&key).filter(|b| !b.is_empty()))
            .or_else(|| self.paths_by_type_name.get(&key).filter(|b| !b.is_empty()));

        match bucket {
            None => Err(ResolutionError::Missing),
            Some(b) if b.len() == 1 => Ok(b[0].clone()),
            Some(b) => Err(ResolutionError::Ambiguous(b.clone())),
        }
    }
}

/// Outcome of `resolve_block_id` for `[[file^id]]` references that target
/// a typed fence inside another file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlock<'a> {
    /// Raw fence-info string — e.g. `"yaml [:assumptions]"`.
    pub info: &'a str,
    /// Verbatim content between the fences.
    pub body: &'a str,
}

/// Reasons a `[[file^id]]` reference couldn't be honored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockResolutionError {
    /// No `^id` marker present in the target file at all.
    NotFound,
    /// `^id` marker exists, but the block it attaches to has no
    /// `[:fieldName]` info — it's a plain code block, not a contribution.
    NotTyped,
}

/// Resolve a `^block-id` reference against a target file's scanned body
/// events. Returns the resolved block's info string + verbatim body when
/// the block is typed; surfaces `NotFound` / `NotTyped` otherwise.
///
/// Callers map the errors to the [[type block-id::au-type-system]] diagnostic codes
/// (`block-id-not-found`, `block-id-not-typed`).
pub fn resolve_block_id<'a>(
    events: &'a [au_parser::BodyEvent<'a>],
    id: &str,
) -> Result<ResolvedBlock<'a>, BlockResolutionError> {
    let mut saw_marker = false;
    for event in events {
        match event {
            au_parser::BodyEvent::FencedBlock {
                info,
                body,
                trailing_block_id,
                ..
            } => {
                if trailing_block_id.as_deref() == Some(id) {
                    return match extract_field_marker(info) {
                        Some(_) => Ok(ResolvedBlock { info, body }),
                        None => Err(BlockResolutionError::NotTyped),
                    };
                }
            }
            au_parser::BodyEvent::BlockIdMarker { id: marker_id, .. } => {
                if *marker_id == id {
                    saw_marker = true;
                }
            }
            _ => {}
        }
    }
    if saw_marker {
        Err(BlockResolutionError::NotTyped)
    } else {
        Err(BlockResolutionError::NotFound)
    }
}

/// Resolve a `#head` anchor against a body's heading events, per the
/// [[type reference::au-type-system]] anchor-matching contract:
/// - case-insensitive, exact text match (Unicode lowercase).
/// - a heading's text excludes its trailing `^id` marker, the scanner
///   strips it before events are emitted.
/// - several matches, the first in document order wins.
///
/// Returns the heading's span. Navigational only — anchors are never
/// type-bearing, so there is no typed/untyped distinction to surface.
pub fn resolve_anchor(events: &[au_parser::BodyEvent<'_>], anchor: &str) -> Option<ByteRange> {
    let wanted = anchor.trim().to_lowercase();
    for event in events {
        if let au_parser::BodyEvent::Heading { text, span, .. } = event {
            if text.trim().to_lowercase() == wanted {
                return Some(*span);
            }
        }
    }
    None
}

/// Extract the `[:fieldName]` marker from a fence-info string, if present.
/// Public so the [[type-instance body contribution::au-type-system]] fence validation can also use the same parse.
pub fn extract_field_marker(info: &str) -> Option<&str> {
    let start = info.find("[:")? + 2;
    let rest = &info[start..];
    let end = rest.find(']')?;
    let name = &rest[..end];
    // The marker may carry a collision qualifier, `[:field{type}]` /
    // `[:field{type::repo}]` ([[type-def fields collision - auto-unify and qualified field::au-type-system]]); the raw form is returned,
    // au-core parses the qualifier out.
    if is_valid_field_attribution(name) {
        Some(name)
    } else {
        None
    }
}

/// The type-name a `*.type.{yaml,yml}` file is reachable by, as a lowercased
/// stem-index alias. A type-def's identity is its type-name, but its file stem
/// carries the `.type` tail (`mcp.tool.propose.type.yaml` stems to
/// `mcp.tool.propose.type`), so a wikilink written as the type-name would miss.
/// The alias closes that gap, so a def-ref `[[mcp.tool.propose]]` resolves to
/// the file for validation, navigational edges, and backlinks alike.
/// See [[type-def shape def-ref::au-type-system]]. The `type/foo.yaml` form already stems to its
/// own name, so only the `*.type.{yaml,yml}` suffix form needs the alias.
fn type_name_alias(absolute: &Path) -> Option<String> {
    let basename = absolute.file_name()?.to_str()?;
    let name = basename
        .strip_suffix(".type.yaml")
        .or_else(|| basename.strip_suffix(".type.yml"))?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

/// True when a basename bucket holds a path whose exact basename case-differs
/// from `exact`, ignoring `skip`. Such a pair is the case-collision a build
/// diagnoses; the incremental delta falls back rather than maintain it. Every
/// path in a bucket shares `exact`'s lowercased basename, so a differing exact
/// basename differs only in case.
fn bucket_has_case_variant(bucket: &[PathBuf], exact: &str, skip: Option<&Path>) -> bool {
    bucket.iter().any(|p| {
        Some(p.as_path()) != skip
            && p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|b| b != exact)
    })
}

/// Insert into a path-sorted bucket at its sorted position, the order a build's
/// sorted-file iteration produces.
fn sorted_insert(bucket: &mut Vec<PathBuf>, path: PathBuf) {
    let pos = bucket.partition_point(|p| p < &path);
    bucket.insert(pos, path);
}

/// Insert a path into the keyed bucket, keeping it path-sorted. Patches the
/// bucket in place (copy-on-write) when the key exists, else inserts a fresh
/// single-element bucket.
fn bucket_insert(map: &mut OrdMap<String, Vec<PathBuf>>, key: String, path: PathBuf) {
    match map.get_mut(&key) {
        Some(bucket) => sorted_insert(bucket, path),
        None => map.insert_mut(key, vec![path]),
    }
}

/// Remove a path from a bucket, dropping the bucket when it empties so the index
/// matches a build, which never holds an empty bucket.
fn remove_from_bucket(map: &mut OrdMap<String, Vec<PathBuf>>, key: &str, path: &Path) {
    let empty = match map.get_mut(key) {
        Some(bucket) => {
            bucket.retain(|p| p != path);
            bucket.is_empty()
        }
        None => false,
    };
    if empty {
        map.remove_mut(key);
    }
}

fn case_collision_diag(
    new_path: &Path,
    existing_path: &Path,
    new_basename: &str,
    existing_basename: &str,
) -> Diagnostic {
    Diagnostic {
        code: codes::CASE_COLLISION_BASENAME,
        severity: Severity::Warning,
        span: Span::for_file(new_path.to_path_buf()),
        message: format!(
            "basename '{}' collides under case-insensitive comparison with '{}' — references to either are ambiguous on case-insensitive filesystems (macOS APFS default)",
            new_basename, existing_basename
        ),
        related: vec![Span::for_file(existing_path.to_path_buf())],
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- target_escapes_repo -----

    #[test]
    fn a_target_climbing_above_the_repo_escapes() {
        // The shape a consumer shipped: a workspace-relative path inside a
        // this-repo-scoped pin, reaching a sibling member.
        assert!(target_escapes_repo("../sibling-repo/notes/x.md"));
        assert!(target_escapes_repo("../x.md"));
        assert!(target_escapes_repo("a/../../b"));
        // Absolute is the same class: not repo-relative, so not addressable.
        assert!(target_escapes_repo("/etc/passwd"));
    }

    #[test]
    fn a_target_staying_inside_the_repo_does_not_escape() {
        assert!(!target_escapes_repo("notes/x.md"));
        assert!(!target_escapes_repo("./notes/x.md"));
        // Interior `..` that stays within the repo is an ordinary path.
        assert!(!target_escapes_repo("a/../b.md"));
        assert!(!target_escapes_repo("a/b/../../c.md"));
        // A bare name has no components to climb with, so it never escapes. A
        // lone `..` is basename mode and simply resolves to nothing, which the
        // ordinary missing-target diagnostic already covers.
        assert!(!target_escapes_repo("note"));
        assert!(!target_escapes_repo(".."));
        assert!(!target_escapes_repo("..note"));
    }

    // ----- parse_wikilink -----

    #[test]
    fn parses_bare_target() {
        let r = parse_wikilink("[[note]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor, None);
        assert_eq!(r.block_id, None);
    }

    #[test]
    fn parses_target_with_path() {
        let r = parse_wikilink("[[notes/sub/foo.md]]").unwrap();
        assert_eq!(r.target, "notes/sub/foo.md");
    }

    #[test]
    fn parses_target_with_anchor() {
        let r = parse_wikilink("[[note#section-2]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor.as_deref(), Some("section-2"));
    }

    #[test]
    fn parses_target_with_block_id() {
        let r = parse_wikilink("[[note^abc-123]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.block_id_str(), Some("abc-123"));
        // A bare `^id` is navigational, not a block-referent.
        assert!(!r.block_id.as_ref().unwrap().referent);
    }

    #[test]
    fn parses_block_referent_double_caret() {
        // `^^id` is the block-referent mode, the block's value fills the slot.
        let r = parse_wikilink("[[note^^abc-123]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.block_id_str(), Some("abc-123"));
        assert!(r.block_id.as_ref().unwrap().referent);
    }

    #[test]
    fn parses_block_referent_with_field() {
        // A body contribution `[[target^^id:field]]`, the doubled caret
        // does not swallow the `:field`.
        let r = parse_wikilink("[[note^^id:field]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.block_id_str(), Some("id"));
        assert!(r.block_id.as_ref().unwrap().referent);
        assert_eq!(r.field.as_deref(), Some("field"));
    }

    #[test]
    fn parses_collision_qualified_field() {
        // The `:field` may carry a collision qualifier, `field{type}` /
        // `field{type::repo}`; the raw form is kept for au-core to split.
        let own = parse_wikilink("[[note:title{tag}]]").unwrap();
        assert_eq!(own.field.as_deref(), Some("title{tag}"));
        let peer = parse_wikilink("[[note:title{tag::base}]]").unwrap();
        assert_eq!(peer.field.as_deref(), Some("title{tag::base}"));
    }

    #[test]
    fn rejects_malformed_field_qualifier() {
        // A bad field name, bad qualifier type, or malformed braces is
        // `wikilink-invalid-field-name`.
        for bad in [
            "[[note:title{}]]",     // empty qualifier
            "[[note:title{1bad}]]", // qualifier violates the type-name regex
            "[[note:title{tag]]",   // unclosed brace
            "[[note:{tag}]]",       // empty field name
        ] {
            assert_eq!(
                parse_wikilink(bad),
                Err(WikilinkParseError::InvalidFieldName),
                "{bad} must be rejected",
            );
        }
    }

    #[test]
    fn field_attribution_accepts_bare_and_qualified() {
        assert!(is_valid_field_attribution("title"));
        assert!(is_valid_field_attribution("title{tag}"));
        assert!(is_valid_field_attribution("title{tag::base}"));
        assert!(!is_valid_field_attribution("title{}"));
        assert!(!is_valid_field_attribution("title{tag}extra"));
        assert!(!is_valid_field_attribution("title{a}{b}"));
    }

    #[test]
    fn extract_field_marker_accepts_qualified() {
        assert_eq!(
            extract_field_marker("yaml [:title{tag}]"),
            Some("title{tag}")
        );
        assert_eq!(
            extract_field_marker("[:title{tag::base}]"),
            Some("title{tag::base}")
        );
        assert_eq!(extract_field_marker("[:title{}]"), None);
    }

    #[test]
    fn parses_local_block_referent() {
        // The local form `[[^^id]]` — empty target, block-referent.
        let r = parse_wikilink("[[^^id]]").unwrap();
        assert!(r.is_local());
        assert_eq!(r.block_id_str(), Some("id"));
        assert!(r.block_id.as_ref().unwrap().referent);
    }

    #[test]
    fn parses_canonical_order_anchor_then_block_referent() {
        let r = parse_wikilink("[[note#section^^abc-123]]").unwrap();
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id_str(), Some("abc-123"));
        assert!(r.block_id.as_ref().unwrap().referent);
    }

    #[test]
    fn parses_repo_qualified_block_referent() {
        // The `::repo` scope is split off the front, so the doubled caret parses
        // the same as the local case.
        let r = parse_wikilink("[[note::base^^abc]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.block_id_str(), Some("abc"));
        assert!(r.block_id.as_ref().unwrap().referent);
    }

    #[test]
    fn parses_commit_pinned_block_referent_with_field() {
        // The full canonical chain with a `^^` block-referent and a `:field`.
        let r = parse_wikilink("[[note::base@a1b2c3d^^id:field]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
        assert_eq!(r.block_id_str(), Some("id"));
        assert!(r.block_id.as_ref().unwrap().referent);
        assert_eq!(r.field.as_deref(), Some("field"));
    }

    #[test]
    fn empty_double_caret_block_id_is_error() {
        // `[[note^^]]` — a doubled caret with no id, still empty.
        assert_eq!(
            parse_wikilink("[[note^^]]"),
            Err(WikilinkParseError::EmptyBlockId)
        );
        assert_eq!(
            parse_wikilink("[[^^]]"),
            Err(WikilinkParseError::EmptyBlockId)
        );
    }

    #[test]
    fn parses_canonical_order_anchor_then_block_id() {
        // `[[target#anchor^block_id]]` is the canonical form when both
        // delimiters appear. All three fields populate distinctly.
        let r = parse_wikilink("[[note#section^abc-123]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id_str(), Some("abc-123"));
    }

    #[test]
    fn reversed_delimiters_is_error() {
        // `^` before `#` is rejected — the values would be ambiguous
        // without arbitrary precedence rules. Canonical order is
        // `target[#anchor][^block_id]`.
        assert_eq!(
            parse_wikilink("[[note^block#anchor]]"),
            Err(WikilinkParseError::ReversedDelimiters)
        );
    }

    #[test]
    fn empty_anchor_is_error() {
        assert_eq!(
            parse_wikilink("[[note#]]"),
            Err(WikilinkParseError::EmptyAnchor)
        );
    }

    #[test]
    fn empty_block_id_is_error() {
        assert_eq!(
            parse_wikilink("[[note^]]"),
            Err(WikilinkParseError::EmptyBlockId)
        );
    }

    #[test]
    fn trims_whitespace_inside_brackets() {
        let r = parse_wikilink("[[  note  ]]").unwrap();
        assert_eq!(r.target, "note");
    }

    #[test]
    fn rejects_non_wikilink_strings() {
        assert_eq!(
            parse_wikilink("note"),
            Err(WikilinkParseError::NotAWikilink)
        );
        assert_eq!(
            parse_wikilink("[note]"),
            Err(WikilinkParseError::NotAWikilink)
        );
        assert_eq!(
            parse_wikilink("[[note]"),
            Err(WikilinkParseError::NotAWikilink)
        );
        assert_eq!(parse_wikilink(""), Err(WikilinkParseError::NotAWikilink));
    }

    // ----- :field fragment ([[type reference::au-type-system]]) -----

    #[test]
    fn parses_target_with_field_fragment() {
        let r = parse_wikilink("[[paper-a:sources]]").unwrap();
        assert_eq!(r.target, "paper-a");
        assert_eq!(r.field.as_deref(), Some("sources"));
        assert_eq!(r.anchor, None);
        assert_eq!(r.block_id, None);
    }

    #[test]
    fn parses_full_canonical_order_with_field() {
        let r = parse_wikilink("[[note#section^block-id:field-name]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.repo, None);
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id_str(), Some("block-id"));
        assert_eq!(r.field.as_deref(), Some("field-name"));
    }

    #[test]
    fn parses_repo_qualifier() {
        let r = parse_wikilink("[[recovery::base]]").unwrap();
        assert_eq!(r.target, "recovery");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.anchor, None);
        assert_eq!(r.block_id, None);
        assert_eq!(r.field, None);
    }

    #[test]
    fn parses_repo_then_all_fragments_in_canonical_order() {
        let r = parse_wikilink("[[note::base#section^block-id:field-name]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id_str(), Some("block-id"));
        assert_eq!(r.field.as_deref(), Some("field-name"));
    }

    #[test]
    fn empty_repo_value_is_error() {
        assert_eq!(
            parse_wikilink("[[note::]]"),
            Err(WikilinkParseError::EmptyRepo)
        );
    }

    #[test]
    fn invalid_repo_name_rejected() {
        // A `::repo` value must follow the identifier regex, like field/type
        // names — a slash, a space, or a leading digit is rejected at parse,
        // not degraded to a misleading unknown-repo error at lookup.
        for bad in ["[[note::b/c]]", "[[note::ba d]]", "[[note::1bad]]"] {
            assert_eq!(
                parse_wikilink(bad),
                Err(WikilinkParseError::InvalidRepoName),
                "{bad} should be InvalidRepoName"
            );
        }
        // A dashed, identifier-shaped repo name (the common case) still parses.
        let r = parse_wikilink("[[note::my-notes]]").unwrap();
        assert_eq!(r.repo.as_deref(), Some("my-notes"));
    }

    #[test]
    fn repo_after_a_fragment_is_out_of_order() {
        // `::repo` must precede `#anchor`.
        assert_eq!(
            parse_wikilink("[[note#sec::base]]"),
            Err(WikilinkParseError::RepoOutOfOrder)
        );
    }

    #[test]
    fn two_repo_qualifiers_are_out_of_order() {
        assert_eq!(
            parse_wikilink("[[note::base::other]]"),
            Err(WikilinkParseError::RepoOutOfOrder)
        );
    }

    #[test]
    fn repo_root_reference_with_empty_target_is_empty_target() {
        // `[[::repo]]` (repo-root) is the deferred error per the qualifier
        // decision; it rejects as an empty target, not a local form.
        assert_eq!(
            parse_wikilink("[[::base]]"),
            Err(WikilinkParseError::EmptyTarget)
        );
    }

    // ----- @commit pin fragment ([[type reference::au-type-system]], pinned references) -----

    #[test]
    fn parses_repo_with_commit_pin() {
        let r = parse_wikilink("[[notes/draft::base@a1b2c3d]]").unwrap();
        assert_eq!(r.target, "notes/draft");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
        assert_eq!(r.anchor, None);
    }

    #[test]
    fn parses_this_repo_commit_pin() {
        // `::@commit` is the this-repo pin: empty repo, a commit present. The
        // `::` keeps `@` anchored even when the repo is the source's own.
        let r = parse_wikilink("[[notes/draft::@a1b2c3d]]").unwrap();
        assert_eq!(r.target, "notes/draft");
        assert_eq!(r.repo, None);
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
    }

    #[test]
    fn parses_this_repo_commit_referent() {
        // `[[::@sha]]` is a COMMIT-only reference: empty name, a commit, this
        // repo. It names a commit, not a file.
        let r = parse_wikilink("[[::@a1b2c3d]]").unwrap();
        assert_eq!(r.target, "");
        assert_eq!(r.repo, None);
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
        assert!(r.is_commit_referent());
        assert!(!r.is_local());
    }

    #[test]
    fn parses_cross_repo_commit_referent() {
        // `[[::repo@sha]]` names a commit in a peer repo, no file.
        let r = parse_wikilink("[[::au-provenance@a1b2c3d]]").unwrap();
        assert_eq!(r.target, "");
        assert_eq!(r.repo.as_deref(), Some("au-provenance"));
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
        assert!(r.is_commit_referent());
        assert!(!r.is_local());
    }

    #[test]
    fn commit_referent_with_a_fragment_is_rejected() {
        // A commit has no addressable file interior, so a locating fragment on
        // a commit-referent is rejected, not silently dropped.
        assert_eq!(
            parse_wikilink("[[::@a1b2c3d^id]]"),
            Err(WikilinkParseError::EmptyTarget)
        );
        assert_eq!(
            parse_wikilink("[[::au-provenance@a1b2c3d#head]]"),
            Err(WikilinkParseError::EmptyTarget)
        );
        assert_eq!(
            parse_wikilink("[[::@a1b2c3d:field]]"),
            Err(WikilinkParseError::EmptyTarget)
        );
    }

    #[test]
    fn a_local_form_is_not_a_commit_referent() {
        // `[[^id]]` / `[[#head]]` stay local: empty target, no commit.
        let r = parse_wikilink("[[^my-id]]").unwrap();
        assert!(r.is_local());
        assert!(!r.is_commit_referent());
        let r = parse_wikilink("[[#a-heading]]").unwrap();
        assert!(r.is_local());
        assert!(!r.is_commit_referent());
    }

    #[test]
    fn parses_commit_then_all_fragments_in_canonical_order() {
        let r = parse_wikilink("[[note::base@a1b2c3d#section^block-id:field-name]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.commit.as_deref(), Some("a1b2c3d"));
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id_str(), Some("block-id"));
        assert_eq!(r.field.as_deref(), Some("field-name"));
    }

    #[test]
    fn empty_commit_value_is_error() {
        // `::@` with no commit, and `::repo@` with no commit, both reject.
        assert_eq!(
            parse_wikilink("[[note::@]]"),
            Err(WikilinkParseError::EmptyCommit)
        );
        assert_eq!(
            parse_wikilink("[[note::base@]]"),
            Err(WikilinkParseError::EmptyCommit)
        );
    }

    #[test]
    fn a_non_oid_commit_is_rejected() {
        // A pin's commit must be an immutable oid. A mutable or relative rev
        // (branch, tag, `HEAD~2`) is rejected: it can be repointed or moves with
        // HEAD, defeating the immutable past the pin records.
        for raw in [
            "[[note::@main]]",
            "[[note::@HEAD]]",
            "[[note::@HEAD~2]]",
            "[[note::@v1.0]]",
            "[[note::base@release]]",
        ] {
            assert_eq!(
                parse_wikilink(raw),
                Err(WikilinkParseError::CommitNotOid),
                "expected {raw} to reject as a non-oid pin"
            );
        }
    }

    #[test]
    fn an_oid_commit_abbreviated_or_full_parses() {
        // Both an abbreviated hex prefix and a full oid are accepted. No length
        // floor: an over-short prefix is syntactically fine, resolution handles
        // ambiguity.
        assert_eq!(
            parse_wikilink("[[note::@a1b2c3]]")
                .unwrap()
                .commit
                .as_deref(),
            Some("a1b2c3")
        );
        assert_eq!(
            parse_wikilink("[[note::@0123456789abcdef0123456789abcdef01234567]]")
                .unwrap()
                .commit
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn bare_at_without_repo_is_a_literal_filename_char() {
        // `@` binds to `::`; with no `::` it is a legal filename character,
        // not a pin. `[[file@sha]]` is the literal name `file@sha`.
        let r = parse_wikilink("[[file@sha]]").unwrap();
        assert_eq!(r.target, "file@sha");
        assert_eq!(r.commit, None);
        assert_eq!(r.repo, None);
    }

    #[test]
    fn at_in_the_name_before_repo_stays_literal() {
        // A `@` in the name part, before `::`, is literal; only a `@` after the
        // repo scope is the commit delimiter.
        let r = parse_wikilink("[[my@file::base]]").unwrap();
        assert_eq!(r.target, "my@file");
        assert_eq!(r.repo.as_deref(), Some("base"));
        assert_eq!(r.commit, None);
    }

    #[test]
    fn empty_repo_without_a_commit_is_still_empty_repo() {
        // The this-repo grant rides on the commit; a bare `::` or `::#frag`
        // with no commit stays EmptyRepo.
        assert_eq!(
            parse_wikilink("[[note::]]"),
            Err(WikilinkParseError::EmptyRepo)
        );
        assert_eq!(
            parse_wikilink("[[note::#sec]]"),
            Err(WikilinkParseError::EmptyRepo)
        );
    }

    #[test]
    fn unpinned_links_keep_commit_none() {
        assert_eq!(parse_wikilink("[[note]]").unwrap().commit, None);
        assert_eq!(parse_wikilink("[[note::base]]").unwrap().commit, None);
        assert_eq!(parse_wikilink("[[note#sec]]").unwrap().commit, None);
    }

    #[test]
    fn colon_after_a_bare_heading_is_literal_anchor_text() {
        // A `:field` never follows a `#head` — a heading is navigational, not
        // an addressable value, so it can not be a contribution source (see
        // [[type reference::au-type-system]]). The `:` is part of the heading text, not a
        // field delimiter, so a heading may legitimately contain a colon.
        let r = parse_wikilink("[[note#section:field-name]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor.as_deref(), Some("section:field-name"));
        assert_eq!(r.block_id, None);
        assert_eq!(r.field, None);
    }

    #[test]
    fn heading_containing_a_colon_is_not_mis_split() {
        // The original motivation: a heading literally named "My: Heading"
        // must anchor whole, not split into anchor "My" + field "Heading".
        let r = parse_wikilink("[[note#My: Heading]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor.as_deref(), Some("My: Heading"));
        assert_eq!(r.field, None);
    }

    #[test]
    fn field_follows_a_block_id_even_after_a_colon_bearing_heading() {
        // A `:field` is valid after a `^block-id` (the block is addressable),
        // and a colon in the heading before the block stays in the anchor.
        let r = parse_wikilink("[[note#My: Sec^rec-1:contributesTo]]").unwrap();
        assert_eq!(r.anchor.as_deref(), Some("My: Sec"));
        assert_eq!(r.block_id_str(), Some("rec-1"));
        assert_eq!(r.field.as_deref(), Some("contributesTo"));
    }

    #[test]
    fn parses_target_block_id_field_no_anchor() {
        let r = parse_wikilink("[[note^id:field]]").unwrap();
        assert_eq!(r.target, "note");
        assert_eq!(r.anchor, None);
        assert_eq!(r.block_id_str(), Some("id"));
        assert_eq!(r.field.as_deref(), Some("field"));
    }

    #[test]
    fn field_before_anchor_is_out_of_order() {
        // `:field` placed before `#anchor` violates the strict order.
        assert_eq!(
            parse_wikilink("[[note:field#anchor]]"),
            Err(WikilinkParseError::FieldOutOfOrder)
        );
    }

    #[test]
    fn field_before_block_id_is_out_of_order() {
        assert_eq!(
            parse_wikilink("[[note:field^id]]"),
            Err(WikilinkParseError::FieldOutOfOrder)
        );
    }

    #[test]
    fn empty_field_value_is_error() {
        assert_eq!(
            parse_wikilink("[[note:]]"),
            Err(WikilinkParseError::EmptyField)
        );
    }

    #[test]
    fn empty_field_target_only_field_is_empty_target() {
        // `[[:field]]` has empty target before the `:field` fragment.
        assert_eq!(
            parse_wikilink("[[:field]]"),
            Err(WikilinkParseError::EmptyTarget)
        );
    }

    #[test]
    fn invalid_field_name_rejected() {
        // Field names must follow the [[type-def legal names::au-type-system]] type-name regex.
        assert_eq!(
            parse_wikilink("[[note:1bad]]"),
            Err(WikilinkParseError::InvalidFieldName)
        );
        assert_eq!(
            parse_wikilink("[[note:bad*name]]"),
            Err(WikilinkParseError::InvalidFieldName)
        );
        assert_eq!(
            parse_wikilink("[[note:bad name]]"),
            Err(WikilinkParseError::InvalidFieldName)
        );
    }

    #[test]
    fn field_name_with_dot_and_underscore_accepted() {
        // Mirrors sealed-leaf naming like `decision.decided`.
        let r = parse_wikilink("[[note:decision.decided]]").unwrap();
        assert_eq!(r.field.as_deref(), Some("decision.decided"));

        let r = parse_wikilink("[[note:field_underscored]]").unwrap();
        assert_eq!(r.field.as_deref(), Some("field_underscored"));
    }

    #[test]
    fn no_field_fragment_keeps_field_none() {
        let r = parse_wikilink("[[bare-target]]").unwrap();
        assert_eq!(r.field, None);
    }

    #[test]
    fn empty_inner_is_distinct_from_not_a_wikilink() {
        // The frame is correct but content is missing — distinct from
        // "didn't even look like a wikilink".
        assert_eq!(parse_wikilink("[[]]"), Err(WikilinkParseError::EmptyInner));
        assert_eq!(parse_wikilink("[[ ]]"), Err(WikilinkParseError::EmptyInner));
    }

    #[test]
    fn target_only_anchor_is_local_form() {
        // `[[#section]]` — a heading in the current file, navigational.
        // Same empty-name grant as the block-id local form.
        let r = parse_wikilink("[[#section]]").unwrap();
        assert!(r.is_local());
        assert_eq!(r.target, "");
        assert_eq!(r.anchor.as_deref(), Some("section"));
        assert_eq!(r.block_id, None);
    }

    #[test]
    fn local_form_with_empty_anchor_is_still_empty_anchor() {
        // `[[#]]` does not become a local form — the anchor value is
        // required, the empty-name grant rides on it.
        assert_eq!(
            parse_wikilink("[[#]]"),
            Err(WikilinkParseError::EmptyAnchor)
        );
    }

    #[test]
    fn target_only_block_id_is_local_form() {
        // `[[^block]]` is the local form per [[type reference::au-type-system]] — empty
        // name means the current file.
        let r = parse_wikilink("[[^block]]").unwrap();
        assert!(r.is_local());
        assert_eq!(r.target, "");
        assert_eq!(r.block_id_str(), Some("block"));
        assert_eq!(r.anchor, None);
        assert_eq!(r.field, None);
    }

    #[test]
    fn local_form_with_field_fragment() {
        // `[[^id:field]]` — a local body contribution.
        let r = parse_wikilink("[[^id:field]]").unwrap();
        assert!(r.is_local());
        assert_eq!(r.block_id_str(), Some("id"));
        assert_eq!(r.field.as_deref(), Some("field"));
    }

    #[test]
    fn local_form_with_empty_block_id_is_still_empty_block_id() {
        // `[[^]]` does not become a local form — the block-id value is
        // required, the empty-name grant rides on it.
        assert_eq!(
            parse_wikilink("[[^]]"),
            Err(WikilinkParseError::EmptyBlockId)
        );
    }

    #[test]
    fn named_target_is_not_local() {
        let r = parse_wikilink("[[note^block]]").unwrap();
        assert!(!r.is_local());
    }

    // ----- resolve_anchor ([[type reference::au-type-system]]) -----

    #[test]
    fn anchor_matches_heading_text_case_insensitively() {
        let events = au_parser::scan_body("# My Section\n\nprose\n");
        let span = resolve_anchor(&events, "my section").expect("matched");
        assert!(span.end > span.start);
        assert!(resolve_anchor(&events, "MY SECTION").is_some());
        assert!(resolve_anchor(&events, "No Such Heading").is_none());
    }

    #[test]
    fn anchor_ignores_a_headings_trailing_marker() {
        // The scanner strips `^id` from heading text, so the anchor
        // matches the clean text, never the marker-bearing form.
        let events = au_parser::scan_body("# My Section ^sec-1\n");
        assert!(resolve_anchor(&events, "My Section").is_some());
        assert!(resolve_anchor(&events, "My Section ^sec-1").is_none());
    }

    #[test]
    fn anchor_first_match_in_document_order_wins() {
        let src = "# Twice\n\nfirst\n\n# Twice\n\nsecond\n";
        let events = au_parser::scan_body(src);
        let span = resolve_anchor(&events, "Twice").expect("matched");
        assert_eq!(span.start, 0, "the first occurrence wins");
    }

    // ----- looks_like_wikilink -----

    #[test]
    fn looks_like_wikilink_accepts_bracketed_content() {
        // Used by callers (au-core's [[type reference::au-type-system]] precheck) that need to route any
        // `[[…]]`-shaped string to wikilink-handling code paths — even
        // malformed forms — so the user sees a precise parse error rather
        // than the generic "doesn't match shape".
        assert!(looks_like_wikilink("[[note]]"));
        assert!(looks_like_wikilink("[[note^]]")); // malformed but shaped
        assert!(looks_like_wikilink("[[#section]]")); // local form
        assert!(looks_like_wikilink("  [[note]]  ")); // trimmable whitespace
    }

    #[test]
    fn looks_like_wikilink_rejects_unshaped_strings() {
        assert!(!looks_like_wikilink(""));
        assert!(!looks_like_wikilink("note"));
        assert!(!looks_like_wikilink("[note]"));
        assert!(!looks_like_wikilink("[[note]"));
        assert!(!looks_like_wikilink("[[]]")); // no inner content
    }

    // ----- RepoIndex resolution -----

    fn repo_index(root: &str, files: &[&str]) -> RepoIndex {
        let root = PathBuf::from(root);
        let files: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        RepoIndex::build(root, files).0
    }

    #[test]
    fn insert_matches_a_from_scratch_build() {
        let mut index = repo_index("/v", &["notes/a.md", "notes/b.md"]);
        assert!(!index.insert(PathBuf::from("/v/notes/c.md")));
        let full = repo_index("/v", &["notes/a.md", "notes/b.md", "notes/c.md"]);
        assert_eq!(index, full);
    }

    #[test]
    fn remove_matches_a_from_scratch_build() {
        let mut index = repo_index("/v", &["notes/a.md", "notes/b.md", "notes/c.md"]);
        assert!(!index.remove(Path::new("/v/notes/b.md")));
        let full = repo_index("/v", &["notes/a.md", "notes/c.md"]);
        assert_eq!(index, full);
    }

    #[test]
    fn same_basename_in_another_dir_is_a_clean_insert() {
        // The same exact basename in a different directory is not a
        // case-collision, so the insert stays on the fast path.
        let mut index = repo_index("/v", &["a/note.md"]);
        assert!(!index.insert(PathBuf::from("/v/b/note.md")));
        let full = repo_index("/v", &["a/note.md", "b/note.md"]);
        assert_eq!(index, full);
    }

    #[test]
    fn insert_signals_a_case_collision() {
        let mut index = repo_index("/v", &["note.md"]);
        assert!(index.insert(PathBuf::from("/v/Note.md")));
    }

    #[test]
    fn remove_signals_a_case_collision() {
        let mut index = repo_index("/v", &["note.md", "Note.md"]);
        assert!(index.remove(Path::new("/v/Note.md")));
    }

    #[test]
    fn resolves_basename_unique() {
        let v = repo_index("/v", &["notes/foo.md", "decisions/bar.md"]);
        let r = v.resolve("foo.md").unwrap();
        assert_eq!(r, PathBuf::from("/v/notes/foo.md"));
    }

    #[test]
    fn resolves_basename_case_insensitive() {
        let v = repo_index("/v", &["notes/Foo.md"]);
        let r = v.resolve("foo.md").unwrap();
        assert_eq!(r, PathBuf::from("/v/notes/Foo.md"));
    }

    #[test]
    fn resolves_extensionless_basename_with_md_fallback() {
        let v = repo_index("/v", &["notes/foo.md"]);
        let r = v.resolve("foo").unwrap();
        assert_eq!(r, PathBuf::from("/v/notes/foo.md"));
    }

    #[test]
    fn resolves_extensionless_target_to_any_stem() {
        // The stem rule covers every file kind, not only `.md`: a nested
        // yaml instance and an asset both resolve by bare name.
        let v = repo_index("/v", &["ops/2026/s-001.yaml", "media/photo.png"]);
        assert_eq!(
            v.resolve("s-001").unwrap(),
            PathBuf::from("/v/ops/2026/s-001.yaml")
        );
        assert_eq!(
            v.resolve("photo").unwrap(),
            PathBuf::from("/v/media/photo.png")
        );
    }

    #[test]
    fn stem_collision_is_ambiguous_extension_disambiguates() {
        // `note.md` and `note.yaml` share the stem: the bare target is
        // ambiguous, the extension picks one.
        let v = repo_index("/v", &["a/note.md", "b/note.yaml"]);
        let Err(ResolutionError::Ambiguous(matches)) = v.resolve("note") else {
            panic!("same-stem files are ambiguous for a bare target");
        };
        assert_eq!(matches.len(), 2);
        assert_eq!(
            v.resolve("note.yaml").unwrap(),
            PathBuf::from("/v/b/note.yaml")
        );
        assert_eq!(v.resolve("note.md").unwrap(), PathBuf::from("/v/a/note.md"));
    }

    #[test]
    fn relpath_extensionless_target_matches_any_stem() {
        let v = repo_index("/v", &["ops/2026/s-001.yaml"]);
        assert_eq!(
            v.resolve("ops/2026/s-001").unwrap(),
            PathBuf::from("/v/ops/2026/s-001.yaml")
        );
    }

    #[test]
    fn resolves_type_def_by_its_type_name() {
        // A `*.type.yaml` file's stem carries the `.type` tail, but its
        // identity is the type-NAME. A def-ref `[[mcp.tool.propose]]` resolves
        // by the name. [[type-def shape def-ref::au-type-system]].
        let v = repo_index("/v", &["type/mcp.tool.propose.type.yaml"]);
        assert_eq!(
            v.resolve("mcp.tool.propose").unwrap(),
            PathBuf::from("/v/type/mcp.tool.propose.type.yaml")
        );
        // The explicit relpath form still resolves too.
        assert_eq!(
            v.resolve("type/mcp.tool.propose.type.yaml").unwrap(),
            PathBuf::from("/v/type/mcp.tool.propose.type.yaml")
        );
    }

    #[test]
    fn resolves_dotted_basename_by_its_bare_name() {
        // A plain file whose basename carries interior dots resolves by that
        // dotted name. The final `.md` is the extension the stem strips, so the
        // stem is the whole dotted name and the bare target reaches it. Dots in
        // the target are not read as an extension boundary. [[type reference::au-type-system]].
        let v = repo_index("/v", &["notes/my.file.name.md"]);
        assert_eq!(
            v.resolve("my.file.name").unwrap(),
            PathBuf::from("/v/notes/my.file.name.md")
        );
        // The explicit basename form resolves too.
        assert_eq!(
            v.resolve("my.file.name.md").unwrap(),
            PathBuf::from("/v/notes/my.file.name.md")
        );
    }

    #[test]
    fn resolves_dotted_relpath_by_its_bare_name() {
        let v = repo_index("/v", &["notes/my.file.name.md"]);
        assert_eq!(
            v.resolve("notes/my.file.name").unwrap(),
            PathBuf::from("/v/notes/my.file.name.md")
        );
    }

    #[test]
    fn wrong_extension_target_still_misses() {
        // A dotted target only matches a file whose stem equals the whole
        // string. A genuine wrong-extension link finds no such stem and stays
        // Missing, so ungating the stem lookup adds no false positive.
        let v = repo_index("/v", &["guide.md"]);
        assert_eq!(v.resolve("guide.pdf"), Err(ResolutionError::Missing));
        assert_eq!(v.resolve("dir/guide.pdf"), Err(ResolutionError::Missing));
    }

    #[test]
    fn exact_basename_wins_over_a_stem_sibling() {
        // `my-file.yaml` (exact basename) and `my-file.yaml.md` (stem
        // `my-file.yaml`) both match the target. Exact basename is definitive,
        // so it wins outright — not an ambiguity, no `reference-target-ambiguous`.
        let v = repo_index("/v", &["a/my-file.yaml", "b/my-file.yaml.md"]);
        assert_eq!(
            v.resolve("my-file.yaml").unwrap(),
            PathBuf::from("/v/a/my-file.yaml")
        );
        // The `.md` sibling is still reachable by its own exact basename.
        assert_eq!(
            v.resolve("my-file.yaml.md").unwrap(),
            PathBuf::from("/v/b/my-file.yaml.md")
        );
    }

    #[test]
    fn ambiguous_basename_returns_all_matches() {
        let v = repo_index("/v", &["a/foo.md", "b/foo.md"]);
        let err = v.resolve("foo.md").unwrap_err();
        match err {
            ResolutionError::Ambiguous(ps) => {
                assert_eq!(ps.len(), 2);
            }
            _ => panic!("expected Ambiguous, got {:?}", err),
        }
    }

    #[test]
    fn missing_basename_returns_missing() {
        let v = repo_index("/v", &["a/foo.md"]);
        assert_eq!(
            v.resolve("missing.md").unwrap_err(),
            ResolutionError::Missing
        );
    }

    #[test]
    fn resolves_relpath_with_slash() {
        let v = repo_index("/v", &["notes/foo.md", "notes/bar.md"]);
        let r = v.resolve("notes/foo.md").unwrap();
        assert_eq!(r, PathBuf::from("/v/notes/foo.md"));
    }

    #[test]
    fn resolves_extensionless_relpath_with_md_fallback() {
        let v = repo_index("/v", &["notes/foo.md"]);
        let r = v.resolve("notes/foo").unwrap();
        assert_eq!(r, PathBuf::from("/v/notes/foo.md"));
    }

    #[test]
    fn missing_relpath_returns_missing_not_basename_match() {
        // Even if the basename matches elsewhere, a `/`-bearing target
        // resolves strictly through relpath.
        let v = repo_index("/v", &["a/foo.md"]);
        assert_eq!(v.resolve("b/foo.md").unwrap_err(), ResolutionError::Missing);
    }

    // ----- case-collision-basename -----

    #[test]
    fn case_collision_emitted_for_mixed_case_pair() {
        let root = PathBuf::from("/v");
        let files = vec![root.join("a/Foo.md"), root.join("b/foo.md")];
        let (_, diags) = RepoIndex::build(root, files);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.code.as_str() == "case-collision-basename")
                .count(),
            1
        );
    }

    #[test]
    fn no_case_collision_for_identical_basenames() {
        // Two `foo.md` files in different dirs is normal (ambiguity at resolve time).
        let root = PathBuf::from("/v");
        let files = vec![root.join("a/foo.md"), root.join("b/foo.md")];
        let (_, diags) = RepoIndex::build(root, files);
        assert!(diags
            .iter()
            .all(|d| d.code.as_str() != "case-collision-basename"));
    }

    #[test]
    fn case_collision_diagnostic_carries_related_span() {
        let root = PathBuf::from("/v");
        let files = vec![root.join("a/Foo.md"), root.join("b/foo.md")];
        let (_, diags) = RepoIndex::build(root, files);
        let collision = diags
            .iter()
            .find(|d| d.code.as_str() == "case-collision-basename")
            .unwrap();
        assert_eq!(collision.related.len(), 1);
        assert_eq!(collision.severity, Severity::Warning);
    }

    #[test]
    fn build_ignores_files_outside_root() {
        let root = PathBuf::from("/v");
        let files = vec![root.join("note.md"), PathBuf::from("/other/foo.md")];
        let (idx, _) = RepoIndex::build(root, files);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn empty_index_is_empty() {
        let v = repo_index("/v", &[]);
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn root_is_returned() {
        let v = repo_index("/some/path", &[]);
        assert_eq!(v.root(), Path::new("/some/path"));
    }

    // ----- resolve_block_id ([[type block-id::au-type-system]]) -----

    #[test]
    fn resolves_typed_block_by_id() {
        let src = "Some prose.\n\n```yaml [:assumptions]\ntype: assumption\ndescription: x\n```\n^pdf-pagination\n";
        let events = au_parser::scan_body(src);
        let resolved = resolve_block_id(&events, "pdf-pagination").unwrap();
        assert_eq!(resolved.info, "yaml [:assumptions]");
        assert!(resolved.body.contains("type: assumption"));
    }

    #[test]
    fn missing_block_id_is_not_found() {
        let src = "Prose only, no fenced blocks.\n";
        let events = au_parser::scan_body(src);
        assert_eq!(
            resolve_block_id(&events, "anything"),
            Err(BlockResolutionError::NotFound)
        );
    }

    #[test]
    fn untyped_fence_with_matching_id_is_not_typed() {
        let src = "```yaml\nplain: data\n```\n^plain-block\n";
        let events = au_parser::scan_body(src);
        assert_eq!(
            resolve_block_id(&events, "plain-block"),
            Err(BlockResolutionError::NotTyped)
        );
    }

    #[test]
    fn standalone_marker_without_fence_is_not_typed() {
        // The marker exists in the file but isn't attached to a typed block.
        let src = "Paragraph text.\n^orphan-marker\n";
        let events = au_parser::scan_body(src);
        assert_eq!(
            resolve_block_id(&events, "orphan-marker"),
            Err(BlockResolutionError::NotTyped)
        );
    }

    #[test]
    fn extract_field_marker_picks_out_name() {
        assert_eq!(
            extract_field_marker("yaml [:assumptions]"),
            Some("assumptions")
        );
        assert_eq!(extract_field_marker("yaml"), None);
        assert_eq!(extract_field_marker(""), None);
        assert_eq!(extract_field_marker("yaml [:1bad]"), None); // invalid name
    }
}
