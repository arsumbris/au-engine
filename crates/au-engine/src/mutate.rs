//! The mutation channel's execution layer per
//! [[spec - mutation channel v1 - a closed primitive catalog through one mediated path]].
//!
//! Raw filesystem writes live here, below the wire — the serve layer parses
//! the `mutate` verb and calls in; nothing re-exposes these primitives.
//! Rejection is for malformed requests only (path escape, hash mismatch,
//! missing `old_string`); a mutation never rejects because the result has
//! validation errors — diagnostics stay advisory.

use std::path::{Component, Path, PathBuf};

use au_core::{
    locate_field_path, parse_instance, Instance, InstanceValue, Located, PathSegment, TypeClaim,
    TypeNameClaim, ValueKind,
};
use au_diagnostics::ByteRange;
use serde_json::{Map, Value};

use crate::ir::ContentHash;
use crate::repo::RepoMap;

/// A rejected mutation: nothing was written. The message is the wire `error`
/// text; `detail` carries machine-usable context (e.g. the current hash on a
/// staleness conflict).
#[derive(Debug)]
pub(crate) struct MutationReject {
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl MutationReject {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        MutationReject {
            message: message.into(),
            detail: None,
        }
    }
}

/// The hex form a content hash travels in on the wire.
pub(crate) fn hash_hex(hash: ContentHash) -> String {
    format!("{:016x}", hash.0)
}

/// Resolve a mutation's path argument to a normalized absolute path and enforce
/// the channel's path rules PER MEMBER: a write may reach any declared workspace
/// member (the same reach reads have, scattered or subdir), but not escape every
/// member, and not write under a member's `.arsumbris/` (the cache is the engine's).
///
/// An absolute arg resolves against its owning member directly; a relative arg joins
/// `fallback_root` (the workspace root) first, preserving single-repo behaviour. The
/// owning member is the deepest declared repo root containing the normalized path; a
/// path under no member is the real "escapes the workspace". Routing keys on the final
/// normalized path, so a `..` that crosses out of one member into another lands in the
/// member that owns the result, not a reject — the host passes clean in-member paths.
pub(crate) fn resolve_repo_path(
    repos: &RepoMap,
    fallback_root: &Path,
    arg: &str,
) -> Result<PathBuf, MutationReject> {
    let joined = if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        fallback_root.join(arg)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(MutationReject::new(format!(
                        "path escapes the workspace: {arg}"
                    )));
                }
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    // The owning member: the deepest declared repo root containing the path.
    // Reads reach members this same way, so writes are symmetric; a path under
    // no member is the real escape.
    let Some(member) = repos.repo_of(&normalized) else {
        return Err(MutationReject::new(format!(
            "path escapes the workspace: {arg}"
        )));
    };
    let relative = normalized
        .strip_prefix(&member.root)
        .expect("repo_of guarantees the member root is a prefix");
    if relative.as_os_str().is_empty() {
        return Err(MutationReject::new("path names a member root, not a file"));
    }
    if relative.components().next() == Some(Component::Normal(std::ffi::OsStr::new(".arsumbris"))) {
        return Err(MutationReject::new(
            "writes under .arsumbris/ are rejected — the cache is the engine's",
        ));
    }
    Ok(normalized)
}

/// `write_file`: full content, parents created. The optional `expected_hash`
/// is the read-before-write guard — the hash of the content the caller last
/// read; a mismatch rejects without writing and reports the current hash.
pub(crate) fn write_file(
    target: &Path,
    content: &str,
    expected_hash: Option<&str>,
) -> Result<ContentHash, MutationReject> {
    if let Some(expected) = expected_hash {
        match std::fs::read(target) {
            Ok(current) => {
                let current_hex = hash_hex(ContentHash::of(&current));
                if current_hex != expected {
                    return Err(MutationReject {
                        message: format!(
                            "expected_hash mismatch: the file changed since it was read \
                             (current {current_hex})"
                        ),
                        detail: Some(serde_json::json!({ "current_hash": current_hex })),
                    });
                }
            }
            Err(_) => {
                return Err(MutationReject::new(
                    "expected_hash given but the file does not exist",
                ));
            }
        }
    }
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(MutationReject::new(format!(
                "cannot create parent directories: {e}"
            )));
        }
    }
    std::fs::write(target, content)
        .map_err(|e| MutationReject::new(format!("cannot write {}: {e}", target.display())))?;
    // The hash of what landed on disk, so the caller can tell whether the
    // rebuilt knowledge base has caught up to this write.
    Ok(ContentHash::of(content.as_bytes()))
}

/// `edit_file`: exact string replacement. `old_string` must match exactly
/// and be unique; `replace_all` lifts uniqueness. The exact match is its own
/// read-before-write precondition — a changed file misses it, so no hash
/// arg exists.
pub(crate) fn edit_file(
    target: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<ContentHash, MutationReject> {
    let content = std::fs::read_to_string(target)
        .map_err(|e| MutationReject::new(format!("cannot read {}: {e}", target.display())))?;
    let updated = compute_edit(&content, old_string, new_string, replace_all)?;
    let written = ContentHash::of(updated.as_bytes());
    std::fs::write(target, updated)
        .map_err(|e| MutationReject::new(format!("cannot write {}: {e}", target.display())))?;
    Ok(written)
}

/// The pure content transform behind [`edit_file`]: apply an exact string
/// replacement to `content`, enforcing the empty / identical / not-found /
/// not-unique guards. [`edit_file`] reads disk, calls this, then writes; the
/// preview read ([`preview_content`]) calls it over the snapshot's bytes, so the
/// two paths cannot diverge on what an edit produces.
pub(crate) fn compute_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, MutationReject> {
    if old_string.is_empty() {
        return Err(MutationReject::new("old_string is empty"));
    }
    if old_string == new_string {
        return Err(MutationReject::new(
            "old_string and new_string are identical",
        ));
    }
    let occurrences = content.matches(old_string).count();
    if occurrences == 0 {
        return Err(MutationReject::new(
            "old_string not found in the file — re-read and retry with current content",
        ));
    }
    if occurrences > 1 && !replace_all {
        return Err(MutationReject {
            message: format!(
                "old_string occurs {occurrences} times, must be unique — add surrounding \
                 context to disambiguate, or pass replace_all"
            ),
            detail: Some(serde_json::json!({ "occurrences": occurrences })),
        });
    }
    Ok(if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    })
}

/// A deterministic mutation to simulate, the input to [`preview_content`]. The
/// v1 preview ops, mirroring the write path's `write_file` / `edit_file` /
/// `delete_file`.
pub(crate) enum PreviewOp {
    Write {
        content: String,
    },
    Edit {
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    Delete,
}

/// One stamp to fold into a preview's would-be content, the preview-side mirror
/// of the write path's `Stamp`. Field, record, and match key are opaque to the
/// engine, spliced by [`splice_ensure_stamp`].
pub(crate) struct PreviewStamp {
    pub field: String,
    pub record: Value,
    pub match_on: Option<Map<String, Value>>,
}

/// Fold a stamp list into a preview's would-be content, purely, the same rider
/// [`fold_stamp`] applies on disk after a real write. A dedup no-op leaves the
/// content unchanged; an `Applied` splice carries forward. A structural refusal
/// (a non-list stamp field) propagates as a [`MutationReject`], so a preview
/// reports the reject the real write would.
pub(crate) fn fold_stamps_content(
    mut content: String,
    path: &Path,
    stamps: &[PreviewStamp],
) -> Result<String, MutationReject> {
    for s in stamps {
        if let StampOutcome::Applied(next) =
            splice_ensure_stamp(&content, path, &s.field, &s.record, s.match_on.as_ref())?
        {
            content = next;
        }
    }
    Ok(content)
}

/// Compute the WOULD-BE content of a deterministic mutation, without touching
/// disk. `current` is the target's present bytes, which the caller reads (the
/// same read the real mutation does); `None` means the target does not exist.
///
/// Returns `Some(content)` for a write or edit, `None` for a delete (a removal),
/// or a [`MutationReject`] for an op that structurally refuses (an edit or delete
/// of an absent file, an edit whose `old_string` is absent or not unique). The
/// pure half the preview read shares with the disk-writing mutation, so a preview
/// cannot diverge from what the write would land. Stamp folding, when requested,
/// is applied by the caller over this result via [`splice_ensure_stamp`], the
/// same rider the write path folds in.
pub(crate) fn preview_content(
    op: &PreviewOp,
    current: Option<&[u8]>,
) -> Result<Option<String>, MutationReject> {
    match op {
        // A write overwrites or creates, so the present bytes do not matter.
        PreviewOp::Write { content } => Ok(Some(content.clone())),
        PreviewOp::Edit {
            old_string,
            new_string,
            replace_all,
        } => {
            let bytes = current.ok_or_else(|| {
                MutationReject::new("cannot edit: the file does not exist — nothing to edit")
            })?;
            let content = std::str::from_utf8(bytes)
                .map_err(|_| MutationReject::new("the file is not valid UTF-8"))?;
            Ok(Some(compute_edit(
                content,
                old_string,
                new_string,
                *replace_all,
            )?))
        }
        // A delete of an absent file rejects, mirroring `delete_file`.
        PreviewOp::Delete => {
            current
                .ok_or_else(|| MutationReject::new("file does not exist — nothing to delete"))?;
            Ok(None)
        }
    }
}

/// `delete_file`: remove a file. The optional `expected_hash` is the
/// read-before-write guard — the hash of the content the caller last read; a
/// mismatch rejects without deleting and reports the current hash. Deleting an
/// absent file rejects rather than succeeding silently — a silent drop would
/// hide a caller working from a stale view.
pub(crate) fn delete_file(
    target: &Path,
    expected_hash: Option<&str>,
) -> Result<(), MutationReject> {
    let current = match std::fs::read(target) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(MutationReject::new(
                "file does not exist — nothing to delete",
            ));
        }
    };
    if let Some(expected) = expected_hash {
        let current_hex = hash_hex(ContentHash::of(&current));
        if current_hex != expected {
            return Err(MutationReject {
                message: format!(
                    "expected_hash mismatch: the file changed since it was read \
                     (current {current_hex})"
                ),
                detail: Some(serde_json::json!({ "current_hash": current_hex })),
            });
        }
    }
    std::fs::remove_file(target)
        .map_err(|e| MutationReject::new(format!("cannot delete {}: {e}", target.display())))?;
    Ok(())
}

