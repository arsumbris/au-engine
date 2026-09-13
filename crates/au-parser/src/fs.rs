//! Filesystem port. The validator never reads disk directly — it goes through a
//! `FileSystem` impl. `RealFileSystem` wraps `std::fs`; `MemoryFileSystem` is a
//! BTreeMap-backed test double.
//!
//! Walks apply a two-layer scope: an unconditional hard floor (`.git`,
//! `.arsumbris`, never walked, never reachable by any pattern) and a per-walk
//! [`WalkFilter`] (the default excludes plus any `.arsumbris/.auignore`). See
//! the `scope` module.

use crate::scope::{WalkFilter, FLOOR_DIR_NAMES};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

/// A directory whose name is in the hard floor is never walked, above the
/// filter, so no pattern can reach it.
fn is_floored_name(name: &str) -> bool {
    FLOOR_DIR_NAMES.contains(&name)
}

/// A path is floored if any of its components is a floored directory name.
/// Used by `MemoryFileSystem`, which filters a flat file map with no traversal
/// to prune.
fn is_floored_path(path: &Path) -> bool {
    path.components()
        .any(|c| is_floored_name(&c.as_os_str().to_string_lossy()))
}

/// Whether the walk would have entered `dir`: `dir` and every directory ancestor
/// below `root` pass `enter_dir` and are not floored. Mirrors the real walk's
/// inherited pruning, used to gate `MemoryFileSystem`'s repo-marker surfacing so
/// a repo under a pruned directory is not discovered.
fn dir_reachable(root: &Path, dir: &Path, filter: &WalkFilter) -> bool {
    let Ok(rel) = dir.strip_prefix(root) else {
        return false;
    };
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        if is_floored_path(&cur) || !filter.enter_dir(&cur) {
            return false;
        }
    }
    true
}

/// The repo roots strictly under `root` among `keys`: a `<D>/.arsumbris/repo.yaml`
/// key marks `D` a repo root, and the walk root itself is excluded (its identity,
/// not a boundary). `MemoryFileSystem` uses this to bound its flat-map walks at a
/// nested repo, the mirror of the real walker stopping at a nested marker. A file
/// under such a root, and a marker below it, belong to the nested repo, not the
/// walk of `root`.
fn nested_repo_roots<'a>(keys: impl Iterator<Item = &'a PathBuf>, root: &Path) -> Vec<PathBuf> {
    keys.filter(|p| {
        p.file_name() == Some(std::ffi::OsStr::new("repo.yaml"))
            && p.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".arsumbris"))
    })
    .filter_map(|p| p.parent().and_then(Path::parent))
    .filter(|d| d.starts_with(root) && *d != root)
    .map(Path::to_path_buf)
    .collect()
}

/// Hard cap on directory depth from the knowledge base root. Exists so a malformed
/// or adversarial tree can't overflow the walker's worklist or starve the
/// process — a real knowledge base is shallow.
const MAX_WALK_DEPTH: usize = 64;

/// Per-entry failure surfaced during a knowledge base walk. Carries the path
/// that failed plus the underlying `io::Error` so the CLI can render it
/// as a `repo-walk-error` diagnostic without losing the OS-level cause.
#[derive(Debug)]
pub struct WalkError {
    pub path: PathBuf,
    pub source: io::Error,
}

/// The boundary-level effect of a walk's scope: the directories the filter
/// pruned and the files it individually dropped, each recorded AT the boundary
/// the walk decides at, never the contents below a pruned directory. A pruned
/// `node_modules` is ONE `ignored_dirs` entry, its contents never enumerated.
///
/// The hard floor (`.git`, `.arsumbris`) is NOT reported here — it is a name
/// check above the filter, surfaced separately as the un-editable floor. This
/// backs the `ignores` read's `resolve` effect, see
/// [[spec - scope management surface - an ignores read and a set_ignores config mutation]].
#[derive(Debug, Default)]
pub struct ScopeBoundaries {
    /// Absolute paths of pruned directory boundaries, in walk order.
    pub ignored_dirs: Vec<PathBuf>,
    /// Absolute paths of individually-dropped files (whose parent was entered).
    pub ignored_files: Vec<PathBuf>,
}

/// The output of a content walk: the kept files, the repo-root markers the walk
/// surfaced, and any per-entry failures.
///
/// `repo_markers` is the one carve-out in the hard floor: as the walk enters a
/// directory holding a `.arsumbris/repo.yaml`, it surfaces that marker PATH
/// here, a repo-DISCOVERY signal, without walking `.arsumbris/` as content. So a
/// repo is discovered content-free, even one holding only its `repo.yaml`. The
/// floor otherwise holds, no other `.arsumbris/` file is a kept file or a marker.
/// See [[spec - knowledge base file scoping - a per-repo auignore file over a hard floor]].
#[derive(Debug, Default)]
pub struct Walk {
    /// Every regular file the filter kept, the content set.
    pub files: Vec<PathBuf>,
    /// Absolute paths of each surfaced `<dir>/.arsumbris/repo.yaml` marker.
    pub repo_markers: Vec<PathBuf>,
    /// Per-entry failures encountered along the way, non-fatal.
    pub errors: Vec<WalkError>,
}

pub trait FileSystem: Send + Sync {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Whether `path` is present AS A REGULAR FILE, without reading it.
    ///
    /// The presence probe for a file the engine never decodes: an asset is a
    /// `RepoIndex` member, so its add or delete flips `file*` and navigational
    /// references, but its bytes are never read. Asking `read_file(...).is_ok()`
    /// instead would pull an arbitrarily large file into memory to answer a
    /// question about existence.
    ///
    /// FILE, not merely existent: a watcher reports directory events too, and a
    /// directory must never be catalogued. The walk yields regular files only,
    /// so this is the predicate that agrees with it.
    fn is_file(&self, path: &Path) -> bool;

