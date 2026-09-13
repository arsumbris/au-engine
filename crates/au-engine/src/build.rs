//! The build pipeline: walk a knowledge base, parse every file, build the type graph,
//! validate every instance, and assemble the held [`KnowledgeBase`].
//!
//! Per-file parsing goes through [`parse_file`]; the pure analysis stays in
//! au-core; this routes data between them and collects the result.
//!
//! A build can reuse a prior parse layer ([`build_reusing`]): a file whose
//! content hash is unchanged reuses its parse, and skips its read, instead of
//! re-parsing. With an empty prior layer ([`build`]) it
//! is a whole-knowledge-base full build. Either way the type graph, validation, and the
//! resolved layer recompute whole-knowledge-base over the catalog, parse reuse bounds the
//! per-file parse and read cost, not the graph and validation cost.
//!
//! This is the whole-knowledge-base pipeline. For the common edits the [`crate::incremental`]
//! fast path recomputes only the changed entities and their dependents, byte-identical
//! to this build, falling back here for anything it does not handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use au_core::{
    build_graph, folded_closure_ids, run_body_typing_checks, run_graph_structure_checks,
    run_inheritance_checks, run_location_checks, validate, validate_body, validate_docstring_links,
    validate_meta_bodies, CrossRepoResolver, TypeDef, TypeGraph, TypeName, ValidateContext,
};
use au_diagnostics::{
    ByteRange, Diagnostic, DiagnosticCode, LineIndex, Severity, Span, SuggestedFix,
};
use au_parser::{classify_by_path, is_instance_candidate_path, FileKind, FileSystem};
use au_references::RepoIndex;

use crate::diagnostics::{file_too_large_diag, read_error_diag, sort_diagnostics, walk_error_diag};
use crate::ir::{
    BuildOutcome, ContentHash, DiagSource, DiagStream, FileEntry, KnowledgeBase, OrdMap,
    ParseLayer, RepoGraphs, RepoIndexes, ResolvedInstance,
};
use crate::parse::{parse_file, FileParse};
use crate::repo::{MemberRole, RepoMap, RepoName};

/// Why a build could not open its entry.
///
/// The entry MUST be a folder-repo: a directory carrying a valid, named
/// `.arsumbris/repo.yaml`. Anything else is refused up front, `EntryNotARepo`,
/// before any walk. An IO failure reading or walking propagates as `Io`. The
/// distinct variants let the daemon map the refusal to its own exit code without
/// sniffing an error string.
#[derive(Debug)]
pub enum BuildError {
    /// An IO failure reading or walking the entry.
    Io(std::io::Error),
    /// The entry is not a folder-repo: no directory-level `.arsumbris/repo.yaml`,
    /// or it is unreadable, unparseable, or nameless. `reason` carries the parse
    /// detail (message plus any quoting hint) when the file exists but will not
    /// load, so the refusal names the cause; `None` when the file is simply
    /// absent.
    EntryNotARepo {
        path: PathBuf,
        reason: Option<String>,
    },
    /// The entry roots at the reserved device root `~/.arsumbris`, whose
    /// `.arsumbris/` IS the device config/data area. A repo cannot root there, so
    /// the daemon refuses to serve it. The distinct variant lets the daemon carry
    /// the clearer message; it maps to the same exit code as `EntryNotARepo`. See
    /// [[spec - arsumbris layout - a reserved multi-tenant device root, owner-namespaced with a category sublayer]].
    EntryReserved { path: PathBuf },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Io(e) => write!(f, "{e}"),
            BuildError::EntryNotARepo { path, reason } => {
                write!(
                    f,
                    "entry '{}' is not a folder-repo: a directory carrying a valid \
                     .arsumbris/repo.yaml is required",
                    path.display()
                )?;
                if let Some(reason) = reason {
                    write!(f, " ({reason})")?;
                }
                Ok(())
            }
            BuildError::EntryReserved { .. } => {
                write!(f, "~/.arsumbris is reserved for device config and data")
            }
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BuildError::Io(e) => Some(e),
            BuildError::EntryNotARepo { .. } => None,
            BuildError::EntryReserved { .. } => None,
        }
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        BuildError::Io(e)
    }
}

/// Build the held representation of a knowledge base from disk.
///
/// Walks the file tree through `fs`, parses every file via [`parse_file`],
/// builds the type graph, runs every load-time check, then validates every
/// instance against the graph and the knowledge base's reference index. Returns the
/// assembled [`KnowledgeBase`].
///
/// An error in the type vocabulary stops the build before instance validation,
/// see [`crate::BuildOutcome`]; the returned knowledge base still holds the load
/// diagnostics and the catalog gathered so far. A walk failure that the file
/// system surfaces as an `io::Error` propagates as [`BuildError::Io`]. An entry
/// that is not a folder-repo is refused up front, [`BuildError::EntryNotARepo`].
///
/// A from-scratch build over an empty registry, no parse reuse; see
/// [`build_reusing`] to feed a prior parse layer and a registry.
#[tracing::instrument(skip_all, fields(repo = %repo.display()))]
pub fn build(repo: &std::path::Path, fs: &impl FileSystem) -> Result<KnowledgeBase, BuildError> {
    // No device package cache: this is the deterministic test / fixture entry,
    // dependency mounting is the daemon's path.
    build_reusing(
        repo,
        fs,
        &crate::repo::UserRegistry::new(),
        &ParseLayer::default(),
        &Fingerprint::new(),
        None,
    )
}

/// The device-global package cache root, `~/.arsumbris/au-engine/cache/packages`.
///
/// `None` when `$HOME` is unknown (the daemon has already refused to start by
/// then). The daemon passes this into [`build_reusing`] so a locked dependency
/// mounts from its cached snapshot.
pub fn default_package_cache_root() -> Option<PathBuf> {
    crate::repo::device_root()
        .ok()
        .map(|d| d.join("au-engine").join("cache").join("packages"))
}

