//! The instance-only fast path: an incremental resolved recompute for an edit
//! that touches only existing instance files, bounded to the changed instances
//! and their dependents rather than the whole knowledge base.
//!
//! Split into two halves so the carrier can evolve without touching the hard
//! logic, see [[spec - incremental resolved recompute - a change recomputes its
//! blast radius byte-identical to a full build]]:
//! - compute the change, the recompute itself, the new resolved entities,
//!   diagnostic slices, and the backlink and closure deltas.
//! - apply the change, how the unchanged rest of the knowledge base reaches the new
//!   snapshot. The carrier clones the held knowledge base, an O(1) structural share (its
//!   patched maps are persistent ordered maps, its invariant structs `Arc`), and
//!   patches it by delta, so the whole apply is O(changed).

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use au_core::{
    closure_of, validate, validate_body, validate_docstring_links, Instance, RecordTargets,
    TypeGraph, TypeName, ValidateContext,
};
use au_diagnostics::{Diagnostic, LineIndex};
use au_parser::{classify_by_path, FileKind, FileSystem};
use au_references::RepoIndex;

use crate::backlinks::{sort_backlinks, source_edges, Backlink};
use crate::crossref::{cross_repo_reference_diagnostics_for, EngineCrossRepoResolver};
use crate::crosstype::cross_repo_type_diagnostics_for;
use crate::ir::{
    BuildOutcome, ContentHash, DiagSource, DiagStream, FileEntry, KnowledgeBase, OrdMap,
    RepoIndexes, ResolvedInstance,
};
use crate::parse::{parse_file, FileParse};
use crate::pathset::edge_flip_sources;
use crate::refnames::{source_name_keys, source_repo, NameKey};
use crate::repo::RepoName;

/// How a dirty set can be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirtyClass {
    /// Every dirty path is an instance edit, add, or delete. The type graphs and
    /// the repo membership are unchanged, so only the changed instances, the
    /// edge-flip set, and their dependents need recompute. The fast path.
    Incremental,
    /// The set holds a type-def, a registry, a manifest, or another
    /// non-instance file. The whole-knowledge-base resolved recompute handles it; finer
    /// paths are later phases.
    NeedsFull,
}

/// Classify a dirty set against the held knowledge base.
///
/// `Incremental` covers an edit, add, or delete of any ordinary content file:
/// a markdown or yaml file whether or not it claims a `type:`, and an asset the
/// engine catalogues but never decodes. A type-def (a graph change), a registry,
/// or a manifest reshapes the graph or membership, and forces the full path. An
/// empty set has nothing to scope.
///
/// Purely by path, it reads no disk and no catalog. The
/// edit-versus-delete-versus-add distinction is the recompute's, once it probes
/// the filesystem, and so is the one catalog-dependent refusal (a held entry
/// that never read at build). `_held` is kept because the finer classifications
/// still to come, a type-def edit's bounded blast radius, need it.
pub(crate) fn classify_dirty(_held: &KnowledgeBase, dirty: &BTreeSet<PathBuf>) -> DirtyClass {
    if dirty.is_empty() {
        return DirtyClass::NeedsFull;
    }
    for path in dirty {
        // By-path exclusions: a type-def or manifest edit is a graph or
        // membership change; an engine-schema file (registry, location, lock)
        // reshapes membership. These hold whether the path is an add or an edit,
        // so they key on the path, not the catalog.
        let kind = classify_by_path(path);
        if kind == Some(FileKind::TypeDef) || is_engine_schema_path(path) {
            return DirtyClass::NeedsFull;
        }
    }
    DirtyClass::Incremental
}

/// Replace a removed DIRECTORY in the dirty set with the catalogued files that
/// were under it.
///
/// A filesystem can report a folder removal as one event naming the folder and
/// nothing else. That path is not a file, was never catalogued (directories are
/// not nodes), and carries no fingerprint entry, so every gate downstream reads
/// it as "nothing changed" and the whole subtree stays in the catalog and the
/// reference index, resolving references to files that are gone.
///
/// If the directory is absent, its children are absent. The catalog already
/// knows which they were, so the delete needs no walk and no disk: a range from
/// the directory forward, taken while the keys stay under it.
///
/// Sound because both orderings agree with the test. [`Path`]'s `Ord` is
/// component-wise, so every child sorts contiguously after its directory; and
/// `starts_with` is component-wise too, so `/v/sub` does not swallow
/// `/v/subdir/a.md`.
///
/// Adds NO judgement of its own. The expanded paths go on to the ordinary
/// [`classify_dirty`], so a type-def or an engine-schema file under the removed
/// folder still forces the whole rebuild.
///
/// Only removal. A directory that APPEARS is left alone: discovering what is
/// inside it needs a walk, which is most of a full build, and a directory that
/// merely exists and was touched must stay a no-op or every ordinary event gets
/// more expensive.
pub(crate) fn expand_removed_directories(
    held: &KnowledgeBase,
    dirty: &BTreeSet<PathBuf>,
    fs: &impl FileSystem,
) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for path in dirty {
        // A catalogued path is a file the engine knows, present or deleted;
        // either way it speaks for itself. A path that exists is not a removal.
        if held.catalog.contains_key(path) || fs.is_file(path) {
            out.insert(path.clone());
            continue;
        }
        let children: Vec<PathBuf> = held
            .catalog
            .range(path.clone()..)
            .take_while(|(child, _)| child.starts_with(path))
            .map(|(child, _)| child.clone())
            .collect();
        if children.is_empty() {
            // Not a directory the catalog knows anything about: a spurious
            // wake, or a removal of something that was never a node. Kept as
            // itself, which contributes nothing downstream.
            out.insert(path.clone());
        } else {
            out.extend(children);
        }
    }
    out
}

/// Whether a path lives under an `.arsumbris/` directory, the engine-schema
/// home: the `repo.yaml` registry, the `repo.lock`, or the `.auignore`. A change
/// to any of them can re-route membership or scope, so
/// it forces a full rebuild. Engine-schema files are floored out of the walk, so
/// they never enter the catalog fingerprint — a rebuild's no-op gate must treat
/// them as always-changed rather than compare a hash that is always absent (else
/// a removal, whose new hash is also absent, reads as no change).
pub(crate) fn is_engine_schema_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".arsumbris")
}

/// The effective type closure of an instance: the union of every claim's
/// transitive ancestors. The keys it is a closure-membership member of. The
/// per-instance unit the full build and the incremental delta share.
pub(crate) fn instance_closure(graph: &TypeGraph, inst: &Instance) -> BTreeSet<TypeName> {
    let mut closure = BTreeSet::new();
    for claim in inst.type_claim.iter() {
        closure.extend(closure_of(graph, &claim.name));
    }
    closure
}

/// Patch the backlink index for a whole batch of changed sources at once: drop
/// every re-applied source's stale inbound edges, add every source's new ones,
/// then re-sort and prune each affected target EXACTLY ONCE.
///
/// The per-delta applier re-sorts a shared target's whole inbound list once per
/// co-dirtied source, so N referrers of one hub node cost O(N^2 log N) (the
/// quadratic). Batching groups the deltas by target first, so the hub's list is
/// rebuilt once per bulk pass, O(N log N).
///
/// Byte-identical to applying the deltas one by one: both end with the same set
/// of backlinks per target under the same total order [`sort_backlinks`], and a
/// source appears in at most one delta (each dirty / deleted / dependent path
/// yields one), so the drop set per target is exactly the sources whose delta
/// touched it. A target left with no edges is removed, the full build holds none.
pub(crate) fn apply_backlink_deltas(
    index: &mut OrdMap<PathBuf, Vec<Backlink>>,
    deltas: Vec<BacklinkDelta>,
) {
    // Group by target: the sources to drop (every source whose delta touched the
    // target, via its old targets or its new edges), and the new edges to add.
    let mut drop_sources: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let mut additions: BTreeMap<PathBuf, Vec<Backlink>> = BTreeMap::new();
    for d in deltas {
        for target in d.old_targets.iter().chain(d.new_edges.keys()) {
            drop_sources
                .entry(target.clone())
                .or_default()
                .insert(d.source.clone());
        }
        for (target, mut edges) in d.new_edges {
            additions.entry(target).or_default().append(&mut edges);
        }
    }

    // Every target with additions also appears in `drop_sources` (its source is
    // added via `new_edges.keys()`), so iterating `drop_sources` covers them all.
    for (target, sources) in drop_sources {
        let adds = additions.remove(&target).unwrap_or_default();
        let empty = match index.get_mut(&target) {
            Some(edges) => {
                edges.retain(|b| !sources.contains(&b.source));
                edges.extend(adds);
                sort_backlinks(edges);
                edges.is_empty()
            }
            None => {
                // Target absent from the held index: seed it with the sorted adds
                // (a target gaining its first inbound edge this pass).
                if !adds.is_empty() {
                    let mut adds = adds;
                    sort_backlinks(&mut adds);
                    index.insert_mut(target.clone(), adds);
                }
                false
            }
        };
        if empty {
            index.remove_mut(&target);
        }
    }
}

/// Patch the backlink index for one changed source. The single-delta convenience
/// over [`apply_backlink_deltas`], the degenerate one-element batch, so both
/// share the one code path.
///
/// `old_targets` are the targets the source pointed at before the edit.
/// `new_edges` is [`crate::backlinks::source_edges`] over its new parse.
///
/// Test-only: the apply path batches through [`apply_backlink_deltas`]; the
/// parity tests exercise the batched code path at N=1 through this shim.
#[cfg(test)]
pub(crate) fn apply_backlink_delta(
    index: &mut OrdMap<PathBuf, Vec<Backlink>>,
    source: &Path,
    old_targets: &BTreeSet<PathBuf>,
    new_edges: BTreeMap<PathBuf, Vec<Backlink>>,
) {
    apply_backlink_deltas(
        index,
        vec![BacklinkDelta {
            source: source.to_path_buf(),
            old_targets: old_targets.clone(),
            new_edges,
        }],
    );
}

/// Patch closure-membership for a whole batch of changed instances at once: drop
/// each from the `(repo, type)` keys it left, add each to those it gained, then
/// re-sort each affected key EXACTLY ONCE.
///
/// The per-delta applier `push`es then `sort`s a shared key's whole membership
/// vec once per instance, so N same-typed instances retyped together cost
/// O(N^2 log N) on the shared key, the closure-membership sibling of the per-hub
/// backlink quadratic.
/// Batching groups the deltas by key first, so a shared key's vec is rebuilt once
/// per bulk pass, O(N log N).
///
/// Byte-identical to applying the deltas one by one: values stay path-sorted, the
/// order the full build produces, and a path appears in at most one delta (each
/// instance yields one), so within a key it is either removed or added, never
/// both. A key left with no members is removed, the full build holds none.
pub(crate) fn apply_closure_deltas(
    members: &mut OrdMap<(RepoName, TypeName), Vec<PathBuf>>,
    deltas: Vec<ClosureDelta>,
) {
    // Group by key: the paths leaving each key (old-minus-new) and entering it
    // (new-minus-old).
    let mut removals: BTreeMap<(RepoName, TypeName), BTreeSet<PathBuf>> = BTreeMap::new();
    let mut additions: BTreeMap<(RepoName, TypeName), BTreeSet<PathBuf>> = BTreeMap::new();
    for d in deltas {
        for ty in d.old_closure.difference(&d.new_closure) {
            removals
                .entry((d.repo.clone(), ty.clone()))
                .or_default()
                .insert(d.path.clone());
        }
        for ty in d.new_closure.difference(&d.old_closure) {
            additions
                .entry((d.repo.clone(), ty.clone()))
                .or_default()
                .insert(d.path.clone());
        }
    }

    let keys: BTreeSet<(RepoName, TypeName)> =
        removals.keys().chain(additions.keys()).cloned().collect();
    for key in keys {
        let remove = removals.get(&key);
        let add = additions.get(&key);
        let empty = match members.get_mut(&key) {
            Some(v) => {
                if let Some(remove) = remove {
                    v.retain(|p| !remove.contains(p));
                }
                if let Some(add) = add {
                    v.extend(add.iter().cloned());
                }
                v.sort();
                v.is_empty()
            }
            None => {
                // Key absent from the held members: seed it with the sorted
                // additions (an instance entering a key with no prior members).
                if let Some(add) = add {
                    let v: Vec<PathBuf> = add.iter().cloned().collect();
                    if !v.is_empty() {
                        members.insert_mut(key.clone(), v);
                    }
                }
                false
            }
        };
        if empty {
            members.remove_mut(&key);
        }
    }
}

/// Patch closure-membership for one changed instance. The single-delta
/// convenience over [`apply_closure_deltas`], the degenerate one-element batch,
/// so both share the one code path.
///
/// Test-only: the apply path batches through [`apply_closure_deltas`]; the parity
/// tests exercise the batched code path at N=1 through this shim.
#[cfg(test)]
pub(crate) fn apply_closure_delta(
    members: &mut OrdMap<(RepoName, TypeName), Vec<PathBuf>>,
    path: &Path,
    repo: &RepoName,
    old_closure: &BTreeSet<TypeName>,
    new_closure: &BTreeSet<TypeName>,
) {
    apply_closure_deltas(
        members,
        vec![ClosureDelta {
            path: path.to_path_buf(),
            repo: repo.clone(),
            old_closure: old_closure.clone(),
            new_closure: new_closure.clone(),
        }],
    );
}