    /// The byte length of `path` as a regular file, WITHOUT reading it.
    ///
    /// The size probe the engine's read-cap consults so an over-cap file is
    /// skipped before its bytes are pulled into memory, the same rationale as
    /// [`FileSystem::is_file`]: asking `read_file(...)?.len()` would allocate an
    /// arbitrarily large file to answer a question about its size.
    ///
    /// The default DOES read and measure, correct but not cheap, so the trait
    /// stays object-safe and every existing impl compiles unchanged.
    /// [`RealFileSystem`] overrides it with a `metadata` stat, one syscall, so a
    /// multi-gigabyte file never allocates. A wrapper that memoizes or overlays
    /// `read_file` should delegate this to its inner filesystem so the cheap
    /// stat survives the wrapping.
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        Ok(self.read_file(path)?.len() as u64)
    }

    /// Walk `root` recursively, returning every regular file path (no
    /// directories) plus any per-entry failures encountered along the
    /// way. All extensions, Markdown / YAML / assets alike — the
    /// classifier (`classify_by_path`) decides what to do with each.
    ///
    /// Scope has two layers. The hard floor (`.git`, `.arsumbris`) is never
    /// walked, unconditionally. The `filter` decides the rest: a matched
    /// directory is pruned, a matched file dropped. `filter` is anchored to
    /// `root`. Order is unspecified — callers that need determinism should
    /// sort the result.
    ///
    /// Per-entry failures (unreadable subdir, broken symlink, etc.) are
    /// returned in the second vec rather than aborting the walk so a
    /// single bad path doesn't void the whole validation. The outer
    /// `io::Result` is reserved for root-level failures (root not a
    /// directory, root canonicalize fails).
    ///
    /// au-references' `RepoIndex` consumes the kept files directly so that bare
    /// `file*` references find PDFs, images, and other assets the type
    /// system doesn't otherwise model.
    ///
    /// The one floor carve-out: [`Walk::repo_markers`] carries each
    /// `<dir>/.arsumbris/repo.yaml` the walk passed, a content-free repo-root
    /// discovery signal. `.arsumbris/` is otherwise never walked, see [`Walk`].
    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<Walk>;

    /// Walk `root` recording the scope BOUNDARIES the `filter` decides at,
    /// without descending a pruned directory. A directory the filter prunes is
    /// one [`ScopeBoundaries::ignored_dirs`] entry and its contents are never
    /// enumerated; a file the filter drops (whose parent WAS entered) is one
    /// [`ScopeBoundaries::ignored_files`] entry. The hard floor is excluded, as
    /// [`ScopeBoundaries`] documents. The second vec carries per-entry failures,
    /// as in [`FileSystem::walk_files`]. Backs the `ignores` read's `resolve`.
    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    /// A `metadata` stat, one syscall, so an over-cap file is measured without
    /// reading it.
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        std::fs::metadata(path).map(|m| m.len())
    }

    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<Walk> {
        walk(root, MAX_WALK_DEPTH, filter)
    }

    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
        walk_boundaries(root, MAX_WALK_DEPTH, filter)
    }
}

/// A boundary decision the walk made, streamed to a sink so `walk` and
/// `walk_boundaries` share one traversal. Emitted in the same order `walk`'s
/// output preserves (see [`walk_core`]).
enum WalkEvent {
    /// A file the filter kept.
    KeptFile(PathBuf),
    /// A directory the filter pruned (never the hard floor); not descended.
    PrunedDir(PathBuf),
    /// A file the filter dropped, whose parent directory WAS entered.
    DroppedFile(PathBuf),
    /// A `<dir>/.arsumbris/repo.yaml` marker surfaced at the floor, the one
    /// carve-out: a repo-root discovery signal, never a kept file.
    RepoMarker(PathBuf),
}

