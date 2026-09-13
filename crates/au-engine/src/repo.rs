//! Repo discovery and membership.
//!
//! A repo is a directory holding a committed `.arsumbris/repo.yaml`
//! that declares its `name:`. Identity is declared, not derived from the
//! directory.
//! A file's repo is its nearest ancestor repo root. A root that declares no
//! registry degrades to one implicit repo, so every walked file has a repo.
//!
//! The registry is probed directly through `read_file`: the walker ignores
//! `.arsumbris/`, so the registry never appears in the walked file list.
//!
//! Membership is load-bearing: each repo resolves against its own type graph
//! and reference index, and the cross-repo composition runs over the per-repo
//! graphs. Discovery runs over a set of roots, just the entry repo in an
//! entry-only workspace, one per mounted member when more compose. Most diagnostics here are
//! advisory `Warning`s. A `repo.yaml` that cannot load, unparseable or nameless,
//! is `Error`: it produces no type graph and blocks every `::repo` fold into it,
//! a downstream stage. The build still never aborts, the growth-never-blocked
//! stance holds, `Error` is a severity tier, not a fatal exit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use au_diagnostics::{ByteRange, Diagnostic, DiagnosticCode, Severity, Span, SuggestedFix};
use au_parser::yaml::{parse, span_to_byte_range, MarkedYaml, Scalar, YamlData};
use au_parser::FileSystem;

/// `.arsumbris/repo.yaml` is not valid UTF-8 or YAML. `Error`: the repo produces
/// no type graph and every `::repo` fold into it is blocked.
pub const REPO_REGISTRY_PARSE_ERROR: DiagnosticCode =
    DiagnosticCode::from_static("repo-registry-parse-error");
/// A `repo.yaml` declares no top-level `name`, or one that is not a valid repo
/// name (the type/field-name grammar). `Error`: the directory is not treated as
/// a repo root, the same vanish-the-repo cascade as a parse failure.
pub const REPO_NAME_MISSING: DiagnosticCode = DiagnosticCode::from_static("repo-name-missing");
/// Two repos in the workspace declare the same `name:`.
pub const DUPLICATE_REPO_NAME: DiagnosticCode = DiagnosticCode::from_static("duplicate-repo-name");
/// A declared repo's on-disk folder basename differs from its declared `name`,
/// compared CASE-INSENSITIVELY so a case-only difference on a case-insensitive
/// filesystem (macOS, Windows) is not a mismatch. A soft convention: the declared
/// name wins, resolution still succeeds. Drift severity, a high-attention
/// advisory. The implicit-repo case (no `repo.yaml`) is exempt, its name IS its
/// folder.
pub const REPO_FOLDER_NAME_MISMATCH: DiagnosticCode =
    DiagnosticCode::from_static("repo-folder-name-mismatch");
/// A declared peer resolves via none of the tiers: unmounted here. The role-keyed
/// unmounted signal for a plain `dep`; an `edit` member (or the entry) escalates
/// to `edit-member-unmounted`, a `discover` member to `discover-member-unmounted`.
pub const PEER_UNMOUNTED: DiagnosticCode = DiagnosticCode::from_static("peer-unmounted");
// Three states need no code, each impossible by construction:
// - a referenced peer that is not a member: `assemble_members` grows the member
//   set through each member's `deps` closure, so every dep of a mounted member is
//   itself a member.
// - an ambiguous workspace entry: the entry is a folder-repo directory, so there
//   is no "which manifest" question to answer.
// - a generic unmounted member: the signal is role-keyed, and every member carries
//   exactly one role (entry / `edit` / `discover` / `dep`).
/// A workspace member's path resolved but cannot be walked: it exists yet is not
/// a directory, or it is unreadable. Distinct from the unmounted codes
/// (`edit-member-unmounted` / `discover-member-unmounted` / `peer-unmounted`,
/// nothing resolved) — here a path was resolved, so the fix is to correct it,
/// not add one. Carries the OS-level cause so the user sees why the walk failed.
pub const WORKSPACE_MEMBER_UNWALKABLE: DiagnosticCode =
    DiagnosticCode::from_static("workspace-member-unwalkable");
/// An `edit` member (or the entry, an editable authoring ROOT) resolves to
/// nothing: no co-present sibling, no registry path, no cache snapshot. The
/// workspace opens DEGRADED over its present members, the missing root surfaced,
/// never silently dropped (a typo in an `edit` member is the silent-drop the
/// engine forbids). One that resolves only to the read-only cache is
/// `edit-member-read-only`.
pub const EDIT_MEMBER_UNMOUNTED: DiagnosticCode =
    DiagnosticCode::from_static("edit-member-unmounted");
/// An `edit` member (or the entry) resolves ONLY to the read-only cache snapshot:
/// present but not a live working tree. An edit member is meant to be editable,
/// so this is surfaced as a warning; the workspace still opens. The fix is to
/// register a local path or place it as a co-present sibling. Distinct from
/// `edit-member-unmounted` (resolves to nothing) and from a cache-mounted `dep` /
/// `discover` (read-only is expected there, no diagnostic).
pub const EDIT_MEMBER_READ_ONLY: DiagnosticCode =
    DiagnosticCode::from_static("edit-member-read-only");
/// A `discover` member resolves to nothing: no co-present sibling, no registry
/// path, no cache snapshot. A `discover` member is a pinned discovery mount, so
/// unmounted it contributes no vocabulary; surfaced as a warning, the workspace
/// still assembles from the rest. Distinct from `peer-unmounted` (a plain `dep`)
/// and `edit-member-unmounted` (an editable authoring root). A cache-mounted
/// `discover` is expected (no diagnostic).
pub const DISCOVER_MEMBER_UNMOUNTED: DiagnosticCode =
    DiagnosticCode::from_static("discover-member-unmounted");
/// A `workspace.yaml`'s `disabled:` overlay names a member absent from both
/// `edit:` and `discover:`: a manifest typo. `disabled:` silences a DECLARED
/// member, so a name it lists must be declared; an undeclared name is inert and
/// silently does nothing, the silent-drop the engine surfaces instead. A warning,
/// never blocking. A `disabled:` entry that DOES match a declared member is
/// intentional and fires nothing (the member is excluded from the mount set with
/// no diagnostic). See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub const DISABLED_MEMBER_NOT_DECLARED: DiagnosticCode =
    DiagnosticCode::from_static("disabled-member-not-declared");
/// A declared workspace member resolves to the reserved device root `~/.arsumbris`,
/// whose `.arsumbris/` IS the device config/data area. The member is EXCLUDED from
/// the mount set (walking a home directory as content would be catastrophic); the
/// workspace opens over the rest. Advisory, the parity of the unmounted family, and
/// specific: it takes precedence over the generic unmounted signal. See
/// [[spec - arsumbris layout - a reserved multi-tenant device root, owner-namespaced with a category sublayer]].
pub const MEMBER_AT_RESERVED_ROOT: DiagnosticCode =
    DiagnosticCode::from_static("member-at-reserved-root");
/// A name appears in both a `workspace.yaml`'s `edit:` and its `discover:` list.
/// A member has ONE role per workspace: `edit` is editable, `discover` is a
/// consumed discovery mount, so the two are contradictory. An error the caller
/// surfaces; the editable role (`edit`) wins meanwhile so assembly stays
/// deterministic. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub const WORKSPACE_MEMBER_ROLE_CONFLICT: DiagnosticCode =
    DiagnosticCode::from_static("workspace-member-role-conflict");
/// A name in a `workspace.yaml`'s `discover:` list is ALSO a declared `dep`
/// (reached through some member's `deps`), so the `discover` listing is
/// redundant: the dep already mounts and pins it (and, unlike a bare `discover`,
/// lets its types be crossed via `::repo`). A hint, never blocking; the member
/// assembles as a `dep` (the higher role). Distinct from
/// `workspace-member-role-conflict` (a contradictory `edit` + `discover`), this
/// is a redundant, compatible listing. Fix: drop the name from `discover`. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub const DISCOVER_MEMBER_IS_A_DEPENDENCY: DiagnosticCode =
    DiagnosticCode::from_static("discover-member-is-a-dependency");
/// A folder-repo's `.arsumbris/workspace.yaml` does not list its own containing
/// repo in `edit:`. The containing repo is the entry, a live working tree at
/// HEAD, so it belongs in `edit`; an omission would read as if the file excludes
/// its own repo. An error, so the file reads as self-complete. Fires only for a
/// folder-repo entry, where the containing repo's identity is known. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub const WORKSPACE_OMITS_CONTAINING_REPO: DiagnosticCode =
    DiagnosticCode::from_static("workspace-omits-containing-repo");
/// A nested `.arsumbris/repo.yaml` under a walked member that is not itself a
/// declared member. The subtree belongs to the nested repo, so the enclosing
/// walk stops at it and the nested repo contributes nothing, never silently
/// absorbed. A warning, the fix names how to include it (declare it as an `edit`
/// / `discover` member or a `dep`). A DECLARED nested repo mounts as its own
/// member and never fires this. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub const UNDECLARED_NESTED_REPO: DiagnosticCode =
    DiagnosticCode::from_static("undeclared-nested-repo");
// A member resolves by NAME through `resolve_repo_name`, which verifies the
// declared name matches, requires a readable `repo.yaml`, and yields one path per
// name. So a name mismatch, an undeclared member, and two members colliding on one
// path are all non-matches rather than diagnosable states, and need no code.
/// Two or more dependency edges require the same package (same remote and
/// subpath) at different versions. A hard conflict: the engine never picks a
/// winner, the conflicted package is left unresolved, the human or agent aligns
/// the edges to one version (or uses a workspace-level override). Supersedes the
/// former `version-skew` advisory.
pub const DEPENDENCY_VERSION_CONFLICT: DiagnosticCode =
    DiagnosticCode::from_static("dependency-version-conflict");
/// One dependency name resolves to two or more different packages (distinct
/// `(remote, subpath)`) across the closure: two unrelated repos wearing one
/// name. A hard identity conflict distinct from a version conflict (which is one
/// package at two shas). The engine never silently picks a winner, so the name
/// is left unresolved rather than the last-walked edge overwriting the earlier
/// one. The identity sibling of `dependency-version-conflict`.
///
/// A SAFETY FLOOR, not the ergonomic end-state: it rejects rather than resolving,
/// over-rejecting the legitimate transitive case (two independent deps sharing a
/// name that should coexist). The end-state is `(name, source)` member identity
/// (Cargo's model); see `todo - 2607021907`.
pub const DEPENDENCY_IDENTITY_CONFLICT: DiagnosticCode =
    DiagnosticCode::from_static("dependency-identity-conflict");
/// A declared dependency the resolver cannot deliver: no registry entry, no
/// remote, or an unreachable remote, direct or transitive. A hard error, a
/// failed open at release. Distinct from an expected-unmounted member, which
/// stays advisory.
pub const DEPENDENCY_RESOLUTION_FAILED: DiagnosticCode =
    DiagnosticCode::from_static("dependency-resolution-failed");
/// A locked dependency is served from a local working tree instead of its
/// pinned cache snapshot, because the workspace's per-machine location file maps
/// the dependency's name to a path. Informational, not a problem: the local tree
/// is editable and, unlike an immutable cache snapshot, it is watched and
/// fingerprinted like any project member. It exists so a forgotten override does
/// not read as a stale or missing dependency. The dependency-side sibling of a
/// Cargo path / `[patch]` override.
pub const DEPENDENCY_PATH_OVERRIDDEN: DiagnosticCode =
    DiagnosticCode::from_static("dependency-path-overridden");
/// A dependency's declared subpath is absolute or escapes its cache snapshot
/// under lexical normalization (a `../..` that climbs above the snapshot root).
/// The subpath is untrusted: a transitive peer's subpath is read verbatim from a
/// FETCHED repo's `.arsumbris/repo.yaml`, so an unchecked join would
/// mount an arbitrary local directory (and persist the escaping path into the
/// lock). The engine refuses the subpath, the dependency is not mounted. A
/// containment guard, not an ergonomic knob.
pub const DEPENDENCY_PATH_ESCAPES_SNAPSHOT: DiagnosticCode =
    DiagnosticCode::from_static("dependency-path-escapes-snapshot");
/// A transitive locked dependency's snapshot is absent from the device cache, so
/// it cannot be mounted offline. Advisory, not blocking: the workspace still
/// assembles from the rest, but the dependency's types are unavailable until a
/// `resolve` fetches its pinned commit. Without this the transitive dep was
/// silently unmounted, surfacing only later as an unresolved `::repo`. A manifest
/// member gets a role-keyed unmounted code instead (`edit-member-unmounted` /
/// `discover-member-unmounted` / `peer-unmounted`), so this fires only for a
/// transitive (lock-only) dependency.
pub const DEPENDENCY_CACHE_MISS: DiagnosticCode =
    DiagnosticCode::from_static("dependency-cache-miss");

pub(crate) const REGISTRY_REL: &str = ".arsumbris/repo.yaml";
pub(crate) const REPO_LOCK_REL: &str = ".arsumbris/repo.lock";

/// A repo's declared identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoName(pub String);

impl RepoName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One registry entry: a repo's identity, the shared shape of `self`, each
/// `peers` entry, and each workspace `member`.
///
/// Identity only, machine-independent. Location lives in a separate per-machine
/// file, see
/// [[spec - repo registry and workspace manifest - each repo declares self and peers, the workspace file scopes members]].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoEntry {
    pub name: RepoName,
    pub description: Option<String>,
    pub remote: Option<String>,
    /// The git ref a dependency resolves at, from a `dependencies:` entry's
    /// `ref:`. A branch, tag, or sha. Only meaningful on a dependency member,
    /// `None` on `self`, a peer, or a project member.
    pub git_ref: Option<String>,
    /// A subpath within the fetched repo, from a `dependencies:` entry's `path:`.
    /// The package is the subtree at `<repo>/<path>`, the monorepo-subpackage
    /// case. `None` means the repo root, the common case.
    pub path: Option<String>,
}

/// One repo: its root directory, declared identity, and the deps it depends on.
///
/// The `repo.yaml` root is the identity, `name` plus `description` / `remote`;
/// `deps` is the set of foreign repos it references and must carry, so the repo
/// stays a self-contained unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub root: PathBuf,
    pub name: RepoName,
    /// `false` for an isolated FALLBACK repo: a member root whose
    /// `.arsumbris/repo.yaml` could not become a declared repo (a duplicate name,
    /// or a parse error), mounted on its own so its files never silently merge
    /// into an enclosing repo. Every folder-repo entry and every resolved member
    /// is `declared: true`; the entry model forbids an undeclared entry, so the
    /// fallback is a safety floor, not the routine path.
    pub declared: bool,
    /// `true` only for the compiled-in `au-engine` repo, whose `root` is a
    /// sentinel and whose defs are the hardwired `au.engine.*` schemas, never
    /// read from disk. It is present in every built knowledge base so `::au-engine`
    /// resolves as a universal peer; consumers of `root()` and member-facing
    /// reads skip it, it is an identity, not a workspace member. See
    /// [`crate::engine_schema`].
    pub builtin: bool,
    pub description: Option<String>,
    pub remote: Option<String>,
    /// The repos this repo declares it depends on (`deps:`), by `::repo` link.
    /// Empty for the implicit fallback repo.
    pub deps: Vec<RepoEntry>,
    /// Each peer's resolved path on this machine, from the per-machine location
    /// file beside this repo's registry. A peer absent here is unmounted, not
    /// present on this machine. Empty when no location file is read.
    pub peer_paths: BTreeMap<RepoName, PathBuf>,
}

/// A workspace member's role, the list it entered scope through.
///
/// A neutral mechanical fact the engine reports; consumers apply policy. The
/// host hides consumed members from the editable tree, the cage scopes a caged
/// agent's writes to the editable authoring surfaces. One role per member, the
/// editable role wins when a repo would carry more than one (the entry or an
/// `edit` member over a `dep` / `discover`), see
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// The entry repo, the workspace home, an editable authoring surface.
    Entry,
    /// A `workspace.yaml edit:` member, an editable authoring surface.
    Edit,
    /// A `workspace.yaml discover:` member, mounted so type-discovery composes
    /// its parts, consumed not editable, not a declared type-dependency.
    Discover,
    /// A member reached through a `deps` edge, a pinned type-dependency,
    /// consumed not editable.
    Dep,
}

impl MemberRole {
    /// The wire token, `entry` / `edit` / `discover` / `dep`.
    pub fn as_str(self) -> &'static str {
        match self {
            MemberRole::Entry => "entry",
            MemberRole::Edit => "edit",
            MemberRole::Discover => "discover",
            MemberRole::Dep => "dep",
        }
    }

    /// Whether the role is an editable authoring surface (the entry or an `edit`
    /// member) versus a consumed member (a `dep` or a `discover`). Role-derived,
    /// independent of where the member resolved on disk.
    pub fn editable(self) -> bool {
        matches!(self, MemberRole::Entry | MemberRole::Edit)
    }

    /// The one-role precedence, higher wins when a member is reached more than one
    /// way. The editable roles win (the entry, then `edit`); among consumed, a
    /// declared `dep` subsumes a `discover` (the dep already mounts and pins it,
    /// so a matching `discover` listing is redundant). See
    /// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
    pub(crate) fn rank(self) -> u8 {
        match self {
            MemberRole::Entry => 4,
            MemberRole::Edit => 3,
            MemberRole::Dep => 2,
            MemberRole::Discover => 1,
        }
    }
}

/// A workspace: its `edit` / `discover` composition, the members it resolves
/// to, and their local paths.
///
/// The composition (`.arsumbris/workspace.yaml`) declares three optional lists,
/// `edit:` (editable members), `discover:` (pinned members mounted for
/// discovery), and `disabled:` (declared but intentionally not mounted). The
/// members are the entry repo plus its `deps`, plus the `edit` and `discover`
/// members and their transitive `deps`, minus any `disabled` name, located by
/// the resolution order (co-present sibling, then the per-user registry, then
/// the cache), see [`assemble_members`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The composition anchor: the entry folder's `.arsumbris/workspace.yaml`.
    pub manifest_path: PathBuf,
    /// The workspace name.
    pub name: String,
    /// The `edit:` composition, the named editable members.
    pub edit: Vec<RepoName>,
    /// The `discover:` composition, the named pinned discovery members.
    pub discover: Vec<RepoName>,
    /// The `disabled:` overlay, names declared but intentionally not mounted. A
    /// name here is excluded from the `edit` / `discover` selection fed to
    /// resolution, so it never mounts and fires no unmounted diagnostic, while
    /// staying in `edit` / `discover` so its declared role is remembered for the
    /// members read. See [[spec - workspace as a folder-repo - an optional
    /// workspace.yaml composes edit and discover members]].
    pub disabled: Vec<RepoName>,
    /// The members in scope, identity entries. The `edit` / `discover` selection
    /// plus the transitive `deps` closure, name-sorted.
    pub members: Vec<RepoEntry>,
    /// Each member's resolved LOCAL path (a co-present sibling or a registry
    /// path). A cache-mounted dependency is added here by
    /// `locate_locked_dependencies`, preserving the sibling → registry → cache
    /// order. A member absent here is unmounted.
    pub member_paths: BTreeMap<RepoName, PathBuf>,
    /// Each member's role, the list it entered scope through. An `edit` member
    /// is editable, a `discover` or a computed transitive dependency is
    /// consumed, see [`MemberRole`].
    pub member_roles: BTreeMap<RepoName, MemberRole>,
    /// Advisory notes raised while resolving the members (a duplicate sibling,
    /// an identity conflict), for the caller to render as diagnostics.
    pub member_notes: Vec<(RepoName, ResolveNote)>,
}

/// The repos discovered in a workspace, with nearest-ancestor membership.
#[derive(Debug, Clone, Default)]
pub struct RepoMap {
    repos: Vec<Repo>,
    /// `repo root -> indices into `repos``, built once at construction. `repo_of`
    /// resolves a path by walking its ancestors against this, O(path_depth)
    /// instead of an O(repos) scan — the dominant whole-build cost at high repo
    /// counts. Kept in sync by the two mutators: `resolve_deps` changes only
    /// `peer_paths` (roots unchanged), and `install_engine_builtin` appends one
    /// entry (existing indices stay valid under a push).
    ///
    /// A `BTreeMap`, not a `HashMap`: the derived `Debug` feeds the knowledge-base-parity
    /// check, so its key order must be deterministic across builds.
    root_index: BTreeMap<PathBuf, Vec<usize>>,
}