/// Patch the referenced-name index for one changed source: drop it from the
/// `(repo, key)` entries it no longer names, add it to those it now names,
/// leave the rest untouched. O(the symmetric difference of its name-keys).
///
/// A bucket left empty is removed, the full build never holds one, so an
/// emptied entry must vanish to match it.
pub(crate) fn apply_refname_delta(
    index: &mut OrdMap<(RepoName, NameKey), BTreeSet<PathBuf>>,
    source: &Path,
    old_keys: &BTreeSet<(RepoName, NameKey)>,
    new_keys: &BTreeSet<(RepoName, NameKey)>,
) {
    for key in old_keys.difference(new_keys) {
        let empty = match index.get_mut(key) {
            Some(sources) => {
                sources.remove(source);
                sources.is_empty()
            }
            None => false,
        };
        if empty {
            index.remove_mut(key);
        }
    }
    for key in new_keys.difference(old_keys) {
        match index.get_mut(key) {
            Some(sources) => {
                sources.insert(source.to_path_buf());
            }
            None => {
                let mut sources = BTreeSet::new();
                sources.insert(source.to_path_buf());
                index.insert_mut(key.clone(), sources);
            }
        }
    }
}

/// A [`au_core::RefData`] backed by the held catalog, with an overlay of the
/// dirty instances' fresh parses.
///
/// Validating a dirty instance or one of its dependents looks up a target's
/// claims, body, and records on demand: the overlay first, so a reference to
/// another dirty instance in the same edit sees its new state, then the held
/// catalog. Only the targets actually referenced are touched, no whole-knowledge-base
/// map. Mirrors [`crate::build::ContextMaps`] exactly, the same arms over the
/// same parse, so a recompute validates byte-identically to a full build.
struct CatalogRefData<'a> {
    held: &'a KnowledgeBase,
    overlay: &'a BTreeMap<PathBuf, Arc<FileParse>>,
    deleted: &'a BTreeSet<PathBuf>,
}

impl CatalogRefData<'_> {
    /// The fresh parse for a path when it is dirty, the held one otherwise, and
    /// `None` for a deleted path, so a referrer's validation sees it gone rather
    /// than reading its stale held parse.
    fn parse_of(&self, path: &Path) -> Option<&FileParse> {
        if self.deleted.contains(path) {
            return None;
        }
        match self.overlay.get(path) {
            Some(arc) => Some(arc.as_ref()),
            None => self.held.file_parse(path),
        }
    }
}

impl au_core::RefData for CatalogRefData<'_> {
    fn claims(&self, path: &Path) -> Option<Cow<'_, [TypeName]>> {
        match self.parse_of(path)? {
            FileParse::Instance {
                instance: Some(inst),
                ..
            } => Some(Cow::Owned(
                inst.type_claim.iter().map(|c| c.name.clone()).collect(),
            )),
            _ => None,
        }
    }
    fn body(&self, path: &Path) -> Option<&str> {
        match self.parse_of(path)? {
            FileParse::Instance {
                instance: Some(_),
                body,
                is_markdown: true,
                ..
            } => Some(body.as_str()),
            _ => None,
        }
    }
    fn record_targets(&self, path: &Path) -> Option<Cow<'_, RecordTargets>> {
        match self.parse_of(path)? {
            FileParse::Instance {
                instance: Some(inst),
                ..
            } => Some(Cow::Owned(crate::resolution_build::record_targets_of_kb(
                self.held, path, inst,
            ))),
            _ => None,
        }
    }
}

impl crate::crossref::TargetClaims for CatalogRefData<'_> {
    fn type_claim(&self, path: &Path) -> Option<&au_core::TypeClaim> {
        match self.parse_of(path)? {
            FileParse::Instance {
                instance: Some(inst),
                ..
            } => Some(&inst.type_claim),
            _ => None,
        }
    }
}

/// The recomputed slices and deltas for an instance-only edit. Produced by
/// [`recompute_dirty`], consumed by the apply half. It carries no commit:
/// the carrier splices these into a knowledge base, so the compute stays independent of
/// how the unchanged rest is carried forward.
///
/// `pub` (with opaque fields) only so `examples/rebuild_bench.rs` can carry it
/// from [`recompute_dirty`] to [`apply_recompute`]. Not part of the documented
/// API, see the `#[doc(hidden)]` re-export in the crate root.
pub struct InstanceRecompute {
    /// New catalog entries for the dirty instances.
    pub(crate) catalog: BTreeMap<PathBuf, FileEntry>,
    /// New resolved entry per dirty instance: `Some` to set, `None` to drop, an
    /// instance in an aborted repo holds no resolved analysis.
    pub(crate) resolved: BTreeMap<PathBuf, Option<ResolvedInstance>>,
    /// New `File` diagnostic slice (raw, pre line/col) per dirty instance.
    pub(crate) file_diags: BTreeMap<PathBuf, Vec<Diagnostic>>,
    /// New `Instance` diagnostic slice (raw) per recomputed source: the dirty
    /// instances, plus the dependents whose validation saw a changed identity.
    pub(crate) instance_diags: BTreeMap<PathBuf, Vec<Diagnostic>>,
    /// Backlink-index deltas, one per dirty instance.
    pub(crate) backlink_deltas: Vec<BacklinkDelta>,
    /// Closure-membership deltas, one per non-aborted dirty instance.
    pub(crate) closure_deltas: Vec<ClosureDelta>,
    /// Referenced-name deltas, one per dirty instance.
    pub(crate) refname_deltas: Vec<RefnameDelta>,
    /// Rebuilt reference indices, one per repo whose path set changed.
    pub(crate) index_updates: Vec<(RepoName, RepoIndex)>,
    /// Deleted instances to drop from the catalog, resolved map, and diagnostic
    /// partition. Their reverse-index entries drop via the deltas.
    pub(crate) removed: Vec<PathBuf>,
    /// The cross-repo resolution fold `recompute_dirty` already computed for
    /// validation, carried forward so `apply_recompute` stores it rather than
    /// refolding the identical result. On an instance edit the graphs, repos, and
    /// patched catalog are the same inputs the refold would use, so this value is
    /// byte-identical to a fresh `build_resolution_graphs`, and moving it avoids a
    /// second O(knowledge base) fold per edit.
    pub(crate) resolution_graphs: crate::ir::ResolutionGraphs,
}

/// One dirty source's referenced-name delta: the `(repo, key)` set its links
/// named before and after.
pub(crate) struct RefnameDelta {
    pub(crate) source: PathBuf,
    pub(crate) old_keys: BTreeSet<(RepoName, NameKey)>,
    pub(crate) new_keys: BTreeSet<(RepoName, NameKey)>,
}

/// One dirty source's backlink delta: its outgoing edges before and after.
pub(crate) struct BacklinkDelta {
    pub(crate) source: PathBuf,
    pub(crate) old_targets: BTreeSet<PathBuf>,
    pub(crate) new_edges: BTreeMap<PathBuf, Vec<Backlink>>,
}

/// One dirty instance's closure-membership delta: its closure before and after,
/// keyed within its repo.
pub(crate) struct ClosureDelta {
    pub(crate) path: PathBuf,
    pub(crate) repo: RepoName,
    pub(crate) old_closure: BTreeSet<TypeName>,
    pub(crate) new_closure: BTreeSet<TypeName>,
}

/// The body a REFERRER can read at this path, through [`au_core::RefData`]: an
/// instance's, never a note's.
///
/// A note's blocks are invisible across files, so a note's body is not part of
/// the identity its referrers depend on, and a note-to-note edit triggers no
/// revalidation. Losing or gaining the whole body, by crossing the claim
/// boundary, is a change like any other.
fn instance_body(parse: &FileParse) -> Option<&str> {
    match parse {
        FileParse::Instance { body, .. } => Some(body.as_str()),
        _ => None,
    }
}

/// The held instance at a path, `None` when the path holds no parsed instance.
fn held_instance<'a>(held: &'a KnowledgeBase, path: &Path) -> Option<&'a Instance> {
    match held.file_parse(path)? {
        FileParse::Instance {
            instance: Some(inst),
            ..
        } => Some(inst),
        _ => None,
    }
}