/// [`build`], reusing a prior parse layer.
///
/// Each file whose content hash matches its entry in `prior` reuses that
/// parse and line index instead of re-parsing; a changed, added, or absent file
/// parses fresh. The reuse is transparent: [`parse_file`] is pure over
/// `(path, bytes)`, so a hash match yields the identical parse. An empty layer
/// parses everything, the from-scratch case.
///
/// `known_hashes` lets the build skip even the read of an unchanged file: a
/// caller that already hashed the disk (the engine's fingerprint) passes the
/// `path -> hash` map, and a known hash that hits `prior` reuses the parse with
/// no read at all. An empty map reads every file then hashes it, the
/// from-scratch case. This does not remove the caller's own hashing pass; it
/// only avoids a second read in the build.
///
/// The rest of the pipeline (graph, validation, resolved layer, backlinks) runs
/// whole-knowledge-base over the resulting catalog, parse reuse alone bounds the
/// per-file parse cost.
#[tracing::instrument(skip_all, fields(repo = %repo.display()))]
pub fn build_reusing(
    repo: &std::path::Path,
    fs: &impl FileSystem,
    registry: &crate::repo::UserRegistry,
    prior: &ParseLayer,
    known_hashes: &Fingerprint,
    cache_root: Option<&std::path::Path>,
) -> Result<KnowledgeBase, BuildError> {
    let mut diags = DiagStore::new();
    let mut catalog: BTreeMap<PathBuf, FileEntry> = BTreeMap::new();

    // Composition: resolve the entry, then run the local walk-resolve fixpoint
    // that assembles the member set. Spanned because member assembly and its
    // walks are per-build costs that parse reuse cannot bound, so a warm
    // rebuild pays them in full.
    // `.entered()` owns the span, so dropping the guard EXITS AND CLOSES it. A
    // bare `.enter()` only exits, leaving the close to fire at end of function,
    // which reports a misleading idle and stretches the span's bar over every
    // later phase. Fields are recorded through the guard's `Deref`.
    let compose = tracing::info_span!("phase.compose", members = tracing::field::Empty).entered();

    // The entry MUST be a folder-repo: a directory carrying a valid, named
    // `.arsumbris/repo.yaml`. `resolve_entry` refuses anything else. The
    // workspace is composed from its `.arsumbris/workspace.yaml` when present,
    // else it is the entry repo plus its own `deps` (an entry-only workspace).
    let entry_span = tracing::info_span!("compose.entry").entered();
    let EntryResolution {
        workspace_dir,
        manifest,
    } = resolve_entry(repo, fs)?;
    // Read the entry manifest once, for both composition and cataloging. A
    // transient read failure at build time (the file existed at `resolve_entry`)
    // degrades to the entry-only workspace, the error surfaced.
    let entry_ws_file: Option<(PathBuf, Vec<u8>)> = match &manifest {
        Some(m) => match fs.read_file(m) {
            Ok(bytes) => Some((m.clone(), bytes)),
            Err(e) => {
                diags.push(DiagSource::KnowledgeBase, read_error_diag(m, &e));
                None
            }
        },
        None => None,
    };
    drop(entry_span);

    // The local walk-resolve fixpoint, shared with `assembly_roots` so the watcher
    // and fingerprint watch exactly the members `build` reads (including a member
    // nested under a declared member). Cache-free; the cache tier is located below.
    let LocalAssembly {
        ws: mut entry_ws,
        mut walked,
        mut walk_errors,
        mut unwalkable,
        mut seed_roots,
        mut auignore_diags,
    } = local_fixpoint(&workspace_dir, &entry_ws_file, registry, fs);
    compose.record("members", entry_ws.member_paths.len());
    drop(compose);

    // Mounting: locate the locked dependencies in the device cache and walk the
    // members that adds. Separate from composition so a dependency-heavy
    // workspace's mount cost is distinguishable from its local assembly.
    let _mount = tracing::info_span!("phase.mount").entered();

    // Locate the workspace's locked dependencies from the device-global package
    // cache, offline, once the local member set is stable. A pinned dependency
    // present in the cache mounts from its immutable snapshot as a member; the walk
    // then treats it like any member. Unix-only (the cache shells git) and only
    // when a cache root exists. A dependency the location maps to a local path
    // stays editable (a `dependency-path-overridden` advisory). The cache snapshot
    // is mounted here yet never watched or fingerprinted — immutable by sha,
    // outside any workspace, so there is nothing to watch, and `assembly_roots`
    // excludes it.
    #[cfg(unix)]
    if let Some(root) = cache_root {
        let cache = crate::pkgcache::PackageCache::new(root.to_path_buf());
        let conflicts = crate::pkgcache::locate_locked_dependencies(&mut entry_ws, &cache, fs);
        diags.extend(DiagSource::KnowledgeBase, conflicts);
    }
    #[cfg(not(unix))]
    let _ = cache_root;
    // The cache tier mounts by locked sha, outside the fixpoint, so its snapshot
    // paths get the same one-spelling normalization before they are walked.
    for path in entry_ws.member_paths.values_mut() {
        *path = crate::repo::canonical_root(path);
    }
    // Walk any cache members just added (their bounded content); the cache closure
    // is complete from the lock, so no further resolution round is needed.
    for root in entry_ws.member_paths.values().cloned().collect::<Vec<_>>() {
        walk_member_once(
            &root,
            fs,
            &mut walked,
            &mut walk_errors,
            &mut unwalkable,
            &mut seed_roots,
            &mut auignore_diags,
        );
    }
    drop(_mount);

    // Discovery: union the member walks into one file set, then discover the
    // repos over its roots and resolve their declared deps.
    let discover = tracing::info_span!(
        "phase.discover",
        files = tracing::field::Empty,
        repos = tracing::field::Empty
    )
    .entered();

    // Union the bounded member walks into one file set with absolute-path identity.
    // Each member's content is its own bounded walk (no overlap, no
    // undeclared-nested content), so the union is deduped by construction. Every
    // walked root is a member (the member set only grows across the fixpoint).
    // `bound_nested_repos` warns for an undeclared nested marker under a member
    // root; its file-drop is a no-op now that the walk is bounded up front.
    let union_span = tracing::info_span!("discover.union").entered();
    let (files, roots, member_names, member_walk_errors, repo_markers): (
        Vec<PathBuf>,
        Vec<PathBuf>,
        BTreeMap<PathBuf, RepoName>,
        BTreeMap<PathBuf, std::io::Error>,
        Vec<PathBuf>,
    ) = {
        let ws = &entry_ws;
        let member_roots: BTreeSet<PathBuf> = ws.member_paths.values().cloned().collect();
        let mut files: Vec<PathBuf> = Vec::new();
        let mut markers: Vec<PathBuf> = Vec::new();
        let mut roots: Vec<PathBuf> = Vec::new();
        for (root, (mfiles, mmarkers)) in &walked {
            if !member_roots.contains(root) {
                continue;
            }
            roots.push(root.clone());
            files.extend(mfiles.iter().cloned());
            markers.extend(mmarkers.iter().cloned());
        }
        files.sort();
        files.dedup();
        roots.sort();
        roots.dedup();
        // A disabled member contributes nothing, exactly as if absent: drop its
        // marker so no repo is discovered for it (it never enters `kb.repos`, the
        // catalog, or the graph). Its declared role stays on the `Workspace` for
        // the members read. A NESTED disabled member's marker is surfaced by the
        // walk, so without this it would be discovered as an empty repo; a
        // sibling / registry disabled member is already absent (never mounted, no
        // marker under a member root), so this makes both cases uniform.
        //
        // Dep-aware: a name that is disabled AND still mounts (a `dep` edge from
        // an active member re-pulls it, `disabled` overlays the ROLE lists not an
        // intrinsic type-dependency) keeps its marker and discovers normally. So
        // the drop is gated on NOT being a mounted member root, which also skips
        // the `declared_name_at` disk read for every mounted marker.
        if !ws.disabled.is_empty() {
            let mounted_roots: BTreeSet<&Path> =
                ws.member_paths.values().map(PathBuf::as_path).collect();
            markers.retain(|m| {
                let root = m.parent().and_then(|p| p.parent());
                match root {
                    // A mounted member root is never dropped (a disabled-and-dep
                    // member discovers as its dep); no read needed.
                    Some(r) if mounted_roots.contains(r) => true,
                    Some(r) => crate::repo::declared_name_at(r, fs)
                        .is_none_or(|name| !ws.disabled.contains(&name)),
                    None => true,
                }
            });
        }
        // Declared members that did NOT mount (ambiguous / unresolved). A nested
        // repo sharing such a name is owned by duplicate-repo-name, not undeclared.
        let declared_unmounted: BTreeSet<RepoName> = ws
            .edit
            .iter()
            .chain(ws.discover.iter())
            .filter(|n| !ws.member_paths.contains_key(*n))
            .cloned()
            .collect();
        auignore_diags.extend(report_undeclared_nested_repos(
            &mut markers,
            &member_roots,
            &declared_unmounted,
            fs,
        ));
        let member_names = ws
            .member_paths
            .iter()
            .map(|(name, path)| (path.clone(), name.clone()))
            .collect();
        (files, roots, member_names, unwalkable, markers)
    };
    diags.extend(
        DiagSource::KnowledgeBase,
        walk_errors.iter().map(walk_error_diag),
    );
    diags.extend(DiagSource::KnowledgeBase, auignore_diags);
    drop(union_span);

    // Repo discovery over the roots.
    // The registry is an engine-schema file: read once, parsed into membership,
    // and catalogued as a first-class node. The diagnostics are advisory.
    //
    // Split three ways: reading every member's registry file, building the repo
    // map from them, and resolving each repo's declared deps. They are separately
    // spanned because they scale differently — the first with member count and
    // disk, the third with the dependency graph — so a fix aimed at the wrong one
    // would measure flat.
    let reg_files = {
        let _s = tracing::info_span!("discover.registry_files").entered();
        crate::repo::registry_files(&roots, &files, &repo_markers, fs)
    };
    let (mut repos, repo_diags) = {
        let _s = tracing::info_span!("discover.repos").entered();
        crate::repo::discover_repos(&roots, &reg_files, &member_names, cache_root)
    };
    // Every `.arsumbris/repo.yaml` marker the walker surfaced should parse. A
    // broken one denies its repo a name, so it drops out of membership before
    // discovery can surface the parse error: a nested broken repo reads only as
    // `undeclared-nested-repo`, a broken member silently vanishes. Validate the
    // surfaced markers directly so the malformed file names itself, deduped by
    // path against whatever discovery already reported for the same file.
    let registry_parse_diags: Vec<Diagnostic> = {
        let _s = tracing::info_span!("discover.validate_markers").entered();
        let mut seen: BTreeSet<&Path> = repo_diags
            .iter()
            .filter(|d| {
                matches!(
                    d.code.as_str(),
                    "repo-registry-parse-error" | "repo-name-missing"
                )
            })
            .map(|d| d.span.file.as_path())
            .collect();
        let mut out = Vec::new();
        for (_root, (_files, mmarkers)) in &walked {
            for marker in mmarkers {
                if !seen.insert(marker.as_path()) {
                    continue;
                }
                if let Some(d) = crate::repo::validate_registry_marker(marker, fs) {
                    out.push(d);
                }
            }
        }
        out
    };
    diags.extend(DiagSource::KnowledgeBase, repo_diags);
    diags.extend(DiagSource::KnowledgeBase, registry_parse_diags);
    // Resolve each repo's declared deps to local paths via the resolution order
    // (sibling -> registry -> cache). Fills peer_paths for the peer diagnostics;
    // the advisory notes ride to consistency_diagnostics.
    let dep_notes = {
        let _s = tracing::info_span!("discover.resolve_deps").entered();
        repos.resolve_deps(registry, fs)
    };
    discover.record("files", files.len());
    discover.record("repos", repos.repos().len());
    drop(discover);

    // Engine-schema nodes: the workspace manifests, the repo registries, and the
    // locks, each read by targeted path and catalogued as a first-class node.
    let _schema_nodes = tracing::info_span!("phase.schema_nodes").entered();

    // The entry workspace, catalogued as a node when it has a
    // `.arsumbris/workspace.yaml` file (its path lives outside every member root,
    // so it is not in `files`). This is the one workspace manifest form; a walked
    // file is never a workspace node.
    let mut workspaces: Vec<crate::repo::Workspace> = Vec::new();
    if let Some((path, bytes)) = &entry_ws_file {
        let entry = workspace_entry(path, bytes);
        diags.extend(
            DiagSource::File(path.clone()),
            entry.parse.diagnostics().iter().cloned(),
        );
        catalog.insert(path.clone(), entry);
    }
    // Snapshot the editable members (the entry plus `edit` members) and their
    // mounted roots before `entry_ws` moves into `workspaces`. These are the repos
    // on the hook for a root README; a consumed or unmounted member is absent from
    // `member_paths`, so it never appears here. Consumed by the README check once
    // the catalog is final. See [`crate::readme`].
    let editable_readme_roots: Vec<(RepoName, PathBuf)> = entry_ws
        .member_roles
        .iter()
        .filter(|(_, role)| role.editable())
        .filter_map(|(name, _)| {
            entry_ws
                .member_paths
                .get(name)
                .map(|root| (name.clone(), root.clone()))
        })
        .collect();
    workspaces.push(entry_ws);
    for (path, bytes) in &reg_files {
        // The registry file is a typed instance of `au.engine.repo`, its type
        // stamped from its kind (no written `type:`), so it is a first-class
        // node queryable via `instances_of`. Discovery still reads it
        // separately via `parse_registry`; this is the node view.
        let parse = crate::parse::parse_engine_schema_instance(
            path,
            bytes,
            "au.engine.repo",
            Some(crate::engine_schema::BUILTIN_ENGINE_REPO),
        );
        // Surface the node-view parse diagnostics (the `engine-schema-type-*`
        // self-description signals, a duplicate key, a reserved-on-instance key).
        // Discovery's `parse_registry` covers the structural repo.yaml errors on
        // a disjoint code set, so this never double-counts.
        diags.extend(
            DiagSource::File(path.clone()),
            parse.diagnostics().iter().cloned(),
        );
        catalog.insert(
            path.clone(),
            FileEntry {
                kind: FileKind::RepoRegistry,
                hash: Some(ContentHash::of(bytes)),
                parse: Arc::new(parse),
                line_index: Some(Arc::new(LineIndex::new(bytes))),
                byte_len: Some(bytes.len()),
            },
        );
    }
    // The engine-written `.arsumbris/repo.lock` is an engine-schema file like the
    // registry: under the walk floor, read by targeted path, and catalogued as a
    // typed instance node (`au.engine.repo-lock`) so it is `instances_of`-queryable
    // and validated against its def. The resolution path (the package cache) reads
    // the same bytes; this is the node view. See [[spec - engine-schema files - hardwired-schema files are first-class substrate nodes]].
    for (path, bytes) in crate::repo::lock_files(&roots, &files, &repo_markers, fs) {
        let (kind, type_name) = match path.file_name().and_then(|n| n.to_str()) {
            Some("repo.lock") => (FileKind::RepoLock, "au.engine.repo-lock"),
            _ => continue,
        };
        let parse = crate::parse::parse_engine_schema_instance(
            &path,
            &bytes,
            type_name,
            Some(crate::engine_schema::BUILTIN_ENGINE_REPO),
        );
        diags.extend(
            DiagSource::File(path.clone()),
            parse.diagnostics().iter().cloned(),
        );
        catalog.insert(
            path.clone(),
            FileEntry {
                kind,
                hash: Some(ContentHash::of(&bytes)),
                parse: Arc::new(parse),
                line_index: Some(Arc::new(LineIndex::new(&bytes))),
                byte_len: Some(bytes.len()),
            },
        );
    }
    drop(_schema_nodes);

    // A repo aborts its instance validation when its own vocabulary cannot be
    // trusted: a type-def that failed to read or parse, or a graph-level load
    // error. Tracked per repo, so a broken repo never poisons a clean one.
    let mut graph_aborted: BTreeSet<RepoName> = BTreeSet::new();

    // Type-graph pass: parse every type-def, partitioning by repo. Each repo
    // resolves its own vocabulary, so its defs feed its own graph; a
    // type a repo uses but does not define dangles.
    let mut type_defs_by_repo: BTreeMap<RepoName, Vec<TypeDef>> = BTreeMap::new();
    // Repo defs that shadow a hardwired engine name, recorded here so their
    // closure-hash can be compared to the engine's copy once the graphs exist
    // (the drift verdict, emitted after the graph loop below).
    let mut engine_shadow_sites: Vec<(PathBuf, RepoName, String, au_diagnostics::ByteRange)> =
        Vec::new();
    let typedefs = tracing::info_span!("phase.typedefs", defs = tracing::field::Empty).entered();
    for file in &files {
        if classify_by_path(file) != Some(FileKind::TypeDef) {
            continue;
        }
        // Advisory: a type-def is classified by its `.type.yaml` suffix, so this
        // fires purely on LOCATION, independent of whether the file parses. A
        // type-def outside the repo's `type/` dir is surfaced, never blocked.
        if let Some(root) = repos.repo_of(file).map(|r| r.root.clone()) {
            if let Some(diag) = type_def_outside_type_dir_diag(file, &root) {
                diags.push(DiagSource::File(file.clone()), diag);
            }
        }
        let (parse, line_index, hash, byte_len) = match acquire_parse(prior, known_hashes, fs, file)
        {
            Ok(AcquireOutcome::Parsed {
                parse,
                line_index,
                hash,
                byte_len,
            }) => (parse, line_index, hash, byte_len),
            Ok(AcquireOutcome::TooLarge { size }) => {
                // An over-cap type-def is skipped like an unreadable one: it
                // cannot feed its repo's vocabulary, so the repo aborts.
                diags.push(
                    DiagSource::File(file.clone()),
                    file_too_large_diag(file, size, MAX_READ_BYTES),
                );
                graph_aborted.insert(repo_of(&repos, file));
                catalog.insert(file.clone(), unread_entry(FileKind::TypeDef));
                continue;
            }
            Err(e) => {
                // An unreadable type-def is a vocabulary error for its repo.
                diags.push(DiagSource::File(file.clone()), read_error_diag(file, &e));
                graph_aborted.insert(repo_of(&repos, file));
                catalog.insert(file.clone(), unread_entry(FileKind::TypeDef));
                continue;
            }
        };
        let parse_diags = parse.diagnostics();
        // A malformed type-def (e.g. unparseable YAML) is a vocabulary error:
        // abort its repo, the graph build never sees the def.
        if parse_diags.iter().any(|d| d.severity == Severity::Error) {
            graph_aborted.insert(repo_of(&repos, file));
        }
        diags.extend(DiagSource::File(file.clone()), parse_diags.iter().cloned());
        if let FileParse::TypeDef {
            type_def: Some(td), ..
        } = parse.as_ref()
        {
            // A knowledge base def named in the reserved `au.engine.*` namespace coexists
            // with the hardwired set as a distinct `(name, hash)` identity; note
            // the reservation, advisory, never a block. The builtin's own defs
            // are seeded below, not walked, so they never reach this check.
            if let Some(diag) = crate::engine_schema::engine_name_reservation_diag(td) {
                diags.push(DiagSource::File(file.clone()), diag);
            }
            // A shadow of a hardwired name gets a post-graph drift check; record
            // its site now, while the def's file and span are in hand.
            if crate::engine_schema::is_hardwired_engine_name(td.name.0.as_str()) {
                engine_shadow_sites.push((
                    file.clone(),
                    repo_of(&repos, file),
                    td.name.0.clone(),
                    td.source_span,
                ));
            }
            type_defs_by_repo
                .entry(repo_of(&repos, file))
                .or_default()
                .push(td.clone());
        }
        catalog.insert(
            file.clone(),
            FileEntry {
                kind: FileKind::TypeDef,
                hash: Some(hash),
                parse,
                line_index,
                byte_len: Some(byte_len),
            },
        );
    }
    // Guarded: `record`'s argument is an ordinary expression, evaluated at the
    // call site whether or not a subscriber exists. `is_disabled()` is a null
    // check on the span's inner handle, so an untraced build pays that and
    // nothing else. See the field rules in
    // [[spec - operation tracing - env-gated spans at the seams emit a perfetto trace and a timing log]].
    if !typedefs.is_disabled() {
        typedefs.record(
            "defs",
            type_defs_by_repo.values().map(Vec::len).sum::<usize>(),
        );
    }
    drop(typedefs);

    // Install the compiled-in `au-engine` repo and seed its hardwired
    // `au.engine.*` defs, so the graph loop below builds its graph on the one
    // code path (running the load checks as a self-test). The builtin has no
    // on-disk root, so no walked file routed into it above; its defs are the
    // only source of its graph. This makes `::au-engine` resolve as a universal
    // peer, see [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]]
    // "Engine files are typed".
    // Spanned because `builtin_engine_defs()` reconstructs the hardwired def set
    // on EVERY build, so a growing engine schema shows up here rather than
    // hiding inside the graph phase.
    let _builtin = tracing::info_span!("phase.builtin").entered();
    repos.install_engine_builtin();
    type_defs_by_repo.insert(
        RepoName(crate::engine_schema::BUILTIN_ENGINE_REPO.to_string()),
        crate::engine_schema::builtin_engine_defs(),
    );
    drop(_builtin);

    // Build one graph per repo and run the load checks per graph. A repo whose
    // own vocabulary has an error aborts only its own instance validation; a
    // clean repo still validates, so the abort gate is per-repo.
    let _phase = tracing::info_span!("phase.graph").entered();
    let mut graphs_map: BTreeMap<RepoName, TypeGraph> = BTreeMap::new();
    for repo in repos.repos() {
        let defs = type_defs_by_repo.remove(&repo.name).unwrap_or_default();
        let graph_build = build_graph(defs);
        let graph = graph_build.graph;
        let mut load_diags = graph_build.diagnostics;
        load_diags.extend(run_graph_structure_checks(&graph));
        load_diags.extend(run_inheritance_checks(&graph));
        load_diags.extend(run_body_typing_checks(&graph));
        // Location field/body checks run POST-FOLD (below), they need the folded
        // resolution graph so a cross-repo-inherited field or body is visible.
        if load_diags.iter().any(|d| d.severity == Severity::Error) {
            graph_aborted.insert(repo.name.clone());
        }
        diags.extend(DiagSource::Repo(repo.name.clone()), load_diags);
        graphs_map.insert(repo.name.clone(), graph);
    }
    let graphs = RepoGraphs::new(graphs_map);
    drop(_phase);

    // Engine-name shadow drift: a knowledge base def sharing a hardwired engine name is a
    // distinct `(name, closure-hash)` identity (the coexistence noted by
    // `engine-name-live-shadow`). Compare its closure-hash to the engine's copy;
    // a divergence is advisory `drift`, the cross-repo-drift tier, so a consumer
    // can rank a diverged shadow above an in-sync one. In-sync (equal) is silent.
    // Spanned even though it is bounded by the shadow-site count, usually zero:
    // a region asserted to be free should be MEASURED free, not assumed.
    let _shadow_drift = tracing::info_span!("phase.shadow_drift").entered();
    let builtin_repo = RepoName(crate::engine_schema::BUILTIN_ENGINE_REPO.to_string());
    for (file, repo, name, span) in engine_shadow_sites {
        let tn = TypeName(name.clone());
        let repo_hash = graphs.of(&repo).closure_id(&tn);
        let builtin_hash = graphs.of(&builtin_repo).closure_id(&tn);
        if let (Some(v), Some(b)) = (repo_hash, builtin_hash) {
            if v != b {
                diags.push(
                    DiagSource::File(file.clone()),
                    crate::engine_schema::engine_name_shadow_drift_diag(&name, file, span),
                );
            }
        }
    }

    drop(_shadow_drift);

    // Reference indices, one per repo: each repo resolves wikilink targets
    // against its own files, so resolution is repo-local. A file routes to its
    // repo by membership. Construction surfaces case-collision diagnostics, now
    // within a repo (two repos sharing a basename is not a collision).
    let _indexes_phase = tracing::info_span!("phase.indexes").entered();
    let mut files_by_repo: BTreeMap<RepoName, Vec<PathBuf>> = BTreeMap::new();
    for file in &files {
        files_by_repo
            .entry(repo_of(&repos, file))
            .or_default()
            .push(file.clone());
    }
    let mut indexes_map: BTreeMap<RepoName, RepoIndex> = BTreeMap::new();
    for repo in repos.repos() {
        let repo_files = files_by_repo.remove(&repo.name).unwrap_or_default();
        let (index, index_diags) = RepoIndex::build(repo.root.clone(), repo_files);
        diags.extend(DiagSource::Repo(repo.name.clone()), index_diags);
        indexes_map.insert(repo.name.clone(), index);
    }
    let indexes = RepoIndexes::new(indexes_map);
    drop(_indexes_phase);

    // Instance pass: parse every instance candidate into the catalog.
    // Parse-reuse-bounded: a warm rebuild reads and parses only the dirty files,
    // so this phase separates reuse working from reuse being bypassed.
    let instances_phase =
        tracing::info_span!("phase.instances", catalog = tracing::field::Empty).entered();
    for file in &files {
        if !is_instance_candidate_path(file) {
            continue;
        }
        let (parse, line_index, hash, byte_len) = match acquire_parse(prior, known_hashes, fs, file)
        {
            Ok(AcquireOutcome::Parsed {
                parse,
                line_index,
                hash,
                byte_len,
            }) => (parse, line_index, hash, byte_len),
            Ok(AcquireOutcome::TooLarge { size }) => {
                // An over-cap instance candidate is skipped like an unreadable
                // one: catalogued as an unread entry, present for existence-
                // based references but carrying no claim, parse, or validation.
                diags.push(
                    DiagSource::File(file.clone()),
                    file_too_large_diag(file, size, MAX_READ_BYTES),
                );
                catalog.insert(file.clone(), unread_entry(FileKind::Unclassified));
                continue;
            }
            Err(e) => {
                diags.push(DiagSource::File(file.clone()), read_error_diag(file, &e));
                catalog.insert(file.clone(), unread_entry(FileKind::Unclassified));
                continue;
            }
        };
        diags.extend(
            DiagSource::File(file.clone()),
            parse.diagnostics().iter().cloned(),
        );
        // A file declaring `type:` is an Instance even if its structural parse
        // failed; everything else routed here is Unclassified.
        let kind = match parse.as_ref() {
            FileParse::Instance { .. } => FileKind::Instance,
            _ => FileKind::Unclassified,
        };
        catalog.insert(
            file.clone(),
            FileEntry {
                kind,
                hash: Some(hash),
                parse,
                line_index,
                byte_len: Some(byte_len),
            },
        );
    }

    // Catalog every remaining walked file (assets, notes). Done before any
    // instance borrow so the catalog is final for the rest of the build.
    catalog_remaining_files(&files, &mut catalog);

    // The catalog is final; collect it into its persistent ordered form for the
    // rest of the build and the held knowledge base. The full build is O(knowledge base)
    // regardless; structural sharing earns its keep in the incremental apply
    // carrier, where a clone is O(1) and a patch is O(log n).
    let catalog: OrdMap<PathBuf, FileEntry> = catalog.into_iter().collect();
    instances_phase.record("catalog", catalog.size());
    drop(instances_phase);

    // The cross-repo resolution graphs, folded from the own graphs plus the
    // catalog's discovered `::repo` import set. Built before validation so an
    // importing repo's instances validate against the folded peer types; only
    // importing repos get one. Wrapped in `Arc` and shared into the held knowledge base.
    // Built BEFORE the context maps so the addressable-record index resolves
    // owner-relative (a slot-pinned peer record carries its owner-qualified claim).
    let _phase = tracing::info_span!("phase.fold").entered();
    let resolution_graphs = Arc::new(crate::resolution_build::build_resolution_graphs(
        &graphs, &repos, &catalog,
    ));
    drop(_phase);

    let _context = tracing::info_span!("phase.context").entered();
    let context_maps = ContextMaps::assemble(&catalog, &graphs, &repos, &resolution_graphs);
    drop(_context);

    // Cross-repo `type:` parent cycles: the fold resolves parent edges to ids and
    // terminates a cycle via its dedup guard, but a cross-boundary cycle is
    // invisible to the per-repo own-graph `check_cycles`. Surface it from each
    // importing repo's resolution graph, anchored at a member that repo owns, so
    // the cyclic `type:` chain is diagnosed like its single-repo sibling.
    //
    // A cyclic `type:` chain has no base case, so the effective shape it induces
    // is undefined; the fold only terminates over a degenerate union. So a repo
    // owning a cyclic member ABORTS its own instance validation, exactly like the
    // single-repo `check_cycles` graph-load error above, rather than validating
    // instances against that union and emitting secondary noise. Each repo owning
    // a cyclic member emits its own diagnostic and aborts its own, so the abort
    // scope mirrors the per-repo own-graph gate with no cross-repo bookkeeping.
    // `graph_aborted` is still unborrowed here (the resolver borrows it below).
    for (repo, rg) in resolution_graphs.iter() {
        let cycle_diags = au_core::cross_repo_type_chain_cycles(rg);
        if cycle_diags.iter().any(|d| d.severity == Severity::Error) {
            graph_aborted.insert(repo.clone());
        }
        diags.extend(DiagSource::Repo(repo.clone()), cycle_diags);
    }

    // A `[[name::repo]]` typed reference is checked across the boundary: the
    // resolver reaches into the named repo's graph so au-core can verify the
    // target's effective type by `(name, canonical-hash)` identity. Wired into
    // every typed-reference surface, frontmatter, body slots, and meta bodies,
    // so cross-repo references are type-checked uniformly.
    let target_claims = crate::crossref::CatalogTargetClaims { catalog: &catalog };
    let cross_repo_resolver = crate::crossref::EngineCrossRepoResolver {
        repos: &repos,
        indexes: &indexes,
        graphs: &graphs,
        workspaces: &workspaces,
        graph_aborted: &graph_aborted,
        resolution_graphs: &resolution_graphs,
        target_claims: &target_claims,
    };

    // Location field/body load checks, POST-FOLD so they resolve a cross-repo
    // inherited field (name check) or a cross-repo `use:`-spliced body (fileType
    // check) over the fold, the parity of the per-instance side. Own-repo defs go
    // through the same call, the fold holds every own def too. Non-aborting: a
    // broken advisory-placement block never suppresses instance validation, so it
    // runs after the abort gate and keys to the repo's vocabulary slice.
    let _locphase = tracing::info_span!("phase.location_checks").entered();
    for repo in repos.repos() {
        let loc_diags = run_location_checks(
            graphs.of(&repo.name),
            resolution_graphs.of(&repo.name),
            Some(&cross_repo_resolver),
        );
        diags.extend(DiagSource::Repo(repo.name.clone()), loc_diags);
    }
    drop(_locphase);

    // Meta-body validation runs at the type-graph layer but needs knowledge base
    // context for reference fields inside meta bodies, so it sits here. Per
    // repo: a repo's meta error aborts only its own instance validation,
    // mirroring the per-repo load-error gate above. A graph-aborted repo never
    // reaches this; its meta bodies validate against a broken graph.
    let mut meta_aborted: BTreeSet<RepoName> = BTreeSet::new();
    for repo in repos.repos() {
        if graph_aborted.contains(&repo.name) {
            continue;
        }
        let graph = graphs.of(&repo.name);
        let ctx = context_maps.context(
            graph,
            indexes.of(&repo.name),
            Some(&cross_repo_resolver),
            resolution_graphs.of(&repo.name),
        );
        let meta_diags = validate_meta_bodies(&ctx);
        if meta_diags.iter().any(|d| d.severity == Severity::Error) {
            meta_aborted.insert(repo.name.clone());
        }
        diags.extend(DiagSource::Repo(repo.name.clone()), meta_diags);
    }

    // A repo aborts instance validation when its own vocabulary or meta bodies
    // erred. Validating against a broken graph would emit noise the user can't
    // act on until the graph is fixed.
    let aborted = |path: &std::path::Path| -> bool {
        repo_of_path(&repos, path)
            .map(|name| graph_aborted.contains(&name) || meta_aborted.contains(&name))
            .unwrap_or(false)
    };

    let _phase = tracing::info_span!("phase.validate").entered();
    for (path, entry) in &catalog {
        let FileParse::Instance {
            instance: Some(inst),
            body,
            body_offset,
            is_markdown,
            doc_links,
            ..
        } = entry.parse.as_ref()
        else {
            continue;
        };
        if aborted(path) {
            continue;
        }
        let ctx = context_maps.context(
            repo_graph_for(&graphs, &repos, path),
            indexes.of(&repo_of(&repos, path)),
            Some(&cross_repo_resolver),
            resolution_graphs.of(&repo_of(&repos, path)),
        );
        diags.extend(DiagSource::Instance(path.clone()), validate(&ctx, inst));
        diags.extend(
            DiagSource::Instance(path.clone()),
            validate_body(&ctx, inst, body, *body_offset, *is_markdown),
        );
        // Placement check: the file's repo-relative path against its type's
        // effective location. au-core is I/O-free, so the engine strips the
        // member root here, the same pattern as `type-def-outside-type-dir`.
        if let Some(root) = repos.repo_of(path).map(|r| r.root.clone()) {
            let rel = path.strip_prefix(&root).unwrap_or(path);
            diags.extend(
                DiagSource::Instance(path.clone()),
                au_core::location_check::validate_location(ctx.graph, ctx.resolution, inst, rel),
            );
        }
        // The Instance slice, matching the incremental path (both the changed
        // file and the dependent-revalidation site put docstring diagnostics
        // there), so an add/remove of a target flips the warning cleanly. A
        // File-slice mismatch would leave a stale warning the incremental
        // Instance-slice replacement never clears.
        diags.extend(
            DiagSource::Instance(path.clone()),
            validate_docstring_links(&ctx, path, doc_links),
        );
    }
    drop(_phase);

    // Navigational diagnostics for a type-def's `#:` docstrings. Type-defs are
    // not instance-validated, so they get their own pass, building a context per
    // file exactly like the instance loop. Only files carrying docstring links do
    // the work.
    let _docphase = tracing::info_span!("phase.validate_typedef_docstrings").entered();
    for (path, entry) in &catalog {
        let FileParse::TypeDef { doc_links, .. } = entry.parse.as_ref() else {
            continue;
        };
        if doc_links.is_empty() || aborted(path) {
            continue;
        }
        let ctx = context_maps.context(
            repo_graph_for(&graphs, &repos, path),
            indexes.of(&repo_of(&repos, path)),
            Some(&cross_repo_resolver),
            resolution_graphs.of(&repo_of(&repos, path)),
        );
        diags.extend(
            DiagSource::File(path.clone()),
            validate_docstring_links(&ctx, path, doc_links),
        );
    }
    drop(_docphase);

    // Repo README obligations: every editable member self-describes through a root
    // README.md, self-declared and in place. Localized in `crate::readme` so a
    // future declarable located-singleton mechanism folds it in as its first user.
    diags.extend(
        DiagSource::KnowledgeBase,
        crate::readme::readme_diagnostics(&editable_readme_roots, &repos, &catalog),
    );

    // Engine-schema floor check ([[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]]).
    // A written `type:` on an engine-schema file must name the file's
    // kind-assigned FLOOR (or a subtype whose closure folds it in). A stamped
    // floor (no written type) trivially contains it and never fires; a
    // wrong-floor written claim (e.g. a `repo.yaml` written
    // `type: au.engine.workspace::au-engine`) omits it and errors. The engine
    // still reads its data from the floor regardless, the resolution path is
    // claim-independent, so it is recoverable, never lost data. An UNRESOLVABLE
    // claim is the `unknown-type-claim` gate's, detected by an empty folded
    // closure and skipped here to avoid a double signal.
    let _schema_floor = tracing::info_span!("phase.schema_floor").entered();
    for (path, entry) in &catalog {
        let Some(floor_name) = engine_schema_floor(entry.kind) else {
            continue;
        };
        let FileParse::Instance {
            instance: Some(inst),
            ..
        } = entry.parse.as_ref()
        else {
            continue;
        };
        if aborted(path) {
            continue;
        }
        let Some(floor_id) = cross_repo_resolver
            .peer_type_id(floor_name, crate::engine_schema::BUILTIN_ENGINE_REPO)
            .map(|p| p.id)
        else {
            continue;
        };
        let Some(rg) = resolution_graphs.of(&repo_of(&repos, path)) else {
            continue;
        };
        let ids = folded_closure_ids(rg, &inst.type_claim);
        if !ids.is_empty() && !ids.contains(&floor_id) {
            diags.push(
                DiagSource::Instance(path.clone()),
                Diagnostic {
                    code: crate::engine_schema::ENGINE_SCHEMA_TYPE_FLOOR_OMITTED,
                    severity: Severity::Error,
                    span: Span::new(path.clone(), inst.type_claim.span()),
                    message: format!(
                        "engine-schema file writes a `type:` whose closure omits the kind's floor `{floor_name}::{}`; the written claim names the wrong kind",
                        crate::engine_schema::BUILTIN_ENGINE_REPO
                    ),
                    related: vec![],
                    fix: Some(SuggestedFix {
                        description: format!(
                            "include `{floor_name}::{}` in the `type:` claim; mixing in more is fine",
                            crate::engine_schema::BUILTIN_ENGINE_REPO
                        ),
                    }),
                },
            );
        }
    }

    drop(_schema_floor);

    // Resolved layer: the graph-derived per-instance analysis. None of this
    // emits diagnostics. Every parsed instance in a non-aborted repo gets an
    // entry; an unresolved claim (`unknown-type-claim` already fired) carries
    // no effective shape.
    //
    // Closure membership is built alongside: each instance's effective closure,
    // the union of its claims' transitive ancestors, inverted to a
    // `(repo, type)` to its members. The reverse index a type-def edit walks to
    // its dependent instances. Keyed by repo because a type name is per-repo, an
    // absent claimed type still keys (the instance depends on that name), which
    // only over-includes, never misses.
    //
    // TODO(cross-repo): this is built from the OWN-graph `instance_closure` (it
    // skips qualified claims) and keyed by the CONSUMING repo, so an importing
    // instance `type: note::base` is NOT indexed under `(base, note)`. It is
    // built-but-unread today (`instances_of` reads the resolved effective shape,
    // which already includes peer types), so nothing breaks now; but the
    // cross-repo type-def-edit reverse-dep walk will need importers keyed under
    // the owner repo/type, off the RESOLVED closure. Deferred deliberately.
    let _phase = tracing::info_span!("phase.resolve").entered();
    let mut instances: BTreeMap<PathBuf, ResolvedInstance> = BTreeMap::new();
    let mut closure_members: BTreeMap<(RepoName, TypeName), Vec<PathBuf>> = BTreeMap::new();
    for (path, entry) in &catalog {
        let FileParse::Instance {
            instance: Some(inst),
            ..
        } = entry.parse.as_ref()
        else {
            continue;
        };
        if aborted(path) {
            continue;
        }
        let graph = repo_graph_for(&graphs, &repos, path);
        let repo = repo_of(&repos, path);
        // The served shape routes through the resolution graph when the repo
        // imports, exactly as validation does, so a `type: foo::repo` instance's
        // served effective shape carries the folded peer fields rather than an
        // own-only (empty) shape inconsistent with its diagnostics.
        let effective_shape = crate::resolution_build::resolved_effective_shape(
            graph,
            resolution_graphs.of(&repo),
            &inst.type_claim,
        );
        instances.insert(path.clone(), ResolvedInstance { effective_shape });

        for ty in crate::incremental::instance_closure(graph, inst) {
            closure_members
                .entry((repo.clone(), ty))
                .or_default()
                .push(path.clone());
        }
    }
    drop(_phase);

    // Reverse reference index over the resolved edges. Forward validation
    // already diagnosed the unresolved ones; this only inverts what resolves.
    let _reverse_indexes = tracing::info_span!("phase.reverse_indexes").entered();
    let backlinks = crate::backlinks::build_index(&catalog, &indexes, &repos, &workspaces);

    // Referenced-name index over every edge's NAME, resolved or dangling: the
    // reverse-dependency index a path-set change walks to its edge-flip set.
    let referenced_names = crate::refnames::build_index(&catalog, &repos);
    drop(_reverse_indexes);

    // The cross-repo advisory passes, each a walk over the catalog or the repo
    // set, all producing diagnostics rather than held structure.
    let _crossrepo = tracing::info_span!("phase.crossrepo").entered();

    // The per-repo build outcome: graph error, then meta error, else complete.
    let mut outcomes: BTreeMap<RepoName, BuildOutcome> = BTreeMap::new();
    for repo in repos.repos() {
        let outcome = if graph_aborted.contains(&repo.name) {
            BuildOutcome::AbortedAtGraph
        } else if meta_aborted.contains(&repo.name) {
            BuildOutcome::AbortedAtMeta
        } else {
            BuildOutcome::Complete
        };
        outcomes.insert(repo.name.clone(), outcome);
    }

    // Cross-boundary references: `[[name::repo]]` resolves into the named repo;
    // the `reference-repo-*` diagnostics (and cross-repo target-missing /
    // ambiguous) ride here, since au-core skips `::repo` links.
    // Cross-boundary references key to their source instance, not the
    // cross-cutting knowledge-base bucket. A `::repo` reference is per source file, like
    // in-repo validation, and cross-repo references are the common case in this
    // multi-repo substrate, not an exception. Keying them per source keeps an
    // instance edit's blast radius bounded: it recomputes only its own
    // references, never a whole-knowledge-base pass. Each diagnostic's `span.file` is its
    // source (crossref emits at the source), so it routes to that instance.
    for d in
        crate::crossref::cross_repo_reference_diagnostics(&catalog, &repos, &indexes, &workspaces)
    {
        diags.push(DiagSource::Instance(d.span.file.clone()), d);
    }

    // Cross-boundary type vocabulary: a `::repo`-qualified claim, parent, or
    // field shape names a peer's type. au-core defers the qualified name; the
    // engine gates the peer here so the author gets feedback before the fold.
    // An instance's claim diagnostics key to its own slice (like validation); a
    // type-def's parent / shape diagnostics key to its repo's vocabulary bucket,
    // where a type-def edit (a NeedsFull rebuild) recomputes them.
    for (path, entry) in &catalog {
        let type_diags = crate::crosstype::cross_repo_type_diagnostics_for(
            entry.parse.as_ref(),
            &repos,
            &graphs,
            &resolution_graphs,
            &workspaces,
        );
        if type_diags.is_empty() {
            continue;
        }
        let source = match entry.kind {
            FileKind::TypeDef => DiagSource::Repo(repo_of(&repos, path)),
            _ => DiagSource::Instance(path.clone()),
        };
        diags.extend(source, type_diags);
    }

    // Cross-repo consistency: undeclared / out-of-scope / unmounted peers.
    // Advisory warnings, each carrying the fix a consumer applies.
    diags.extend(
        DiagSource::KnowledgeBase,
        consistency_diagnostics(
            &repos,
            &workspaces,
            &member_walk_errors,
            &dep_notes,
            registry,
            cache_root,
        ),
    );

    drop(_crossrepo);

    // The served stream is derived from the partitioned store: flatten, attach
    // line/columns, sort. Held alongside the partition so a recompute replaces a
    // source's slice and re-derives.
    // Collect the remaining patched map fields into their persistent ordered form
    // once the full build has finished mutating them (the catalog is already
    // collected, above). See that comment for why.
    let serve_stream = tracing::info_span!(
        "phase.serve_stream",
        diags = tracing::field::Empty,
        diag_keys = tracing::field::Empty
    )
    .entered();
    let instances: OrdMap<PathBuf, ResolvedInstance> = instances.into_iter().collect();
    let diagnostics_by_source: OrdMap<DiagSource, Vec<Diagnostic>> =
        diags.into_by_source().into_iter().collect();
    let served = served_stream(&diagnostics_by_source, &catalog);
    // `diags` is the diagnostic count a reader actually wants, and it needs a
    // walk over the groups. The guard is what makes recording it free when
    // untraced, so the useful number is reported rather than a cheap proxy.
    // `diag_keys` is the group count, the sort keys the stream is bucketed by.
    if !serve_stream.is_disabled() {
        serve_stream.record("diags", served.values().map(Vec::len).sum::<usize>());
        serve_stream.record("diag_keys", served.size());
    }
    drop(serve_stream);

    // The held-state assembly: four `BTreeMap -> OrdMap` collects (backlinks,
    // closure members, referenced names, outcomes), each an O(n log n)
    // persistent-map build over an index that scales with the edge count. The
    // guard drops after the tail expression, so the span covers the struct.
    let _assemble = tracing::info_span!("phase.assemble").entered();
    Ok(KnowledgeBase {
        catalog,
        graphs: Arc::new(graphs),
        resolution_graphs,
        indexes,
        instances,
        backlinks: backlinks.into_iter().collect(),
        closure_members: closure_members.into_iter().collect(),
        referenced_names: referenced_names.into_iter().collect(),
        served,
        diagnostics_by_source,
        repos: Arc::new(repos),
        workspaces: Arc::new(workspaces),
        outcomes: outcomes.into_iter().collect(),
    })
}