impl RepoMap {
    /// The repo a path belongs to, the deepest repo root that is an ancestor.
    ///
    /// Depth alone is not a total order. Two distinct roots cannot both be
    /// ancestors of one path at equal depth (a shared component-prefix forces
    /// the shorter to nest in the longer), so the only tie is two repos
    /// sharing the identical root — a member-path collision, diagnosed
    /// elsewhere. The repo name breaks that tie, so `repo_of` is deterministic
    /// regardless of the `repos` vector's order even in that degenerate case.
    /// Names are unique by construction (discovery disambiguates duplicates).
    pub fn repo_of(&self, path: &Path) -> Option<&Repo> {
        // The longest-prefix root is the DEEPEST ancestor of `path` that is a repo
        // root. `ancestors()` yields deepest-first, so the first hit is that root,
        // matching the former `max_by(root component count)`. Among repos sharing
        // one root (a diagnosed collision) the max-by-name tie-break matches the
        // former `.then_with(name)`. O(path_depth) hash lookups, not O(repos).
        for ancestor in path.ancestors() {
            if let Some(idxs) = self.root_index.get(ancestor) {
                let idx = *idxs
                    .iter()
                    .max_by(|a, b| {
                        self.repos[**a]
                            .name
                            .as_str()
                            .cmp(self.repos[**b].name.as_str())
                    })
                    .expect("a root_index bucket is never empty");
                return Some(&self.repos[idx]);
            }
        }
        None
    }

    /// A repo by its declared name.
    pub fn by_name(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.name.as_str() == name)
    }

    /// The workspace-tree root repo, the one rooted at the shallowest path.
    ///
    /// Present whenever the workspace is a single walked tree, the only shape
    /// `build` represents today: discovery guarantees a repo at the walked
    /// root, declared or implicit, so single / mono / nested always have one.
    /// A scattered multi-repo workspace, members with no common ancestor, has
    /// no such root; that topology is not yet assembled, and the type-name
    /// reads that lean on this will carry an explicit repo scope when it lands.
    pub fn root(&self) -> Option<&Repo> {
        self.repos
            .iter()
            .filter(|r| !r.builtin)
            .min_by_key(|r| r.root.components().count())
    }

    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    /// Install the compiled-in `au-engine` repo, the fixed identity that owns
    /// the hardwired `au.engine.*` schema defs. Its sentinel root matches no
    /// walked file, so `repo_of` never routes a real file to it and `root()`
    /// skips it; its graph is seeded from [`crate::engine_schema`], not from
    /// disk. Present in every built knowledge base so `::au-engine` resolves as a
    /// universal peer. Idempotent: a second call is a no-op.
    pub(crate) fn install_engine_builtin(&mut self) {
        if self
            .repos
            .iter()
            .any(|r| r.name.as_str() == crate::engine_schema::BUILTIN_ENGINE_REPO)
        {
            return;
        }
        self.repos.push(crate::engine_schema::builtin_engine_repo());
        // Keep the root index in sync. The builtin's sentinel root is never an
        // ancestor of a real path, so this entry is never returned; indexing it
        // anyway keeps the invariant "every repo is in the index" simple.
        let idx = self.repos.len() - 1;
        self.root_index
            .entry(self.repos[idx].root.clone())
            .or_default()
            .push(idx);
    }

    /// Build a `RepoMap` from an explicit member set, indexing each repo's root
    /// for `repo_of`. The one construction seam, so the index is always built.
    pub(crate) fn from_repos(repos: Vec<Repo>) -> Self {
        let mut root_index: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for (i, r) in repos.iter().enumerate() {
            root_index.entry(r.root.clone()).or_default().push(i);
        }
        RepoMap { repos, root_index }
    }

    /// Resolve each repo's declared deps to local paths via the resolution
    /// order, filling `peer_paths`, the tier-based successor to the per-machine
    /// location file.
    ///
    /// Each dep name resolves sibling → registry → cache, see
    /// [`resolve_repo_name`]; a resolved dep lands in `peer_paths`, an unmounted
    /// one is absent (the legitimate-absent state, `peer-unmounted`). The cache
    /// tier is not consulted here: a cache-mounted dep is a discovered repo, so
    /// the caller's mounted-check honors it. The advisory notes each dep raised
    /// are returned tagged by the declaring repo, for the caller to render as
    /// diagnostics with spans.
    pub fn resolve_deps(
        &mut self,
        registry: &UserRegistry,
        fs: &impl FileSystem,
    ) -> Vec<(RepoName, ResolveNote)> {
        let roots: Vec<PathBuf> = self.repos.iter().map(|r| r.root.clone()).collect();
        let siblings = SiblingIndex::from_roots(&roots, fs);
        let mut notes = Vec::new();
        // `roots`, `registry`, and the cache lookup are all fixed across this
        // loop, so the resolution is a pure function of the NAME alone. A
        // workspace resolves the same name once per depender otherwise, which on
        // a real corpus is most of the work: many repos declaring a handful of
        // shared dependencies.
        //
        // The notes are replayed per DEPENDER rather than memoized away, since
        // each one is reported against the repo that declared the dep.
        let mut seen: BTreeMap<RepoName, Resolved> = BTreeMap::new();
        for repo in &mut self.repos {
            let dep_names: Vec<RepoName> = repo.deps.iter().map(|d| d.name.clone()).collect();
            let mut paths = BTreeMap::new();
            for name in &dep_names {
                let resolved = seen
                    .entry(name.clone())
                    .or_insert_with(|| resolve_repo_name(name, &siblings, registry, |_| None, fs));
                if let ResolveOutcome::Local(p) | ResolveOutcome::Cache(p) = &resolved.outcome {
                    paths.insert(name.clone(), p.clone());
                }
                for note in &resolved.notes {
                    notes.push((repo.name.clone(), note.clone()));
                }
            }
            repo.peer_paths = paths;
        }
        notes
    }
}

/// A workspace's resolved-dependency lockfile: each dependency name pinned to
/// the concrete commit sha it resolved to, plus the remote it was fetched from.
///
/// The solve, committed per-repo in `.arsumbris/repo.lock`, machine-independent.
/// A version pin is merged by text. Entries iterate sorted by name.
pub type PackageLock = BTreeMap<RepoName, LockedPackage>;

/// One dependency's pin in a [`PackageLock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    /// The remote the dependency was fetched from, so the pin re-fetches without
    /// re-consulting the registry.
    pub remote: String,
    /// The resolved commit sha, the content-exact pin the cache is keyed on.
    pub sha: String,
    /// The subpath the package sits at within the repo, the monorepo case.
    /// `None` means the repo root.
    pub path: Option<String>,
}

/// The path of a repo's dependency lockfile, `<root>/.arsumbris/repo.lock`,
/// beside its `repo.yaml`.
///
/// Per-repo, because deps are declared per-repo in `repo.yaml`, so the solve that
/// pins them belongs to the repo, not the workspace. Committed and
/// machine-independent. Each repo's lock pins its OWN transitive fetched closure,
/// so the repo is reproducible standalone.
/// A member root in the ONE spelling the engine holds it under.
///
/// Every path identity in the knowledge base derives from a member root: the
/// walk produces catalog keys under it, `Repo.root` records it, and the write
/// path rejoins tree-relative paths onto it. So two spellings of one directory
/// (one reached through a symlink) would be two identities for the same files.
///
/// The consequences are not theoretical. Two spellings mount one physical git
/// tree as two saga members, which lands two commits carrying one `Mutation-Id`
/// where compensation can only revert one, a half-apply reported as success.
///
/// Normalizing HERE, at the assembly boundary, is what lets everything
/// downstream stay lexical: a lexical ancestor walk over a canonical root yields
/// a canonical answer, and a lexical join reproduces a catalog key exactly.
///
/// Falls back to the path as given when it cannot be resolved (it does not exist
/// yet, or the filesystem has no notion of canonical, as the in-memory one does
/// not). A member root that does not exist mounts nothing, so the fallback keeps
/// the absent case behaving as before rather than inventing a path.
pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub fn repo_lock_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".arsumbris").join("repo.lock")
}

/// The `.arsumbris/workspace.lock` path for a folder-repo entry, beside its
/// `workspace.yaml`. Per entry-repo, engine-written, it pins the workspace's
/// `discover` closure (the discover members plus their transitive deps) so those
/// members mount reproducibly and offline. Parallel to [`repo_lock_path`] and the
/// same `au.engine.repo-lock` `packages` shape; an `edit` member stays live and
/// is never locked here. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub fn workspace_lock_path(entry_root: &Path) -> PathBuf {
    entry_root.join(".arsumbris").join("workspace.lock")
}

/// Parse a workspace's package lockfile into per-dependency pins.
///
/// Shape, a `packages` sequence of `{ name, remote, sha }`. Malformed input
/// degrades to an empty map; an entry missing a field is dropped, the lock is
/// regenerable so a dropped entry re-resolves rather than reading wrong.
pub fn parse_package_lock(bytes: &[u8]) -> PackageLock {
    let mut out = PackageLock::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return out;
    };
    let Ok(docs) = parse(text) else {
        return out;
    };
    let Some(doc) = docs.first() else {
        return out;
    };
    let Some(node) = field(doc, "packages") else {
        return out;
    };
    let YamlData::Sequence(items) = &node.data else {
        return out;
    };
    for item in items {
        let (Some(name), Some(remote), Some(sha)) = (
            field(item, "name").and_then(scalar_string),
            field(item, "remote").and_then(scalar_string),
            // The sha can be all-digits, so read its raw lexeme, not a typed scalar.
            field(item, "sha").and_then(|n| scalar_text(n, text)),
        ) else {
            continue;
        };
        let path = field(item, "path").and_then(scalar_string);
        out.insert(RepoName(name), LockedPackage { remote, sha, path });
    }
    out
}

/// Serialize a [`PackageLock`] to its on-disk text, the inverse of
/// [`parse_package_lock`]. Entries iterate sorted by name, so the output is
/// deterministic and merges by text.
pub fn serialize_package_lock(lock: &PackageLock, type_name: &str) -> String {
    // Engine-written files self-describe with a qualified `type:` so they carry
    // no `engine-schema-type-unwritten` drift. The lock reader ignores the key
    // (it reads only `packages`), see [`parse_package_lock`]. See
    // [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
    let mut out = format!(
        "type: {type_name}::{}\npackages:\n",
        crate::engine_schema::BUILTIN_ENGINE_REPO
    );
    for (name, pkg) in lock {
        out.push_str(&format!(
            "  - name: {}\n    remote: {}\n    sha: {}\n",
            name.as_str(),
            pkg.remote,
            pkg.sha
        ));
        if let Some(path) = &pkg.path {
            out.push_str(&format!("    path: {path}\n"));
        }
    }
    out
}

/// One package's entry in the package registry: where to fetch it and at which
/// audited ref, plus an optional human description.
///
/// The registry maps a package name to this. A name-only dependency resolves
/// through it. See [[spec - package manager - git-ref dependencies resolved through a registry repo into a device-local cache]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub remote: String,
    pub git_ref: String,
    /// A subpath within the fetched repo, the monorepo-subpackage case. `None`
    /// means the repo root.
    pub path: Option<String>,
    pub description: Option<String>,
}

/// Parse a package registry file (`registry.yaml`) into name to entry.
///
/// Shape, a `packages` sequence of `{ name, remote, ref, description? }`.
/// Malformed input degrades to an empty map; an entry missing `name`, `remote`,
/// or `ref` is dropped.
pub fn parse_package_registry(bytes: &[u8]) -> BTreeMap<RepoName, RegistryEntry> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return out;
    };
    let Ok(docs) = parse(text) else {
        return out;
    };
    let Some(doc) = docs.first() else {
        return out;
    };
    let Some(node) = field(doc, "packages") else {
        return out;
    };
    let YamlData::Sequence(items) = &node.data else {
        return out;
    };
    for item in items {
        let (Some(name), Some(remote), Some(git_ref)) = (
            field(item, "name").and_then(scalar_string),
            field(item, "remote").and_then(scalar_string),
            // The ref can be a sha, which may be all-digits, so read its lexeme.
            field(item, "ref").and_then(|n| scalar_text(n, text)),
        ) else {
            continue;
        };
        let path = field(item, "path").and_then(scalar_string);
        let description = field(item, "description").and_then(scalar_string);
        out.insert(
            RepoName(name),
            RegistryEntry {
                remote,
                git_ref,
                path,
                description,
            },
        );
    }
    out
}

/// A per-user registry entry's location: the repo's remote and its local path
/// on this machine. The locator the resolver uses for a repo that is not
/// co-present as a sibling, see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryLocation {
    /// The repo's git remote, a recorded convenience sourced from the owner.
    pub remote: Option<String>,
    /// The repo's local path on this machine.
    pub path: PathBuf,
}

/// The per-user registry, `~/.arsumbris/au-engine/config/repos.yaml`: every known
/// repo name to its `{ remote, path }`. Engine-written, read on build, the locator
/// for repos that are not co-present as siblings.
pub type UserRegistry = BTreeMap<RepoName, RegistryLocation>;

/// Parse `repos.yaml` bytes into `name -> { remote, path }`.
///
/// A `repos:` sequence of `{ name, path, remote? }` mappings. An entry with no
/// non-empty `name` or no non-empty `path` is skipped, it cannot locate a repo.
/// Malformed input (bad UTF-8, bad YAML, or no `repos:` list) degrades to an
/// empty registry, advisory, never a block, per the resolution spec.
pub fn parse_user_registry(bytes: &[u8]) -> UserRegistry {
    let mut out = UserRegistry::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return out;
    };
    let Ok(docs) = parse(text) else {
        return out;
    };
    let Some(doc) = docs.first() else {
        return out;
    };
    let Some(repos) = field(doc, "repos") else {
        return out;
    };
    let YamlData::Sequence(items) = &repos.data else {
        return out;
    };
    for item in items {
        let Some(name) = field(item, "name")
            .and_then(scalar_string)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let Some(path) = field(item, "path")
            .and_then(scalar_string)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let remote = field(item, "remote").and_then(scalar_string);
        out.insert(
            RepoName(name),
            RegistryLocation {
                remote,
                path: PathBuf::from(path),
            },
        );
    }
    out
}

/// `$HOME` is unset or empty, so the device area `~/.arsumbris` cannot be located.
///
/// The device root requires `$HOME`. Unset or empty is a loud refusal, not a
/// silently-degraded state: a device area with no `$HOME` is unresolvable, not
/// partially usable. See
/// [[spec - arsumbris layout - a reserved multi-tenant device root, owner-namespaced with a category sublayer]].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeUnset;

impl std::fmt::Display for HomeUnset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "$HOME must be set to locate the device area ~/.arsumbris"
        )
    }
}

impl std::error::Error for HomeUnset {}

/// The device-global root, `canonical($HOME)/.arsumbris`.
///
/// The single source for the device registries, the package cache, and the
/// daemon socket, and the location the reservation guard refuses a repo from
/// rooting at. Built from a canonical `$HOME` so the written location and the
/// guarded location are one spelling.
///
/// `$HOME` is required: unset or empty is [`HomeUnset`], so the daemon refuses to
/// start rather than binding a socket in a temp dir with no registry. The
/// injected config port ([`ConfigSource::Dir`] / [`ConfigSource::Empty`]) never
/// consults `$HOME`, so tests and headless runs do not hit this.
pub fn device_root() -> Result<PathBuf, HomeUnset> {
    device_root_from(std::env::var_os("HOME"))
}

/// [`device_root`] over an explicit `$HOME`, the determinism seam for tests.
fn device_root_from(home: Option<std::ffi::OsString>) -> Result<PathBuf, HomeUnset> {
    match home {
        Some(h) if !h.is_empty() => Ok(canonical_root(&PathBuf::from(h)).join(".arsumbris")),
        _ => Err(HomeUnset),
    }
}

/// Whether `root` is the reserved device root: a repo rooting there would make its
/// `.arsumbris/` BE the device config/data area.
///
/// EQUALITY, never a subtree test. The package cache lives UNDER `~/.arsumbris`,
/// so a prefix test would refuse every cache-mounted member. Both sides are
/// canonical ([`device_root`] canonicalizes `$HOME`, this canonicalizes `root`),
/// so a symlinked or case-differing home cannot slip past. `false` when `$HOME` is
/// unset, there is then no device area to collide with. See
/// [[spec - arsumbris layout - a reserved multi-tenant device root, owner-namespaced with a category sublayer]].
pub(crate) fn is_reserved_root(root: &Path) -> bool {
    device_root().is_ok_and(|dev| reserved_against(root, &dev))
}

/// [`is_reserved_root`] against an explicit device root, the determinism seam.
fn reserved_against(root: &Path, device_root: &Path) -> bool {
    canonical_root(root).join(".arsumbris") == device_root
}

/// Where the per-user config (`repos.yaml`, `workspaces.yaml`) is read from.
///
/// STATED, never defaulted. The former `Option<&Path>` used `None` to mean "the
/// developer's real `$HOME`", so forgetting to inject silently reached the
/// machine's own registry — a test could then resolve a dep against a real repo
/// on disk and pass only on that laptop. The three states are now distinct and
/// a caller must pick one, so reading the real config is a visible, greppable
/// choice rather than the thing that happens when nobody says otherwise.
///
/// See [[repos yaml::au-type-system]]: "the injected path is the build's determinism seam".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// The real per-user config under the device root, `~/.arsumbris`. The
    /// daemon's choice; a run with no `$HOME` reads nothing.
    User,
    /// An explicit device-root directory, standing in for `~/.arsumbris`. A test
    /// injects a tempdir when it exercises the registry itself, reads or `register`
    /// writes; the same `au-engine/config/` interior is joined onto it.
    Dir(PathBuf),
    /// No per-user config at all: the registry is empty, resolution falls to
    /// co-present siblings and the cache. The hermetic default for a test that
    /// does not care about the registry, and the reason forgetting to choose is
    /// no longer possible — there is nothing to forget.
    Empty,
}

impl ConfigSource {
    /// The device-root base directory (`~/.arsumbris`), `None` when there is no
    /// per-user config to read (`Empty`, or `User` with no `$HOME`).
    pub fn base(&self) -> Option<PathBuf> {
        match self {
            ConfigSource::User => device_root().ok(),
            ConfigSource::Dir(dir) => Some(dir.clone()),
            ConfigSource::Empty => None,
        }
    }
}

/// The per-user registry file, `~/.arsumbris/au-engine/config/repos.yaml`.
///
/// `None` when the source resolves no base, so the registry is empty and
/// resolution falls to siblings and the cache only.
pub fn user_registry_path(config: &ConfigSource) -> Option<PathBuf> {
    config
        .base()
        .map(|base| base.join("au-engine").join("config").join("repos.yaml"))
}

/// Build the scoped-config-channel path for one consumer file, under the given
/// `.arsumbris` directory: the device root `~/.arsumbris` at machine scope, or
/// `<repo>/.arsumbris` at repo scope. Both then join `<consumer>/config/<file>`,
/// so the two scopes share one construction.
///
/// This is a SANCTIONED bypass of the `.arsumbris/` write-guard, so the two
/// caller-controlled segments carry no guard behind them and MUST be sanitized
/// here. `consumer` and `file` are each rejected when empty, when they start with
/// a dot (which covers `.`, `..`, and a hidden name), or when they carry a path
/// separator or a control character (which covers an absolute path). And
/// `consumer` equal to the engine's own writing-tenant name (`au-engine`) is
/// reserved: a machine-scope path there would resolve straight onto the engine's
/// own `~/.arsumbris/au-engine/config/repos.yaml`, so the channel refuses it,
/// keeping the engine's own config separate by mechanism. See
/// [[spec - scoped config channel - a config read and set_config mutation over scope, consumer, file, type]].
///
/// `Err(reason)` names the offending segment; nothing is built, so a caller
/// rejects loudly rather than writing outside `<consumer>/config/`.
pub(crate) fn config_path(
    arsumbris_dir: &Path,
    consumer: &str,
    file: &str,
) -> Result<PathBuf, String> {
    check_config_segments(consumer, file)?;
    Ok(arsumbris_dir.join(consumer).join("config").join(file))
}

/// The path-safety gate on its own, for a caller that must validate the segments
/// before a scope base is known (a machine-scope read with `$HOME` unset still
/// rejects a malformed `consumer` / `file`). [`config_path`] runs this before it
/// joins.
///
/// The engine's own `au-engine` owner segment is reserved on BOTH scopes. Only
/// machine scope can actually collide (its `~/.arsumbris/au-engine/config/` IS the
/// engine's device registry); repo scope cannot, but the engine's name is
/// genuinely reserved everywhere, so the check is uniform rather than
/// scope-conditional. The compare is CASE-INSENSITIVE: on a case-insensitive
/// filesystem (macOS APFS default) `Au-Engine/config/repos.yaml` and
/// `au-engine/config/repos.yaml` are the same inode, so a case-variant `consumer`
/// would otherwise slip past the reserve and clobber the registry.
pub(crate) fn check_config_segments(consumer: &str, file: &str) -> Result<(), String> {
    reject_config_segment("consumer", consumer)?;
    reject_config_segment("file", file)?;
    if consumer.eq_ignore_ascii_case(crate::engine_schema::BUILTIN_ENGINE_REPO) {
        return Err(format!(
            "consumer '{consumer}' is reserved for the engine's own config and cannot be written through this channel"
        ));
    }
    Ok(())
}