/// Recompute a dirty set's blast radius without touching the rest of the knowledge base.
///
/// Reads and re-parses each dirty path, partitioning edits from adds, then for
/// each recomputes its resolved analysis, its `File` and `Instance` diagnostic
/// slices, and its backlink, closure, and referenced-name deltas. An add
/// reshapes its repo's reference index, so the affected indices are rebuilt and
/// every source whose resolution flips (the edge-flip set) is re-validated,
/// alongside the inbound identity-dependents of an edit. It returns the slices,
/// deltas, and index updates, and commits nothing.
///
/// A path need not be a TYPED instance. An untyped note, an empty file, and a
/// file whose frontmatter broke all carry a catalog entry, a `File` diagnostic
/// slice, outgoing edges, and referenced names in a full build, and every one of
/// those is recomputed here by the same functions the build calls. What such a
/// file does NOT carry is a claim, so it gets no resolved analysis, no closure
/// membership, and no validation, mirroring the build's own per-kind gates. A
/// file that CROSSES that line, gaining or losing its `type:`, changes what its
/// referrers read through [`au_core::RefData`], so it triggers their
/// revalidation like any other identity change.
///
/// `None` falls the caller back to a full rebuild for anything wider than this
/// stage: an add that moves a case-collision diagnostic, a dependent whose parse
/// shape the build's edge inversion does not model, or a
/// held entry that is still THERE but no longer reads (a full build walks it and
/// catalogues it as an unread entry carrying a read-error diagnostic, so
/// splicing it out as a delete would diverge). Only an absent file is a delete.
/// The whole-knowledge-base recompute is always correct.
///
/// `pub` for `examples/rebuild_bench.rs`; not part of the documented API, see
/// the `#[doc(hidden)]` re-export in the crate root.
#[tracing::instrument(skip_all, fields(dirty = dirty.len()))]
pub fn recompute_dirty(
    held: &KnowledgeBase,
    dirty: &BTreeSet<PathBuf>,
    fs: &impl FileSystem,
) -> Option<InstanceRecompute> {
    // Read and re-parse each dirty path, partitioning edits (present in the held
    // catalog) from adds (absent). A read failure of a present path is a delete;
    // of an absent path, spurious.
    let mut overlay: BTreeMap<PathBuf, Arc<FileParse>> = BTreeMap::new();
    let mut catalog: BTreeMap<PathBuf, FileEntry> = BTreeMap::new();
    let mut added: BTreeSet<PathBuf> = BTreeSet::new();
    let mut deleted: BTreeSet<PathBuf> = BTreeSet::new();
    for path in dirty {
        // An ASSET: the build never decodes it, so there is nothing to parse and
        // its catalog entry carries no hash. Only its PRESENCE is load-bearing,
        // as a `RepoIndex` member that resolves `file*` and navigational
        // references. Probed, never read, since it can be arbitrarily large.
        //
        // `is_file`, not mere existence: a watcher reports directory events too,
        // and the walk yields regular files only, so a directory must not be
        // catalogued.
        if !crate::build::build_reads_content(path) {
            match (fs.is_file(path), held.catalog.contains_key(path)) {
                // An add: it joins the catalog and its repo's index.
                (true, false) => {
                    added.insert(path.clone());
                    catalog.insert(
                        path.clone(),
                        crate::build::unread_entry(FileKind::Unclassified),
                    );
                }
                // Present and already catalogued: a content edit, which the
                // engine cannot observe. It contributes nothing. (The no-op gate
                // ahead of this normally skips such a write entirely; arriving
                // here anyway must still be a no-op, not a phantom change.)
                (true, true) => {}
                // A delete.
                (false, true) => {
                    deleted.insert(path.clone());
                }
                // Absent and uncatalogued: a spurious wake, or a directory
                // event. Contributes nothing.
                (false, false) => {}
            }
            continue;
        }
        // An over-cap dirty file is skipped like a present-but-unreadable one:
        // fall back to the full build (`return None`), which catalogues it as an
        // unread entry carrying the file-too-large diagnostic. Delegating rather
        // than reproducing that entry+diagnostic here keeps the incremental
        // result byte-identical to a full build, the parity the oracle asserts,
        // and mirrors the `Some(_) => return None` read-failure arm below. Only a
        // byte-size change moves a file across the fixed cap, and that already
        // dirties it, so this fires exactly when the file itself was edited.
        if crate::build::over_read_cap(fs, path).is_some() {
            return None;
        }
        match fs.read_file(path) {
            Ok(bytes) => {
                let parse = parse_file(path, &bytes);
                // Mirror the build's kind assignment exactly: a file declaring
                // `type:` is an Instance even when its structural parse failed,
                // and everything else routed through the instance pass (a note,
                // an unparsed file) is Unclassified. A type-def cannot reach
                // here, `classify_dirty` excludes it by path, but it is refused
                // rather than assumed away.
                let kind = match &parse {
                    FileParse::Instance { .. } => FileKind::Instance,
                    FileParse::Note { .. } | FileParse::Unparsed { .. } => FileKind::Unclassified,
                    FileParse::TypeDef { .. } => return None,
                };
                let arc = Arc::new(parse);
                catalog.insert(
                    path.clone(),
                    FileEntry {
                        kind,
                        hash: Some(ContentHash::of(&bytes)),
                        parse: arc.clone(),
                        line_index: Some(Arc::new(LineIndex::new(&bytes))),
                        byte_len: Some(bytes.len()),
                    },
                );
                if !held.catalog.contains_key(path) {
                    added.insert(path.clone());
                }
                overlay.insert(path.clone(), arc);
            }
            Err(_) => match held.catalog.get(path) {
                // A failed read is two different facts, and the existence probe
                // is what separates them. GONE is a delete. PRESENT but
                // unreadable is not: a full build still walks the file and still
                // catalogues it, as an unread entry carrying a read-error
                // diagnostic, so splicing it out would diverge and the absent
                // hash would then keep it diverged (the fingerprint drops the
                // path, so no later reconcile sees anything left to reconcile).
                //
                // A hash comparison cannot make this call. An unread entry and
                // an absent file both carry no hash, which is the same
                // content-versus-presence conflation the no-op gate and the
                // scoped fingerprint update each got half of.
                Some(_) if !fs.is_file(path) => {
                    deleted.insert(path.clone());
                }
                Some(_) => return None,
                // Absent and unreadable: a spurious wake, contributes nothing.
                None => {}
            },
        }
    }

    // An add or delete reshapes the affected repos' reference indices. Patch
    // each affected repo's index incrementally, insert the adds and remove the
    // deletes, rather than rebuilding it from the catalog, and assemble the index
    // view the recompute validates against. A case-collision-changing op would
    // need a held-bucket diagnostic splice, so it falls back. The held-indexes
    // clone for the validation view is still O(knowledge base), the Arc-share
    // carrier removes it.
    let mut index_updates: Vec<(RepoName, RepoIndex)> = Vec::new();
    let path_set_change = !added.is_empty() || !deleted.is_empty();
    let rebuilt_indexes: Option<RepoIndexes> = if !path_set_change {
        None
    } else {
        let mut affected: BTreeSet<RepoName> = BTreeSet::new();
        for path in added.iter().chain(deleted.iter()) {
            if let Some(r) = held.repos.repo_of(path) {
                affected.insert(r.name.clone());
            }
        }
        let mut ni = held.indexes.clone();
        for repo_name in &affected {
            let Some(_repo) = held.repos.by_name(repo_name.as_str()) else {
                continue;
            };
            // Patch a clone of the repo's held index. An instance add or delete
            // never touches a registry, so the index's file set changes by
            // exactly the dirty instances. A collision-changing insert or remove
            // falls back.
            let mut index = held.indexes.of(repo_name).clone();
            for path in &added {
                if held
                    .repos
                    .repo_of(path)
                    .is_some_and(|r| &r.name == repo_name)
                    && index.insert(path.clone())
                {
                    return None;
                }
            }
            for path in &deleted {
                if held
                    .repos
                    .repo_of(path)
                    .is_some_and(|r| &r.name == repo_name)
                    && index.remove(path)
                {
                    return None;
                }
            }
            ni.replace(repo_name.clone(), index.clone());
            index_updates.push((repo_name.clone(), index));
        }
        Some(ni)
    };
    let new_indexes: &RepoIndexes = rebuilt_indexes.as_ref().unwrap_or(&held.indexes);

    // The graph-aborted repos, reconstructed from the held outcomes: a repo that
    // aborted at its graph layer. An instance edit or add never changes a repo's
    // outcome, so the held set still holds; the cross-repo resolver skips these.
    let graph_aborted: BTreeSet<RepoName> = held
        .outcomes
        .iter()
        .filter(|(_, o)| **o == BuildOutcome::AbortedAtGraph)
        .map(|(name, _)| name.clone())
        .collect();
    let ref_data = CatalogRefData {
        held,
        overlay: &overlay,
        deleted: &deleted,
    };

    // Fresh cross-repo resolution graphs from the PATCHED catalog: the held
    // entries minus the deleted and the overlaid, plus the overlay's new parses.
    // Computed up front so a dirty instance validates against the current import
    // set, not the held (stale) fold. The graphs themselves are unchanged on an
    // instance edit, so only the catalog-derived seeds move. Bounded to importing
    // repos, and identical to what the full build (and `apply_recompute`, from the
    // patched catalog) computes.
    let fresh_resolution = {
        let held_parses = held
            .catalog
            .iter()
            .filter(|(p, _)| !deleted.contains(p.as_path()) && !overlay.contains_key(p.as_path()))
            .map(|(p, e)| (p.as_path(), e.parse.as_ref()));
        let overlay_parses = overlay
            .iter()
            .map(|(p, parse)| (p.as_path(), parse.as_ref()));
        crate::resolution_build::build_resolution_graphs_from(
            &held.graphs,
            &held.repos,
            held_parses.chain(overlay_parses),
        )
    };

    // Built after the fresh fold and `ref_data`, since a qualified-demand check
    // folds a target's closure over the FRESH resolution graphs and reads the
    // target's parse through `ref_data` (the patched view).
    let resolver = EngineCrossRepoResolver {
        repos: &held.repos,
        indexes: new_indexes,
        graphs: &held.graphs,
        workspaces: &held.workspaces,
        graph_aborted: &graph_aborted,
        resolution_graphs: &fresh_resolution,
        target_claims: &ref_data,
    };

    let mut resolved: BTreeMap<PathBuf, Option<ResolvedInstance>> = BTreeMap::new();
    let mut file_diags: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    let mut instance_diags: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    let mut backlink_deltas: Vec<BacklinkDelta> = Vec::new();
    let mut closure_deltas: Vec<ClosureDelta> = Vec::new();
    let mut refname_deltas: Vec<RefnameDelta> = Vec::new();
    // The inbound dependents and edge-flip sources to re-validate, unioned and
    // deduped. A recomputed path is handled in full, so it is excluded below.
    let mut dependents: BTreeSet<PathBuf> = BTreeSet::new();

    // The recomputed paths: every edit and add that parsed. Collected so the
    // loop does not borrow `overlay` while `ref_data` also borrows it.
    let touched: Vec<PathBuf> = overlay.keys().cloned().collect();
    for path in &touched {
        let new_parse = overlay.get(path).expect("touched path was parsed").as_ref();
        // The typed half of the file, present only for a parsed instance. A
        // note, an empty file, and a structurally-broken one all reach here and
        // all lack a claim, so the build gives them no validation, no resolved
        // entry, and no closure membership; `None` reproduces that.
        let typed = match new_parse {
            FileParse::Instance {
                instance: Some(inst),
                body,
                body_offset,
                is_markdown,
                doc_links,
                ..
            } => Some((
                inst,
                body.as_str(),
                *body_offset,
                *is_markdown,
                doc_links.as_slice(),
            )),
            _ => None,
        };
        let graph = held.graph_for_path(path);
        let aborted = held.outcome_for_path(path).aborted();

        // File slice: the structural parse diagnostics. Every parse shape
        // carries one, so this is unconditional.
        file_diags.insert(path.clone(), new_parse.diagnostics().to_vec());

        // Resolved analysis and the validate half of the Instance slice. An
        // aborted repo validates nothing and holds no resolved entry, mirroring
        // the build's per-repo abort gate; so does a file with no claim.
        //
        // `None` DROPS any held resolved entry, which is exactly what a file
        // that stopped being an instance needs.
        let mut diags: Vec<Diagnostic> = Vec::new();
        if let (false, Some((inst, body, body_offset, is_markdown, doc_links))) = (aborted, typed) {
            // Route the served shape through the FRESH resolution graph, the same
            // one the validation ctx below uses, so the resolved layer and the
            // diagnostics agree and both reflect the current import set (no
            // one-edit staleness).
            let resolution = fresh_resolution.of(&source_repo(&held.repos, path));
            let shape = crate::resolution_build::resolved_effective_shape(
                graph,
                resolution,
                &inst.type_claim,
            );
            resolved.insert(
                path.clone(),
                Some(ResolvedInstance {
                    effective_shape: shape,
                }),
            );
            let ctx = ValidateContext {
                graph,
                repo_index: new_indexes.of(&source_repo(&held.repos, path)),
                ref_data: &ref_data,
                cross_repo: Some(&resolver),
                // The fresh resolution graph, same as the served shape above.
                resolution,
                meta_marker: Some(au_core::MetaMarker {
                    name: crate::engine_schema::ENGINE_META_TYPE,
                    repo: crate::engine_schema::BUILTIN_ENGINE_REPO,
                }),
            };
            diags.extend(validate(&ctx, inst));
            diags.extend(validate_body(&ctx, inst, body, body_offset, is_markdown));
            diags.extend(validate_docstring_links(&ctx, path, doc_links));
            if let Some(root) = held.repos.repo_of(path).map(|r| r.root.clone()) {
                let rel = path.strip_prefix(&root).unwrap_or(path.as_path());
                diags.extend(au_core::location_check::validate_location(
                    ctx.graph,
                    ctx.resolution,
                    inst,
                    rel,
                ));
            }
        } else {
            resolved.insert(path.clone(), None);
        }
        // The cross-boundary `::repo` references key to the Instance slice too,
        // aborted or not, matching the whole-catalog pass which runs over every
        // entry regardless of its repo's abort.
        diags.extend(cross_repo_reference_diagnostics_for(
            new_parse,
            &held.repos,
            new_indexes,
            &held.workspaces,
        ));
        // The `::repo` claim gating keys to this Instance slice too. A type-def
        // edit is a NeedsFull rebuild, so only an instance's identity / inline
        // claims recompute incrementally; parent / shape gating rides the full
        // build. Pass the FRESH fold (built from the patched catalog above), not
        // the held one, for consistency with the resolved layer and validate. The
        // gate's instance arm does not read it today, but fresh is correct if it
        // ever does (a held-stale footgun otherwise).
        diags.extend(cross_repo_type_diagnostics_for(
            new_parse,
            &held.repos,
            &held.graphs,
            &fresh_resolution,
            &held.workspaces,
        ));
        instance_diags.insert(path.clone(), diags);

        // Backlink delta: this source's outgoing edges, the old set resolved
        // against the held index, the new against the rebuilt index, so an added
        // file's appearance flips the resolution of every edge that named it.
        let old_targets: BTreeSet<PathBuf> = match held.file_parse(path) {
            Some(pp) => source_edges(path, pp, &held.indexes, &held.repos, &held.workspaces)
                .into_keys()
                .collect(),
            None => BTreeSet::new(),
        };
        let new_edges = source_edges(path, new_parse, new_indexes, &held.repos, &held.workspaces);
        backlink_deltas.push(BacklinkDelta {
            source: path.clone(),
            old_targets,
            new_edges,
        });

        // Referenced-name delta: the names this source's links mention, old vs
        // new. Maintained regardless of abort, the index inverts every edge's
        // name like the backlink index inverts every resolved edge.
        let src_repo = source_repo(&held.repos, path);
        let old_keys = held
            .file_parse(path)
            .map(|p| source_name_keys(p, &src_repo))
            .unwrap_or_default();
        let new_keys = source_name_keys(new_parse, &src_repo);
        refname_deltas.push(RefnameDelta {
            source: path.clone(),
            old_keys,
            new_keys,
        });

        // Closure delta and the inbound-dependent trigger, both skipped when the
        // repo aborted, matching the build, which builds neither there.
        //
        // Both are computed over the file's IDENTITY rather than over an
        // instance, so a file crossing the claim boundary in either direction is
        // handled: an instance losing its `type:` drops its closure memberships
        // and re-triggers its typed referrers, and a note gaining one adds them.
        if !aborted {
            let Some(repo) = held.repos.repo_of(path).map(|r| r.name.clone()) else {
                continue;
            };
            let old_inst = held_instance(held, path);
            let new_inst = typed.map(|(inst, ..)| inst);
            // A file with no claim is a member of no closure key, so an empty
            // new closure is what drops an ex-instance's memberships.
            let closure_of_opt =
                |i: Option<&Instance>| i.map(|i| instance_closure(graph, i)).unwrap_or_default();
            let old_closure = closure_of_opt(old_inst);
            let new_closure = closure_of_opt(new_inst);

            // The two inbound edges, separated by what triggers them, see the
            // spec's identity-versus-existence split. A whole-file typed ref
            // (`block_id` is none) depends on the target's claim; a block pull
            // depends on the target's addressable blocks, its body or records.
            //
            // Both read the target through [`au_core::RefData`], which surfaces
            // a claim, a body, and record targets ONLY for a parsed instance. So
            // an absent claim is itself a value a referrer sees, and a
            // note-to-note edit correctly triggers nothing: there is nothing
            // about a note that a referrer's validation can read.
            let claim_changed = old_inst.map(|i| &i.type_claim) != new_inst.map(|i| &i.type_claim);
            let record_targets_of = |i: Option<&Instance>| {
                i.map(|i| crate::resolution_build::record_targets_of_kb(held, path, i))
                    .unwrap_or_default()
            };
            let blocks_changed = held.file_parse(path).and_then(instance_body)
                != instance_body(new_parse)
                || record_targets_of(old_inst) != record_targets_of(new_inst);
            for b in held.backlinks(path) {
                if b.slot.is_none() {
                    continue; // navigational, not an identity edge
                }
                let triggered = if b.block_id.is_some() {
                    blocks_changed
                } else {
                    claim_changed
                };
                if triggered {
                    dependents.insert(b.source.clone());
                }
            }

            closure_deltas.push(ClosureDelta {
                path: path.clone(),
                repo,
                old_closure,
                new_closure,
            });
        }
    }

    // Edge-flip sources: an appearing or disappearing file flips the resolution
    // of every source that names it, found through the held referenced-name
    // index. They re-validate against the rebuilt index, the same as the inbound
    // identity-dependents. A deleted file's own referrers are found here too.
    for src in edge_flip_sources(
        &held.referenced_names,
        &held.repos,
        added.iter().chain(deleted.iter()).cloned(),
    ) {
        dependents.insert(src);
    }

    // Re-validate each dependent / edge-flip source, excluding the recomputed
    // paths, which are already handled in full. Its own parse is held, so its
    // resolved analysis, closure, and names do not change; only its Instance
    // slice and its backlink delta move, the slice because its validation reads
    // the changed target through the overlay and the rebuilt index, the delta
    // because an appearing or disappearing target flips its outgoing edges.
    //
    // A changed file is its own edge-flip source when it names another changed
    // file (`edge_flip_sources` does not separate them), so exclude both the
    // recomputed paths (adds and edits, held in the overlay) AND the deleted
    // paths. A deleted file especially must not be re-validated as a dependent:
    // its now-dangling links would fire a phantom diagnostic that apply re-inserts
    // after the removal. A deleted file's edges and removal are handled in full by
    // the `removed` mechanism and the deleted loop below.
    for dep in &dependents {
        if overlay.contains_key(dep) || deleted.contains(dep) {
            continue;
        }
        let Some(parse) = held.file_parse(dep) else {
            continue;
        };
        // A dependent is an instance or a note: the two kinds that form edges and
        // `::repo` diagnostics (the only kinds `source_name_keys` / `source_edges`
        // admit). Mirror the build, which inverts edges and keys `::repo`
        // diagnostics for instances AND notes, so a note dependent must be
        // re-validated, not skipped, or its backlink edge and `::repo` diagnostic
        // diverge from a full build. An unmodelled shape falls back to the full
        // build rather than silently diverging.
        let instance = match parse {
            FileParse::Instance {
                instance: Some(inst),
                body,
                body_offset,
                is_markdown,
                doc_links,
                ..
            } => Some((inst, body, body_offset, is_markdown, doc_links.as_slice())),
            FileParse::Note { .. } => None,
            _ => return None,
        };
        let mut diags: Vec<Diagnostic> = Vec::new();
        // Typed validation runs for an instance only; a note has no type. Skipped
        // when the dependent's repo aborted its graph.
        if let Some((inst, body, body_offset, is_markdown, doc_links)) = instance {
            if !held.outcome_for_path(dep).aborted() {
                let ctx = ValidateContext {
                    graph: held.graph_for_path(dep),
                    repo_index: new_indexes.of(&source_repo(&held.repos, dep)),
                    ref_data: &ref_data,
                    cross_repo: Some(&resolver),
                    // A dependent's own claims are unchanged, so its fold
                    // resolution is identical held or fresh; use the fresh graph
                    // for one consistent source across the recompute.
                    resolution: fresh_resolution.of(&source_repo(&held.repos, dep)),
                    meta_marker: Some(au_core::MetaMarker {
                        name: crate::engine_schema::ENGINE_META_TYPE,
                        repo: crate::engine_schema::BUILTIN_ENGINE_REPO,
                    }),
                };
                diags.extend(validate(&ctx, inst));
                diags.extend(validate_body(&ctx, inst, body, *body_offset, *is_markdown));
                diags.extend(validate_docstring_links(&ctx, dep, doc_links));
                if let Some(root) = held.repos.repo_of(dep).map(|r| r.root.clone()) {
                    let rel = dep.strip_prefix(&root).unwrap_or(dep.as_path());
                    diags.extend(au_core::location_check::validate_location(
                        ctx.graph,
                        ctx.resolution,
                        inst,
                        rel,
                    ));
                }
            }
        }
        diags.extend(cross_repo_reference_diagnostics_for(
            parse,
            &held.repos,
            new_indexes,
            &held.workspaces,
        ));
        diags.extend(cross_repo_type_diagnostics_for(
            parse,
            &held.repos,
            &held.graphs,
            &fresh_resolution,
            &held.workspaces,
        ));
        instance_diags.insert(dep.clone(), diags);

        // The backlink delta, old against the held index, new against the
        // rebuilt one. A no-op for an identity-dependent (its resolution did not
        // change), the flipped edge for an edge-flip source.
        let old_targets: BTreeSet<PathBuf> =
            source_edges(dep, parse, &held.indexes, &held.repos, &held.workspaces)
                .into_keys()
                .collect();
        let new_edges = source_edges(dep, parse, new_indexes, &held.repos, &held.workspaces);
        backlink_deltas.push(BacklinkDelta {
            source: dep.clone(),
            old_targets,
            new_edges,
        });
    }

    // A deleted file leaves the maps entirely: its catalog, resolved, and
    // diagnostic entries are removed (carried as `removed`), and its outgoing
    // edges, closure membership, and referenced names drop via deltas to the
    // empty set. Its inbound edges clear through its referrers' deltas above.
    for path in &deleted {
        let parse = held.file_parse(path);
        let old_targets: BTreeSet<PathBuf> = parse
            .map(|p| {
                source_edges(path, p, &held.indexes, &held.repos, &held.workspaces)
                    .into_keys()
                    .collect()
            })
            .unwrap_or_default();
        backlink_deltas.push(BacklinkDelta {
            source: path.clone(),
            old_targets,
            new_edges: BTreeMap::new(),
        });

        let src_repo = source_repo(&held.repos, path);
        let old_keys = parse
            .map(|p| source_name_keys(p, &src_repo))
            .unwrap_or_default();
        refname_deltas.push(RefnameDelta {
            source: path.clone(),
            old_keys,
            new_keys: BTreeSet::new(),
        });

        if !held.outcome_for_path(path).aborted() {
            if let (Some(inst), Some(repo)) = (
                held_instance(held, path),
                held.repos.repo_of(path).map(|r| r.name.clone()),
            ) {
                let old_closure = instance_closure(held.graph_for_path(path), inst);
                closure_deltas.push(ClosureDelta {
                    path: path.clone(),
                    repo,
                    old_closure,
                    new_closure: BTreeSet::new(),
                });
            }
        }
    }

    Some(InstanceRecompute {
        catalog,
        resolved,
        file_diags,
        instance_diags,
        backlink_deltas,
        closure_deltas,
        refname_deltas,
        index_updates,
        removed: deleted.into_iter().collect(),
        resolution_graphs: fresh_resolution,
    })
}