/// Accumulates diagnostics keyed by producing source through a build, so the
/// held stream is partitioned for incremental replacement. The served stream is
/// derived from it by [`served_diagnostics`].
struct DiagStore {
    by_source: BTreeMap<DiagSource, Vec<Diagnostic>>,
}

impl DiagStore {
    fn new() -> Self {
        Self {
            by_source: BTreeMap::new(),
        }
    }

    fn push(&mut self, source: DiagSource, diag: Diagnostic) {
        self.by_source.entry(source).or_default().push(diag);
    }

    fn extend(&mut self, source: DiagSource, diags: impl IntoIterator<Item = Diagnostic>) {
        self.by_source.entry(source).or_default().extend(diags);
    }

    fn into_by_source(self) -> BTreeMap<DiagSource, Vec<Diagnostic>> {
        self.by_source
    }
}

/// Derive the served diagnostic stream from the partitioned store.
///
/// Flatten every source's raw diagnostics, attach line/columns, and sort into
/// the canonical total order. The total sort makes the flatten order
/// irrelevant, so the held-map
/// iteration order does not affect the result, the partition can be re-merged in
/// any order after an incremental replacement and yield the identical stream.
pub(crate) fn served_diagnostics(
    by_source: &OrdMap<DiagSource, Vec<Diagnostic>>,
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> Vec<Diagnostic> {
    let mut merged: Vec<Diagnostic> = by_source.values().flatten().cloned().collect();
    attach_line_cols(&mut merged, catalog);
    sort_diagnostics(&mut merged);
    merged
}

/// One source's raw diagnostics in served form: line/columns attached. The
/// per-source unit the incremental apply splices into the held stream, the same
/// transform [`served_diagnostics`] runs over the whole stream, minus the
/// cross-source sort (the held stream is keyed in sort order, so a spliced entry
/// lands in place).
pub(crate) fn serve_slice(
    raw: &[Diagnostic],
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> Vec<Diagnostic> {
    let mut v = raw.to_vec();
    attach_line_cols(&mut v, catalog);
    v
}

/// The served diagnostic stream as the held [`DiagStream`]: the sorted served
/// `Vec`, grouped so consecutive entries sharing the total sort key
/// `(file, byte-start, code)` go in one bucket, preserving their within-key
/// content order. Iterating the result reproduces the sorted `Vec`.
pub(crate) fn served_stream(
    by_source: &OrdMap<DiagSource, Vec<Diagnostic>>,
    catalog: &OrdMap<PathBuf, FileEntry>,
) -> DiagStream {
    let mut map = DiagStream::new_sync();
    for d in served_diagnostics(by_source, catalog) {
        let key = (
            d.span.file.clone(),
            d.span.range.start,
            d.code.as_str().to_string(),
        );
        match map.get_mut(&key) {
            Some(bucket) => bucket.push(d),
            None => map.insert_mut(key, vec![d]),
        }
    }
    map
}

/// The resolved entry: the folder-repo workspace directory and its optional
/// composition.
#[derive(Debug)]
pub(crate) struct EntryResolution {
    /// The entry directory, the folder-repo root and the walk root.
    pub workspace_dir: PathBuf,
    /// The `.arsumbris/workspace.yaml` composition, when the entry declares one.
    /// `None` means the workspace is the entry repo plus its own `deps`.
    pub manifest: Option<PathBuf>,
}

/// Resolve the entry into a folder-repo workspace directory and its optional
/// composition, or refuse a non-repo entry.
///
/// The entry MUST be a directory carrying a valid, named `.arsumbris/repo.yaml`;
/// anything else, a bare directory, a plain file, a nameless or unreadable
/// `repo.yaml`, is [`BuildError::EntryNotARepo`]. When the entry also
/// carries a `.arsumbris/workspace.yaml`, that composition is returned; otherwise
/// the workspace is the entry repo plus its own `deps` (an entry-only workspace).
pub(crate) fn resolve_entry(
    entry: &std::path::Path,
    fs: &impl FileSystem,
) -> Result<EntryResolution, BuildError> {
    // Refuse an entry rooted at the reserved device root `~/.arsumbris` before the
    // not-a-repo check, so `au daemon start ~` gets the clearer reserved message
    // rather than the generic not-a-folder-repo. The device area IS this entry's
    // `.arsumbris`, so serving it would collide config/data with a repo declaration.
    if crate::repo::is_reserved_root(entry) {
        return Err(BuildError::EntryReserved {
            path: entry.to_path_buf(),
        });
    }
    // A folder-repo declares a valid, named `.arsumbris/repo.yaml`. Reuse the
    // discovery parse so a nameless or malformed `repo.yaml` refuses the same way,
    // and carry the parse detail so the refusal names the cause (an unquoted `:`
    // in a value is the common footgun) instead of a bare not-a-folder-repo.
    if let Err(load_err) = crate::repo::entry_repo_load(entry, fs) {
        let reason = load_err.map(|d| match d.fix {
            Some(fix) => format!("{}: {}", d.message, fix.description),
            None => d.message,
        });
        return Err(BuildError::EntryNotARepo {
            path: entry.to_path_buf(),
            reason,
        });
    }
    let ws = entry.join(".arsumbris").join("workspace.yaml");
    // Distinguish an ABSENT manifest from a present-but-unreadable one, never
    // conflate them. Absent is an entry-only workspace (`None`). Present but
    // unreadable is carried through as `Some`, so the content read in
    // `build_reusing` surfaces the error as a diagnostic instead of silently
    // dropping every declared member (a silent drop is worse than a rejection).
    let manifest = match fs.read_file(&ws) {
        Ok(_) => Some(ws),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(ws),
    };
    Ok(EntryResolution {
        workspace_dir: entry.to_path_buf(),
        manifest,
    })
}

/// Verify an entry is a folder-repo (a directory carrying a valid, named
/// `.arsumbris/repo.yaml`), the daemon's preflight before it boots the engine and
/// serves. `Ok(())` when the entry can be opened, `Err(BuildError::EntryNotARepo)`
/// when it must be refused. Does not build the knowledge base.
pub fn verify_entry(entry: &std::path::Path) -> Result<(), BuildError> {
    resolve_entry(entry, &au_parser::RealFileSystem).map(|_| ())
}

/// Whether a manifest path is a folder-repo `.arsumbris/workspace.yaml`. Its
/// containing repo is the grandparent directory.
pub(crate) fn is_folder_repo_manifest(manifest: &std::path::Path) -> bool {
    manifest.file_name().is_some_and(|n| n == "workspace.yaml")
        && manifest
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == ".arsumbris")
}

/// The walk / watch / fingerprint roots for an engine pointed at `entry`, plus
/// the entry manifest when it declares a `.arsumbris/workspace.yaml`. Always
/// assembly now: the mounted member roots (a `workspace.yaml` composition, else
/// the entry repo plus its `deps`). Shared by `fingerprint` and the watcher so
/// both cover exactly what `build` reads. A non-repo entry degrades to watching
/// the entry directory, so the daemon re-derives once a valid `repo.yaml`
/// appears; `build` refuses meanwhile.
/// Spanned: it runs the same walk-resolve fixpoint the build does, so a rebuild
/// can walk the member set more than once. The duplication is only visible with
/// both this and `phase.compose` named.
#[tracing::instrument(skip_all)]
pub(crate) fn assembly_roots(
    entry: &std::path::Path,
    registry: &crate::repo::UserRegistry,
    fs: &impl FileSystem,
) -> (Vec<PathBuf>, Option<PathBuf>) {
    let Ok(EntryResolution {
        workspace_dir,
        manifest,
    }) = resolve_entry(entry, fs)
    else {
        return (vec![entry.to_path_buf()], None);
    };
    // Read the entry manifest. A present-but-unreadable manifest degrades to an
    // entry-only assembly (the entry repo plus its `deps`), exactly what `build`
    // reads in the same case, so the watcher / fingerprint still cover the entry's
    // scattered deps. The manifest path is still returned so the watcher re-derives
    // once it becomes readable; `build` surfaces the read error as a diagnostic.
    let entry_ws_file = match &manifest {
        Some(m) => fs.read_file(m).ok().map(|bytes| (m.clone(), bytes)),
        None => None,
    };
    // The SAME local fixpoint `build` runs, so the watcher and fingerprint watch
    // exactly the members `build` reads, a member nested under a declared member
    // included. Cache members are deliberately absent (immutable, unwatched).
    let assembly = local_fixpoint(&workspace_dir, &entry_ws_file, registry, fs);
    let mut roots: Vec<PathBuf> = assembly.ws.member_paths.values().cloned().collect();
    roots.sort();
    roots.dedup();
    (roots, manifest)
}

/// The walk filter for a member (or tree-mode knowledge base) root. Reads
/// `<root>/.arsumbris/.auignore` out-of-band, the same pattern as the registry.
/// Absent → the default excludes. Present and valid → the default excludes
/// layered under the user file. Present but unreadable or malformed → an
/// `auignore-load-error` diagnostic and a fall back to the default excludes, so
/// scoping degrades loudly rather than silently.
///
/// The `bool` reports whether a user `.auignore` was applied (not a fallback to
/// the default excludes), so the caller can flag an over-broad scope that
/// empties the member.
pub(crate) fn member_walk_filter(
    root: &Path,
    fs: &impl FileSystem,
    diags: &mut Vec<Diagnostic>,
) -> (au_parser::WalkFilter, bool) {
    let auignore = root.join(".arsumbris").join(".auignore");
    match fs.read_file(&auignore) {
        Ok(bytes) => {
            let contents = String::from_utf8_lossy(&bytes);
            match au_parser::WalkFilter::with_auignore(root, &contents) {
                Ok(filter) => (filter, true),
                Err(e) => {
                    diags.push(crate::diagnostics::auignore_load_error_diag(
                        &auignore,
                        &e.to_string(),
                    ));
                    (au_parser::WalkFilter::default_excludes(root), false)
                }
            }
        }
        // Absent is the common case, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (au_parser::WalkFilter::default_excludes(root), false)
        }
        // Present but unreadable (permission, I/O): report and fall back.
        Err(e) => {
            diags.push(crate::diagnostics::auignore_load_error_diag(
                &auignore,
                &e.to_string(),
            ));
            (au_parser::WalkFilter::default_excludes(root), false)
        }
    }
}