/// The structural path-safety gate for one caller-controlled config segment.
/// Rejects a segment that could escape `<consumer>/config/`: empty, dot-leading
/// (`.`, `..`, hidden), or carrying a separator or control character (an absolute
/// path starts with a separator, so it is caught here too).
fn reject_config_segment(kind: &str, seg: &str) -> Result<(), String> {
    if seg.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if seg.starts_with('.') {
        return Err(format!(
            "{kind} '{seg}' must not start with a dot (no `.`, `..`, or hidden segment)"
        ));
    }
    if seg.contains(['/', '\\']) {
        return Err(format!("{kind} '{seg}' must not contain a path separator"));
    }
    if seg.contains(['\0', '\n', '\r']) {
        return Err(format!(
            "{kind} '{seg}' must not contain a control character"
        ));
    }
    Ok(())
}

/// Load the per-user registry through `fs`, resolving its path with
/// [`user_registry_path`]. A missing file or any parse failure yields an empty
/// registry, advisory, never a block.
pub fn load_user_registry(config: &ConfigSource, fs: &impl FileSystem) -> UserRegistry {
    let Some(path) = user_registry_path(config) else {
        return UserRegistry::new();
    };
    match fs.read_file(&path) {
        Ok(bytes) => parse_user_registry(&bytes),
        Err(_) => UserRegistry::new(),
    }
}

/// Serialize a [`UserRegistry`] to the `repos.yaml` text, the inverse of
/// [`parse_user_registry`]. Name-sorted (the map is a `BTreeMap`), so the output
/// is deterministic.
pub fn serialize_user_registry(reg: &UserRegistry) -> String {
    // Self-describe so the engine-written device file carries no
    // `engine-schema-type-unwritten` drift. A device file validates in the
    // builtin `au-engine` graph's OWN scope, so the claim is BARE (a `::au-engine`
    // self-qualifier there would be a redundant `type-repo-self` hint), unlike an
    // in-repo file's qualified form. The reader ignores the key, see
    // [`parse_user_registry`]. See
    // [[spec - engine-schema file claims - the kind assigns a floor, a written type self-describes and mixes in more]].
    let mut out = String::from("type: au.engine.repos\nrepos:\n");
    for (name, loc) in reg {
        out.push_str(&format!("  - name: {}\n", name.0));
        if let Some(remote) = &loc.remote {
            out.push_str(&format!("    remote: {remote}\n"));
        }
        out.push_str(&format!("    path: {}\n", loc.path.display()));
    }
    out
}

/// The per-user workspaces index, `~/.arsumbris/au-engine/config/workspaces.yaml`:
/// a local alias `name` to the workspace-repo FOLDER it points at.
/// USER-AUTHORED, read only (never engine-written, unlike [`UserRegistry`]), the
/// lookup for `au open <name>`. Symmetric with `repos.yaml`: one indexes repo
/// locations, this indexes workspace-repo folders, see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
pub type UserWorkspaces = BTreeMap<String, PathBuf>;

/// Parse `workspaces.yaml` bytes into `name -> workspace-repo folder`.
///
/// A `workspaces:` sequence of `{ path, name? }` mappings, each `path` a
/// workspace-repo FOLDER (a directory carrying `.arsumbris/repo.yaml`, opened as a
/// folder-repo entry). The lookup name is the explicit `name`, else the folder
/// BASENAME. An entry with no non-empty `path`, or one whose path has no basename
/// (the filesystem root) and no explicit `name`, is skipped, it cannot be named. A
/// name claimed twice keeps the FIRST, deterministic. Malformed input (bad UTF-8,
/// bad YAML, or no `workspaces:` list) degrades to an empty index, advisory, never
/// a block.
///
/// This only POINTS at workspace folders, it never inlines a selection, so a
/// workspace has exactly one definition form (its committed `.arsumbris/workspace.yaml`)
/// and this only names its folder.
pub fn parse_user_workspaces(bytes: &[u8]) -> UserWorkspaces {
    let mut out = UserWorkspaces::new();
    let Ok(text) = std::str::from_utf8(bytes) else {
        return out;
    };
    let Ok(docs) = parse(text) else {
        return out;
    };
    let Some(doc) = docs.first() else {
        return out;
    };
    let Some(node) = field(doc, "workspaces") else {
        return out;
    };
    let YamlData::Sequence(items) = &node.data else {
        return out;
    };
    for item in items {
        let Some(path) = field(item, "path")
            .and_then(scalar_string)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let path = PathBuf::from(path);
        let name = field(item, "name")
            .and_then(scalar_string)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        if name.is_empty() {
            continue; // no explicit name and the path has no basename (the root).
        }
        out.entry(name).or_insert(path);
    }
    out
}

/// The per-user workspaces file, `~/.arsumbris/au-engine/config/workspaces.yaml`.
///
/// The `ConfigSource` selects the device-root base, the test injection seam,
/// mirroring [`user_registry_path`]. `Empty`, or `User` with no `$HOME`, returns
/// `None`, so the index is empty.
pub fn user_workspaces_path(config: &ConfigSource) -> Option<PathBuf> {
    config.base().map(|base| {
        base.join("au-engine")
            .join("config")
            .join("workspaces.yaml")
    })
}

/// Load the per-user workspaces index through `fs`, resolving its path with
/// [`user_workspaces_path`]. A missing file or any parse failure yields an empty
/// index, advisory, never a block.
pub fn load_user_workspaces(config: &ConfigSource, fs: &impl FileSystem) -> UserWorkspaces {
    let Some(path) = user_workspaces_path(config) else {
        return UserWorkspaces::new();
    };
    match fs.read_file(&path) {
        Ok(bytes) => parse_user_workspaces(&bytes),
        Err(_) => UserWorkspaces::new(),
    }
}

/// The outcome of a `register` write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// The entry was written or updated.
    Written,
    /// Refused, nothing written: an identity conflict
    /// (`dependency-identity-conflict`), or an invalid identity string (a
    /// non-grammar name, a newline-bearing path / remote). Both surface as a
    /// mutation reject frame carrying this reason.
    Conflict(String),
}

/// Register (write or update) one `{ name, remote, path }` entry in the per-user
/// registry at `registry_path`, the engine-owned device-scoped write, the
/// `set_ignores` pattern for a device file.
///
/// Refuses (`Conflict`, nothing written) on an invalid identity string (a
/// non-grammar name, a newline-bearing path / remote), when the name already maps
/// to a DIFFERENT remote, when the target path's `repo.yaml` declares a name other
/// than the key, or when the target has no readable / parseable `repo.yaml`
/// (register-before-clone is not supported, a registered path must be an existing
/// name-declaring checkout). This is the same identity check resolution runs, so a
/// mis-registration is caught before it lands, never silently resolved later. A
/// bare `{ name, path }` re-register PRESERVES the entry's recorded remote rather
/// than clearing it. Device-scoped, outside every repo, so NO git commit, unlike
/// the per-repo `.auignore`.
///
/// Uses `std::fs` directly, NOT the [`FileSystem`] port. The port is the knowledge base
/// read boundary (`read_file` / `walk_files`, read-only); `repos.yaml` is a
/// device-global file outside every knowledge base, and this WRITES it, so the port does
/// not apply. The write is atomic (temp sibling then `rename`), so a crash or a
/// reader never observes a truncated `repos.yaml` — which `parse_user_registry`
/// would silently degrade to an EMPTY registry, unmounting every registered repo
/// at once. The whole read-modify-write is guarded by an exclusive `flock` on a
/// sibling lock file, so two daemons registering concurrently SERIALIZE rather
/// than lose one update (a returned `Written` that a racing write overwrote). The
/// `flock` auto-releases on process death, so a crashed daemon never leaves a
/// stale lock.
pub fn register_entry(
    registry_path: &Path,
    name: &RepoName,
    remote: Option<&str>,
    path: &Path,
) -> std::io::Result<RegisterOutcome> {
    // Validate the identity strings BEFORE any I/O. The `name` and `path` arrive
    // over the wire (an agent may pass any string), so this is the boundary gate.
    // A name must be a valid repo name (also the `sibling_scan` path-component
    // safety gate, see `parse_entry`). A `path` or `remote` carrying a newline
    // would forge a second entry the next `parse_user_registry` silently accepts,
    // a bare-scalar breakout in the line-oriented registry, so reject it loudly
    // per silent-drops-worse-than-rejection.
    if !au_core::is_valid_type_name(name.0.as_str()) {
        return Ok(RegisterOutcome::Conflict(format!(
            "'{}' is not a valid repo name (a leading letter, then letters / digits / '_' / '-', dot-separated)",
            name.0
        )));
    }
    if path.to_string_lossy().contains(['\n', '\r']) {
        return Ok(RegisterOutcome::Conflict(
            "path contains a newline, which the line-oriented registry cannot represent"
                .to_string(),
        ));
    }
    if remote.is_some_and(|r| r.contains(['\n', '\r'])) {
        return Ok(RegisterOutcome::Conflict(
            "remote contains a newline".to_string(),
        ));
    }

    // Serialize the read-modify-write across processes: two daemons (from two
    // knowledge bases) registering into the one device-global `repos.yaml` at once would
    // otherwise each read → insert → rename, and the later rename would drop the
    // earlier's entry — a lost registration reported as `Written`. Hold an
    // exclusive `flock` on a sibling `.repos.yaml.lock` for the whole critical
    // section; it auto-releases when `registry_lock` drops (or the process dies), so a
    // crash never leaves a stale lock. The lock file's own contents are unused.
    if let Some(parent) = registry_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = lock_sibling(registry_path);
    let registry_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    fs4::fs_std::FileExt::lock_exclusive(&registry_lock)?;

    let mut current = std::fs::read(registry_path)
        .ok()
        .map(|b| parse_user_registry(&b))
        .unwrap_or_default();

    // Identity check 1: an existing entry with a disagreeing remote.
    if let Some(existing) = current.get(name) {
        if let (Some(old), Some(new)) = (existing.remote.as_deref(), remote) {
            if old != new {
                return Ok(RegisterOutcome::Conflict(format!(
                    "'{}' is already registered with remote '{old}', not '{new}'",
                    name.0
                )));
            }
        }
    }

    // Identity check 2: the target's own `repo.yaml` must declare this name, the
    // same check resolution runs, so a mis-registration is caught up front. An
    // absent or unparseable `repo.yaml` REJECTS rather than writing: a registered
    // path must be an existing checkout that declares the name, else the registry
    // would record a locator resolution can never validate (a non-mount later, not
    // an error at the write). Register-before-clone is deliberately not supported.
    let target = path.join(REGISTRY_REL);
    match std::fs::read(&target) {
        Ok(bytes) => match parse_registry(&target, &bytes) {
            Ok(reg) if reg.self_entry.name != *name => {
                return Ok(RegisterOutcome::Conflict(format!(
                    "path declares '{}', not '{}'",
                    reg.self_entry.name.0, name.0
                )));
            }
            Ok(_) => {}
            Err(_) => {
                return Ok(RegisterOutcome::Conflict(format!(
                    "path '{}' has an unparseable {REGISTRY_REL}; register a checkout that declares '{}'",
                    path.display(),
                    name.0
                )));
            }
        },
        Err(_) => {
            return Ok(RegisterOutcome::Conflict(format!(
                "path '{}' has no readable {REGISTRY_REL}; register an existing checkout that declares '{}'",
                path.display(),
                name.0
            )));
        }
    }

    // Preserve the existing remote when this register omits one, so a bare
    // `{ name, path }` re-register does not discard a recorded remote (which
    // would also disarm identity-check-1 for the next registration).
    let effective_remote = remote
        .map(|s| s.to_string())
        .or_else(|| current.get(name).and_then(|e| e.remote.clone()));

    current.insert(
        name.clone(),
        RegistryLocation {
            remote: effective_remote,
            path: path.to_path_buf(),
        },
    );
    // The parent directory was created above, before the lock was taken.
    atomic_write(registry_path, &serialize_user_registry(&current))?;
    Ok(RegisterOutcome::Written)
}

/// The sibling lock-file path for a device / config file: a hidden `.<name>.lock`
/// beside it, `flock`-ed to serialize the read-modify-write across processes.
/// Distinct from the file's own name and from `atomic_write`'s `.tmp` scratch.
fn lock_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("registry");
    path.with_file_name(format!(".{name}.lock"))
}

/// Write `contents` to `path` atomically: write a sibling temp file, then
/// `rename` it over `path`. The POSIX rename is atomic, so a reader (or a crash
/// mid-write) never observes a half-written file. The temp sibling shares the
/// target's directory so the rename stays on one filesystem; a per-process
/// suffix keeps two writers' scratch files from colliding. A failed rename
/// removes the temp file rather than leaking it. Used for the engine-owned
/// device / config files (the per-user registry, the per-repo locks).
pub(crate) fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("registry");
    // A per-PROCESS pid plus a per-CALL monotonic counter, so two threads (or two
    // atomic writes) never collide on the scratch file — a shared name would race
    // a `rename` (one consumes the temp, the other's rename then fails NotFound).
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write one machine-scope scoped-config file, `~/.arsumbris/<consumer>/config/<file>`,
/// under a cross-daemon exclusive lock, so two daemons writing one device-global
/// file serialize rather than tear. The same `flock`-on-a-sibling pattern
/// [`register_entry`] uses for `repos.yaml`, plus an atomic (temp + rename) write,
/// so no reader observes a half-written file. NO git commit, the file sits outside
/// every repo.
///
/// `expected_hash` is a compare-and-set against the current file, checked under
/// the lock: a mismatch (or an absent file) rejects with a reason, nothing
/// written. Returns the written content hash.
/// `produce` receives the CURRENT file content (read INSIDE the lock, `None` when
/// absent) and returns the new content. This keeps a keyed `edit` splice's read +
/// transform + write atomic against a racing daemon, the whole critical section
/// under the one `flock`. A whole-`content` write is the degenerate producer that
/// ignores `current`.
pub(crate) fn write_device_config_with(
    path: &Path,
    expected_hash: Option<&str>,
    produce: impl FnOnce(Option<&str>) -> Result<String, crate::mutate::MutationReject>,
) -> Result<crate::ir::ContentHash, crate::mutate::MutationReject> {
    use crate::mutate::MutationReject;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MutationReject::new(format!("cannot create parent directories: {e}")))?;
    }
    // Serialize the read-modify-write across processes on a sibling lock file. The
    // `flock` auto-releases when `lock` drops (or the process dies), so a crash
    // never leaves a stale lock. The lock file's own contents are unused.
    let lock_path = lock_sibling(path);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| {
            MutationReject::new(format!(
                "cannot open config lock {}: {e}",
                lock_path.display()
            ))
        })?;
    fs4::fs_std::FileExt::lock_exclusive(&lock)
        .map_err(|e| MutationReject::new(format!("cannot lock config: {e}")))?;

    let current = std::fs::read_to_string(path).ok();
    if let Some(expected) = expected_hash {
        match current.as_deref() {
            Some(cur) => {
                let current_hex =
                    crate::mutate::hash_hex(crate::ir::ContentHash::of(cur.as_bytes()));
                if current_hex != expected {
                    return Err(MutationReject {
                        message: format!(
                            "expected_hash mismatch: the file changed since it was read (current {current_hex})"
                        ),
                        detail: Some(serde_json::json!({ "current_hash": current_hex })),
                    });
                }
            }
            None => {
                return Err(MutationReject::new(
                    "expected_hash given but the file does not exist",
                ))
            }
        }
    }
    let new = produce(current.as_deref())?;
    atomic_write(path, &new)
        .map_err(|e| MutationReject::new(format!("cannot write {}: {e}", path.display())))?;
    Ok(crate::ir::ContentHash::of(new.as_bytes()))
}

/// The outcome of a sibling scan for one wanted repo name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiblingScan {
    /// Exactly one co-present sibling declares the name, at this root.
    Found(PathBuf),
    /// No co-present sibling declares the name.
    NotFound,
    /// Two or more distinct siblings declare the name: refuse to pick.
    Ambiguous(Vec<PathBuf>),
}

/// Resolve `name` among the co-present repos, the tier-1 resolver, the zero-config
/// monorepo case. Resolution is by DECLARED name, the marker is the definitive
/// repo-root indicator.
///
/// Two candidate sources, each confirmed by the target's declared name so the
/// declared name is always the key:
/// - each located root ITSELF, a marker-surfaced repo. Matched by its declared
///   name, so folder-name drift never hides it: a repo declaring `lib` in a
///   `mylib/` folder still resolves (X).
/// - `parent(root)/<name>`, the folder-name convention for a beside-the-entry
///   sibling that has no surfaced marker (it sits outside the walked tree). A
///   folder-drifted beside sibling is not found here and falls to the registry.
///
/// A name-read only (not full validation, a `repo.yaml` malformed elsewhere still
/// resolves by its readable name), and it NEVER writes the registry (it takes
/// `&fs`). Two distinct repos declaring one name are `Ambiguous`, never an
/// arbitrary pick, so the determinism invariant holds, see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
pub fn sibling_scan(
    name: &RepoName,
    located_roots: &[PathBuf],
    fs: &impl FileSystem,
) -> SiblingScan {
    SiblingIndex::from_roots(located_roots, fs).scan(name, fs)
}

/// The located roots, indexed by the name each one DECLARES.
///
/// The scan's two candidate sources answer to different structures, and reading
/// every root's `repo.yaml` on every scan conflates them:
/// - a located root is matched by its declared name, which is a LOOKUP. Reading
///   every root to answer it is a linear probe of data that does not vary by the
///   name being resolved.
/// - `parent(root)/<name>` genuinely varies by name, so it stays a probe, but one
///   per DISTINCT PARENT rather than one per root.
///
/// Substituting the lookup for the probe is sound because the scan's result is a
/// SET, not a sequence: it dedups its candidates and sorts its matches, so any
/// construction yielding the same matching roots yields the same outcome and the
/// same diagnostic bytes. See [`SiblingIndex::scan`].
///
/// Built from the declared name at each root, never from an already-assembled
/// `RepoMap`: [`discover_repos`] DROPS the second repo declaring a duplicate name
/// and remounts its root under a uniquified one, so a map-derived index would see
/// one winner where the scan must see an ambiguity, and silently resolve a name
/// that should refuse to resolve.
#[derive(Debug, Default, Clone)]
pub struct SiblingIndex {
    /// Declared name to the roots declaring it. A `BTreeSet` so the matches come
    /// out sorted and deduped, which is the order the scan's `Ambiguous` list and
    /// its diagnostic message carry.
    by_name: BTreeMap<RepoName, BTreeSet<PathBuf>>,
    /// The distinct parent directories of the located roots, the only directories
    /// a beside-the-entry probe can land in.
    parents: BTreeSet<PathBuf>,
    /// Roots already folded in, so growth is idempotent and re-reads nothing.
    known: BTreeSet<PathBuf>,
}

impl SiblingIndex {
    /// Read each located root's declared name once.
    pub fn from_roots(roots: &[PathBuf], fs: &impl FileSystem) -> Self {
        let mut index = SiblingIndex::default();
        for root in roots {
            index.insert_root(root, fs);
        }
        index
    }

    /// Fold one more located root in, for a caller whose root set grows as it
    /// resolves. Idempotent: a root already folded in is not re-read.
    pub fn insert_root(&mut self, root: &Path, fs: &impl FileSystem) {
        if !self.known.insert(root.to_path_buf()) {
            return;
        }
        // Declared name wins, so a folder whose repo declares a different name is
        // not a match, and a drifted repo is matched by its declared name.
        if let Some(name) = declared_name_at(root, fs) {
            self.by_name
                .entry(name)
                .or_default()
                .insert(root.to_path_buf());
        }
        if let Some(parent) = root.parent() {
            self.parents.insert(parent.to_path_buf());
        }
    }

    /// Tier 1 of resolution: the located roots declaring `name`, plus any
    /// beside-the-entry sibling sitting at `parent/<name>`.
    ///
    /// The parent probe is the one position where a FOLDER name is a resolution
    /// key, for a repo outside the walked tree that surfaced no marker and so
    /// offers no declared name to match, see
    /// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
    /// A probe landing on a root already matched is skipped, the same
    /// confirm-once rule a candidate dedup gives.
    pub fn scan(&self, name: &RepoName, fs: &impl FileSystem) -> SiblingScan {
        let mut found: BTreeSet<PathBuf> = self.by_name.get(name).cloned().unwrap_or_default();
        for parent in &self.parents {
            let candidate = parent.join(&name.0);
            if found.contains(&candidate) {
                continue;
            }
            if declared_name_at(&candidate, fs).as_ref() == Some(name) {
                found.insert(candidate);
            }
        }
        let found: Vec<PathBuf> = found.into_iter().collect();
        match found.len() {
            0 => SiblingScan::NotFound,
            1 => SiblingScan::Found(found.into_iter().next().unwrap()),
            _ => SiblingScan::Ambiguous(found),
        }
    }

