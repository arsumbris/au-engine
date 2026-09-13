//! The reverse reference index: who points at each file.
//!
//! Forward resolution reuses [`RepoIndex::resolve`]. This inverts the resolved
//! outgoing wikilink references of every instance into an inbound index keyed
//! by target file, each edge tagged with the slot it fills.
//!
//! Only references that resolve to a file are indexed. Dangling and ambiguous
//! references are already diagnosed by forward validation, see au-core's
//! reference checks; the graph holds resolved edges, so this pass emits no
//! diagnostics of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use au_core::{DocOrigin, DocstringLink, InstanceValue, NavLink};
use au_diagnostics::ByteRange;
use au_parser::{scan_body, BodyEvent};
use au_references::{parse_wikilink_inner, RepoIndex};

use crate::ir::FileEntry;
use crate::parse::FileParse;

/// Which surface an inbound reference originates from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefSurface {
    Frontmatter,
    Body,
    /// A `#:` docstring on a type-def or instance declaration. Navigational
    /// only, so it shares the `navigational` kind with a body prose link; the
    /// surface is what distinguishes a documentation reference from a prose
    /// mention. See [[type docstring::au-type-system]].
    Docstring,
}

/// The COARSE edge kind, derived from the two fields an edge always carries.
/// The one classification the `references_in` read and the `neighborhood` walk
/// share, so a kind means the same thing whichever direction it was found in.
///
/// - `navigational` — a body prose link, no `:field` attribution.
/// - `contributing` — a body `[[target:field]]`, a data contribution.
/// - `field` — a frontmatter-surface edge.
///
/// Coarser than the OUTBOUND `references_out` five: it collapses the frontmatter
/// split (`field-reference` / `field-string-wikilink` / `unknown`) into one
/// `field`, because that split needs the referrer's resolved shape and the
/// two-snapshot handling it forces. Deferred uniformly; a later refinement
/// subdivides `field` in every sharing surface at once.
///
/// For a body edge, the `slot` IS the `:field` attribution, so `slot.is_some()`
/// is the whole `contributing` test.
pub fn coarse_edge_kind(surface: RefSurface, slot: Option<&str>) -> &'static str {
    match surface {
        RefSurface::Frontmatter => "field",
        RefSurface::Body if slot.is_some() => "contributing",
        RefSurface::Body => "navigational",
        // A docstring link is navigational regardless of which declaration it
        // documents; the `slot` names that declaration, it does not make the
        // link a data contribution. The `Docstring` surface carries the origin.
        RefSurface::Docstring => "navigational",
    }
}

/// Every value [`coarse_edge_kind`] can produce, the closed vocabulary a
/// consumer filters on. The classifier's outputs and this set are the SAME three
/// strings; a caller validating a `kinds` argument checks against this rather
/// than a second copy, so a new kind is added in one place.
pub const COARSE_KINDS: [&str; 3] = ["navigational", "contributing", "field"];

/// The neighborhood WALK's filter vocabulary: the coarse kinds plus
/// `commit-referent`. A commit-only reference (`[[::@sha]]`) mints that kind on
/// its OUTBOUND edge, and it is settled by the link, not by surface + slot, so it
/// is NOT a [`coarse_edge_kind`] output and does not belong in [`COARSE_KINDS`].
/// Outbound-only: a commit-referent forms no backlink, so it never appears
/// inbound. The walk validates its `kinds` argument against this superset.
pub const WALK_KINDS: [&str; 4] = [
    COARSE_KINDS[0],
    COARSE_KINDS[1],
    COARSE_KINDS[2],
    "commit-referent",
];

/// One inbound reference edge: a file pointing at this target, and how.
#[derive(Debug, Clone)]
pub struct Backlink {
    /// The file the reference originates from.
    pub source: PathBuf,
    /// The slot the edge fills: a frontmatter field key (the innermost key for
    /// a nested inline value), a body wikilink's `:field` attribution, or
    /// `None` for an untyped prose link.
    pub slot: Option<String>,
    pub surface: RefSurface,
    /// File-relative byte span of the reference in the source.
    pub span: ByteRange,
    /// The block-id targeted within this file, if the reference named one.
    pub block_id: Option<au_references::BlockId>,
    /// The `^:` id of the nearest enclosing inline record the reference
    /// originates from, if any — a session-log event's edge renders as
    /// `[[<source>^<source_block_id>]]`. Nested records override outer ones.
    pub source_block_id: Option<String>,
}