/// Whether the member's own `.auignore` patterns are what emptied it, versus a
/// genuinely empty member (or one holding only default-excluded / floored
/// content). Callers reach this only after the full-filter walk already came
/// back empty for a member that carries an `.auignore`, so a non-empty
/// default-excludes walk means the user patterns did the emptying, the real
/// `auignore-empty-scope` signal. Absent that, the member was empty anyway and
/// the warning would be a false positive. The extra walk is affordable because
/// this runs only for the rare emptied-member case, and a failing re-walk
/// (already unlikely, the full walk just succeeded) conservatively suppresses
/// the warning rather than inventing one.
fn auignore_emptied_the_member(root: &Path, fs: &impl FileSystem) -> bool {
    fs.walk_files(root, &au_parser::WalkFilter::default_excludes(root))
        .map(|w| !w.files.is_empty())
        .unwrap_or(false)
}

/// Walk one member root once, bounded at nested repo markers, into the fixpoint's
/// accumulators. Records the member's `(files, markers)` in `walked`, moves its
/// soft per-entry walk errors into `walk_errors`, and folds its FIRST-level nested
/// marker roots into `seed_roots`. Returns whether a NEW marker root appeared, the
/// walk-resolve fixpoint's progress signal: a fresh marker means a member nested
/// under this one can now resolve, so another resolution round is due.
///
/// Skips a root already walked or already known-unwalkable, so each member is
/// walked at most once across the whole fixpoint (the single-walk-per-repo
/// invariant). A root-level walk failure (missing, a file not a directory, or
/// unreadable) is recorded in `unwalkable`, NOT dropped: the member is absent from
/// the walked set, so no repo is discovered there, and the consistency taxonomy
/// surfaces it as `workspace-member-unwalkable` or a role-keyed unmounted code
/// (`edit-member-unmounted` / `discover-member-unmounted` / `peer-unmounted`).
///
/// Each member honors its own `.arsumbris/.auignore`; an over-broad scope that
/// empties a member fires `auignore-empty-scope`.
fn walk_member_once(
    root: &Path,
    fs: &impl FileSystem,
    walked: &mut BTreeMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)>,
    walk_errors: &mut Vec<au_parser::WalkError>,
    unwalkable: &mut BTreeMap<PathBuf, std::io::Error>,
    seed_roots: &mut BTreeSet<PathBuf>,
    auignore_diags: &mut Vec<Diagnostic>,
) -> bool {
    if walked.contains_key(root) || unwalkable.contains_key(root) {
        return false;
    }
    // Spanned below the early return, so one span means one real walk: the
    // fixpoint calls this repeatedly per member and every later call is a
    // no-op. Per member rather than per phase, since the question a slow
    // compose raises is WHICH member's tree is expensive.
    let walk = tracing::info_span!(
        "compose.walk",
        root = %root.display(),
        files = tracing::field::Empty
    )
    .entered();
    let (filter, had_auignore) = member_walk_filter(root, fs, auignore_diags);
    match fs.walk_files(root, &filter) {
        Ok(w) => {
            // Only warn when the user's patterns are what emptied the member, not
            // a genuinely empty member.
            if had_auignore && w.files.is_empty() && auignore_emptied_the_member(root, fs) {
                auignore_diags.push(crate::diagnostics::auignore_empty_scope_diag(root));
            }
            let au_parser::Walk {
                files,
                repo_markers,
                errors,
            } = w;
            walk_errors.extend(errors);
            let mut grew = false;
            for r in crate::repo::marker_roots(&repo_markers) {
                if seed_roots.insert(r) {
                    grew = true;
                }
            }
            walk.record("files", files.len());
            walked.insert(root.to_path_buf(), (files, repo_markers));
            grew
        }
        Err(source) => {
            unwalkable.insert(root.to_path_buf(), source);
            false
        }
    }
}

/// The result of the local walk-resolve fixpoint: the stable workspace and the
/// per-member bounded walks it accumulated (`root -> (files, markers)`), plus the
/// soft walk errors, the unwalkable members, the accumulated marker roots, and the
/// scoping diagnostics. Cache-free: the cache tier is located by the caller.
pub(crate) struct LocalAssembly {
    pub(crate) ws: crate::repo::Workspace,
    walked: BTreeMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)>,
    walk_errors: Vec<au_parser::WalkError>,
    unwalkable: BTreeMap<PathBuf, std::io::Error>,
    seed_roots: BTreeSet<PathBuf>,
    auignore_diags: Vec<Diagnostic>,
}

/// Run the local walk-resolve fixpoint over a folder-repo entry.
///
/// Walk the entry as its first member, resolve the workspace, walk each member once
/// (bounded at nested markers), surfacing markers that seed the next round, until a
/// full pass adds no new marker root. The member set is then stable and independent
/// of `workspace.yaml` order, and a member nested under a declared member resolves
/// once its ancestor is walked. `seed_roots` grows monotonically to a fixed point,
/// so it terminates.
///
/// Cache-free: `assemble_*` never consults the cache (tier 3), so a caller that
/// wants cache members locates them afterward. Shared by `build_reusing` (which
/// reuses the walks for content and locates the cache after) and `assembly_roots`
/// (which takes just the member roots to watch and fingerprint), so the two never
/// disagree about the member set.
/// A read-through cache over a `FileSystem`, scoped to member assembly.
///
/// `local_fixpoint` re-resolves every member name against a growing root set,
/// each round, and each resolution re-reads the same handful of `repo.yaml`
/// markers — an O(members² × rounds) re-read of a fixed file set (measured at
/// ~510ms for ~150 members). Caching successful reads collapses that to one read
/// per distinct path. A build reads a fixed on-disk snapshot, so a cached read is
/// byte-identical to a fresh one; errors pass through uncached (rare, possibly
/// transient). Walks delegate straight through; only `read_file` is memoized.
struct AssemblyReadCache<'a, F: FileSystem> {
    inner: &'a F,
    cache: std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<u8>>>,
}

impl<'a, F: FileSystem> AssemblyReadCache<'a, F> {
    fn new(inner: &'a F) -> Self {
        Self {
            inner,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl<F: FileSystem> FileSystem for AssemblyReadCache<'_, F> {
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        if let Some(bytes) = self.cache.lock().unwrap().get(path) {
            return Ok(bytes.clone());
        }
        let bytes = self.inner.read_file(path)?;
        self.cache
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), bytes.clone());
        Ok(bytes)
    }

    /// Not cached: it decodes nothing, so there is no read to save, and a cached
    /// presence answer could outlive the fact it recorded.
    fn is_file(&self, path: &Path) -> bool {
        self.inner.is_file(path)
    }

    /// Delegated to the inner filesystem, NOT the read-based default: the read
    /// cap must stay a cheap stat, and a caching wrapper measuring size by
    /// reading would defeat the very allocation the cap avoids.
    fn file_len(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.file_len(path)
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

pub(crate) fn local_fixpoint(
    workspace_dir: &Path,
    entry_ws_file: &Option<(PathBuf, Vec<u8>)>,
    registry: &crate::repo::UserRegistry,
    fs: &impl FileSystem,
) -> LocalAssembly {
    // Dedupe the fixpoint's repeated repo.yaml / config reads across the whole
    // assembly. Scoped to this call, dropped on return.
    let cache = AssemblyReadCache::new(fs);
    let fs = &cache;

    // One spelling per member root, fixed before anything walks. The walk keys
    // the catalog by these, `Repo.root` records them, and the write path rejoins
    // onto them, so a second spelling of one directory would be a second identity
    // for the same files. Normalizing at this boundary is what lets every
    // downstream path operation stay lexical. See [`crate::repo::canonical_root`].
    let workspace_dir = &crate::repo::canonical_root(workspace_dir);

    let mut walked: BTreeMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)> = BTreeMap::new();
    let mut walk_errors: Vec<au_parser::WalkError> = Vec::new();
    let mut unwalkable: BTreeMap<PathBuf, std::io::Error> = BTreeMap::new();
    let mut seed_roots: BTreeSet<PathBuf> = BTreeSet::new();
    let mut auignore_diags: Vec<Diagnostic> = Vec::new();
    walk_member_once(
        workspace_dir,
        fs,
        &mut walked,
        &mut walk_errors,
        &mut unwalkable,
        &mut seed_roots,
        &mut auignore_diags,
    );
    let ws = loop {
        let seed_vec: Vec<PathBuf> = seed_roots.iter().cloned().collect();
        // The RESOLVE half of composition, as opposed to the walk half the
        // `compose.walk` spans cover: registry lookups, sibling probes, and
        // repo.yaml parsing for every member. Fires once per fixpoint round, so
        // its busy total is the whole cost across rounds.
        let mut ws = {
            let _load = tracing::info_span!("compose.load_workspace").entered();
            match entry_ws_file {
                Some((m, bytes)) => crate::repo::load_workspace(m, bytes, &seed_vec, registry, fs),
                None => crate::repo::entry_only_workspace(workspace_dir, &seed_vec, registry, fs),
            }
        };
        // Resolution reaches a member three ways (sibling probe, registry entry,
        // cache snapshot), each of which can hand back a different spelling of the
        // same directory. Normalize every one before it is walked, so the member
        // set is keyed by directory rather than by how it was addressed.
        for path in ws.member_paths.values_mut() {
            *path = crate::repo::canonical_root(path);
        }
        let mut grew = false;
        for root in ws.member_paths.values().cloned().collect::<Vec<_>>() {
            if walk_member_once(
                &root,
                fs,
                &mut walked,
                &mut walk_errors,
                &mut unwalkable,
                &mut seed_roots,
                &mut auignore_diags,
            ) {
                grew = true;
            }
        }
        if !grew {
            break ws;
        }
    };
    LocalAssembly {
        ws,
        walked,
        walk_errors,
        unwalkable,
        seed_roots,
        auignore_diags,
    }
}