    /// A beside-the-entry sibling `parent/<name>` whose `repo.yaml` is present
    /// but will not load, for a dep that failed to resolve by name. A malformed
    /// sibling cannot declare its name, so `scan` never matches it; this probes
    /// the folder-name position (the resolution convention) and returns the parse
    /// diagnostic so the malformed file is named rather than reading as
    /// `peer-unmounted`.
    pub fn malformed_candidate(&self, name: &RepoName, fs: &impl FileSystem) -> Option<Diagnostic> {
        for parent in &self.parents {
            let marker = parent.join(&name.0).join(REGISTRY_REL);
            if let Some(d) = validate_registry_marker(&marker, fs) {
                return Some(d);
            }
        }
        None
    }
}

/// Where a repo name resolved to local bytes, plus advisory notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub outcome: ResolveOutcome,
    pub notes: Vec<ResolveNote>,
}

/// The tier a repo name resolved through, or that it did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// An editable local working tree: a co-present sibling (tier 1) or a
    /// registry `path:` (tier 2).
    Local(PathBuf),
    /// A read-only cache snapshot, by locked sha (tier 3).
    Cache(PathBuf),
    /// None of the tiers resolved: advisory unmounted.
    Unmounted,
}

/// An advisory note the resolver raises, mapped to a diagnostic by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveNote {
    /// Two co-present siblings declare one name: refuse to pick
    /// (`duplicate-repo-name`), the outcome is `Unmounted`.
    DuplicateSibling { name: RepoName, roots: Vec<PathBuf> },
    /// A local path (sibling or registry) shadowed a locked cache snapshot
    /// (`dependency-path-overridden`, a hint).
    PathOverridden { name: RepoName },
    /// A registry `path:` whose target `repo.yaml` declares a different name
    /// than the key (`dependency-identity-conflict`); the outcome is
    /// `Unmounted`, never silently resolved.
    IdentityConflict { key: RepoName, declared: RepoName },
    /// A declared dep resolved to a candidate `repo.yaml` that is present but
    /// will not load (`repo-registry-parse-error`). A parse failure denies the
    /// repo a name, so it is invisible to name-matched resolution and reads only
    /// as a misleading `peer-unmounted`; this carries the parse diagnostic (its
    /// span is the malformed file) so the actual cause is named. Raised only when
    /// the dep is ultimately unresolved.
    MalformedRepoYaml(Diagnostic),
    /// A member resolved to the reserved device root `~/.arsumbris`
    /// (`member-at-reserved-root`). It is EXCLUDED from the mount set, never
    /// walked. Keyed by the member it rides beside, rendered in the
    /// member-outcome pass, not the notes pass.
    ReservedRoot,
}

/// Resolve one repo `name` to local bytes in the redesign's strict tier order:
/// a co-present sibling, then a registry `path:`, then the package cache, else
/// unmounted.
///
/// `cache_lookup` maps a name to its cached snapshot root (via the lock),
/// keeping this function agnostic of the cache internals. Advisory notes ride
/// alongside the outcome for the caller to render as diagnostics with spans.
/// Sibling-first is deliberate: a co-present tree wins over a stale registry
/// path. A `@commit`-pinned reference bypasses this order entirely (resolved
/// against the cache by sha at the reference site), see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
pub fn resolve_repo_name(
    name: &RepoName,
    siblings: &SiblingIndex,
    registry: &UserRegistry,
    cache_lookup: impl Fn(&RepoName) -> Option<PathBuf>,
    fs: &impl FileSystem,
) -> Resolved {
    let mut notes = Vec::new();
    // A candidate `repo.yaml` that is present but will not load, held until the
    // final outcome: emitted only if the dep is ultimately unresolved, so a dep
    // that resolves by a later tier raises no spurious parse note. The registry
    // tier fills it inline; the sibling probe below is the fallback.
    let mut malformed: Option<Diagnostic> = None;

    // Tier 1: a co-present sibling wins, always, over a possibly-stale registry.
    match siblings.scan(name, fs) {
        SiblingScan::Found(root) => {
            if cache_lookup(name).is_some() {
                notes.push(ResolveNote::PathOverridden { name: name.clone() });
            }
            return Resolved {
                outcome: ResolveOutcome::Local(root),
                notes,
            };
        }
        SiblingScan::Ambiguous(roots) => {
            notes.push(ResolveNote::DuplicateSibling {
                name: name.clone(),
                roots,
            });
            return Resolved {
                outcome: ResolveOutcome::Unmounted,
                notes,
            };
        }
        SiblingScan::NotFound => {}
    }

    // Tier 2: a registry `path:`, verified against the target's declared name.
    if let Some(loc) = registry.get(name) {
        let registry_file = loc.path.join(REGISTRY_REL);
        let declared = match fs.read_file(&registry_file) {
            Ok(b) => match parse_registry(&registry_file, &b) {
                Ok(r) => Some(r.self_entry.name),
                Err(d) => {
                    // The registered path points at a `repo.yaml` that will not
                    // load: cannot verify identity, so fall through unresolved,
                    // but hold the parse diagnostic to name it if nothing else
                    // resolves this dep.
                    malformed.get_or_insert(d);
                    None
                }
            },
            Err(_) => None,
        };
        match declared {
            Some(d) if d == *name => {
                if cache_lookup(name).is_some() {
                    notes.push(ResolveNote::PathOverridden { name: name.clone() });
                }
                return Resolved {
                    outcome: ResolveOutcome::Local(loc.path.clone()),
                    notes,
                };
            }
            Some(d) => {
                // A registered path that declares another name is a real
                // identity conflict: surface it and leave the name unmounted,
                // never silently resolved past.
                notes.push(ResolveNote::IdentityConflict {
                    key: name.clone(),
                    declared: d,
                });
                return Resolved {
                    outcome: ResolveOutcome::Unmounted,
                    notes,
                };
            }
            None => {
                // A registered path with no readable `repo.yaml` cannot be
                // verified, so it does not resolve; fall through to the cache.
            }
        }
    }

    // Tier 3: a cached snapshot, read-only.
    if let Some(snapshot) = cache_lookup(name) {
        return Resolved {
            outcome: ResolveOutcome::Cache(snapshot),
            notes,
        };
    }

    // Tier 4: unmounted, advisory. Before giving up, name a malformed candidate:
    // the registry path (held above) or a beside-the-entry sibling at the
    // folder-name position, so a present-but-broken dep is not misreported as
    // merely absent.
    if let Some(d) = malformed.or_else(|| siblings.malformed_candidate(name, fs)) {
        notes.push(ResolveNote::MalformedRepoYaml(d));
    }
    Resolved {
        outcome: ResolveOutcome::Unmounted,
        notes,
    }
}

/// One member's resolution in an assembled workspace: where it resolved, and
/// whether it is a named primary root versus a computed transitive dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMember {
    /// Where the name resolved: an editable local tree, a read-only cache
    /// snapshot, or unmounted.
    pub outcome: ResolveOutcome,
    /// The member's role, the highest-ranked way it was reached: a seeded `edit`
    /// or `discover` member, or a `dep` reached through another member's `deps`.
    /// The one-role precedence applies (editable wins, a dep subsumes a discover),
    /// see [`MemberRole::rank`].
    pub role: MemberRole,
    /// The declaration this member was first reached by: a dep's `RepoEntry`
    /// (carrying its optional `remote` / `ref` / `path`) for a computed
    /// dependency, or a bare-name entry for a named member. The `resolve` verb
    /// reads the remote / ref from here, since a dep's source lives in the
    /// depender's `deps`, not in the workspace composition.
    pub entry: RepoEntry,
}

impl ResolvedMember {
    /// The resolved root, `None` when unmounted.
    pub fn root(&self) -> Option<&Path> {
        match &self.outcome {
            ResolveOutcome::Local(p) | ResolveOutcome::Cache(p) => Some(p),
            ResolveOutcome::Unmounted => None,
        }
    }

    /// Whether the member resolved to a live LOCAL working tree, versus an
    /// immutable cache snapshot or an unmounted member. This is the mount / watch
    /// signal (a live tree is watched, a snapshot is not), NOT editability.
    /// Editability is role-derived and independent of where the member resolved,
    /// so a local-tree `dep` / `discover` is watched but still not editable (the
    /// path-override case). See [`MemberRole::editable`].
    pub fn is_local_tree(&self) -> bool {
        matches!(self.outcome, ResolveOutcome::Local(_))
    }
}

/// The members an assembled workspace contains, plus the advisory notes raised
/// while resolving them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssembledMembers {
    /// Each member name to its resolution: the `edit` / `discover` selection plus
    /// the transitive closure of every resolved member's `deps`.
    pub members: BTreeMap<RepoName, ResolvedMember>,
    /// Notes from [`resolve_repo_name`], tagged by the member name they concern,
    /// for the caller to render as diagnostics with spans.
    pub notes: Vec<(RepoName, ResolveNote)>,
}

/// Assemble a workspace's member set from its `edit` / `discover` selection.
///
/// The members are the `edit` and `discover` selection plus the transitive
/// closure of every resolved member's declared `deps`. Each name resolves through
/// [`resolve_repo_name`] (co-present sibling, then registry, then cache); a
/// resolved member's root is added to the located set so its own siblings
/// resolve, and its `repo.yaml` `deps` join the frontier as `dep` reaches.
/// `seed_roots` are the co-present repo roots the caller discovered under the
/// entry, the sibling tier's starting set.
///
/// `entry` is the containing folder-repo's `(name, root)` for a folder-repo entry,
/// the workspace home. Its root is the entry DIRECTORY, known directly, so it is
/// pre-resolved rather than looked up by name (a folder-repo's folder name need
/// not match its declared repo name, which the sibling scan keys on). It is ALWAYS
/// a member with role [`MemberRole::Entry`], independent of the `edit` list, so a
/// `workspace.yaml` that omits it (a `workspace-omits-containing-repo` error the
/// caller emits) still mounts its own repo. `None` when the manifest is not a
/// folder-repo `.arsumbris/workspace.yaml`, or its `repo.yaml` is nameless or
/// unreadable, so there is no single entry repo.
///
/// Each member carries its [`MemberRole`], the HIGHEST-ranked way it was reached:
/// the entry beats every other role, an `edit` member beats a `dep` / `discover`,
/// and a `discover` member also reached as a declared `dep` upgrades to `dep`
/// (the dep subsumes it). The walk is a fixpoint over names, so a dependency
/// cycle terminates and a diamond is resolved once; the first-seen declaration
/// supplies the entry data, later reaches only raise the role. The caller maps an
/// unmounted or cache-only editable member to `edit-member-unmounted` /
/// `edit-member-read-only`, see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
pub fn assemble_members(
    entry: Option<(&RepoName, &Path)>,
    edit: &[RepoName],
    discover: &[RepoName],
    seed_roots: &[PathBuf],
    registry: &UserRegistry,
    cache_lookup: impl Fn(&RepoName) -> Option<PathBuf>,
    fs: &impl FileSystem,
) -> AssembledMembers {
    let mut roots: Vec<PathBuf> = seed_roots.to_vec();
    // Grown alongside `roots`, so each newly-resolved member's `repo.yaml` is
    // read once for the whole assembly rather than re-read by every later
    // resolution. The fixpoint above this runs the assembly several times per
    // build, so the saving compounds.
    let mut siblings = SiblingIndex::from_roots(&roots, fs);
    let mut members: BTreeMap<RepoName, ResolvedMember> = BTreeMap::new();
    let mut notes: Vec<(RepoName, ResolveNote)> = Vec::new();
    // The entry repo is pre-resolved to its known root (the entry directory), so
    // its declared name need not match its folder name. Seed it into `members`
    // directly and grow its `deps`, so an `edit` listing of the same name later
    // just finds it (Entry outranks Edit) and its deps join the closure.
    let mut frontier: Vec<(RepoEntry, MemberRole)> = Vec::new();
    if let Some((name, root)) = entry {
        if !roots.contains(&root.to_path_buf()) {
            roots.push(root.to_path_buf());
            siblings.insert_root(root, fs);
        }
        if let Ok(bytes) = fs.read_file(&root.join(REGISTRY_REL)) {
            for dep in parse_repo_deps(&bytes) {
                frontier.push((dep, MemberRole::Dep));
            }
        }
        members.insert(
            name.clone(),
            ResolvedMember {
                outcome: ResolveOutcome::Local(root.to_path_buf()),
                role: MemberRole::Entry,
                entry: RepoEntry {
                    name: name.clone(),
                    ..RepoEntry::default()
                },
            },
        );
    }
    // Frontier of (DECLARATION, reach role) to resolve: the `edit` members, then
    // `discover`, then each resolved member's `deps` (as `dep` reaches). The
    // `members` map dedupes; a name reached again only RAISES its role (a `dep`
    // reach subsuming a `discover` seed), the first-seen declaration is kept.
    let seed = |names: &[RepoName], role: MemberRole| -> Vec<(RepoEntry, MemberRole)> {
        names
            .iter()
            .map(|n| {
                (
                    RepoEntry {
                        name: n.clone(),
                        ..RepoEntry::default()
                    },
                    role,
                )
            })
            .collect()
    };
    frontier.extend(seed(edit, MemberRole::Edit));
    frontier.extend(seed(discover, MemberRole::Discover));
    let mut i = 0;
    while i < frontier.len() {
        let (entry, reach_role) = frontier[i].clone();
        i += 1;
        // Already resolved: raise its role if this reach outranks the recorded
        // one (a `dep` subsuming a `discover`), then skip re-resolving.
        if let Some(existing) = members.get_mut(&entry.name) {
            if reach_role.rank() > existing.role.rank() {
                existing.role = reach_role;
            }
            // Adopt an explicit source (remote / ref / path) when the stored
            // declaration is bare. A `discover` seed is bare (a name only), so a
            // later `dep` reach carrying the depender's declared remote / ref must
            // win, else the fetch and the workspace lock fall back to the registry
            // and can pin the wrong source. A stored explicit source is KEPT
            // (first-seen wins among explicit edges; a genuine disagreement between
            // two explicit edges is a version conflict, surfaced by the resolve
            // solve, not silently overwritten here).
            let stored_bare = existing.entry.remote.is_none()
                && existing.entry.git_ref.is_none()
                && existing.entry.path.is_none();
            let incoming_explicit =
                entry.remote.is_some() || entry.git_ref.is_some() || entry.path.is_some();
            if stored_bare && incoming_explicit {
                existing.entry = entry.clone();
            }
            continue;
        }
        let resolved = resolve_repo_name(&entry.name, &siblings, registry, &cache_lookup, fs);
        for note in resolved.notes {
            notes.push((entry.name.clone(), note));
        }
        if let ResolveOutcome::Local(root) | ResolveOutcome::Cache(root) = &resolved.outcome {
            if !roots.contains(root) {
                roots.push(root.clone());
                siblings.insert_root(root, fs);
            }
            // Read this member's own deps and add them to the frontier as `dep`
            // reaches, so the closure grows past the selection into its transitive
            // deps. A dep already resolved is still pushed, so its role can be
            // raised to `dep`; the frontier stays bounded (each deps edge is pushed
            // once per member, and each member resolves once).
            if let Ok(bytes) = fs.read_file(&root.join(REGISTRY_REL)) {
                for dep in parse_repo_deps(&bytes) {
                    frontier.push((dep, MemberRole::Dep));
                }
            }
        }
        members.insert(
            entry.name.clone(),
            ResolvedMember {
                outcome: resolved.outcome,
                role: reach_role,
                entry,
            },
        );
    }
    AssembledMembers { members, notes }
}

/// The repo roots discovered from walker-surfaced `.arsumbris/repo.yaml`
/// markers. Each marker `<root>/.arsumbris/repo.yaml` maps back to `<root>`.
///
/// Content-free: a repo holding only a `repo.yaml` is discovered the same as one
/// with content, and a folder-name mismatch never hides one, the marker is the
/// definitive repo-root indicator, see
/// [[spec - cross-repo resolution - in-repo identity and deps over a per-user repo registry]].
pub fn marker_roots(markers: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = markers
        .iter()
        .filter_map(|m| m.parent().and_then(Path::parent).map(Path::to_path_buf))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Find the repo-registry files under a set of roots and read their bytes.
///
/// A repo root is a directory with a readable `.arsumbris/repo.yaml`.
/// Candidate directories are the roots, the walker-surfaced `markers`
/// ([`marker_roots`]), plus the ancestors of the walked files, bounded to stay
/// within some root. The markers discover a repo content-free; the ancestor scan
/// is the fallback for callers that pass no markers. The bytes are read once
/// here, then reused for parsing, cataloguing, and fingerprinting.
///
/// One root is the entry-only case (the entry repo alone). Many roots is a composed
/// workspace, one root per mounted member, each its own repo subtree wherever it sits.
pub fn registry_files(
    roots: &[PathBuf],
    files: &[PathBuf],
    markers: &[PathBuf],
    fs: &impl FileSystem,
) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for dir in candidate_repo_dirs(roots, files, markers) {
        let registry = dir.join(REGISTRY_REL);
        if let Ok(bytes) = fs.read_file(&registry) {
            out.push((registry, bytes));
        }
    }
    out
}

/// The candidate repo-root directories: the roots, the walker-surfaced `markers`
/// ([`marker_roots`]), plus the ancestors of the walked files, bounded to stay
/// within some root. The markers discover a repo content-free; the ancestor scan
/// is the fallback for callers that pass no markers. Sorted and deduped by
/// `BTreeSet`, so the reads are deterministic.
fn candidate_repo_dirs(
    roots: &[PathBuf],
    files: &[PathBuf],
    markers: &[PathBuf],
) -> BTreeSet<PathBuf> {
    let mut candidates: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        candidates.insert(root.clone());
    }
    for dir in marker_roots(markers) {
        candidates.insert(dir);
    }
    let within_a_root = |dir: &Path| roots.iter().any(|r| dir.starts_with(r));
    let is_a_root = |dir: &Path| roots.iter().any(|r| dir == r);
    for f in files {
        let mut cur = f.parent();
        while let Some(dir) = cur {
            if !within_a_root(dir) {
                break;
            }
            candidates.insert(dir.to_path_buf());
            if is_a_root(dir) {
                break;
            }
            cur = dir.parent();
        }
    }
    candidates
}

/// Find the engine-written `.arsumbris/` lock files under a set of roots and read
/// their bytes. Each returned path is a `repo.lock` beside a repo's `repo.yaml`,
/// over the same candidate directories as [`registry_files`].
///
/// The lock sits under the walk floor, so it is never walked as a knowledge base file.
/// The engine reads it for resolution by targeted path already (the package
/// cache); this surfaces the same bytes so it can be catalogued as a first-class
/// `au.engine.repo-lock` node.
pub fn lock_files(
    roots: &[PathBuf],
    files: &[PathBuf],
    markers: &[PathBuf],
    fs: &impl FileSystem,
) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for dir in candidate_repo_dirs(roots, files, markers) {
        let lock = dir.join(REPO_LOCK_REL);
        if let Ok(bytes) = fs.read_file(&lock) {
            out.push((lock, bytes));
        }
    }
    out
}