/// Iterative directory walk with a depth cap and symlink cycle detection,
/// streaming every scope decision to `on_event`. The shared core of `walk`
/// (which keeps `KeptFile`) and `walk_boundaries` (which keeps the pruned /
/// dropped events).
///
/// Symlinks are followed (via `metadata` rather than `file_type`, so a
/// symlinked directory traverses normally instead of being silently
/// dropped). Cycles are detected by tracking canonical paths of visited
/// directories — a symlink pointing at an ancestor is followed once, then
/// deduped. The depth cap is a final safeguard against pathological trees;
/// realistic knowledge bases are shallow.
///
/// Event order preserves depth-first-in-readdir-order: for each directory, its
/// entries are emitted in the order `read_dir` returned them, with each
/// subdirectory's full contents inlined at its position. The pruned / dropped
/// events interleave with the kept files WITHOUT reordering the kept files
/// relative to one another, so `walk`'s observable file order is unchanged.
/// Downstream `RepoIndex` builds collision / ambiguity diagnostics whose
/// primary span is the first-seen file, so that ordering is observable.
fn walk_core(
    root: &Path,
    max_depth: usize,
    filter: &WalkFilter,
    mut on_event: impl FnMut(WalkEvent),
) -> io::Result<Vec<WalkError>> {
    let mut errors: Vec<WalkError> = Vec::new();
    // A root that is missing or is not a directory is a hard fail, per the
    // `walk_files` contract: the outer `io::Result` is reserved for root-level
    // failures. Returning empty-success would report a typo'd or wrong knowledge base
    // path as "0 files, no problems" — a silent drop.
    if !root.is_dir() {
        let kind = if root.exists() {
            io::ErrorKind::InvalidInput
        } else {
            io::ErrorKind::NotFound
        };
        return Err(io::Error::new(
            kind,
            format!("repo root is not a directory: {}", root.display()),
        ));
    }

    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    // Root canonicalize must succeed — without it, cycle detection breaks
    // for the whole walk. Genuine hard-fail.
    visited.insert(std::fs::canonicalize(root)?);

    // The boundary events (`PrunedDir` / `DroppedFile`) ride the same stack as
    // `File`, so they emit in walk order on pop, never during the readdir loop —
    // that keeps the kept-file order identical to a boundary-unaware walk.
    enum Item {
        File(PathBuf),
        Dir(PathBuf, usize),
        PrunedDir(PathBuf),
        DroppedFile(PathBuf),
    }

    let mut stack: Vec<Item> = vec![Item::Dir(root.to_path_buf(), 0)];

    while let Some(item) = stack.pop() {
        match item {
            Item::File(p) => on_event(WalkEvent::KeptFile(p)),
            Item::PrunedDir(p) => on_event(WalkEvent::PrunedDir(p)),
            Item::DroppedFile(p) => on_event(WalkEvent::DroppedFile(p)),
            Item::Dir(dir, depth) => {
                let read_dir = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(e) => {
                        errors.push(WalkError {
                            path: dir.clone(),
                            source: e,
                        });
                        continue;
                    }
                };
                let mut entries: Vec<Item> = Vec::new();
                for entry in read_dir {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            errors.push(WalkError {
                                path: dir.clone(),
                                source: e,
                            });
                            continue;
                        }
                    };
                    let path = entry.path();
                    // metadata() follows symlinks; file_type() does not.
                    // Following is required so a symlinked directory is
                    // traversed instead of silently dropped. A broken
                    // symlink (target missing) fails here — recorded and
                    // skipped rather than aborting the whole walk.
                    let metadata = match std::fs::metadata(&path) {
                        Ok(m) => m,
                        Err(e) => {
                            errors.push(WalkError {
                                path: path.clone(),
                                source: e,
                            });
                            continue;
                        }
                    };
                    if metadata.is_dir() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        // Hard floor first, unconditional, above the filter. The
                        // floor is never a scope boundary — it emits no event.
                        // One carve-out: an `.arsumbris` holding a `repo.yaml` is
                        // a repo root, surfaced as a discovery marker (never
                        // walked as content), so a repo is found content-free.
                        if is_floored_name(&name_str) {
                            if name_str == ".arsumbris" {
                                let marker = path.join("repo.yaml");
                                if std::fs::metadata(&marker)
                                    .map(|m| m.is_file())
                                    .unwrap_or(false)
                                {
                                    on_event(WalkEvent::RepoMarker(marker));
                                }
                            }
                            continue;
                        }
                        // A nested repo root below the walk root bounds the walk:
                        // its subtree belongs to the nested repo, so surface its
                        // `repo.yaml` on the discovery channel and do NOT descend.
                        // Checked BEFORE the user filter, structural like the floor
                        // above: a nested repo is a repo boundary even when its
                        // folder also matches a user prune pattern, so it is always
                        // a `RepoMarker` (discovered), never a `PrunedDir` (a
                        // user-scope boundary, which would both hide the marker and
                        // report the nested repo as an ignore). The walk's own root
                        // is entered directly, never reached here as a child, so it
                        // is never bounded — its content walks.
                        let nested_marker = path.join(".arsumbris").join("repo.yaml");
                        if std::fs::metadata(&nested_marker)
                            .map(|m| m.is_file())
                            .unwrap_or(false)
                        {
                            on_event(WalkEvent::RepoMarker(nested_marker));
                            continue;
                        }
                        // Then the user filter prunes a matched directory: a scope
                        // boundary, recorded and not descended.
                        if !filter.enter_dir(&path) {
                            entries.push(Item::PrunedDir(path));
                            continue;
                        }
                        let next_depth = depth + 1;
                        if next_depth > max_depth {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!(
                                    "repo walk exceeded max depth {} at {}",
                                    max_depth,
                                    path.display()
                                ),
                            ));
                        }
                        let canonical = match std::fs::canonicalize(&path) {
                            Ok(c) => c,
                            Err(e) => {
                                errors.push(WalkError {
                                    path: path.clone(),
                                    source: e,
                                });
                                continue;
                            }
                        };
                        if visited.insert(canonical) {
                            entries.push(Item::Dir(path, next_depth));
                        }
                    } else if metadata.is_file() {
                        if filter.keep_file(&path) {
                            entries.push(Item::File(path));
                        } else {
                            entries.push(Item::DroppedFile(path));
                        }
                    }
                    // Other entry types (sockets, FIFOs, block devices) are
                    // not file content the type system can address.
                }
                // Push reversed so popping yields the original read_dir order.
                for item in entries.into_iter().rev() {
                    stack.push(item);
                }
            }
        }
    }
    Ok(errors)
}