/// Report each UNDECLARED nested repo and hide its marker from discovery, the
/// folder-repo nested-repo-skip rule.
///
/// A surfaced marker under a member root that is NOT itself a member root is an
/// undeclared nested repo. The bounded walk already stopped at it, so its content
/// was never walked, there is nothing to drop from the file set. This drops its
/// marker from `markers` so `discover_repos` builds no repo for it (it contributes
/// nothing), and reports `undeclared-nested-repo` naming the fix. A DECLARED nested
/// repo is itself a member root, walked as its own member, and never flagged.
///
/// Only the OUTERMOST undeclared repo on each path is reported: the enclosing
/// member's bounded walk surfaces first-level markers only, so a repo buried inside
/// another undeclared repo is never walked, never surfaces, and is the outer repo's
/// concern.
fn report_undeclared_nested_repos(
    markers: &mut Vec<PathBuf>,
    member_roots: &BTreeSet<PathBuf>,
    declared_unmounted: &BTreeSet<RepoName>,
    fs: &impl FileSystem,
) -> Vec<Diagnostic> {
    use au_diagnostics::{ByteRange, Span, SuggestedFix};

    let all_roots = crate::repo::marker_roots(markers);
    // A marker root strictly under a member root, not itself a member.
    let nested: BTreeSet<PathBuf> = all_roots
        .iter()
        .filter(|r| {
            !member_roots.contains(*r)
                && member_roots
                    .iter()
                    .any(|m| r.starts_with(m) && r.as_path() != m.as_path())
                // A DECLARED name that failed to MOUNT (e.g. two nested repos share
                // one name, an ambiguous collision) is owned by `duplicate-repo-name`
                // and the unmounted-member diagnostics, not "undeclared", so skip the
                // false warning. A name that DID mount elsewhere makes this nested
                // root a same-named impostor, still genuinely undeclared, so keep it.
                && crate::repo::declared_name_at(r, fs)
                    .is_none_or(|n| !declared_unmounted.contains(&n))
        })
        .cloned()
        .collect();
    if nested.is_empty() {
        return Vec::new();
    }
    // Drop nested markers so `discover_repos` builds no repo for them.
    markers.retain(|m| {
        m.parent()
            .and_then(|p| p.parent())
            .is_none_or(|r| !nested.contains(r))
    });
    // One advisory per undeclared nested repo, named by its declared repo name.
    nested
        .iter()
        .map(|root| {
            let marker = root.join(".arsumbris").join("repo.yaml");
            let name = crate::repo::declared_name_at(root, fs)
                .map(|n| n.as_str().to_string())
                .unwrap_or_else(|| {
                    root.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            Diagnostic {
                code: crate::repo::UNDECLARED_NESTED_REPO,
                severity: Severity::Warning,
                span: Span::new(marker, ByteRange::new(0, 0)),
                message: format!(
                    "nested repo '{name}' is not a declared member; its subtree is skipped and contributes nothing"
                ),
                related: vec![],
                fix: Some(SuggestedFix {
                    description: format!(
                        "declare '{name}' as an 'edit' / 'discover' member in workspace.yaml, or as a 'dep' in repo.yaml"
                    ),
                }),
            }
        })
        .collect()
}

/// The kind-assigned FLOOR type-name for an engine-schema file kind, or `None`
/// for a kind that is not an engine-schema node. Drives the
/// `engine-schema-type-floor-omitted` check: a written `type:` whose resolved
/// closure omits this floor names the wrong kind. See
/// [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
fn engine_schema_floor(kind: FileKind) -> Option<&'static str> {
    match kind {
        FileKind::RepoRegistry => Some("au.engine.repo"),
        FileKind::Workspace => Some("au.engine.workspace"),
        FileKind::RepoLock => Some("au.engine.repo-lock"),
        _ => None,
    }
}

/// The catalog entry for a workspace manifest: classified, hashed, and indexed,
/// but not structurally parsed (its `members` load into a [`Workspace`]).
fn workspace_entry(path: &Path, bytes: &[u8]) -> FileEntry {
    // The entry `.arsumbris/workspace.yaml` is a typed instance of
    // `au.engine.workspace`, its type stamped from its kind (no written `type:`),
    // a first-class node.
    let parse = crate::parse::parse_engine_schema_instance(
        path,
        bytes,
        "au.engine.workspace",
        Some(crate::engine_schema::BUILTIN_ENGINE_REPO),
    );
    FileEntry {
        kind: FileKind::Workspace,
        hash: Some(ContentHash::of(bytes)),
        parse: Arc::new(parse),
        line_index: Some(Arc::new(LineIndex::new(bytes))),
        byte_len: Some(bytes.len()),
    }
}

/// The cross-repo consistency warnings, each advisory and carrying a fix.
///
/// - `peer-unmounted`: a declared peer (a plain `dep`) resolves via none of the
///   tiers, a legitimate state, surfaced so the user can register a path.
/// - `edit-member-unmounted` / `discover-member-unmounted`: a declared `edit` /
///   `discover` member resolves to nothing via any tier, role-keyed.
///
/// The workspace-scoped checks (the member-state taxonomy) run over `workspaces`,
/// which holds the single entry workspace (the entry repo's
/// `.arsumbris/workspace.yaml` composition, else the entry repo plus its `deps`).
fn consistency_diagnostics(
    repos: &RepoMap,
    workspaces: &[crate::repo::Workspace],
    member_walk_errors: &BTreeMap<PathBuf, std::io::Error>,
    dep_notes: &[(crate::repo::RepoName, crate::repo::ResolveNote)],
    registry: &crate::repo::UserRegistry,
    cache_root: Option<&std::path::Path>,
) -> Vec<Diagnostic> {
    use au_diagnostics::{ByteRange, Severity, Span, SuggestedFix};

    let warn = |code, span: Span, message: String, fix: String| Diagnostic {
        code,
        severity: Severity::Warning,
        span,
        message,
        related: Vec::new(),
        fix: Some(SuggestedFix { description: fix }),
    };
    let err = |code, span: Span, message: String, fix: String| Diagnostic {
        code,
        severity: Severity::Error,
        span,
        message,
        related: Vec::new(),
        fix: Some(SuggestedFix { description: fix }),
    };
    let hint = |code, span: Span, message: String, fix: String| Diagnostic {
        code,
        severity: Severity::Hint,
        span,
        message,
        related: Vec::new(),
        fix: Some(SuggestedFix { description: fix }),
    };
    let mut diags = Vec::new();
    // A malformed dep `repo.yaml` can be reported by both the dep-notes and the
    // member-notes channel (the entry's deps resolve through each); dedup the
    // parse diagnostic by its file so one broken dep names itself once.
    let mut seen_malformed: BTreeSet<PathBuf> = BTreeSet::new();

    // Notes from dep resolution: a co-present sibling clash is duplicate-repo-name
    // (the dep stays unmounted), a registry path whose target declares another
    // name is dependency-identity-conflict.
    for (repo_name, note) in dep_notes {
        let span = repos
            .by_name(repo_name.as_str())
            .map(|r| {
                Span::new(
                    r.root.join(".arsumbris").join("repo.yaml"),
                    ByteRange::new(0, 0),
                )
            })
            .unwrap_or_else(|| Span::new(PathBuf::from(repo_name.as_str()), ByteRange::new(0, 0)));
        match note {
            crate::repo::ResolveNote::DuplicateSibling { name, roots } => {
                let list = roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                diags.push(warn(
                    crate::repo::DUPLICATE_REPO_NAME,
                    span,
                    format!(
                        "dep '{}' matches more than one co-present sibling ({list}); resolution refuses to pick, it stays unmounted",
                        name.as_str()
                    ),
                    format!(
                        "give the siblings distinct names, or register a path for '{}'",
                        name.as_str()
                    ),
                ));
            }
            crate::repo::ResolveNote::IdentityConflict { key, declared } => {
                diags.push(err(
                    crate::repo::DEPENDENCY_IDENTITY_CONFLICT,
                    span,
                    format!(
                        "dep '{}' resolves via the registry to a repo that declares '{}'; a registry path must match its key",
                        key.as_str(),
                        declared.as_str()
                    ),
                    format!("fix the registry path for '{}', or rename the repo", key.as_str()),
                ));
            }
            crate::repo::ResolveNote::PathOverridden { .. } => {}
            // Not produced on the dep-resolution path (the reserved-root exclusion
            // is in the workspace member assembly), exhaustiveness only.
            crate::repo::ResolveNote::ReservedRoot => {}
            crate::repo::ResolveNote::MalformedRepoYaml(d) => {
                if seen_malformed.insert(d.span.file.clone()) {
                    diags.push(d.clone());
                }
            }
        }
    }

    // dependency-identity-conflict (remote): the canonical remote is the OWNER's
    // own repo.yaml remote. A depender's `deps[].remote` assertion, or a registry
    // entry, that disagrees with it is a conflict, a real disagreement about the
    // owner rather than an artifact of which depender resolved first.
    for repo in repos.repos() {
        for dep in &repo.deps {
            let (Some(asserted), Some(owner)) =
                (dep.remote.as_deref(), repos.by_name(dep.name.as_str()))
            else {
                continue;
            };
            let Some(owner_remote) = owner.remote.as_deref() else {
                continue;
            };
            if asserted != owner_remote {
                diags.push(err(
                    crate::repo::DEPENDENCY_IDENTITY_CONFLICT,
                    Span::new(
                        repo.root.join(".arsumbris").join("repo.yaml"),
                        ByteRange::new(0, 0),
                    ),
                    format!(
                        "dep '{name}' asserts remote '{asserted}', but '{name}' declares '{owner_remote}'",
                        name = dep.name.as_str()
                    ),
                    "align the dep's remote with the owner's repo.yaml".to_string(),
                ));
            }
        }
    }
    for (name, loc) in registry {
        let (Some(reg_remote), Some(owner)) = (loc.remote.as_deref(), repos.by_name(name.as_str()))
        else {
            continue;
        };
        let Some(owner_remote) = owner.remote.as_deref() else {
            continue;
        };
        if reg_remote != owner_remote {
            diags.push(err(
                crate::repo::DEPENDENCY_IDENTITY_CONFLICT,
                Span::new(
                    owner.root.join(".arsumbris").join("repo.yaml"),
                    ByteRange::new(0, 0),
                ),
                format!(
                    "registry remote for '{}' is '{reg_remote}', but the repo declares '{owner_remote}'",
                    name.as_str()
                ),
                "align the registry entry with the owner's remote".to_string(),
            ));
        }
    }

    // peer-unmounted: a declared dep with no local resolution on this machine.
    // Mounted means it resolved to a path (peer_paths) or is a discovered repo
    // (a co-present sibling or a cache-mounted member).
    for repo in repos.repos() {
        for peer in &repo.deps {
            if repo.peer_paths.contains_key(&peer.name)
                || repos.by_name(peer.name.as_str()).is_some()
            {
                continue;
            }
            let registry = repo.root.join(".arsumbris").join("repo.yaml");
            diags.push(warn(
                crate::repo::PEER_UNMOUNTED,
                Span::new(registry, ByteRange::new(0, 0)),
                format!(
                    "peer '{}' is declared but not mounted on this machine",
                    peer.name.as_str()
                ),
                format!(
                    "register a path for '{}', or place it as a co-present sibling",
                    peer.name.as_str()
                ),
            ));
        }
    }

    // No out-of-scope-peer check: `assemble_members` grows the member set through
    // each member's `deps` closure, so every dep of a mounted member is itself a
    // member, and a referenced peer that is not a member cannot arise.

    // Member-resolution notes: a primary/member matching two co-present siblings
    // is duplicate-repo-name; a registry key mismatch is
    // dependency-identity-conflict. Anchored at the manifest.
    for ws in workspaces {
        for (_member, note) in &ws.member_notes {
            let span = Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0));
            match note {
                crate::repo::ResolveNote::DuplicateSibling { name, roots } => {
                    let list = roots
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    diags.push(warn(
                        crate::repo::DUPLICATE_REPO_NAME,
                        span,
                        format!(
                            "workspace member '{}' matches more than one co-present sibling ({list}); resolution refuses to pick, it stays unmounted",
                            name.as_str()
                        ),
                        format!(
                            "give the siblings distinct names, or register a path for '{}'",
                            name.as_str()
                        ),
                    ));
                }
                crate::repo::ResolveNote::IdentityConflict { key, declared } => {
                    diags.push(err(
                        crate::repo::DEPENDENCY_IDENTITY_CONFLICT,
                        span,
                        format!(
                            "workspace member '{}' resolves via the registry to a repo that declares '{}'; a registry path must match its key",
                            key.as_str(),
                            declared.as_str()
                        ),
                        format!("fix the registry path for '{}', or rename the repo", key.as_str()),
                    ));
                }
                crate::repo::ResolveNote::PathOverridden { .. } => {}
                // Rendered in the member-outcome pass below (so it suppresses the
                // generic unmounted signal), not here.
                crate::repo::ResolveNote::ReservedRoot => {}
                crate::repo::ResolveNote::MalformedRepoYaml(d) => {
                    if seen_malformed.insert(d.span.file.clone()) {
                        diags.push(d.clone());
                    }
                }
            }
        }
    }

    // A declared member is mounted (a repo was discovered at its resolved path)
    // or not. Not-mounted splits into unwalkable (a resolved path that could not
    // be walked) and unmounted (no resolution). name-mismatch, undeclared, and
    // path-collision can no longer arise: `resolve_repo_name` binds a member only
    // to a repo declaring its name (so no mismatch), reads a `repo.yaml` (so no
    // undeclared member), and gives one path per name (so no collision). Advisory,
    // the workspace still assembles without the member.
    for ws in workspaces {
        // A name in both `edit:` and `discover:` is a role conflict: a member has
        // one role per workspace. Error, but the editable role wins meanwhile
        // (see `assemble_workspace`), so the workspace still assembles.
        for name in &ws.edit {
            if ws.discover.contains(name) {
                diags.push(err(
                    crate::repo::WORKSPACE_MEMBER_ROLE_CONFLICT,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace member '{}' appears in both 'edit' and 'discover'; a member has one role per workspace",
                        name.as_str()
                    ),
                    format!(
                        "keep '{}' in 'edit' (editable) or 'discover' (consumed), not both",
                        name.as_str()
                    ),
                ));
            }
        }
        // A `discover` name that also resolves as a `dep` (subsumed to the higher
        // role during assembly) is a redundant listing: the dep already mounts and
        // pins it, and lets its types be crossed. A hint, not a conflict.
        for name in &ws.discover {
            if ws.member_roles.get(name) == Some(&MemberRole::Dep) {
                diags.push(hint(
                    crate::repo::DISCOVER_MEMBER_IS_A_DEPENDENCY,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace member '{}' is listed in 'discover' but is also a declared dependency; the 'discover' listing is redundant",
                        name.as_str()
                    ),
                    format!("drop '{}' from 'discover'; its dep already mounts and pins it", name.as_str()),
                ));
            }
        }
        // A `disabled:` overlay silences a DECLARED member, so a disabled name must
        // appear in `edit:` or `discover:`. A name in neither is a manifest typo:
        // it does nothing, so surface it rather than let it be silently inert. A
        // disabled name that DOES match a declared member is intentional and fires
        // nothing (the member is excluded from the mount set with no diagnostic).
        for name in &ws.disabled {
            if !ws.edit.contains(name) && !ws.discover.contains(name) {
                diags.push(warn(
                    crate::repo::DISABLED_MEMBER_NOT_DECLARED,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace 'disabled' names '{}', which is not a declared member; a disabled name must appear in 'edit' or 'discover'",
                        name.as_str()
                    ),
                    format!(
                        "add '{}' to 'edit' or 'discover', or remove it from 'disabled'",
                        name.as_str()
                    ),
                ));
            }
        }
        // A folder-repo `.arsumbris/workspace.yaml` must list its containing repo
        // in `edit:` (the entry is a live working tree at HEAD, so it belongs
        // there). An omission reads as excluding the file's own repo, an error, so
        // the file is self-complete. Fires only for a folder-repo entry, where the
        // containing repo's identity is known (its `repo.yaml` sits beside this
        // file).
        if is_folder_repo_manifest(&ws.manifest_path) {
            if let Some(entry_repo) = ws
                .manifest_path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|root| repos.repos().iter().find(|r| r.root.as_path() == root))
            {
                if !ws.edit.contains(&entry_repo.name) {
                    diags.push(err(
                        crate::repo::WORKSPACE_OMITS_CONTAINING_REPO,
                        Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                        format!(
                            "workspace.yaml does not list its containing repo '{}' in 'edit'; the entry repo is a live working tree and belongs in 'edit'",
                            entry_repo.name.as_str()
                        ),
                        format!("add '{}' to the 'edit' list", entry_repo.name.as_str()),
                    ));
                }
            }
        }
        for member in &ws.members {
            // A member resolving to the reserved device root `~/.arsumbris` is
            // excluded from the mount set (its `.arsumbris/` IS the device area, so
            // walking it would treat the whole home directory as content). Specific,
            // and takes precedence over the generic unmounted signal below.
            if ws.member_notes.iter().any(|(n, note)| {
                n == &member.name && matches!(note, crate::repo::ResolveNote::ReservedRoot)
            }) {
                diags.push(warn(
                    crate::repo::MEMBER_AT_RESERVED_ROOT,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace member '{}' resolves to the reserved device root ~/.arsumbris; it is excluded, the workspace opens over the rest",
                        member.name.as_str()
                    ),
                    format!(
                        "point '{}' at a directory other than your home directory",
                        member.name.as_str()
                    ),
                ));
                continue;
            }
            // Member outcomes are role-keyed. An editable member (the entry or an
            // `edit` member) is an authoring ROOT: unmounted is `edit-member-unmounted`
            // (degraded open), cache-only is `edit-member-read-only`. A `discover`
            // member unmounted is `discover-member-unmounted` (cache-only is the
            // expected pinned state, no diagnostic). A `dep` is owned by the
            // `peer-unmounted` loop above, so it emits nothing here (no double-signal).
            let role = ws.member_roles.get(&member.name).copied();
            let editable = role.is_some_and(|r| r.editable());
            let mounted_root = ws
                .member_paths
                .get(&member.name)
                .filter(|path| repos.repos().iter().any(|r| &r.root == *path));
            if let Some(root) = mounted_root {
                let read_only = cache_root.is_some_and(|c| root.starts_with(c));
                if editable && read_only {
                    diags.push(warn(
                        crate::repo::EDIT_MEMBER_READ_ONLY,
                        Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                        format!(
                            "workspace edit member '{}' resolves only to the read-only cache; an edit member is meant to be editable",
                            member.name.as_str()
                        ),
                        format!(
                            "register a local path for '{}', or place it as a co-present sibling",
                            member.name.as_str()
                        ),
                    ));
                }
                continue;
            }
            // A resolved path present but unwalkable (a file, or unreadable) is
            // distinct from no resolution. `NotFound` reads as unmounted.
            let unwalkable = ws
                .member_paths
                .get(&member.name)
                .and_then(|path| member_walk_errors.get(path))
                .filter(|err| err.kind() != std::io::ErrorKind::NotFound);
            if let Some(err) = unwalkable {
                diags.push(warn(
                    crate::repo::WORKSPACE_MEMBER_UNWALKABLE,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace member '{}' has a path that cannot be walked: {err}",
                        member.name.as_str()
                    ),
                    format!(
                        "point '{}' at a readable directory, or place it as a co-present sibling",
                        member.name.as_str()
                    ),
                ));
            } else if matches!(role, Some(MemberRole::Entry) | Some(MemberRole::Edit)) {
                diags.push(warn(
                    crate::repo::EDIT_MEMBER_UNMOUNTED,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace edit member '{}' resolves to nothing; the workspace opens degraded over its present members",
                        member.name.as_str()
                    ),
                    format!(
                        "register a local path for '{}', or place it as a co-present sibling",
                        member.name.as_str()
                    ),
                ));
            } else if matches!(role, Some(MemberRole::Discover)) {
                diags.push(warn(
                    crate::repo::DISCOVER_MEMBER_UNMOUNTED,
                    Span::new(ws.manifest_path.clone(), ByteRange::new(0, 0)),
                    format!(
                        "workspace discover member '{}' is declared but not mounted on this machine",
                        member.name.as_str()
                    ),
                    format!(
                        "register a path for '{}', place it as a co-present sibling, or run resolve to fetch it",
                        member.name.as_str()
                    ),
                ));
            }
        }
    }

    // No path-collision check: a dep resolves via the order (sibling -> registry
    // -> cache), and a registry path's target declares exactly one name, so a
    // second dep pointing at it is a dependency-identity-conflict. Two deps can
    // never land on one path.

    diags
}

/// A `.type.yaml` type-def file that does not live under a `type/` directory.
/// Advisory `warning`: the `.type.yaml` suffix is the classification marker, so
/// the file is a valid type-def regardless; the `type/` directory is an
/// authoring convention, and a type-def outside it is surfaced, never blocked.
/// See [[spec - diagnostic codes::au-type-system^type-def-outside-type-dir]].
pub const TYPE_DEF_OUTSIDE_TYPE_DIR: DiagnosticCode =
    DiagnosticCode::from_static("type-def-outside-type-dir");

/// Emit `type-def-outside-type-dir` for a type-def file that sits under no
/// `type/` directory. `root` is the file's member root; the check is over the
/// path RELATIVE to it, so a `type` component in the root's own prefix does not
/// falsely satisfy the convention. Returns `None` for a well-placed file.
fn type_def_outside_type_dir_diag(
    file: &std::path::Path,
    root: &std::path::Path,
) -> Option<Diagnostic> {
    let rel = file.strip_prefix(root).unwrap_or(file);
    if au_parser::is_under_type_dir(rel) {
        return None;
    }
    Some(Diagnostic {
        code: TYPE_DEF_OUTSIDE_TYPE_DIR,
        severity: Severity::Warning,
        span: Span::new(file.to_path_buf(), ByteRange::new(0, 0)),
        message:
            "type-def does not live under a `type/` directory; by convention every type-def sits under the repo's `type/`"
                .to_string(),
        related: vec![],
        fix: Some(SuggestedFix {
            description: "move this file under the repo's `type/` directory".to_string(),
        }),
    })
}

/// The declared name of the repo a path belongs to. Falls back to the root
/// repo's name, so a path always routes to a graph. `None` only for a
/// repo-less workspace, which discovery never produces.
fn repo_of(repos: &RepoMap, path: &std::path::Path) -> RepoName {
    repos
        .repo_of(path)
        .map(|r| r.name.clone())
        .or_else(|| repos.root().map(|r| r.name.clone()))
        .unwrap_or_else(|| RepoName(String::new()))
}

/// The repo name a path belongs to, `None` for a path under no repo.
fn repo_of_path(repos: &RepoMap, path: &std::path::Path) -> Option<RepoName> {
    repos.repo_of(path).map(|r| r.name.clone())
}

/// The type graph a path resolves against during the build: its repo's graph.
fn repo_graph_for<'a>(
    graphs: &'a RepoGraphs,
    repos: &RepoMap,
    path: &std::path::Path,
) -> &'a TypeGraph {
    match repos.repo_of(path) {
        Some(r) => graphs.of(&r.name),
        None => graphs.empty(),
    }
}

/// The owned per-repo maps a [`ValidateContext`] borrows: instance claims,
/// markdown bodies, and addressable inline records.
///
/// Assembled from the catalog and graph, so the build pass and read-time value
/// validation share one construction rather than duplicating the three loops.
pub(crate) struct ContextMaps {
    claims_by_path: BTreeMap<PathBuf, Vec<TypeName>>,
    body_sources: BTreeMap<PathBuf, String>,
    record_targets: BTreeMap<PathBuf, au_core::RecordTargets>,
}

impl ContextMaps {
    pub(crate) fn assemble(
        catalog: &OrdMap<PathBuf, FileEntry>,
        graphs: &RepoGraphs,
        repos: &RepoMap,
        resolution_graphs: &crate::ir::ResolutionGraphs,
    ) -> Self {
        // Claims indexed by path, so reference checks see the whole instance set.
        let mut claims_by_path: BTreeMap<PathBuf, Vec<TypeName>> = BTreeMap::new();
        for entry in catalog.values() {
            if let FileParse::Instance {
                instance: Some(inst),
                ..
            } = entry.parse.as_ref()
            {
                let claims = inst.type_claim.iter().map(|c| c.name.clone()).collect();
                claims_by_path.insert(inst.source_path.clone(), claims);
            }
        }

        // Markdown bodies only, for cross-file `^block-id` resolution.
        let body_sources = catalog
            .iter()
            .filter_map(|(p, e)| match e.parse.as_ref() {
                FileParse::Instance {
                    instance: Some(_),
                    body,
                    is_markdown: true,
                    ..
                } => Some((p.clone(), body.clone())),
                _ => None,
            })
            .collect();

        // Addressable inline records per file ([[type block-id::au-type-system]]): `^:` id →
        // effective claim names, so `[[file^id]]` / `[[^id]]` references can
        // resolve to records and type-check against their claims. Each file's
        // records resolve against its own repo's graph.
        let record_targets = catalog
            .values()
            .filter_map(|e| match e.parse.as_ref() {
                FileParse::Instance {
                    instance: Some(inst),
                    ..
                } => Some((
                    inst.source_path.clone(),
                    crate::resolution_build::record_targets_of(
                        graphs,
                        repos,
                        resolution_graphs,
                        &inst.source_path,
                        inst,
                    ),
                )),
                _ => None,
            })
            .collect();

        Self {
            claims_by_path,
            body_sources,
            record_targets,
        }
    }

    /// Borrow the maps into a [`ValidateContext`] against a graph and index.
    ///
    /// `cross_repo` is the resolver for `[[name::repo]]` references; instance
    /// validation passes one so cross-boundary typed refs are type-checked,
    /// meta and read-time value validation pass `None` (repo-local only).
    pub(crate) fn context<'a>(
        &'a self,
        graph: &'a TypeGraph,
        repo_index: &'a RepoIndex,
        cross_repo: Option<&'a dyn au_core::CrossRepoResolver>,
        resolution: Option<&'a au_core::ResolutionGraph>,
    ) -> ValidateContext<'a> {
        ValidateContext {
            graph,
            repo_index,
            ref_data: self,
            cross_repo,
            resolution,
            meta_marker: Some(au_core::MetaMarker {
                name: crate::engine_schema::ENGINE_META_TYPE,
                repo: crate::engine_schema::BUILTIN_ENGINE_REPO,
            }),
        }
    }
}

/// The whole-knowledge-base build resolves a target's claims, body, and records from the
/// pre-assembled maps. The incremental path supplies its own [`au_core::RefData`]
/// backed by the held catalog, so it looks up only the targets it references.
impl au_core::RefData for ContextMaps {
    fn claims(&self, path: &std::path::Path) -> Option<std::borrow::Cow<'_, [TypeName]>> {
        self.claims_by_path
            .get(path)
            .map(|v| std::borrow::Cow::Borrowed(v.as_slice()))
    }
    fn body(&self, path: &std::path::Path) -> Option<&str> {
        self.body_sources.get(path).map(String::as_str)
    }
    fn record_targets(
        &self,
        path: &std::path::Path,
    ) -> Option<std::borrow::Cow<'_, au_core::RecordTargets>> {
        self.record_targets
            .get(path)
            .map(std::borrow::Cow::Borrowed)
    }
}

/// A content fingerprint of a knowledge base: the file set plus a content hash per
/// readable text file. Cheap, walk and hash, no parse.
///
/// Comparing two fingerprints decides whether a rebuild is needed: an
/// unchanged fingerprint means no file the build reads changed, so the held
/// analysis still holds. Asset files contribute presence only (the build does
/// not read their bytes, so their content cannot alter the analysis); their
/// addition or removal still changes the key set.
pub(crate) type Fingerprint = BTreeMap<PathBuf, Option<ContentHash>>;

/// Whether the build reads this path's BYTES.
///
/// A type-def, a workspace manifest, and an instance candidate are decoded. An
/// asset is not: the walker keeps it in the `RepoIndex` so a `file*` reference
/// resolves to it, and the parser never touches its bytes, which is why a PDF or
/// a PNG never trips `repo-file-not-utf8`.
///
/// So a non-read path carries NO content hash in the [`Fingerprint`], and its
/// content cannot change anything the engine holds. Its PRESENCE still can, it
/// is an index member.
///
/// Shared with the scoped no-op gate in `engine.rs`, which must ask exactly the
/// question the fingerprint asked. When the two disagree the errors are silent
/// and opposite: a gate that hashes bytes the fingerprint left as `None` reads
/// every asset write as a change, and reads every asset DELETE as no change,
/// since an absent file and an unread one both hash to `None`.
pub(crate) fn build_reads_content(path: &Path) -> bool {
    classify_by_path(path) == Some(FileKind::TypeDef) || is_instance_candidate_path(path)
}

