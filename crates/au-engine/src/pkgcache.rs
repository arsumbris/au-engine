//! The device-global package cache: each resolved dependency materialized as an
//! immutable, read-only, content-addressed snapshot.
//!
//! A package fetches once into a temp store, extracts its tree to a temp sibling
//! directory, then renames that into `<root>/<sha>`. The rename is atomic on one
//! filesystem, so a half-fetched package never serves, per "a mutation never
//! leaves the graph broken by its own hand". The published snapshot is a plain
//! file tree with no `.git`, the assembly walk reads it unchanged, and it lives
//! outside any workspace so nothing watches it.
//!
//! Identity is the resolved commit sha. A re-resolve of a sha already present is
//! a `lookup`, no fetch. The resolver consults the lockfile for the offline,
//! reproducible path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use au_diagnostics::{Diagnostic, Severity, Span, SuggestedFix};
use au_parser::FileSystem;

use crate::gitwriter::{CommitMessage, CommitSha, GitWriteError, GitWriter, MutationId, ShellGit};
use crate::repo::{LockedPackage, MemberRole, PackageLock, RegistryEntry, RepoName, Workspace};

/// The first-party package registry, a git repo whose `registry.yaml` maps a
/// package name to its remote and audited ref.
///
/// A constant for now, the single place to change the registry source. A future
/// workspace-level override is planned, see the package-manager spec.
pub(crate) const DEFAULT_REGISTRY_REMOTE: &str = "git@github.com:arsumbris/registry.git";

/// The git ref the registry repo is read at, its default branch.
pub(crate) const DEFAULT_REGISTRY_REF: &str = "main";

/// The registry file inside the registry repo.
const REGISTRY_FILE: &str = "registry.yaml";

/// A resolved dependency present in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedPackage {
    /// The resolved commit sha the snapshot was extracted from.
    pub sha: String,
    /// The snapshot directory, `<root>/<sha>`, an immutable file tree.
    pub path: PathBuf,
}

/// The device-global package cache, rooted at `root`.
///
/// `root` is e.g. `~/.arsumbris/au-engine/cache/packages`; the caller resolves the home
/// dir, the cache stays a pure function of its root so it is testable off the
/// real home.
pub(crate) struct PackageCache {
    root: PathBuf,
}

/// Disambiguates concurrent temp scratch dirs within one process. Combined with
/// the pid it keeps two populates from colliding before the atomic rename.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl PackageCache {
    pub(crate) fn new(root: PathBuf) -> Self {
        PackageCache { root }
    }

    /// The cache root, `~/.arsumbris/au-engine/cache/packages`. Distinguishes an editable
    /// member (a local working tree, its root NOT under here) from a read-only
    /// cache snapshot, so the per-repo lock partition locks only editable members.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The snapshot directory for `sha`, if already populated.
    ///
    /// The offline path: a lockfile-pinned sha resolves with no fetch when the
    /// cache holds it.
    pub(crate) fn lookup(&self, sha: &str) -> Option<PathBuf> {
        let dir = self.root.join(sha);
        dir.is_dir().then_some(dir)
    }

    /// Resolve `remote` at `ref_`, populate its snapshot, and return it.
    ///
    /// Fetches to resolve the concrete sha, then short-circuits when that sha is
    /// already cached. Otherwise extracts the tree to a temp sibling and renames
    /// it in atomically. The temp scratch is always cleaned; a failure leaves no
    /// `<sha>` entry.
    pub(crate) fn ensure(&self, remote: &str, ref_: &str) -> Result<CachedPackage, GitWriteError> {
        let tmp = self.tmp_dir();
        let result = self.populate(remote, ref_, &tmp);
        // The published snapshot, if any, was renamed out of `tmp` already, so
        // dropping the scratch never removes it. Best-effort.
        let _ = std::fs::remove_dir_all(&tmp);
        result
    }

    fn populate(
        &self,
        remote: &str,
        ref_: &str,
        tmp: &Path,
    ) -> Result<CachedPackage, GitWriteError> {
        let git_dir = tmp.join("git");
        let sha = ShellGit.fetch_ref(&git_dir, remote, ref_)?;
        let final_path = self.root.join(&sha);
        // Content-addressed: a present snapshot for this sha is identical, reuse it.
        if final_path.is_dir() {
            return Ok(CachedPackage {
                sha,
                path: final_path,
            });
        }

        let extract = tmp.join("extract");
        ShellGit.extract_tree(&git_dir, &sha, &extract)?;
        // Deter accidental mutation of the content-addressed snapshot: mark its
        // files read-only BEFORE the atomic publish, so `<sha>` is read-only from
        // the instant it appears. Directories stay writable so the cache stays
        // disposable.
        make_snapshot_read_only(&extract);

        std::fs::create_dir_all(&self.root).map_err(|e| {
            GitWriteError::new(format!("could not create {}: {e}", self.root.display()))
        })?;
        // Atomic publish. A concurrent populate of the same sha may have won the
        // race, leaving `final_path` populated; its content is identical (same
        // sha), so treat that as success rather than clobbering it.
        match std::fs::rename(&extract, &final_path) {
            Ok(()) => {}
            Err(_) if final_path.is_dir() => {}
            Err(e) => {
                return Err(GitWriteError::new(format!(
                    "could not publish cache snapshot {}: {e}",
                    final_path.display()
                )));
            }
        }
        Ok(CachedPackage {
            sha,
            path: final_path,
        })
    }

    /// A unique scratch directory under `<root>/.tmp`, a sibling of the published
    /// snapshots so the publish rename stays on one filesystem.
    fn tmp_dir(&self) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        self.root.join(".tmp").join(format!("{pid}-{n}"))
    }
}

/// The outcome of resolving one dependency member.
///
/// The caller turns these into lockfile entries and into a loud diagnostic for a
/// failed resolution.
#[derive(Debug)]
pub(crate) struct DependencyResolution {
    pub name: RepoName,
    /// The remote actually fetched, from the member's explicit declaration or
    /// from the registry for a name-only dependency. `None` when resolution
    /// failed before a remote was known (the registry load or lookup failed).
    pub remote: Option<String>,
    /// The subpath the package sits at within the fetched repo, the monorepo
    /// case. `None` is the repo root. Recorded so the lock can pin it.
    pub path: Option<String>,
    /// The cached whole-repo snapshot on success, the fetch error otherwise.
    /// The mounted member is `outcome.path.join(self.path)`.
    pub outcome: Result<CachedPackage, GitWriteError>,
}

/// Mark every file under a published snapshot read-only, a deterrence against
/// accidental local mutation of the content-addressed cache (a stray editor or
/// script write hits `EACCES`). Directories stay writable so the cache stays
/// disposable (removable), the engine's canonical-state-is-elsewhere principle.
/// Symlinks are not followed. Best-effort, a permission error never fails the
/// publish, the snapshot content is still valid.
///
/// This is deterrence, not cryptographic integrity. Git's fetch-time sha is the
/// content check, and a process that can chmod the cache back owns the home
/// directory anyway. Re-hashing the tree on every lookup was rejected as
/// disproportionate for the accidental-mutation threat (and the `<sha>` name is a
/// COMMIT sha, not a hash of the extracted working tree, so it is not a cheap
/// compare).
fn make_snapshot_read_only(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            make_snapshot_read_only(&path);
        } else if ft.is_file() {
            if let Ok(meta) = entry.metadata() {
                let mut perms = meta.permissions();
                perms.set_readonly(true);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
        // A symlink is neither is_dir nor is_file here, so it is skipped, never
        // followed and never chmod'd through to its target.
    }
}

/// The mounted member path for a snapshot and an optional subpath. `None` when
/// the subpath is unsafe (absolute, or escaping the snapshot root), the
/// containment guard against a hostile dependency's registry declaring
/// `path: ../../..`. The subpath is untrusted (a transitive peer's subpath is
/// read verbatim from a fetched repo's registry), so this is the choke point,
/// every mount site routes through it. See
/// [[spec - diagnostic codes::au-type-system^dependency-path-escapes-snapshot]].
fn member_path(snapshot: &Path, subpath: &Option<String>) -> Option<PathBuf> {
    match subpath {
        Some(p) if is_safe_subpath(p) => Some(snapshot.join(p)),
        Some(_) => None,
        None => Some(snapshot.to_path_buf()),
    }
}