/// Every regular file the filter keeps under `root`. The boundary-unaware view
/// over [`walk_core`]: it drops the pruned / dropped events, so its output is
/// byte-identical to the pre-boundary walk.
fn walk(root: &Path, max_depth: usize, filter: &WalkFilter) -> io::Result<Walk> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut repo_markers: Vec<PathBuf> = Vec::new();
    let errors = walk_core(root, max_depth, filter, |ev| match ev {
        WalkEvent::KeptFile(p) => files.push(p),
        WalkEvent::RepoMarker(p) => repo_markers.push(p),
        WalkEvent::PrunedDir(_) | WalkEvent::DroppedFile(_) => {}
    })?;
    // Sort the markers so the real walk matches `MemoryFileSystem`, whose walk
    // sorts them. Consumers re-sort into a `BTreeMap` today, so this is parity,
    // not a correctness fix — but it keeps the two ports observationally equal.
    repo_markers.sort();
    Ok(Walk {
        files,
        repo_markers,
        errors,
    })
}

/// The scope boundaries under `root`: the pruned directories and dropped files,
/// never the contents below a pruned directory. The boundary view over
/// [`walk_core`], keeping only the pruned / dropped events.
fn walk_boundaries(
    root: &Path,
    max_depth: usize,
    filter: &WalkFilter,
) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
    let mut boundaries = ScopeBoundaries::default();
    let errors = walk_core(root, max_depth, filter, |ev| match ev {
        WalkEvent::PrunedDir(p) => boundaries.ignored_dirs.push(p),
        WalkEvent::DroppedFile(p) => boundaries.ignored_files.push(p),
        WalkEvent::KeptFile(_) | WalkEvent::RepoMarker(_) => {}
    })?;
    Ok((boundaries, errors))
}

#[derive(Debug, Default, Clone)]
pub struct MemoryFileSystem {
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl MemoryFileSystem {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), content.into());
    }
}