/// `rename`: move `from` to `to`, returning the hash of the moved content so the
/// caller can tell whether the rebuilt knowledge base has caught up. `from` must exist,
/// `to` must not, and `to` must not collide case-insensitively with any existing
/// sibling — so a rename never clobbers, and a name that would clash on a
/// case-insensitive filesystem is rejected on every filesystem. The case-only
/// rename (`a.md` to `A.md`) is rejected too, since the source itself is a
/// colliding sibling; v1 has no need for it.
pub(crate) fn rename_file(from: &Path, to: &Path) -> Result<ContentHash, MutationReject> {
    let content = std::fs::read(from)
        .map_err(|_| MutationReject::new("path does not exist — nothing to rename"))?;
    if to.exists() {
        return Err(MutationReject::new(format!(
            "destination already exists: {}",
            to.display()
        )));
    }
    if let (Some(parent), Some(name)) = (to.parent(), to.file_name()) {
        let wanted = name.to_string_lossy().to_lowercase();
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().to_lowercase() == wanted {
                    return Err(MutationReject::new(format!(
                        "destination collides case-insensitively with an existing file: {}",
                        entry.file_name().to_string_lossy()
                    )));
                }
            }
        }
        std::fs::create_dir_all(parent)
            .map_err(|e| MutationReject::new(format!("cannot create parent directories: {e}")))?;
    }
    std::fs::rename(from, to).map_err(|e| {
        MutationReject::new(format!(
            "cannot rename {} to {}: {e}",
            from.display(),
            to.display()
        ))
    })?;
    Ok(ContentHash::of(&content))
}

/// Generate a block id for `assign_block_id`: `b-<unix-millis base36>`,
/// suffixed with a counter when `taken` says the id already appears.
/// `b-` distinguishes engine-assigned ids; the validity alphabet is
/// [[type-def legal names::au-type-system]]'s block-id charset.
pub(crate) fn generate_block_id(taken: impl Fn(&str) -> bool) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64;
    bump_until_free(format!("b-{}", base36(millis)), taken)
}

/// Bump a base id until it is free: the base itself, then `base-1`, `base-2`,
/// … until `taken` returns false. Split from the clock-reading entry so the
/// bump is deterministic and testable against a fixed base.
fn bump_until_free(base: String, taken: impl Fn(&str) -> bool) -> String {
    let mut candidate = base.clone();
    let mut counter: u64 = 1;
    while taken(&candidate) {
        candidate = format!("{base}-{}", base36(counter));
        counter += 1;
    }
    candidate
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 is ascii")
}

/// A correlation id for one mutation: `m-<base36 millis>-<base36 counter>`.
/// Stamped into every commit of the mutation as a `Mutation-Id` trailer. The
/// process-wide counter disambiguates ids minted in the same millisecond.
pub(crate) fn generate_mutation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("m-{}-{}", base36(millis), base36(counter))
}