/// A dependency subpath is safe when it is relative and stays within the
/// snapshot root under lexical normalization. Rejects an absolute path and any
/// `..` that would climb above the root. Purely lexical, no filesystem access,
/// so it holds in the offline locate path too.
fn is_safe_subpath(subpath: &str) -> bool {
    use std::path::Component;
    let p = Path::new(subpath);
    if p.is_absolute() {
        return false;
    }
    let mut depth: i32 = 0;
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            // Absolute markers, already caught by `is_absolute`, rejected defensively.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The refuse-to-mount diagnostic for an escaping dependency subpath, anchored
/// at the workspace manifest (the actionable site, like the conflict codes).
fn dependency_path_escapes(name: &RepoName, subpath: Option<&str>, manifest: &Path) -> Diagnostic {
    Diagnostic {
        code: crate::repo::DEPENDENCY_PATH_ESCAPES_SNAPSHOT,
        severity: Severity::Error,
        span: Span::for_file(manifest.to_path_buf()),
        message: format!(
            "dependency '{}' declares a subpath '{}' that escapes its cache snapshot; \
             an absolute or parent-escaping path is refused, the dependency is not mounted",
            name.as_str(),
            subpath.unwrap_or("")
        ),
        related: Vec::new(),
        fix: None,
    }
}

/// One dependency to resolve: a declared name plus optional fetch info. The
/// seed is the workspace's direct `dependencies:`; the walk grows it with each
/// fetched dependency's declared `peers:`.
#[derive(Clone)]
struct DepSpec {
    name: RepoName,
    remote: Option<String>,
    git_ref: Option<String>,
    path: Option<String>,
}

impl DepSpec {
    /// The dedup key for the worklist, also the cycle guard: a spec already
    /// enqueued is never walked again, so a dependency cycle terminates.
    fn key(&self) -> (RepoName, Option<String>, Option<String>, Option<String>) {
        (
            self.name.clone(),
            self.remote.clone(),
            self.git_ref.clone(),
            self.path.clone(),
        )
    }
}

/// The outcome of resolving a workspace's dependency closure.
pub(crate) struct DependencyResolutions {
    /// Every resolved edge, for the resolve frame's `resolved` / `failed`.
    pub resolutions: Vec<DependencyResolution>,
    /// One `dependency-version-conflict` per package required at two versions.
    pub diagnostics: Vec<Diagnostic>,
    /// The member names involved in a conflict, excluded from the mounts and the
    /// lock so the engine never picks a winner.
    pub conflicted: BTreeSet<RepoName>,
    /// The dependency graph among FETCHED members: each resolved fetched dep to
    /// the names it declares as its own `deps`, captured at the peer-walk. The
    /// per-repo lock partition BFSes this to collect a repo's transitive fetched
    /// closure, see [`partition_repo_locks`].
    pub fetched_edges: BTreeMap<RepoName, Vec<RepoName>>,
}

/// Resolve a workspace's dependency closure into the cache, transitively.
///
/// Seeds with the direct `dependencies:` members, then for each fetched
/// dependency reads its repo-root `peers:` record and resolves those too, until
/// the closure is exhausted. Cycle-safe: a spec already enqueued is never
/// re-walked, so `A -> B -> A` terminates. A local project member shadows a
/// transitive edge of the same name and is never fetched. Each resolved member
/// is located at its immutable snapshot and recorded with `Dependency`
/// provenance, so the assembly walk and the `members` read see the full closure.
///
/// A package required at two versions across the closure, the same
/// `(remote, path)` at two shas, is a hard `dependency-version-conflict`: the
/// engine never picks a winner, so the conflicted package is left unmounted and
/// its names returned in `conflicted` for the caller to exclude from the lock.
pub(crate) fn resolve_dependencies(
    ws: &mut Workspace,
    cache: &PackageCache,
    registry_remote: &str,
) -> DependencyResolutions {
    let project_members: BTreeSet<RepoName> = ws
        .member_roles
        .iter()
        .filter(|(_, r)| r.editable())
        .map(|(n, _)| n.clone())
        .collect();

    // Seed the worklist with the direct dependency members AND the `discover`
    // members. A `discover` member is a consumed mount, not a type-dependency, but
    // it IS fetched and pinned so it mounts reproducibly and offline; its closure
    // lands in the workspace lock (`partition_workspace_lock`), the dep closures in
    // the per-repo locks. One unified solve, so a package required by both a dep
    // and a discover member is fetched once and conflict-checked together.
    let mut worklist: Vec<DepSpec> = ws
        .members
        .iter()
        .filter(|e| {
            matches!(
                ws.member_roles.get(&e.name),
                Some(MemberRole::Dep | MemberRole::Discover)
            )
        })
        .map(|e| DepSpec {
            name: e.name.clone(),
            remote: e.remote.clone(),
            git_ref: e.git_ref.clone(),
            path: e.path.clone(),
        })
        .collect();
    let mut enqueued: BTreeSet<_> = worklist.iter().map(DepSpec::key).collect();

    // Also seed every LOCAL editable member's RAW dep edges. `assemble_members`
    // collapses the closure to one edge per name in `ws.members`, so two editable
    // members declaring the same package at DIFFERENT remote/ref would otherwise
    // reach the solve as a single edge and silently pick a winner. Reading each
    // member's `deps` directly re-surfaces the divergent edges, deduped by key (an
    // edge identical to the collapsed one is a no-op), so a genuine disagreement
    // lands in `resolution_pins` and `detect_version_conflicts` fires, exactly as
    // it already does for the fetched-snapshot BFS below. A local project member
    // shadows a transitive edge, the same skip the BFS applies.
    for name in &project_members {
        let Some(root) = ws.member_paths.get(name) else {
            continue;
        };
        for dep in read_repo_peers(root) {
            if project_members.contains(&dep.name) {
                continue;
            }
            let spec = DepSpec {
                name: dep.name,
                remote: dep.remote,
                git_ref: dep.git_ref,
                path: dep.path,
            };
            if enqueued.insert(spec.key()) {
                worklist.push(spec);
            }
        }
    }

    // The registry is loaded at most once, and only if a name-only dependency
    // needs it.
    let mut registry: Option<Result<BTreeMap<RepoName, RegistryEntry>, GitWriteError>> = None;
    let mut resolutions: Vec<DependencyResolution> = Vec::new();
    // name -> located snapshot, filled into `member_paths` only after conflict
    // detection so a conflicted package is never mounted.
    let mut mounts: BTreeMap<RepoName, PathBuf> = BTreeMap::new();
    // A snapshot's peers are walked once, keyed by its resolved sha.
    let mut walked: BTreeSet<String> = BTreeSet::new();
    // Members whose declared subpath escapes the snapshot, refused and excluded
    // from both the mounts and the lock, exactly like a conflict.
    let mut path_escaped: BTreeSet<RepoName> = BTreeSet::new();
    let mut path_diagnostics: Vec<Diagnostic> = Vec::new();
    // Each fetched dep to the names it declares as `deps`, for the per-repo lock
    // partition's BFS over the fetched dependency graph.
    let mut fetched_edges: BTreeMap<RepoName, Vec<RepoName>> = BTreeMap::new();

    while let Some(spec) = worklist.pop() {
        let (remote, path, outcome) =
            match resolve_source(&spec, cache, registry_remote, &mut registry) {
                Ok((r, rf, p)) => {
                    let outcome = cache.ensure(&r, &rf);
                    (Some(r), p, outcome)
                }
                Err(e) => (None, None, Err(e)),
            };
        if let Ok(pkg) = &outcome {
            match member_path(&pkg.path, &path) {
                Some(located) => {
                    mounts.insert(spec.name.clone(), located);
                    // Read this snapshot's declared peers: they both record this
                    // dep's outgoing edges (for the lock partition) and grow the
                    // resolve closure (once per sha).
                    let peers = read_repo_peers(&pkg.path);
                    fetched_edges.insert(
                        spec.name.clone(),
                        peers.iter().map(|p| p.name.clone()).collect(),
                    );
                    if walked.insert(pkg.sha.clone()) {
                        for peer in peers {
                            if project_members.contains(&peer.name) {
                                continue; // a local project member shadows a transitive edge.
                            }
                            let next = DepSpec {
                                name: peer.name,
                                remote: peer.remote,
                                git_ref: peer.git_ref,
                                path: peer.path,
                            };
                            if enqueued.insert(next.key()) {
                                worklist.push(next);
                            }
                        }
                    }
                }
                None => {
                    // An escaping subpath is refused: not mounted, and its peers
                    // are not walked (a rejected member contributes nothing).
                    path_escaped.insert(spec.name.clone());
                    path_diagnostics.push(dependency_path_escapes(
                        &spec.name,
                        path.as_deref(),
                        &ws.manifest_path,
                    ));
                }
            }
        }
        resolutions.push(DependencyResolution {
            name: spec.name,
            remote,
            path,
            outcome,
        });
    }

    let (mut diagnostics, mut conflicted) =
        detect_version_conflicts(resolution_pins(&resolutions), &ws.manifest_path);
    let (id_diags, id_conflicted) =
        detect_identity_conflicts(resolution_pins(&resolutions), &ws.manifest_path);
    diagnostics.extend(id_diags);
    diagnostics.extend(path_diagnostics);
    conflicted.extend(id_conflicted);
    conflicted.extend(path_escaped);

    // Mount every resolved member except the conflicted ones. Record provenance
    // so the offline locate and the `members` read see the full closure; a name
    // already declared keeps its provenance (a project member stays project).
    for (name, located) in mounts {
        if conflicted.contains(&name) {
            continue;
        }
        ws.member_paths.insert(name.clone(), located);
        ws.member_roles.entry(name).or_insert(MemberRole::Dep);
    }

    DependencyResolutions {
        resolutions,
        diagnostics,
        conflicted,
        fetched_edges,
    }
}

/// Resolve one spec to a concrete `(remote, ref, subpath)` to fetch. An explicit
/// remote + ref is used directly; otherwise the name is looked up in the
/// registry, loaded (and cached) at most once across the whole walk.
fn resolve_source(
    spec: &DepSpec,
    cache: &PackageCache,
    registry_remote: &str,
    registry: &mut Option<Result<BTreeMap<RepoName, RegistryEntry>, GitWriteError>>,
) -> Result<(String, String, Option<String>), GitWriteError> {
    match (&spec.remote, &spec.git_ref) {
        (Some(r), Some(rf)) => Ok((r.clone(), rf.clone(), spec.path.clone())),
        _ => match registry.get_or_insert_with(|| load_registry(cache, registry_remote)) {
            Ok(map) => match map.get(&spec.name) {
                Some(entry) => Ok((
                    entry.remote.clone(),
                    entry.git_ref.clone(),
                    entry.path.clone(),
                )),
                None => Err(GitWriteError::new(format!(
                    "dependency '{}' is not in the registry",
                    spec.name.as_str()
                ))),
            },
            Err(e) => Err(GitWriteError::new(format!(
                "could not load the registry: {}",
                e.message
            ))),
        },
    }
}

/// A fetched dependency's declared `peers`, its transitive dependency record,
/// read from the snapshot's repo-root `.arsumbris/repo.yaml`. A
/// monorepo dependency's record is the whole-repo root, so a subpath package
/// contributes its repo's whole peer set, an over-approximation; the per-member
/// axis is a later refinement.
fn read_repo_peers(snapshot: &Path) -> Vec<crate::repo::RepoEntry> {
    let registry = snapshot.join(crate::repo::REGISTRY_REL);
    match std::fs::read(&registry) {
        Ok(bytes) => crate::repo::parse_repo_deps(&bytes),
        Err(_) => Vec::new(),
    }
}

/// The `(name, remote, path, sha)` pin of each successfully-resolved edge, the
/// input to conflict detection.
fn resolution_pins(
    resolutions: &[DependencyResolution],
) -> impl Iterator<Item = (&RepoName, &str, Option<&str>, &str)> {
    resolutions.iter().filter_map(|r| {
        let (Ok(pkg), Some(remote)) = (&r.outcome, &r.remote) else {
            return None;
        };
        Some((
            &r.name,
            remote.as_str(),
            r.path.as_deref(),
            pkg.sha.as_str(),
        ))
    })
}

/// Canonicalize a git remote for identity comparison, so two spellings of one
/// upstream compare equal, a URL scheme (`https://`, `ssh://`, `git://`), a
/// `user@` userinfo, an scp-form `host:path`, and a trailing `.git` or `/`. So
/// `git@github.com:org/x.git` and `https://github.com/org/x` both canonicalize
/// to `github.com/org/x`.
///
/// Conservative: it collapses only spelling variants that denote the same repo,
/// and does NOT lowercase the path (a case-sensitive host may distinguish two
/// repos), so it never MERGES two genuinely-different remotes into a false
/// conflict. A missed conflict (the bug this fixes) silently mounts two
/// versions; a false conflict would be the worse failure, so the bias is toward
/// under-merging.
fn normalize_remote(remote: &str) -> String {
    let mut s = remote.trim();
    // Drop a URL scheme.
    if let Some(pos) = s.find("://") {
        s = &s[pos + 3..];
    }
    // Drop `user@` userinfo, only the `@` before the first path separator.
    let first_slash = s.find('/').unwrap_or(s.len());
    if let Some(at) = s[..first_slash].find('@') {
        s = &s[at + 1..];
    }
    // Fixed-point strip of a trailing `/` and `.git`, in either order.
    loop {
        let t = s.trim_end_matches('/');
        let t = t.strip_suffix(".git").unwrap_or(t);
        if t.len() == s.len() {
            break;
        }
        s = t;
    }
    // scp-form `host:path` -> `host/path` (the scheme is already gone).
    s.replacen(':', "/", 1)
}

/// Detect packages required at conflicting versions: the same NORMALIZED
/// `(remote, path)` resolved to two or more distinct shas. Returns one
/// `dependency-version-conflict` error per conflicting package plus the set of
/// member names involved, so the caller excludes them from the mounts and lock.
/// The engine never auto-solves, so two versions of one package is a hard error,
/// not an advisory.
fn detect_version_conflicts<'a>(
    entries: impl Iterator<Item = (&'a RepoName, &'a str, Option<&'a str>, &'a str)>,
    manifest_path: &Path,
) -> (Vec<Diagnostic>, BTreeSet<RepoName>) {
    let mut by_source: BTreeMap<(String, Option<&str>), (BTreeSet<&str>, BTreeSet<&'a RepoName>)> =
        BTreeMap::new();
    for (name, remote, path, sha) in entries {
        let group = by_source
            .entry((normalize_remote(remote), path))
            .or_default();
        group.0.insert(sha);
        group.1.insert(name);
    }

    let mut diags = Vec::new();
    let mut conflicted: BTreeSet<RepoName> = BTreeSet::new();
    for ((remote, path), (shas, names)) in &by_source {
        if shas.len() < 2 {
            continue;
        }
        let package = match path {
            Some(p) => format!("{remote}/{p}"),
            None => remote.to_string(),
        };
        let shas_list = shas.iter().copied().collect::<Vec<_>>().join(", ");
        let names_list = names
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        diags.push(Diagnostic {
            code: crate::repo::DEPENDENCY_VERSION_CONFLICT,
            severity: Severity::Error,
            span: Span::for_file(manifest_path.to_path_buf()),
            message: format!(
                "package '{package}' is required at {} conflicting versions ({shas_list}); \
                 align the members [{names_list}] to one version",
                shas.len()
            ),
            related: Vec::new(),
            fix: Some(SuggestedFix {
                description: format!("align the members [{names_list}] to one version"),
            }),
        });
        for n in names {
            conflicted.insert((*n).clone());
        }
    }
    (diags, conflicted)
}

/// Detect one dependency NAME resolved to two or more different packages
/// (distinct `(remote, subpath)`): two unrelated repos wearing one name. Returns
/// one `dependency-identity-conflict` per colliding name plus the names, so the
/// caller excludes them from the mounts and lock rather than letting the
/// last-walked edge silently overwrite the earlier one. The identity sibling of
/// [`detect_version_conflicts`] (which catches one package at two shas); a name
/// at two remotes is a different, orthogonal collision the version group misses.
fn detect_identity_conflicts<'a>(
    entries: impl Iterator<Item = (&'a RepoName, &'a str, Option<&'a str>, &'a str)>,
    manifest_path: &Path,
) -> (Vec<Diagnostic>, BTreeSet<RepoName>) {
    // name -> the distinct NORMALIZED packages (remote, path) it resolved to, so
    // one repo spelled two ways under one name is not a false identity conflict.
    let mut by_name: BTreeMap<&'a RepoName, BTreeSet<(String, Option<&str>)>> = BTreeMap::new();
    for (name, remote, path, _sha) in entries {
        by_name
            .entry(name)
            .or_default()
            .insert((normalize_remote(remote), path));
    }

    let mut diags = Vec::new();
    let mut conflicted: BTreeSet<RepoName> = BTreeSet::new();
    for (name, packages) in &by_name {
        if packages.len() < 2 {
            continue;
        }
        let packages_list = packages
            .iter()
            .map(|(remote, path)| match path {
                Some(p) => format!("{remote}/{p}"),
                None => remote.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        diags.push(Diagnostic {
            code: crate::repo::DEPENDENCY_IDENTITY_CONFLICT,
            severity: Severity::Error,
            span: Span::for_file(manifest_path.to_path_buf()),
            message: format!(
                "dependency name '{}' resolves to {} different packages ({packages_list}); \
                 a name must identify one repo, rename or align the sources",
                name.as_str(),
                packages.len()
            ),
            related: Vec::new(),
            fix: Some(SuggestedFix {
                description: format!(
                    "give '{}' a single source, or rename one of the colliding repos",
                    name.as_str()
                ),
            }),
        });
        conflicted.insert((*name).clone());
    }
    (diags, conflicted)
}

/// Load and parse the package registry, fetching it into the cache.
///
/// The registry is a git repo; its `registry.yaml` maps a package name to its
/// remote and audited ref. Read at the registry's default branch.
fn load_registry(
    cache: &PackageCache,
    registry_remote: &str,
) -> Result<BTreeMap<RepoName, RegistryEntry>, GitWriteError> {
    let pkg = cache.ensure(registry_remote, DEFAULT_REGISTRY_REF)?;
    let bytes = std::fs::read(pkg.path.join(REGISTRY_FILE))
        .map_err(|e| GitWriteError::new(format!("registry has no {REGISTRY_FILE}: {e}")))?;
    Ok(crate::repo::parse_package_registry(&bytes))
}

/// The package lock for a set of resolutions: each successfully-resolved
/// dependency pinned to its remote and resolved sha.
///
/// A failed resolution contributes no pin, so the lock records only what
/// resolved. A `conflicted` member is excluded too, so a version conflict is
/// never persisted, the engine never picks a winner. Reads the remote from the
/// member's declaration.
pub(crate) fn lock_from_resolutions(
    resolutions: &[DependencyResolution],
    conflicted: &BTreeSet<RepoName>,
) -> PackageLock {
    let mut lock = PackageLock::new();
    for r in resolutions {
        if conflicted.contains(&r.name) {
            continue;
        }
        let (Ok(pkg), Some(remote)) = (&r.outcome, &r.remote) else {
            continue;
        };
        lock.insert(
            r.name.clone(),
            LockedPackage {
                remote: remote.clone(),
                sha: pkg.sha.clone(),
                path: r.path.clone(),
            },
        );
    }
    lock
}

/// Write a package lock to disk at `lock_path`.
///
/// The engine-schema write half of resolve. A real-disk write, like the cache,
/// not routed through the injected read `FileSystem`. The commit into each
/// editable repo rides the git write path.
pub(crate) fn write_package_lock(lock_path: &Path, lock: &PackageLock) -> std::io::Result<()> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The lock's floor type is its kind, derived from the filename: `repo.lock`
    // is `au.engine.repo-lock`, `workspace.lock` is `au.engine.workspace-lock`.
    let type_name = match lock_path.file_name().and_then(|n| n.to_str()) {
        Some("workspace.lock") => "au.engine.workspace-lock",
        _ => "au.engine.repo-lock",
    };
    // Atomic temp-sibling-then-rename, so a crash mid-write never leaves a repo's
    // `repo.lock` half-written (which would parse to a truncated dependency set).
    crate::repo::atomic_write(
        lock_path,
        &crate::repo::serialize_package_lock(lock, type_name),
    )
}

/// Partition the resolved fetched closure into one lock per EDITABLE member.
///
/// Each editable member (a local working tree, its root not under the cache) gets
/// its OWN transitive fetched closure: a BFS from that repo's declared `deps`,
/// traversing the whole dependency graph (through local members AND fetched deps)
/// and collecting every reachable FETCHED pin. A local member is never pinned, it
/// is resolved by location; it is only traversed to reach the fetched deps behind
/// it. Duplicating a shared fetched dep across two repos' locks is accepted, the
/// price of self-sufficiency (a repo re-opens standalone without depending on any
/// other repo having committed its own lock, the Cargo-per-crate model). See the
/// full-closure decision in [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
///
/// Returns a lock for EVERY editable member, empty when that repo reaches no
/// fetched dep, so the caller can prune a now-stale lock.
pub(crate) fn partition_repo_locks(
    ws: &Workspace,
    resolved: &DependencyResolutions,
    cache: &PackageCache,
    full_lock: &PackageLock,
) -> BTreeMap<RepoName, (PathBuf, PackageLock)> {
    // The editable members: a local working tree, its root NOT under the cache.
    // The `member_paths` map holds the cache mounts by now too, so filter them out.
    let editable_roots: BTreeMap<RepoName, PathBuf> = ws
        .member_paths
        .iter()
        .filter(|(_, root)| !root.starts_with(cache.root()))
        .map(|(n, root)| (n.clone(), root.clone()))
        .collect();

    let mut out: BTreeMap<RepoName, (PathBuf, PackageLock)> = BTreeMap::new();
    for (name, root) in &editable_roots {
        let mut reachable: BTreeSet<RepoName> = BTreeSet::new();
        let mut visited: BTreeSet<RepoName> = BTreeSet::new();
        // Seed with this repo's OWN declared deps, then walk the closure.
        let mut queue: Vec<RepoName> = read_repo_peers(root).into_iter().map(|e| e.name).collect();
        while let Some(dep) = queue.pop() {
            if !visited.insert(dep.clone()) {
                continue;
            }
            if full_lock.contains_key(&dep) {
                // A fetched pin: record it, and traverse its declared deps.
                reachable.insert(dep.clone());
                if let Some(edges) = resolved.fetched_edges.get(&dep) {
                    queue.extend(edges.iter().cloned());
                }
            } else if let Some(local_root) = editable_roots.get(&dep) {
                // A local member: not pinned (resolved by location), but traversed
                // so a fetched dep reached only through it still lands in the lock.
                queue.extend(read_repo_peers(local_root).into_iter().map(|e| e.name));
            }
            // else: unresolved / unmounted, nothing to pin.
        }
        let lock: PackageLock = reachable
            .iter()
            .filter_map(|n| full_lock.get(n).map(|p| (n.clone(), p.clone())))
            .collect();
        out.insert(name.clone(), (crate::repo::repo_lock_path(root), lock));
    }
    out
}

/// Collect the workspace's `discover` closure into one flat lock: every fetched
/// pin reachable from the `discover` members through the dependency graph.
///
/// A BFS seeded from the `discover` members, traversing fetched edges — and a
/// LOCAL discover member's own declared deps, so a fetched dep reached only
/// through a co-present discover member still pins. Mirrors `partition_repo_locks`
/// but yields ONE lock for the whole discover closure (written to the entry's
/// `.arsumbris/workspace.lock`) rather than one per editable member. A local
/// member is never pinned (resolved by location), only traversed. So an `edit`
/// member stays live while a `discover` member mounts pinned and offline. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub(crate) fn partition_workspace_lock(
    ws: &Workspace,
    resolved: &DependencyResolutions,
    cache: &PackageCache,
    full_lock: &PackageLock,
) -> PackageLock {
    // Local (non-cache) member roots: traversed to reach fetched pins behind them,
    // never themselves pinned.
    let local_roots: BTreeMap<RepoName, PathBuf> = ws
        .member_paths
        .iter()
        .filter(|(_, root)| !root.starts_with(cache.root()))
        .map(|(n, root)| (n.clone(), root.clone()))
        .collect();
    let mut reachable: BTreeSet<RepoName> = BTreeSet::new();
    let mut visited: BTreeSet<RepoName> = BTreeSet::new();
    let mut queue: Vec<RepoName> = ws
        .member_roles
        .iter()
        .filter(|(_, r)| **r == MemberRole::Discover)
        .map(|(n, _)| n.clone())
        .collect();
    while let Some(name) = queue.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if full_lock.contains_key(&name) {
            // A fetched pin: record it, and traverse its declared deps.
            reachable.insert(name.clone());
            if let Some(edges) = resolved.fetched_edges.get(&name) {
                queue.extend(edges.iter().cloned());
            }
        } else if let Some(local_root) = local_roots.get(&name) {
            // A local member (a co-present discover member, or a local dep behind
            // one): not pinned, but traversed so a fetched dep behind it pins.
            queue.extend(read_repo_peers(local_root).into_iter().map(|e| e.name));
        }
        // else: unresolved / unmounted, nothing to pin.
    }
    reachable
        .iter()
        .filter_map(|n| full_lock.get(n).map(|p| (n.clone(), p.clone())))
        .collect()
}