/// Build the membership map from the pre-read registry files.
///
/// Assembly passes one root per mounted member, each carrying a declared
/// `.arsumbris/repo.yaml`. A member root whose registry could NOT become a
/// declared repo (a duplicate name, or a parse error) is isolated as a fallback
/// repo (`declared: false`) so its files never silently merge into an enclosing
/// repo. This is the safety floor: the folder-repo entry model forbids an
/// undeclared entry, so the routine path is all-declared, and the fallback fires
/// only for a broken or colliding registry.
///
/// `member_names` maps a member root to its declared name in the manifest, so a
/// fallback member is named by its workspace identity rather than its path
/// basename, and two members sharing a basename stay distinct. Every per-repo
/// graph and index is keyed by `RepoName`, so the name must be unique across the
/// final set: a residual collision is diagnosed and disambiguated, never silently
/// merged.
pub fn discover_repos(
    roots: &[PathBuf],
    registries: &[(PathBuf, Vec<u8>)],
    member_names: &BTreeMap<PathBuf, RepoName>,
    cache_root: Option<&Path>,
) -> (RepoMap, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let mut repos: Vec<Repo> = Vec::new();
    let mut name_to_root: BTreeMap<String, PathBuf> = BTreeMap::new();

    for (registry, bytes) in registries {
        let Some(root) = repo_root_of_registry(registry) else {
            continue;
        };
        match parse_registry(registry, bytes) {
            Ok(reg) => {
                let name = reg.self_entry.name.0.clone();
                if let Some(first) = name_to_root.get(&name) {
                    diagnostics.push(diag(
                        DUPLICATE_REPO_NAME,
                        registry,
                        ByteRange::new(0, 0),
                        format!("repo name '{name}' is declared by more than one repo; this registry is ignored and its directory is mounted as an implicit repo so it stays isolated"),
                        vec![Span::for_file(first.join(REGISTRY_REL))],
                    ));
                    continue;
                }
                name_to_root.insert(name.clone(), root.clone());
                // Folder-name convention: a declared repo should sit in a folder
                // matching its name. A mismatch is soft drift, the declared name
                // wins. Implicit repos (no repo.yaml) are exempt, handled below.
                let basename = root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // A cache-mounted dependency lives at `<cache>/<sha>`, so its
                // folder is a sha, never its name. Managed deps are exempt from
                // the convention, see the todo. Only local checkouts warn.
                // TODO: revisit this cache-root exemption once an
                // editable/provenance signal distinguishes a user checkout more
                // precisely than "not under the cache root".
                let under_cache = cache_root.is_some_and(|c| root.starts_with(c));
                // Compare case-INSENSITIVELY: on a case-insensitive filesystem
                // (macOS APFS, Windows) a repo `MyRepo` legitimately lives in
                // `myrepo/`, and firing `drift` (high-attention) on every build
                // for that is noise. A genuine mismatch (different letters) still
                // drifts. Repo names are ASCII (the type/field-name grammar), so
                // ASCII case-folding is sufficient.
                if !under_cache && !basename.eq_ignore_ascii_case(&name) {
                    diagnostics.push(drift(
                        REPO_FOLDER_NAME_MISMATCH,
                        registry,
                        ByteRange::new(0, 0),
                        format!(
                            "repo '{name}' lives in a folder named '{basename}'; the declared name wins"
                        ),
                        vec![],
                    ));
                }
                repos.push(Repo {
                    root,
                    name: reg.self_entry.name,
                    declared: true,
                    builtin: false,
                    description: reg.self_entry.description,
                    remote: reg.self_entry.remote,
                    deps: reg.deps,
                    peer_paths: BTreeMap::new(),
                });
            }
            Err(d) => diagnostics.push(d),
        }
    }

    // Isolated fallback repo per root whose registry could NOT become a declared
    // repo (a duplicate name, or a parse error). Such a root must still be its own
    // repo, or its files would route into the parent and merge their vocabularies
    // silently. So union the discovery roots with every registry root, and drop
    // those already mounted as declared repos; what remains is the broken-or-
    // colliding set (the folder-repo entry model forbids an undeclared entry, so a
    // plain undeclared root no longer arises here).
    //
    // Name a fallback repo after its declared member name, its identity in the
    // manifest, when assembly provides one; else its path basename. Process roots
    // in a stable order so any disambiguation is deterministic.
    let registry_roots = registries
        .iter()
        .filter_map(|(registry, _)| repo_root_of_registry(registry));
    let mut fallback_roots: Vec<PathBuf> = roots
        .iter()
        .cloned()
        .chain(registry_roots)
        .filter(|root| !repos.iter().any(|r| &r.root == root))
        .collect();
    fallback_roots.sort();
    fallback_roots.dedup();
    for root in &fallback_roots {
        let preferred = member_names
            .get(root)
            .map(|n| n.0.clone())
            .unwrap_or_else(|| {
                root.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "root".to_string())
            });
        let name = unique_repo_name(preferred, root, &mut name_to_root, &mut diagnostics);
        repos.push(Repo {
            root: root.clone(),
            name,
            declared: false,
            builtin: false,
            description: None,
            remote: None,
            deps: Vec::new(),
            peer_paths: BTreeMap::new(),
        });
    }

    repos.sort_by(|a, b| a.root.cmp(&b.root));
    (RepoMap::from_repos(repos), diagnostics)
}

/// A unique `RepoName` for an implicit (undeclared) repo, the backstop that
/// keeps the per-repo keying sound. Every per-repo graph and index is keyed by
/// `RepoName`, so a collision would silently merge two repos into one. A
/// preferred name already taken is diagnosed and disambiguated, so the two stay
/// isolated. `used` tracks every name claimed so far, declared repos included.
fn unique_repo_name(
    preferred: String,
    root: &Path,
    used: &mut BTreeMap<String, PathBuf>,
    diagnostics: &mut Vec<Diagnostic>,
) -> RepoName {
    if let Some(first) = used.get(&preferred).cloned() {
        let mut n = 2;
        let mut candidate = format!("{preferred}@{n}");
        while used.contains_key(&candidate) {
            n += 1;
            candidate = format!("{preferred}@{n}");
        }
        diagnostics.push(diag(
            DUPLICATE_REPO_NAME,
            root,
            ByteRange::new(0, 0),
            format!(
                "repo name '{preferred}' is already used by another repo; this member is mounted as '{candidate}' so the two stay isolated"
            ),
            vec![Span::for_file(first)],
        ));
        used.insert(candidate.clone(), root.to_path_buf());
        RepoName(candidate)
    } else {
        used.insert(preferred.clone(), root.to_path_buf());
        RepoName(preferred)
    }
}

/// The repo root of a `<root>/.arsumbris/repo.yaml` path.
fn repo_root_of_registry(registry: &Path) -> Option<PathBuf> {
    registry.parent()?.parent().map(|p| p.to_path_buf())
}

/// A parsed `repo.yaml`: the repo's own identity entry and its declared `deps`.
struct ParsedRegistry {
    self_entry: RepoEntry,
    deps: Vec<RepoEntry>,
}

/// Parse `.arsumbris/repo.yaml` into the identity entry plus `deps`.
///
/// The document root is the identity, a non-empty top-level `name` is required;
/// a missing or nameless root is `repo-name-missing`. `deps` is optional;
/// malformed dep entries (no `name`) are skipped, not fatal, the file still
/// identifies the repo.
fn parse_registry(registry: &Path, bytes: &[u8]) -> Result<ParsedRegistry, Diagnostic> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        err_diag(
            REPO_REGISTRY_PARSE_ERROR,
            registry,
            ByteRange::new(0, 0),
            "repo registry is not valid UTF-8".to_string(),
            None,
        )
    })?;
    let docs = parse(text).map_err(|e| {
        err_diag(
            REPO_REGISTRY_PARSE_ERROR,
            registry,
            e.range,
            format!("repo registry is not valid YAML: {}", e.message),
            unquoted_colon_hint(text),
        )
    })?;
    let missing = || {
        err_diag(
            REPO_NAME_MISSING,
            registry,
            ByteRange::new(0, 0),
            "repo.yaml must declare a valid top-level `name`".to_string(),
            None,
        )
    };
    let doc = docs.first().ok_or_else(missing)?;
    let self_entry = parse_entry(doc).ok_or_else(missing)?;
    let deps = field(doc, "deps").map(parse_entries).unwrap_or_default();
    Ok(ParsedRegistry { self_entry, deps })
}

/// A quoting hint for the recurring `repo.yaml` footgun: an unquoted `:` inside
/// a scalar value (e.g. `description: text: colon`) is not valid YAML, the
/// embedded colon-space reads as a nested mapping.
///
/// Scans for an unquoted value carrying a `:` followed by whitespace, or a
/// trailing `:`. The whitespace requirement is what lets a `remote:
/// git@host:org/repo.git` through, its colon has no following space so YAML
/// takes the whole thing as a plain scalar. Only a hint on an already-failed
/// parse, so a miss just omits the suggestion.
fn unquoted_colon_hint(text: &str) -> Option<SuggestedFix> {
    for line in text.lines() {
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let value = rhs.trim();
        if value.is_empty()
            || value.starts_with('"')
            || value.starts_with('\'')
            || value.starts_with('[')
            || value.starts_with('{')
        {
            continue;
        }
        let embedded_colon = value.contains(": ") || value.contains(":\t") || value.ends_with(':');
        if embedded_colon {
            let key = lhs.trim_start().trim_start_matches("- ").trim();
            return Some(SuggestedFix {
                description: format!(
                    "an unquoted `:` in a value is not valid YAML; quote it, write `{key}: \"{value}\"`"
                ),
            });
        }
    }
    None
}

/// A fetched dependency's declared `deps`, its own transitive dependency record.
///
/// Reads the `deps:` list from a repo's `.arsumbris/repo.yaml` bytes, the same
/// shape the transitive resolver walks. A malformed or nameless `repo.yaml`
/// yields no deps, so the dependency reads as a leaf rather than aborting the
/// walk; an incomplete closure surfaces downstream as an unresolved `::repo`
/// reference, not here.
pub(crate) fn parse_repo_deps(bytes: &[u8]) -> Vec<RepoEntry> {
    parse_registry(Path::new(REGISTRY_REL), bytes)
        .map(|reg| reg.deps)
        .unwrap_or_default()
}

fn scalar_string(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// The textual value of a scalar, reading the raw source lexeme for a non-string
/// scalar.
///
/// A token that looks like a number (an all-digit git sha) types as a YAML
/// number, so `scalar_string` would reject it. Slicing the original source by
/// the node's span recovers the literal text, type-agnostic. `source` is the
/// document the node was parsed from, at yaml-offset 0.
fn scalar_text(node: &MarkedYaml<'_>, source: &str) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        YamlData::Value(_) => {
            let r = span_to_byte_range(source, 0, node.span);
            source.get(r.start..r.end).map(|s| s.trim().to_string())
        }
        _ => None,
    }
}

/// The value for a scalar `key` in a mapping node, else `None`.
fn field<'a>(node: &'a MarkedYaml<'a>, key: &str) -> Option<&'a MarkedYaml<'a>> {
    let YamlData::Mapping(map) = &node.data else {
        return None;
    };
    map.iter()
        .find_map(|(k, v)| (scalar_string(k).as_deref() == Some(key)).then_some(v))
}

/// Parse one entry, the shared shape of the `repo.yaml` root identity, a `deps`
/// item, and a manifest `member`. `None` when the node is not a mapping, or its
/// `name` is empty or not a valid repo name. Identity only; location lives in the
/// per-user registry.
///
/// The name must match the repo-name grammar (`is_valid_type_name`): a leading
/// letter, then letters / digits / `_` / `-`, dot-separated. This is also a
/// security gate, a valid name has no `/`, `..`, whitespace, or newline, so a
/// resolved name is always ONE benign path component. `sibling_scan` joins a
/// name onto a parent directory, so an unchecked `../x` or `a/b` name would probe
/// outside the intended sibling; rejecting it here means no such name is ever
/// built into a `RepoName`.
fn parse_entry(node: &MarkedYaml<'_>) -> Option<RepoEntry> {
    if !matches!(&node.data, YamlData::Mapping(_)) {
        return None;
    }
    let name = field(node, "name")
        .and_then(scalar_string)
        .filter(|n| au_core::is_valid_type_name(n))?;
    Some(RepoEntry {
        name: RepoName(name),
        description: field(node, "description").and_then(scalar_string),
        remote: field(node, "remote").and_then(scalar_string),
        git_ref: field(node, "ref").and_then(scalar_string),
        path: field(node, "path").and_then(scalar_string),
    })
}

/// Parse a `deps` / `members` sequence, skipping entries without a `name`.
fn parse_entries(node: &MarkedYaml<'_>) -> Vec<RepoEntry> {
    let YamlData::Sequence(items) = &node.data else {
        return Vec::new();
    };
    items.iter().filter_map(parse_entry).collect()
}

/// Load a workspace from its `workspace.yaml`: parse `edit` / `discover`, then
/// resolve the member closure (the selection plus each repo's transitive `deps`)
/// via [`assemble_members`].
///
/// `seed_roots` are the co-present repo roots the caller discovered under the
/// entry, the sibling tier's starting set. Only the LOCAL tiers (co-present
/// sibling, then registry) resolve here; the cache tier is applied afterward by
/// `locate_locked_dependencies`, so the effective order stays sibling → registry
/// → cache. `member_roles` are derived from the selection: an `edit` member is
/// editable, a `discover` member is a consumed discovery mount, a computed dep is
/// a dependency member.
pub fn load_workspace(
    manifest_path: &Path,
    bytes: &[u8],
    seed_roots: &[PathBuf],
    registry: &UserRegistry,
    fs: &impl FileSystem,
) -> Workspace {
    let manifest = parse_workspace_manifest(bytes);
    let name = workspace_name(manifest_path);
    // A folder-repo `.arsumbris/workspace.yaml` has a containing repo, its own
    // home, always a member (role `Entry`) even if the `edit` list omits it. Its
    // `(name, root)` comes from the sibling `repo.yaml` and the entry directory.
    // None when the containing `repo.yaml` is absent, nameless, or unreadable.
    let entry = folder_repo_entry(manifest_path, fs);
    // `seed_roots` are the walker-surfaced repo roots the caller discovered under
    // the entry (see `marker_roots`), so a content-less member (only a
    // `repo.yaml`) is already among them, discovered content-free. No
    // `<manifest-dir>/<member>` guess is needed.
    assemble_workspace(
        manifest_path,
        name,
        entry,
        manifest.edit,
        manifest.discover,
        manifest.disabled,
        seed_roots,
        registry,
        fs,
    )
}

/// The declared repo name at a root, read from its `.arsumbris/repo.yaml`. `None`
/// when absent, unreadable, or nameless. The declared name wins over the folder
/// name, so a caller uses this rather than the basename.
pub(crate) fn declared_name_at(root: &Path, fs: &impl FileSystem) -> Option<RepoName> {
    let repo_yaml = root.join(REGISTRY_REL);
    let bytes = fs.read_file(&repo_yaml).ok()?;
    parse_registry(&repo_yaml, &bytes)
        .ok()
        .map(|r| r.self_entry.name)
}

/// The entry's `repo.yaml` load outcome, for a legible boot refusal.
///
/// `Ok(())` when it parses and declares a name. `Err(None)` when the file is
/// absent or unreadable, an ordinary not-a-repo. `Err(Some(diag))` when it is
/// present but will not load, unparseable or nameless, carrying the blocking
/// diagnostic so the refusal can name the cause. Distinct from `declared_name_at`,
/// which collapses all three failures into `None`.
pub(crate) fn entry_repo_load(root: &Path, fs: &impl FileSystem) -> Result<(), Option<Diagnostic>> {
    let repo_yaml = root.join(REGISTRY_REL);
    let bytes = match fs.read_file(&repo_yaml) {
        Ok(b) => b,
        Err(_) => return Err(None),
    };
    parse_registry(&repo_yaml, &bytes).map(|_| ()).map_err(Some)
}

/// Validate a surfaced `.arsumbris/repo.yaml` marker in place, for the discovery
/// pass over the walked markers.
///
/// `None` when it loads (or vanished / is unreadable). `Some(diag)` when it is
/// present but will not parse or declares no name. Unlike membership resolution,
/// which drops a repo whose `repo.yaml` will not load and never says so, this
/// surfaces the parse error against the marker's own path. The `marker` is the
/// full `.arsumbris/repo.yaml` path, so the diagnostic points at the file.
pub(crate) fn validate_registry_marker(marker: &Path, fs: &impl FileSystem) -> Option<Diagnostic> {
    let bytes = fs.read_file(marker).ok()?;
    parse_registry(marker, &bytes).err()
}

/// The `(declared name, root)` of a folder-repo `.arsumbris/workspace.yaml`'s
/// containing repo: the name from the sibling `.arsumbris/repo.yaml`, the root the
/// entry directory (its grandparent). `None` when the path is not a folder-repo
/// `.arsumbris/workspace.yaml`, or its containing `repo.yaml` is unreadable or
/// nameless.
fn folder_repo_entry(manifest_path: &Path, fs: &impl FileSystem) -> Option<(RepoName, PathBuf)> {
    if manifest_path.file_name()? != "workspace.yaml" {
        return None;
    }
    let arsumbris = manifest_path.parent()?;
    if arsumbris.file_name()? != ".arsumbris" {
        return None;
    }
    let root = arsumbris.parent()?;
    let name = declared_name_at(root, fs)?;
    Some((name, root.to_path_buf()))
}

/// Assemble the workspace for a folder-repo entry that has no
/// `.arsumbris/workspace.yaml`: the entry repo plus its transitive `deps`, with no
/// `edit` / `discover` selection.
///
/// The entry repo is ALWAYS a member (role [`MemberRole::Entry`]), its
/// `(name, root)` read from `<entry_dir>/.arsumbris/repo.yaml`. The no-manifest
/// analogue of [`load_workspace`]: an entry-only composition, the degenerate
/// single-repo workspace. Drives `locate_locked_dependencies` (mount pinned deps
/// from the entry repo's own `repo.lock`) and `resolve_and_lock` (write the lock),
/// so a repo opened standalone reproduces and resolves from its own lock.
/// `entry_dir` is the entry directory (there is no manifest file); it anchors
/// diagnostic spans and the per-repo lock paths derive from each member's own
/// root, not from here. See
/// [[spec - workspace as a folder-repo - an optional workspace.yaml composes edit and discover members]].
pub fn entry_only_workspace(
    entry_dir: &Path,
    seed_roots: &[PathBuf],
    registry: &UserRegistry,
    fs: &impl FileSystem,
) -> Workspace {
    // The entry repo is pre-resolved to its known root (the entry directory), its
    // declared name from its own `repo.yaml`. `resolve_entry` already verified this
    // is a folder-repo, so the name is present barring a mid-build race.
    let entry = declared_name_at(entry_dir, fs).map(|name| (name, entry_dir.to_path_buf()));
    let name = entry_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    assemble_workspace(
        entry_dir,
        name,
        entry,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        seed_roots,
        registry,
        fs,
    )
}

/// The shared core of [`load_workspace`] and [`entry_only_workspace`]: resolve
/// the entry repo plus the `edit` / `discover` selection plus their transitive
/// `deps` closure into a [`Workspace`].
///
/// Roles come from [`assemble_members`], the highest-ranked way each member was
/// reached: the `entry` repo is [`MemberRole::Entry`], an `edit` member is
/// [`MemberRole::Edit`], a `discover` member is [`MemberRole::Discover`], a name
/// reached through a `deps` edge is [`MemberRole::Dep`], and a `discover` also
/// reached as a `dep` upgrades to `dep` (the dep subsumes it). The entry beats an
/// `edit` listing of the same repo, and `edit` beats `discover` (the editable
/// role wins; a name in both `edit` and `discover` is a
/// `workspace-member-role-conflict` the caller emits).
#[allow(clippy::too_many_arguments)]
fn assemble_workspace(
    anchor: &Path,
    name: String,
    entry: Option<(RepoName, PathBuf)>,
    edit: Vec<RepoName>,
    discover: Vec<RepoName>,
    disabled: Vec<RepoName>,
    seed_roots: &[PathBuf],
    registry: &UserRegistry,
    fs: &impl FileSystem,
) -> Workspace {
    // The `disabled:` overlay is applied BEFORE resolution: a disabled name is
    // dropped from the `edit` / `discover` selection fed to `assemble_members`, so
    // it never resolves, never mounts, and never enters `member_roles` (so the
    // member-outcome pass emits no unmounted diagnostic for it). The full `edit` /
    // `discover` lists are still stored on the `Workspace`, so the declaration-
    // integrity checks (role conflict, discover-is-a-dependency) and the members
    // read still see the declared role. A genuine `dep` edge from an active member
    // still pulls the repo: `disabled` overlays the ROLE lists, not an intrinsic
    // type-dependency.
    let edit_active: Vec<RepoName> = edit
        .iter()
        .filter(|n| !disabled.contains(n))
        .cloned()
        .collect();
    let discover_active: Vec<RepoName> = discover
        .iter()
        .filter(|n| !disabled.contains(n))
        .cloned()
        .collect();
    let assembled = assemble_members(
        entry.as_ref().map(|(n, r)| (n, r.as_path())),
        &edit_active,
        &discover_active,
        seed_roots,
        registry,
        |_| None,
        fs,
    );

    let mut members = Vec::new();
    let mut member_paths = BTreeMap::new();
    let mut member_roles = BTreeMap::new();
    let mut member_notes = assembled.notes;
    for (member, resolved) in &assembled.members {
        // The carried entry holds the dep's remote / ref (a bare name for a
        // named member), so the resolve verb can fetch a not-yet-present dep.
        members.push(resolved.entry.clone());
        if let Some(root) = resolved.root() {
            // A member resolving to the reserved device root `~/.arsumbris` is
            // EXCLUDED from the mount set: its `.arsumbris/` IS the device area, so
            // walking it would treat the whole home directory as content. Never a
            // member_path (so it is never walked); a `ReservedRoot` note renders as
            // `member-at-reserved-root`. Equality not subtree, so a cache-mounted
            // member under `~/.arsumbris` is unaffected, see [`is_reserved_root`].
            if is_reserved_root(root) {
                member_notes.push((member.clone(), ResolveNote::ReservedRoot));
            } else {
                member_paths.insert(member.clone(), root.to_path_buf());
            }
        }
        member_roles.insert(member.clone(), resolved.role);
    }

    Workspace {
        manifest_path: anchor.to_path_buf(),
        name,
        edit,
        discover,
        disabled,
        members,
        member_paths,
        member_roles,
        member_notes,
    }
}