/// The total sort key of a served diagnostic: `(file, byte-start, code)`, the
/// key the held [`DiagStream`] buckets by. Matches the first three keys of
/// [`crate::diagnostics::sort_diagnostics`]; the within-bucket content
/// tiebreaker orders the rest.
fn served_key(d: &Diagnostic) -> (PathBuf, usize, String) {
    (
        d.span.file.clone(),
        d.span.range.start,
        d.code.as_str().to_string(),
    )
}

/// Remove one served diagnostic from the stream, by value, dropping the bucket
/// when it empties so the stream matches a full build, which holds no empty
/// bucket.
fn remove_served(stream: &mut DiagStream, d: &Diagnostic) {
    let key = served_key(d);
    let empty = match stream.get_mut(&key) {
        Some(bucket) => {
            if let Some(pos) = bucket.iter().position(|x| x == d) {
                bucket.remove(pos);
            }
            bucket.is_empty()
        }
        None => false,
    };
    if empty {
        stream.remove_mut(&key);
    }
}

/// Insert one served diagnostic, keeping its bucket in the content-tiebreaker
/// order [`crate::diagnostics::sort_diagnostics`] produces, so iterating the
/// stream reproduces the sorted full-build stream. The bucket holds diagnostics
/// sharing `(file, start, code)`, near-always one, so the re-sort is trivial.
fn insert_served(stream: &mut DiagStream, d: Diagnostic) {
    let key = served_key(&d);
    match stream.get_mut(&key) {
        Some(bucket) => {
            bucket.push(d);
            bucket.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        }
        None => stream.insert_mut(key, vec![d]),
    }
}