/// Resolve a workspace's dependency closure and write each editable repo's lock.
///
/// Walks the transitive closure (`resolve_dependencies`), partitions the resolved
/// fetched pins into one lock per editable member (`partition_repo_locks`), and
/// writes each. Returns the per-member resolutions (for the resolve frame), the
/// version-conflict diagnostics (for the caller to surface), and the
/// `(repo, lock_path)` set that was written, so the caller commits each.
///
/// The locks are (re)written ONLY when EVERY fetch in the resolve succeeded. A
/// transient fetch failure (an unreachable remote) drops a package from the
/// resolved closure, and overwriting a lock then would prune a still-good pin on a
/// network blip. So on any fetch failure the prior locks are preserved untouched
/// and the failure is surfaced via the returned diagnostics; a dropped pin is
/// never a blip, only a real removal (a SUCCESSFUL resolve to an empty closure).
///
/// A member's lock is written when it is non-empty OR a lock already exists at its
/// path. The second case prunes a stale lock: a repo that dropped its last `deps`
/// entry resolves an empty closure, and overwriting the lock empty stops
/// `locate_locked_dependencies` from re-mounting the removed dependency offline.
/// A removal must take effect, per "a mutation never leaves the graph broken".
/// Overwrite-empty, not delete: the commit step commits only a path that exists,
/// so a deletion would never commit. A write failure aborts before any COMMIT, so
/// the guarantee is COMMIT-level (no partial set is committed), not
/// filesystem-level: an earlier member's `repo.lock` may already be overwritten on
/// disk when a later write fails, and each individual write is atomic (temp +
/// rename, see [`write_package_lock`]), never half-written.
pub(crate) fn resolve_and_lock(
    ws: &mut Workspace,
    cache: &PackageCache,
    registry_remote: &str,
) -> std::io::Result<(
    Vec<DependencyResolution>,
    Vec<Diagnostic>,
    Vec<(RepoName, PathBuf)>,
)> {
    let resolved = resolve_dependencies(ws, cache, registry_remote);
    let mut written: Vec<(RepoName, PathBuf)> = Vec::new();
    // Preserve the prior locks untouched on any fetch failure: a failed fetch drops
    // its package from the closure, so writing would clobber a good pin on a
    // transient network error. Only a fully-fetched resolve (re)writes, so a
    // prune-empty is a real removal, never a blip.
    let all_fetched = resolved.resolutions.iter().all(|r| r.outcome.is_ok());
    if all_fetched {
        // Built once and shared by both partition passes below; each used to rebuild
        // it independently from the same inputs.
        let full_lock = lock_from_resolutions(&resolved.resolutions, &resolved.conflicted);
        let partition = partition_repo_locks(ws, &resolved, cache, &full_lock);
        for (name, (lock_path, lock)) in &partition {
            if !lock.is_empty() || lock_path.exists() {
                write_package_lock(lock_path, lock)?;
                written.push((name.clone(), lock_path.clone()));
            }
        }
        // The workspace lock pins the `discover` closure, beside the entry's
        // `workspace.yaml`. Written only for a folder-repo entry (an entry-only or
        // tree workspace has no discover members). Prune-empty like a repo.lock, so
        // dropping the last discover member overwrites the lock empty and stops the
        // offline locate from re-mounting a removed member. Keyed by the entry repo
        // (role `Entry`), whose git tree holds the `.arsumbris/`.
        // Only write when the entry repo (whose git tree holds `.arsumbris/`) is
        // present, so a workspace with no `Entry` role never leaves an orphaned,
        // uncommitted `workspace.lock` on disk. Gate the WRITE on the entry, not
        // just the staging.
        if crate::build::is_folder_repo_manifest(&ws.manifest_path) {
            if let Some((entry_name, _)) = ws
                .member_roles
                .iter()
                .find(|(_, r)| **r == MemberRole::Entry)
            {
                let ws_lock = partition_workspace_lock(ws, &resolved, cache, &full_lock);
                let ws_lock_path = ws.manifest_path.with_file_name("workspace.lock");
                if !ws_lock.is_empty() || ws_lock_path.exists() {
                    write_package_lock(&ws_lock_path, &ws_lock)?;
                    written.push((entry_name.clone(), ws_lock_path));
                }
            }
        }
    }
    Ok((resolved.resolutions, resolved.diagnostics, written))
}

/// Commit one repo's package lock at `lock_path` into `repo_root`, its enclosing
/// git tree, returning the new commit sha.
///
/// The engine-schema commit half of resolve.
/// Resolve is an installation operation, not a closed mutation primitive, so it
/// uses the low-level git commit primitive directly, per repo, outside the
/// mutation-channel saga. The commit is path-scoped to the lock alone, so a
/// human's unrelated working-tree changes stay put. `mutation_id` rides as the
/// `Mutation-Id` trailer; the caller shares one id across all per-repo lock
/// commits of a resolve, so the whole solve is one auditable, correlated set.
///
/// This is the first engine-schema commit outside the saga.
/// Returns `Ok(None)` when the lock is unchanged at HEAD, the idempotent
/// re-resolve (a warm cache re-writes byte-identical content). That is a no-op,
/// not a failure, so the caller leaves the commit null without an error.
/// `Ok(Some(sha))` is a real commit; `Err` is a genuine git failure.
pub(crate) fn commit_package_lock(
    repo_root: &Path,
    lock_path: &Path,
    repo_name: RepoName,
    mutation_id: &str,
) -> Result<Option<CommitSha>, GitWriteError> {
    let lock_path = lock_path.to_path_buf();
    // The commit primitive is path-scoped relative to the repo root, like the saga.
    let scoped = lock_path
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .unwrap_or(lock_path);
    let paths = [scoped];
    // Nothing to commit when the lock matches HEAD. `git commit` on a clean tree
    // exits non-zero, so without this the no-op would read as a git failure.
    if ShellGit.is_clean(repo_root, &paths)? {
        return Ok(None);
    }
    let message = CommitMessage {
        summary: "Resolve workspace dependencies".to_string(),
        mutation_id: MutationId(mutation_id.to_string()),
        members: vec![repo_name],
        moves: Vec::new(),
        reverts: None,
        attribution: Vec::new(),
    };
    ShellGit.commit(repo_root, &paths, &message).map(Some)
}