/// A parsed `.arsumbris/workspace.yaml` composition: three optional name lists.
///
/// `edit:` names the editable members, `discover:` the pinned members mounted
/// for type-discovery, `disabled:` the declared members intentionally not
/// mounted. All optional; the dependencies are computed from each repo's `deps`,
/// never listed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceManifest {
    pub edit: Vec<RepoName>,
    pub discover: Vec<RepoName>,
    /// The `disabled:` overlay, declared members intentionally not mounted.
    pub disabled: Vec<RepoName>,
}

/// Parse a `.arsumbris/workspace.yaml` into its `edit` / `discover` selection.
///
/// Each entry is a bare name scalar. A missing or malformed list yields an empty
/// one, so a workspace with only `edit:` (or neither) parses cleanly.
fn parse_workspace_manifest(bytes: &[u8]) -> WorkspaceManifest {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return WorkspaceManifest::default();
    };
    let Ok(docs) = parse(text) else {
        return WorkspaceManifest::default();
    };
    let Some(doc) = docs.first() else {
        return WorkspaceManifest::default();
    };
    let names = |key: &str| -> Vec<RepoName> {
        match field(doc, key).map(|n| &n.data) {
            Some(YamlData::Sequence(items)) => items
                .iter()
                .filter_map(scalar_string)
                // Only well-formed repo names, the same gate `parse_entry`
                // applies: a name is joined onto a parent dir during
                // `sibling_scan`, so a traversing scalar (`../../x`) must never
                // become a `RepoName`.
                .filter(|n| au_core::is_valid_type_name(n))
                .map(RepoName)
                .collect(),
            _ => Vec::new(),
        }
    };
    WorkspaceManifest {
        edit: names("edit"),
        discover: names("discover"),
        disabled: names("disabled"),
    }
}

/// The workspace name. The one manifest form is a folder-repo
/// `.arsumbris/workspace.yaml`, named after its containing repo folder (the
/// grandparent), matching `entry_only_workspace`'s basename naming.
fn workspace_name(manifest_path: &Path) -> String {
    manifest_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The advisory (`Warning`) repo-discovery diagnostics, see the module note.
fn diag(
    code: DiagnosticCode,
    file: &Path,
    range: ByteRange,
    message: String,
    related: Vec<Span>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Warning,
        span: Span::new(file.to_path_buf(), range),
        message,
        related,
        fix: None,
    }
}

/// A blocking (`Error`) repo-load diagnostic, for a `repo.yaml` that cannot
/// parse or declares no name. Error because the repo produces no type graph and
/// every `::repo` fold into it is blocked, a downstream stage, not mere advice.
/// Carries an optional `fix`, e.g. the unquoted-colon quoting hint.
fn err_diag(
    code: DiagnosticCode,
    file: &Path,
    range: ByteRange,
    message: String,
    fix: Option<SuggestedFix>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Error,
        span: Span::new(file.to_path_buf(), range),
        message,
        related: vec![],
        fix,
    }
}