/// Compute the [`Fingerprint`] of a knowledge base.
///
/// Spans the same roots `build` reads: the entry repo alone in an entry-only
/// workspace, every mounted member in a composed one, plus the engine-schema files
/// the walker never sees (registries and the entry manifest).
/// Spanned: this walks EVERY member root and reads and hashes every file it
/// keeps, a whole-knowledge-base pass that runs OUTSIDE `build_reusing` and so
/// is invisible in the build's own span tree. The full-rebuild path calls it
/// twice, once to snapshot and once to confirm disk held still.
#[tracing::instrument(skip_all, fields(files = tracing::field::Empty))]
pub(crate) fn fingerprint(
    dir: &std::path::Path,
    config: &crate::repo::ConfigSource,
    fs: &impl FileSystem,
) -> std::io::Result<Fingerprint> {
    // The member roots depend on the per-user registry (scattered members),
    // read through the injected config dir so a test stays hermetic.
    let registry = crate::repo::load_user_registry(config, fs);
    let (roots, entry_manifest) = assembly_roots(dir, &registry, fs);
    let mut fp = Fingerprint::new();
    let mut all_files: Vec<PathBuf> = Vec::new();
    let mut all_markers: Vec<PathBuf> = Vec::new();
    for root in &roots {
        let Ok(walk) = fs.walk_files(root, &au_parser::WalkFilter::default_excludes(root)) else {
            continue;
        };
        all_markers.extend(walk.repo_markers);
        let files = walk.files;
        for file in &files {
            let hash = if build_reads_content(file) {
                // The read cap must hold HERE too, not only at `acquire_parse`:
                // this pass runs FIRST, so an unguarded read would pull an
                // over-cap file whole into memory (the OOM the cap exists to
                // prevent) before the cap ever runs. For an over-cap file, hash
                // its SIZE instead of its content, so no whole-file read happens
                // yet the hash still flips on an over-cap→over-cap size change
                // (refreshing the file-too-large size the diagnostic reports) and
                // on any under/over-cap transition, keeping the file's dirty
                // tracking correct without the allocation.
                match over_read_cap(fs, file) {
                    Some(size) => Some(ContentHash::of(&size.to_le_bytes())),
                    None => fs.read_file(file).ok().map(|b| ContentHash::of(&b)),
                }
            } else {
                None
            };
            fp.insert(file.clone(), hash);
        }
        all_files.extend(files);
    }
    // Repo-registry files are engine-schema substrate the walker ignores, so a
    // registry edit must be added explicitly.
    for (path, bytes) in crate::repo::registry_files(&roots, &all_files, &all_markers, fs) {
        fp.insert(path.clone(), Some(ContentHash::of(&bytes)));
        if let Some(d) = path.parent() {
            // The `.arsumbris/repo.lock` sits beside the registry, also engine
            // schema the walker ignores and catalogued as a typed node; a lock
            // edit must trigger a rebuild so its node view (and validation)
            // refreshes.
            let lock = d.join("repo.lock");
            if let Ok(b) = fs.read_file(&lock) {
                fp.insert(lock, Some(ContentHash::of(&b)));
            }
        }
    }
    // The entry manifest (assembly mode) lives in `dir`, outside every member
    // root, so it is added explicitly. An edit to it reshapes the member set and
    // must trigger a rebuild.
    if let Some(m) = &entry_manifest {
        if let Ok(b) = fs.read_file(m) {
            fp.insert(m.clone(), Some(ContentHash::of(&b)));
        }
    }
    tracing::Span::current().record("files", fp.len());
    Ok(fp)
}

/// Reuse a file's parse and line index from the prior layer when its content
/// hash is unchanged, else parse fresh.
///
/// `parse_file` is pure over `(path, bytes)`, so a path-and-hash match in the
/// prior layer yields the identical parse without re-running it.
fn reuse_or_parse(
    prior: &ParseLayer,
    file: &Path,
    hash: ContentHash,
    bytes: &[u8],
) -> (Arc<FileParse>, Option<Arc<LineIndex>>, usize) {
    if let Some(reused) = prior.reuse(file, hash) {
        return reused;
    }
    let parse = Arc::new(parse_file(file, bytes));
    let line_index = Some(Arc::new(LineIndex::new(bytes)));
    (parse, line_index, bytes.len())
}

/// The per-file read cap. A content file larger than this is skipped, not read,
/// so an oversized note or export dropped into the vault surfaces as
/// [`au_parser::FILE_TOO_LARGE`] rather than an OOM on the read itself.
///
/// A generous FIXED ceiling: no real note or type-def approaches 32 MiB, and it
/// bounds per-file memory. Deliberately not yet a config knob — a runtime-
/// changeable cap would have to re-evaluate which files cross it on every
/// change, an incremental-invalidation the fixed value avoids entirely (the only
/// way a file crosses a fixed cap is a byte-size change, which already dirties
/// it). Making it per-repo adjustable via the scoped config channel is the
/// tracked refinement. Assets are never read, so their size is never capped.
pub(crate) const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// `Some(size)` when `file` exceeds [`MAX_READ_BYTES`], probed WITHOUT reading
/// it (via [`au_parser::FileSystem::file_len`]); `None` when it is within cap or
/// its size could not be probed.
///
/// A size-probe failure falls through to `None` on purpose: the ordinary read
/// then runs and its own failure path diagnoses a truly broken file, so a
/// stat error never masquerades as over-cap. Shared by the full build and the
/// incremental recompute so both skip identically, byte-identical per the
/// parity oracle.
pub(crate) fn over_read_cap(fs: &impl FileSystem, file: &Path) -> Option<u64> {
    match fs.file_len(file) {
        Ok(len) if len > MAX_READ_BYTES => Some(len),
        _ => None,
    }
}

/// The outcome of [`acquire_parse`]: a parse (reused or freshly read), or a
/// skip because the file is over the read cap.
enum AcquireOutcome {
    Parsed {
        parse: Arc<FileParse>,
        line_index: Option<Arc<LineIndex>>,
        hash: ContentHash,
        byte_len: usize,
    },
    /// The file exceeds [`MAX_READ_BYTES`]; it was NOT read. The caller emits
    /// [`file_too_large_diag`] and catalogs an unread entry.
    TooLarge { size: u64 },
}

/// Acquire a file's parse, line index, and content hash, reading its bytes only
/// when it cannot reuse and is within the read cap.
///
/// Read-skip: when `known_hashes` already carries this file's hash and the prior
/// layer holds that exact hash, the parse and line index are reused with NO I/O
/// at all — not even a size stat. Only when a read would actually happen is the
/// [`MAX_READ_BYTES`] cap enforced, via a cheap [`au_parser::FileSystem::file_len`]
/// stat, so an over-cap file is never pulled whole into memory and an unchanged
/// under-cap file pays nothing. A read failure propagates for the caller to
/// diagnose.
fn acquire_parse(
    prior: &ParseLayer,
    known_hashes: &Fingerprint,
    fs: &impl FileSystem,
    file: &Path,
) -> std::io::Result<AcquireOutcome> {
    if let Some(Some(hash)) = known_hashes.get(file) {
        if let Some((parse, line_index, byte_len)) = prior.reuse(file, *hash) {
            return Ok(AcquireOutcome::Parsed {
                parse,
                line_index,
                hash: *hash,
                byte_len,
            });
        }
    }
    // About to read: enforce the cap first, after the reuse check, so a reused
    // file never stats and an over-cap file never allocates.
    if let Some(size) = over_read_cap(fs, file) {
        return Ok(AcquireOutcome::TooLarge { size });
    }
    let bytes = fs.read_file(file)?;
    let hash = ContentHash::of(&bytes);
    let (parse, line_index, byte_len) = reuse_or_parse(prior, file, hash, &bytes);
    Ok(AcquireOutcome::Parsed {
        parse,
        line_index,
        hash,
        byte_len,
    })
}

/// A catalog entry for a file that was classified but not read or parsed.
///
/// `pub(crate)` for the incremental path, which must produce byte-identical
/// entries for an added asset: the build's own counterpart is
/// [`catalog_remaining_files`].
pub(crate) fn unread_entry(kind: FileKind) -> FileEntry {
    FileEntry {
        kind,
        hash: None,
        parse: Arc::new(FileParse::Unparsed {
            diagnostics: Vec::new(),
        }),
        line_index: None,
        byte_len: None,
    }
}

/// Attach the line/column rendering to every diagnostic span whose file the
/// build read. Spans into unread files (assets, unreadable paths) stay
/// byte-only.
fn attach_line_cols(diagnostics: &mut [Diagnostic], catalog: &OrdMap<PathBuf, FileEntry>) {
    let index_of = |file: &PathBuf| catalog.get(file).and_then(|e| e.line_index.as_deref());
    for d in diagnostics.iter_mut() {
        if let Some(idx) = index_of(&d.span.file) {
            d.span.attach_line_col(idx);
        }
        for r in d.related.iter_mut() {
            if let Some(idx) = index_of(&r.file) {
                r.attach_line_col(idx);
            }
        }
    }
}