/// Apply a recompute to the held knowledge base, producing the next knowledge base.
///
/// Clone-and-patch: clone the held knowledge base, then splice in the recomputed entries
/// and deltas, patch the closure-dependent transitive drift and the served
/// diagnostic stream, and return it.
///
/// The clone is O(1): the held knowledge base's patched maps are persistent ordered maps
/// that share structure on clone, and its invariant structs are `Arc`-shared.
/// Every patch is O(changed x log N), so the whole apply is O(changed). See
/// [[spec - incremental resolved recompute - a change recomputes its blast
/// radius byte-identical to a full build]].
///
/// `pub` for `examples/rebuild_bench.rs`; not part of the documented API, see
/// the `#[doc(hidden)]` re-export in the crate root.
#[tracing::instrument(skip_all)]
pub fn apply_recompute(held: &KnowledgeBase, rc: InstanceRecompute) -> KnowledgeBase {
    let mut next = held.clone();

    // The diagnostic sources whose bucket this edit changes: the recomputed
    // `File` and `Instance` slices, plus both buckets of each removed instance.
    // Captured before `rc` is consumed; the served stream is spliced per source
    // at the end. Every other source is invariant, so its served diagnostics
    // stay, shared by pointer.
    let changed_diag_sources: BTreeSet<DiagSource> = rc
        .removed
        .iter()
        .flat_map(|p| [DiagSource::File(p.clone()), DiagSource::Instance(p.clone())])
        .chain(rc.file_diags.keys().map(|p| DiagSource::File(p.clone())))
        .chain(
            rc.instance_diags
                .keys()
                .map(|p| DiagSource::Instance(p.clone())),
        )
        .collect();

    // Deleted instances leave the catalog, the resolved map, and their
    // diagnostic slices. Their reverse-index entries drop via the deltas below.
    for path in rc.removed {
        next.catalog.remove_mut(&path);
        next.instances.remove_mut(&path);
        next.diagnostics_by_source
            .remove_mut(&DiagSource::File(path.clone()));
        next.diagnostics_by_source
            .remove_mut(&DiagSource::Instance(path));
    }

    // Catalog and resolved entries for the dirty instances.
    for (path, entry) in rc.catalog {
        next.catalog.insert_mut(path, entry);
    }
    for (path, resolved) in rc.resolved {
        match resolved {
            Some(ri) => {
                next.instances.insert_mut(path, ri);
            }
            None => {
                next.instances.remove_mut(&path);
            }
        }
    }

    // Diagnostic partition: overwrite each recomputed source's slice. An empty
    // slice writes an empty bucket, which the served stream and the parity
    // oracle both ignore, so there is no need to match the build's empty-bucket
    // structure exactly.
    for (path, diags) in rc.file_diags {
        next.diagnostics_by_source
            .insert_mut(DiagSource::File(path), diags);
    }
    for (path, diags) in rc.instance_diags {
        next.diagnostics_by_source
            .insert_mut(DiagSource::Instance(path), diags);
    }

    // Backlink and closure-membership deltas, applied at the affected entries.
    // Backlinks batch by target so a hub's inbound list is rebuilt once per bulk
    // pass, not once per co-dirtied referrer.
    apply_backlink_deltas(&mut next.backlinks, rc.backlink_deltas);
    // Closure membership batches by `(repo, type)` key too, so a shared key's
    // member list is rebuilt once per bulk pass, not once per co-typed instance.
    apply_closure_deltas(&mut next.closure_members, rc.closure_deltas);
    for d in rc.refname_deltas {
        apply_refname_delta(
            &mut next.referenced_names,
            &d.source,
            &d.old_keys,
            &d.new_keys,
        );
    }

    // Rebuilt reference indices for repos whose path set changed.
    for (repo, index) in rc.index_updates {
        next.indexes.replace(repo, index);
    }

    // Splice the served stream per changed source instead of rebuilding it whole.
    // For each changed source: remove its old served diagnostics, re-derived from
    // the held bucket against the held catalog so they match what the served map
    // holds, then insert its new ones, the patched bucket served against the
    // patched catalog. A non-dirty file's line index is unchanged, so an
    // unchanged source's served diagnostics are byte-identical and left in place.
    // O(changed diagnostics x log N), not O(knowledge base).
    for src in &changed_diag_sources {
        if let Some(old_raw) = held.diagnostics_by_source.get(src) {
            for d in crate::build::serve_slice(old_raw, &held.catalog) {
                remove_served(&mut next.served, &d);
            }
        }
        if let Some(new_raw) = next.diagnostics_by_source.get(src) {
            for d in crate::build::serve_slice(new_raw, &next.catalog) {
                insert_served(&mut next.served, d);
            }
        }
    }

    // Store the cross-repo resolution graphs `recompute_dirty` already folded for
    // validation, carried forward rather than refolded. On an instance edit the
    // graphs, repos, and patched catalog are the same inputs a fresh
    // `build_resolution_graphs` would use, so the value is identical, and reusing
    // it drops a second O(knowledge base) fold per edit. Finer per-repo incremental folding
    // (the residual single fold) is tracked separately.
    next.resolution_graphs = Arc::new(rc.resolution_graphs);

    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// A single repo: registry, one type-def, one instance.
    fn kb() -> KnowledgeBase {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/v/a.md", b"---\ntype: note\ntitle: A\n---\n".to_vec());
        build(Path::new("/v"), &fs).unwrap()
    }

    fn set(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn an_instance_edit_is_incremental() {
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/a.md"])),
            DirtyClass::Incremental
        );
    }

    #[test]
    fn a_type_def_edit_needs_full() {
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/type/note.type.yaml"])),
            DirtyClass::NeedsFull
        );
    }

    #[test]
    fn a_new_instance_path_is_incremental() {
        // Absent from the catalog and an instance-candidate path: an add, which
        // the recompute reads and either handles or falls back on.
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/new.md"])),
            DirtyClass::Incremental
        );
    }

    #[test]
    fn a_new_type_def_path_needs_full() {
        // An added type-def is a graph change, excluded by path even when absent.
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/type/new.type.yaml"])),
            DirtyClass::NeedsFull
        );
    }

    #[test]
    fn a_registry_edit_needs_full() {
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/.arsumbris/repo.yaml"])),
            DirtyClass::NeedsFull
        );
    }

    #[test]
    fn a_mixed_set_needs_full() {
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/a.md", "/v/type/note.type.yaml"])),
            DirtyClass::NeedsFull
        );
    }

    #[test]
    fn an_empty_set_needs_full() {
        assert_eq!(classify_dirty(&kb(), &set(&[])), DirtyClass::NeedsFull);
    }

    #[test]
    fn a_held_untyped_note_is_incremental() {
        // The catalog holds a note as `Unclassified`, but it is a file the parse
        // layer handles, so its blast radius is the one the recompute models: a
        // path-set membership plus its own outgoing edges.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/prose.md", b"# Prose\n".to_vec());
        let held = build(Path::new("/v"), &fs).unwrap();
        assert_eq!(
            held.catalog.get(Path::new("/v/prose.md")).unwrap().kind,
            FileKind::Unclassified,
            "fixture must hold a note, not an instance"
        );
        assert_eq!(
            classify_dirty(&held, &set(&["/v/prose.md"])),
            DirtyClass::Incremental
        );
    }

    #[test]
    fn an_asset_is_incremental() {
        // An asset's blast radius is strictly NARROWER than a note's: a pure
        // path-set change, with no parse, no diagnostics, and no outgoing edges.
        // The recompute probes its presence rather than reading it.
        assert_eq!(
            classify_dirty(&kb(), &set(&["/v/photo.png"])),
            DirtyClass::Incremental
        );
    }

    #[test]
    fn a_directory_event_contributes_nothing() {
        // A watcher reports directory events, and the walk yields regular files
        // only, so a directory must never reach the catalog. `is_file` is what
        // separates the two; a mere existence probe would catalogue the
        // directory and diverge from a full build.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/sub/a.md", b"---\ntype: note\n---\n".to_vec());
        let held = build(Path::new("/v"), &fs).unwrap();

        let rc = recompute_dirty(&held, &set(&["/v/sub"]), &fs).expect("handled");
        let next = apply_recompute(&held, rc);
        assert!(
            !next.catalog.contains_key(Path::new("/v/sub")),
            "a directory must never be catalogued"
        );
        crate::ir::assert_kb_parity(&next, &build(Path::new("/v"), &fs).unwrap());
    }

    /// A [`MemoryFileSystem`] whose read fails for one path, so the walk still
    /// lists the file but the build cannot read it.
    ///
    /// The only way to produce the `hash: None` UNREAD catalog entry, which
    /// `MemoryFileSystem` alone cannot express (its `read_file` succeeds for
    /// every key its `walk_files` returns).
    struct UnreadableAt {
        inner: MemoryFileSystem,
        path: PathBuf,
    }

    impl au_parser::FileSystem for UnreadableAt {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            if path == self.path {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unreadable by test",
                ));
            }
            self.inner.read_file(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            // Presence, not readability: the walk still lists the file.
            self.inner.is_file(path)
        }
        fn walk_files(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<au_parser::Walk> {
            self.inner.walk_files(root, filter)
        }
        fn walk_scope_boundaries(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<(au_parser::ScopeBoundaries, Vec<au_parser::WalkError>)> {
            self.inner.walk_scope_boundaries(root, filter)
        }
    }

    #[test]
    fn a_removed_directory_expands_to_the_files_that_were_under_it() {
        // The unit the wiring rests on. A folder event names only the folder,
        // and the folder is not a node, so without this every gate downstream
        // reads it as "nothing changed".
        // Everything that SURVIVES the delete. `MemoryFileSystem` has no
        // removal, so the after-state is built rather than edited.
        let survivors = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            // THE PREFIX HAZARD, as a fixture. A byte-wise prefix test would
            // drag `/v/subdir/c.md` into a delete of `/v/sub`.
            fs.insert("/v/subdir/c.md", b"---\ntype: note\n---\n".to_vec());
            fs.insert("/v/keep.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut fs = MemoryFileSystem::new();
        survivors(&mut fs);
        fs.insert("/v/sub/a.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/sub/b.md", b"---\ntype: note\n---\n".to_vec());
        let held = build(Path::new("/v"), &fs).unwrap();

        // The directory is gone; everything else still reads.
        let mut after = MemoryFileSystem::new();
        survivors(&mut after);

        assert_eq!(
            expand_removed_directories(&held, &set(&["/v/sub"]), &after),
            set(&["/v/sub/a.md", "/v/sub/b.md"]),
            "a removed directory becomes its catalogued children, and `/v/subdir` is not one"
        );
        // A live file speaks for itself, and an unknown path stays as itself.
        assert_eq!(
            expand_removed_directories(&held, &set(&["/v/keep.md", "/v/ghost"]), &after),
            set(&["/v/keep.md", "/v/ghost"]),
            "only a removed directory with catalogued children expands"
        );
    }

    #[test]
    fn a_held_entry_that_never_read_falls_back() {
        // An unread entry and a deleted file both present as "the read failed",
        // and the catalog cannot tell them apart: both carry no hash. A full
        // build RE-WALKS the unreadable file and re-inserts its unread entry, so
        // splicing it out as a delete would diverge. The existence probe is what
        // bails, and it is the SAME probe that
        // `a_present_but_unreadable_entry_falls_back` exercises, on an entry that
        // does carry a hash — which is why the hash was never the signal.
        let mut inner = MemoryFileSystem::new();
        inner.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        inner.insert("/v/prose.md", b"# Prose\n".to_vec());
        let fs = UnreadableAt {
            inner,
            path: PathBuf::from("/v/prose.md"),
        };
        let held = build(Path::new("/v"), &fs).unwrap();
        assert!(
            held.catalog
                .get(Path::new("/v/prose.md"))
                .expect("the walk still lists it")
                .hash
                .is_none(),
            "fixture must hold an UNREAD entry"
        );
        assert!(
            recompute_dirty(&held, &set(&["/v/prose.md"]), &fs).is_none(),
            "a still-unreadable held entry is not a delete"
        );
    }

    #[test]
    fn a_present_but_unreadable_entry_falls_back() {
        // The mirror of the test above, and the case a hash comparison gets
        // WRONG: the file WAS readable at build, so its entry carries a hash,
        // and it is still on disk. A full build walks it, fails the read, and
        // catalogues an unread entry carrying `repo-file-read-error`. Splicing
        // it out as a delete diverges, and does not converge back: the
        // fingerprint drops the path too, so every later reconcile finds
        // nothing left to reconcile.
        //
        // TWO filesystems, because one cannot express the transition.
        // `UnreadableAt` fails the read from the start, so a build over it can
        // only ever produce the UNREAD entry the test above covers. The held
        // state has to come from a plain `MemoryFileSystem`.
        //
        // A NOTE and an INSTANCE, because they reach the arm by different
        // routes and only one of them was ever handled correctly. A held
        // non-instance used to bail unconditionally; widening the fast path
        // past typed instances is what walked it into the delete arm.
        for (label, bytes) in [
            ("a note", &b"# Prose\n"[..]),
            ("an instance", &b"---\ntype: note\n---\n"[..]),
        ] {
            let mut inner = MemoryFileSystem::new();
            inner.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            inner.insert("/v/type/note.type.yaml", b"fields: {}\n".to_vec());
            inner.insert("/v/prose.md", bytes.to_vec());

            // Held state over the READABLE filesystem, so the entry has a hash.
            let held = build(Path::new("/v"), &inner).unwrap();
            assert!(
                held.catalog
                    .get(Path::new("/v/prose.md"))
                    .expect("catalogued")
                    .hash
                    .is_some(),
                "{label}: fixture must hold a READ entry, else this is the other test"
            );

            // Same bytes, same walk; only the read now fails.
            let unreadable = UnreadableAt {
                inner,
                path: PathBuf::from("/v/prose.md"),
            };
            assert!(
                unreadable.is_file(Path::new("/v/prose.md")),
                "{label}: fixture must keep the file PRESENT, only unreadable"
            );
            assert!(
                recompute_dirty(&held, &set(&["/v/prose.md"]), &unreadable).is_none(),
                "{label}: a present-but-unreadable file is not a delete"
            );
        }
    }

    #[test]
    fn backlink_and_closure_deltas_match_a_from_scratch_build() {
        // a.md moves its `link` from t1 to t2 and its claim from note to
        // special. The backlink and closure deltas, applied to the prior knowledge base's
        // held state, must equal a from-scratch build of the edited knowledge base.
        use crate::backlinks::source_edges;
        use crate::parse::FileParse;

        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  link?: note*\n".to_vec(),
            );
            fs.insert("/v/type/special.type.yaml", b"extends: note\n".to_vec());
            fs.insert("/v/t1.md", b"---\ntype: note\n---\n".to_vec());
            fs.insert("/v/t2.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut fs_a = MemoryFileSystem::new();
        base(&mut fs_a);
        fs_a.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[t1]]\"\n---\n".to_vec(),
        );
        let va = build(Path::new("/v"), &fs_a).unwrap();

        let mut fs_b = MemoryFileSystem::new();
        base(&mut fs_b);
        fs_b.insert(
            "/v/a.md",
            b"---\ntype: special\nlink: \"[[t2]]\"\n---\n".to_vec(),
        );
        let vb = build(Path::new("/v"), &fs_b).unwrap();

        let a = Path::new("/v/a.md");
        let inst_of = |v: &KnowledgeBase| -> Instance {
            match v.catalog.get(a).unwrap().parse.as_ref() {
                FileParse::Instance {
                    instance: Some(i), ..
                } => i.clone(),
                _ => panic!("a.md is an instance"),
            }
        };

        // Backlink delta.
        let old_targets: BTreeSet<PathBuf> = source_edges(
            a,
            va.catalog.get(a).unwrap().parse.as_ref(),
            &va.indexes,
            &va.repos,
            &va.workspaces,
        )
        .into_keys()
        .collect();
        let new_edges = source_edges(
            a,
            vb.catalog.get(a).unwrap().parse.as_ref(),
            &vb.indexes,
            &vb.repos,
            &vb.workspaces,
        );
        let mut backlinks = va.backlinks.clone();
        apply_backlink_delta(&mut backlinks, a, &old_targets, new_edges);
        // Backlink is not PartialEq; compare the deterministic Debug form.
        assert_eq!(
            format!("{backlinks:?}"),
            format!("{:?}", vb.backlinks),
            "backlink delta equals a from-scratch build"
        );

        // Closure delta: note -> special gains the `special` membership key.
        let old_closure = instance_closure(va.graph_for_path(a), &inst_of(&va));
        let new_closure = instance_closure(vb.graph_for_path(a), &inst_of(&vb));
        let repo = va.repos.repo_of(a).unwrap().name.clone();
        let mut members = va.closure_members.clone();
        apply_closure_delta(&mut members, a, &repo, &old_closure, &new_closure);
        assert_eq!(
            members, vb.closure_members,
            "closure delta equals a from-scratch build"
        );
    }

    #[test]
    fn recompute_slices_match_a_from_scratch_build() {
        use crate::ir::DiagSource;

        // a references b through a `note*` typed slot. Editing b's claim from
        // `note` to `other` (whose closure excludes note) flips a's reference to
        // `reference-target-type-mismatch`. b is the dirty instance, a is its
        // inbound identity-dependent, so the recompute must re-validate a.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  link?: note*\n".to_vec(),
            );
            fs.insert(
                "/v/type/other.type.yaml",
                b"fields:\n  x?: String\n".to_vec(),
            );
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
            );
        };
        let mut fs_a = MemoryFileSystem::new();
        base(&mut fs_a);
        fs_a.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        let va = build(Path::new("/v"), &fs_a).unwrap();

        let mut fs_b = MemoryFileSystem::new();
        base(&mut fs_b);
        fs_b.insert("/v/b.md", b"---\ntype: other\n---\n".to_vec());
        let vb = build(Path::new("/v"), &fs_b).unwrap();

        let a = PathBuf::from("/v/a.md");
        let b = PathBuf::from("/v/b.md");
        let rc = recompute_dirty(&va, &set(&["/v/b.md"]), &fs_b).expect("instance-only");

        let inst_slice = |v: &KnowledgeBase, p: &PathBuf| -> Vec<Diagnostic> {
            v.diagnostics_by_source
                .get(&DiagSource::Instance(p.clone()))
                .cloned()
                .unwrap_or_default()
        };
        let file_slice = |v: &KnowledgeBase, p: &PathBuf| -> Vec<Diagnostic> {
            v.diagnostics_by_source
                .get(&DiagSource::File(p.clone()))
                .cloned()
                .unwrap_or_default()
        };

        // The dirty instance b: resolved entry, File and Instance slices.
        assert_eq!(
            format!("{:?}", rc.resolved[&b]),
            format!("{:?}", Some(vb.instances[&b].clone())),
            "b resolved entry matches from-scratch"
        );
        assert_eq!(
            format!("{:?}", rc.file_diags[&b]),
            format!("{:?}", file_slice(&vb, &b)),
            "b File slice matches from-scratch"
        );
        assert_eq!(
            format!("{:?}", rc.instance_diags[&b]),
            format!("{:?}", inst_slice(&vb, &b)),
            "b Instance slice matches from-scratch"
        );

        // The dependent a was re-validated and now equals its from-scratch slice.
        assert!(
            rc.instance_diags.contains_key(&a),
            "a was re-validated as an inbound dependent"
        );
        assert_eq!(
            format!("{:?}", rc.instance_diags[&a]),
            format!("{:?}", inst_slice(&vb, &a)),
            "dependent a Instance slice matches from-scratch"
        );
        // Not vacuous: the edit actually moves a's diagnostics.
        assert_ne!(
            format!("{:?}", inst_slice(&va, &a)),
            format!("{:?}", inst_slice(&vb, &a)),
            "the edit changes a's diagnostics"
        );

        // The closure delta (b: note -> other) applied to the held members.
        let mut members = va.closure_members.clone();
        for d in &rc.closure_deltas {
            apply_closure_delta(
                &mut members,
                &d.path,
                &d.repo,
                &d.old_closure,
                &d.new_closure,
            );
        }
        assert_eq!(
            members, vb.closure_members,
            "closure delta matches from-scratch"
        );

        // The backlink delta (b has no outgoing edges) applied to the held index.
        let mut backlinks = va.backlinks.clone();
        for d in &rc.backlink_deltas {
            apply_backlink_delta(
                &mut backlinks,
                &d.source,
                &d.old_targets,
                d.new_edges.clone(),
            );
        }
        assert_eq!(
            format!("{backlinks:?}"),
            format!("{:?}", vb.backlinks),
            "backlink delta matches from-scratch"
        );
    }

    /// Build before from `entry`, apply the recompute of `dirty` read from
    /// after, and assert the spliced knowledge base is byte-identical to a from-scratch
    /// build of after. `entry` is the build entry point, a repo root or a
    /// workspace manifest's directory.
    fn assert_parity_at(
        entry: &str,
        before: &MemoryFileSystem,
        after: &MemoryFileSystem,
        dirty: &[&str],
    ) {
        let held = build(Path::new(entry), before).unwrap();
        let rc = recompute_dirty(&held, &set(dirty), after).expect("dirty set is instance-only");
        let incremental = apply_recompute(&held, rc);
        let scratch = build(Path::new(entry), after).unwrap();
        crate::ir::assert_kb_parity(&incremental, &scratch);
    }

    /// [`assert_parity_at`] for a single-repo knowledge base rooted at `/v`.
    fn assert_parity(before: &MemoryFileSystem, after: &MemoryFileSystem, dirty: &[&str]) {
        assert_parity_at("/v", before, after, dirty);
    }

    #[test]
    fn adding_the_first_peer_import_incrementally_folds_freshly() {
        // The Option-3 staleness fix. `app` peers `base` (which owns
        // `note { title }`). Before: `app/n.md` claims an own type, no import, so
        // app has NO resolution graph. After: it claims `note::base` with no
        // `title`. The incremental recompute must fold `note` FRESH and flag the
        // missing peer field — not read the held (import-less) fold. Parity with a
        // from-scratch build of `after` proves it end to end.
        let mut before = MemoryFileSystem::new();
        before.insert("/v/.arsumbris/repo.yaml", "name: v\n");
        before.insert(
            "/v/.arsumbris/workspace.yaml",
            "edit:\n  - v\n  - base\n  - app\n",
        );
        before.insert("/v/base/.arsumbris/repo.yaml", "name: base\n");
        before.insert("/v/base/type/note.type.yaml", "fields:\n  title: String\n");
        before.insert(
            "/v/app/.arsumbris/repo.yaml",
            "name: app\ndeps:\n  - name: base\n",
        );
        before.insert("/v/app/type/thing.type.yaml", "fields:\n  x: String\n");
        before.insert("/v/app/n.md", "---\ntype: thing\nx: hi\n---\n");

        let mut after = before.clone();
        after.insert("/v/app/n.md", "---\ntype: note::base\n---\n");

        let held = build(Path::new("/v"), &before).unwrap();
        // app's `repo.yaml` is a typed instance folding the builtin au.engine.repo,
        // so app already has a resolution graph before the edit; but it authors no
        // `::base`, so base's `note` is not in it yet — the edit folds it freshly.
        let rg = held
            .resolution_graphs
            .of(&RepoName("app".into()))
            .expect("repo.yaml folds au.engine.repo");
        assert!(
            rg.resolve_authored(&au_core::TypeName("note".into()), Some("base"))
                .is_none(),
            "app does not import base's note before the edit"
        );

        let rc =
            recompute_dirty(&held, &set(&["/v/app/n.md"]), &after).expect("instance-only edit");
        let incremental = apply_recompute(&held, rc);

        assert!(
            incremental
                .diagnostics()
                .any(|d| d.code.as_str() == "required-field-absent"
                    && d.span.file == Path::new("/v/app/n.md")),
            "the freshly-folded peer field `title` must be flagged missing: {:?}",
            incremental
                .diagnostics()
                .map(|d| (d.code.as_str(), d.span.file.clone()))
                .collect::<Vec<_>>()
        );

        let scratch = build(Path::new("/v"), &after).unwrap();
        crate::ir::assert_kb_parity(&incremental, &scratch);
    }

    #[test]
    fn parity_cross_repo_slot_pinned_nested_record_edit() {
        // The shape the scale fuzzer is blind to: a slot-pinned nested
        // record inside a CROSS-REPO-claiming host. `base` owns
        // `research-extraction` (slot `concepts?: concept-candidate&[]`) and
        // `concept-candidate`. `app/extraction.md` claims `research-extraction::base`
        // with a claim-LESS nested record (`^: c1`, owner-pinned to
        // `concept-candidate::base`), and `app/cite.md` type-checks a `^^c1`
        // block-referent against `concept-candidate::base*`. Owner-relative
        // record-target resolution feeds HELD state (the addressable-record index
        // and the dependent edge), so the incremental recompute of an edit to the
        // host must be byte-identical to a full build. scale_fuzz never generates
        // this shape (its cross-repo instances fill a reference slot, its
        // slot-pinned nested records sit on own-repo hosts), so it is asserted here.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/.arsumbris/workspace.yaml",
                b"edit:\n  - v\n  - base\n  - app\n".to_vec(),
            );
            fs.insert("/v/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
            fs.insert(
                "/v/base/type/research-extraction.type.yaml",
                b"fields:\n  concepts?: concept-candidate&[]\n".to_vec(),
            );
            fs.insert(
                "/v/base/type/concept-candidate.type.yaml",
                b"fields:\n  salience?: String\n".to_vec(),
            );
            fs.insert(
                "/v/app/.arsumbris/repo.yaml",
                b"name: app\ndeps:\n  - name: base\n".to_vec(),
            );
            fs.insert(
                "/v/app/type/citation.type.yaml",
                b"fields:\n  evidence: concept-candidate::base*\n".to_vec(),
            );
            fs.insert(
                "/v/app/cite.md",
                b"---\ntype: citation\nevidence: \"[[extraction^^c1]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert(
            "/v/app/extraction.md",
            b"---\ntype: research-extraction::base\nconcepts:\n  - ^: c1\n    salience: focal\n---\n"
                .to_vec(),
        );
        // Rename the nested record's block-id c1 -> c2; cite.md's `^^c1` now dangles.
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert(
            "/v/app/extraction.md",
            b"---\ntype: research-extraction::base\nconcepts:\n  - ^: c2\n    salience: focal\n---\n"
                .to_vec(),
        );

        // Non-vacuity: before, `^^c1` resolves owner-relative (c1 pins to
        // concept-candidate::base, satisfying the concept-candidate::base* demand),
        // so cite.md is clean; after, the rename dangles it to block-id-not-found.
        // Proves the edit truly exercises the cross-repo slot-pinned nested path.
        let held_before = build(Path::new("/v"), &before).unwrap();
        assert!(
            !held_before
                .diagnostics()
                .any(|d| d.span.file == Path::new("/v/app/cite.md")
                    && d.code.as_str() == "block-id-not-found"),
            "before: the ^^c1 block-referent resolves owner-relative, cite.md clean: {:?}",
            held_before
                .diagnostics()
                .map(|d| (d.code.as_str().to_string(), d.span.file.clone()))
                .collect::<Vec<_>>()
        );
        let scratch_after = build(Path::new("/v"), &after).unwrap();
        assert!(
            scratch_after
                .diagnostics()
                .any(|d| d.span.file == Path::new("/v/app/cite.md")
                    && d.code.as_str() == "block-id-not-found"),
            "after: renaming the block-id dangles cite.md's ^^c1: {:?}",
            scratch_after
                .diagnostics()
                .map(|d| (d.code.as_str().to_string(), d.span.file.clone()))
                .collect::<Vec<_>>()
        );

        // The incremental recompute of the host edit must equal a full build.
        assert_parity(&before, &after, &["/v/app/extraction.md"]);
    }

    /// Registry plus a `note` type with a typed self-reference slot.
    fn linked_base(fs: &mut MemoryFileSystem) {
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title?: String\n  link?: note*\n".to_vec(),
        );
    }

    #[test]
    fn parity_prose_only_edit() {
        // A body-only edit: no claim, no reference, no dependent revalidation.
        let mut before = MemoryFileSystem::new();
        linked_base(&mut before);
        before.insert("/v/a.md", b"---\ntype: note\n---\nhello\n".to_vec());
        before.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        linked_base(&mut after);
        after.insert("/v/a.md", b"---\ntype: note\n---\ngoodbye\n".to_vec());
        after.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/a.md"]);
    }

    #[test]
    fn an_edit_splices_only_the_changed_sources_served_diagnostics() {
        // Every instance carries a standing reference-target-missing (a typed
        // `link` to a target that does not exist). Editing one instance must
        // splice only its served diagnostics into the held stream, leaving every
        // other instance's in place. This is the served-stream splice over a
        // knowledge base with standing diagnostics, the case the dirty benchmark measures:
        // an apply that touches O(changed), not the whole stream, must still be
        // byte-identical to a full build.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            for i in 0..8 {
                fs.insert(
                    &format!("/v/n{i}.md"),
                    format!("---\ntype: note\ntitle: N{i}\nlink: \"[[ghost{i}]]\"\n---\n")
                        .into_bytes(),
                );
            }
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);

        // Non-vacuity: the fixture genuinely carries a standing diagnostic per
        // instance, so the splice has unchanged sources to preserve.
        let held = build(Path::new("/v"), &before).unwrap();
        assert!(
            held.diagnostics_len() >= 8,
            "fixture should carry a standing diagnostic per instance, got {}",
            held.diagnostics_len()
        );

        let mut after = MemoryFileSystem::new();
        base(&mut after);
        // n0's dangling target changes, so its own diagnostic moves; n1..n7 keep
        // theirs untouched through the splice.
        after.insert(
            "/v/n0.md",
            b"---\ntype: note\ntitle: N0\nlink: \"[[elsewhere]]\"\n---\n".to_vec(),
        );
        assert_parity(&before, &after, &["/v/n0.md"]);
    }

    #[test]
    fn parity_claim_edit_flips_a_dependent() {
        // b's claim changes from note to a non-note type, flipping the referrer
        // a's typed reference to a mismatch. Exercises the inbound-identity edge.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/type/other.type.yaml",
                b"fields:\n  x?: String\n".to_vec(),
            );
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/b.md", b"---\ntype: other\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/b.md"]);
    }

    #[test]
    fn parity_referrer_edit_moves_its_own_edge() {
        // Editing the source of a reference: a's link moves from b to c, so a's
        // outgoing edge flips and the backlinks at b and c both change.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
            fs.insert("/v/c.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
        );
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[c]]\"\n---\n".to_vec(),
        );
        assert_parity(&before, &after, &["/v/a.md"]);
    }

    #[test]
    fn parity_multi_file_edit_sees_new_state_through_the_overlay() {
        // a references b, and both are dirty in one edit: a's body changes and
        // b's claim changes. Validating a must see b's NEW claim, the overlay.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/type/other.type.yaml",
                b"fields:\n  x?: String\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[b]]\"\n---\nhi\n".to_vec(),
        );
        before.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[b]]\"\n---\nbye\n".to_vec(),
        );
        after.insert("/v/b.md", b"---\ntype: other\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/a.md", "/v/b.md"]);
    }

    #[test]
    fn parity_add_resolves_a_dangling_referrer() {
        // Adding target.md resolves a's dangling `[[target]]`, clearing its
        // reference-target-missing and forming a backlink, plus the added file's
        // own resolved entry. The edge-flip set finds a through its named edge.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[target]]\"\n---\n".to_vec(),
            );
            fs.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/target.md"]);
    }

    #[test]
    fn parity_delete_dangles_a_referrer() {
        // Deleting target.md dangles a's `[[target]]`, firing
        // reference-target-missing, dropping its backlink, and removing the
        // file's own catalog, resolved, and diagnostic entries.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[target]]\"\n---\n".to_vec(),
            );
            fs.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        assert_parity(&before, &after, &["/v/target.md"]);
    }

    #[test]
    fn parity_rename_moves_a_target() {
        // A rename is a delete of the old path and an add of the new in one dirty
        // set: target leaves (a's `[[target]]` dangles) and renamed appears (b's
        // `[[renamed]]` resolves). Both edge-flips land in the same recompute.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[target]]\"\n---\n".to_vec(),
            );
            fs.insert(
                "/v/b.md",
                b"---\ntype: note\nlink: \"[[renamed]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/renamed.md", b"---\ntype: note\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/target.md", "/v/renamed.md"]);
    }

    #[test]
    fn parity_untyped_note_referrer_to_a_path_set_change() {
        // The referrer is an UNTYPED note (no `type:` claim) that links the
        // target through a body wikilink. A note forms a backlink edge in the
        // full build exactly as a typed instance does, so the edge-flip path must
        // re-validate a note dependent, not skip it. Regression for the
        // dropped-note-dependent bug: add, delete, and rename of the target each
        // diverged from a full build before the fix.
        let note_and_type = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  title?: String\n".to_vec(),
            );
            // Untyped markdown note: no `type:` claim, a body wikilink to target.
            fs.insert("/v/note.md", b"# Note\n\nsee [[target]].\n".to_vec());
        };

        // Non-vacuity: the untyped note genuinely forms a target backlink in a
        // full build, so the fast path has an edge to diverge on.
        let mut full = MemoryFileSystem::new();
        note_and_type(&mut full);
        full.insert("/v/target.md", b"---\ntype: note\ntitle: T\n---\n".to_vec());
        let v = build(Path::new("/v"), &full).unwrap();
        assert!(
            v.backlinks
                .get(&PathBuf::from("/v/target.md"))
                .is_some_and(|edges| edges
                    .iter()
                    .any(|b| b.source == PathBuf::from("/v/note.md"))),
            "fixture must form a note->target backlink (is note.md a FileParse::Note?)"
        );

        let mut without = MemoryFileSystem::new();
        note_and_type(&mut without);
        let mut with = MemoryFileSystem::new();
        note_and_type(&mut with);
        with.insert("/v/target.md", b"---\ntype: note\ntitle: T\n---\n".to_vec());

        // Add: target appears, the note's edge must form.
        assert_parity(&without, &with, &["/v/target.md"]);
        // Delete: target vanishes, the note's stale edge must drop.
        assert_parity(&with, &without, &["/v/target.md"]);

        // Rename: target -> renamed; the note still links `[[target]]`, which now
        // dangles, so its edge to the old name must drop (both basenames flip).
        let mut renamed = MemoryFileSystem::new();
        note_and_type(&mut renamed);
        renamed.insert(
            "/v/renamed.md",
            b"---\ntype: note\ntitle: T\n---\n".to_vec(),
        );
        assert_parity(&with, &renamed, &["/v/target.md", "/v/renamed.md"]);
    }

    #[test]
    fn parity_add_an_empty_file_flips_a_dangling_referrer() {
        // The empty-file case, as a parity assertion. An empty `.md` splits to a
        // note with an empty body, so it claims nothing, but it IS a path-set
        // member: `a`'s typed `[[ghost]]` stops dangling and starts mismatching,
        // because the target now exists and its closure excludes `note`.
        let mut before = MemoryFileSystem::new();
        linked_base(&mut before);
        before.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[ghost]]\"\n---\nsee [[ghost]].\n".to_vec(),
        );
        let mut after = before.clone();
        after.insert("/v/ghost.md", Vec::new());

        // Non-vacuity: the add genuinely moves `a`'s diagnostics, so a fast path
        // that ignored the empty file would be caught.
        let codes = |fs: &MemoryFileSystem| -> Vec<String> {
            build(Path::new("/v"), fs)
                .unwrap()
                .diagnostics()
                .map(|d| d.code.as_str().to_string())
                .collect()
        };
        assert_ne!(
            codes(&before),
            codes(&after),
            "the empty file must change what `a` reports"
        );

        assert_parity(&before, &after, &["/v/ghost.md"]);
        // And the delete side, the same file going away again.
        assert_parity(&after, &before, &["/v/ghost.md"]);
    }

    #[test]
    fn parity_add_and_delete_an_asset() {
        // An asset is catalogued but never decoded, so its whole contribution is
        // a path-set membership: it enters the `RepoIndex` and resolves a
        // `file*` reference, and nothing else. `a` holds a typed `file*` slot
        // pointing at it, so the add clears a dangling reference and the delete
        // restores it.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  asset?: file*\n".to_vec(),
            );
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nasset: \"[[photo.png]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/photo.png", vec![0x89, b'P', b'N', b'G']);

        // Non-vacuity: the asset's presence really does decide `a`'s diagnostic.
        let codes = |fs: &MemoryFileSystem| -> Vec<String> {
            build(Path::new("/v"), fs)
                .unwrap()
                .diagnostics()
                .map(|d| d.code.as_str().to_string())
                .collect()
        };
        assert!(
            codes(&before)
                .iter()
                .any(|c| c == "reference-target-missing"),
            "without the asset the reference must dangle: {:?}",
            codes(&before)
        );
        assert!(
            !codes(&after)
                .iter()
                .any(|c| c == "reference-target-missing"),
            "with the asset present the reference must resolve: {:?}",
            codes(&after)
        );

        assert_parity(&before, &after, &["/v/photo.png"]);
        assert_parity(&after, &before, &["/v/photo.png"]);
    }

    #[test]
    fn parity_an_asset_edit_changes_nothing() {
        // The engine never decodes an asset, so different bytes at the same path
        // must produce a byte-identical knowledge base. The version-advance half
        // of this is `engine::tests::an_asset_content_edit_advances_nothing`;
        // this is the state half, and it holds even when the no-op gate is
        // bypassed and the recompute runs anyway.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  asset?: file*\n".to_vec(),
            );
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nasset: \"[[photo.png]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/photo.png", b"original".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/photo.png", b"a completely different image".to_vec());

        assert_parity(&before, &after, &["/v/photo.png"]);
    }

    #[test]
    fn parity_add_and_delete_an_untyped_note() {
        // The note carries its OWN outgoing edge, so its add must enter the
        // backlink and referenced-name indices and its delete must leave both.
        let mut before = MemoryFileSystem::new();
        linked_base(&mut before);
        before.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = before.clone();
        after.insert("/v/prose.md", b"# Prose\n\nsee [[target]].\n".to_vec());

        assert_parity(&before, &after, &["/v/prose.md"]);
        assert_parity(&after, &before, &["/v/prose.md"]);
    }

    #[test]
    fn parity_untyped_note_edit_moves_its_own_edge() {
        // A note's body edit is a pure outgoing-edge move: no claim, so no
        // validation and no dependent revalidation, but the backlink index must
        // still follow it.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert("/v/t1.md", b"---\ntype: note\n---\n".to_vec());
            fs.insert("/v/t2.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/prose.md", b"# Prose\n\nsee [[t1]].\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/prose.md", b"# Prose\n\nsee [[t2]].\n".to_vec());
        assert_parity(&before, &after, &["/v/prose.md"]);
    }

    #[test]
    fn parity_an_instance_that_loses_its_claim_becomes_a_note() {
        // The claim boundary, crossed downward. `b` drops its `type:`, so it
        // stops being an instance: its resolved entry and its closure membership
        // must go, and `a` must re-validate, since a referrer reads the target's
        // claim through `RefData` and there is no longer one to read.
        //
        // Both live inside the typed-instance branch of the recompute, so this
        // is the case a naive widening drops.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/b.md", b"---\ntype: note\ntitle: B\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/b.md", b"---\ntitle: B\n---\nprose\n".to_vec());

        // Non-vacuity, on both halves: `b` really does leave the resolved map,
        // and `a`'s diagnostics really do move.
        let vb = build(Path::new("/v"), &after).unwrap();
        assert!(
            !vb.instances.contains_key(Path::new("/v/b.md")),
            "b must stop carrying resolved analysis"
        );
        let a_codes = |fs: &MemoryFileSystem| -> Vec<String> {
            build(Path::new("/v"), fs)
                .unwrap()
                .diagnostics()
                .filter(|d| d.span.file == Path::new("/v/a.md"))
                .map(|d| d.code.as_str().to_string())
                .collect()
        };
        assert_ne!(
            a_codes(&before),
            a_codes(&after),
            "dropping b's claim must move a's diagnostics"
        );

        assert_parity(&before, &after, &["/v/b.md"]);
    }

    #[test]
    fn parity_a_note_that_gains_a_claim_becomes_an_instance() {
        // The claim boundary, crossed upward: the mirror of the test above. `b`
        // gains a `type:`, so it enters the resolved map and the closure
        // membership index, and `a`'s typed reference to it starts satisfying
        // its slot.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/b.md", b"---\ntitle: B\n---\nprose\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/b.md", b"---\ntype: note\ntitle: B\n---\n".to_vec());

        let vb = build(Path::new("/v"), &after).unwrap();
        assert!(
            vb.instances.contains_key(Path::new("/v/b.md")),
            "b must start carrying resolved analysis"
        );

        assert_parity(&before, &after, &["/v/b.md"]);
    }

    #[test]
    fn parity_a_broken_frontmatter_edit() {
        // An unterminated frontmatter parses to `Unparsed`, which carries a File
        // diagnostic but no fields, no body events, and no claim. It rides the
        // same non-instance path as a note, so its diagnostic must land in the
        // held partition and its old edges must drop.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert("/v/t.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[t]]\"\n---\n".to_vec(),
        );
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/a.md", b"---\ntype: note\nlink: \"[[t]]\"\n".to_vec());

        assert!(
            build(Path::new("/v"), &after)
                .unwrap()
                .diagnostics()
                .any(|d| d.code.as_str() == "frontmatter-unterminated"),
            "fixture must actually break the frontmatter"
        );
        assert_parity(&before, &after, &["/v/a.md"]);
    }

    #[test]
    fn parity_mutual_referrers_deleted_together() {
        // a and b reference each other; both are deleted in one dirty set. Each is
        // an edge-flip source for the other (it names a disappearing file), but a
        // deleted file must NOT be re-validated as its own dependent: a full build
        // has neither file nor any diagnostic for it. Before the deleted-path
        // exclusion the dependents loop re-validated the deleted files (the
        // overlay excludes only adds and edits, not deletes), firing a phantom
        // reference-target-missing that apply re-inserted after the removal.
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert(
                "/v/a.md",
                b"---\ntype: note\nlink: \"[[b]]\"\n---\n".to_vec(),
            );
            fs.insert(
                "/v/b.md",
                b"---\ntype: note\nlink: \"[[a]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        let mut after = MemoryFileSystem::new();
        linked_base(&mut after); // both a and b gone
        assert_parity(&before, &after, &["/v/a.md", "/v/b.md"]);
    }

    #[test]
    fn parity_add_and_edit_together() {
        // One dirty set holds both an add (target.md appears) and an edit (a.md
        // gains its `[[target]]` link in the same commit).
        let base = |fs: &mut MemoryFileSystem| {
            linked_base(fs);
            fs.insert("/v/b.md", b"---\ntype: note\n---\n".to_vec());
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/a.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[target]]\"\n---\n".to_vec(),
        );
        after.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        assert_parity(&before, &after, &["/v/a.md", "/v/target.md"]);
    }

    #[test]
    fn parity_add_in_an_assembled_workspace_flips_a_cross_repo_referrer() {
        // Adding base's n resolves app's cross-repo `[[n::base]]`, exercising the
        // index rebuild and the edge-flip set across the boundary in a real
        // assembled two-repo workspace.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
            fs.insert(
                "/ws/.arsumbris/workspace.yaml",
                b"edit:\n  - ws\n  - base\n  - app\n".to_vec(),
            );
            fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
            fs.insert(
                "/ws/base/type/note.type.yaml",
                b"fields:\n  link?: note*\n".to_vec(),
            );
            fs.insert(
                "/ws/app/.arsumbris/repo.yaml",
                b"name: app\ndeps:\n  - name: base\n".to_vec(),
            );
            fs.insert(
                "/ws/app/type/note.type.yaml",
                b"fields:\n  link?: note*\n".to_vec(),
            );
            fs.insert(
                "/ws/app/t.md",
                b"---\ntype: note\nlink: \"[[n::base]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/ws/base/n.md", b"---\ntype: note\n---\n".to_vec());
        assert_parity_at("/ws", &before, &after, &["/ws/base/n.md"]);
    }

    #[test]
    fn parity_cross_repo_referrer_edit() {
        // The referrer carries a `::repo` reference to an undeclared repo, which
        // diagnoses in its Instance slice. Editing the referrer's body must
        // reproduce that cross-boundary diagnostic through the fast path.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  link?: note*\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[t::other]]\"\n---\nhi\n".to_vec(),
        );
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert(
            "/v/a.md",
            b"---\ntype: note\nlink: \"[[t::other]]\"\n---\nbye\n".to_vec(),
        );
        assert_parity(&before, &after, &["/v/a.md"]);
    }

    #[test]
    fn parity_block_pull_target_edit_flips_a_dependent() {
        // decision pulls a typed block from research-notes via
        // `[[research-notes^extractor-stability:assumptions]]` into an
        // `assumption&[+]` body slot. Editing the pulled block's `type:` from
        // `assumption` to `note` (which does not satisfy the slot) flips
        // decision's body contribution to a mismatch. research-notes is the
        // dirty target, decision its block-pull dependent, the block-id edge.
        let base = |fs: &mut MemoryFileSystem| {
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/assumption.type.yaml",
                b"fields:\n  description: String\n".to_vec(),
            );
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  description: String\n  assumptions?: assumption&[]\n".to_vec(),
            );
            fs.insert(
                "/v/type/decision.type.yaml",
                b"fields:\n  description: String\n  assumptions?: assumption&[+]\nbody:\n  - section: Why\n    fills: assumptions\n".to_vec(),
            );
            fs.insert(
                "/v/decision.md",
                b"---\ntype: decision\ndescription: d\nassumptions:\n---\n\n# Why\n\nfrom [[research-notes^extractor-stability:assumptions]].\n".to_vec(),
            );
        };
        let block = |claim: &str| {
            format!(
                "---\ntype: note\ndescription: r\nassumptions:\n---\n\n# Findings\n\n```yaml [:assumptions]\ntype: {claim}\ndescription: x\n```\n^extractor-stability\n"
            )
            .into_bytes()
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/v/research-notes.md", block("assumption"));
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/v/research-notes.md", block("note"));
        assert_parity(&before, &after, &["/v/research-notes.md"]);
    }

    #[test]
    fn parity_cross_repo_dependent_across_an_assembled_workspace() {
        // A genuine two-repo workspace: app's task carries a typed cross-repo
        // reference `ref?: note*` filled with `[[n::base]]`, resolving into the
        // co-present `base` member. Editing base's n from `note` to `other` flips
        // app's cross-repo reference to a mismatch. n is the dirty target in
        // base, app's t is its inbound identity-dependent across the boundary.
        let base = |fs: &mut MemoryFileSystem| {
            // The workspace entry and its co-present members.
            fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
            fs.insert(
                "/ws/.arsumbris/workspace.yaml",
                b"edit:\n  - ws\n  - base\n  - app\n".to_vec(),
            );
            // base owns note and other.
            fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
            fs.insert(
                "/ws/base/type/note.type.yaml",
                b"fields:\n  title?: String\n".to_vec(),
            );
            fs.insert(
                "/ws/base/type/other.type.yaml",
                b"fields:\n  y?: String\n".to_vec(),
            );
            // app owns its own note (byte-identical to base's, so the same
            // identity) and a task with a typed cross-repo slot.
            fs.insert(
                "/ws/app/.arsumbris/repo.yaml",
                b"name: app\ndeps:\n  - name: base\n".to_vec(),
            );
            fs.insert(
                "/ws/app/type/note.type.yaml",
                b"fields:\n  title?: String\n".to_vec(),
            );
            fs.insert(
                "/ws/app/type/task.type.yaml",
                b"fields:\n  ref?: note*\n".to_vec(),
            );
            fs.insert(
                "/ws/app/t.md",
                b"---\ntype: task\nref: \"[[n::base]]\"\n---\n".to_vec(),
            );
        };
        let mut before = MemoryFileSystem::new();
        base(&mut before);
        before.insert("/ws/base/n.md", b"---\ntype: note\n---\n".to_vec());
        let mut after = MemoryFileSystem::new();
        base(&mut after);
        after.insert("/ws/base/n.md", b"---\ntype: other\n---\n".to_vec());
        assert_parity_at("/ws", &before, &after, &["/ws/base/n.md"]);
    }

    #[test]
    fn a_collision_creating_add_falls_back() {
        // Adding `Note.md` when `note.md` exists case-collides on the basename.
        // The index delta cannot splice the collision diagnostic into the held
        // repo bucket, so the recompute bails (`None`) and the caller takes the
        // full rebuild. Anything else would diverge from a from-scratch build.
        let mut before = MemoryFileSystem::new();
        linked_base(&mut before);
        before.insert("/v/note.md", b"---\ntype: note\n---\n".to_vec());
        let held = build(Path::new("/v"), &before).unwrap();

        let mut after = MemoryFileSystem::new();
        linked_base(&mut after);
        after.insert("/v/note.md", b"---\ntype: note\n---\n".to_vec());
        after.insert("/v/Note.md", b"---\ntype: note\n---\n".to_vec());
        assert!(recompute_dirty(&held, &set(&["/v/Note.md"]), &after).is_none());
    }

    /// Micro-bench (ignored, run by hand): the incremental fast path's recompute
    /// work (`recompute_dirty` + `apply_recompute`) for a BULK dirty set of N
    /// instance edits, the branch-switch / bulk-import shape. This work now runs
    /// OFF the state lock (`try_incremental_fast_path` snapshots, computes, then
    /// locks only to swap), so it no longer blocks reads. The bench measures the
    /// raw compute floor that still bounds rebuild latency and feeds the
    /// O(n^2)-in-co-dirtied-referrers concern.
    /// Memory-fs isolates the parse+recompute floor; the real daemon path adds a
    /// bounded disk read pass on top.
    ///
    /// Run:
    /// `cargo test -p au-engine --lib incremental::tests::bulk_recompute_lock_hold_micro_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bulk_recompute_lock_hold_micro_bench() {
        // `hub`: every note references note-0, so one node accrues N backlinks
        // (probes a concentration cost). `!hub`: a chain, each note references
        // its predecessor, so backlinks per node stay O(1) (probes intrinsic
        // per-file recompute). The gap between the two isolates a hub artifact
        // from an intrinsic super-linearity.
        fn make_fs(n: usize, hub: bool) -> MemoryFileSystem {
            let mut fs = MemoryFileSystem::new();
            fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
            fs.insert(
                "/v/type/note.type.yaml",
                b"fields:\n  title: String\n  related?: note*\n".to_vec(),
            );
            for i in 0..n {
                let target = if hub { 0 } else { i.saturating_sub(1) };
                let body = format!(
                    "---\ntype: note\ntitle: Note {i}\nrelated: \"[[note-{target}]]\"\n---\n\
                     # Note {i}\n\nProse for note {i}, see [[note-{target}]].\n\
                     More text so the parser has a handful of spans to convert.\n"
                );
                fs.insert(format!("/v/note-{i}.md"), body);
            }
            fs
        }

        for (label, hub) in [
            ("hub (note-0 gets N backlinks)", true),
            ("chain (O(1) backlinks/node)", false),
        ] {
            println!("bulk incremental recompute, {label}:");
            for &n in &[50usize, 200, 500, 1000, 2000] {
                let fs = make_fs(n, hub);
                let held = build(Path::new("/v"), &fs).unwrap();
                let dirty: BTreeSet<PathBuf> = (0..n)
                    .map(|i| PathBuf::from(format!("/v/note-{i}.md")))
                    .collect();

                let _ = recompute_dirty(&held, &dirty, &fs); // warm up

                let reps = 5;
                let mut best = std::time::Duration::MAX;
                for _ in 0..reps {
                    let t = std::time::Instant::now();
                    let rc =
                        recompute_dirty(&held, &dirty, &fs).expect("bulk instance edit recomputes");
                    let next = apply_recompute(&held, rc);
                    best = best.min(t.elapsed());
                    std::hint::black_box(&next);
                }
                println!(
                    "  N={n:>5}  recompute+apply (best of {reps}) = {:>8.2} ms  ({:>6.1} us/file)",
                    best.as_secs_f64() * 1e3,
                    best.as_secs_f64() * 1e6 / n as f64,
                );
            }
        }
    }
}