impl FileSystem for MemoryFileSystem {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    /// Every key is a regular file; the map holds no directories.
    fn is_file(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    /// The stored bytes' length, no clone, the in-memory parity of a stat.
    fn file_len(&self, path: &Path) -> io::Result<u64> {
        self.files
            .get(path)
            .map(|b| b.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn walk_files(&self, root: &Path, filter: &WalkFilter) -> io::Result<Walk> {
        // A nested repo root bounds the walk: its subtree, and any repo marker
        // below it, belong to that repo, not this walk.
        let nested_repo_roots = nested_repo_roots(self.files.keys(), root);
        // A path inside a nested repo belongs to that repo, not this walk.
        let under_nested = |p: &Path| nested_repo_roots.iter().any(|d| p.starts_with(d));
        // A flat file map, no traversal to prune, so the floor and the filter
        // are both applied per file. `keep_file` consults parent directories,
        // so a file under a pruned directory is dropped the same as a real
        // walk would prune it. A file inside a nested repo is bounded out.
        let files: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|p| {
                p.starts_with(root)
                    && !is_floored_path(p)
                    && filter.keep_file(p)
                    && !under_nested(p)
            })
            .cloned()
            .collect();
        // Repo-root markers: a `<dir>/.arsumbris/repo.yaml` key under root,
        // surfaced when the walk would have entered `<dir>` (its ancestors not
        // pruned) AND no nested repo root sits strictly between `root` and `dir`
        // (the bounded walk stops at the first nested repo on each path, so a
        // deeper repo never surfaces). The marker sits under the floored
        // `.arsumbris`, so it is absent from `files`; here it is a discovery
        // signal, not content.
        let mut repo_markers: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|p| {
                p.starts_with(root)
                    && p.file_name() == Some(std::ffi::OsStr::new("repo.yaml"))
                    && p.parent().and_then(Path::file_name)
                        == Some(std::ffi::OsStr::new(".arsumbris"))
                    && p.parent().and_then(Path::parent).is_some_and(|dir| {
                        // The walk root's OWN marker always surfaces (the real
                        // walker enters the root directly and surfaces its
                        // `.arsumbris/repo.yaml` at the floor). A NESTED repo marker
                        // is structural too: surfaced regardless of whether the
                        // nested repo's OWN folder matches the user filter (it is a
                        // repo boundary, not a user-pruned dir), so only the
                        // ancestors ABOVE it must be reachable, mirroring the real
                        // walker checking the nested marker BEFORE the filter.
                        // `is_floored_path(dir)` still bars a marker under the hard
                        // floor.
                        !is_floored_path(dir)
                            && (dir == root
                                || dir
                                    .parent()
                                    .is_some_and(|parent| dir_reachable(root, parent, filter)))
                            && !nested_repo_roots
                                .iter()
                                .any(|d| dir.starts_with(d) && dir != d)
                    })
            })
            .cloned()
            .collect();
        repo_markers.sort();
        Ok(Walk {
            files,
            repo_markers,
            errors: Vec::new(),
        })
    }

    fn walk_scope_boundaries(
        &self,
        root: &Path,
        filter: &WalkFilter,
    ) -> io::Result<(ScopeBoundaries, Vec<WalkError>)> {
        // No real tree to prune: reconstruct the boundaries from the flat map.
        // For each key under `root`, find the SHALLOWEST directory ancestor the
        // filter prunes — that is the boundary, deduped so a pruned subtree with
        // many files yields one entry, mirroring a real walk that never descends
        // it. Absent a pruned ancestor, a file the filter itself drops is one
        // `ignored_files` entry. The floor is excluded, as `is_floored_path`
        // skips the whole key.
        //
        // A nested repo bounds the walk too, like the floor: a key inside a nested
        // repo belongs to that repo, so it is skipped entirely (never a boundary,
        // never an ignored file). This mirrors the real walker stopping at the
        // nested marker, so a member's `ignores` read never reports a nested repo's
        // subtree as its own exclusion.
        let nested_repo_roots = nested_repo_roots(self.files.keys(), root);
        let mut ignored_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        let mut ignored_files: BTreeSet<PathBuf> = BTreeSet::new();
        for key in self.files.keys() {
            if !key.starts_with(root) || is_floored_path(key) {
                continue;
            }
            if nested_repo_roots.iter().any(|d| key.starts_with(d)) {
                continue;
            }
            let Ok(rel) = key.strip_prefix(root) else {
                continue;
            };
            let comps: Vec<_> = rel.components().collect();
            // All components but the last are directory ancestors; the last is
            // the file itself.
            let mut dir = root.to_path_buf();
            let mut boundary = None;
            for comp in &comps[..comps.len().saturating_sub(1)] {
                dir.push(comp);
                if !filter.enter_dir(&dir) {
                    boundary = Some(dir.clone());
                    break;
                }
            }
            match boundary {
                Some(d) => {
                    ignored_dirs.insert(d);
                }
                None => {
                    if !filter.keep_file(key) {
                        ignored_files.insert(key.clone());
                    }
                }
            }
        }
        Ok((
            ScopeBoundaries {
                ignored_dirs: ignored_dirs.into_iter().collect(),
                ignored_files: ignored_files.into_iter().collect(),
            },
            Vec::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default scope (default excludes only) for tests that just exercise
    /// the walk mechanics. The temp trees carry no `node_modules` / `target`,
    /// so it behaves as "no user scoping".
    fn scope(root: &Path) -> WalkFilter {
        WalkFilter::default_excludes(root)
    }

    #[test]
    fn memory_fs_round_trip() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/a.yaml", b"x: 1".to_vec());
        assert_eq!(fs.read_file(Path::new("/v/a.yaml")).unwrap(), b"x: 1");
    }

    #[test]
    fn memory_fs_walk_skips_ignored() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.yaml", b"".to_vec());
        fs.insert("/v/.git/HEAD", b"".to_vec());
        fs.insert("/v/node_modules/foo/index.js", b"".to_vec());
        let w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        assert_eq!(w.files, vec![PathBuf::from("/v/keep.yaml")]);
        assert!(w.errors.is_empty());
    }

    #[test]
    fn memory_fs_bounds_nested_repos_and_surfaces_their_markers() {
        // Walking a directory that CONTAINS repos bounds at each nested repo: its
        // marker surfaces (content-free discovery, even a repo holding only its
        // `repo.yaml`), but its content belongs to it, not this walk. The content
        // surfaces only when that repo is walked as its own root.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/a/.arsumbris/repo.yaml", b"name: a\n".to_vec());
        fs.insert("/v/a/note.md", b"".to_vec());
        fs.insert("/v/b/.arsumbris/repo.yaml", b"name: b\n".to_vec());
        let w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        assert!(
            w.files.is_empty(),
            "nested repo content is bounded out, not this walk's: {:?}",
            w.files
        );
        assert_eq!(
            w.repo_markers,
            vec![
                PathBuf::from("/v/a/.arsumbris/repo.yaml"),
                PathBuf::from("/v/b/.arsumbris/repo.yaml"),
            ],
            "both repos discovered by marker, content-free b included"
        );
        // Walking repo `a` as its own root yields its content and its own marker.
        let wa = fs
            .walk_files(Path::new("/v/a"), &scope(Path::new("/v/a")))
            .unwrap();
        assert_eq!(wa.files, vec![PathBuf::from("/v/a/note.md")]);
        assert_eq!(
            wa.repo_markers,
            vec![PathBuf::from("/v/a/.arsumbris/repo.yaml")]
        );
    }

    #[test]
    fn real_fs_bounds_a_nested_repos_content() {
        // The real walker stops at a nested repo: its marker surfaces, its content
        // does not, that content belongs to the nested repo's own walk.
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(nested.join(".arsumbris")).unwrap();
        std::fs::write(nested.join(".arsumbris/repo.yaml"), b"name: nested\n").unwrap();
        std::fs::write(nested.join("inside.md"), b"").unwrap();
        std::fs::write(tmp.path().join("top.md"), b"").unwrap();
        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        assert_eq!(
            w.files,
            vec![tmp.path().join("top.md")],
            "nested content bounded out"
        );
        assert_eq!(
            w.repo_markers,
            vec![nested.join(".arsumbris/repo.yaml")],
            "nested repo surfaced by marker"
        );
    }

    #[test]
    fn real_fs_only_surfaces_the_outermost_nested_repo() {
        // The bounded walk stops at the first nested repo on each path, so a repo
        // buried inside another nested repo never surfaces from the outer walk.
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        std::fs::create_dir_all(outer.join(".arsumbris")).unwrap();
        std::fs::write(outer.join(".arsumbris/repo.yaml"), b"name: outer\n").unwrap();
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join(".arsumbris")).unwrap();
        std::fs::write(inner.join(".arsumbris/repo.yaml"), b"name: inner\n").unwrap();
        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        assert_eq!(
            w.repo_markers,
            vec![outer.join(".arsumbris/repo.yaml")],
            "only the outermost repo surfaces; inner is buried"
        );
        assert!(w.files.is_empty());
    }

    #[test]
    fn memory_fs_only_surfaces_the_outermost_nested_repo() {
        // Ports stay observationally equal: the flat-map walk bounds at the first
        // nested repo too, so a buried repo and its content never surface.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/outer/.arsumbris/repo.yaml", b"name: outer\n".to_vec());
        fs.insert(
            "/v/outer/inner/.arsumbris/repo.yaml",
            b"name: inner\n".to_vec(),
        );
        fs.insert("/v/outer/inner/deep.md", b"".to_vec());
        let w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        assert_eq!(
            w.repo_markers,
            vec![PathBuf::from("/v/outer/.arsumbris/repo.yaml")],
            "only outer surfaces from the /v walk"
        );
        assert!(w.files.is_empty(), "all content is bounded behind outer");
    }

    #[test]
    fn memory_fs_does_not_surface_a_marker_under_a_pruned_dir() {
        // A repo under a filter-pruned directory is not discovered: pruning is
        // inherited, mirroring the real walk that never descends the pruned dir.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert(
            "/v/target/dep/.arsumbris/repo.yaml",
            b"name: dep\n".to_vec(),
        );
        let w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        assert!(
            w.repo_markers.is_empty(),
            "a repo under pruned target/ is not discovered: {:?}",
            w.repo_markers
        );
    }