fn drift(
    code: DiagnosticCode,
    file: &Path,
    range: ByteRange,
    message: String,
    related: Vec<Span>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity: Severity::Drift,
        span: Span::new(file.to_path_buf(), range),
        message,
        related,
        fix: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au_parser::MemoryFileSystem;

    #[test]
    fn member_role_editable_is_the_two_authoring_roles() {
        // The decision point the mutation-channel write gate and the wire's
        // `editable` field both consult (`serve.rs` resolve_editable_target,
        // `wire.rs` member view). Distinct from the outcome-level `editable()`,
        // which is a live-tree notion.
        assert!(MemberRole::Entry.editable());
        assert!(MemberRole::Edit.editable());
        assert!(!MemberRole::Discover.editable());
        assert!(!MemberRole::Dep.editable());
    }

    /// Simulate assembly over the bounded walker: walk the entry, then each nested
    /// repo it surfaces, recursing, unioning files and markers. The walk stops at
    /// each nested repo, so a nested repo's content comes from its own walk, the
    /// same way the build unions bounded member walks before discovery. Every
    /// present repo is treated as a member here (these helpers test discovery /
    /// attribution mechanics, not membership gating).
    fn assemble_walk(fs: &MemoryFileSystem, entry: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut files = Vec::new();
        let mut markers = Vec::new();
        let mut queue = vec![entry.to_path_buf()];
        let mut seen = BTreeSet::new();
        while let Some(root) = queue.pop() {
            if !seen.insert(root.clone()) {
                continue;
            }
            let filter = au_parser::WalkFilter::default_excludes(&root);
            let Ok(w) = fs.walk_files(&root, &filter) else {
                continue;
            };
            files.extend(w.files);
            for m in w.repo_markers {
                if let Some(rr) = m.parent().and_then(Path::parent) {
                    if rr != root {
                        queue.push(rr.to_path_buf());
                    }
                }
                markers.push(m);
            }
        }
        files.sort();
        files.dedup();
        markers.sort();
        markers.dedup();
        (files, markers)
    }

    fn repo_with(files: &[(&str, &str)]) -> (MemoryFileSystem, Vec<PathBuf>) {
        let mut fs = MemoryFileSystem::new();
        for (p, c) in files {
            fs.insert(*p, c.as_bytes().to_vec());
        }
        let (walked, _) = assemble_walk(&fs, Path::new("/v"));
        (fs, walked)
    }

    fn discover(fs: &MemoryFileSystem, files: &[PathBuf]) -> (RepoMap, Vec<Diagnostic>) {
        let roots = [PathBuf::from("/v")];
        // Marker-fed discovery (the new model): the surfaced repo markers, not a
        // file-ancestor scan, find the repos. Mirrors `build`, which passes the
        // assembled walk's `repo_markers` to `registry_files`.
        let (_, markers) = assemble_walk(fs, Path::new("/v"));
        let regs = registry_files(&roots, files, &markers, fs);
        discover_repos(&roots, &regs, &BTreeMap::new(), None)
    }

    #[test]
    fn unparseable_repo_yaml_is_an_error_carrying_a_quoting_hint() {
        // An unquoted `:` inside a scalar value breaks the YAML parse, the
        // recurring `repo.yaml` footgun.
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: v\n"),
            (
                "/v/dep/.arsumbris/repo.yaml",
                "name: dep\ndescription: text: colon\n",
            ),
        ]);
        let (_map, diags) = discover(&fs, &files);
        let parse_err = diags
            .iter()
            .find(|d| d.code.as_str() == "repo-registry-parse-error")
            .unwrap_or_else(|| panic!("expected repo-registry-parse-error, got {diags:?}"));
        assert_eq!(
            parse_err.severity,
            Severity::Error,
            "a repo.yaml that will not parse blocks a downstream stage"
        );
        assert!(
            parse_err
                .fix
                .as_ref()
                .is_some_and(|f| f.description.contains("quote")),
            "expected a quoting hint, got {:?}",
            parse_err.fix
        );
    }

    #[test]
    fn unquoted_colon_hint_targets_the_value_not_a_git_remote() {
        // The footgun: an embedded colon-space in an unquoted value.
        assert!(
            unquoted_colon_hint("name: x\ndescription: text: colon\n")
                .is_some_and(|f| f.description.contains("\"text: colon\"")),
            "want a quoting hint naming the value"
        );
        // A git remote's colon has no following space, valid YAML, no false hint.
        assert!(unquoted_colon_hint("remote: git@github.com:org/repo.git\n").is_none());
        // An already-quoted value is fine.
        assert!(unquoted_colon_hint("description: \"a: b\"\n").is_none());
        // A clean file yields nothing.
        assert!(unquoted_colon_hint("name: x\n").is_none());
    }

    #[test]
    fn folder_name_mismatch_warns_at_drift_declared_name_wins() {
        // a repo declaring `base` lives in a `notbase` folder.
        let (fs, files) = repo_with(&[
            ("/v/notbase/.arsumbris/repo.yaml", "name: base\n"),
            ("/v/notbase/note.md", "x"),
        ]);
        let (map, diags) = discover(&fs, &files);
        // the declared name wins: the repo is `base`, never positional.
        assert!(map.by_name("base").is_some(), "declared name wins");
        let d = diags
            .iter()
            .find(|d| d.code.as_str() == "repo-folder-name-mismatch")
            .expect("folder-name-mismatch emitted");
        assert_eq!(d.severity, Severity::Drift);
    }

    #[test]
    fn folder_name_match_and_implicit_repo_do_not_warn() {
        // a matching folder does not warn.
        let (fs, files) = repo_with(&[
            ("/v/base/.arsumbris/repo.yaml", "name: base\n"),
            ("/v/base/note.md", "x"),
        ]);
        let (_, diags) = discover(&fs, &files);
        assert!(!diags
            .iter()
            .any(|d| d.code.as_str() == "repo-folder-name-mismatch"));
        // an implicit repo (no repo.yaml) is positional-by-folder, no mismatch.
        let (fs2, files2) = repo_with(&[("/v/note.md", "x")]);
        let (_, diags2) = discover(&fs2, &files2);
        assert!(!diags2
            .iter()
            .any(|d| d.code.as_str() == "repo-folder-name-mismatch"));
    }

    fn reg_yaml(name: &str) -> Vec<u8> {
        format!("name: {name}\n").into_bytes()
    }

    #[test]
    fn sibling_scan_finds_a_co_present_monorepo_member() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/kb/library/.arsumbris/repo.yaml", reg_yaml("library"));
        let located = [PathBuf::from("/kb/notes")];
        assert_eq!(
            sibling_scan(&RepoName("library".into()), &located, &fs),
            SiblingScan::Found(PathBuf::from("/kb/library"))
        );
    }

    #[test]
    fn sibling_scan_absent_name_is_not_found() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        let located = [PathBuf::from("/kb/notes")];
        assert_eq!(
            sibling_scan(&RepoName("ghost".into()), &located, &fs),
            SiblingScan::NotFound
        );
    }

    #[test]
    fn sibling_scan_refuses_two_siblings_of_one_name() {
        // two located repos in different trees, each with a `lib` sibling.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/a/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/a/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        fs.insert("/b/other/.arsumbris/repo.yaml", reg_yaml("other"));
        fs.insert("/b/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/a/notes"), PathBuf::from("/b/other")];
        assert_eq!(
            sibling_scan(&RepoName("lib".into()), &located, &fs),
            SiblingScan::Ambiguous(vec![PathBuf::from("/a/lib"), PathBuf::from("/b/lib")])
        );
    }

    #[test]
    fn sibling_scan_resolves_a_marker_surfaced_repo_by_declared_name() {
        // A repo whose marker is surfaced (in `located_roots`) resolves by its
        // DECLARED name, even under folder-name drift: `lib` in a `mylib/` folder
        // resolves. This is X, the marker is the definitive repo-root indicator.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/mylib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/mylib")];
        assert_eq!(
            sibling_scan(&RepoName("lib".into()), &located, &fs),
            SiblingScan::Found(PathBuf::from("/kb/mylib"))
        );
    }

    #[test]
    fn sibling_scan_a_beside_drifted_repo_needs_the_registry() {
        // A drifted repo NOT in `located_roots` (a beside-the-entry sibling with no
        // surfaced marker) is not found by the folder-name probe of `<parent>/lib`,
        // so it falls to the registry tier. The residual of X: only marker-surfaced
        // repos are drift-tolerant.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/kb/mylib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/notes")];
        assert_eq!(
            sibling_scan(&RepoName("lib".into()), &located, &fs),
            SiblingScan::NotFound
        );
        // and a folder named `lib` that declares a DIFFERENT name is not a match.
        let mut fs2 = MemoryFileSystem::new();
        fs2.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs2.insert("/kb/lib/.arsumbris/repo.yaml", reg_yaml("something-else"));
        assert_eq!(
            sibling_scan(&RepoName("lib".into()), &located, &fs2),
            SiblingScan::NotFound
        );
    }

    fn reg(entries: &[(&str, &str)]) -> UserRegistry {
        entries
            .iter()
            .map(|(n, p)| {
                (
                    RepoName((*n).into()),
                    RegistryLocation {
                        remote: None,
                        path: PathBuf::from(p),
                    },
                )
            })
            .collect()
    }
    fn no_cache(_: &RepoName) -> Option<PathBuf> {
        None
    }

    #[test]
    fn resolve_sibling_wins_over_registry() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/kb/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/notes")];
        // registry points elsewhere, but the co-present sibling wins.
        let registry = reg(&[("lib", "/somewhere/else")]);
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Local(PathBuf::from("/kb/lib")));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn resolve_registry_path_verified_against_declared_name() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/reg/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/notes")];
        let registry = reg(&[("lib", "/reg/lib")]);
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Local(PathBuf::from("/reg/lib")));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn resolve_registry_key_mismatch_is_identity_conflict_and_unmounted() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/reg/x/.arsumbris/repo.yaml", reg_yaml("other"));
        let located = [PathBuf::from("/kb/notes")];
        let registry = reg(&[("lib", "/reg/x")]);
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Unmounted);
        assert_eq!(
            r.notes,
            vec![ResolveNote::IdentityConflict {
                key: RepoName("lib".into()),
                declared: RepoName("other".into()),
            }]
        );
    }

    #[test]
    fn resolve_registry_path_malformed_names_the_file() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        // The registered path points at a repo.yaml that will not parse, an
        // unquoted `:` in a value. Cannot verify identity, so it stays unmounted,
        // but the malformed file is now named rather than read as merely absent.
        fs.insert(
            "/reg/lib/.arsumbris/repo.yaml",
            b"name: lib\ndescription: x: y\n".to_vec(),
        );
        let located = [PathBuf::from("/kb/notes")];
        let registry = reg(&[("lib", "/reg/lib")]);
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Unmounted);
        match r.notes.as_slice() {
            [ResolveNote::MalformedRepoYaml(d)] => {
                assert_eq!(d.code.as_str(), "repo-registry-parse-error");
                assert_eq!(d.severity, Severity::Error);
                assert_eq!(d.span.file, PathBuf::from("/reg/lib/.arsumbris/repo.yaml"));
                assert!(d.fix.is_some(), "carries the quoting hint");
            }
            other => panic!("expected one MalformedRepoYaml note, got {other:?}"),
        }
    }

    #[test]
    fn resolve_sibling_malformed_names_the_file() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        // A beside-the-entry sibling at the folder-name position `/kb/lib`, its
        // repo.yaml broken. It cannot declare its name, so `scan` never matches
        // it; the fallback probe names it instead of a bare peer-unmounted.
        fs.insert(
            "/kb/lib/.arsumbris/repo.yaml",
            b"name: lib\ndescription: x: y\n".to_vec(),
        );
        let located = [PathBuf::from("/kb/notes")];
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &reg(&[]),
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Unmounted);
        match r.notes.as_slice() {
            [ResolveNote::MalformedRepoYaml(d)] => {
                assert_eq!(d.code.as_str(), "repo-registry-parse-error");
                assert_eq!(d.span.file, PathBuf::from("/kb/lib/.arsumbris/repo.yaml"));
            }
            other => panic!("expected one MalformedRepoYaml note, got {other:?}"),
        }
    }

    #[test]
    fn resolve_registry_path_valid_raises_no_malformed_note() {
        // A dep that resolves cleanly raises no spurious parse note.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/reg/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/notes")];
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &reg(&[("lib", "/reg/lib")]),
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Local(PathBuf::from("/reg/lib")));
        assert!(r.notes.is_empty());
    }

    #[test]
    fn resolve_falls_to_cache_then_unmounted() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        let located = [PathBuf::from("/kb/notes")];
        let registry = reg(&[]);
        // no sibling, no registry entry: cache resolves.
        let with_cache = |n: &RepoName| (n.0 == "lib").then(|| PathBuf::from("/cache/sha"));
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            with_cache,
            &fs,
        );
        assert_eq!(
            r.outcome,
            ResolveOutcome::Cache(PathBuf::from("/cache/sha"))
        );
        // an unknown name resolves to nothing.
        let g = resolve_repo_name(
            &RepoName("ghost".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            with_cache,
            &fs,
        );
        assert_eq!(g.outcome, ResolveOutcome::Unmounted);
        assert!(g.notes.is_empty());
    }

    #[test]
    fn resolve_local_over_cache_fires_override_hint() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/kb/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/kb/notes")];
        let registry = reg(&[]);
        // the name is a locked dependency (cache present) but a sibling shadows it.
        let with_cache = |n: &RepoName| (n.0 == "lib").then(|| PathBuf::from("/cache/sha"));
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            with_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Local(PathBuf::from("/kb/lib")));
        assert_eq!(
            r.notes,
            vec![ResolveNote::PathOverridden {
                name: RepoName("lib".into())
            }]
        );
    }

    #[test]
    fn resolve_ambiguous_sibling_refuses() {
        let mut fs = MemoryFileSystem::new();
        fs.insert("/a/notes/.arsumbris/repo.yaml", reg_yaml("notes"));
        fs.insert("/a/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        fs.insert("/b/other/.arsumbris/repo.yaml", reg_yaml("other"));
        fs.insert("/b/lib/.arsumbris/repo.yaml", reg_yaml("lib"));
        let located = [PathBuf::from("/a/notes"), PathBuf::from("/b/other")];
        let registry = reg(&[]);
        let r = resolve_repo_name(
            &RepoName("lib".into()),
            &SiblingIndex::from_roots(&located, &fs),
            &registry,
            no_cache,
            &fs,
        );
        assert_eq!(r.outcome, ResolveOutcome::Unmounted);
        assert_eq!(
            r.notes,
            vec![ResolveNote::DuplicateSibling {
                name: RepoName("lib".into()),
                roots: vec![PathBuf::from("/a/lib"), PathBuf::from("/b/lib")],
            }]
        );
    }

    // A repo.yaml declaring `name` plus a `deps` list, for the closure tests.
    fn reg_yaml_deps(name: &str, deps: &[&str]) -> Vec<u8> {
        let mut s = format!("name: {name}\ndeps:\n");
        for d in deps {
            s.push_str(&format!("  - name: {d}\n"));
        }
        s.into_bytes()
    }

    #[test]
    fn assemble_members_pulls_transitive_deps() {
        // primary `base` depends on `mid`, which depends on `leaf`; all co-present.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/kb/base/.arsumbris/repo.yaml",
            reg_yaml_deps("base", &["mid"]),
        );
        fs.insert(
            "/kb/mid/.arsumbris/repo.yaml",
            reg_yaml_deps("mid", &["leaf"]),
        );
        fs.insert("/kb/leaf/.arsumbris/repo.yaml", reg_yaml("leaf"));
        let seed = [
            PathBuf::from("/kb/base"),
            PathBuf::from("/kb/mid"),
            PathBuf::from("/kb/leaf"),
        ];
        let out = assemble_members(
            None,
            &[RepoName("base".into())],
            &[],
            &seed,
            &reg(&[]),
            no_cache,
            &fs,
        );
        // all three are members, reached transitively.
        assert_eq!(out.members.len(), 3);
        let base = &out.members[&RepoName("base".into())];
        assert_eq!(
            base.outcome,
            ResolveOutcome::Local(PathBuf::from("/kb/base"))
        );
        assert_eq!(base.role, MemberRole::Edit);
        assert!(base.is_local_tree());
        // mid and leaf are computed deps, resolved to live trees but role Dep.
        let leaf = &out.members[&RepoName("leaf".into())];
        assert_eq!(
            leaf.outcome,
            ResolveOutcome::Local(PathBuf::from("/kb/leaf"))
        );
        assert_eq!(leaf.role, MemberRole::Dep);
        // A local tree (watched), but role Dep, so consumed not editable: the
        // mount signal and editability are separate axes.
        assert!(leaf.is_local_tree() && !leaf.role.editable());
        assert!(out.notes.is_empty());
    }

    #[test]
    fn assemble_members_cache_dep_is_read_only() {
        // primary `app` depends on `ext`, present only in the cache.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/kb/app/.arsumbris/repo.yaml",
            reg_yaml_deps("app", &["ext"]),
        );
        let seed = [PathBuf::from("/kb/app")];
        let cache = |n: &RepoName| (n.as_str() == "ext").then(|| PathBuf::from("/cache/deadbeef"));
        let out = assemble_members(
            None,
            &[RepoName("app".into())],
            &[],
            &seed,
            &reg(&[]),
            cache,
            &fs,
        );
        let ext = &out.members[&RepoName("ext".into())];
        assert_eq!(
            ext.outcome,
            ResolveOutcome::Cache(PathBuf::from("/cache/deadbeef"))
        );
        assert!(
            !ext.is_local_tree(),
            "a cache-only member is a snapshot, not a live local tree"
        );
        assert_eq!(ext.role, MemberRole::Dep);
    }

    #[test]
    fn assemble_members_unmounted_member_has_no_root() {
        // primary `app` depends on `ghost`, resolvable nowhere.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/kb/app/.arsumbris/repo.yaml",
            reg_yaml_deps("app", &["ghost"]),
        );
        let seed = [PathBuf::from("/kb/app")];
        let out = assemble_members(
            None,
            &[RepoName("app".into())],
            &[],
            &seed,
            &reg(&[]),
            no_cache,
            &fs,
        );
        let ghost = &out.members[&RepoName("ghost".into())];
        assert_eq!(ghost.outcome, ResolveOutcome::Unmounted);
        assert!(ghost.root().is_none() && !ghost.is_local_tree());
    }

    #[test]
    fn assemble_members_cycle_terminates_and_assigns_roles() {
        // a <-> b mutual deps; `a` is the edit seed. The fixpoint must terminate.
        let mut fs = MemoryFileSystem::new();
        fs.insert("/kb/a/.arsumbris/repo.yaml", reg_yaml_deps("a", &["b"]));
        fs.insert("/kb/b/.arsumbris/repo.yaml", reg_yaml_deps("b", &["a"]));
        let seed = [PathBuf::from("/kb/a"), PathBuf::from("/kb/b")];
        let out = assemble_members(
            None,
            &[RepoName("a".into())],
            &[],
            &seed,
            &reg(&[]),
            no_cache,
            &fs,
        );
        assert_eq!(out.members.len(), 2);
        // `a` is the edit seed even though `b` deps back onto it (editable wins).
        assert_eq!(out.members[&RepoName("a".into())].role, MemberRole::Edit);
        assert_eq!(out.members[&RepoName("b".into())].role, MemberRole::Dep);
    }

    #[test]
    fn a_discover_seed_adopts_a_deps_explicit_remote_and_ref() {
        // `base` is BOTH a `discover` member (seeded bare, name only) AND `app`'s
        // declared dep WITH an explicit remote + ref. The dep reach must upgrade
        // the stored bare discover entry to carry the remote / ref, else the fetch
        // and the workspace lock fall back to the registry and pin the wrong source.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/kb/app/.arsumbris/repo.yaml",
            b"name: app\ndeps:\n  - name: base\n    remote: git@example.com:base.git\n    ref: v2\n"
                .to_vec(),
        );
        // `base` has no co-present local root, so it stays unmounted; only the
        // declaration source matters here.
        let seed = [PathBuf::from("/kb/app")];
        let out = assemble_members(
            None,
            &[RepoName("app".into())],
            &[RepoName("base".into())],
            &seed,
            &reg(&[]),
            no_cache,
            &fs,
        );
        let base = &out.members[&RepoName("base".into())];
        assert_eq!(
            base.role,
            MemberRole::Dep,
            "the dep reach subsumes the discover seed"
        );
        assert_eq!(
            base.entry.remote.as_deref(),
            Some("git@example.com:base.git"),
            "the dep's explicit remote is adopted over the bare discover seed"
        );
        assert_eq!(base.entry.git_ref.as_deref(), Some("v2"));
    }

    #[test]
    fn assemble_members_dep_reach_subsumes_a_discover_seed() {
        // `base` is a discover member AND `app`'s declared dep. The dep subsumes
        // the discover, so `base` ends up role Dep, not Discover. `host` is a
        // discover-only member, so it stays Discover.
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/kb/app/.arsumbris/repo.yaml",
            reg_yaml_deps("app", &["base"]),
        );
        fs.insert("/kb/base/.arsumbris/repo.yaml", reg_yaml("base"));
        fs.insert("/kb/host/.arsumbris/repo.yaml", reg_yaml("host"));
        let seed = [
            PathBuf::from("/kb/app"),
            PathBuf::from("/kb/base"),
            PathBuf::from("/kb/host"),
        ];
        let out = assemble_members(
            None,
            &[RepoName("app".into())],
            &[RepoName("base".into()), RepoName("host".into())],
            &seed,
            &reg(&[]),
            no_cache,
            &fs,
        );
        assert_eq!(out.members.len(), 3);
        assert_eq!(out.members[&RepoName("app".into())].role, MemberRole::Edit);
        assert_eq!(
            out.members[&RepoName("base".into())].role,
            MemberRole::Dep,
            "a dep reach subsumes a discover seed"
        );
        assert_eq!(
            out.members[&RepoName("host".into())].role,
            MemberRole::Discover,
            "a discover-only member stays discover"
        );
    }

    #[test]
    fn user_registry_parses_name_to_remote_and_path() {
        let bytes = b"repos:\n  - name: notes\n    remote: git@github.com:me/notes.git\n    path: /kb/notes\n  - name: library\n    path: /kb/library\n";
        let reg = parse_user_registry(bytes);
        assert_eq!(reg.len(), 2);
        let notes = reg.get(&RepoName("notes".into())).unwrap();
        assert_eq!(notes.remote.as_deref(), Some("git@github.com:me/notes.git"));
        assert_eq!(notes.path, PathBuf::from("/kb/notes"));
        let library = reg.get(&RepoName("library".into())).unwrap();
        assert_eq!(library.remote, None); // remote optional
        assert_eq!(library.path, PathBuf::from("/kb/library"));
    }

    #[test]
    fn user_registry_skips_entries_missing_name_or_path() {
        // no path -> cannot locate; no name -> nothing to key on.
        let bytes = b"repos:\n  - name: haspath\n    path: /p\n  - name: nopath\n  - path: /q\n";
        let reg = parse_user_registry(bytes);
        assert_eq!(reg.len(), 1);
        assert!(reg.contains_key(&RepoName("haspath".into())));
    }

    #[test]
    fn user_registry_malformed_degrades_to_empty() {
        assert!(parse_user_registry(b"\xff\xfe not utf8").is_empty());
        assert!(parse_user_registry(b": : not yaml :").is_empty());
        assert!(parse_user_registry(b"notrepos:\n  - name: x\n    path: /p\n").is_empty());
    }

    #[test]
    fn device_root_requires_home() {
        // A set, non-empty HOME resolves to `<home>/.arsumbris`. A path that does
        // not exist canonicalizes to itself, so the join is deterministic.
        assert_eq!(
            device_root_from(Some("/no/such/home".into())),
            Ok(PathBuf::from("/no/such/home/.arsumbris"))
        );
        // Unset or empty HOME is a loud refusal, not a fallback.
        assert_eq!(device_root_from(None), Err(HomeUnset));
        assert_eq!(device_root_from(Some("".into())), Err(HomeUnset));
    }

    #[test]
    fn reserved_against_is_equality_not_subtree() {
        // Non-existent paths canonicalize to themselves, so this is deterministic.
        let dev = PathBuf::from("/no/such/home/.arsumbris");
        // The home dir itself is reserved: its `.arsumbris` IS the device area.
        assert!(reserved_against(Path::new("/no/such/home"), &dev));
        // A repo beside it is not.
        assert!(!reserved_against(Path::new("/no/such/home/repo"), &dev));
        // A cache-mounted member UNDER `~/.arsumbris` is not: equality, not subtree.
        assert!(!reserved_against(
            Path::new("/no/such/home/.arsumbris/au-engine/cache/packages/abc"),
            &dev
        ));
    }

    #[cfg(unix)]
    #[test]
    fn reserved_root_resolves_symlinked_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real_home = tmp.path().join("real");
        std::fs::create_dir(&real_home).unwrap();
        let linked_home = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_home, &linked_home).unwrap();

        // The device root is derived from the REAL home. An entry given via the
        // SYMLINK still matches, because both sides are canonicalized.
        let dev = canonical_root(&real_home).join(".arsumbris");
        assert!(reserved_against(&linked_home, &dev));
    }

    #[test]
    fn user_registry_path_uses_injected_config_dir() {
        let dir = PathBuf::from("/cfg");
        assert_eq!(
            user_registry_path(&ConfigSource::Dir(dir.clone())),
            Some(PathBuf::from("/cfg/au-engine/config/repos.yaml"))
        );
    }

    #[test]
    fn config_path_builds_the_scoped_tail_under_the_arsumbris_dir() {
        // Machine scope: the `.arsumbris` dir IS the device root.
        assert_eq!(
            config_path(
                Path::new("/home/me/.arsumbris"),
                "host-app",
                "viewer-defaults.yaml"
            ),
            Ok(PathBuf::from(
                "/home/me/.arsumbris/host-app/config/viewer-defaults.yaml"
            ))
        );
        // Repo scope: the `.arsumbris` dir is `<repo>/.arsumbris`.
        assert_eq!(
            config_path(
                Path::new("/kb/notes/.arsumbris"),
                "agent-tools",
                "tool-permissions.yaml"
            ),
            Ok(PathBuf::from(
                "/kb/notes/.arsumbris/agent-tools/config/tool-permissions.yaml"
            ))
        );
    }

    #[test]
    fn config_path_rejects_traversal_and_escaping_segments() {
        let dir = Path::new("/home/me/.arsumbris");
        // `..`, a leading dot, a separator, and an absolute path are all rejected,
        // in either segment; nothing is built.
        for (consumer, file) in [
            ("..", "x.yaml"),
            ("host-app", ".."),
            (".hidden", "x.yaml"),
            ("host-app", ".hidden"),
            ("a/b", "x.yaml"),
            ("host-app", "a/b"),
            ("host-app", "/etc/passwd"),
            ("", "x.yaml"),
            ("host-app", ""),
            ("host-app", "x\ny.yaml"),
        ] {
            assert!(
                config_path(dir, consumer, file).is_err(),
                "expected reject for consumer={consumer:?} file={file:?}"
            );
        }
    }

    #[test]
    fn config_path_reserves_the_engine_owner_segment() {
        // A machine-scope write with consumer `au-engine` would resolve onto the
        // engine's own `~/.arsumbris/au-engine/config/repos.yaml`, so the channel
        // refuses the reserved owner segment.
        let dir = Path::new("/home/me/.arsumbris");
        assert!(config_path(dir, crate::engine_schema::BUILTIN_ENGINE_REPO, "repos.yaml").is_err());
        assert!(config_path(dir, "au-engine", "anything.yaml").is_err());
        // A dotted consumer that is NOT the engine, and a normal file, still build.
        assert!(config_path(dir, "host-app", "viewer-defaults.yaml").is_ok());
    }

    /// `Empty` reaches NO file on this machine, whatever `HOME` says. This is
    /// the determinism seam: a test constructing an engine with `Empty` cannot
    /// read the developer's real `~/.arsumbris/au-engine/config`, so a fixture naming a
    /// dep that happens to be registered there resolves to nothing rather than
    /// to a real repo on disk.
    #[test]
    fn empty_config_source_reaches_no_file_on_this_machine() {
        assert_eq!(ConfigSource::Empty.base(), None);
        assert_eq!(user_registry_path(&ConfigSource::Empty), None);
        assert_eq!(user_workspaces_path(&ConfigSource::Empty), None);

        // And the load degrades to empty rather than falling back to the env.
        let fs = MemoryFileSystem::new();
        assert!(load_user_registry(&ConfigSource::Empty, &fs).is_empty());
        assert!(load_user_workspaces(&ConfigSource::Empty, &fs).is_empty());
    }

    #[test]
    fn load_user_registry_reads_injected_path_and_degrades_when_absent() {
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/cfg/au-engine/config/repos.yaml",
            b"repos:\n  - name: notes\n    path: /kb/notes\n".to_vec(),
        );
        let cfg = PathBuf::from("/cfg");
        let reg = load_user_registry(&ConfigSource::Dir(cfg.clone()), &fs);
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.get(&RepoName("notes".into())).unwrap().path,
            PathBuf::from("/kb/notes")
        );
        // an absent file is advisory, not a block.
        let empty = load_user_registry(&ConfigSource::Dir("/nope".into()), &fs);
        assert!(empty.is_empty());
    }

    #[test]
    fn user_workspaces_name_defaults_to_the_folder_basename() {
        let bytes = b"workspaces:\n  \
            - path: /kb/my-workspace\n  \
            - name: scratch\n    path: /exp/scratchpad\n";
        let ws = parse_user_workspaces(bytes);
        assert_eq!(ws.len(), 2);
        // No explicit name -> the folder basename `my-workspace`.
        assert_eq!(
            ws.get("my-workspace"),
            Some(&PathBuf::from("/kb/my-workspace"))
        );
        // Explicit alias wins over the basename.
        assert_eq!(ws.get("scratch"), Some(&PathBuf::from("/exp/scratchpad")));
    }

    #[test]
    fn user_workspaces_skips_unnameable_and_keeps_first_on_a_name_clash() {
        // No path -> cannot point anywhere; the filesystem root has no basename,
        // so with no explicit name it is unnameable, skipped. Two folders with the
        // same basename `kb` clash; the first wins.
        let bytes = b"workspaces:\n  \
            - name: noPath\n  \
            - path: /\n  \
            - path: /a/kb\n  \
            - path: /b/kb\n";
        let ws = parse_user_workspaces(bytes);
        assert_eq!(ws.len(), 1, "only the first `kb` survives: {ws:?}");
        // First-wins on the duplicate basename.
        assert_eq!(ws.get("kb"), Some(&PathBuf::from("/a/kb")));
    }

    #[test]
    fn user_workspaces_malformed_degrades_to_empty() {
        assert!(parse_user_workspaces(b"\xff\xfe not utf8").is_empty());
        assert!(
            parse_user_workspaces(b"notworkspaces:\n  - path: /p/.arsumbris/workspace.yaml\n")
                .is_empty()
        );
    }

    #[test]
    fn user_workspaces_path_uses_injected_config_dir() {
        let dir = PathBuf::from("/cfg");
        assert_eq!(
            user_workspaces_path(&ConfigSource::Dir(dir.clone())),
            Some(PathBuf::from("/cfg/au-engine/config/workspaces.yaml"))
        );
    }

    #[test]
    fn load_user_workspaces_reads_injected_path_and_degrades_when_absent() {
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/cfg/au-engine/config/workspaces.yaml",
            b"workspaces:\n  - path: /kb/my-workspace\n".to_vec(),
        );
        let cfg = PathBuf::from("/cfg");
        let ws = load_user_workspaces(&ConfigSource::Dir(cfg.clone()), &fs);
        assert_eq!(
            ws.get("my-workspace"),
            Some(&PathBuf::from("/kb/my-workspace"))
        );
        assert!(load_user_workspaces(&ConfigSource::Dir("/nope".into()), &fs).is_empty());
    }

    #[test]
    fn register_writes_a_new_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        let target = dir.path().join("notes");
        std::fs::create_dir_all(target.join(".arsumbris")).unwrap();
        std::fs::write(target.join(".arsumbris/repo.yaml"), "name: notes\n").unwrap();

        let out = register_entry(
            &reg_path,
            &RepoName("notes".into()),
            Some("git@x:me/notes.git"),
            &target,
        )
        .unwrap();
        assert_eq!(out, RegisterOutcome::Written);
        let reg = parse_user_registry(&std::fs::read(&reg_path).unwrap());
        let loc = reg.get(&RepoName("notes".into())).unwrap();
        assert_eq!(loc.remote.as_deref(), Some("git@x:me/notes.git"));
        assert_eq!(loc.path, target);
    }

    #[test]
    fn register_write_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        let mk = |name: &str| {
            let target = dir.path().join(name);
            std::fs::create_dir_all(target.join(".arsumbris")).unwrap();
            std::fs::write(
                target.join(".arsumbris/repo.yaml"),
                format!("name: {name}\n"),
            )
            .unwrap();
            target
        };

        register_entry(&reg_path, &RepoName("notes".into()), None, &mk("notes")).unwrap();
        // A second register rewrites the existing file via temp + rename.
        register_entry(&reg_path, &RepoName("other".into()), None, &mk("other")).unwrap();

        // The read-modify-write preserved the first entry across the rewrite.
        let reg = parse_user_registry(&std::fs::read(&reg_path).unwrap());
        assert!(reg.contains_key(&RepoName("notes".into())));
        assert!(reg.contains_key(&RepoName("other".into())));

        // No scratch temp file leaked beside the target (the rename consumed it).
        let leaked = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".repos.yaml.tmp"))
            });
        assert!(!leaked, "atomic write leaked a temp file");
    }

    #[test]
    fn register_rejects_invalid_and_injection_strings() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");

        // An invalid name (a path separator) is rejected before any I/O, also the
        // sibling_scan path-component safety gate.
        let out =
            register_entry(&reg_path, &RepoName("a/b".into()), None, Path::new("/x")).unwrap();
        assert!(
            matches!(out, RegisterOutcome::Conflict(_)),
            "invalid name: {out:?}"
        );

        // A newline in the name would forge a second registry entry on the next
        // parse (a bare-scalar breakout); rejected.
        let out = register_entry(
            &reg_path,
            &RepoName("app\n  - name: victim\n    path: /evil".into()),
            None,
            Path::new("/x"),
        )
        .unwrap();
        assert!(
            matches!(out, RegisterOutcome::Conflict(_)),
            "newline name: {out:?}"
        );

        // The same breakout via the path field; rejected.
        let out = register_entry(
            &reg_path,
            &RepoName("notes".into()),
            None,
            Path::new("/evil\n  - name: victim\n    path: /x"),
        )
        .unwrap();
        assert!(
            matches!(out, RegisterOutcome::Conflict(_)),
            "newline path: {out:?}"
        );

        // No rejection wrote anything — no forged entry ever lands.
        assert!(!reg_path.exists(), "a rejected register writes nothing");
    }

    #[test]
    fn an_invalid_repo_name_does_not_resolve() {
        // A repo.yaml whose name carries a path separator is not a repo root:
        // parse_entry rejects it, so no such name is ever built into a RepoName
        // that sibling_scan could join outside its parent.
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: ../evil\n"),
            ("/v/note.md", "x"),
        ]);
        let (map, diags) = discover(&fs, &files);
        assert!(
            diags.iter().any(|d| d.code.as_str() == "repo-name-missing"),
            "an invalid name surfaces repo-name-missing: {diags:?}"
        );
        let r = map.repo_of(Path::new("/v/note.md")).unwrap();
        assert_ne!(
            r.name.as_str(),
            "../evil",
            "an invalid name never becomes a resolving RepoName"
        );
        assert!(
            !r.declared,
            "the invalid-named root degrades to an implicit repo"
        );
    }

    #[test]
    fn register_refuses_a_disagreeing_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        std::fs::write(
            &reg_path,
            "repos:\n  - name: notes\n    remote: git@x:me/notes.git\n    path: /old\n",
        )
        .unwrap();
        let target = dir.path().join("notes");
        std::fs::create_dir_all(target.join(".arsumbris")).unwrap();
        std::fs::write(target.join(".arsumbris/repo.yaml"), "name: notes\n").unwrap();

        let out = register_entry(
            &reg_path,
            &RepoName("notes".into()),
            Some("git@x:me/OTHER.git"),
            &target,
        )
        .unwrap();
        assert!(matches!(out, RegisterOutcome::Conflict(_)), "{out:?}");
        // the file is unchanged.
        let reg = parse_user_registry(&std::fs::read(&reg_path).unwrap());
        assert_eq!(
            reg.get(&RepoName("notes".into())).unwrap().path,
            PathBuf::from("/old")
        );
    }

    #[test]
    fn concurrent_registers_do_not_lose_updates() {
        use std::sync::Arc;
        // N target checkouts, each declaring its own name.
        let dir = Arc::new(tempfile::TempDir::new().unwrap());
        let reg_path = dir.path().join("repos.yaml");
        let n = 8;
        for i in 0..n {
            let name = format!("dep{i}");
            let arsumbris = dir.path().join(&name).join(".arsumbris");
            std::fs::create_dir_all(&arsumbris).unwrap();
            std::fs::write(arsumbris.join("repo.yaml"), format!("name: {name}\n")).unwrap();
        }
        // Register all N concurrently, each from its own thread (its own lock-file
        // fd). The `flock` serializes the read-modify-write, so every entry
        // survives; without it, racing `rename`s would drop some (a lost update).
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let dir = Arc::clone(&dir);
                std::thread::spawn(move || {
                    let reg = dir.path().join("repos.yaml");
                    let name = format!("dep{i}");
                    let target = dir.path().join(&name);
                    register_entry(&reg, &RepoName(name), None, &target).unwrap()
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), RegisterOutcome::Written);
        }
        let reg = parse_user_registry(&std::fs::read(&reg_path).unwrap());
        for i in 0..n {
            assert!(
                reg.contains_key(&RepoName(format!("dep{i}"))),
                "dep{i} survived the concurrent registers; registry: {reg:?}"
            );
        }
    }

    #[test]
    fn concurrent_device_config_writes_never_tear() {
        use std::sync::Arc;
        // N threads write DISTINCT contents to the ONE device-global file at once.
        // The `flock` serializes the read-modify-write and `atomic_write` renames,
        // so the final file is exactly ONE complete written value, never a torn
        // mix, and every write succeeds.
        let dir = Arc::new(tempfile::TempDir::new().unwrap());
        let path = dir.path().join("host-app").join("config").join("v.yaml");
        let n = 8;
        let values: Vec<String> = (0..n)
            .map(|i| format!("type: t\nviewer: pane-{i}\n"))
            .collect();
        let values = Arc::new(values);
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = path.clone();
                let values = Arc::clone(&values);
                std::thread::spawn(move || {
                    write_device_config_with(&path, None, |_| Ok(values[i].clone())).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // The final file is one of the written values, byte-for-byte (no tear),
        // and no scratch temp leaked.
        let final_content = std::fs::read_to_string(&path).unwrap();
        assert!(
            values.iter().any(|v| *v == final_content),
            "the final file is one complete written value, not a torn mix: {final_content:?}"
        );
        let leaked = std::fs::read_dir(path.parent().unwrap()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_str()
                .is_some_and(|nm| nm.contains(".tmp"))
        });
        assert!(!leaked, "atomic write leaked a temp file");
    }

    #[test]
    fn device_config_expected_hash_compare_and_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("host-app").join("config").join("v.yaml");
        // First write creates the file.
        let hash =
            write_device_config_with(&path, None, |_| Ok("type: t\nviewer: a\n".to_string()))
                .unwrap();
        let hex = crate::mutate::hash_hex(hash);
        // A wrong expected_hash rejects, nothing written.
        assert!(write_device_config_with(&path, Some("deadbeef"), |_| Ok(
            "type: t\nviewer: b\n".to_string()
        ))
        .is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "type: t\nviewer: a\n"
        );
        // The matching hash succeeds.
        write_device_config_with(
            &path,
            Some(&hex),
            |_| Ok("type: t\nviewer: b\n".to_string()),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "type: t\nviewer: b\n"
        );
    }

    #[test]
    fn register_preserves_remote_on_a_bare_reregister() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        let target = dir.path().join("notes");
        std::fs::create_dir_all(target.join(".arsumbris")).unwrap();
        std::fs::write(target.join(".arsumbris/repo.yaml"), "name: notes\n").unwrap();

        // First register records a remote.
        register_entry(
            &reg_path,
            &RepoName("notes".into()),
            Some("git@x:me/notes.git"),
            &target,
        )
        .unwrap();
        // A bare re-register (no remote) must NOT clear the recorded remote.
        let out = register_entry(&reg_path, &RepoName("notes".into()), None, &target).unwrap();
        assert_eq!(out, RegisterOutcome::Written);
        let reg = parse_user_registry(&std::fs::read(&reg_path).unwrap());
        assert_eq!(
            reg.get(&RepoName("notes".into()))
                .unwrap()
                .remote
                .as_deref(),
            Some("git@x:me/notes.git"),
            "a bare re-register preserves the recorded remote"
        );
        // The preserved remote keeps identity-check-1 armed for the next register.
        let out = register_entry(
            &reg_path,
            &RepoName("notes".into()),
            Some("git@x:me/OTHER.git"),
            &target,
        )
        .unwrap();
        assert!(
            matches!(out, RegisterOutcome::Conflict(_)),
            "guard rearmed: {out:?}"
        );
    }

    #[test]
    fn register_refuses_an_absent_target_repo_yaml() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        // A path with no `.arsumbris/repo.yaml`: register-before-clone is refused,
        // a registered path must be an existing name-declaring checkout.
        let target = dir.path().join("not-a-checkout");
        std::fs::create_dir_all(&target).unwrap();
        let out = register_entry(&reg_path, &RepoName("notes".into()), None, &target).unwrap();
        assert!(matches!(out, RegisterOutcome::Conflict(_)), "{out:?}");
        assert!(
            !reg_path.exists(),
            "nothing written when the target has no repo.yaml"
        );
    }

    #[test]
    fn register_refuses_when_target_declares_another_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg_path = dir.path().join("repos.yaml");
        let target = dir.path().join("mislabeled");
        std::fs::create_dir_all(target.join(".arsumbris")).unwrap();
        std::fs::write(target.join(".arsumbris/repo.yaml"), "name: other\n").unwrap();

        let out = register_entry(&reg_path, &RepoName("notes".into()), None, &target).unwrap();
        assert!(matches!(out, RegisterOutcome::Conflict(_)), "{out:?}");
        assert!(!reg_path.exists(), "nothing written on conflict");
    }

    #[test]
    fn undeclared_entry_is_one_implicit_root_repo() {
        let (fs, files) = repo_with(&[("/v/note.md", "x"), ("/v/sub/foo.md", "y")]);
        let (map, diags) = discover(&fs, &files);
        assert!(diags.is_empty());
        assert_eq!(map.repos().len(), 1);
        let r = map.repo_of(Path::new("/v/sub/foo.md")).unwrap();
        assert_eq!(r.root, PathBuf::from("/v"));
        assert!(!r.declared);
    }

    #[test]
    fn declared_repo_root_uses_its_name() {
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: root\n"),
            ("/v/note.md", "x"),
        ]);
        let (map, diags) = discover(&fs, &files);
        // the repo-root repo declares `root` in a `v` folder, an expected drift.
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() == "repo-folder-name-mismatch"),
            "{diags:?}"
        );
        let r = map.repo_of(Path::new("/v/note.md")).unwrap();
        assert_eq!(r.name.as_str(), "root");
        assert!(r.declared);
    }

    #[test]
    fn folder_name_mismatch_is_case_insensitive() {
        // A case-only difference (`MyRepo` in `myrepo/`) does NOT drift — a repo
        // cloned onto a case-insensitive filesystem is a legitimate layout, not a
        // high-attention convention breach.
        let (fs, files) = repo_with(&[
            ("/v/myrepo/.arsumbris/repo.yaml", "name: MyRepo\n"),
            ("/v/myrepo/note.md", "x"),
        ]);
        let (_map, diags) = discover(&fs, &files);
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_str() == "repo-folder-name-mismatch"),
            "a case-only folder difference does not drift: {diags:?}"
        );

        // A genuine mismatch (different letters) still drifts.
        let (fs, files) = repo_with(&[
            ("/v/bar/.arsumbris/repo.yaml", "name: foo\n"),
            ("/v/bar/note.md", "x"),
        ]);
        let (_map, diags) = discover(&fs, &files);
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_str() == "repo-folder-name-mismatch"),
            "a real folder-name mismatch still drifts: {diags:?}"
        );
    }

    #[test]
    fn discover_finds_a_content_less_repo_by_marker_not_by_file_ancestor() {
        // A nested repo holding ONLY its `repo.yaml`, no content files, has no
        // walked file whose ancestor reveals it. Marker-fed discovery still finds
        // it (the build passes the walker's markers to `registry_files`); the
        // retired file-ancestor scan would miss it. Regression guard for the
        // fragility the bounded walk exposed: a `discover` fed only file ancestors
        // silently drops a content-less repo.
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: v\n"),
            ("/v/note.md", "x"),
            ("/v/cl/.arsumbris/repo.yaml", "name: cl\n"),
            // no content under /v/cl: only its marker can reveal it.
        ]);
        let (map, diags) = discover(&fs, &files);
        assert!(diags.is_empty(), "no drift, names match folders: {diags:?}");
        assert!(
            map.by_name("cl").is_some(),
            "a content-less repo is discovered by its marker, not by a file ancestor"
        );
        assert_eq!(
            map.repos().len(),
            2,
            "both the entry and the content-less repo"
        );
    }

    #[test]
    fn nested_repo_takes_membership_in_its_subtree() {
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: root\n"),
            ("/v/note.md", "x"),
            ("/v/pkg/.arsumbris/repo.yaml", "name: bento\n"),
            ("/v/pkg/inner.md", "y"),
        ]);
        let (map, diags) = discover(&fs, &files);
        // both repos declare names differing from their folders, expected drift.
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() == "repo-folder-name-mismatch"),
            "{diags:?}"
        );
        assert_eq!(
            map.repo_of(Path::new("/v/note.md")).unwrap().name.as_str(),
            "root"
        );
        assert_eq!(
            map.repo_of(Path::new("/v/pkg/inner.md"))
                .unwrap()
                .name
                .as_str(),
            "bento"
        );
        assert!(map.by_name("bento").is_some());
    }

    #[test]
    fn repo_of_is_deterministic_when_two_repos_share_a_root() {
        // A member-path collision can leave two repos at the identical root.
        // `repo_of` must resolve the same one regardless of the vector's
        // order — depth and root are equal, so the unique name breaks the tie.
        fn repo_at(root: &str, name: &str) -> Repo {
            Repo {
                root: PathBuf::from(root),
                name: RepoName(name.into()),
                declared: false,
                builtin: false,
                description: None,
                remote: None,
                deps: Vec::new(),
                peer_paths: BTreeMap::new(),
            }
        }
        let forward = RepoMap::from_repos(vec![repo_at("/v", "alpha"), repo_at("/v", "beta")]);
        let reversed = RepoMap::from_repos(vec![repo_at("/v", "beta"), repo_at("/v", "alpha")]);
        let p = Path::new("/v/note.md");
        assert_eq!(forward.repo_of(p).unwrap().name.as_str(), "beta");
        assert_eq!(reversed.repo_of(p).unwrap().name.as_str(), "beta");
    }

    #[test]
    fn duplicate_repo_name_is_diagnosed() {
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "name: dup\n"),
            ("/v/a.md", "x"),
            ("/v/pkg/.arsumbris/repo.yaml", "name: dup\n"),
            ("/v/pkg/b.md", "y"),
        ]);
        let (map, diags) = discover(&fs, &files);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.code.as_str() == "duplicate-repo-name")
                .count(),
            1
        );
        // The duplicate registry is ignored, but its directory stays its own
        // repo: it degrades to an implicit repo named for its directory, so its
        // files are not absorbed into the parent.
        assert_eq!(map.repos().len(), 2);
        let pkg = map.repo_of(Path::new("/v/pkg/b.md")).unwrap();
        assert_eq!(pkg.root, PathBuf::from("/v/pkg"));
        assert!(!pkg.declared);
        assert_eq!(pkg.name.as_str(), "pkg");
    }

    #[test]
    fn missing_self_is_diagnosed() {
        let (fs, files) = repo_with(&[
            ("/v/.arsumbris/repo.yaml", "description: no self\n"),
            ("/v/a.md", "x"),
        ]);
        let (_map, diags) = discover(&fs, &files);
        assert!(diags.iter().any(|d| d.code.as_str() == "repo-name-missing"));
    }

    #[test]
    fn resolve_deps_fills_peer_paths_from_a_co_present_sibling() {
        let (fs, files) = repo_with(&[
            (
                "/v/app/.arsumbris/repo.yaml",
                "name: app\ndeps:\n  - name: base\n",
            ),
            ("/v/base/.arsumbris/repo.yaml", "name: base\n"),
            ("/v/app/note.md", "x"),
        ]);
        let (mut map, diags) = discover(&fs, &files);
        assert!(diags.is_empty(), "{diags:?}");
        let notes = map.resolve_deps(&UserRegistry::new(), &fs);
        assert!(notes.is_empty(), "{notes:?}");
        let app = map.by_name("app").expect("app declared");
        assert_eq!(
            app.peer_paths.get(&RepoName("base".into())),
            Some(&PathBuf::from("/v/base")),
            "a co-present sibling resolves the dep"
        );
    }

    #[test]
    fn parse_workspace_manifest_rejects_a_traversing_name() {
        // A `sibling_scan` joins a name onto a parent dir, so a traversing scalar
        // must never become a `RepoName`.
        let m = parse_workspace_manifest(b"edit:\n  - app\n  - ../../x\ndiscover:\n  - ok\n");
        assert_eq!(
            m.edit,
            vec![RepoName("app".into())],
            "a traversing edit name is dropped"
        );
        assert_eq!(m.discover, vec![RepoName("ok".into())]);
    }

    #[test]
    fn workspace_name_of_a_folder_repo_manifest_is_the_container_folder() {
        assert_eq!(
            workspace_name(Path::new("/ws/.arsumbris/workspace.yaml")),
            "ws",
            "a folder-repo manifest is named after its containing repo folder"
        );
    }

    #[test]
    fn load_workspace_resolves_edit_and_computed_deps() {
        // The manifest names only the edit members; `app` depends on `base`, a
        // co-present sibling, so `base` is a computed member.
        let manifest = "edit:\n  - app\n";
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            reg_yaml_deps("app", &["base"]),
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", reg_yaml("base"));
        let seed = [PathBuf::from("/v/app"), PathBuf::from("/v/base")];
        let ws = load_workspace(
            Path::new("/v/demo/.arsumbris/workspace.yaml"),
            manifest.as_bytes(),
            &seed,
            &UserRegistry::new(),
            &fs,
        );
        assert_eq!(ws.name, "demo");
        assert_eq!(ws.edit, vec![RepoName("app".into())]);
        // edit member plus its computed dep.
        assert_eq!(ws.members.len(), 2);
        assert_eq!(
            ws.member_paths.get(&RepoName("app".into())),
            Some(&PathBuf::from("/v/app"))
        );
        assert_eq!(
            ws.member_paths.get(&RepoName("base".into())),
            Some(&PathBuf::from("/v/base")),
            "the computed dep resolves via the co-present sibling seed"
        );
    }

    #[test]
    fn load_workspace_derives_role_from_edit_selection() {
        // `app` is an edit member (editable); `base` is a computed dep.
        let manifest = "edit:\n  - app\n";
        let mut fs = MemoryFileSystem::new();
        fs.insert(
            "/v/app/.arsumbris/repo.yaml",
            reg_yaml_deps("app", &["base"]),
        );
        fs.insert("/v/base/.arsumbris/repo.yaml", reg_yaml("base"));
        let seed = [PathBuf::from("/v/app"), PathBuf::from("/v/base")];
        let ws = load_workspace(
            Path::new("/v/demo/.arsumbris/workspace.yaml"),
            manifest.as_bytes(),
            &seed,
            &UserRegistry::new(),
            &fs,
        );
        assert_eq!(
            ws.member_roles.get(&RepoName("app".into())),
            Some(&MemberRole::Edit),
            "an edit member is editable"
        );
        assert_eq!(
            ws.member_roles.get(&RepoName("base".into())),
            Some(&MemberRole::Dep),
            "a computed transitive dep is a dependency member"
        );
    }

    #[test]
    fn load_workspace_unmounted_edit_member_has_no_path() {
        // An edit member that resolves nowhere is a member with no path (surfaced
        // by the caller as edit-member-unmounted).
        let manifest = "edit:\n  - ghost\n";
        let ws = load_workspace(
            Path::new("/v/demo/.arsumbris/workspace.yaml"),
            manifest.as_bytes(),
            &[],
            &UserRegistry::new(),
            &MemoryFileSystem::new(),
        );
        assert_eq!(ws.edit, vec![RepoName("ghost".into())]);
        assert!(ws.member_paths.is_empty());
        assert_eq!(ws.members.len(), 1, "still a member, just unmounted");
    }

    #[test]
    fn load_workspace_resolves_a_content_less_primary_from_marker_seed_roots() {
        // `proj` is a co-present primary with ONLY a repo.yaml (no walkable
        // content). The walker surfaces its `repo.yaml` marker, so the build
        // passes its root in `seed_roots`; load_workspace then resolves it with no
        // manifest-dir guess (that seed-hack is retired, discovery is the marker).
        let manifest = "edit:\n  - proj\n";
        let mut fs = MemoryFileSystem::new();
        fs.insert("/ws/proj/.arsumbris/repo.yaml", reg_yaml("proj"));
        let ws = load_workspace(
            Path::new("/ws/demo/.arsumbris/workspace.yaml"),
            manifest.as_bytes(),
            &[PathBuf::from("/ws/proj")],
            &UserRegistry::new(),
            &fs,
        );
        assert_eq!(
            ws.member_paths.get(&RepoName("proj".into())),
            Some(&PathBuf::from("/ws/proj")),
            "a content-less primary resolves from its marker-derived seed root"
        );
    }

    #[test]
    fn resolve_deps_leaves_unresolved_deps_unmounted() {
        let (fs, files) = repo_with(&[
            (
                "/v/app/.arsumbris/repo.yaml",
                "name: app\ndeps:\n  - name: base\n",
            ),
            ("/v/app/note.md", "x"),
        ]);
        let (mut map, _) = discover(&fs, &files);
        let notes = map.resolve_deps(&UserRegistry::new(), &fs);
        assert!(notes.is_empty());
        let app = map.by_name("app").unwrap();
        assert!(
            app.peer_paths.is_empty(),
            "no sibling, no registry: the dep is unmounted"
        );
    }

    #[test]
    fn self_and_peers_with_remote_parse() {
        let registry = "\
name: app
description: the app repo
remote: git@example.com:me/app.git
deps:
  - name: base
    remote: git@example.com:me/base.git
  - name: shared
";
        let (fs, files) = repo_with(&[("/v/.arsumbris/repo.yaml", registry), ("/v/a.md", "x")]);
        let (map, diags) = discover(&fs, &files);
        // `app` declared in a `v` folder, an expected folder-name drift.
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_str() == "repo-folder-name-mismatch"),
            "{diags:?}"
        );
        let r = map.by_name("app").expect("app declared");
        assert_eq!(r.description.as_deref(), Some("the app repo"));
        assert_eq!(r.remote.as_deref(), Some("git@example.com:me/app.git"));
        // Peers carry identity; `base` has a remote, `shared` is name-only.
        assert_eq!(r.deps.len(), 2);
        let base = r.deps.iter().find(|p| p.name.as_str() == "base").unwrap();
        assert_eq!(base.remote.as_deref(), Some("git@example.com:me/base.git"));
        let shared = r.deps.iter().find(|p| p.name.as_str() == "shared").unwrap();
        assert!(shared.remote.is_none());
    }

    #[test]
    fn package_lock_round_trips() {
        let mut lock = PackageLock::new();
        lock.insert(
            RepoName("bento".into()),
            LockedPackage {
                remote: "https://example.com/org/bento".into(),
                sha: "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c".into(),
                // A monorepo subpath round-trips.
                path: Some("projections/bento".into()),
            },
        );
        lock.insert(
            RepoName("atlas".into()),
            LockedPackage {
                remote: "git@example.com:org/atlas".into(),
                sha: "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4".into(),
                path: None,
            },
        );
        // An all-numeric sha is rare but legal; the parser must read it as text,
        // not type it as a YAML number.
        lock.insert(
            RepoName("numeric".into()),
            LockedPackage {
                remote: "https://example.com/org/numeric".into(),
                sha: "1234567890123456789012345678901234567890".into(),
                path: None,
            },
        );
        let text = serialize_package_lock(&lock, "au.engine.repo-lock");
        // The engine-written lock self-describes with its qualified type.
        assert!(text.starts_with("type: au.engine.repo-lock::au-engine\n"));
        // Sorted by name, deterministic: atlas before bento.
        assert!(text.find("atlas").unwrap() < text.find("bento").unwrap());
        // The reader ignores the `type:` key, round-tripping the pins.
        assert_eq!(parse_package_lock(text.as_bytes()), lock);
    }

    #[test]
    fn registry_parses_entries_and_drops_incomplete_ones() {
        let reg = "\
packages:
  - name: bento
    remote: git@example.com:org/bento.git
    ref: v1
    path: projections/bento
    description: a thing
  - name: atlas
    remote: git@example.com:org/atlas.git
    ref: main
  - name: broken
    remote: git@example.com:org/broken.git
";
        let parsed = parse_package_registry(reg.as_bytes());
        assert_eq!(parsed.len(), 2, "the ref-less entry is dropped");
        let bento = parsed.get(&RepoName("bento".into())).unwrap();
        assert_eq!(bento.remote, "git@example.com:org/bento.git");
        assert_eq!(bento.git_ref, "v1");
        assert_eq!(bento.path.as_deref(), Some("projections/bento"));
        assert_eq!(bento.description.as_deref(), Some("a thing"));
        let atlas = parsed.get(&RepoName("atlas".into())).unwrap();
        assert_eq!(atlas.git_ref, "main");
        assert!(atlas.path.is_none());
        assert!(atlas.description.is_none());
    }

    #[test]
    fn repo_lock_path_sits_beside_repo_yaml() {
        let p = repo_lock_path(Path::new("/repos/notes"));
        assert_eq!(p, PathBuf::from("/repos/notes/.arsumbris/repo.lock"));
    }

    #[test]
    fn malformed_package_lock_degrades_to_empty() {
        assert!(parse_package_lock(b"not: a packages list\n").is_empty());
        // An entry missing a field is dropped, not half-read.
        let partial = "packages:\n  - name: bento\n    sha: abc\n";
        assert!(parse_package_lock(partial.as_bytes()).is_empty());
    }
}