/// Insert `^: <id>` as the first key of the inline record whose span starts
/// at `record_start` in `content`. The id rides the record's first-key line;
/// following keys keep their indentation, matching the hand-authored shape:
///
/// ```yaml
/// - ^: b-xyz
///   type: sessionEvent
/// ```
pub(crate) fn insert_record_id(
    content: &str,
    record_start: usize,
    id: &str,
) -> Result<String, MutationReject> {
    // The offset is sliced below; a non-char-boundary value would panic.
    // `record_start` comes from the held parse so this holds in practice,
    // but the guard keeps the slice total against any drift.
    if !content.is_char_boundary(record_start) {
        return Err(MutationReject::new(
            "offset does not fall on a character boundary",
        ));
    }
    if content.as_bytes().get(record_start) == Some(&b'{') {
        return Err(MutationReject::new(
            "the record is flow-style ({ .. }); v1 assigns ids to block-style records only",
        ));
    }
    let line_start = content[..record_start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent = record_start - line_start;
    let mut updated = String::with_capacity(content.len() + id.len() + indent + 8);
    updated.push_str(&content[..record_start]);
    updated.push_str("^: ");
    updated.push_str(id);
    updated.push('\n');
    updated.push_str(&" ".repeat(indent));
    updated.push_str(&content[record_start..]);
    Ok(updated)
}

/// Append a ` ^<id>` marker to the end of the markdown block containing
/// `at`: the last non-blank line before the next blank line (or EOF) gains
/// the trailing marker, per [[type block-id::au-type-system]]'s bare-marker form.
pub(crate) fn insert_markdown_marker(
    content: &str,
    at: usize,
    id: &str,
) -> Result<String, MutationReject> {
    if at >= content.len() {
        return Err(MutationReject::new("offset is past the end of the file"));
    }
    // `at` arrives from the wire and is sliced below. A value mid-way through
    // a multi-byte UTF-8 character would panic the slice, so reject it as a
    // clean mutation error rather than crashing the connection task.
    if !content.is_char_boundary(at) {
        return Err(MutationReject::new(
            "offset does not fall on a character boundary",
        ));
    }
    // The block ends at the next blank line; the marker rides the last
    // non-whitespace byte of the block.
    let block_end = content[at..]
        .find("\n\n")
        .map(|i| at + i)
        .unwrap_or(content.len());
    let insert_at = content[..block_end]
        .rfind(|c: char| !c.is_whitespace())
        .map(|i| i + content[i..].chars().next().map(char::len_utf8).unwrap_or(1))
        .ok_or_else(|| MutationReject::new("the offset lands on blank content"))?;
    let mut updated = String::with_capacity(content.len() + id.len() + 2);
    updated.push_str(&content[..insert_at]);
    updated.push_str(" ^");
    updated.push_str(id);
    updated.push_str(&content[insert_at..]);
    Ok(updated)
}

/// The `^id` a markdown block already carries, if any. Mirrors
/// `insert_markdown_marker`'s block boundary (the block runs to the next blank
/// line) so the idempotency check agrees with WHERE a marker would be written:
/// the trailing `^id` token on the block's last non-blank line. Returns `None`
/// when the block carries no marker — the markdown analogue of the inline
/// record's `existing` field, so a re-assign returns the id unwritten instead
/// of accumulating a second marker.
pub(crate) fn existing_markdown_marker(content: &str, at: usize) -> Option<String> {
    if at >= content.len() || !content.is_char_boundary(at) {
        return None;
    }
    // Same block boundary as `insert_markdown_marker`.
    let block_end = content[at..]
        .find("\n\n")
        .map(|i| at + i)
        .unwrap_or(content.len());
    // The last non-whitespace byte of the block, and the line it sits on.
    let last = content[..block_end].rfind(|c: char| !c.is_whitespace())?;
    let line_start = content[..last].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let last_line = content[line_start..block_end].trim_end();
    // A marker is the trailing whitespace-delimited `^<id>` token; a glued
    // `word^id` is prose, not a marker (its token does not start with `^`).
    let token = last_line.split_whitespace().last()?;
    let id = token.strip_prefix('^')?;
    au_parser::is_valid_block_id(id).then(|| id.to_string())
}

// ---------------------------------------------------------------------------
// Nested-record mutations: edit_record / append_record.
//
// Pure content transforms (string in, string out): parse the freshly-read
// bytes, locate the node by `field_path` via au-core, splice at the byte spans.
// Every untouched byte stays identical, so comments and doc comments survive.
// The CAS guard, the on_invalid validation, and the write live in the serve
// layer, see [[spec - nested record edits - patch a record and append to a sequence by byte-splice, comments preserved]].
// ---------------------------------------------------------------------------

/// One byte-range replacement. An insert is a zero-width edit (`start == end`).
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Apply edits high-offset first, so an earlier splice never shifts a later
/// (lower) span. Ranges are parser byte offsets, on char boundaries.
fn apply_edits(content: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by(|a, b| b.start.cmp(&a.start));
    let mut out = content.to_string();
    for e in edits {
        out.replace_range(e.start..e.end, &e.text);
    }
    out
}

/// The byte offset of the start of the line containing `pos`.
fn line_start(content: &str, pos: usize) -> usize {
    content[..pos.min(content.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// The column of `pos`: bytes from the line start. Indentation is ASCII spaces,
/// so this is the indent when `pos` is the first non-space on its line.
fn col(content: &str, pos: usize) -> usize {
    pos - line_start(content, pos)
}

/// The leading-space count of the line containing `pos`.
fn line_indent(content: &str, pos: usize) -> usize {
    let ls = line_start(content, pos);
    content[ls..]
        .find(|c: char| c != ' ')
        .unwrap_or(content.len() - ls)
}

/// Trim trailing whitespace and newlines back to the last content byte. A
/// block-collection span can over-extend to the end of its enclosing document
/// (past the real content, e.g. up to a frontmatter close), so an insert anchor
/// derived from it must first walk back to the last authored byte.
fn trim_trailing_ws(content: &str, end: usize) -> usize {
    let b = content.as_bytes();
    let mut e = end.min(content.len());
    while e > 0 && matches!(b[e - 1], b'\n' | b'\r' | b' ' | b'\t') {
        e -= 1;
    }
    e
}

/// The byte offset just past the newline that ends `pos`'s line, or the content
/// length when that line is unterminated (EOF with no trailing newline).
fn after_line(content: &str, pos: usize) -> usize {
    let from = pos.min(content.len());
    match content[from..].find('\n') {
        Some(rel) => from + rel + 1,
        None => content.len(),
    }
}

/// A splice must never emit content that no longer parses as its instance, the
/// never-broken invariant. A re-parse failure is an internal splice bug, so it
/// rejects loudly, nothing written, rather than landing broken YAML.
fn structural_check(path: &Path, candidate: &str) -> Result<(), MutationReject> {
    parse_instance_for_mutation(path, candidate).map(|_| ()).map_err(|_| {
        MutationReject::new(
            "the edit produced content that no longer parses as a typed record — internal splice error, nothing written",
        )
    })
}

/// Parse the instance from a file's current bytes, for a mutation locate. The
/// spans come out file-absolute (the frontmatter offset is folded in), so a
/// splice writes at the right place. `Err` when the file is not a typed record.
fn parse_instance_for_mutation(path: &Path, content: &str) -> Result<Instance, MutationReject> {
    let split = match au_parser::split_frontmatter(content) {
        Ok(Some(s)) => s,
        Ok(None) => au_parser::whole_as_frontmatter(content),
        Err(_) => {
            return Err(MutationReject::new(
                "frontmatter opens with '---' but never closes",
            ))
        }
    };
    let offset = split.frontmatter_range.start;
    let docs = au_parser::yaml::parse(split.frontmatter)
        .map_err(|_| MutationReject::new("file frontmatter is not valid YAML"))?;
    let doc = docs
        .first()
        .ok_or_else(|| MutationReject::new("file has no YAML document"))?;
    au_core::parse_instance(path, content, offset, doc)
        .instance
        .ok_or_else(|| MutationReject::new("target is not a typed record file (no 'type:' claim)"))
}

/// `edit_record`: patch a nested record's fields in place. Returns the new file
/// content; the caller CAS-guards, validates, and writes. `type` re-types the
/// record (a claim splice); other keys replace a scalar field in place or insert
/// an absent field at the record's indent. Every untouched byte, and every
/// comment, is preserved.
pub(crate) fn splice_edit_record(
    content: &str,
    path: &Path,
    field_path: &[PathSegment],
    patch: &Map<String, Value>,
) -> Result<String, MutationReject> {
    if patch.is_empty() {
        return Err(MutationReject::new("patch is empty"));
    }
    let instance = parse_instance_for_mutation(path, content)?;
    let located = locate_field_path(&instance, field_path).ok_or_else(|| {
        MutationReject::new(
            "field_path resolves to no record — re-read and retry with a fresh instances_of locator",
        )
    })?;
    let record = match located {
        Located::Record(r) => r,
        Located::Sequence(_) => {
            return Err(MutationReject::new(
                "field_path addresses a sequence, not a record — use append_record",
            ))
        }
    };
    if content.as_bytes().get(record.span.start) == Some(&b'{') {
        return Err(MutationReject::new(
            "the record is flow-style ({ .. }); v1 edits block-style records only",
        ));
    }

    let mut edits: Vec<Edit> = Vec::new();
    let mut inserts: Vec<(&String, &Value)> = Vec::new();

    for (key, value) in patch {
        match key.as_str() {
            "^" => {
                return Err(MutationReject::new(
                    "'^' is a block-id, minted by assign_block_id, not patchable",
                ))
            }
            "type" => {
                let Some(type_span) = record.type_span else {
                    return Err(MutationReject::new(
                        "record has no explicit 'type:' claim to replace",
                    ));
                };
                let rendered =
                    crate::yaml_render::claim_value(value).map_err(MutationReject::new)?;
                edits.push(Edit {
                    start: type_span.start,
                    end: type_span.end,
                    text: rendered,
                });
            }
            _ => match record.fields.iter().find(|f| &f.key == key) {
                Some(f) => {
                    if f.kind != ValueKind::Scalar {
                        return Err(MutationReject::new(format!(
                            "field '{key}' holds a list or mapping; v1 replaces a scalar field value only"
                        )));
                    }
                    if value.is_array() || value.is_object() {
                        return Err(MutationReject::new(format!(
                            "field '{key}' patch value is a list or mapping; v1 replaces a scalar field value only"
                        )));
                    }
                    let token =
                        crate::yaml_render::scalar_token(value).map_err(MutationReject::new)?;
                    edits.push(Edit {
                        start: f.value_span.start,
                        end: f.value_span.end,
                        text: token,
                    });
                }
                None => inserts.push((key, value)),
            },
        }
    }

    if !inserts.is_empty() {
        // The field indent: the column of an existing field's key, else the
        // record's first authored line (a bare `type:`-only record).
        let indent = match record.fields.first() {
            Some(f) => col(content, f.key_span.start),
            None => col(content, record.span.start),
        };
        let mut block = String::new();
        for (key, value) in &inserts {
            block.push_str(&crate::yaml_render::field(key, value, indent));
        }
        // Insert after the record's LAST AUTHORED line, the latest content end
        // among its fields and its claim/block-id. The record's own span can
        // over-extend to the enclosing document end (past a frontmatter close),
        // so it is not the anchor.
        let last_authored = record
            .fields
            .iter()
            .map(|f| f.value_span.end)
            .chain(record.type_span.map(|s| s.end))
            .chain(record.block_id_span.map(|s| s.end))
            .max()
            .unwrap_or(record.span.start);
        let at = after_line(content, last_authored);
        let text = if at == content.len() && !content.ends_with('\n') {
            format!("\n{block}")
        } else {
            block
        };
        edits.push(Edit {
            start: at,
            end: at,
            text,
        });
    }

    let result = apply_edits(content, edits);
    structural_check(path, &result)?;
    Ok(result)
}

/// `append_record`: add one element to a sequence field. Returns the new file
/// content. Splices after the last element (or seeds an empty `[]`), leaving
/// every existing element, and its comments, byte-identical.
pub(crate) fn splice_append_record(
    content: &str,
    path: &Path,
    field_path: &[PathSegment],
    value: &Value,
) -> Result<String, MutationReject> {
    let instance = parse_instance_for_mutation(path, content)?;
    let located = locate_field_path(&instance, field_path).ok_or_else(|| {
        MutationReject::new(
            "field_path resolves to no sequence — re-read and retry with a fresh instances_of locator",
        )
    })?;
    let seq = match located {
        Located::Sequence(s) => s,
        Located::Record(_) => {
            return Err(MutationReject::new(
                "field_path addresses a record, not a sequence — use edit_record",
            ))
        }
    };

    if seq.elements.is_empty() {
        let result = seed_first_element(content, seq.span, value);
        structural_check(path, &result)?;
        return Ok(result);
    }
    // A populated flow sequence is out of v1; the empty `[]` seed above is fine.
    if content.as_bytes().get(seq.span.start) == Some(&b'[') {
        return Err(MutationReject::new(
            "the sequence is flow-style ([ .. ]); v1 appends to block-style sequences only",
        ));
    }

    let result = append_after_last_element(content, &seq.elements, value);
    structural_check(path, &result)?;
    Ok(result)
}

/// Byte-splice one element after the last of a block sequence. Every existing
/// element, and its comments, stays byte-identical. No structural check — the
/// caller runs the one that fits its context (typed record, or type-agnostic
/// frontmatter for a stamp).
fn append_after_last_element(content: &str, elements: &[ByteRange], value: &Value) -> String {
    let last = *elements.last().expect("non-empty");
    let indent = line_indent(content, last.start);
    let block = crate::yaml_render::element(value, indent);
    // The last element's span can over-extend to the document end, so trim back
    // to its last content byte before finding the line to insert after.
    let at = after_line(content, trim_trailing_ws(content, last.end));
    let text = if at == content.len() && !content.ends_with('\n') {
        format!("\n{block}")
    } else {
        block
    };
    apply_edits(
        content,
        vec![Edit {
            start: at,
            end: at,
            text,
        }],
    )
}

/// Seed the first element into an empty `key: []`: rewrite the flow-empty marker
/// as a block list, `key:\n  - <element>`. The child indent is the parent key's
/// indent plus two. `value_span` is the `[]` marker's span. No structural check —
/// the caller runs it.
fn seed_first_element(content: &str, value_span: ByteRange, value: &Value) -> String {
    let item_indent = line_indent(content, value_span.start) + 2;
    let block = crate::yaml_render::element(value, item_indent);
    // The empty marker is `[]` (a block sequence cannot be empty). The located
    // span is not reliable across both brackets, so consume the marker on the
    // bytes: the `[` / `]` and their surrounding spaces, plus the trailing
    // newline, so `key: []\n` becomes `key:\n  - x\n` with no dangling `]`.
    let bytes = content.as_bytes();
    let mut start = value_span.start;
    while start > 0 && matches!(bytes[start - 1], b' ' | b'\t' | b'[') {
        start -= 1;
    }
    let mut end = value_span.start;
    while end < bytes.len() && matches!(bytes[end], b'[' | b']' | b' ' | b'\t') {
        end += 1;
    }
    if bytes.get(end) == Some(&b'\n') {
        end += 1;
    }
    apply_edits(
        content,
        vec![Edit {
            start,
            end,
            text: format!("\n{block}"),
        }],
    )
}

/// The outcome of an idempotent stamp ensure: a dedup match wrote nothing, or the
/// new file content with the record ensured. The caller writes only on `Applied`,
/// so a no-op contributes no change to its commit.
#[derive(Debug)]
pub(crate) enum StampOutcome {
    NoOp,
    Applied(String),
}

/// `stamp`: idempotently ensure `record` in the frontmatter list-field `field` of
/// a typed record file, a comment-preserving byte-splice
/// ([[spec - stamp injection - a write rider idempotently ensures a frontmatter record folded into the write's commit]]).
///
/// - the field ABSENT: insert `field:\n  - <record>` at the frontmatter root.
/// - the field a SEQUENCE (empty `[]` or populated): with `match_on` present, if
///   some existing element carries every pair the stamp is a `NoOp`; else the
///   record is appended after the last element (existing elements stay
///   byte-identical). With `match_on` absent, always append.
/// - any other field shape (a scalar, a null anchor, a mapping) rejects: a stamp
///   ensures a record in a list-field.
///
/// The engine treats `record`, `field`, and `match_on` as OPAQUE: it splices a
/// caller-named value into a caller-named field under a caller-named key, knowing
/// none of their meaning. v1 bound: the target must parse as a typed record file
/// (a frontmatter `type:` claim), the same bound the nested-record verbs carry.
pub(crate) fn splice_ensure_stamp(
    content: &str,
    path: &Path,
    field: &str,
    record: &Value,
    match_on: Option<&Map<String, Value>>,
) -> Result<StampOutcome, MutationReject> {
    // Locate the frontmatter, type-agnostically. A stamp records a frontmatter
    // fact; it does not require a typed instance. Three entry states:
    // - `---` frontmatter present: edit it (typed or not).
    // - a pure-yaml instance (`.yaml`/`.yml`): the whole file IS the record.
    // - a markdown file with no frontmatter: GAIN one, a `---` block prepended.
    let split = match au_parser::split_frontmatter(content) {
        Ok(Some(s)) => s,
        Ok(None) => {
            if au_parser::is_pure_yaml_instance_path(path) {
                au_parser::whole_as_frontmatter(content)
            } else {
                let block =
                    crate::yaml_render::field(field, &Value::Array(vec![record.clone()]), 0);
                let new = format!("---\n{block}---\n{content}");
                stamp_structural_check(path, &new)?;
                return Ok(StampOutcome::Applied(new));
            }
        }
        Err(_) => {
            return Err(MutationReject::new(
                "frontmatter opens with '---' but never closes",
            ))
        }
    };

    let instance = parse_frontmatter_stamped(path, content, &split)?;
    match instance.fields.iter().find(|f| f.key == field) {
        // ABSENT: insert `field:\n  - <record>` at the END of the frontmatter,
        // just before the closing `---` (or at EOF for a pure-yaml file). Anchored
        // on the frontmatter RANGE, not on the last field's `value_span`: a
        // sequence or mapping field's span over-extends past the frontmatter close,
        // so anchoring on it would place the new field in the body and duplicate
        // the key on the next stamp. A trailing comment inside the frontmatter ends
        // up before the new field, the frontmatter stays valid.
        None => {
            let block = crate::yaml_render::field(field, &Value::Array(vec![record.clone()]), 0);
            let at = split.frontmatter_range.end;
            // The frontmatter body ends with a newline before the delimiter; if it
            // does not (a pure-yaml file with no trailing newline), start one.
            let text = if at == 0 || content[..at].ends_with('\n') {
                block
            } else {
                format!("\n{block}")
            };
            let new = apply_edits(content, vec![Edit { start: at, end: at, text }]);
            stamp_structural_check(path, &new)?;
            Ok(StampOutcome::Applied(new))
        }
        Some(existing) => match &existing.value {
            InstanceValue::Sequence(elements) => {
                // The idempotent ensure: a full shallow match on any existing
                // element makes this a no-op, so the frontmatter is untouched.
                if let Some(m) = match_on {
                    if elements.iter().any(|el| element_matches_all(&el.value, m)) {
                        return Ok(StampOutcome::NoOp);
                    }
                }
                let new = if elements.is_empty() {
                    seed_first_element(content, existing.value_span, record)
                } else {
                    if content.as_bytes().get(existing.value_span.start) == Some(&b'[') {
                        return Err(MutationReject::new(
                            "the stamp field is flow-style ([ .. ]); v1 appends to block-style sequences only",
                        ));
                    }
                    let spans: Vec<ByteRange> = elements.iter().map(|e| e.span).collect();
                    append_after_last_element(content, &spans, record)
                };
                stamp_structural_check(path, &new)?;
                Ok(StampOutcome::Applied(new))
            }
            _ => Err(MutationReject::new(format!(
                "stamp field '{field}' exists but is not a sequence; a stamp ensures a record in a frontmatter list-field"
            ))),
        },
    }
}

/// Fold a stamp into a file the write already touched, on disk. Reads the
/// just-written bytes, idempotent-ensures the record, and writes the final bytes
/// once. Returns the new content hash when the stamp CHANGED the file (so the
/// response reflects the stamped content), `None` on a dedup no-op.
///
/// Runs INSIDE a write closure, after the primary write and before the saga
/// commit, so the stamp shares the write's own commit. An error propagates as a
/// `MutationReject`, so the saga compensates and nothing half-lands.
pub(crate) fn fold_stamp(
    target: &Path,
    field: &str,
    record: &Value,
    match_on: Option<&Map<String, Value>>,
) -> Result<Option<ContentHash>, MutationReject> {
    let content = std::fs::read_to_string(target).map_err(|e| {
        MutationReject::new(format!(
            "stamp could not read {} to fold into: {e}",
            target.display()
        ))
    })?;
    match splice_ensure_stamp(&content, target, field, record, match_on)? {
        StampOutcome::NoOp => Ok(None),
        StampOutcome::Applied(new) => {
            std::fs::write(target, &new).map_err(|e| {
                MutationReject::new(format!("stamp could not write {}: {e}", target.display()))
            })?;
            Ok(Some(ContentHash::of(new.as_bytes())))
        }
    }
}

/// Parse a file's frontmatter for a stamp, TYPE-AGNOSTICALLY: a synthetic `type:`
/// claim is stamped when the file writes none, so an untyped frontmatter still
/// yields an `Instance` with its field spans. A written `type:` always wins. The
/// same synthetic-stamp mechanism engine-schema files use.
fn parse_frontmatter_stamped(
    path: &Path,
    content: &str,
    split: &au_parser::FrontmatterSplit<'_>,
) -> Result<Instance, MutationReject> {
    let offset = split.frontmatter_range.start;
    let docs = au_parser::yaml::parse(split.frontmatter)
        .map_err(|_| MutationReject::new("file frontmatter is not valid YAML"))?;
    let doc = docs
        .first()
        .ok_or_else(|| MutationReject::new("file has no YAML document"))?;
    let stamp = TypeNameClaim::parse("au.stamp", ByteRange::new(0, 0));
    au_core::parse_instance_stamped(path, content, offset, doc, Some(stamp))
        .instance
        .ok_or_else(|| MutationReject::new("stamp target frontmatter is not a mapping"))
}

/// The stamp's structural check: the result must still parse as frontmatter YAML.
/// Type-agnostic, unlike [`structural_check`], because a stamp does not require a
/// typed record. Catches a splice that produced broken YAML.
fn stamp_structural_check(path: &Path, candidate: &str) -> Result<(), MutationReject> {
    let split = match au_parser::split_frontmatter(candidate) {
        Ok(Some(s)) => s,
        Ok(None) if au_parser::is_pure_yaml_instance_path(path) => {
            au_parser::whole_as_frontmatter(candidate)
        }
        // A markdown file with no frontmatter after a stamp means nothing was
        // spliced into frontmatter (the create-block path always prepends one),
        // so there is nothing to re-validate.
        Ok(None) => return Ok(()),
        Err(_) => return Err(MutationReject::new(
            "the stamp produced unterminated frontmatter — internal splice error, nothing written",
        )),
    };
    au_parser::yaml::parse(split.frontmatter).map(|_| ()).map_err(|_| {
        MutationReject::new(
            "the stamp produced content that no longer parses as YAML — internal splice error, nothing written",
        )
    })
}

/// Whether an existing sequence element carries every `match_on` pair. v1 is
/// SHALLOW: each key names a top-level slot of the element, compared by value
/// equality. A non-mapping element matches nothing. A caller needing a richer
/// predicate composes the discriminator into the values it matches on.
///
/// The `type` key is special: an element's `type:` is parsed as a CLAIM, not a
/// field, so it is matched against the element's type-claim (a name string, or a
/// list of names for a mixin). This is what makes a provenance dedup on
/// `{ type: file-change.edit, session: ... }` actually match.
fn element_matches_all(el: &InstanceValue, match_on: &Map<String, Value>) -> bool {
    let InstanceValue::Mapping(inline) = el else {
        return false;
    };
    match_on.iter().all(|(key, want)| {
        if key == "type" {
            type_claim_matches(inline.type_claim.as_ref(), want)
        } else {
            inline
                .fields
                .iter()
                .find(|f| &f.key == key)
                .is_some_and(|f| instance_value_eq_json(&f.value, want))
        }
    })
}

/// Whether an element's `type:` claim equals a `match_on` `type` value. A string
/// matches when it is one of the claim's authored names (`name` or `name::repo`);
/// a list matches the whole claim set. An element with no claim matches nothing.
fn type_claim_matches(claim: Option<&TypeClaim>, want: &Value) -> bool {
    let Some(claim) = claim else {
        return false;
    };
    let names: Vec<String> = claim.iter().map(|c| c.authored()).collect();
    match want {
        Value::String(s) => names.iter().any(|n| n == s),
        Value::Array(arr) => {
            arr.len() == names.len()
                && arr
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| names.iter().any(|n| n == s)))
        }
        _ => false,
    }
}

/// Structural equality between a parsed `InstanceValue` and a wire
/// `serde_json::Value`, for the stamp's shallow dedup. Scalars compare by value;
/// a mapping compares by its keys (order-independent), a sequence element-wise
/// (order-sensitive). A shape mismatch is inequality, never a panic.
fn instance_value_eq_json(iv: &InstanceValue, jv: &Value) -> bool {
    match (iv, jv) {
        (InstanceValue::String(s), Value::String(js)) => s == js,
        (InstanceValue::Integer(i), Value::Number(n)) => n.as_i64() == Some(*i),
        (InstanceValue::Float(f), Value::Number(n)) => n.as_f64() == Some(*f),
        (InstanceValue::Boolean(b), Value::Bool(jb)) => b == jb,
        (InstanceValue::Null, Value::Null) => true,
        (InstanceValue::Sequence(els), Value::Array(arr)) => {
            els.len() == arr.len()
                && els
                    .iter()
                    .zip(arr)
                    .all(|(e, j)| instance_value_eq_json(&e.value, j))
        }
        (InstanceValue::Mapping(inline), Value::Object(map)) => {
            inline.fields.len() == map.len()
                && inline.fields.iter().all(|f| {
                    map.get(&f.key)
                        .is_some_and(|j| instance_value_eq_json(&f.value, j))
                })
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ensure_mixin: the type-claim analog of the stamp splice. The PURE mechanical
// splice lives here; the resolved-graph decision (closure no-op, no-new-error
// gate, strict/lenient policy) is the caller's, see
// [[spec - ensure-mixin write directive - a governed write ensures a type-claim mixin idempotently, folded into the write's own commit]].
// ---------------------------------------------------------------------------

/// The outcome of a pure mixin splice: the mixin name was already literally on
/// the claim (nothing to splice), or the new content with the mixin ensured on
/// the file's top-level `type:` claim.
#[derive(Debug)]
pub(crate) enum MixinSpliceOutcome {
    AlreadyPresent,
    Applied(String),
}

/// `ensure_mixin`: ensure `mixin` (a `::repo`-qualified type name in authored
/// form, e.g. `provenance::au-provenance`) is present on a file's top-level
/// `type:` claim, a comment-preserving byte-splice.
///
/// This is the PURE mechanical splice, no graph. It only ensures the NAME is
/// written on the claim. Whether the mixin SHOULD be applied — the closure no-op
/// and the no-new-error gate over the resolved graph — is the caller's.
///
/// - the name already on the claim: `AlreadyPresent`, nothing spliced.
/// - a bare claim `type: X`: becomes the flow list `type: [X, mixin]`.
/// - a flow-list claim `type: [X, Y]`: becomes `type: [X, Y, mixin]`.
/// - a block-list claim: the mixin is appended as a new `- mixin` item, every
///   existing item byte-identical.
/// - NO `type:` key (a mapping note): a `type: <mixin>` key is created at the
///   frontmatter start, promoting the note to a typed instance.
/// - a markdown file with no frontmatter GAINS a `---` block carrying the claim,
///   the body preserved verbatim.
pub(crate) fn splice_ensure_mixin(
    content: &str,
    path: &Path,
    mixin: &str,
) -> Result<MixinSpliceOutcome, MutationReject> {
    // Locate the frontmatter, the same three entry states as the stamp splice:
    // a `---` block, a pure-yaml instance (whole file), or a markdown file with
    // no frontmatter (which gains one).
    let split = match au_parser::split_frontmatter(content) {
        Ok(Some(s)) => s,
        Ok(None) => {
            if au_parser::is_pure_yaml_instance_path(path) {
                au_parser::whole_as_frontmatter(content)
            } else {
                let key = crate::yaml_render::field("type", &Value::String(mixin.to_string()), 0);
                let new = format!("---\n{key}---\n{content}");
                structural_check(path, &new)?;
                return Ok(MixinSpliceOutcome::Applied(new));
            }
        }
        Err(_) => {
            return Err(MutationReject::new(
                "frontmatter opens with '---' but never closes",
            ))
        }
    };

    let offset = split.frontmatter_range.start;
    let docs = au_parser::yaml::parse(split.frontmatter)
        .map_err(|_| MutationReject::new("file frontmatter is not valid YAML"))?;
    let doc = docs
        .first()
        .ok_or_else(|| MutationReject::new("file has no YAML document"))?;

    let parsed = parse_instance(path, content, offset, doc);
    let Some(instance) = parsed.instance else {
        // No usable `type:` claim. A mapping note (a `type:` key genuinely
        // absent) gains one; a non-mapping frontmatter, or a malformed `type:`
        // claim, has nothing to splice into and rejects.
        if parsed
            .diagnostics
            .iter()
            .any(|d| d.code == au_core::codes::MISSING_TYPE_CLAIM)
        {
            let key = crate::yaml_render::field("type", &Value::String(mixin.to_string()), 0);
            let new = apply_edits(
                content,
                vec![Edit {
                    start: offset,
                    end: offset,
                    text: key,
                }],
            );
            structural_check(path, &new)?;
            return Ok(MixinSpliceOutcome::Applied(new));
        }
        return Err(MutationReject::new(
            "ensure_mixin target frontmatter is not a typed record (a non-mapping, or a malformed 'type:' claim)",
        ));
    };

    // Already literally on the claim: a byte-level no-op.
    if instance.type_claim.iter().any(|c| c.authored() == mixin) {
        return Ok(MixinSpliceOutcome::AlreadyPresent);
    }

    let new = match &instance.type_claim {
        // `type: X` -> `type: [X, mixin]`, replacing the bare name value in place.
        TypeClaim::Bare(c) => {
            let list = Value::Array(vec![
                Value::String(c.authored()),
                Value::String(mixin.to_string()),
            ]);
            let rendered = crate::yaml_render::claim_value(&list).map_err(MutationReject::new)?;
            apply_edits(
                content,
                vec![Edit {
                    start: c.span.start,
                    end: c.span.end,
                    text: rendered,
                }],
            )
        }
        TypeClaim::List { items, .. } => {
            // Flow vs block: a flow sequence opens with `[` just before the first
            // item (the sequence node's own span starts at the item, not the
            // bracket, so probe the source). A block sequence has a `-` there.
            let bytes = content.as_bytes();
            let first = items.first().expect("non-empty list claim").span.start;
            let mut i = first;
            while i > 0 && matches!(bytes[i - 1], b' ' | b'\t') {
                i -= 1;
            }
            let is_flow = i > 0 && bytes[i - 1] == b'[';
            if is_flow {
                // Flow list `[X, Y]` -> `[X, Y, mixin]`: insert `, mixin` after the
                // last item, before the closing `]`, so the others stay identical.
                let last_end = items.last().expect("non-empty list claim").span.end;
                let token = crate::yaml_render::scalar_token(&Value::String(mixin.to_string()))
                    .map_err(MutationReject::new)?;
                apply_edits(
                    content,
                    vec![Edit {
                        start: last_end,
                        end: last_end,
                        text: format!(", {token}"),
                    }],
                )
            } else {
                // Block list: append `- mixin` after the last item, byte-minimal,
                // every existing item unchanged.
                let spans: Vec<ByteRange> = items.iter().map(|c| c.span).collect();
                append_after_last_element(content, &spans, &Value::String(mixin.to_string()))
            }
        }
    };
    structural_check(path, &new)?;
    Ok(MixinSpliceOutcome::Applied(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_edit_matches_edit_file_guards() {
        // Single unique occurrence replaces once.
        assert_eq!(
            compute_edit("a b a", "b", "X", false).unwrap(),
            "a X a".to_string()
        );
        // Empty old_string rejects.
        assert!(compute_edit("abc", "", "X", false).is_err());
        // Identical old/new rejects.
        assert!(compute_edit("abc", "b", "b", false).is_err());
        // Absent old_string rejects with the re-read hint.
        let err = compute_edit("abc", "z", "X", false).unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
        // Non-unique without replace_all rejects and reports the count.
        let err = compute_edit("a a a", "a", "X", false).unwrap_err();
        assert!(err.message.contains("3 times"), "{}", err.message);
        assert_eq!(err.detail.unwrap()["occurrences"], serde_json::json!(3));
        // Non-unique WITH replace_all replaces every occurrence.
        assert_eq!(
            compute_edit("a a a", "a", "X", true).unwrap(),
            "X X X".to_string()
        );
    }

    #[test]
    fn preview_content_write_is_the_content_arg() {
        let op = PreviewOp::Write {
            content: "---\ntype: note\n---\n".to_string(),
        };
        // A write ignores the present bytes, new file or overwrite alike.
        assert_eq!(
            preview_content(&op, None).unwrap(),
            Some("---\ntype: note\n---\n".to_string())
        );
        assert_eq!(
            preview_content(&op, Some(b"old")).unwrap(),
            Some("---\ntype: note\n---\n".to_string())
        );
    }

    #[test]
    fn preview_content_edit_applies_over_current() {
        let op = PreviewOp::Edit {
            old_string: "old".to_string(),
            new_string: "new".to_string(),
            replace_all: false,
        };
        assert_eq!(
            preview_content(&op, Some(b"an old value")).unwrap(),
            Some("an new value".to_string())
        );
        // An edit of an absent file rejects, no product.
        assert!(preview_content(&op, None).is_err());
    }

    #[test]
    fn preview_content_delete_is_a_removal() {
        // A delete of a present file yields None, the removal.
        assert_eq!(
            preview_content(&PreviewOp::Delete, Some(b"x")).unwrap(),
            None
        );
        // A delete of an absent file rejects.
        assert!(preview_content(&PreviewOp::Delete, None).is_err());
    }

    #[test]
    fn record_id_insertion_rides_the_first_key_line() {
        let content = "type: log\nevents:\n  - type: ev\n    at: t1\n";
        let record_start = content.find("type: ev").unwrap();
        let updated = insert_record_id(content, record_start, "b-1").unwrap();
        assert_eq!(
            updated,
            "type: log\nevents:\n  - ^: b-1\n    type: ev\n    at: t1\n"
        );
    }

    #[test]
    fn flow_style_records_are_rejected() {
        let content = "events:\n  - {type: ev}\n";
        let start = content.find("{type").unwrap();
        assert!(insert_record_id(content, start, "b-1").is_err());
    }

    #[test]
    fn markdown_marker_lands_at_the_block_end() {
        let content = "# Head\n\nA paragraph\nwith two lines.\n\nNext block.\n";
        let at = content.find("paragraph").unwrap();
        let updated = insert_markdown_marker(content, at, "b-2").unwrap();
        assert_eq!(
            updated,
            "# Head\n\nA paragraph\nwith two lines. ^b-2\n\nNext block.\n"
        );
    }

    #[test]
    fn marker_offset_off_a_char_boundary_is_rejected_not_panicked() {
        // `at` comes from the wire. An offset mid-way through a multi-byte
        // character (the second byte of `é`) once panicked the slice. It is
        // now a clean reject.
        let content = "café paragraph\n\nnext\n";
        let mid_char = 4; // 'é' occupies bytes 3..=4; byte 4 is not a boundary
        assert!(!content.is_char_boundary(mid_char));
        let err = insert_markdown_marker(content, mid_char, "b-1").unwrap_err();
        assert!(
            err.message.contains("character boundary"),
            "{}",
            err.message
        );
    }

    #[test]
    fn record_offset_off_a_char_boundary_is_rejected_not_panicked() {
        let content = "café\n";
        let mid_char = 4;
        assert!(!content.is_char_boundary(mid_char));
        let err = insert_record_id(content, mid_char, "b-1").unwrap_err();
        assert!(
            err.message.contains("character boundary"),
            "{}",
            err.message
        );
    }

    #[test]
    fn generated_ids_avoid_taken_names() {
        // The clock-reading entry produces a `b-` prefixed id.
        assert!(generate_block_id(|_| false).starts_with("b-"));

        // The bump is deterministic against a fixed base: a free base comes
        // back as-is, a taken base bumps to `base-1`, still prefixed by it.
        let base = "b-abc".to_string();
        assert_eq!(bump_until_free(base.clone(), |_| false), "b-abc");
        let bumped = bump_until_free(base.clone(), |c| c == base);
        assert_ne!(bumped, base);
        assert!(bumped.starts_with(&base));
    }

    #[test]
    fn member_paths_resolve_and_guards_hold_per_member() {
        use crate::repo::{Repo, RepoName};
        use std::collections::BTreeMap;

        fn member(root: &str, name: &str) -> Repo {
            Repo {
                root: PathBuf::from(root),
                name: RepoName(name.into()),
                declared: true,
                builtin: false,
                description: None,
                remote: None,
                deps: Vec::new(),
                peer_paths: BTreeMap::new(),
            }
        }
        // The workspace root `/v`, plus a SCATTERED member rooted outside it.
        let repos =
            RepoMap::from_repos(vec![member("/v", "root"), member("/scattered", "content")]);
        let root = Path::new("/v");

        // Relative joins the fallback root and lands in the root member.
        assert_eq!(
            resolve_repo_path(&repos, root, "notes/a.md").unwrap(),
            PathBuf::from("/v/notes/a.md")
        );
        assert_eq!(
            resolve_repo_path(&repos, root, "/v/notes/a.md").unwrap(),
            PathBuf::from("/v/notes/a.md")
        );
        // A SCATTERED member is reachable by absolute path — the read/write
        // symmetry this change exists for.
        assert_eq!(
            resolve_repo_path(&repos, root, "/scattered/compositions/default.yaml").unwrap(),
            PathBuf::from("/scattered/compositions/default.yaml")
        );
        // Lexical `..` that stays inside resolves; escaping every member rejects.
        assert_eq!(
            resolve_repo_path(&repos, root, "notes/../a.md").unwrap(),
            PathBuf::from("/v/a.md")
        );
        assert!(resolve_repo_path(&repos, root, "../outside.md").is_err());
        assert!(resolve_repo_path(&repos, root, "/etc/passwd").is_err());
        // The `.arsumbris/` guard applies PER MEMBER.
        assert!(resolve_repo_path(&repos, root, ".arsumbris/cache.db").is_err());
        assert!(resolve_repo_path(&repos, root, "/scattered/.arsumbris/cache.db").is_err());
    }

    #[test]
    fn delete_file_guards_absence_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.md");
        std::fs::write(&f, "hello").unwrap();
        // A stale expected_hash rejects, the file stays.
        assert!(delete_file(&f, Some("0000000000000000")).is_err());
        assert!(f.exists());
        // The matching hash deletes.
        let h = hash_hex(ContentHash::of(b"hello"));
        assert!(delete_file(&f, Some(&h)).is_ok());
        assert!(!f.exists());
        // Deleting an absent file rejects rather than succeeding silently.
        assert!(delete_file(&f, None).is_err());
    }

    // --- nested-record mutations ---------------------------------------------

    use serde_json::json;

    fn seg(s: &str) -> PathSegment {
        PathSegment::Field(s.to_string())
    }
    fn obj(v: serde_json::Value) -> Map<String, Value> {
        v.as_object().expect("a json object").clone()
    }
    fn yaml_path() -> &'static Path {
        Path::new("/v/plan.yaml")
    }
    fn md_path() -> &'static Path {
        Path::new("/v/note.md")
    }

    const ACTION_SRC: &str = "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action.open\n        description: a\n";

    #[test]
    fn edit_record_replaces_a_scalar_field_and_keeps_a_trailing_doc() {
        let content =
            "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action.open\n        description: a  #: what to do\n";
        let fp = [
            seg("phases"),
            PathSegment::Index(0),
            seg("actions"),
            PathSegment::Index(0),
        ];
        let out = splice_edit_record(
            content,
            yaml_path(),
            &fp,
            &obj(json!({"description": "done"})),
        )
        .unwrap();
        assert_eq!(
            out,
            "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action.open\n        description: done  #: what to do\n"
        );
    }

    #[test]
    fn edit_record_retypes_scalar_and_inline_list() {
        let fp = [
            seg("phases"),
            PathSegment::Index(0),
            seg("actions"),
            PathSegment::Index(0),
        ];
        let out = splice_edit_record(
            ACTION_SRC,
            yaml_path(),
            &fp,
            &obj(json!({"type": "action.done"})),
        )
        .unwrap();
        assert!(out.contains("      - type: action.done\n"), "{out}");
        let out = splice_edit_record(
            ACTION_SRC,
            yaml_path(),
            &fp,
            &obj(json!({"type": ["action.done", "priority"]})),
        )
        .unwrap();
        assert!(
            out.contains("      - type: [action.done, priority]\n"),
            "{out}"
        );
    }

    #[test]
    fn edit_record_action_done_sets_type_and_inserts_outputs_preserving_comments() {
        let content =
            "type: plan\nphases:\n  # the first phase\n  - type: phase\n    actions:\n      - type: action.open\n        description: a  #: what to do\n";
        let fp = [
            seg("phases"),
            PathSegment::Index(0),
            seg("actions"),
            PathSegment::Index(0),
        ];
        let out = splice_edit_record(
            content,
            yaml_path(),
            &fp,
            &obj(json!({"type": "action.done", "outputs": ["finished it"]})),
        )
        .unwrap();
        assert_eq!(
            out,
            "type: plan\nphases:\n  # the first phase\n  - type: phase\n    actions:\n      - type: action.done\n        description: a  #: what to do\n        outputs:\n          - finished it\n"
        );
    }

    #[test]
    fn edit_record_inserts_into_a_bare_type_only_record() {
        let content = "type: plan\nphases:\n  - type: phase\n";
        let fp = [seg("phases"), PathSegment::Index(0)];
        let out =
            splice_edit_record(content, yaml_path(), &fp, &obj(json!({"title": "Intro"}))).unwrap();
        assert_eq!(
            out,
            "type: plan\nphases:\n  - type: phase\n    title: Intro\n"
        );
    }

    #[test]
    fn edit_record_rejects_are_loud() {
        let content =
            "type: plan\nphases:\n  - type: phase\n    actions:\n      - type: action.open\n        tags:\n          - x\n        description: a\n";
        let action = [
            seg("phases"),
            PathSegment::Index(0),
            seg("actions"),
            PathSegment::Index(0),
        ];
        // A block-id key is not patchable.
        assert!(
            splice_edit_record(content, yaml_path(), &action, &obj(json!({"^": "id"}))).is_err()
        );
        // A list field value is not replaced in v1.
        assert!(
            splice_edit_record(content, yaml_path(), &action, &obj(json!({"tags": ["y"]})))
                .is_err()
        );
        // A scalar field cannot be replaced by a list.
        assert!(splice_edit_record(
            content,
            yaml_path(),
            &action,
            &obj(json!({"description": ["x"]}))
        )
        .is_err());
        // A stale path resolves to no record.
        assert!(
            splice_edit_record(content, yaml_path(), &[seg("nope")], &obj(json!({"a": 1})))
                .is_err()
        );
        // A sequence path belongs to append_record.
        assert!(splice_edit_record(
            content,
            yaml_path(),
            &[seg("phases")],
            &obj(json!({"a": 1}))
        )
        .is_err());
        // An empty patch is a no-op reject.
        assert!(splice_edit_record(content, yaml_path(), &action, &Map::new()).is_err());
    }

    #[test]
    fn append_record_adds_an_element_preserving_sibling_comments() {
        let content =
            "type: plan\nphases:\n  - type: phase\n    title: first  #: the intro phase\n";
        let out = splice_append_record(
            content,
            yaml_path(),
            &[seg("phases")],
            &json!({"type": "phase", "title": "second"}),
        )
        .unwrap();
        assert_eq!(
            out,
            "type: plan\nphases:\n  - type: phase\n    title: first  #: the intro phase\n  - type: phase\n    title: second\n"
        );
    }

    #[test]
    fn append_record_seeds_an_empty_list() {
        let content = "type: plan\nprogressLog: []\n";
        let out = splice_append_record(
            content,
            yaml_path(),
            &[seg("progressLog")],
            &json!({"type": "logEntry", "note": "started"}),
        )
        .unwrap();
        assert_eq!(
            out,
            "type: plan\nprogressLog:\n  - type: logEntry\n    note: started\n"
        );
    }

    #[test]
    fn append_record_rejects_flow_and_a_record_target() {
        let content = "type: plan\ntags: [a, b]\nphases:\n  - type: phase\n";
        // A populated flow sequence is out of v1.
        assert!(splice_append_record(content, yaml_path(), &[seg("tags")], &json!("c")).is_err());
        // A record path belongs to edit_record.
        assert!(splice_append_record(
            content,
            yaml_path(),
            &[seg("phases"), PathSegment::Index(0)],
            &json!("x")
        )
        .is_err());
    }

    fn applied(outcome: StampOutcome) -> String {
        match outcome {
            StampOutcome::Applied(s) => s,
            StampOutcome::NoOp => panic!("expected Applied, got NoOp"),
        }
    }

    #[test]
    fn stamp_creates_the_field_when_absent() {
        let content = "type: note\ndescription: original\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                yaml_path(),
                "provenance",
                &json!({"session": "s1", "kind": "edit"}),
                None,
            )
            .unwrap(),
        );
        // The renderer emits keys in canonical order: `type` first, the rest
        // alphabetical (kind before session). The record is opaque, so its key
        // order is the renderer's, not the caller's insertion order.
        assert_eq!(
            out,
            "type: note\ndescription: original\nprovenance:\n  - kind: edit\n    session: s1\n"
        );
    }

    #[test]
    fn stamp_inserts_after_a_sequence_field_in_the_frontmatter_not_the_body() {
        // Regression: a sequence (or mapping) frontmatter field's value_span
        // over-extends past the closing `---`, so the absent-field insert must
        // anchor on the frontmatter RANGE, not the last field span. Otherwise the
        // new field lands in the body and the next stamp duplicates the key.
        let content = "---\ntype: log\nevents:\n  - a\n  - b\n---\n# Body\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                md_path(),
                "provenance",
                &json!({"kind": "create"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            out,
            "---\ntype: log\nevents:\n  - a\n  - b\nprovenance:\n  - kind: create\n---\n# Body\n"
        );
        // A second stamp appends to the SAME field, never a duplicate `provenance:`.
        let out2 = applied(
            splice_ensure_stamp(
                &out,
                md_path(),
                "provenance",
                &json!({"kind": "edit"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            out2.matches("provenance:").count(),
            1,
            "the second stamp must append, not duplicate the key: {out2}"
        );
    }

    #[test]
    fn stamp_seeds_an_empty_list() {
        let content = "type: note\nprovenance: []\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                yaml_path(),
                "provenance",
                &json!({"kind": "create"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(out, "type: note\nprovenance:\n  - kind: create\n");
    }

    #[test]
    fn stamp_appends_into_a_populated_sequence_keeping_comments() {
        // An existing entry carries a comment; it must survive byte-identical.
        let content = "type: note\nprovenance:\n  - session: s1  # first touch\n    kind: create\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                yaml_path(),
                "provenance",
                &json!({"session": "s2", "kind": "edit"}),
                None,
            )
            .unwrap(),
        );
        // The existing element stays byte-identical (comment and key order); the
        // appended element renders in canonical order (kind before session).
        assert_eq!(
            out,
            "type: note\nprovenance:\n  - session: s1  # first touch\n    kind: create\n  - kind: edit\n    session: s2\n"
        );
    }

    #[test]
    fn stamp_dedups_to_a_noop_on_a_full_match() {
        // An edit already stamped for this session: match_on hits, nothing appends.
        let content = "type: note\nprovenance:\n  - session: s1\n    kind: edit\n";
        let outcome = splice_ensure_stamp(
            content,
            yaml_path(),
            "provenance",
            &json!({"session": "s1", "kind": "edit"}),
            Some(&obj(json!({"session": "s1", "kind": "edit"}))),
        )
        .unwrap();
        assert!(matches!(outcome, StampOutcome::NoOp), "expected a no-op");
    }

    #[test]
    fn stamp_match_on_needs_every_pair_on_one_element() {
        // The session matches one element and the kind another, but no single
        // element carries BOTH pairs, so the stamp is not a no-op.
        let content = "type: note\nprovenance:\n  - session: s1\n    kind: create\n  - session: s2\n    kind: edit\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                yaml_path(),
                "provenance",
                &json!({"session": "s2", "kind": "create"}),
                Some(&obj(json!({"session": "s2", "kind": "create"}))),
            )
            .unwrap(),
        );
        // Appended (canonical key order): s2/create matches no single existing element.
        assert!(out.contains("  - kind: create\n    session: s2\n"), "{out}");
    }

    #[test]
    fn stamp_without_match_on_always_appends() {
        // No match_on, so even an identical record appends again (the rename case,
        // one entry per event).
        let content = "type: note\nprovenance:\n  - kind: rename\n    from: old.md\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                yaml_path(),
                "provenance",
                &json!({"kind": "rename", "from": "old.md"}),
                None,
            )
            .unwrap(),
        );
        // The existing element keeps its authored key order; the appended one
        // renders canonical (from before kind). Same logical record, appended
        // again because there is no match_on.
        assert_eq!(
            out,
            "type: note\nprovenance:\n  - kind: rename\n    from: old.md\n  - from: old.md\n    kind: rename\n"
        );
    }

    #[test]
    fn stamp_rejects_a_non_sequence_field() {
        // A scalar `provenance:` is not a list-field the stamp can ensure into.
        let content = "type: note\nprovenance: a-scalar\n";
        assert!(splice_ensure_stamp(
            content,
            yaml_path(),
            "provenance",
            &json!({"kind": "edit"}),
            None,
        )
        .is_err());
    }

    #[test]
    fn stamp_frontmatter_izes_a_plain_markdown_file() {
        // A markdown file with no frontmatter GAINS one carrying the stamp; the
        // body survives verbatim.
        let content = "# My Note\n\nSome prose.\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                md_path(),
                "provenance",
                &json!({"kind": "create"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            out,
            "---\nprovenance:\n  - kind: create\n---\n# My Note\n\nSome prose.\n"
        );
    }

    #[test]
    fn stamp_inserts_into_untyped_frontmatter() {
        // Frontmatter present but no `type:`: the stamp still inserts, no typed
        // instance required.
        let content = "---\ntitle: hi\n---\n# Body\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                md_path(),
                "provenance",
                &json!({"kind": "edit"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            out,
            "---\ntitle: hi\nprovenance:\n  - kind: edit\n---\n# Body\n"
        );
    }

    #[test]
    fn stamp_dedups_on_the_element_type_claim_the_provenance_shape() {
        // The real provenance shape: each element's `type:` is a CLAIM (a sealed
        // file-change leaf), plus a `session` reference. Dedup is on (type,
        // session), so a second edit in the same session no-ops, but the same
        // session under a different type still appends.
        let content = "---\ntype: my-note\nprovenance:\n  - type: file-change.create\n    session: \"[[session-0042.spans]]\"\n  - type: file-change.edit\n    session: \"[[session-0051.spans]]\"\n---\n# My note\n";

        // A second edit in session 0051: matches the existing edit element → no-op.
        let outcome = splice_ensure_stamp(
            content,
            md_path(),
            "provenance",
            &json!({"type": "file-change.edit", "session": "[[session-0051.spans]]"}),
            Some(&obj(
                json!({"type": "file-change.edit", "session": "[[session-0051.spans]]"}),
            )),
        )
        .unwrap();
        assert!(
            matches!(outcome, StampOutcome::NoOp),
            "a repeat edit in the same session dedups on the type claim"
        );

        // An EDIT in session 0042 — the create's session — must still append: the
        // session matches the create element but the type does not.
        let out = applied(
            splice_ensure_stamp(
                content,
                md_path(),
                "provenance",
                &json!({"type": "file-change.edit", "session": "[[session-0042.spans]]"}),
                Some(&obj(
                    json!({"type": "file-change.edit", "session": "[[session-0042.spans]]"}),
                )),
            )
            .unwrap(),
        );
        assert!(
            out.contains("  - type: file-change.edit\n    session: \"[[session-0042.spans]]\"\n"),
            "type discriminates create from edit within one session: {out}"
        );
    }

    #[test]
    fn stamp_appends_in_markdown_frontmatter_keeping_the_body() {
        // A typed markdown file with a body: the append lands inside the
        // frontmatter sequence, the body is untouched.
        let content = "---\ntype: note\nprovenance:\n  - kind: create\n---\n# Heading\n\nprose\n";
        let out = applied(
            splice_ensure_stamp(
                content,
                md_path(),
                "provenance",
                &json!({"kind": "edit"}),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            out,
            "---\ntype: note\nprovenance:\n  - kind: create\n  - kind: edit\n---\n# Heading\n\nprose\n"
        );
    }

    // --- ensure_mixin (the pure type-claim splice) ---

    const MIXIN: &str = "provenance::au-provenance";

    fn applied_mixin(outcome: MixinSpliceOutcome) -> String {
        match outcome {
            MixinSpliceOutcome::Applied(s) => s,
            MixinSpliceOutcome::AlreadyPresent => {
                panic!("expected Applied, got AlreadyPresent")
            }
        }
    }

    #[test]
    fn mixin_bare_claim_becomes_a_flow_list() {
        let content = "---\ntype: note\ntitle: x\n---\nbody\n";
        let out = applied_mixin(splice_ensure_mixin(content, md_path(), MIXIN).unwrap());
        assert_eq!(
            out,
            "---\ntype: [note, provenance::au-provenance]\ntitle: x\n---\nbody\n"
        );
    }

    #[test]
    fn mixin_block_list_appends_an_item_keeping_the_others() {
        let content = "---\ntype:\n  - note\n  - task\n---\n";
        let out = applied_mixin(splice_ensure_mixin(content, md_path(), MIXIN).unwrap());
        assert_eq!(
            out,
            "---\ntype:\n  - note\n  - task\n  - provenance::au-provenance\n---\n"
        );
    }

    #[test]
    fn mixin_flow_list_appends_in_place() {
        let content = "---\ntype: [note, task]\n---\n";
        let out = applied_mixin(splice_ensure_mixin(content, md_path(), MIXIN).unwrap());
        assert_eq!(
            out,
            "---\ntype: [note, task, provenance::au-provenance]\n---\n"
        );
    }

    #[test]
    fn mixin_creates_the_claim_on_a_mapping_note() {
        // Frontmatter present, no `type:` key: the note is promoted to a typed
        // instance, the claim inserted first, the other keys and body untouched.
        let content = "---\ntitle: x\n---\nbody\n";
        let out = applied_mixin(splice_ensure_mixin(content, md_path(), MIXIN).unwrap());
        assert_eq!(
            out,
            "---\ntype: provenance::au-provenance\ntitle: x\n---\nbody\n"
        );
    }

    #[test]
    fn mixin_frontmatter_izes_a_plain_markdown_file() {
        let content = "# Heading\n\nprose\n";
        let out = applied_mixin(splice_ensure_mixin(content, md_path(), MIXIN).unwrap());
        assert_eq!(
            out,
            "---\ntype: provenance::au-provenance\n---\n# Heading\n\nprose\n"
        );
    }

    #[test]
    fn mixin_creates_the_claim_on_a_pure_yaml_note() {
        let content = "title: x\n";
        let out = applied_mixin(splice_ensure_mixin(content, yaml_path(), MIXIN).unwrap());
        assert_eq!(out, "type: provenance::au-provenance\ntitle: x\n");
    }

    #[test]
    fn mixin_already_present_is_a_no_op() {
        let content = "---\ntype:\n  - note\n  - provenance::au-provenance\n---\n";
        let outcome = splice_ensure_mixin(content, md_path(), MIXIN).unwrap();
        assert!(
            matches!(outcome, MixinSpliceOutcome::AlreadyPresent),
            "the literal name is on the claim, so the splice is a no-op"
        );
    }

    #[test]
    fn mixin_bare_claim_already_the_mixin_is_a_no_op() {
        let content = "---\ntype: provenance::au-provenance\n---\n";
        let outcome = splice_ensure_mixin(content, md_path(), MIXIN).unwrap();
        assert!(matches!(outcome, MixinSpliceOutcome::AlreadyPresent));
    }

    #[test]
    fn mixin_rejects_a_non_mapping_frontmatter() {
        // A sequence frontmatter is not a typed record and has no claim to splice.
        let content = "---\n- a\n- b\n---\n";
        assert!(splice_ensure_mixin(content, md_path(), MIXIN).is_err());
    }
}