    #[test]
    fn real_fs_surfaces_a_content_less_repo_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub/.arsumbris");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("repo.yaml"), b"name: sub\n").unwrap();
        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        assert!(w.files.is_empty(), "content-less: no kept files");
        assert_eq!(
            w.repo_markers,
            vec![tmp.path().join("sub/.arsumbris/repo.yaml")],
            "the content-less repo is discovered by its marker"
        );
    }

    #[test]
    fn memory_fs_walk_returns_assets_alongside_typed_files() {
        // Assets (non-frontmatter files) must come back so au-references'
        // RepoIndex can resolve `file*` slots pointing at PDFs / images /
        // arbitrary binaries.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/notes/foo.md", b"".to_vec());
        fs.insert("/v/types/bar.type.yaml", b"".to_vec());
        fs.insert("/v/assets/diagram.pdf", b"%PDF".to_vec());
        fs.insert("/v/assets/photo.png", b"\x89PNG".to_vec());
        fs.insert("/v/scripts/build.sh", b"#!/bin/sh\n".to_vec());
        let mut w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        w.files.sort();
        assert_eq!(
            w.files,
            vec![
                PathBuf::from("/v/assets/diagram.pdf"),
                PathBuf::from("/v/assets/photo.png"),
                PathBuf::from("/v/notes/foo.md"),
                PathBuf::from("/v/scripts/build.sh"),
                PathBuf::from("/v/types/bar.type.yaml"),
            ]
        );
        assert!(w.errors.is_empty());
    }

    #[test]
    fn memory_fs_walk_drops_assets_under_ignored_dirs() {
        // The ignore list takes precedence over asset-friendliness: an
        // image under `target/` or `node_modules/` is still cache, not
        // knowledge base content.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.png", b"".to_vec());
        fs.insert("/v/target/intermediate/cached.png", b"".to_vec());
        fs.insert("/v/.arsumbris/index.bin", b"".to_vec());
        let w = fs
            .walk_files(Path::new("/v"), &scope(Path::new("/v")))
            .unwrap();
        assert_eq!(w.files, vec![PathBuf::from("/v/keep.png")]);
        assert!(w.errors.is_empty());
    }

    #[test]
    fn memory_fs_scope_boundaries_report_a_pruned_dir_once() {
        // A pruned directory with many files is ONE `ignored_dirs` entry, its
        // contents never enumerated — the whole point of reporting boundaries.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert("/v/node_modules/a/index.js", b"".to_vec());
        fs.insert("/v/node_modules/b/index.js", b"".to_vec());
        fs.insert("/v/docs/guide.md", b"".to_vec());
        let filter = WalkFilter::with_auignore(Path::new("/v"), "docs/\n").unwrap();
        let (b, errs) = fs.walk_scope_boundaries(Path::new("/v"), &filter).unwrap();
        assert!(errs.is_empty());
        assert_eq!(
            b.ignored_dirs,
            vec![PathBuf::from("/v/docs"), PathBuf::from("/v/node_modules")],
            "each pruned dir once, contents absent"
        );
        assert!(b.ignored_files.is_empty(), "no individually-dropped files");
    }

    #[test]
    fn memory_fs_scope_boundaries_report_an_individually_dropped_file() {
        // A file dropped while its parent dir is entered is an `ignored_files`
        // entry, not an `ignored_dirs` one.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert("/v/notes/scratch.log", b"".to_vec());
        let filter = WalkFilter::with_auignore(Path::new("/v"), "*.log\n").unwrap();
        let (b, _) = fs.walk_scope_boundaries(Path::new("/v"), &filter).unwrap();
        assert!(b.ignored_dirs.is_empty(), "notes/ was entered, not pruned");
        assert_eq!(b.ignored_files, vec![PathBuf::from("/v/notes/scratch.log")]);
    }

    #[test]
    fn memory_fs_scope_boundaries_exclude_the_floor() {
        // The hard floor is not a scope boundary — it is reported separately.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert("/v/.git/HEAD", b"".to_vec());
        fs.insert("/v/.arsumbris/index.bin", b"".to_vec());
        let filter = WalkFilter::default_excludes(Path::new("/v"));
        let (b, _) = fs.walk_scope_boundaries(Path::new("/v"), &filter).unwrap();
        assert!(b.ignored_dirs.is_empty() && b.ignored_files.is_empty());
    }

    #[test]
    fn a_nested_repo_is_not_a_scope_boundary() {
        // A nested repo bounds the walk like the floor, not like a user-ignore: it
        // never appears in `ScopeBoundaries`, and a filter-dropped file INSIDE it
        // is not reported as this member's ignore (it belongs to the nested repo).
        // Both ports agree, so a member's `ignores` read never claims a nested
        // repo's subtree as its own exclusion.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(nested.join(".arsumbris")).unwrap();
        std::fs::write(nested.join(".arsumbris/repo.yaml"), b"name: nested\n").unwrap();
        std::fs::write(nested.join("scratch.log"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "*.log\n").unwrap();
        let (b, _) = walk_boundaries(tmp.path(), 8, &filter).unwrap();
        assert!(
            b.ignored_dirs.is_empty(),
            "a nested repo is not an ignored dir: {:?}",
            b.ignored_dirs
        );
        assert!(
            b.ignored_files.is_empty(),
            "a file inside the nested repo is not this member's ignore: {:?}",
            b.ignored_files
        );

        // The memory port is observationally equal.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/m/keep.md", b"".to_vec());
        fs.insert("/m/nested/.arsumbris/repo.yaml", b"name: nested\n".to_vec());
        fs.insert("/m/nested/scratch.log", b"".to_vec());
        let filter = WalkFilter::with_auignore(Path::new("/m"), "*.log\n").unwrap();
        let (b, _) = fs.walk_scope_boundaries(Path::new("/m"), &filter).unwrap();
        assert!(
            b.ignored_dirs.is_empty() && b.ignored_files.is_empty(),
            "memory port agrees: a nested repo is not this member's boundary: {b:?}"
        );
    }

    #[test]
    fn a_nested_repo_matching_a_prune_pattern_still_surfaces_by_marker() {
        // The regression: a nested repo whose FOLDER also matches a user prune
        // pattern is a repo boundary, not a user-ignore. Its marker must still
        // surface (so a declared member there mounts), its content stays bounded
        // out, and it is never reported as an ignored dir. Both ports agree.
        //
        // Real FS. `nested/` prunes the very folder holding the nested repo.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(nested.join(".arsumbris")).unwrap();
        std::fs::write(nested.join(".arsumbris/repo.yaml"), b"name: nested\n").unwrap();
        std::fs::write(nested.join("inside.md"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "nested/\n").unwrap();
        let w = walk(tmp.path(), 8, &filter).unwrap();
        assert_eq!(
            w.repo_markers,
            vec![nested.join(".arsumbris/repo.yaml")],
            "the marker surfaces despite the folder matching the prune"
        );
        assert_eq!(
            w.files,
            vec![tmp.path().join("keep.md")],
            "nested content stays bounded out"
        );
        let (b, _) = walk_boundaries(tmp.path(), 8, &filter).unwrap();
        assert!(
            b.ignored_dirs.is_empty(),
            "a nested repo is a repo boundary, never an ignored dir: {:?}",
            b.ignored_dirs
        );

        // Memory port, observationally equal.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/m/keep.md", b"".to_vec());
        fs.insert("/m/nested/.arsumbris/repo.yaml", b"name: nested\n".to_vec());
        fs.insert("/m/nested/inside.md", b"".to_vec());
        let filter = WalkFilter::with_auignore(Path::new("/m"), "nested/\n").unwrap();
        let w = fs.walk_files(Path::new("/m"), &filter).unwrap();
        assert_eq!(
            w.repo_markers,
            vec![PathBuf::from("/m/nested/.arsumbris/repo.yaml")],
            "memory port: marker surfaces despite the prune"
        );
        assert_eq!(w.files, vec![PathBuf::from("/m/keep.md")]);
        let (b, _) = fs.walk_scope_boundaries(Path::new("/m"), &filter).unwrap();
        assert!(
            b.ignored_dirs.is_empty(),
            "memory port agrees: a nested repo is not an ignored dir: {b:?}"
        );
    }

    #[test]
    fn real_fs_scope_boundaries_record_a_pruned_dir_boundary() {
        // A real walk records the pruned directory and never descends it, so its
        // files do not appear as `ignored_files`.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        let docs = tmp.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("guide.md"), b"").unwrap();
        std::fs::write(docs.join("more.md"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "docs/\n").unwrap();
        let (b, errs) = walk_boundaries(tmp.path(), 8, &filter).unwrap();
        assert!(errs.is_empty());
        assert_eq!(b.ignored_dirs, vec![docs], "docs recorded as one boundary");
        assert!(
            b.ignored_files.is_empty(),
            "contents of the pruned dir are never enumerated"
        );
    }

    #[test]
    fn walk_and_boundaries_partition_the_same_tree() {
        // `walk` (kept files) and `walk_boundaries` (dropped) are complementary
        // views of one traversal: a dropped file is exactly one the kept walk
        // omits, and `walk`'s output order is unchanged by the boundary events.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        std::fs::write(tmp.path().join("scratch.log"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "*.log\n").unwrap();
        let kept = walk(tmp.path(), 8, &filter).unwrap().files;
        let (b, _) = walk_boundaries(tmp.path(), 8, &filter).unwrap();
        assert_eq!(kept, vec![tmp.path().join("keep.md")]);
        assert_eq!(b.ignored_files, vec![tmp.path().join("scratch.log")]);
    }

    #[test]
    fn memory_fs_walk_honors_an_auignore_filter() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/keep.md", b"".to_vec());
        fs.insert("/v/docs/guide.md", b"".to_vec());
        let filter = WalkFilter::with_auignore(Path::new("/v"), "docs/\n").unwrap();
        let walked = fs.walk_files(Path::new("/v"), &filter).unwrap().files;
        assert_eq!(walked, vec![PathBuf::from("/v/keep.md")], "docs/ dropped");
    }

    #[test]
    fn real_fs_walk_prunes_a_matched_directory() {
        // A directory the filter matches is pruned during traversal, so its
        // contents never appear.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        let docs = tmp.path().join("docs");
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(docs.join("guide.md"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "docs/\n").unwrap();
        let w = walk(tmp.path(), 8, &filter).unwrap();
        let (walked, errors) = (w.files, w.errors);
        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["keep.md"], "docs pruned, keep.md remains");
        assert!(errors.is_empty());
    }

    #[test]
    fn real_fs_walk_floor_holds_against_negation() {
        // `!.arsumbris` cannot re-include the floor: it stays a name check
        // above the matcher.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("keep.md"), b"").unwrap();
        let arsumbris = tmp.path().join(".arsumbris");
        std::fs::create_dir(&arsumbris).unwrap();
        std::fs::write(arsumbris.join("index.bin"), b"").unwrap();
        let filter = WalkFilter::with_auignore(tmp.path(), "!.arsumbris\n").unwrap();
        let walked = walk(tmp.path(), 8, &filter).unwrap().files;
        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["keep.md"],
            "floor holds, .arsumbris never walked"
        );
    }

    #[test]
    fn real_fs_walk_errors_when_depth_cap_exceeded() {
        // Build a `MAX + 1`-deep nested tree and confirm `walk` errors out
        // instead of overflowing the stack. Uses a small cap so the test
        // doesn't actually create 64 directories.
        let cap = 4;
        let tmp = tempfile::tempdir().unwrap();
        let mut path = tmp.path().to_path_buf();
        for i in 0..=cap + 1 {
            path.push(format!("d{i}"));
            std::fs::create_dir(&path).unwrap();
        }
        std::fs::write(path.join("leaf.txt"), b"").unwrap();
        let err = walk(tmp.path(), cap, &scope(tmp.path())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("max depth"));
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_walk_follows_symlinked_directory() {
        // A symlink pointing at a real sibling directory is traversed,
        // not silently dropped. Pre-fix `entry.file_type().is_dir()` was
        // false for symlinks (Linux/macOS readdir returns DT_LNK), so
        // the linked subtree's files never appeared in the index.
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("inside.txt"), b"").unwrap();
        let link_dir = tmp.path().join("linked");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        let (mut walked, errors) = (w.files, w.errors);
        walked.sort();
        // Cycle detection dedupes the canonical target — we see the file
        // once via whichever path the walker visited first.
        assert_eq!(walked.len(), 1);
        assert!(walked[0].ends_with("inside.txt"));
        assert!(errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_walk_terminates_on_symlink_cycle() {
        // A symlink that points back at its own ancestor doesn't loop —
        // canonical-path dedup catches it on the second visit.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("top.txt"), b"").unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("bottom.txt"), b"").unwrap();
        // sub/loop -> tmp (parent of sub)
        std::os::unix::fs::symlink(tmp.path(), sub.join("loop")).unwrap();

        let mut walked = walk(tmp.path(), 8, &scope(tmp.path())).unwrap().files;
        walked.sort();
        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["bottom.txt", "top.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_walk_continues_past_broken_file_symlink() {
        // A symlink at file position whose target doesn't exist: `metadata`
        // follows the link and fails. Pre-fix this aborted the whole walk;
        // now it's recorded in `errors` and the siblings still come back.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("good.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("nonexistent-target"),
            tmp.path().join("broken-link"),
        )
        .unwrap();

        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        let (walked, errors) = (w.files, w.errors);
        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["good.txt"], "sibling must still appear");
        assert_eq!(errors.len(), 1, "broken symlink must be recorded");
        assert!(errors[0].path.ends_with("broken-link"));
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_walk_continues_past_broken_dir_symlink() {
        // A symlink at directory position whose target doesn't exist:
        // `metadata` already fails before we get to canonicalize. Either
        // way the walk should record the error and continue with siblings.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("sibling.txt"), b"").unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("nonexistent-dir"),
            tmp.path().join("dangling-dir"),
        )
        .unwrap();

        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        let (walked, errors) = (w.files, w.errors);
        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["sibling.txt"]);
        assert_eq!(errors.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn real_fs_walk_continues_past_unreadable_subdir() {
        // chmod 000 on a subdir: `read_dir` fails. Self-skip if running
        // as root (chmod is a no-op for root) — detected by checking
        // whether the chmod actually restricted access.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ok.txt"), b"").unwrap();
        let locked = tmp.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("hidden.txt"), b"").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipping: chmod 000 did not restrict access (likely root)");
            return;
        }

        let w = walk(tmp.path(), 8, &scope(tmp.path())).unwrap();
        let (walked, errors) = (w.files, w.errors);

        // Restore so tempdir cleanup can remove the dir.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

        let names: Vec<_> = walked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"ok.txt".to_string()),
            "sibling must still appear; walked: {:?}",
            names
        );
        assert!(
            !names.contains(&"hidden.txt".to_string()),
            "unreadable subdir's contents must be skipped"
        );
        assert!(
            errors.iter().any(|e| e.path == locked),
            "expected error for locked subdir; got {:?}",
            errors
        );
    }

    #[test]
    fn real_fs_walk_hard_fails_when_root_is_not_a_dir() {
        // A non-directory root is a hard fail per the `walk_files` contract,
        // not an empty success: a wrong knowledge base path must be rejected, not
        // silently report zero files.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir.txt");
        std::fs::write(&file, b"").unwrap();
        let err = walk(&file, 8, &scope(&file)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // A missing root is a hard fail too, distinguished as NotFound.
        let missing = tmp.path().join("does-not-exist");
        let err = walk(&missing, 8, &scope(&missing)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