/// Catalog every walked file not yet recorded, as an unread, unclassified
/// entry. These are assets (binaries) and notes the passes skipped; they are
/// known by path but not read, so they carry no hash and an empty parse.
fn catalog_remaining_files(files: &[PathBuf], catalog: &mut BTreeMap<PathBuf, FileEntry>) {
    for file in files {
        catalog
            .entry(file.clone())
            .or_insert_with(|| unread_entry(FileKind::Unclassified));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// A folder-repo workspace in memory: the entry `/ws` is a content-free repo
    /// whose `.arsumbris/workspace.yaml` composes the members `base` and `app`,
    /// each a subdirectory repo declaring its own name.
    fn scattered_fs() -> MemoryFileSystem {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - base\n  - app\n".to_vec(),
        );
        fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
        fs.insert(
            "/ws/base/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/ws/app/.arsumbris/repo.yaml", b"name: app\n".to_vec());
        fs.insert(
            "/ws/app/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs
    }

    /// Wraps a `MemoryFileSystem` but fails the walk of one root. The in-memory
    /// fs never fails a walk, so this models a member path that resolves yet
    /// cannot be walked (it is a file, or unreadable).
    struct FailWalkFs {
        inner: MemoryFileSystem,
        fail: PathBuf,
        kind: std::io::ErrorKind,
    }

    impl au_parser::FileSystem for FailWalkFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.inner.read_file(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.inner.is_file(path)
        }
        fn walk_files(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<au_parser::Walk> {
            if root == self.fail {
                return Err(std::io::Error::new(self.kind, "member root walk failed"));
            }
            self.inner.walk_files(root, filter)
        }
        fn walk_scope_boundaries(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<(au_parser::ScopeBoundaries, Vec<au_parser::WalkError>)> {
            if root == self.fail {
                return Err(std::io::Error::new(self.kind, "member root walk failed"));
            }
            self.inner.walk_scope_boundaries(root, filter)
        }
    }

    /// Wraps a `MemoryFileSystem` but fails `read_file` of one path with a
    /// non-`NotFound` error. The in-memory fs only ever returns `NotFound` for a
    /// missing key, so this models a file that EXISTS yet is unreadable (a
    /// permission or transient I/O error), which `NotFound`-vs-other must not
    /// conflate with absence.
    struct FailReadFs {
        inner: MemoryFileSystem,
        fail: PathBuf,
    }

    impl au_parser::FileSystem for FailReadFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            if path == self.fail {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "unreadable",
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
    fn a_present_but_unreadable_workspace_manifest_surfaces_a_read_error() {
        // `/ws` is a folder-repo whose `.arsumbris/workspace.yaml` EXISTS but is
        // unreadable. The build degrades to the entry-only workspace, but the read
        // error must SURFACE as a diagnostic, never a silent drop of the whole
        // composition. (Absence, by contrast, is a legitimate entry-only workspace
        // with no diagnostic.)
        let fs = FailReadFs {
            inner: scattered_fs(),
            fail: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
        };
        let kb = build(Path::new("/ws"), &fs).unwrap();
        assert!(
            kb.diagnostics()
                .any(|d| d.code == au_parser::REPO_FILE_READ_ERROR),
            "an unreadable workspace.yaml must surface a read error, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// Wraps a `MemoryFileSystem` and reports ONE path as over the read cap via
    /// an inflated `file_len`. The cap logic keys on `file_len`, so an inflated
    /// size is exactly the input under test — this drives the cap branch without
    /// a 32 MiB fixture, and without a config knob to lower the cap.
    ///
    /// `read_file` of the over-cap path PANICS: a real over-cap file is huge, so
    /// no code path may ever read it whole. Every read boundary (the `fingerprint`
    /// pass, `acquire_parse`, `recompute_dirty`) must skip it via the size probe,
    /// so this double turns each cap test into a structural guard that the
    /// OOM-avoidance property holds, not just the skip/diagnose decision.
    struct OversizedFs {
        inner: MemoryFileSystem,
        big: PathBuf,
    }

    impl au_parser::FileSystem for OversizedFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            assert_ne!(
                path,
                self.big.as_path(),
                "read_file of an over-cap file: some read boundary skipped the cap \
                 and would OOM on a real large file"
            );
            self.inner.read_file(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.inner.is_file(path)
        }
        fn file_len(&self, path: &Path) -> std::io::Result<u64> {
            if path == self.big {
                return Ok(super::MAX_READ_BYTES + 1);
            }
            self.inner.file_len(path)
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
    fn an_over_cap_instance_is_skipped_with_a_file_too_large_error() {
        let mut inner = MemoryFileSystem::new();
        inner.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        inner.insert("/v/type/note.type.yaml", b"fields: {}\n".to_vec());
        inner.insert("/v/big.md", b"---\ntype: note\n---\n".to_vec());
        inner.insert("/v/small.md", b"---\ntype: note\n---\n".to_vec());
        let fs = OversizedFs {
            inner,
            big: PathBuf::from("/v/big.md"),
        };
        let kb = build(Path::new("/v"), &fs).unwrap();

        // The over-cap file surfaces a `file-too-large` Error at its own span.
        let big_diag = kb
            .diagnostics()
            .find(|d| d.code == au_parser::FILE_TOO_LARGE)
            .expect("expected a file-too-large diagnostic");
        assert_eq!(big_diag.severity, au_diagnostics::Severity::Error);
        assert_eq!(big_diag.span.file, PathBuf::from("/v/big.md"));

        // It is catalogued but UNREAD: present so `file*` / navigation resolve,
        // yet carrying no hash, no parse, no resolved instance.
        let entry = kb
            .catalog
            .get(Path::new("/v/big.md"))
            .expect("over-cap file stays catalogued");
        assert!(entry.hash.is_none(), "an over-cap file carries no hash");
        assert!(
            !kb.instances.contains_key(Path::new("/v/big.md")),
            "an over-cap file has no resolved instance"
        );

        // The under-cap sibling is unaffected: read, parsed, validated.
        assert!(
            kb.instances.contains_key(Path::new("/v/small.md")),
            "the under-cap instance still validates"
        );
    }

    #[test]
    fn an_over_cap_type_def_is_skipped_and_aborts_its_repos_validation() {
        let mut inner = MemoryFileSystem::new();
        inner.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        inner.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        inner.insert("/v/n.md", b"---\ntype: note\ntitle: hi\n---\n".to_vec());
        let fs = OversizedFs {
            inner,
            big: PathBuf::from("/v/type/note.type.yaml"),
        };
        let kb = build(Path::new("/v"), &fs).unwrap();

        assert!(
            kb.diagnostics().any(|d| d.code == au_parser::FILE_TOO_LARGE
                && d.span.file == PathBuf::from("/v/type/note.type.yaml")),
            "an over-cap type-def surfaces a file-too-large diagnostic"
        );
        // An unread type-def is a vocabulary error, so its repo aborts its own
        // instance validation, exactly like an unreadable type-def.
        assert!(
            kb.root_outcome().aborted(),
            "an over-cap type-def aborts its repo's validation"
        );
    }

    #[test]
    fn an_over_cap_dirty_file_falls_back_to_a_full_build() {
        let mut inner = MemoryFileSystem::new();
        inner.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        inner.insert("/v/type/note.type.yaml", b"fields: {}\n".to_vec());
        inner.insert("/v/big.md", b"---\ntype: note\n---\n".to_vec());
        let fs = OversizedFs {
            inner,
            big: PathBuf::from("/v/big.md"),
        };
        let held = build(Path::new("/v"), &fs).unwrap();

        // Editing the over-cap file: the incremental path refuses (returns None),
        // so the full build re-derives the unread entry + diagnostic identically.
        let dirty: std::collections::BTreeSet<PathBuf> =
            std::iter::once(PathBuf::from("/v/big.md")).collect();
        assert!(
            crate::incremental::recompute_dirty(&held, &dirty, &fs).is_none(),
            "an over-cap dirty file must fall back to a full build"
        );
    }

    #[test]
    fn an_over_cap_file_coexists_through_an_incremental_edit_of_another() {
        let mut before = MemoryFileSystem::new();
        before.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        before.insert("/v/type/note.type.yaml", b"fields: {}\n".to_vec());
        before.insert("/v/big.md", b"---\ntype: note\n---\n".to_vec());
        before.insert("/v/y.md", b"---\ntype: note\n---\n".to_vec());

        // Edit only y.md; big.md stays over-cap and untouched.
        let mut after_inner = before.clone();
        after_inner.insert("/v/y.md", b"---\ntype: note\nextra: 1\n---\n".to_vec());

        let fs_before = OversizedFs {
            inner: before,
            big: PathBuf::from("/v/big.md"),
        };
        let fs_after = OversizedFs {
            inner: after_inner,
            big: PathBuf::from("/v/big.md"),
        };

        let held = build(Path::new("/v"), &fs_before).unwrap();
        let dirty: std::collections::BTreeSet<PathBuf> =
            std::iter::once(PathBuf::from("/v/y.md")).collect();
        let rc = crate::incremental::recompute_dirty(&held, &dirty, &fs_after)
            .expect("y.md is a normal instance edit");
        let incremental = crate::incremental::apply_recompute(&held, rc);
        let scratch = build(Path::new("/v"), &fs_after).unwrap();
        // The over-cap file, untouched, stays a byte-identical unread entry
        // through an incremental edit of a different file.
        crate::ir::assert_kb_parity(&incremental, &scratch);
    }

    #[test]
    fn auignore_excludes_a_subtree_from_the_catalog() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/keep.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/docs/guide.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/.arsumbris/.auignore", b"docs/\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            kb.catalog.contains_key(Path::new("/v/keep.md")),
            "kept file present"
        );
        assert!(
            !kb.catalog.contains_key(Path::new("/v/docs/guide.md")),
            "docs/ excluded from the catalog"
        );
    }

    #[test]
    fn auignore_allowlist_keeps_only_matched() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/keep-out.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/docs/guide.md", b"---\ntype: note\n---\n".to_vec());
        // The gitignore allowlist idiom: ignore everything at root, re-include docs.
        fs.insert("/v/.arsumbris/.auignore", b"/*\n!/docs\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            kb.catalog.contains_key(Path::new("/v/docs/guide.md")),
            "allowlisted dir kept"
        );
        assert!(
            !kb.catalog.contains_key(Path::new("/v/keep-out.md")),
            "unlisted file dropped"
        );
    }

    #[test]
    fn auignore_negation_reincludes_a_default_exclude() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/target/out.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/.arsumbris/.auignore", b"!target\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            kb.catalog.contains_key(Path::new("/v/target/out.md")),
            "!target re-includes the default-excluded directory"
        );
    }

    #[test]
    fn auignore_that_excludes_everything_warns() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/only.md", b"---\ntype: note\n---\n".to_vec());
        // `*` matches every top-level entry, emptying the tree.
        fs.insert("/v/.arsumbris/.auignore", b"*\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            kb.diagnostics()
                .any(|d| d.code == au_parser::AUIGNORE_EMPTY_SCOPE),
            "expected auignore-empty-scope, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
        assert!(
            !kb.catalog.contains_key(Path::new("/v/only.md")),
            "the file is indeed scoped out"
        );
    }

    #[test]
    fn auignore_over_an_already_empty_tree_does_not_warn() {
        // The only content sits under a default-excluded directory, so the tree
        // is empty with or without the `.auignore`. The user's `docs/` pattern
        // is not what emptied it, so `auignore-empty-scope` must not fire.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/target/out.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/.arsumbris/.auignore", b"docs/\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            !kb.diagnostics()
                .any(|d| d.code == au_parser::AUIGNORE_EMPTY_SCOPE),
            "auignore did not empty the tree; no warning expected, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn malformed_auignore_diagnoses_and_falls_back() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert("/v/keep.md", b"---\ntype: note\n---\n".to_vec());
        // A lone backslash is not a valid glob.
        fs.insert("/v/.arsumbris/.auignore", b"\\\n".to_vec());
        let kb = build(Path::new("/v"), &fs).unwrap();
        assert!(
            kb.diagnostics()
                .any(|d| d.code == au_parser::AUIGNORE_LOAD_ERROR),
            "expected auignore-load-error, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
        // Fell back to the default excludes, so the knowledge base still walked.
        assert!(
            kb.catalog.contains_key(Path::new("/v/keep.md")),
            "fallback keeps walking"
        );
    }

    /// A folder-repo entry `/main` with a SCATTERED member `lib` at `/ext/lib`,
    /// OUTSIDE the entry tree, reachable only via the per-user registry. A build
    /// over `/main` walks `lib` from its own root, so a walk failure there is a
    /// genuine member-walk failure the entry walk never masks (a member nested
    /// under the entry would be covered by the entry repo's own walk).
    fn scattered_member_fs() -> (MemoryFileSystem, crate::repo::UserRegistry) {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/main/.arsumbris/repo.yaml", b"name: main\n".to_vec());
        fs.insert(
            "/main/.arsumbris/workspace.yaml",
            b"edit:\n  - main\n  - lib\n".to_vec(),
        );
        fs.insert("/main/m.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/ext/lib/.arsumbris/repo.yaml", b"name: lib\n".to_vec());
        fs.insert(
            "/ext/lib/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        let mut registry = crate::repo::UserRegistry::new();
        registry.insert(
            crate::repo::RepoName("lib".into()),
            crate::repo::RegistryLocation {
                remote: None,
                path: PathBuf::from("/ext/lib"),
            },
        );
        (fs, registry)
    }

    #[test]
    fn present_but_unwalkable_member_is_diagnosed_with_cause() {
        // `lib` resolves via the registry but its root cannot be walked (a file, or
        // unreadable): surfaced as unwalkable (with cause), not dropped, and the
        // entry repo still assembles.
        let (inner, registry) = scattered_member_fs();
        let fs = FailWalkFs {
            inner,
            fail: PathBuf::from("/ext/lib"),
            kind: std::io::ErrorKind::InvalidInput,
        };
        let kb = build_reusing(
            Path::new("/main"),
            &fs,
            &registry,
            &ParseLayer::default(),
            &Fingerprint::new(),
            None,
        )
        .unwrap();
        assert!(
            kb.diagnostics()
                .any(|d| d.code == crate::repo::WORKSPACE_MEMBER_UNWALKABLE),
            "expected workspace-member-unwalkable, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
        assert!(kb.catalog.contains_key(Path::new("/main/m.md")));
    }

    #[test]
    fn missing_member_path_stays_unmounted_not_unwalkable() {
        // A resolved path that does not exist keeps the intended unmounted reading,
        // not unwalkable. `lib` is an edit member, so its unmounted outcome
        // is edit-member-unmounted; the NotFound-vs-unwalkable distinction
        // this test guards is unchanged.
        let (inner, registry) = scattered_member_fs();
        let fs = FailWalkFs {
            inner,
            fail: PathBuf::from("/ext/lib"),
            kind: std::io::ErrorKind::NotFound,
        };
        let kb = build_reusing(
            Path::new("/main"),
            &fs,
            &registry,
            &ParseLayer::default(),
            &Fingerprint::new(),
            None,
        )
        .unwrap();
        assert!(
            kb.diagnostics()
                .any(|d| d.code == crate::repo::EDIT_MEMBER_UNMOUNTED),
            "expected edit-member-unmounted (lib is an unmounted edit member), got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
        assert!(
            !kb.diagnostics()
                .any(|d| d.code == crate::repo::WORKSPACE_MEMBER_UNWALKABLE),
            "should not be unwalkable, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_disabled_member_is_excluded_and_fires_no_unmounted() {
        // `base` is a declared edit member present on disk, but also listed in
        // `disabled:`. It is excluded before resolution: not mounted, contributes
        // no files or types, and fires no unmounted diagnostic (found-but-off, not
        // declared-but-missing). `app` stays mounted, so the workspace still
        // composes over the rest.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - base\n  - app\ndisabled:\n  - base\n".to_vec(),
        );
        fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
        fs.insert(
            "/ws/base/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/ws/app/.arsumbris/repo.yaml", b"name: app\n".to_vec());
        fs.insert(
            "/ws/app/type/other.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        let kb = build(Path::new("/ws"), &fs).unwrap();

        let codes: Vec<&str> = kb.diagnostics().map(|d| d.code.as_str()).collect();
        // Contributes nothing: the disabled member's files are never walked.
        assert!(
            !kb.catalog
                .contains_key(Path::new("/ws/base/type/note.type.yaml")),
            "disabled member's files must not enter the catalog; diags {codes:?}"
        );
        // A non-disabled member still mounts.
        assert!(
            kb.catalog
                .contains_key(Path::new("/ws/app/type/other.type.yaml")),
            "a non-disabled member still mounts"
        );
        // No unmounted diagnostic: found-but-off, not declared-but-missing.
        assert!(
            !codes.contains(&"edit-member-unmounted"),
            "a disabled member fires no edit-member-unmounted; diags {codes:?}"
        );
        // No undeclared-nested-repo: the member is declared, just disabled.
        assert!(
            !codes.contains(&"undeclared-nested-repo"),
            "a disabled declared member is not an undeclared nested repo; diags {codes:?}"
        );
        // A declared-and-disabled member is intentional: no typo warning.
        assert!(
            !codes.contains(&"disabled-member-not-declared"),
            "a matching disabled member fires no typo warning; diags {codes:?}"
        );
        // The workspace remembers the declared role and the overlay, but excludes
        // the member from the resolved set.
        let ws = kb
            .workspaces
            .iter()
            .find(|w| w.name == "ws")
            .expect("workspace present");
        assert!(ws.disabled.iter().any(|n| n.as_str() == "base"));
        assert!(ws.edit.iter().any(|n| n.as_str() == "base"));
        assert!(
            !ws.member_roles.keys().any(|n| n.as_str() == "base"),
            "disabled member absent from member_roles"
        );
        // Fully absent from the mounted set: the marker is dropped, so no empty
        // repo enters kb.repos (exactly as if absent).
        assert!(
            !kb.repos.repos().iter().any(|r| r.name.as_str() == "base"),
            "a disabled member builds no repo, got {:?}",
            kb.repos
                .repos()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
        );
        // The members read surfaces the disabled member greyed, with its declared
        // edit role; a mounted member reports disabled: false.
        let mv = crate::wire::introspect_members(&kb, Path::new("/ws"), None);
        let base = mv
            .members
            .iter()
            .find(|m| m.repo == "base")
            .expect("disabled member appears in the members read");
        assert!(base.disabled, "base is disabled");
        assert_eq!(base.role, "edit", "declared role remembered");
        assert!(base.editable, "an edit-role disabled member is editable");
        assert!(
            base.root.is_empty(),
            "a disabled member is not resolved here"
        );
        let app = mv
            .members
            .iter()
            .find(|m| m.repo == "app")
            .expect("mounted member present");
        assert!(!app.disabled, "a mounted member is not disabled");
    }

    #[test]
    fn a_disabled_name_that_is_also_a_dep_still_mounts_as_a_dep() {
        // `disabled:` overlays the edit/discover ROLE lists, not an intrinsic
        // type-dependency. `base` is disabled AND a dep of the active member
        // `app`, so it must still mount as a dep (its types available), never
        // orphan its walked files. Regression guard for the marker-drop being
        // dep-blind.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - app\ndisabled:\n  - base\n".to_vec(),
        );
        fs.insert(
            "/ws/app/.arsumbris/repo.yaml",
            b"name: app\ndeps:\n  - name: base\n".to_vec(),
        );
        fs.insert("/ws/base/.arsumbris/repo.yaml", b"name: base\n".to_vec());
        fs.insert(
            "/ws/base/type/thing.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        let kb = build(Path::new("/ws"), &fs).unwrap();
        // base mounts as a dep: discovered as a repo, its files attributed to it.
        assert!(
            kb.repos.repos().iter().any(|r| r.name.as_str() == "base"),
            "a disabled name that is a dep still mounts, got repos {:?}",
            kb.repos
                .repos()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            kb.catalog
                .contains_key(Path::new("/ws/base/type/thing.type.yaml")),
            "base's dep files are walked and not orphaned"
        );
        // The file is attributed to base, not misattributed to a surviving
        // ancestor (app / ws) — the orphaning the review predicted.
        assert_eq!(
            kb.repos
                .repo_of(Path::new("/ws/base/type/thing.type.yaml"))
                .map(|r| r.name.as_str()),
            Some("base"),
            "base's file is attributed to base, not an ancestor repo"
        );
        // The members read shows base mounted as a dep, NOT disabled, and once.
        let mv = crate::wire::introspect_members(&kb, Path::new("/ws"), None);
        let n_base = mv.members.iter().filter(|m| m.repo == "base").count();
        assert_eq!(n_base, 1, "base listed exactly once");
        let base = mv.members.iter().find(|m| m.repo == "base").unwrap();
        assert!(
            !base.disabled,
            "a dep-mounted member is never reported disabled"
        );
        assert_eq!(base.role, "dep");
    }

    #[test]
    fn a_disabled_name_not_declared_warns() {
        // `ghost` is listed in `disabled:` but declared in neither `edit:` nor
        // `discover:`: a manifest typo, a disabled overlay silences a DECLARED
        // member. Surfaced rather than left silently inert.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\ndisabled:\n  - ghost\n".to_vec(),
        );
        let kb = build(Path::new("/ws"), &fs).unwrap();
        let codes: Vec<&str> = kb.diagnostics().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&"disabled-member-not-declared"),
            "an undeclared disabled name warns; diags {codes:?}"
        );
    }

    #[test]
    fn a_member_nested_under_a_member_resolves_regardless_of_edit_order() {
        // The load-bearing fixpoint property. `deep` is a declared member nested
        // under `mid`, itself nested under the entry (two levels). The entry walk
        // surfaces only `mid`'s marker; `deep`'s marker appears only after `mid` is
        // walked. A single forward resolution pass would resolve `deep` unmounted
        // and never retry, and the outcome would depend on the `edit` order. The
        // iterate-to-stable fixpoint re-resolves `deep` once `mid`'s walk surfaces
        // its marker, so `deep` mounts regardless of order. A member's content is
        // walked only if it mounts, so a catalogued `deep` file proves it mounted.
        for order in [["ws", "mid", "deep"], ["ws", "deep", "mid"]] {
            let manifest = format!(
                "edit:\n{}",
                order
                    .iter()
                    .map(|n| format!("  - {n}\n"))
                    .collect::<String>()
            );
            let mut fs = MemoryFileSystem::new();
            fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
            fs.insert("/ws/.arsumbris/workspace.yaml", manifest.into_bytes());
            fs.insert("/ws/mid/.arsumbris/repo.yaml", b"name: mid\n".to_vec());
            fs.insert("/ws/mid/note.md", b"---\ntype: note\n---\n".to_vec());
            fs.insert(
                "/ws/mid/deep/.arsumbris/repo.yaml",
                b"name: deep\n".to_vec(),
            );
            fs.insert("/ws/mid/deep/leaf.md", b"---\ntype: note\n---\n".to_vec());
            let kb = build(Path::new("/ws"), &fs).unwrap();
            assert!(
                kb.catalog.contains_key(Path::new("/ws/mid/deep/leaf.md")),
                "deep's content is walked, so deep mounted; edit order = {order:?}"
            );
            assert!(
                kb.catalog.contains_key(Path::new("/ws/mid/note.md")),
                "mid's content is walked; edit order = {order:?}"
            );
        }
    }

    #[test]
    fn only_the_outermost_undeclared_nested_repo_warns() {
        // The bounded walk stops at the first nested repo on each path, so an
        // undeclared repo buried inside another undeclared repo never surfaces. The
        // entry walk stops at `vendor`, so `vendor/inner` is never walked and never
        // flagged: only the outermost undeclared repo warns, and its content, and
        // everything below it, contributes nothing.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert("/ws/keep.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert(
            "/ws/vendor/.arsumbris/repo.yaml",
            b"name: vendor\n".to_vec(),
        );
        fs.insert("/ws/vendor/v.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert(
            "/ws/vendor/inner/.arsumbris/repo.yaml",
            b"name: inner\n".to_vec(),
        );
        fs.insert("/ws/vendor/inner/i.md", b"---\ntype: note\n---\n".to_vec());
        let kb = build(Path::new("/ws"), &fs).unwrap();
        let undeclared: Vec<&str> = kb
            .diagnostics()
            .filter(|d| d.code == crate::repo::UNDECLARED_NESTED_REPO)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            undeclared.len(),
            1,
            "only the outermost undeclared repo warns, got {undeclared:?}"
        );
        assert!(
            undeclared[0].contains("vendor") && !undeclared[0].contains("inner"),
            "the warning names vendor (outermost), not inner: {undeclared:?}"
        );
        assert!(
            kb.catalog.contains_key(Path::new("/ws/keep.md")),
            "the entry's own content is walked"
        );
        assert!(
            !kb.catalog.contains_key(Path::new("/ws/vendor/v.md")),
            "the undeclared repo's content contributes nothing"
        );
        assert!(
            !kb.catalog.contains_key(Path::new("/ws/vendor/inner/i.md")),
            "content buried inside an undeclared repo contributes nothing"
        );
    }

    #[test]
    fn a_folder_drifted_declared_member_resolves_by_its_marker() {
        // A declared member whose folder name differs from its declared name still
        // resolves: the marker is the definitive repo-root indicator (X). `lib`
        // lives in a `mylib/` folder; it mounts (its content is walked), with a
        // folder-name-mismatch drift advisory. Before X, the folder-name sibling
        // probe missed it and it stayed unmounted.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - lib\n".to_vec(),
        );
        fs.insert("/ws/mylib/.arsumbris/repo.yaml", b"name: lib\n".to_vec());
        fs.insert("/ws/mylib/note.md", b"---\ntype: note\n---\n".to_vec());
        let kb = build(Path::new("/ws"), &fs).unwrap();
        assert!(
            kb.catalog.contains_key(Path::new("/ws/mylib/note.md")),
            "the drifted member's content is walked, so it mounted"
        );
        assert!(
            kb.diagnostics()
                .any(|d| d.code == crate::repo::REPO_FOLDER_NAME_MISMATCH),
            "the folder drift is surfaced as an advisory, got {:?}",
            kb.diagnostics().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn assembly_roots_covers_a_member_nested_under_a_member() {
        // The watcher and fingerprint derive their roots from `assembly_roots`, so
        // it must reach the same deep members `build` reads, else an edit to a
        // member nested under a declared member never triggers a rebuild. Sharing
        // the walk-resolve fixpoint guarantees it: `deep` (nested under `mid`) is a
        // watched root.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/.arsumbris/repo.yaml", b"name: ws\n".to_vec());
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - mid\n  - deep\n".to_vec(),
        );
        fs.insert("/ws/mid/.arsumbris/repo.yaml", b"name: mid\n".to_vec());
        fs.insert(
            "/ws/mid/deep/.arsumbris/repo.yaml",
            b"name: deep\n".to_vec(),
        );
        let (roots, _) = assembly_roots(Path::new("/ws"), &crate::repo::UserRegistry::new(), &fs);
        for expected in ["/ws", "/ws/mid", "/ws/mid/deep"] {
            assert!(
                roots.contains(&PathBuf::from(expected)),
                "{expected} is a watched root: {roots:?}"
            );
        }
    }

    #[test]
    fn the_out_of_build_seams_still_carry_their_spans() {
        // `fingerprint` and `assembly_roots` run OUTSIDE `build_reusing`, so
        // nothing in the build's own span tree accounts for them and only their
        // own spans make them visible. The spec makes that coverage a property,
        // see [[spec - operation tracing - env-gated spans at the seams emit a
        // perfetto trace and a timing log]].
        //
        // REGRESSION GUARD, for a failure that is completely silent. An
        // `#[instrument]` attribute binds to whatever item FOLLOWS it, so
        // inserting a function between the attribute and the function it was
        // written for moves the span onto the new one. That happened to
        // `fingerprint`: the span vanished from every trace, its doc comment
        // ended up on the wrong function, and a per-file helper got instrumented
        // in a hot loop instead. Nothing failed; a number just stopped appearing.
        let seen = crate::spancap::capture(|| {
            let fs = scattered_fs();
            let _ = fingerprint(Path::new("/ws"), &crate::repo::ConfigSource::Empty, &fs);
        });
        for expected in ["fingerprint", "assembly_roots"] {
            assert!(
                seen.iter().any(|n| n == expected),
                "the `{expected}` seam lost its span; an #[instrument] attribute \
                 most likely rebound to an item inserted after it. Seen: {seen:?}"
            );
        }
        // The per-path predicate must NOT be spanned: it is called once per
        // walked file, so instrumenting it puts a span in a hot loop and floods
        // the trace with noise that hides the seams above.
        assert!(
            !seen.iter().any(|n| n == "build_reads_content"),
            "a per-file helper is spanned, which floods the trace: {seen:?}"
        );
    }

    #[test]
    fn fingerprint_spans_co_present_members_and_manifest() {
        let fs = scattered_fs();
        let fp = fingerprint(Path::new("/ws"), &crate::repo::ConfigSource::Empty, &fs).unwrap();

        // Each co-present member's type-def is in the fingerprint.
        assert!(fp.contains_key(Path::new("/ws/base/type/note.type.yaml")));
        assert!(fp.contains_key(Path::new("/ws/app/type/note.type.yaml")));
        // Each member's registry, the walker ignores `.arsumbris/`.
        assert!(fp.contains_key(Path::new("/ws/base/.arsumbris/repo.yaml")));
        assert!(fp.contains_key(Path::new("/ws/app/.arsumbris/repo.yaml")));
        // The entry manifest in `/ws`.
        assert!(fp.contains_key(Path::new("/ws/.arsumbris/workspace.yaml")));
    }

    #[test]
    fn a_member_edit_changes_the_fingerprint() {
        let before = fingerprint(
            Path::new("/ws"),
            &crate::repo::ConfigSource::Empty,
            &scattered_fs(),
        )
        .unwrap();

        let mut fs = scattered_fs();
        fs.insert(
            "/ws/base/type/note.type.yaml",
            b"fields:\n  title: String\n  extra: Boolean\n".to_vec(),
        );
        let after = fingerprint(Path::new("/ws"), &crate::repo::ConfigSource::Empty, &fs).unwrap();

        assert_ne!(
            before, after,
            "editing a co-present member must move the fingerprint"
        );
    }

    #[test]
    fn a_manifest_edit_changes_the_fingerprint() {
        let before = fingerprint(
            Path::new("/ws"),
            &crate::repo::ConfigSource::Empty,
            &scattered_fs(),
        )
        .unwrap();

        // Drop a member from the manifest: the member set, hence the rebuild, changes.
        let mut fs = scattered_fs();
        fs.insert(
            "/ws/.arsumbris/workspace.yaml",
            b"edit:\n  - ws\n  - base\n".to_vec(),
        );
        let after = fingerprint(Path::new("/ws"), &crate::repo::ConfigSource::Empty, &fs).unwrap();

        assert_ne!(
            before, after,
            "editing the manifest must move the fingerprint"
        );
    }

    #[test]
    fn entry_only_fingerprint_is_unchanged_without_a_manifest() {
        // No manifest in the entry dir: assembly_roots returns the entry repo alone.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: solo\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        let fp = fingerprint(Path::new("/v"), &crate::repo::ConfigSource::Empty, &fs).unwrap();
        assert!(fp.contains_key(Path::new("/v/type/note.type.yaml")));
        assert!(fp.contains_key(Path::new("/v/.arsumbris/repo.yaml")));
    }
}

#[cfg(test)]
mod reuse_tests {
    //! Parse-layer reuse across builds: an unchanged file reuses its prior
    //! parse (same `Arc` allocation), a changed file re-parses, and a reused
    //! build is identical to a from-scratch build.

    use super::*;
    use au_parser::MemoryFileSystem;
    use std::path::Path;

    /// A single-repo knowledge base: registry, one type-def, two instances.
    fn base_fs() -> MemoryFileSystem {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A\n---\nbody a\n".to_vec(),
        );
        fs.insert(
            "/v/b.md",
            b"---\ntype: note\ntitle: B\n---\nbody b\n".to_vec(),
        );
        fs
    }

    /// The identity of a catalog entry's parse, for `Arc`-allocation comparison:
    /// equal pointers mean the parse was reused, not re-built.
    fn parse_ptr(kb: &KnowledgeBase, path: &str) -> *const FileParse {
        Arc::as_ptr(&kb.catalog.get(Path::new(path)).unwrap().parse)
    }

    #[test]
    fn broken_entry_repo_yaml_refusal_names_the_parse_error() {
        let mut fs = MemoryFileSystem::new();
        // An unquoted `:` in the entry repo.yaml value breaks the parse.
        fs.insert(
            "/v/.arsumbris/repo.yaml",
            b"name: v\ndescription: text: colon\n".to_vec(),
        );
        match resolve_entry(Path::new("/v"), &fs) {
            Err(BuildError::EntryNotARepo {
                reason: Some(reason),
                ..
            }) => {
                assert!(reason.contains("not valid YAML"), "reason: {reason}");
                assert!(
                    reason.contains("quote"),
                    "the refusal should carry the quoting hint: {reason}"
                );
            }
            other => panic!("expected EntryNotARepo with a reason, got {other:?}"),
        }
    }

    #[test]
    fn absent_entry_repo_yaml_refuses_without_a_reason() {
        // No repo.yaml at all, an ordinary not-a-repo, no parse detail to give.
        let fs = MemoryFileSystem::new();
        assert!(matches!(
            resolve_entry(Path::new("/v"), &fs),
            Err(BuildError::EntryNotARepo { reason: None, .. })
        ));
    }

    #[test]
    fn editing_one_instance_reuses_the_others() {
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A edited\n---\nbody a2\n".to_vec(),
        );
        let kb2 = build_reusing(
            Path::new("/v"),
            &fs2,
            &crate::repo::UserRegistry::new(),
            &prior,
            &Fingerprint::new(),
            None,
        )
        .unwrap();

        // The edited file re-parsed: a fresh allocation.
        assert_ne!(parse_ptr(&kb1, "/v/a.md"), parse_ptr(&kb2, "/v/a.md"));
        // The untouched files reused the same parse allocation.
        assert_eq!(parse_ptr(&kb1, "/v/b.md"), parse_ptr(&kb2, "/v/b.md"));
        assert_eq!(
            parse_ptr(&kb1, "/v/type/note.type.yaml"),
            parse_ptr(&kb2, "/v/type/note.type.yaml"),
        );
    }

    #[test]
    fn entry_only_mounts_a_scattered_registry_dep_without_a_cache() {
        // Entry-only folder-repo `/main` (a repo whose folder matches its name, so it
        // self-resolves) declares dep `lib`. `lib` is a SCATTERED editable repo at
        // `/scattered/lib` (outside the entry), mapped by the per-user registry.
        // With NO package cache (cache_root None), member assembly must still mount
        // `lib` so its types resolve — matching what `assembly_roots` (the watcher
        // / fingerprint view) resolves, so the mounted and watched sets agree.
        // Under the old cache-gated assembly, `lib` stayed unwalked and dangled.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/main/.arsumbris/repo.yaml",
            b"name: main\ndeps:\n  - name: lib\n".to_vec(),
        );
        fs.insert("/main/type/mainthing.type.yaml", b"fields: {}\n".to_vec());
        fs.insert(
            "/scattered/lib/.arsumbris/repo.yaml",
            b"name: lib\n".to_vec(),
        );
        fs.insert(
            "/scattered/lib/type/libthing.type.yaml",
            b"fields: {}\n".to_vec(),
        );

        let mut registry = crate::repo::UserRegistry::new();
        registry.insert(
            crate::repo::RepoName("lib".into()),
            crate::repo::RegistryLocation {
                remote: None,
                path: PathBuf::from("/scattered/lib"),
            },
        );

        let kb = build_reusing(
            Path::new("/main"),
            &fs,
            &registry,
            &ParseLayer::default(),
            &Fingerprint::new(),
            None, // NO cache root — the crux.
        )
        .unwrap();

        let lib = kb
            .repos
            .by_name("lib")
            .expect("a scattered registry dep mounts in an entry-only workspace without a cache");
        assert_eq!(lib.root, PathBuf::from("/scattered/lib"));
        assert!(
            kb.graph_for_repo("lib")
                .and_then(|g| g.closure_id(&au_core::TypeName("libthing".into())))
                .is_some(),
            "the scattered dep's own types resolve once it is mounted"
        );
    }

    #[test]
    fn cross_repo_extends_parent_reaches_the_types_read() {
        // Entry `/main` depends on `lib`. `lib` owns base type `base`. `main`'s
        // `sub` extends the peer base via `extends: base::lib`. The `types` read
        // must report sub.parents == ["base::lib"], verbatim, per the wire
        // contract (a cross-repo parent reads `name::repo`). Guards all three
        // read modes: repo full, repo summary, and the workspace read.
        const QUALIFIED_PARENT: &str = "base::lib";
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/main/.arsumbris/repo.yaml",
            b"name: main\ndeps:\n  - name: lib\n".to_vec(),
        );
        fs.insert(
            "/main/type/sub.type.yaml",
            b"extends: base::lib\nfields:\n  own: String\n".to_vec(),
        );
        fs.insert(
            "/scattered/lib/.arsumbris/repo.yaml",
            b"name: lib\n".to_vec(),
        );
        fs.insert(
            "/scattered/lib/type/base.type.yaml",
            b"fields:\n  shared: String\n".to_vec(),
        );

        let mut registry = crate::repo::UserRegistry::new();
        registry.insert(
            crate::repo::RepoName("lib".into()),
            crate::repo::RegistryLocation {
                remote: None,
                path: PathBuf::from("/scattered/lib"),
            },
        );

        let kb = build_reusing(
            Path::new("/main"),
            &fs,
            &registry,
            &ParseLayer::default(),
            &Fingerprint::new(),
            None,
        )
        .unwrap();

        // Repo-scoped, full mode.
        let crate::wire::WireTypesResult::Full(views) = crate::wire::introspect_repo_types_paged(
            &kb,
            "main",
            0,
            None,
            false,
            crate::wire::TypeScope::all(),
        )
        .expect("main is a known repo") else {
            panic!("full mode requested");
        };
        let sub = views
            .iter()
            .find(|v| v.def.name == "sub")
            .expect("sub is in main's graph");
        assert_eq!(
            sub.def.parents,
            vec![QUALIFIED_PARENT.to_string()],
            "repo full: cross-repo extends parent must be reported verbatim"
        );

        // Repo-scoped, summary mode (the cheaper `type_summary_view` path, which
        // also reads `def.parents`).
        let crate::wire::WireTypesResult::Summary(sums) = crate::wire::introspect_repo_types_paged(
            &kb,
            "main",
            0,
            None,
            true,
            crate::wire::TypeScope::all(),
        )
        .expect("main is a known repo") else {
            panic!("summary mode requested");
        };
        let sub_sum = sums
            .iter()
            .find(|v| v.name == "sub")
            .expect("sub is in the summary");
        assert_eq!(
            sub_sum.parents,
            vec![QUALIFIED_PARENT.to_string()],
            "repo summary: cross-repo extends parent must be reported verbatim"
        );

        // Workspace read (no `repo` scope).
        let crate::wire::WireTypesResult::Full(ws) = crate::wire::introspect_workspace_types_paged(
            &kb,
            0,
            None,
            false,
            crate::wire::TypeScope::all(),
        ) else {
            panic!("full mode requested");
        };
        let sub_ws = ws
            .iter()
            .find(|v| v.def.name == "sub")
            .expect("sub is in the workspace read");
        assert_eq!(
            sub_ws.def.parents,
            vec![QUALIFIED_PARENT.to_string()],
            "workspace: cross-repo extends parent must be reported verbatim"
        );
    }

    #[test]
    fn editing_a_type_def_reuses_instance_parses() {
        // The parse/resolved split: instances reuse their parse even though the
        // graph recomputes whole-knowledge-base off the changed type-def.
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n  subtitle: String\n".to_vec(),
        );
        let kb2 = build_reusing(
            Path::new("/v"),
            &fs2,
            &crate::repo::UserRegistry::new(),
            &prior,
            &Fingerprint::new(),
            None,
        )
        .unwrap();

        assert_ne!(
            parse_ptr(&kb1, "/v/type/note.type.yaml"),
            parse_ptr(&kb2, "/v/type/note.type.yaml"),
        );
        assert_eq!(parse_ptr(&kb1, "/v/a.md"), parse_ptr(&kb2, "/v/a.md"));
        assert_eq!(parse_ptr(&kb1, "/v/b.md"), parse_ptr(&kb2, "/v/b.md"));
    }

    #[test]
    fn adding_a_file_reuses_existing_parses() {
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/c.md",
            b"---\ntype: note\ntitle: C\n---\nbody c\n".to_vec(),
        );
        let kb2 = build_reusing(
            Path::new("/v"),
            &fs2,
            &crate::repo::UserRegistry::new(),
            &prior,
            &Fingerprint::new(),
            None,
        )
        .unwrap();

        assert_eq!(parse_ptr(&kb1, "/v/a.md"), parse_ptr(&kb2, "/v/a.md"));
        assert_eq!(parse_ptr(&kb1, "/v/b.md"), parse_ptr(&kb2, "/v/b.md"));
        assert!(kb2.catalog.contains_key(Path::new("/v/c.md")));
    }

    #[test]
    fn deleting_a_file_reuses_the_rest() {
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        // The after-delete fs: no `b.md`. `MemoryFileSystem` has no remove, so
        // construct the surviving set directly.
        let mut fs2 = MemoryFileSystem::new();
        fs2.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs2.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs2.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A\n---\nbody a\n".to_vec(),
        );
        let kb2 = build_reusing(
            Path::new("/v"),
            &fs2,
            &crate::repo::UserRegistry::new(),
            &prior,
            &Fingerprint::new(),
            None,
        )
        .unwrap();

        assert!(!kb2.catalog.contains_key(Path::new("/v/b.md")));
        assert_eq!(parse_ptr(&kb1, "/v/a.md"), parse_ptr(&kb2, "/v/a.md"));
    }

    #[test]
    fn a_reused_build_matches_a_from_scratch_build() {
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A edited\n---\nbody a2\n".to_vec(),
        );

        let reused = build_reusing(
            Path::new("/v"),
            &fs2,
            &crate::repo::UserRegistry::new(),
            &prior,
            &Fingerprint::new(),
            None,
        )
        .unwrap();
        let scratch = build(Path::new("/v"), &fs2).unwrap();

        // Reuse must not change the build: the whole knowledge base, not just the
        // catalog, must equal a from-scratch build. The parity oracle the
        // incremental-resolved effort asserts against.
        crate::ir::assert_kb_parity(&reused, &scratch);
    }

    #[test]
    fn kb_parity_holds_for_two_scratch_builds() {
        // The oracle holds on equality: two builds of the same disk are
        // byte-identical, the structural IR is deterministic.
        let fs = base_fs();
        let a = build(Path::new("/v"), &fs).unwrap();
        let b = build(Path::new("/v"), &fs).unwrap();
        crate::ir::assert_kb_parity(&a, &b);
    }

    #[test]
    fn kb_parity_detects_a_difference() {
        // The oracle is not vacuous: an actual content change diverges the
        // facts, so a stale incremental reuse would be caught.
        let fs1 = base_fs();
        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: CHANGED\n---\nbody a\n".to_vec(),
        );
        let a = build(Path::new("/v"), &fs1).unwrap();
        let b = build(Path::new("/v"), &fs2).unwrap();
        assert_ne!(a.parity_facts(), b.parity_facts());
    }

    #[test]
    fn closure_membership_keys_by_repo_and_includes_ancestors() {
        // Two repos each declare a same-named `base`; repo a adds `child: base`.
        // Closure membership keys by (repo, type), so the two `base` keys stay
        // distinct, and a `child` instance is a member of both child and base.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/.arsumbris/workspace.yaml",
            b"edit:\n  - v\n  - a\n  - b\n".to_vec(),
        );
        fs.insert("/v/a/.arsumbris/repo.yaml", b"name: a\n".to_vec());
        fs.insert(
            "/v/a/type/base.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/v/a/type/child.type.yaml", b"extends: base\n".to_vec());
        fs.insert("/v/a/x.md", b"---\ntype: child\ntitle: X\n---\n".to_vec());
        fs.insert("/v/b/.arsumbris/repo.yaml", b"name: b\n".to_vec());
        fs.insert(
            "/v/b/type/base.type.yaml",
            b"fields:\n  title: String\n".to_vec(),
        );
        fs.insert("/v/b/y.md", b"---\ntype: base\ntitle: Y\n---\n".to_vec());

        let kb = build(Path::new("/v"), &fs).unwrap();

        let member = |repo: &str, ty: &str, file: &str| {
            kb.closure_members.iter().any(|((r, t), ms)| {
                r.as_str() == repo && t.as_str() == ty && ms.iter().any(|p| p.ends_with(file))
            })
        };
        // The child instance is a member of child AND its ancestor base, in a.
        assert!(member("a", "child", "x.md"), "child membership");
        assert!(member("a", "base", "x.md"), "ancestor base membership");
        // Repo b's base is a distinct key; the two repos never bleed together.
        assert!(member("b", "base", "y.md"), "b's own base membership");
        assert!(
            !member("b", "base", "x.md"),
            "a's instance must not enter b's base"
        );
        assert!(
            !member("a", "base", "y.md"),
            "b's instance must not enter a's base"
        );
    }

    #[test]
    fn identity_dependents_are_typed_referrers_not_navigational() {
        // src links to target through a typed slot, so it depends on target's
        // identity; nav links only navigationally, so it does not.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/v/.arsumbris/repo.yaml", b"name: v\n".to_vec());
        fs.insert(
            "/v/type/note.type.yaml",
            b"fields:\n  link?: note*\n".to_vec(),
        );
        fs.insert("/v/target.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert(
            "/v/src.md",
            b"---\ntype: note\nlink: \"[[target]]\"\n---\nalso see [[target]]\n".to_vec(),
        );
        fs.insert(
            "/v/nav.md",
            b"---\ntype: note\n---\nonly navigational [[target]]\n".to_vec(),
        );

        let kb = build(Path::new("/v"), &fs).unwrap();
        let deps = kb.identity_dependents(Path::new("/v/target.md"));
        let names: Vec<&str> = deps
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(
            names.contains(&"src.md"),
            "a typed referrer is an identity dependent, got {names:?}"
        );
        assert!(
            !names.contains(&"nav.md"),
            "a navigational-only referrer is not an identity dependent, got {names:?}"
        );
    }

    /// Wraps a `MemoryFileSystem` and records every `read_file` path, so a test
    /// can assert which files the build actually read.
    struct CountingFs {
        inner: MemoryFileSystem,
        reads: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl CountingFs {
        fn new(inner: MemoryFileSystem) -> Self {
            Self {
                inner,
                reads: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn reads_of(&self, path: &str) -> usize {
            self.reads
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.as_path() == Path::new(path))
                .count()
        }
    }

    impl au_parser::FileSystem for CountingFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.reads.lock().unwrap().push(path.to_path_buf());
            self.inner.read_file(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            // Deliberately NOT counted as a read: it decodes nothing, and this
            // double exists to assert which files the build actually reads.
            self.inner.is_file(path)
        }
        fn file_len(&self, path: &Path) -> std::io::Result<u64> {
            // A stat, NOT a read: delegate to the inner (in-memory) size so the
            // read-cap probe never inflates the read count this double asserts.
            // Without this the trait default would route through `read_file` and
            // count as a read.
            self.inner.file_len(path)
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
    fn read_skip_avoids_reading_unchanged_files() {
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        // Edit a.md; the others are byte-identical.
        let mut edited = fs1.clone();
        edited.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A edited\n---\nbody a2\n".to_vec(),
        );
        // The hashes the engine would pass: computed over the post-edit disk,
        // before wrapping in the counter, so this pass is not counted.
        let known =
            fingerprint(Path::new("/v"), &crate::repo::ConfigSource::Empty, &edited).unwrap();

        let counting = CountingFs::new(edited);
        let kb2 = build_reusing(
            Path::new("/v"),
            &counting,
            &crate::repo::UserRegistry::new(),
            &prior,
            &known,
            None,
        )
        .unwrap();

        // The unchanged files were reused without a read in the build pass.
        assert_eq!(
            counting.reads_of("/v/b.md"),
            0,
            "unchanged instance not read"
        );
        assert_eq!(
            counting.reads_of("/v/type/note.type.yaml"),
            0,
            "unchanged type-def not read"
        );
        // The edited file was read once.
        assert_eq!(counting.reads_of("/v/a.md"), 1, "edited instance read once");

        // Read-skip must not change the build: whole-knowledge-base parity with a
        // from-scratch build.
        let scratch = build(Path::new("/v"), &counting).unwrap();
        crate::ir::assert_kb_parity(&kb2, &scratch);
    }

    #[test]
    fn omitting_a_dirty_path_from_known_reads_only_it() {
        // The engine's scoped rebuild builds `known` as the prior fingerprint
        // minus the dirty paths, so a dirty path is read and every other file
        // reuses its parse with no read. This mirrors that map.
        let fs1 = base_fs();
        let kb1 = build(Path::new("/v"), &fs1).unwrap();
        let prior = kb1.parse_layer();

        let mut known: Fingerprint = kb1
            .catalog
            .iter()
            .map(|(p, e)| (p.clone(), e.hash))
            .collect();
        known.remove(Path::new("/v/a.md"));

        let mut fs2 = fs1.clone();
        fs2.insert(
            "/v/a.md",
            b"---\ntype: note\ntitle: A edited\n---\nbody a2\n".to_vec(),
        );
        let counting = CountingFs::new(fs2);
        build_reusing(
            Path::new("/v"),
            &counting,
            &crate::repo::UserRegistry::new(),
            &prior,
            &known,
            None,
        )
        .unwrap();

        assert_eq!(
            counting.reads_of("/v/a.md"),
            1,
            "omitted dirty path is read"
        );
        assert_eq!(counting.reads_of("/v/b.md"), 0, "non-dirty instance reused");
        assert_eq!(
            counting.reads_of("/v/type/note.type.yaml"),
            0,
            "non-dirty type-def reused"
        );
    }
}