/// Which rebuild path each catalogued operation drives the engine down.
///
/// The durable guard on write latency. A wall-clock assertion is machine
/// dependent, so it flakes and then gets muted; WHICH PATH a write takes is
/// machine independent, and it is the fact that decides whether a write costs
/// microseconds or a whole rebuild.
///
/// Deliberately inside the crate rather than in `tests/it`: the real decision
/// is `classify_dirty` first and `recompute_dirty` second, and the former is
/// crate-private. Asserting on `recompute_dirty` alone would happen to agree
/// today and could silently drift from the path the engine actually takes.
///
/// No watcher and no timing here, so nothing in this module can flake on a
/// loaded machine or a coalescing filesystem.
#[cfg(test)]
mod rebuild_path {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use au_parser::RealFileSystem;
    use au_testkit::opcat::{self, RebuildPath};
    use au_testkit::repogen::{self, Profile};

    use super::{classify_dirty, recompute_dirty, DirtyClass};

    /// Run one operation against a fresh workspace and report the path taken.
    fn path_taken(op: &opcat::Operation) -> RebuildPath {
        let dir = tempfile::tempdir().expect("tempdir");
        let generated = repogen::generate_profile(dir.path(), Profile::Small, 7);
        let targets = opcat::Targets::from_gen(&generated);
        let fs = RealFileSystem;

        // Setup runs BEFORE the build, so its file is part of the held state
        // rather than showing up as part of the operation's own dirty set.
        if let Some(prepare) = op.prepare {
            prepare(&targets).expect("prepare");
        }
        let held = crate::build::build(&generated.entry, &fs).expect("build");

        let applied = (op.apply)(&targets).expect("apply");
        let dirty: BTreeSet<PathBuf> = applied.touched.iter().cloned().collect();

        // The engine's own two steps, in order.
        match classify_dirty(&held, &dirty) {
            DirtyClass::NeedsFull => RebuildPath::Full,
            // Classified incremental, but the recompute still refuses anything
            // wider than a typed instance; a `None` is the fall-through to the
            // whole-knowledge-base rebuild.
            DirtyClass::Incremental => match recompute_dirty(&held, &dirty, &fs) {
                Some(_) => RebuildPath::Incremental,
                None => RebuildPath::Full,
            },
        }
    }