/// One outgoing wikilink edge as the WALK sees it, before any consumer projects
/// it: the raw link, where it sits, and what it resolved to.
///
/// The single source of truth for "what does this file point at". Both
/// directions derive from it, so they cannot disagree about which values are
/// scanned, how sequences and inline records are descended, or how a target
/// resolves.
///
/// The two projections differ, deliberately:
/// - the backlink INDEX keeps only `resolved.is_some()` edges. It is the write
///   path's reference map, and `rename` rewrites referrer bytes off each edge's
///   span, so an unresolved edge there would have it rewriting a link that
///   points at nothing.
/// - the `references_out` READ keeps every edge, resolved or dangling, since
///   reporting a broken link is half its job.
#[derive(Debug, Clone)]
pub struct SourceEdge {
    /// The field key a frontmatter edge sits in (the INNERMOST key for a nested
    /// inline value), a body wikilink's `:field` attribution, or `None` for an
    /// untyped prose link.
    pub slot: Option<String>,
    pub surface: RefSurface,
    /// File-relative byte span of the reference in the source.
    pub span: ByteRange,
    /// The `^:` id of the nearest enclosing inline record, if any. Nested
    /// records override outer ones.
    pub source_block_id: Option<String>,
    /// The parsed link, every fragment intact. The index throws these away;
    /// the forward read needs them.
    pub link: au_references::WikilinkRef,
    /// The file this resolved to, `None` for a dangling or ambiguous target.
    pub resolved: Option<PathBuf>,
    /// Frontmatter only: the field value is EXACTLY this one wikilink, not a
    /// link embedded in a longer string. Decidable here, at the value, without
    /// any type information — which is why it is carried rather than re-derived.
    pub whole_value: bool,
}

/// Invert resolved outgoing wikilink references into an inbound index keyed by
/// target file.
///
/// Walks every parsed instance in the catalog: frontmatter field values
/// (descending into sequences and inline-value mappings) and body wikilinks.
/// Each reference resolves repo-local, through the source's own repo index, so
/// a cross-repo edge forms only for a `::repo`-qualified link; a resolving
/// reference becomes an inbound edge on its target.
pub fn build_index(
    catalog: &crate::ir::OrdMap<PathBuf, FileEntry>,
    indexes: &crate::ir::RepoIndexes,
    repos: &crate::repo::RepoMap,
    workspaces: &[crate::repo::Workspace],
) -> BTreeMap<PathBuf, Vec<Backlink>> {
    let mut index: BTreeMap<PathBuf, Vec<Backlink>> = BTreeMap::new();
    for (p, entry) in catalog.iter() {
        for (target, mut edges) in source_edges(p, entry.parse.as_ref(), indexes, repos, workspaces)
        {
            index.entry(target).or_default().append(&mut edges);
        }
    }
    // Stable order within each target's inbound set. Applied after the merge,
    // so the per-source iteration order does not affect the result.
    for edges in index.values_mut() {
        sort_backlinks(edges);
    }
    index
}

/// The inbound edges one source file's parse contributes, keyed by target.
///
/// The per-source unit [`build_index`] loops and merges. The incremental path
/// calls it to recompute one changed instance's edges without walking the whole
/// catalog. The returned per-target lists are unsorted; the caller sorts the
/// merged result with [`sort_backlinks`]. A non-instance, non-note parse
/// contributes none.
pub fn source_edges(
    path: &Path,
    parse: &FileParse,
    indexes: &crate::ir::RepoIndexes,
    repos: &crate::repo::RepoMap,
    workspaces: &[crate::repo::Workspace],
) -> BTreeMap<PathBuf, Vec<Backlink>> {
    let mut index: BTreeMap<PathBuf, Vec<Backlink>> = BTreeMap::new();
    for edge in walk_edges(path, parse, indexes, repos, workspaces) {
        // Resolved edges only. See [`SourceEdge`]: this index drives the write
        // path's rewrites, so a dangling edge must not enter it.
        let Some(target) = edge.resolved else {
            continue;
        };
        index.entry(target).or_default().push(Backlink {
            source: path.to_path_buf(),
            slot: edge.slot,
            surface: edge.surface,
            span: edge.span,
            block_id: edge.link.block_id,
            source_block_id: edge.source_block_id,
        });
    }
    index
}