/// Locate a workspace's locked dependencies from the cache, offline, and return
/// any version-conflict diagnostics.
///
/// UNIONS every EDITABLE member's own `.arsumbris/repo.lock` (a local working
/// tree, its root not under the cache), and for each pinned dependency present in
/// the cache fills `member_paths` with its snapshot. No network: the reproducible,
/// offline re-open path. A pinned sha absent from the cache leaves that member
/// unlocated, a re-resolve or a loud diagnostic is the caller's concern. Each
/// repo's lock carries its OWN full transitive fetched closure, so a transitive
/// member, absent from `member_roles`, mounts here as a dependency; only a
/// project member is skipped, never relocated. A cache member's own committed
/// lock is NOT read: it is redundant with the editable members' locks and could
/// be stale, so only editable roots contribute.
///
/// A dep pinned at two shas ACROSS two editable members' locks is the
/// cross-repo version conflict, caught by `detect_version_conflicts` over the
/// union; the same dep at one sha in two locks dedups. A resolve-written lock set
/// pins a shared dep to one sha, so the conflict check guards a hand-edited or
/// external lock.
///
/// A LOCAL resolution OVERRIDES the pinned cache snapshot: if the dependency's
/// name is already in `member_paths` (a co-present sibling or a registry path
/// resolved it in `assemble_members`), the local working tree wins and stays
/// editable, the cache snapshot is not mounted, and a `dependency-path-overridden`
/// advisory is emitted at the local root. This runs AFTER the local tiers, so the
/// effective resolution order is sibling → registry → cache.
pub(crate) fn locate_locked_dependencies(
    ws: &mut Workspace,
    cache: &PackageCache,
    fs: &impl FileSystem,
) -> Vec<Diagnostic> {
    // The editable members, snapshotted before any cache mount is inserted below.
    let editable_roots: Vec<PathBuf> = ws
        .member_paths
        .values()
        .filter(|root| !root.starts_with(cache.root()))
        .cloned()
        .collect();
    // Union every editable member's own lock. A member with no lock contributes
    // nothing; a fetched dep shared across two locks at one sha dedups below.
    let mut all_entries: Vec<(RepoName, LockedPackage)> = Vec::new();
    for root in &editable_roots {
        let lock_path = crate::repo::repo_lock_path(root);
        if let Ok(bytes) = fs.read_file(&lock_path) {
            all_entries.extend(crate::repo::parse_package_lock(&bytes));
        }
    }
    // Also union the entry's `workspace.lock` (the `discover` closure), beside its
    // `workspace.yaml`. Same `packages` shape, so its pins join the union and share
    // the conflict / dedup / mount / cache-miss logic below: a `discover` member
    // (not editable) mounts from the cache at its pin, a co-present one overrides
    // with the local tree (`dependency-path-overridden`), an absent pin surfaces as
    // `discover-member-unmounted` (a member) via the consistency loop.
    if crate::build::is_folder_repo_manifest(&ws.manifest_path) {
        let ws_lock_path = ws.manifest_path.with_file_name("workspace.lock");
        if let Ok(bytes) = fs.read_file(&ws_lock_path) {
            all_entries.extend(crate::repo::parse_package_lock(&bytes));
        }
    }
    if all_entries.is_empty() {
        return Vec::new();
    }
    let entry_view = || {
        all_entries.iter().map(|(name, pkg)| {
            (
                name,
                pkg.remote.as_str(),
                pkg.path.as_deref(),
                pkg.sha.as_str(),
            )
        })
    };
    let (mut diagnostics, mut conflicted) =
        detect_version_conflicts(entry_view(), &ws.manifest_path);
    // Also run the IDENTITY detector over the union, exactly as the single-lock
    // `resolve_dependencies` path does. Two per-repo locks may pin one dep NAME to
    // two DIFFERENT remotes (legal under same-name-coexistence); version-conflict
    // groups by (remote, path), so it never sees them, and the first-seen dedup
    // below would silently mount one repo's package for the other. Merging the
    // identity-conflicted names into the skip set excludes them from the mounts.
    let (identity_diags, identity_conflicted) =
        detect_identity_conflicts(entry_view(), &ws.manifest_path);
    diagnostics.extend(identity_diags);
    conflicted.extend(identity_conflicted);
    // Dedup the union into one pin per name (same-sha entries agree; a two-sha
    // name is `conflicted` and skipped below). Sorted, deterministic.
    let mut lock: PackageLock = PackageLock::new();
    for (name, pkg) in all_entries {
        lock.entry(name).or_insert(pkg);
    }
    for (name, pkg) in &lock {
        if conflicted.contains(name) {
            continue; // never mount a conflicted package.
        }
        // Skip an editable member (the entry or an `edit` member) that ALREADY
        // resolved to a local working tree: it is editable, never relocated onto a
        // cache snapshot. But an UNMOUNTED editable member pinned in the lock DOES
        // mount read-only from the cache, the `edit-member-read-only` case: it falls
        // through here, so its `member_path` lands under the cache root and the
        // consistency pass surfaces it. A consumed member (a `dep` or `discover`)
        // is not editable, so it mounts regardless.
        if ws.member_roles.get(name).is_some_and(|r| r.editable())
            && ws.member_paths.contains_key(name)
        {
            continue;
        }
        // A local resolution overrides the cache snapshot: the dependency already
        // resolved to a working tree in `member_paths` (a co-present sibling or a
        // registry path), so mount the editable local tree, not the pinned
        // snapshot. Surface it so a forgotten override does not read as stale.
        if let Some(local) = ws.member_paths.get(name) {
            diagnostics.push(Diagnostic {
                code: crate::repo::DEPENDENCY_PATH_OVERRIDDEN,
                severity: Severity::Hint,
                span: Span::for_file(local.clone()),
                message: format!(
                    "dependency '{}' is served from a local working tree at {}, not its locked \
                     snapshot at {}; edits to the local tree take effect, remove the local \
                     resolution (unregister its path or move the co-present sibling) to return \
                     to the pinned dependency",
                    name.as_str(),
                    local.display(),
                    pkg.sha
                ),
                related: Vec::new(),
                fix: None,
            });
            continue;
        }
        // Mount the subtree `<snapshot>/<path>`, the snapshot root when no path.
        // A hand-edited or hostile lock can carry an escaping subpath, so the
        // containment guard runs here too (a resolve-written lock already
        // excludes one).
        if let Some(snapshot) = cache.lookup(&pkg.sha) {
            match member_path(&snapshot, &pkg.path) {
                Some(located) => {
                    ws.member_paths.insert(name.clone(), located);
                    ws.member_roles
                        .entry(name.clone())
                        .or_insert(MemberRole::Dep);
                }
                None => {
                    diagnostics.push(dependency_path_escapes(
                        name,
                        pkg.path.as_deref(),
                        &ws.manifest_path,
                    ));
                }
            }
        } else if !ws.members.iter().any(|m| &m.name == name) {
            // A transitive locked dependency (not a manifest member) whose
            // snapshot is absent from the cache would otherwise be silently
            // unmounted, surfacing only as a later unresolved `::repo`. A manifest
            // member is covered by the member-consistency loop (a role-keyed
            // `edit-member-unmounted` / `discover-member-unmounted` / `peer-unmounted`),
            // so this fires only for a transitive dep.
            diagnostics.push(Diagnostic {
                code: crate::repo::DEPENDENCY_CACHE_MISS,
                severity: Severity::Warning,
                span: Span::for_file(ws.manifest_path.clone()),
                message: format!(
                    "transitive dependency '{}' is locked at {} but its snapshot is not in \
                     the device cache; run resolve to fetch it",
                    name.as_str(),
                    pkg.sha
                ),
                related: Vec::new(),
                fix: Some(SuggestedFix {
                    description: format!(
                        "run resolve to fetch '{}' at its locked commit into the device cache",
                        name.as_str()
                    ),
                }),
            });
        }
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::RepoEntry;
    use au_parser::RealFileSystem;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn run(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A source repo with a nested file, standing in for a dependency remote.
    fn source_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        fs::write(p.join("readme.md"), "the package\n").unwrap();
        fs::create_dir_all(p.join("type")).unwrap();
        fs::write(p.join("type/thing.type.yaml"), "fields: {}\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "v1"]);
        let sha = run(p, &["rev-parse", "HEAD"]);
        (dir, sha)
    }

    fn cache() -> (TempDir, PackageCache) {
        let dir = TempDir::new().unwrap();
        let cache = PackageCache::new(dir.path().join("packages"));
        (dir, cache)
    }

    /// A co-present folder-repo `<parent>/proj/` declaring `dep` as a remote
    /// dependency in its own `repo.yaml`, and carrying a `.arsumbris/workspace.yaml`
    /// selecting itself. `proj` is the entry repo; its per-repo lock lands in its
    /// own `.arsumbris/`. Returns the manifest path.
    fn proj_with_remote_dep(parent: &Path, remote: &str) -> PathBuf {
        let proj = parent.join("proj");
        fs::create_dir_all(proj.join(".arsumbris")).unwrap();
        fs::write(
            proj.join(".arsumbris/repo.yaml"),
            format!("name: proj\ndeps:\n  - name: dep\n    remote: {remote}\n    ref: main\n"),
        )
        .unwrap();
        let manifest = proj.join(".arsumbris/workspace.yaml");
        fs::write(&manifest, "edit:\n  - proj\n").unwrap();
        manifest
    }

    /// Write a co-present editable primary `<ws>/proj/` declaring `deps` (each a
    /// `(name, remote)` at `ref: main`) in its own `repo.yaml`. The per-repo lock
    /// lands at `proj/.arsumbris/repo.lock`.
    fn write_proj(ws: &Path, deps: &[(&str, &str)]) -> PathBuf {
        let proj = ws.join("proj");
        fs::create_dir_all(proj.join(".arsumbris")).unwrap();
        let mut yaml = String::from("name: proj\n");
        if !deps.is_empty() {
            yaml.push_str("deps:\n");
            for (name, remote) in deps {
                yaml.push_str(&format!(
                    "  - name: {name}\n    remote: {remote}\n    ref: main\n"
                ));
            }
        }
        fs::write(proj.join(".arsumbris/repo.yaml"), yaml).unwrap();
        proj
    }

    /// A workspace over a co-present folder-repo primary `proj/` declaring `deps`,
    /// assembled so `proj` is the editable entry (in `member_paths`). The manifest
    /// sits at `proj/.arsumbris/workspace.yaml`. This is the per-repo-lock model:
    /// the lock lands in `proj/.arsumbris/repo.lock`, beside the manifest. Returns
    /// the ws tempdir, the manifest path, and the loaded Workspace.
    fn proj_workspace(deps: &[(&str, &str)]) -> (TempDir, PathBuf, Workspace) {
        let ws_dir = TempDir::new().unwrap();
        let proj = write_proj(ws_dir.path(), deps);
        let manifest = proj.join(".arsumbris/workspace.yaml");
        fs::write(&manifest, "edit:\n  - proj\n").unwrap();
        let ws = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[proj],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        (ws_dir, manifest, ws)
    }

    #[test]
    fn ensure_fetches_and_extracts_a_content_addressed_snapshot() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();

        let pkg = cache.ensure(&src.path().to_string_lossy(), "main").unwrap();

        assert_eq!(pkg.sha, sha, "snapshot keyed by the resolved sha");
        assert!(pkg.path.ends_with(&sha), "snapshot dir named by the sha");
        // The whole tree is materialized, nested paths included.
        assert_eq!(
            fs::read_to_string(pkg.path.join("readme.md")).unwrap(),
            "the package\n"
        );
        assert_eq!(
            fs::read_to_string(pkg.path.join("type/thing.type.yaml")).unwrap(),
            "fields: {}\n"
        );
        // No `.git`: a plain immutable file tree, not a working clone.
        assert!(
            !pkg.path.join(".git").exists(),
            "snapshot must carry no .git"
        );
    }

    #[test]
    fn ensure_is_idempotent_and_lookup_finds_the_snapshot() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();
        let remote = src.path().to_string_lossy().into_owned();

        let first = cache.ensure(&remote, "main").unwrap();
        // A second resolve returns the same snapshot, no second extraction.
        let second = cache.ensure(&remote, "main").unwrap();
        assert_eq!(first, second);

        // The offline path: the sha resolves from the cache with no fetch.
        assert_eq!(cache.lookup(&sha), Some(first.path));
        assert_eq!(
            cache.lookup("0000000000000000000000000000000000000000"),
            None
        );
    }

    #[test]
    fn no_scratch_survives_a_successful_ensure() {
        let (src, _sha) = source_repo();
        let (_cache_dir, cache) = cache();
        cache.ensure(&src.path().to_string_lossy(), "main").unwrap();
        // The `.tmp` scratch is cleaned, only the sha snapshot remains.
        let tmp = cache.root.join(".tmp");
        let leftover = tmp.read_dir().map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(leftover, 0, "temp scratch should be cleaned up");
    }

    #[test]
    fn a_bad_ref_errors_and_publishes_nothing() {
        let (src, _sha) = source_repo();
        let (_cache_dir, cache) = cache();
        let err = cache.ensure(&src.path().to_string_lossy(), "no-such-ref");
        assert!(err.is_err(), "an unfetchable ref must surface an error");
        // No snapshot was published.
        let published = cache
            .root
            .read_dir()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name() != ".tmp")
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(published, 0, "a failed resolve must leave no snapshot");
    }

    fn dep_workspace(
        name: &str,
        remote: &Path,
        git_ref: &str,
        manifest_path: PathBuf,
    ) -> Workspace {
        let mut member_roles = BTreeMap::new();
        member_roles.insert(RepoName(name.to_string()), MemberRole::Dep);
        Workspace {
            manifest_path,
            name: "demo".to_string(),
            members: vec![RepoEntry {
                name: RepoName(name.to_string()),
                description: None,
                remote: Some(remote.to_string_lossy().into_owned()),
                git_ref: Some(git_ref.to_string()),
                path: None,
            }],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles,
        }
    }

    /// A source repo carrying a type-def AND a repo-registry declaring `self`
    /// plus the given peers `(name, remote, ref)`, its transitive dependency
    /// record. The transitive resolver reads these from the fetched snapshot.
    fn repo_with_peers(self_name: &str, peers: &[(&str, &str, &str)]) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        fs::create_dir_all(p.join("type")).unwrap();
        fs::write(
            p.join(format!("type/{self_name}.type.yaml")),
            "fields: {}\n",
        )
        .unwrap();
        fs::create_dir_all(p.join(".arsumbris")).unwrap();
        let mut yaml = format!("name: {self_name}\n");
        if !peers.is_empty() {
            yaml.push_str("deps:\n");
            for (name, remote, git_ref) in peers {
                yaml.push_str(&format!(
                    "  - name: {name}\n    remote: {remote}\n    ref: {git_ref}\n"
                ));
            }
        }
        fs::write(p.join(".arsumbris/repo.yaml"), yaml).unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "v1"]);
        let sha = run(p, &["rev-parse", "HEAD"]);
        (dir, sha)
    }

    #[test]
    fn transitive_resolution_mounts_a_dependencys_peers() {
        let (leaf, leaf_sha) = source_repo();
        // `mid` declares `leaf` as a peer, its own transitive dependency.
        let (mid, _) = repo_with_peers("mid", &[("leaf", &leaf.path().to_string_lossy(), "main")]);
        let (_cache_dir, cache) = cache();
        let mut ws = dep_workspace(
            "mid",
            mid.path(),
            "main",
            PathBuf::from("/ws/.arsumbris/workspace.yaml"),
        );

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        assert!(resolved.diagnostics.is_empty(), "no conflict");
        assert!(ws.member_paths.contains_key(&RepoName("mid".into())));
        let leaf_mount = ws
            .member_paths
            .get(&RepoName("leaf".into()))
            .expect("the transitive peer mounted");
        assert_eq!(leaf_mount, &cache.lookup(&leaf_sha).unwrap());
        // The discovered transitive member is recorded with dependency provenance.
        assert_eq!(
            ws.member_roles.get(&RepoName("leaf".into())),
            Some(&MemberRole::Dep)
        );
    }

    #[test]
    fn transitive_walk_is_cycle_safe() {
        // A peers B, B peers A. The walk must terminate, mounting both.
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let setup = |p: &Path, self_name: &str, peer: &str, peer_remote: &Path| {
            run(p, &["init", "-q", "-b", "main"]);
            run(p, &["config", "user.name", "Tester"]);
            run(p, &["config", "user.email", "tester@example.com"]);
            fs::create_dir_all(p.join("type")).unwrap();
            fs::write(
                p.join(format!("type/{self_name}.type.yaml")),
                "fields: {}\n",
            )
            .unwrap();
            fs::create_dir_all(p.join(".arsumbris")).unwrap();
            fs::write(
                p.join(".arsumbris/repo.yaml"),
                format!(
                    "name: {self_name}\ndeps:\n  - name: {peer}\n    remote: {}\n    ref: main\n",
                    peer_remote.to_string_lossy()
                ),
            )
            .unwrap();
            run(p, &["add", "-A"]);
            run(p, &["commit", "-q", "-m", "v1"]);
        };
        setup(dir_a.path(), "a", "b", dir_b.path());
        setup(dir_b.path(), "b", "a", dir_a.path());
        let (_cache_dir, cache) = cache();
        let mut ws = dep_workspace(
            "a",
            dir_a.path(),
            "main",
            PathBuf::from("/ws/.arsumbris/workspace.yaml"),
        );

        // Terminates (the spec-dedup guard halts the cycle) and mounts both.
        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);
        assert!(resolved.diagnostics.is_empty());
        assert!(ws.member_paths.contains_key(&RepoName("a".into())));
        assert!(ws.member_paths.contains_key(&RepoName("b".into())));
    }

    #[test]
    fn a_failed_fetch_preserves_the_prior_lock_instead_of_clobbering_it() {
        // `app` is an editable member whose dep points at an UNREACHABLE remote, so
        // its fetch fails. A prior good repo.lock exists on disk. The failed resolve
        // must NOT overwrite it: a transient network error must not drop a good pin.
        let (app, _) = repo_with_peers("app", &[("gone", "/no/such/remote.git", "main")]);
        let lock_path = app.path().join(".arsumbris/repo.lock");
        let prior = "packages:\n  - name: gone\n    remote: /no/such/remote.git\n    sha: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";
        fs::write(&lock_path, prior).unwrap();
        let (_cache_dir, cache) = cache();

        let mut roles = BTreeMap::new();
        roles.insert(RepoName("app".into()), MemberRole::Edit);
        let mut member_paths = BTreeMap::new();
        member_paths.insert(RepoName("app".into()), app.path().to_path_buf());
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "ws".into(),
            members: vec![RepoEntry {
                name: RepoName("app".into()),
                description: None,
                remote: None,
                git_ref: None,
                path: None,
            }],
            member_paths,
            edit: vec![RepoName("app".into())],
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: roles,
        };

        let (res, _diags, written) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert!(
            res.iter().any(|r| r.outcome.is_err()),
            "the unreachable remote's fetch failed"
        );
        assert!(written.is_empty(), "a failed resolve stages no lock");
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            prior,
            "the prior lock is preserved, not clobbered empty"
        );
    }

    #[test]
    fn two_local_edit_members_disagreeing_on_a_dep_ref_conflict() {
        // A leaf repo with two tagged versions.
        let leaf = TempDir::new().unwrap();
        let lp = leaf.path();
        run(lp, &["init", "-q", "-b", "main"]);
        run(lp, &["config", "user.name", "Tester"]);
        run(lp, &["config", "user.email", "tester@example.com"]);
        fs::write(lp.join("readme.md"), "v1\n").unwrap();
        run(lp, &["add", "-A"]);
        run(lp, &["commit", "-q", "-m", "v1"]);
        run(lp, &["tag", "v1"]);
        fs::write(lp.join("readme.md"), "v2\n").unwrap();
        run(lp, &["add", "-A"]);
        run(lp, &["commit", "-q", "-m", "v2"]);
        run(lp, &["tag", "v2"]);

        // Two co-present EDITABLE members, each declaring `leaf` at a DIFFERENT
        // ref. `assemble_members` collapses them to one edge in `ws.members`, so
        // the raw dep edges of the local members must both reach the conflict solve.
        let lref = lp.to_string_lossy();
        let (app, _) = repo_with_peers("app", &[("leaf", &lref, "v1")]);
        let (base, _) = repo_with_peers("base", &[("leaf", &lref, "v2")]);
        let (_cache_dir, cache) = cache();

        let mut roles = BTreeMap::new();
        roles.insert(RepoName("app".into()), MemberRole::Edit);
        roles.insert(RepoName("base".into()), MemberRole::Edit);
        // `leaf` is the SINGLE collapsed edge assemble_members would produce (v1,
        // first-seen); the divergent v2 edge lives only in base's raw `deps`.
        roles.insert(RepoName("leaf".into()), MemberRole::Dep);
        let mut member_paths = BTreeMap::new();
        member_paths.insert(RepoName("app".into()), app.path().to_path_buf());
        member_paths.insert(RepoName("base".into()), base.path().to_path_buf());
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "ws".into(),
            members: vec![
                RepoEntry {
                    name: RepoName("app".into()),
                    description: None,
                    remote: None,
                    git_ref: None,
                    path: None,
                },
                RepoEntry {
                    name: RepoName("base".into()),
                    description: None,
                    remote: None,
                    git_ref: None,
                    path: None,
                },
                RepoEntry {
                    name: RepoName("leaf".into()),
                    description: None,
                    remote: Some(lref.clone().into_owned()),
                    git_ref: Some("v1".into()),
                    path: None,
                },
            ],
            member_paths,
            edit: vec![RepoName("app".into()), RepoName("base".into())],
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: roles,
        };

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_VERSION_CONFLICT),
            "two local members disagreeing on leaf's ref must conflict, got {:?}",
            resolved
                .diagnostics
                .iter()
                .map(|d| &d.code)
                .collect::<Vec<_>>()
        );
        assert!(
            resolved.conflicted.contains(&RepoName("leaf".into())),
            "the disagreed package is conflicted"
        );
        assert!(
            !ws.member_paths.contains_key(&RepoName("leaf".into())),
            "a conflicted package is never mounted"
        );
    }

    #[test]
    fn version_conflict_when_two_edges_require_different_shas() {
        // A leaf repo with two tagged versions.
        let leaf = TempDir::new().unwrap();
        let lp = leaf.path();
        run(lp, &["init", "-q", "-b", "main"]);
        run(lp, &["config", "user.name", "Tester"]);
        run(lp, &["config", "user.email", "tester@example.com"]);
        fs::write(lp.join("readme.md"), "v1\n").unwrap();
        run(lp, &["add", "-A"]);
        run(lp, &["commit", "-q", "-m", "v1"]);
        let sha1 = run(lp, &["rev-parse", "HEAD"]);
        run(lp, &["tag", "v1"]);
        fs::write(lp.join("readme.md"), "v2\n").unwrap();
        run(lp, &["add", "-A"]);
        run(lp, &["commit", "-q", "-m", "v2"]);
        let sha2 = run(lp, &["rev-parse", "HEAD"]);
        assert_ne!(sha1, sha2);

        // `mid` peers leaf at `main` (sha2); the workspace also depends on leaf at
        // `v1` (sha1). Two edges, same package, two versions.
        let (mid, _) = repo_with_peers("mid", &[("leaf", &lp.to_string_lossy(), "main")]);
        let (_cache_dir, cache) = cache();
        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("leaf".into()), MemberRole::Dep);
        provenance.insert(RepoName("mid".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![
                RepoEntry {
                    name: RepoName("leaf".into()),
                    description: None,
                    remote: Some(lp.to_string_lossy().into_owned()),
                    git_ref: Some("v1".into()),
                    path: None,
                },
                RepoEntry {
                    name: RepoName("mid".into()),
                    description: None,
                    remote: Some(mid.path().to_string_lossy().into_owned()),
                    git_ref: Some("main".into()),
                    path: None,
                },
            ],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        // leaf is required at two shas: a hard conflict, the engine picks no winner.
        assert_eq!(resolved.diagnostics.len(), 1, "one conflicted package");
        assert_eq!(
            resolved.diagnostics[0].code,
            crate::repo::DEPENDENCY_VERSION_CONFLICT
        );
        assert!(resolved.conflicted.contains(&RepoName("leaf".into())));
        // The conflicted package is unmounted; the non-conflicted `mid` mounts.
        assert!(
            !ws.member_paths.contains_key(&RepoName("leaf".into())),
            "a conflicted package is never mounted"
        );
        assert!(ws.member_paths.contains_key(&RepoName("mid".into())));
        // And excluded from the lock, so a conflict is never persisted.
        let lock = lock_from_resolutions(&resolved.resolutions, &resolved.conflicted);
        assert!(!lock.contains_key(&RepoName("leaf".into())));
        assert!(lock.contains_key(&RepoName("mid".into())));
    }

    #[test]
    fn a_transitive_locked_dependency_absent_from_the_cache_is_flagged() {
        // A locked dependency whose snapshot is missing from the device cache
        // must not be silently unmounted; it surfaces an advisory (else it
        // appears only as a later unresolved `::repo`). ws.members is empty, so
        // the dep is transitive (a manifest member is covered elsewhere).
        let (_cache_dir, cache) = cache();
        let ws_dir = TempDir::new().unwrap();
        let proj = ws_dir.path().join("proj");
        fs::create_dir_all(proj.join(".arsumbris")).unwrap();
        // The editable member `proj`'s own lock pins a transitive dep at an
        // uncached sha.
        let mut lock = PackageLock::new();
        lock.insert(
            RepoName("dep".into()),
            locked("https://example.invalid/dep", "sha-not-cached", None),
        );
        write_package_lock(&crate::repo::repo_lock_path(&proj), &lock).unwrap();
        let mut member_paths = BTreeMap::new();
        member_paths.insert(RepoName("proj".into()), proj);
        let mut ws = Workspace {
            manifest_path: ws_dir.path().join(".arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![],
            member_paths,
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: BTreeMap::new(),
        };

        let diags = locate_locked_dependencies(&mut ws, &cache, &RealFileSystem);

        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_CACHE_MISS),
            "a transitive locked dep missing from the cache is flagged"
        );
        assert!(
            !ws.member_paths.contains_key(&RepoName("dep".into())),
            "the absent dependency is not mounted"
        );
    }

    #[test]
    fn normalize_remote_collapses_spelling_variants() {
        let canon = "github.com/org/x";
        for r in [
            "git@github.com:org/x.git",
            "https://github.com/org/x",
            "https://github.com/org/x.git",
            "https://github.com/org/x/",
            "ssh://git@github.com/org/x",
            "  git@github.com:org/x.git  ",
        ] {
            assert_eq!(normalize_remote(r), canon, "spelling {r:?}");
        }
        // Distinct repos stay distinct, no false merge.
        assert_ne!(
            normalize_remote("github.com/org/x"),
            normalize_remote("github.com/org/y")
        );
    }

    #[test]
    fn version_conflict_fires_across_remote_spellings() {
        // The same upstream spelled two ways at two shas is one package at two
        // versions, a conflict, not two silently-mounted independent edges.
        let a = RepoName("a".into());
        let b = RepoName("b".into());
        let entries: Vec<(&RepoName, &str, Option<&str>, &str)> = vec![
            (&a, "git@github.com:org/x.git", None, "sha1"),
            (&b, "https://github.com/org/x", None, "sha2"),
        ];
        let (diags, conflicted) = detect_version_conflicts(
            entries.into_iter(),
            Path::new("/ws/.arsumbris/workspace.yaml"),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_VERSION_CONFLICT),
            "two spellings of one remote at two shas is a version conflict"
        );
        assert!(conflicted.contains(&a) && conflicted.contains(&b));
    }

    #[test]
    fn a_published_snapshot_is_read_only() {
        // The content-addressed cache is deterred from accidental mutation: a
        // published snapshot's files are read-only (deterrence, not integrity).
        let (leaf, _) = repo_with_peers("leaf", &[]);
        let (_cache_dir, cache) = cache();
        let pkg = cache
            .ensure(&leaf.path().to_string_lossy(), "main")
            .expect("fetch and publish");
        let file = pkg.path.join(".arsumbris/repo.yaml");
        let perms = std::fs::metadata(&file).unwrap().permissions();
        assert!(
            perms.readonly(),
            "a published snapshot file must be read-only"
        );
    }

    #[test]
    fn is_safe_subpath_rejects_absolute_and_escaping() {
        // Safe: relative, staying within the snapshot root.
        assert!(is_safe_subpath("sub"));
        assert!(is_safe_subpath("sub/pkg"));
        assert!(is_safe_subpath("./sub"));
        assert!(is_safe_subpath("a/b/../c"));
        // Unsafe: absolute, or a net climb above the root.
        assert!(!is_safe_subpath("/etc"));
        assert!(!is_safe_subpath("../etc"));
        assert!(!is_safe_subpath("../../home/user/.ssh"));
        assert!(!is_safe_subpath("a/../../b"));
    }

    #[test]
    fn member_path_refuses_an_escaping_subpath() {
        let snap = Path::new("/cache/deadbeef");
        assert_eq!(member_path(snap, &None), Some(snap.to_path_buf()));
        assert_eq!(
            member_path(snap, &Some("pkg".into())),
            Some(snap.join("pkg"))
        );
        assert_eq!(member_path(snap, &Some("../../etc".into())), None);
        assert_eq!(member_path(snap, &Some("/etc".into())), None);
    }

    #[test]
    fn an_escaping_dependency_subpath_is_refused_and_not_mounted_or_locked() {
        // A dependency whose declared subpath climbs out of its cache snapshot
        // (`path: ../../etc`) must be refused: never mounted, never locked. The
        // subpath is untrusted (a transitive peer's comes from a fetched
        // registry), so an unchecked `snapshot.join(path)` would mount an
        // arbitrary local directory.
        let (leaf, _) = repo_with_peers("leaf", &[]);
        let (_cache_dir, cache) = cache();
        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("leaf".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![RepoEntry {
                name: RepoName("leaf".into()),
                description: None,
                remote: Some(leaf.path().to_string_lossy().into_owned()),
                git_ref: Some("main".into()),
                path: Some("../../etc".into()),
            }],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_PATH_ESCAPES_SNAPSHOT),
            "an escaping subpath is flagged"
        );
        assert!(
            !ws.member_paths.contains_key(&RepoName("leaf".into())),
            "an escaping dependency is never mounted"
        );
        let lock = lock_from_resolutions(&resolved.resolutions, &resolved.conflicted);
        assert!(
            !lock.contains_key(&RepoName("leaf".into())),
            "an escaping subpath is never persisted into the lock"
        );
    }

    #[test]
    fn identity_conflict_when_one_name_resolves_to_two_remotes() {
        // Two DIFFERENT repos, both peered under the name "cee" from different
        // remotes: an identity collision, not a version conflict. Without the
        // check, the last-walked edge silently overwrites the earlier one in the
        // mounts map. The engine must instead flag it and leave "cee" unmounted.
        let (cee_a, _) = repo_with_peers("cee", &[]);
        let (cee_b, _) = repo_with_peers("cee", &[]);
        let cee_a_remote = cee_a.path().to_string_lossy().into_owned();
        let cee_b_remote = cee_b.path().to_string_lossy().into_owned();
        let (a, _) = repo_with_peers("a", &[("cee", &cee_a_remote, "main")]);
        let (b, _) = repo_with_peers("b", &[("cee", &cee_b_remote, "main")]);
        let (_cache_dir, cache) = cache();
        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("a".into()), MemberRole::Dep);
        provenance.insert(RepoName("b".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![
                RepoEntry {
                    name: RepoName("a".into()),
                    description: None,
                    remote: Some(a.path().to_string_lossy().into_owned()),
                    git_ref: Some("main".into()),
                    path: None,
                },
                RepoEntry {
                    name: RepoName("b".into()),
                    description: None,
                    remote: Some(b.path().to_string_lossy().into_owned()),
                    git_ref: Some("main".into()),
                    path: None,
                },
            ],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_IDENTITY_CONFLICT),
            "one name from two remotes is a hard identity conflict"
        );
        assert!(
            resolved.conflicted.contains(&RepoName("cee".into())),
            "cee is conflicted"
        );
        assert!(
            !ws.member_paths.contains_key(&RepoName("cee".into())),
            "a conflicted name is never silently mounted"
        );
    }

    #[test]
    fn a_project_member_shadows_a_transitive_edge() {
        // `mid` declares `leaf` as a peer, but `leaf` is a local project member.
        // The walk must not fetch it: the local copy shadows the transitive edge.
        let (mid, _) = repo_with_peers("mid", &[("leaf", "/no/such/remote", "main")]);
        let (_cache_dir, cache) = cache();
        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("leaf".into()), MemberRole::Edit);
        provenance.insert(RepoName("mid".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![
                RepoEntry {
                    name: RepoName("leaf".into()),
                    description: None,
                    remote: None,
                    git_ref: None,
                    path: None,
                },
                RepoEntry {
                    name: RepoName("mid".into()),
                    description: None,
                    remote: Some(mid.path().to_string_lossy().into_owned()),
                    git_ref: Some("main".into()),
                    path: None,
                },
            ],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        let resolved = resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE);

        // `mid` resolved; `leaf` was shadowed, never fetched (had it been fetched
        // from `/no/such/remote`, it would appear as a failed resolution).
        assert!(resolved
            .resolutions
            .iter()
            .any(|r| r.name == RepoName("mid".into())));
        assert!(
            !resolved
                .resolutions
                .iter()
                .any(|r| r.name == RepoName("leaf".into())),
            "a project member is never fetched as a transitive edge"
        );
        assert!(!ws.member_paths.contains_key(&RepoName("leaf".into())));
        // `leaf` keeps its project provenance, not overwritten to dependency.
        assert_eq!(
            ws.member_roles.get(&RepoName("leaf".into())),
            Some(&MemberRole::Edit)
        );
    }

    #[test]
    fn an_empty_closure_prunes_a_stale_lock() {
        let (src, _sha) = source_repo();
        let (_cache_dir, cache) = cache();

        // First resolve: proj declares one dependency, its own lock is written.
        let (ws_dir, manifest, mut ws) = proj_workspace(&[("dep", &src.path().to_string_lossy())]);
        let lock_path = crate::repo::repo_lock_path(&ws_dir.path().join("proj"));
        resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert!(
            crate::repo::parse_package_lock(&fs::read(&lock_path).unwrap())
                .contains_key(&RepoName("dep".into()))
        );

        // The dependency is dropped: rewrite proj's `repo.yaml` with no deps and
        // reload, so its resolved closure is empty.
        write_proj(ws_dir.path(), &[]);
        let reload = || {
            crate::repo::load_workspace(
                &manifest,
                &fs::read(&manifest).unwrap(),
                &[ws_dir.path().join("proj")],
                &crate::repo::UserRegistry::new(),
                &RealFileSystem,
            )
        };
        let mut emptied = reload();
        resolve_and_lock(&mut emptied, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();

        // The stale lock is pruned to empty (overwritten, not left behind, its path
        // already existed), so an offline locate re-mounts nothing. A removal takes
        // effect.
        assert!(
            crate::repo::parse_package_lock(&fs::read(&lock_path).unwrap()).is_empty(),
            "the stale lock is pruned to empty"
        );
        let mut reopened = reload();
        locate_locked_dependencies(&mut reopened, &cache, &RealFileSystem);
        assert!(
            !reopened.member_paths.contains_key(&RepoName("dep".into())),
            "the removed dependency no longer mounts"
        );
    }

    #[test]
    fn resolve_locates_an_explicit_remote_dependency_at_its_snapshot() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();
        let mut ws = dep_workspace(
            "dep",
            src.path(),
            "main",
            PathBuf::from("/ws/.arsumbris/workspace.yaml"),
        );

        let resolutions =
            resolve_dependencies(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).resolutions;

        assert_eq!(resolutions.len(), 1);
        let r = &resolutions[0];
        assert_eq!(r.name, RepoName("dep".to_string()));
        let pkg = r.outcome.as_ref().expect("dependency resolves");
        assert_eq!(pkg.sha, sha);

        // The member is now located at its immutable snapshot, the assembly walk
        // mounts it from here like any other member.
        let path = ws.member_paths.get(&RepoName("dep".to_string())).unwrap();
        assert_eq!(path, &pkg.path);
        assert_eq!(
            fs::read_to_string(path.join("type/thing.type.yaml")).unwrap(),
            "fields: {}\n"
        );
    }

    #[test]
    fn resolve_skips_projects_and_fails_a_name_only_dep_with_no_registry() {
        let (src, _sha) = source_repo();
        let (_cache_dir, cache) = cache();
        let remote = src.path().to_string_lossy().into_owned();

        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("proj".into()), MemberRole::Edit);
        provenance.insert(RepoName("named".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![
                // A project member that happens to carry a remote and ref: never fetched.
                RepoEntry {
                    name: RepoName("proj".into()),
                    description: None,
                    remote: Some(remote.clone()),
                    git_ref: Some("main".into()),
                    path: None,
                },
                // A dependency declared by name only: resolved via the registry.
                RepoEntry {
                    name: RepoName("named".into()),
                    description: None,
                    remote: None,
                    git_ref: None,
                    path: None,
                },
            ],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        // The project member is skipped; the name-only dependency attempts the
        // registry and fails loudly when it is unreachable.
        let resolutions = resolve_dependencies(&mut ws, &cache, "/no/such/registry").resolutions;
        assert_eq!(
            resolutions.len(),
            1,
            "only the name-only dependency is attempted"
        );
        let r = &resolutions[0];
        assert_eq!(r.name, RepoName("named".into()));
        assert!(
            r.outcome.is_err(),
            "an unreachable registry is a failed resolution"
        );
        assert!(r.remote.is_none(), "no remote was learned");
        assert!(ws.member_paths.is_empty(), "no member located");
    }

    /// A registry repo holding the given `(name, remote, ref, path?)` entries.
    fn registry_repo(entries: &[(&str, &str, &str, Option<&str>)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        let mut yaml = String::from("packages:\n");
        for (name, remote, git_ref, path) in entries {
            yaml.push_str(&format!(
                "  - name: {name}\n    remote: {remote}\n    ref: {git_ref}\n"
            ));
            if let Some(path) = path {
                yaml.push_str(&format!("    path: {path}\n"));
            }
        }
        fs::write(p.join("registry.yaml"), yaml).unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "registry"]);
        dir
    }

    /// A monorepo source with two packages, each at its own subpath.
    fn monorepo_source() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        fs::create_dir_all(p.join("pkgs/a/type")).unwrap();
        fs::write(p.join("pkgs/a/type/a.type.yaml"), "fields: {}\n").unwrap();
        fs::create_dir_all(p.join("pkgs/b/type")).unwrap();
        fs::write(p.join("pkgs/b/type/b.type.yaml"), "fields: {}\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "monorepo"]);
        let sha = run(p, &["rev-parse", "HEAD"]);
        (dir, sha)
    }

    #[test]
    fn name_only_dependency_resolves_through_the_registry() {
        let (pkg_src, sha) = source_repo();
        let registry = registry_repo(&[("dep", &pkg_src.path().to_string_lossy(), "main", None)]);
        let (_cache_dir, cache) = cache();

        // The workspace declares `dep` by NAME ONLY, no remote or ref.
        let mut member_roles = BTreeMap::new();
        member_roles.insert(RepoName("dep".into()), MemberRole::Dep);
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![RepoEntry {
                name: RepoName("dep".into()),
                description: None,
                remote: None,
                git_ref: None,
                path: None,
            }],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles,
        };

        let resolutions =
            resolve_dependencies(&mut ws, &cache, &registry.path().to_string_lossy()).resolutions;
        assert_eq!(resolutions.len(), 1);
        let r = &resolutions[0];
        let pkg = r.outcome.as_ref().expect("resolves via the registry");
        assert_eq!(pkg.sha, sha);
        // The recorded remote is the package's, learned from the registry.
        assert_eq!(
            r.remote.as_deref(),
            Some(pkg_src.path().to_string_lossy().as_ref())
        );
        assert_eq!(
            ws.member_paths.get(&RepoName("dep".into())),
            Some(&pkg.path)
        );
    }

    #[test]
    fn resolve_writes_a_lock_and_a_fresh_load_locates_offline() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();

        let (ws_dir, manifest, mut ws) = proj_workspace(&[("dep", &src.path().to_string_lossy())]);
        let (resolutions, _, written) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert_eq!(resolutions.len(), 1);

        // The lock landed in proj's own `.arsumbris/`, pinning the resolved sha.
        let lock_path = crate::repo::repo_lock_path(&ws_dir.path().join("proj"));
        assert!(
            written
                .iter()
                .any(|(n, p)| n.as_str() == "proj" && p == &lock_path),
            "proj's own lock was written: {written:?}"
        );
        let lock = crate::repo::parse_package_lock(&fs::read(&lock_path).unwrap());
        assert_eq!(lock.get(&RepoName("dep".into())).unwrap().sha, sha);

        // The source disappears: a later open must not need the network.
        drop(src);

        // A fresh load locates the dep from proj's lock + cache, no fetch.
        let mut reopened = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        locate_locked_dependencies(&mut reopened, &cache, &RealFileSystem);
        assert_eq!(
            reopened.member_paths.get(&RepoName("dep".into())),
            cache.lookup(&sha).as_ref()
        );
        let path = reopened.member_paths.get(&RepoName("dep".into())).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("type/thing.type.yaml")).unwrap(),
            "fields: {}\n"
        );
    }

    #[test]
    fn a_repos_lock_pins_its_full_transitive_fetched_closure() {
        // proj depends on `mid` (fetched), which depends on `leaf` (fetched).
        // proj's OWN lock must pin BOTH: the full transitive fetched closure, so
        // proj re-opens standalone without relying on any other repo's lock. This
        // is the full-closure decision, see the plan's 2607081930 adjustment.
        let (leaf, leaf_sha) = source_repo();
        let (mid, mid_sha) =
            repo_with_peers("mid", &[("leaf", &leaf.path().to_string_lossy(), "main")]);
        let (_cache_dir, cache) = cache();
        let (ws_dir, _manifest, mut ws) = proj_workspace(&[("mid", &mid.path().to_string_lossy())]);

        let (_res, _diags, written) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();

        let lock_path = crate::repo::repo_lock_path(&ws_dir.path().join("proj"));
        assert!(written
            .iter()
            .any(|(n, p)| n.as_str() == "proj" && p == &lock_path));
        let lock = crate::repo::parse_package_lock(&fs::read(&lock_path).unwrap());
        assert_eq!(
            lock.get(&RepoName("mid".into())).unwrap().sha,
            mid_sha,
            "the direct fetched dep is pinned"
        );
        assert_eq!(
            lock.get(&RepoName("leaf".into())).unwrap().sha,
            leaf_sha,
            "the transitive fetched dep is pinned too (full closure)"
        );
        assert_eq!(lock.len(), 2, "exactly the fetched closure: {lock:?}");
    }

    #[test]
    fn a_location_path_override_wins_over_the_cache_snapshot() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();

        // Resolve once: fetch into the cache and write proj's own lock.
        let (ws_dir, manifest, mut ws) = proj_workspace(&[("dep", &src.path().to_string_lossy())]);
        let (resolutions, _, _) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert_eq!(resolutions.len(), 1);
        drop(src);

        // A local checkout of the dependency, standing in for a co-present sibling
        // or a registry path that resolves `dep` to a working tree.
        let local = TempDir::new().unwrap();
        fs::create_dir_all(local.path().join("type")).unwrap();
        fs::write(local.path().join("type/thing.type.yaml"), "fields: {}\n").unwrap();

        // Reopen with `dep` mapped to the local path; the cache still holds the
        // pinned sha, and proj's lock still pins it.
        let mut reopened = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        reopened
            .member_paths
            .insert(RepoName("dep".into()), local.path().to_path_buf());

        let diags = locate_locked_dependencies(&mut reopened, &cache, &RealFileSystem);

        // The local override wins: member_paths keeps the local path, the cache
        // snapshot at the pinned sha does NOT overwrite it.
        assert_eq!(
            reopened.member_paths.get(&RepoName("dep".into())),
            Some(&local.path().to_path_buf()),
            "the local override must win over the cache snapshot"
        );
        assert_ne!(
            reopened.member_paths.get(&RepoName("dep".into())),
            cache.lookup(&sha).as_ref(),
            "the cache snapshot must not overwrite the override"
        );

        // The override is surfaced so a forgotten one does not read as stale.
        assert_eq!(diags.len(), 1, "one advisory for the override");
        assert_eq!(diags[0].code, crate::repo::DEPENDENCY_PATH_OVERRIDDEN);
        assert_eq!(diags[0].severity, Severity::Hint);
    }

    #[test]
    fn a_conflicted_dependency_with_a_local_override_still_conflicts_and_mounts_the_override() {
        // Characterizes the conflict + override interaction: the conflict skip
        // precedes the override check, so a conflicted dependency still reports
        // its version conflict and gets no override advisory, yet its local path
        // (already in member_paths from the location file) stays mounted. Making
        // an override RESOLVE the conflict is the deferred workspace pin-override,
        // todo 2606292008; F1 keeps the conservative behavior locked here.
        let (_cache_dir, cache) = cache();
        let ws_dir = TempDir::new().unwrap();
        let proj = ws_dir.path().join("proj");
        fs::create_dir_all(proj.join(".arsumbris")).unwrap();

        // proj's own lock holds two names requiring the SAME package (one remote,
        // no subpath) at two different shas: a version conflict marking both.
        let remote = "https://example.invalid/base";
        let mut lock = PackageLock::new();
        lock.insert(RepoName("base".into()), locked(remote, "sha1", None));
        lock.insert(RepoName("base-old".into()), locked(remote, "sha2", None));
        write_package_lock(&crate::repo::repo_lock_path(&proj), &lock).unwrap();

        // `base` is overridden to a local path (as a co-present sibling would);
        // `base-old` is a plain conflicted dependency with no override.
        let local = ws_dir.path().join("base-local");
        let mut member_roles = BTreeMap::new();
        member_roles.insert(RepoName("base".into()), MemberRole::Dep);
        member_roles.insert(RepoName("base-old".into()), MemberRole::Dep);
        let mut member_paths = BTreeMap::new();
        member_paths.insert(RepoName("proj".into()), proj);
        member_paths.insert(RepoName("base".into()), local.clone());
        let mut ws = Workspace {
            manifest_path: ws_dir.path().join(".arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![],
            member_paths,
            member_roles,
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
        };

        let diags = locate_locked_dependencies(&mut ws, &cache, &RealFileSystem);

        // The version conflict still fires (the lock genuinely holds two shas).
        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_VERSION_CONFLICT),
            "the conflict is real and still reported"
        );
        // No override advisory: the conflict skip precedes the override check.
        assert!(
            !diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_PATH_OVERRIDDEN),
            "a conflicted dependency never reaches the override branch"
        );
        // The local override survives the conflict skip (the cache is never
        // consulted for a conflicted name, so it cannot overwrite it either).
        assert_eq!(
            ws.member_paths.get(&RepoName("base".into())),
            Some(&local),
            "the local override tree stays mounted despite the conflict"
        );
        // The other conflicted dependency, with no override, is left unmounted.
        assert!(
            !ws.member_paths.contains_key(&RepoName("base-old".into())),
            "a conflicted dependency with no override is unmounted"
        );
    }

    #[test]
    fn per_repo_lock_union_flags_one_name_at_two_remotes() {
        let (_cache_dir, cache) = cache();
        let ws_dir = TempDir::new().unwrap();
        let proj_a = ws_dir.path().join("proj-a");
        let proj_b = ws_dir.path().join("proj-b");
        fs::create_dir_all(proj_a.join(".arsumbris")).unwrap();
        fs::create_dir_all(proj_b.join(".arsumbris")).unwrap();

        // Two editable members each pin `util`, but to DIFFERENT remotes (legal
        // under same-name-coexistence). Different remotes fall in separate version
        // groups, so `detect_version_conflicts` misses this; without the identity
        // detector the union silently mounts one repo's `util` for the other.
        let mut lock_a = PackageLock::new();
        lock_a.insert(
            RepoName("util".into()),
            locked("https://example.invalid/x/util", "sha1", None),
        );
        write_package_lock(&crate::repo::repo_lock_path(&proj_a), &lock_a).unwrap();
        let mut lock_b = PackageLock::new();
        lock_b.insert(
            RepoName("util".into()),
            locked("https://example.invalid/y/util", "sha2", None),
        );
        write_package_lock(&crate::repo::repo_lock_path(&proj_b), &lock_b).unwrap();

        let mut member_paths = BTreeMap::new();
        member_paths.insert(RepoName("proj-a".into()), proj_a);
        member_paths.insert(RepoName("proj-b".into()), proj_b);
        let mut ws = Workspace {
            manifest_path: ws_dir.path().join(".arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![],
            member_paths,
            member_roles: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
        };

        let diags = locate_locked_dependencies(&mut ws, &cache, &RealFileSystem);
        assert!(
            diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_IDENTITY_CONFLICT),
            "a name at two remotes across per-repo locks is an identity conflict: {diags:?}"
        );
        // Specifically the identity detector: version-conflict groups by
        // (remote, path), so two different remotes never collide there.
        assert!(
            !diags
                .iter()
                .any(|d| d.code == crate::repo::DEPENDENCY_VERSION_CONFLICT),
            "different remotes are not a version conflict: {diags:?}"
        );
    }

    #[test]
    fn resolve_commits_the_lock_into_the_project_repo() {
        let (src, sha) = source_repo();
        let (_cache_dir, cache) = cache();

        // The primary repo `proj/` is itself a git repo, holding the manifest and
        // the lock inside its own `.arsumbris/`.
        let ws_dir = TempDir::new().unwrap();
        let manifest = proj_with_remote_dep(ws_dir.path(), &src.path().to_string_lossy());
        let proj = ws_dir.path().join("proj");
        run(&proj, &["init", "-q", "-b", "main"]);
        run(&proj, &["config", "user.name", "Tester"]);
        run(&proj, &["config", "user.email", "tester@example.com"]);
        run(&proj, &["add", "-A"]);
        run(&proj, &["commit", "-q", "-m", "init"]);
        let before = run(&proj, &["rev-parse", "HEAD"]);

        // Resolve writes the lock, then commit it into the project repo.
        let mut workspace = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        resolve_and_lock(&mut workspace, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        let lock_path = crate::repo::repo_lock_path(&proj);
        let commit = commit_package_lock(&proj, &lock_path, RepoName("proj".into()), "m-test-1")
            .unwrap()
            .expect("a fresh lock is a real commit");

        // HEAD advanced to the lock commit.
        let head = run(&proj, &["rev-parse", "HEAD"]);
        assert_ne!(head, before, "the lock commit advances HEAD");
        assert_eq!(head, commit.0, "the returned sha is the new HEAD");

        // A second commit with the lock unchanged is a no-op, not an error.
        let again =
            commit_package_lock(&proj, &lock_path, RepoName("proj".into()), "m-test-2").unwrap();
        assert!(
            again.is_none(),
            "an unchanged lock is nothing to commit, not a failure"
        );
        assert_eq!(
            run(&proj, &["rev-parse", "HEAD"]),
            head,
            "HEAD does not advance on a no-op commit"
        );

        // The committed tree holds proj's own lock, pinning the resolved sha.
        let committed = run(&proj, &["show", "HEAD:.arsumbris/repo.lock"]);
        assert!(
            committed.contains(&sha),
            "the committed lock pins the resolved sha: {committed}"
        );

        // The commit carries the Mutation-Id trailer, the audit correlation.
        let body = run(&proj, &["log", "-1", "--format=%B"]);
        assert!(
            body.contains("Mutation-Id: m-test-1"),
            "trailer missing: {body}"
        );
    }

    #[test]
    fn locate_is_a_noop_without_a_lockfile() {
        let (_cache_dir, cache) = cache();
        let ws_dir = TempDir::new().unwrap();
        let manifest = ws_dir.path().join(".arsumbris/workspace.yaml");
        let mut ws = dep_workspace("dep", Path::new("/gone"), "main", manifest);
        locate_locked_dependencies(&mut ws, &cache, &RealFileSystem);
        assert!(ws.member_paths.is_empty());
    }

    #[test]
    fn monorepo_subpackages_share_one_snapshot_and_mount_subpaths() {
        let (mono, sha) = monorepo_source();
        let remote = mono.path().to_string_lossy().into_owned();
        let registry = registry_repo(&[
            ("a", &remote, "main", Some("pkgs/a")),
            ("b", &remote, "main", Some("pkgs/b")),
        ]);
        let (_cache_dir, cache) = cache();

        // The workspace declares both monorepo subpackages by name.
        let mut provenance = BTreeMap::new();
        provenance.insert(RepoName("a".into()), MemberRole::Dep);
        provenance.insert(RepoName("b".into()), MemberRole::Dep);
        let mk = |n: &str| RepoEntry {
            name: RepoName(n.into()),
            description: None,
            remote: None,
            git_ref: None,
            path: None,
        };
        let mut ws = Workspace {
            manifest_path: PathBuf::from("/ws/.arsumbris/workspace.yaml"),
            name: "demo".into(),
            members: vec![mk("a"), mk("b")],
            member_paths: BTreeMap::new(),
            edit: Vec::new(),
            discover: Vec::new(),
            disabled: Vec::new(),
            member_notes: Vec::new(),
            member_roles: provenance,
        };

        let resolutions =
            resolve_dependencies(&mut ws, &cache, &registry.path().to_string_lossy()).resolutions;
        assert_eq!(resolutions.len(), 2);
        assert!(resolutions.iter().all(|r| r.outcome.is_ok()));

        // Both packages came from one monorepo at one sha, so the fetch deduped
        // to a single snapshot; each mounts its own subpath of it.
        let snapshot = cache.lookup(&sha).expect("the monorepo cached once");
        let a = ws.member_paths.get(&RepoName("a".into())).unwrap();
        let b = ws.member_paths.get(&RepoName("b".into())).unwrap();
        assert_eq!(a, &snapshot.join("pkgs/a"));
        assert_eq!(b, &snapshot.join("pkgs/b"));
        assert!(a.join("type/a.type.yaml").exists());
        assert!(b.join("type/b.type.yaml").exists());

        // The lock pins each subpath.
        let lock = lock_from_resolutions(&resolutions, &BTreeSet::new());
        assert_eq!(
            lock.get(&RepoName("a".into())).unwrap().path.as_deref(),
            Some("pkgs/a")
        );
    }

    #[test]
    fn the_watcher_and_fingerprint_roots_exclude_the_cache() {
        // A locked dependency, resolved into the cache.
        let (src, _sha) = source_repo();
        let cache_dir = TempDir::new().unwrap();
        // Canonical, matching the one spelling the assembly mounts a snapshot under.
        let cache_root = crate::repo::canonical_root(cache_dir.path()).join("packages");
        let cache = PackageCache::new(cache_root.clone());
        let ws_dir = TempDir::new().unwrap();
        let manifest = proj_with_remote_dep(ws_dir.path(), &src.path().to_string_lossy());
        let mut ws = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();

        // The watcher and the fingerprint derive their roots from assembly_roots,
        // which resolves only co-present and registry members, never the cache.
        // So the immutable cache snapshot is never a watched or fingerprinted root.
        let (roots, _) = crate::build::assembly_roots(
            &ws_dir.path().join("proj"),
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        assert!(
            !roots.iter().any(|r| r.starts_with(&cache_root)),
            "the cache must not be a watched/fingerprinted root: {roots:?}"
        );
    }

    #[test]
    fn build_mounts_a_locked_dependency_offline() {
        let (src, sha) = source_repo();
        let cache_dir = TempDir::new().unwrap();
        // Canonical, matching the one spelling the assembly mounts a snapshot under.
        let cache_root = crate::repo::canonical_root(cache_dir.path()).join("packages");
        let cache = PackageCache::new(cache_root.clone());

        // A co-present primary declaring the dependency by explicit remote.
        let ws_dir = TempDir::new().unwrap();
        let manifest = proj_with_remote_dep(ws_dir.path(), &src.path().to_string_lossy());

        // Resolve once: fetch into the cache and write proj's own lock.
        let mut ws = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        let (resolutions, _, _) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert_eq!(resolutions.len(), 1);

        // The source remote can vanish; the re-open must not touch the network.
        drop(src);

        // A full build over the folder-repo `proj` locates the dep from proj's own
        // lock + cache and mounts it (an entry-only workspace: proj + its dep).
        let kb = crate::build::build_reusing(
            &ws_dir.path().join("proj"),
            &RealFileSystem,
            &crate::repo::UserRegistry::new(),
            &crate::ir::ParseLayer::default(),
            &crate::build::Fingerprint::new(),
            Some(&cache_root),
        )
        .unwrap();

        // The dependency's type-def is in the catalog, at its cache snapshot path.
        let snapshot = cache_root.join(&sha);
        assert!(
            kb.catalog
                .contains_key(snapshot.join("type/thing.type.yaml").as_path()),
            "the locked dependency should mount from its cache snapshot"
        );
    }

    #[test]
    fn a_cache_only_edit_member_is_edit_member_read_only() {
        // A dependency source repo that declares itself `dep`.
        let src = TempDir::new().unwrap();
        {
            let p = src.path();
            run(p, &["init", "-q", "-b", "main"]);
            run(p, &["config", "user.name", "Tester"]);
            run(p, &["config", "user.email", "tester@example.com"]);
            fs::create_dir_all(p.join("type")).unwrap();
            fs::write(p.join("type/depthing.type.yaml"), "fields: {}\n").unwrap();
            fs::create_dir_all(p.join(".arsumbris")).unwrap();
            fs::write(p.join(".arsumbris/repo.yaml"), "name: dep\n").unwrap();
            run(p, &["add", "-A"]);
            run(p, &["commit", "-q", "-m", "v1"]);
        }
        let sha = run(src.path(), &["rev-parse", "HEAD"]);

        let cache_dir = TempDir::new().unwrap();
        // Canonical, matching the one spelling the assembly mounts a snapshot under.
        let cache_root = crate::repo::canonical_root(cache_dir.path()).join("packages");
        let cache = PackageCache::new(cache_root.clone());
        // Populate the cache with dep's snapshot; the re-open below is offline.
        cache.ensure(&src.path().to_string_lossy(), "main").unwrap();

        // A co-present editable primary `proj` declaring `dep`, plus a manifest that
        // names BOTH `proj` and `dep` as primaries. `dep` is not co-present and not
        // in the registry, so it resolves only from the cache: a read-only primary.
        let ws_dir = TempDir::new().unwrap();
        let proj = write_proj(ws_dir.path(), &[("dep", &src.path().to_string_lossy())]);
        fs::create_dir_all(proj.join("type")).unwrap();
        fs::write(proj.join("type/projthing.type.yaml"), "fields: {}\n").unwrap();
        // A folder-repo entry composing proj (co-present, editable) and dep, which
        // is not co-present and not in the registry: dep is an editable member
        // resolving ONLY from the cache, a read-only editable member.
        fs::create_dir_all(ws_dir.path().join(".arsumbris")).unwrap();
        fs::write(ws_dir.path().join(".arsumbris/repo.yaml"), "name: ws\n").unwrap();
        fs::write(
            ws_dir.path().join(".arsumbris/workspace.yaml"),
            "edit:\n  - ws\n  - proj\n  - dep\n",
        )
        .unwrap();

        // proj's own lock pins dep at its fetched sha.
        let mut lock = PackageLock::new();
        lock.insert(
            RepoName("dep".into()),
            locked(&src.path().to_string_lossy(), &sha, None),
        );
        write_package_lock(&crate::repo::repo_lock_path(&proj), &lock).unwrap();
        // The remote can vanish: the mount is from the cache, no network.
        drop(src);

        let kb = crate::build::build_reusing(
            ws_dir.path(),
            &RealFileSystem,
            &crate::repo::UserRegistry::new(),
            &crate::ir::ParseLayer::default(),
            &crate::build::Fingerprint::new(),
            Some(&cache_root),
        )
        .unwrap();

        let codes: Vec<String> = kb
            .diagnostics()
            .map(|d| d.code.as_str().to_string())
            .collect();
        assert!(
            codes.iter().any(|c| c == "edit-member-read-only"),
            "a cache-only edit member is present but read-only: {codes:?}"
        );
        // It mounted (present), so it is NOT edit-member-unmounted.
        assert!(
            !codes.iter().any(|c| c == "edit-member-unmounted"),
            "a cache-only edit member is present, not unmounted: {codes:?}"
        );
        // dep's type mounted from its cache snapshot.
        let snapshot = cache_root.join(&sha);
        assert!(
            kb.catalog
                .contains_key(snapshot.join("type/depthing.type.yaml").as_path()),
            "dep mounts read-only from its cache snapshot"
        );
    }

    #[test]
    fn a_scattered_registry_path_dependency_is_walked_and_watched() {
        use crate::repo::{RegistryLocation, RepoName, UserRegistry};

        // A scattered dependency `lib` at a path OUTSIDE the entry repo, reachable
        // only via the per-user registry (not co-present, not the cache).
        let scattered = TempDir::new().unwrap();
        let lib = fs::canonicalize(scattered.path()).unwrap().join("lib");
        fs::create_dir_all(lib.join(".arsumbris")).unwrap();
        fs::write(lib.join(".arsumbris/repo.yaml"), "name: lib\n").unwrap();
        fs::create_dir_all(lib.join("type")).unwrap();
        fs::write(lib.join("type/libthing.type.yaml"), "fields: {}\n").unwrap();

        // The entry is the folder-repo `main`, declaring `lib` as a dep (an
        // entry-only workspace: main + its dep). `lib` resolves via the registry.
        let ws_dir = TempDir::new().unwrap();
        let main = fs::canonicalize(ws_dir.path()).unwrap();
        fs::create_dir_all(main.join(".arsumbris")).unwrap();
        fs::write(
            main.join(".arsumbris/repo.yaml"),
            "name: main\ndeps:\n  - name: lib\n",
        )
        .unwrap();
        fs::create_dir_all(main.join("type")).unwrap();
        fs::write(main.join("type/mainthing.type.yaml"), "fields: {}\n").unwrap();

        // The registry locates `lib` at its scattered path.
        let mut registry = UserRegistry::new();
        registry.insert(
            RepoName("lib".into()),
            RegistryLocation {
                remote: None,
                path: lib.clone(),
            },
        );

        // A cache root gates the cache-tier locate (the daemon's path); it stays
        // empty here, `lib` resolves via the registry, not the cache.
        let cache_dir = TempDir::new().unwrap();
        // Canonical, matching the one spelling the assembly mounts a snapshot under.
        let cache_root = crate::repo::canonical_root(cache_dir.path()).join("packages");

        let kb = crate::build::build_reusing(
            &main,
            &RealFileSystem,
            &registry,
            &crate::ir::ParseLayer::default(),
            &crate::build::Fingerprint::new(),
            Some(&cache_root),
        )
        .unwrap();

        // The scattered member's own type mounts: it was WALKED, not merely
        // resolved into member_paths.
        assert!(
            kb.catalog
                .contains_key(lib.join("type/libthing.type.yaml").as_path()),
            "the scattered dep's types mount"
        );

        // Its editable local root is a watched / fingerprinted root.
        let (roots, _) = crate::build::assembly_roots(&main, &registry, &RealFileSystem);
        assert!(
            roots.iter().any(|r| r == &lib),
            "the scattered local member is watched: {roots:?}"
        );
    }

    #[test]
    fn build_mounts_a_path_overridden_dependency_from_the_local_tree() {
        let (src, sha) = source_repo();
        let cache_dir = TempDir::new().unwrap();
        // Canonical, matching the one spelling the assembly mounts a snapshot under.
        let cache_root = crate::repo::canonical_root(cache_dir.path()).join("packages");
        let cache = PackageCache::new(cache_root.clone());

        let ws_dir = TempDir::new().unwrap();
        let manifest = proj_with_remote_dep(ws_dir.path(), &src.path().to_string_lossy());

        // Resolve once: fetch into the cache and write the lock.
        let mut ws = crate::repo::load_workspace(
            &manifest,
            &fs::read(&manifest).unwrap(),
            &[ws_dir.path().join("proj")],
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        drop(src);

        // A co-present sibling `dep/` carrying a DISTINCT type-def, so the local
        // tree is distinguishable from the cache snapshot by content. The sibling
        // resolves ahead of the cache pin, overriding it.
        // Canonical, the spelling the catalog keys the mounted tree under.
        let local = crate::repo::canonical_root(ws_dir.path()).join("dep");
        fs::create_dir_all(local.join(".arsumbris")).unwrap();
        fs::write(local.join(".arsumbris/repo.yaml"), "name: dep\n").unwrap();
        fs::create_dir_all(local.join("type")).unwrap();
        fs::write(local.join("type/local-only.type.yaml"), "fields: {}\n").unwrap();

        // A folder-repo entry composing proj; proj deps dep, which resolves ahead
        // of the cache pin to the co-present sibling `dep/` (the override).
        fs::create_dir_all(ws_dir.path().join(".arsumbris")).unwrap();
        fs::write(ws_dir.path().join(".arsumbris/repo.yaml"), "name: ws\n").unwrap();
        fs::write(
            ws_dir.path().join(".arsumbris/workspace.yaml"),
            "edit:\n  - ws\n  - proj\n",
        )
        .unwrap();

        // The build mounts the LOCAL tree, not the cache snapshot.
        let kb = crate::build::build_reusing(
            ws_dir.path(),
            &RealFileSystem,
            &crate::repo::UserRegistry::new(),
            &crate::ir::ParseLayer::default(),
            &crate::build::Fingerprint::new(),
            Some(&cache_root),
        )
        .unwrap();
        assert!(
            kb.catalog
                .contains_key(local.join("type/local-only.type.yaml").as_path()),
            "the override mounts the local working tree"
        );
        let snapshot = cache_root.join(&sha);
        assert!(
            !kb.catalog
                .contains_key(snapshot.join("type/thing.type.yaml").as_path()),
            "the cache snapshot must not mount for an overridden dependency"
        );

        // The override advisory flows through the build.
        assert!(
            kb.diagnostics()
                .any(|d| d.code == crate::repo::DEPENDENCY_PATH_OVERRIDDEN),
            "the build surfaces the path override"
        );

        // The override is served from a live local tree (watched, hinted), but it
        // is a `dep` role, so consumed: the wire `editable` flag is role-derived
        // and stays false, decoupled from the served-locally fact (decision
        // 2607161333).
        let members = crate::wire::introspect_members(&kb, ws_dir.path(), Some(&cache_root));
        let dep = members
            .members
            .iter()
            .find(|m| m.repo == "dep")
            .expect("dep is a member");
        // The override decoupling on the wire: served from a live local tree
        // (`local: true`), but a `dep` role, so consumed (`editable: false`).
        assert!(
            !dep.editable && dep.local,
            "an overridden dep is consumed (not editable) yet local: {} {}",
            dep.editable,
            dep.local
        );

        // The watcher/fingerprint roots include the override (unlike a cache dep),
        // so the two views agree and edits to the local tree are tracked.
        let (roots, _) = crate::build::assembly_roots(
            ws_dir.path(),
            &crate::repo::UserRegistry::new(),
            &RealFileSystem,
        );
        assert!(
            roots.iter().any(|r| r == &local),
            "the override path is watched/fingerprinted: {roots:?}"
        );
        assert!(
            !roots.iter().any(|r| r.starts_with(&cache_root)),
            "the cache is still never a watched root: {roots:?}"
        );
    }

    #[test]
    fn a_discover_member_is_fetched_pinned_in_the_workspace_lock_and_mounts_offline() {
        // A `discover` member is fetched and pinned (like a dep), its closure
        // landing in the entry's `.arsumbris/workspace.lock`, so it mounts
        // reproducibly and offline. Sourced by an explicit remote to isolate the
        // fetch/lock machinery from registry name-resolution.
        let (src, sha) = source_repo(); // owns the type-def `thing`
        let remote = src.path().to_string_lossy().into_owned();
        let (_cache_dir, cache) = cache();

        let ws_dir = TempDir::new().unwrap();
        fs::create_dir_all(ws_dir.path().join(".arsumbris")).unwrap();
        let manifest = ws_dir.path().join(".arsumbris/workspace.yaml");
        let ws_lock_path = ws_dir.path().join(".arsumbris/workspace.lock");

        let make_ws = || {
            let mut roles = BTreeMap::new();
            roles.insert(RepoName("ws".into()), MemberRole::Entry);
            roles.insert(RepoName("shared".into()), MemberRole::Discover);
            let mut paths = BTreeMap::new();
            paths.insert(RepoName("ws".into()), ws_dir.path().to_path_buf());
            Workspace {
                manifest_path: manifest.clone(),
                name: "ws".into(),
                members: vec![
                    RepoEntry {
                        name: RepoName("ws".into()),
                        description: None,
                        remote: None,
                        git_ref: None,
                        path: None,
                    },
                    RepoEntry {
                        name: RepoName("shared".into()),
                        description: None,
                        remote: Some(remote.clone()),
                        git_ref: Some("main".into()),
                        path: None,
                    },
                ],
                member_paths: paths,
                edit: vec![RepoName("ws".into())],
                discover: vec![RepoName("shared".into())],
                disabled: Vec::new(),
                member_notes: Vec::new(),
                member_roles: roles,
            }
        };

        // Resolve: fetch `shared` and write the workspace.lock.
        let mut ws = make_ws();
        let (_res, diags, written) =
            resolve_and_lock(&mut ws, &cache, DEFAULT_REGISTRY_REMOTE).unwrap();
        assert!(diags.is_empty(), "a clean resolve: {diags:?}");

        // The workspace.lock is written beside the workspace.yaml, pinning `shared`
        // at the fetched sha, and is staged for commit under the entry repo.
        assert!(ws_lock_path.exists(), "workspace.lock written");
        let lock = crate::repo::parse_package_lock(&fs::read(&ws_lock_path).unwrap());
        let pin = lock
            .get(&RepoName("shared".into()))
            .expect("shared pinned in the workspace lock");
        assert_eq!(pin.sha, sha, "pinned at the fetched sha");
        assert!(
            written
                .iter()
                .any(|(n, p)| n.as_str() == "ws" && p == &ws_lock_path),
            "workspace.lock staged under the entry repo: {written:?}"
        );

        // Offline re-open: drop the remote, a fresh workspace mounts `shared` from
        // the cache via the workspace.lock (no network).
        drop(src);
        let mut reopened = make_ws();
        let diags = locate_locked_dependencies(&mut reopened, &cache, &RealFileSystem);
        assert!(diags.is_empty(), "the offline mount is clean: {diags:?}");
        let mounted = reopened
            .member_paths
            .get(&RepoName("shared".into()))
            .expect("shared mounts offline");
        assert!(
            mounted.starts_with(cache.root()) && mounted.ends_with(&sha),
            "shared mounts from the cache snapshot at the pinned sha: {mounted:?}"
        );
    }

    fn locked(remote: &str, sha: &str, path: Option<&str>) -> LockedPackage {
        LockedPackage {
            remote: remote.into(),
            sha: sha.into(),
            path: path.map(str::to_string),
        }
    }

    /// Conflict detection over a lock's pins, the offline-path input shape.
    fn lock_conflicts(lock: &PackageLock, manifest: &str) -> (Vec<Diagnostic>, BTreeSet<RepoName>) {
        detect_version_conflicts(
            lock.iter().map(|(name, pkg)| {
                (
                    name,
                    pkg.remote.as_str(),
                    pkg.path.as_deref(),
                    pkg.sha.as_str(),
                )
            }),
            Path::new(manifest),
        )
    }

    #[test]
    fn version_conflict_fires_for_two_versions_of_one_package() {
        let mut lock = PackageLock::new();
        // Two members, same (remote, path), different sha: the same package at
        // two versions, a hard conflict.
        lock.insert(
            RepoName("bento".into()),
            locked("R", "s1", Some("projections/bento")),
        );
        lock.insert(
            RepoName("bento-old".into()),
            locked("R", "s2", Some("projections/bento")),
        );
        // A different package, no conflict.
        lock.insert(RepoName("atlas".into()), locked("Q", "s3", None));

        let (diags, conflicted) = lock_conflicts(&lock, "/ws/.arsumbris/workspace.yaml");
        assert_eq!(diags.len(), 1, "exactly one conflicted package");
        assert_eq!(diags[0].code, crate::repo::DEPENDENCY_VERSION_CONFLICT);
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(
            diags[0].message.contains("projections/bento")
                && diags[0].message.contains("2 conflicting versions"),
            "{}",
            diags[0].message
        );
        // Both conflicting members are flagged, so neither is mounted or locked.
        assert!(conflicted.contains(&RepoName("bento".into())));
        assert!(conflicted.contains(&RepoName("bento-old".into())));
        assert!(!conflicted.contains(&RepoName("atlas".into())));
    }

    #[test]
    fn no_version_conflict_when_each_package_has_one_version() {
        let mut lock = PackageLock::new();
        lock.insert(RepoName("a".into()), locked("R", "s1", None));
        lock.insert(RepoName("b".into()), locked("Q", "s2", None));
        // The SAME repo at the SAME sha but two subpaths is two distinct packages,
        // not a conflict.
        lock.insert(RepoName("c".into()), locked("M", "s9", Some("pkgs/c")));
        lock.insert(RepoName("d".into()), locked("M", "s9", Some("pkgs/d")));
        let (diags, conflicted) = lock_conflicts(&lock, "/wsx/.arsumbris/workspace.yaml");
        assert!(diags.is_empty());
        assert!(conflicted.is_empty());
    }

    #[test]
    fn distinct_monorepo_subpaths_at_different_shas_is_not_a_conflict() {
        // Two DIFFERENT packages (different subpaths) of one monorepo, each pinned
        // at its own sha. Distinct packages, freely versioned, never a conflict.
        let mut lock = PackageLock::new();
        lock.insert(
            RepoName("bento".into()),
            locked("apps", "sha1", Some("projections/bento")),
        );
        lock.insert(
            RepoName("editor".into()),
            locked("apps", "sha2", Some("projections/editor")),
        );
        assert!(lock_conflicts(&lock, "/wsx/.arsumbris/workspace.yaml")
            .0
            .is_empty());
    }
}