    /// Every catalogued operation still takes the path the catalog records.
    ///
    /// Some of those paths are known-wrong, carried as `wanted` on the
    /// operation. Pinning them keeps the gate green while the catalog still
    /// states the behaviour is undesirable; closing a gap flips `today` and
    /// clears `wanted` in the same commit, so the fix shows up in the diff
    /// rather than as a test quietly changing meaning.
    #[test]
    fn every_operation_takes_its_recorded_path() {
        let mut wrong = Vec::new();
        for op in opcat::catalog() {
            let taken = path_taken(&op);
            if taken != op.today {
                wrong.push(format!(
                    "{}: recorded {:?}, took {:?}",
                    op.name, op.today, taken
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "rebuild path changed for {} operation(s):\n  {}\n\n\
             If this is a deliberate fix, update `today` in au_testkit::opcat \
             (and clear `wanted` when the gap closes). If not, a write that used \
             to splice now rebuilds the whole knowledge base.",
            wrong.len(),
            wrong.join("\n  ")
        );
    }

    /// No catalogued write takes a path it should not.
    ///
    /// Separate from the pin above so the gap set cannot quietly GROW: that one
    /// compares each operation against its own `today`, so a new operation
    /// recorded as Full-but-wanting-Incremental would pass it. This one fails on
    /// the gap set itself.
    ///
    /// Asserts the SET, not each gap in turn. A per-gap check after this
    /// assertion could never run, since the assertion passing means there are
    /// none; the two together read as belt and braces and were one belt.
    #[test]
    fn no_operation_takes_a_path_it_should_not() {
        let gaps: Vec<&str> = opcat::catalog()
            .iter()
            .filter(|op| op.is_known_gap())
            .map(|op| op.name)
            .collect();
        assert_eq!(
            gaps,
            Vec::<&str>::new(),
            "an operation is recorded as taking the wrong rebuild path"
        );
    }
}