/// EVERY outgoing wikilink edge of one file, frontmatter then body, in source
/// order. The shared traversal behind the backlink index and the
/// `references_out` read.
///
/// It resolves but does not CLASSIFY: deciding whether a frontmatter edge fills
/// a reference slot needs the effective shape, which lives in the resolved
/// layer. That layer is not available here and must not be — `build.rs` skips
/// aborted repos when populating it, so it is not total over the files this
/// walks, and the incremental path would need two different resolved snapshots
/// inside one delta. So the walk emits raw edges and the READ classifies.
pub fn walk_edges(
    path: &Path,
    parse: &FileParse,
    indexes: &crate::ir::RepoIndexes,
    repos: &crate::repo::RepoMap,
    workspaces: &[crate::repo::Workspace],
) -> Vec<SourceEdge> {
    let mut out = Vec::new();
    let cross = CrossCtx {
        repos,
        indexes,
        workspaces,
    };

    // Docstring links. A type-def or an instance carries `[[...]]` in its `#:`
    // docstrings; each is a navigational edge tagged with the `Docstring`
    // surface, its `slot` naming the head (`None`) or the documented field. This
    // runs for a type-def too, the one edge source a type-def has, so it sits
    // ahead of the instance/note-only fields-and-body pass below.
    let (doc_source, doc_links): (&Path, &[DocstringLink]) = match parse {
        FileParse::TypeDef {
            type_def,
            doc_links,
            ..
        } => (
            type_def
                .as_ref()
                .map(|td| td.source_path.as_path())
                .unwrap_or(path),
            doc_links,
        ),
        FileParse::Instance {
            instance,
            doc_links,
            ..
        } => (
            instance
                .as_ref()
                .map(|i| i.source_path.as_path())
                .unwrap_or(path),
            doc_links,
        ),
        _ => (path, &[]),
    };
    if !doc_links.is_empty() {
        let doc_repo_index = repos
            .repo_of(doc_source)
            .map(|r| indexes.of(&r.name))
            .unwrap_or_else(|| indexes.empty());
        for dl in doc_links {
            let Ok(link) = parse_wikilink_inner(&dl.link.raw) else {
                continue;
            };
            let resolved = resolve_link(doc_source, &link, doc_repo_index, &cross);
            out.push(SourceEdge {
                slot: match &dl.origin {
                    DocOrigin::Head => None,
                    DocOrigin::Field(key) => Some(key.clone()),
                },
                surface: RefSurface::Docstring,
                span: dl.link.span,
                source_block_id: None,
                link,
                resolved,
                // A docstring link is never a field VALUE.
                whole_value: false,
            });
        }
    }

    // Typed instances and untyped notes both carry frontmatter fields and a
    // markdown body that may hold wikilinks.
    //
    // An instance whose `type:` claim FAILED to parse (`instance: None`, e.g.
    // `type: []`) still has an intact body, and its body links are real edges.
    // Requiring `Some` dropped the whole file — and since the index is the write
    // path's reference map, `rename` then silently stranded those links. Its
    // frontmatter is unparsed, so it contributes no FIELD edges; there is
    // nothing to attribute them to.
    static NO_FIELDS: &[au_core::InstanceField] = &[];
    let (source, fields, body, body_offset, is_markdown) = match parse {
        FileParse::Instance {
            instance: Some(instance),
            body,
            body_offset,
            is_markdown,
            ..
        } => (
            instance.source_path.as_path(),
            instance.fields.as_slice(),
            body,
            *body_offset,
            *is_markdown,
        ),
        FileParse::Instance {
            instance: None,
            body,
            body_offset,
            is_markdown,
            ..
        } => (path, NO_FIELDS, body, *body_offset, *is_markdown),
        FileParse::Note {
            source_path,
            fields,
            body,
            body_offset,
            ..
        } => (
            source_path.as_path(),
            fields.as_slice(),
            body,
            *body_offset,
            true,
        ),
        _ => return out,
    };

    // Resolution is repo-local: the source's links resolve against the source's
    // own repo index, so an unqualified link never forms a cross-repo edge.
    let repo_index = repos
        .repo_of(source)
        .map(|r| indexes.of(&r.name))
        .unwrap_or_else(|| indexes.empty());

    for field in fields {
        collect_value_edges(
            source,
            &field.key,
            &field.value,
            &field.nav_links,
            None,
            repo_index,
            &cross,
            &mut out,
        );
    }

    if is_markdown {
        // One body scan drives both prose wikilinks and marked record fences.
        for ev in scan_body(body) {
            match ev {
                BodyEvent::Wikilink { raw, span } => {
                    let Ok(link) = parse_wikilink_inner(raw) else {
                        continue;
                    };
                    let resolved = resolve_link(source, &link, repo_index, &cross);
                    out.push(SourceEdge {
                        slot: link.field.clone(),
                        surface: RefSurface::Body,
                        span: shift(span, body_offset),
                        source_block_id: None,
                        link,
                        resolved,
                        // A body link is never a field VALUE, so the whole-value
                        // question does not arise for it.
                        whole_value: false,
                    });
                }
                // A marked record fence (` ```yaml [:field] `) contributes an
                // inline record from the body. Its field VALUES are real
                // references, so they index like a frontmatter inline record;
                // its own `#:` docstrings index like any docstring. A plain code
                // fence carries no `[:field]` marker, and a verbatim
                // `String` / `any` fence is not a mapping, so both are skipped.
                BodyEvent::FencedBlock {
                    info,
                    body: fence_body,
                    ..
                } => {
                    let Some(field) = au_references::extract_field_marker(info) else {
                        continue;
                    };
                    let base = body_offset + offset_in(body, fence_body);
                    let Some((inline, fence_doc_links)) =
                        au_core::parse_block_record(source, fence_body, base)
                    else {
                        continue;
                    };
                    // Field-value references. `parse_block_record` returns
                    // file-absolute spans, so no shift; `collect_value_edges`
                    // descends the record like a frontmatter inline value,
                    // emitting `Frontmatter`-surface edges keyed by the inner
                    // field, so a fence record and its frontmatter twin produce
                    // identical edges.
                    collect_value_edges(
                        source,
                        field,
                        &InstanceValue::Mapping(inline),
                        &[],
                        None,
                        repo_index,
                        &cross,
                        &mut out,
                    );
                    // The fence record's own `#:` docstring links, as
                    // navigational `Docstring` edges, the body-fence twin of the
                    // top-of-function docstring pass.
                    for dl in fence_doc_links {
                        let Ok(link) = parse_wikilink_inner(&dl.link.raw) else {
                            continue;
                        };
                        let resolved = resolve_link(source, &link, repo_index, &cross);
                        out.push(SourceEdge {
                            slot: match &dl.origin {
                                DocOrigin::Head => None,
                                DocOrigin::Field(key) => Some(key.clone()),
                            },
                            surface: RefSurface::Docstring,
                            span: dl.link.span,
                            source_block_id: None,
                            link,
                            resolved,
                            whole_value: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Byte offset of `child` within `parent`, where `child` is a sub-slice of
/// `parent` (a `scan_body` fence body inside the markdown body). Pointer
/// arithmetic, the same helper `crosstype` and `serve` use to place a fence.
fn offset_in(parent: &str, child: &str) -> usize {
    child.as_ptr() as usize - parent.as_ptr() as usize
}

/// Resolve one parsed link the way the graph does: a `::repo` link into the
/// named repo, an unqualified one repo-local, and the LOCAL form (an empty
/// target with a locating fragment, `[[^id]]` / `[[#head]]`) to the source file
/// itself.
///
/// The local form is why this is one function rather than two call sites.
/// [[type reference::au-type-system]]'s Local form is explicit that resolution skips name
/// lookup, so it has no missing outcome — reporting it as dangling (which the
/// forward read did while the index resolved it) contradicted the spec.
fn resolve_link(
    source: &Path,
    link: &au_references::WikilinkRef,
    repo_index: &RepoIndex,
    cross: &CrossCtx<'_>,
) -> Option<PathBuf> {
    // A commit-pinned link (`[[file::@sha]]`, and the empty-target commit-referent
    // `[[::@sha]]`) is an INERT snapshot into an immutable past. It resolves to NO
    // path here, so it forms no inbound backlink and is never a self-link, and a
    // reference-rewriting refactor never touches it (it is not in the index off
    // which `rename` rewrites referrer bytes). Resolving it live by name would
    // misattribute on name reuse: a pin to a since-deleted `F` would re-point to a
    // new file later named `F`. This MUST precede both the `::repo` branch (which
    // would resolve the target in the peer) and the empty-target self-link branch
    // below. See the pinned-references spec.
    if link.is_inert_pin() {
        return None;
    }
    if let Some(repo_q) = link.repo.as_deref() {
        return match crate::crossref::resolve_cross_repo(
            cross.repos,
            cross.indexes,
            cross.workspaces,
            source,
            repo_q,
            &link.target,
        ) {
            crate::crossref::CrossRepoRef::Resolved(p) => Some(p),
            _ => None,
        };
    }
    if link.target.is_empty() {
        return Some(source.to_path_buf());
    }
    repo_index.resolve(&link.target).ok()
}

/// Stable order within a target's inbound set: by source, then span start,
/// then slot. Applied after a full build and after an incremental delta, so
/// both yield the identical order.
pub fn sort_backlinks(edges: &mut [Backlink]) {
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then(a.span.start.cmp(&b.span.start))
            .then(a.slot.cmp(&b.slot))
    });
}

/// Recurse a field value, emitting an edge for every wikilink in a string. The
/// slot is the field key; for an inline-value mapping it becomes the NESTED
/// field's key. `nav_links` are the holder's embedded links, extracted at parse
/// time (whole-value and embedded alike). `record_id` is the nearest enclosing
/// record's `^:` id; descending into a mapping that declares one replaces it.
#[allow(clippy::too_many_arguments)]
fn collect_value_edges(
    source: &Path,
    slot: &str,
    value: &InstanceValue,
    nav_links: &[NavLink],
    record_id: Option<&str>,
    repo_index: &RepoIndex,
    cross: &CrossCtx<'_>,
    out: &mut Vec<SourceEdge>,
) {
    match value {
        InstanceValue::String(raw) => {
            for nl in nav_links {
                let Ok(link) = parse_wikilink_inner(&nl.raw) else {
                    continue;
                };
                let resolved = resolve_link(source, &link, repo_index, cross);
                // `nl.raw` is the link's INNER text, so the comparison re-frames
                // it. Whole-value versus embedded is a VALUE-level fact, settled
                // by [[type reference::au-type-system]] without any type information: a link
                // inside a longer string is part of that string, never a
                // reference, whatever the slot admits.
                let whole_value = raw.trim() == format!("[[{}]]", nl.raw);
                out.push(SourceEdge {
                    slot: Some(slot.to_string()),
                    surface: RefSurface::Frontmatter,
                    span: nl.span,
                    source_block_id: record_id.map(str::to_string),
                    link,
                    resolved,
                    whole_value,
                });
            }
        }
        InstanceValue::Sequence(elems) => {
            for e in elems {
                collect_value_edges(
                    source,
                    slot,
                    &e.value,
                    &e.nav_links,
                    record_id,
                    repo_index,
                    cross,
                    out,
                );
            }
        }
        InstanceValue::Mapping(inline) => {
            let record_id = inline
                .block_id
                .as_ref()
                .map(|b| b.id.as_str())
                .or(record_id);
            for f in &inline.fields {
                collect_value_edges(
                    source,
                    &f.key,
                    &f.value,
                    &f.nav_links,
                    record_id,
                    repo_index,
                    cross,
                    out,
                );
            }
        }
        _ => {}
    }
}

/// The repos, indices, and workspaces a `::repo` edge resolves against.
struct CrossCtx<'a> {
    repos: &'a crate::repo::RepoMap,
    indexes: &'a crate::ir::RepoIndexes,
    workspaces: &'a [crate::repo::Workspace],
}

/// Shift a body-relative span to file-relative.
fn shift(span: ByteRange, body_offset: usize) -> ByteRange {
    ByteRange::new(span.start + body_offset, span.end + body_offset)
}
