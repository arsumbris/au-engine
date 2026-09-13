//! The continuous engine: a held IR over a knowledge base, rebuilt on change.
//!
//! [`Engine`] holds a [`KnowledgeBase`] in memory and keeps it current. A rebuild reuses
//! every unchanged file's parse from the prior build and recomputes the graph
//! and validation whole-knowledge-base, then swaps the held knowledge base and advances a
//! monotonic version. A rebuild is scoped to what changed: the mutation channel
//! and the watcher pass the changed paths, so only those are read and re-parsed.
//! A whole-knowledge-base pass is the fallback, on the first build, a watcher rescan or
//! error, and the watcher's idle reconcile. A change whose content matches the
//! held one costs nothing: no rebuild, no version advance.
//!
//! Lifecycle follows [[design - engine shape]], scoped to the engine's single
//! ref: the engine is Starting until its first build completes, then Up;
//! the ref is Deriving until that first build, then Ready and staying Ready
//! across later rebuilds. A read on a Deriving ref returns not-ready.
//!
//! A watcher thread drives rebuilds from disk changes, debounced to a quiescence
//! window, passing the changed paths so the rebuild reads only those. A
//! configurable idle reconcile ([`ReconcilePolicy`]) does a whole-knowledge-base pass
//! after the watcher goes quiet, backstopping silently-dropped events. The
//! rebuild logic is also callable directly so a harness can drive the engine
//! deterministically without the watcher.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use au_diagnostics::Diagnostic;
use au_parser::{FileSystem, RealFileSystem};
use notify::{RecursiveMode, Watcher};
use tokio::sync::watch;

use crate::build::{build_reusing, fingerprint, Fingerprint};
use crate::incremental::{
    apply_recompute, classify_dirty, expand_removed_directories, is_engine_schema_path,
    recompute_dirty, DirtyClass,
};
use crate::ir::ContentHash;
use crate::mutate::{MutationReject, PreviewOp, PreviewStamp};
use crate::overlay::{OverlayFileSystem, Override};
use crate::KnowledgeBase;

/// The engine-level lifecycle, coarse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// The process is up; the first build has not completed.
    Starting,
    /// The engine accepts work; per-ref readiness governs each answer.
    Up,
}

/// The single ref's readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefState {
    /// The ref's views are being built for the first time. Reads return
    /// not-ready.
    Deriving,
    /// The ref's views are current. Reads resolve; rebuilds advance the
    /// version without leaving Ready.
    Ready,
}

/// The outcome of a read: either the ref is not yet ready, or a value stamped
/// with the version it was observed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read<T> {
    NotReady,
    Ready { version: u64, value: T },
}

impl<T> Read<T> {
    /// The value, if ready.
    pub fn value(self) -> Option<T> {
        match self {
            Read::Ready { value, .. } => Some(value),
            Read::NotReady => None,
        }
    }
}

/// The default quiescence window: external edits within this window of each
/// other coalesce into one rebuild.
const QUIESCENCE: Duration = Duration::from_millis(150);

/// The default idle-reconcile window: after this much quiet, the watcher does
/// one whole-knowledge-base reconcile to catch silently-dropped events.
const DEFAULT_RECONCILE_WINDOW: Duration = Duration::from_secs(5);

/// How much of the knowledge base a rebuild treats as possibly changed.
///
/// `Full` re-detects change across the whole knowledge base, the always-correct path.
/// `Paths` trusts a dirty set: those paths are read, every other walked file is
/// assumed unchanged and reuses its parse with no read. The set's source owns
/// its correctness, a mutation target is exact, a watcher set is backed by the
/// reconcile policy.
enum RebuildScope {
    Full,
    Paths(BTreeSet<PathBuf>),
}

/// Whether and how often the watcher does a whole-knowledge-base reconcile to catch
/// filesystem events the backend silently dropped.
///
/// The dirty set the watcher builds from notify events can miss a change when
/// the backend coalesces or drops an event without a rescan signal. The
/// reconcile is the backstop. It is configurable because it is only needed when
/// the watcher is relied on: where every write goes through the mutation channel
/// the dirty set is exact and the reconcile is pure overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcilePolicy {
    /// No reconcile. Correct when the mutation channel is the only writer.
    Off,
    /// A full reconcile after the watcher is idle for `window`.
    Idle { window: Duration },
}

impl Default for ReconcilePolicy {
    fn default() -> Self {
        ReconcilePolicy::Idle {
            window: DEFAULT_RECONCILE_WINDOW,
        }
    }
}

impl ReconcilePolicy {
    /// Read the policy from the environment, the per-environment knob.
    /// `AU_IDLE_RECONCILE=off` disables it; a positive integer sets the idle
    /// window in seconds; unset or unparseable falls back to the default.
    pub fn from_env() -> Self {
        Self::parse(std::env::var("AU_IDLE_RECONCILE").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            None => Self::default(),
            Some(v) if v.eq_ignore_ascii_case("off") => ReconcilePolicy::Off,
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|secs| *secs > 0)
                .map(|secs| ReconcilePolicy::Idle {
                    window: Duration::from_secs(secs),
                })
                .unwrap_or_default(),
        }
    }

    /// The idle window, `None` when off.
    fn window(self) -> Option<Duration> {
        match self {
            ReconcilePolicy::Off => None,
            ReconcilePolicy::Idle { window } => Some(window),
        }
    }
}

/// Fold one watcher message into the accumulating dirty set, returning whether
/// it forces a whole-knowledge-base rebuild.
///
/// A notify error or a rescan signal cannot be attributed to specific paths, so
/// it forces `Full`. Otherwise the event's paths join the dirty set; an empty
/// path list contributes nothing.
fn accumulate_event(dirty: &mut BTreeSet<PathBuf>, message: notify::Result<notify::Event>) -> bool {
    // One span per message the backend delivered, so "the watcher told us
    // something" is an OBSERVABLE FACT rather than something a reader infers.
    //
    // It exists because the inference was made and was wrong. A harness reported
    // "the watcher event never arrived" whenever a whole-knowledge-base
    // reconcile served a row, which only ever showed WHICH PATH RAN; nothing
    // watched the event stream. That reading then propagated into notes and a
    // design as though it were measured. A span here is what lets the claim be
    // checked instead of assumed.
    //
    // `kind` distinguishes a real change from the two signals that force a full
    // rebuild, so a rescan is never miscounted as ordinary traffic.
    let kind = match &message {
        Err(_) => "error",
        Ok(event) if event.need_rescan() => "rescan",
        Ok(_) => "change",
    };
    let _seen = tracing::info_span!("watch_event", kind).entered();
    match message {
        Err(_) => true,
        Ok(event) if event.need_rescan() => true,
        Ok(event) => {
            dirty.extend(event.paths);
            false
        }
    }
}

/// The paths whose fingerprint entry differs between two passes: added,
/// removed, or changed in content.
///
/// KEYS AND VALUES, and the key half is not an afterthought. A file the build
/// never reads carries NO hash at all, so an asset add or delete moves only the
/// key set and a value-only comparison would see nothing change. That is the
/// same conflation of "has no content" with "is not here" that the asset work
/// had to fix twice, in the no-op gate and in the scoped fingerprint update.
///
/// This is what turns the reconcile from a detector into an attributor: it
/// already computed both fingerprints, and asking only whether they are EQUAL
/// threw away the one thing that makes recovery cheap.
fn fingerprint_diff(prior: &Fingerprint, next: &Fingerprint) -> BTreeSet<PathBuf> {
    let mut dirty: BTreeSet<PathBuf> = BTreeSet::new();
    // Added, or content changed. `prior.get` is `None` for a path the prior pass
    // never saw, which differs from `Some(&None)`, a path it saw and did not read.
    for (path, hash) in next {
        if prior.get(path) != Some(hash) {
            dirty.insert(path.clone());
        }
    }
    // Removed.
    for path in prior.keys() {
        if !next.contains_key(path) {
            dirty.insert(path.clone());
        }
    }
    dirty
}

/// Whether a dirty path's on-disk state differs from what the last build's
/// [`Fingerprint`] recorded for it. The scoped no-op gate's one definition.
///
/// Both rebuild paths ask this, and they used to ask it with a copy each. The
/// copies agreed on the read case and were both wrong on the other, which is the
/// argument for one definition rather than two correct-looking ones.
///
/// The split is [`crate::build::build_reads_content`]:
/// - a path the build READS is compared by content hash.
/// - a path it does not is compared by PRESENCE, and never read. Its bytes are
///   not in any held state, so its content cannot differ; what can is whether it
///   is there, since it is a `RepoIndex` member that resolves `file*` and
///   navigational references. Hashing it instead gets BOTH directions wrong: an
///   edit reads as a change (`Some` against the recorded `None`) and a delete
///   reads as no change (an absent file hashes to `None`, and so did the entry).
///
/// An engine-schema path always counts as changed: it is floored out of the
/// walk, so it never enters the fingerprint at all, and comparing against an
/// always-absent entry would read a removal as no change.
///
/// Takes its filesystem rather than reaching for [`RealFileSystem`], so the
/// predicate can be exercised over a [`au_parser::MemoryFileSystem`]. It decides
/// both whether a write is observable and whether a splice has settled, and
/// neither was reachable from a test while the disk was wired in.
fn path_changed(path: &Path, prior_fp: &Fingerprint, fs: &impl FileSystem) -> bool {
    if is_engine_schema_path(path) {
        return true;
    }
    if !crate::build::build_reads_content(path) {
        return prior_fp.contains_key(path) != fs.is_file(path);
    }
    let now = fs.read_file(path).ok().map(|bytes| ContentHash::of(&bytes));
    now != recorded_content(prior_fp, path)
}

/// The content the last build recorded for `path`, if it recorded any.
///
/// **This deliberately answers `None` for two different states, and the collapse
/// is load-bearing.** A [`Fingerprint`] distinguishes three: the path was absent
/// (no entry), the path was there but no content was recorded (`Some(None)`),
/// and the path was read (`Some(Some(h))`). [`fingerprint_diff`] and
/// [`updated_fingerprint`] all three, and MUST, since an asset moves only the
/// key set.
///
/// Here the first two are the same answer, because the caller has already
/// established that this is a path the build READS, and it is comparing against
/// a fresh read that failed. "It was not there" and "it was there and would not
/// read" both mean there is no content to differ from, and a read that fails now
/// means there is none to compare. So they agree, and saying so is what makes
/// the comparison converge.
///
/// Keeping them apart here would NOT be more precise, it would be wrong: a file
/// that failed to read at build and still fails would compare unequal on every
/// pass, rebuild, commit the identical state, and do it again on the next event.
/// A rebuild loop over a file nobody can read.
///
/// The subtlety is narrower than it looks, and worth stating exactly. Merely
/// un-flattening is SAFE: a failed read maps to `None`, which is what a
/// present-but-unread entry already holds, so the two still compare equal. What
/// breaks it is giving "present but unread" a value of its OWN, as an enum with
/// an `Unhashed` variant would, while a failed read keeps mapping to absence.
/// The correctness here rests on the two states being spelled the same, not on
/// the comparison's shape.
///
/// `an_unreadable_file_rebuilds_once_then_settles` is the guard, and it was
/// checked against that exact translation rather than assumed to cover it.
fn recorded_content(fp: &Fingerprint, path: &Path) -> Option<ContentHash> {
    fp.get(path).copied().flatten()
}

/// Which of `candidates` disk no longer agrees with `fp` about.
///
/// [`path_changed`] asked one path at a time and answered "is this write worth
/// rebuilding for". Asked over a set that was JUST spliced, and against the
/// fingerprint that splice committed, the same predicate answers a different
/// question: which of them moved again while the recompute was running.
///
/// So this is the settle check for [`Inner::splice_until_settled`], and the
/// reason `path_changed` takes its filesystem.
fn still_moving(
    candidates: &BTreeSet<PathBuf>,
    fp: &Fingerprint,
    fs: &impl FileSystem,
) -> BTreeSet<PathBuf> {
    candidates
        .iter()
        .filter(|path| path_changed(path, fp, fs))
        .cloned()
        .collect()
}

/// The fingerprint after a scoped rebuild: the prior one with each dirty path
/// updated to what a full [`fingerprint`] would record, or dropped when the path
/// left the catalog. Non-dirty entries are unchanged by assumption, so the
/// stored fingerprint stays consistent with the next full-rebuild no-op gate.
///
/// Keyed on CATALOG PRESENCE, never on the hash. A file the build does not read
/// is catalogued with no hash, so folding the two together would drop a present
/// asset from the fingerprint, and [`path_changed`] would then read its later
/// delete as "already gone" and never rebuild — leaving a deleted file in the
/// catalog and still resolving `file*` references to it.
fn updated_fingerprint(
    mut fp: Fingerprint,
    dirty: &BTreeSet<PathBuf>,
    kb: &KnowledgeBase,
) -> Fingerprint {
    for path in dirty {
        match kb.catalog.get(path) {
            Some(entry) => {
                fp.insert(path.clone(), entry.hash);
            }
            None => {
                fp.remove(path);
            }
        }
    }
    fp
}

/// Drop dirty paths the current scope excludes, so an edit to a scoped-out file
/// (or a floored `.git` path) does not re-enter the graph via the incremental
/// fast path. Engine-schema (`.arsumbris/...`) paths are always kept: a
/// `.auignore` or registry change must still reach `classify_dirty` and force
/// the full re-walk. The member root of each path comes from the held knowledge base's
/// repos, so this reads only the small `.auignore` files, it never re-walks.
fn scope_filter_dirty(
    kb: &KnowledgeBase,
    dirty: &BTreeSet<PathBuf>,
    fs: &impl FileSystem,
) -> BTreeSet<PathBuf> {
    let mut filters: BTreeMap<PathBuf, au_parser::WalkFilter> = BTreeMap::new();
    // A malformed `.auignore` load error is intentionally discarded here: the
    // full rebuild owns that diagnostic. The `.auignore` edit is an `.arsumbris`
    // path, so it survives this filter, forces a full re-walk, and that rebuild
    // re-emits `auignore-load-error` authoritatively. This sink is only the
    // scope decision for the incremental path, not a diagnostic surface.
    let mut sink: Vec<Diagnostic> = Vec::new();
    dirty
        .iter()
        .filter(|path| {
            // Engine-schema survives, so a `.auignore` / registry change forces
            // the full re-walk downstream.
            if path.components().any(|c| c.as_os_str() == ".arsumbris") {
                return true;
            }
            // `.git` is floored; the filter's matcher does not cover the floor.
            if path.components().any(|c| c.as_os_str() == ".git") {
                return false;
            }
            // Otherwise the enclosing member's scope decides. A path under no
            // repo is left in for the rebuild to handle.
            let Some(repo) = kb.repos.repo_of(path) else {
                return true;
            };
            let root = repo.root.clone();
            let filter = filters
                .entry(root.clone())
                .or_insert_with(|| crate::build::member_walk_filter(&root, fs, &mut sink).0);
            filter.keep_file(path)
        })
        .cloned()
        .collect()
}

/// The watcher thread's body: debounce a burst into one rebuild, and reconcile
/// on a timeout.
///
/// A free function taking its event source, rather than a closure inlined into
/// [`Engine::watch`], so a test can drive it with a channel it controls. This
/// loop IS the watcher policy, and the arming bug it used to carry survived
/// precisely because nothing could reach it.
fn watch_loop(
    inner: Arc<Inner>,
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    reconcile: Option<Duration>,
) {
    // One reconcile is due once the watcher goes idle after activity.
    let mut pending_reconcile = false;
    loop {
        // Wait for the next event. With a reconcile window, a timeout
        // reconciles: a whole-knowledge-base pass catching events the
        // backend silently dropped.
        //
        // PERIODIC, not armed by a preceding burst. A dropped event can
        // arrive at any moment, including with no delivered sibling and
        // after arbitrary quiet, and arming on a burst leaves exactly
        // that case uncaught forever: the timeout fires, nothing is
        // pending, and the change sits undetected until unrelated
        // activity happens to arm the next check.
        //
        // Which is the case the backstop exists for. "Nothing has
        // happened" is precisely what a dropped event makes unknowable,
        // so a backstop cannot condition itself on knowing it.
        //
        // The cost is one pass per window while idle, where it used to
        // be one per burst. That is real and it bounds how short the
        // window can be, so it is measured rather than assumed.
        let first = match reconcile {
            Some(window) => match rx.recv_timeout(window) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    if pending_reconcile {
                        inner.rebuild();
                        pending_reconcile = false;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(message) => message,
                // Channel closed means the watcher was dropped, exit.
                Err(_) => return,
            },
        };

        // Accumulate the burst until the knowledge base goes quiet for the
        // quiescence window. A rescan or error anywhere in the burst
        // forces a whole-knowledge-base rebuild.
        let mut dirty = BTreeSet::new();
        let mut full = accumulate_event(&mut dirty, first);
        loop {
            match rx.recv_timeout(QUIESCENCE) {
                Ok(message) => full |= accumulate_event(&mut dirty, message),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        // An attributable burst rebuilds only its dirty paths; a forced
        // or empty one takes the whole-knowledge-base path.
        if full || dirty.is_empty() {
            inner.rebuild_scoped(RebuildScope::Full);
        } else {
            inner.rebuild_scoped(RebuildScope::Paths(dirty));
        }
        // Armed by ACTIVITY, because activity is when a drop is possible.
        //
        // A pass per window regardless would also cover a change that produced
        // no event at all, but that is not what the backstop is for and the
        // trade is bad: continuous cost on every machine against a case whose
        // real cause is an UNWATCHED path rather than a lost event, and which
        // self-heals the moment anything else happens.
        pending_reconcile = reconcile.is_some();
    }
}

struct Held {
    engine_state: EngineState,
    ref_state: RefState,
    version: u64,
    // Held behind an `Arc` so a read snapshots it with an O(1) refcount bump,
    // then runs off the state lock. The knowledge base is immutable per version, so a
    // rebuild builds a fresh one and swaps the `Arc` rather than mutating in
    // place; outstanding readers keep serving the version they snapshotted.
    kb: Option<Arc<KnowledgeBase>>,
    fingerprint: Fingerprint,
}

struct Inner {
    /// The workspace directory, the knowledge base root for identity (socket, watcher,
    /// `.arsumbris/` markers) and the walk. Always the folder-repo entry
    /// directory; `build` refuses a non-repo entry.
    root: PathBuf,
    held: Mutex<Held>,
    /// Serializes rebuilds across threads, distinct from `held`. Every rebuild
    /// entry point holds it across the whole snapshot -> compute -> swap, so two
    /// rebuilds (the watcher thread and a mutation's `spawn_blocking` task) can
    /// never both snapshot the same base and clobber each other's swap. It does
    /// NOT wrap read work: reads snapshot `held` and run off it, so a long
    /// rebuild never blocks a read. Not reentrant, so the lock-acquiring
    /// wrappers ([`Inner::rebuild`], [`Inner::rebuild_paths`]) call the
    /// lock-free `*_locked` bodies, which never re-take it.
    rebuild_lock: Mutex<()>,
    /// Broadcasts the ref version on every advance: 0 while Deriving, then the
    /// current version. The serve layer's per-subscription tasks park on a
    /// receiver and wake on each rebuild. Sending is sync, callable from the
    /// watcher thread inside `rebuild`.
    version_tx: watch::Sender<u64>,
    /// Serializes the write pipeline. Every mutation and reconcile holds it
    /// across the whole write, so sagas are strictly sequential across all
    /// connections — the single-threaded model the intent marker and crash
    /// recovery depend on. Reads and subscriptions never take it.
    write_lock: tokio::sync::Mutex<()>,
    /// The device-global package-cache root, where the resolver materializes
    /// dependency snapshots and the build locates them. Defaults to
    /// [`crate::build::default_package_cache_root`]; a test injects a tempdir so
    /// the resolve verb stays hermetic. The build and the resolve verb read the
    /// same value, so they never disagree on where a snapshot lives.
    package_cache_root: Option<PathBuf>,
    /// The registry repo the resolve verb resolves name-only dependencies
    /// through. Defaults to [`crate::pkgcache::DEFAULT_REGISTRY_REMOTE`]; a test
    /// injects a local fixture registry so the verb stays hermetic. The
    /// designed workspace-level override plugs in here.
    registry_remote: String,
    /// The device root holding `au-engine/config/{repos,workspaces}.yaml`.
    /// `User` resolves the device root (`$HOME/.arsumbris`, then
    /// `au-engine/config/`); a test injects a tempdir so the
    /// `register` write and every registry read stay hermetic, off the
    /// developer's real `~/.arsumbris/au-engine/config`. The build, the resolve verb, and
    /// the `register` mutation read the same value, so they never disagree.
    config: crate::repo::ConfigSource,
    /// The on-demand git-reflog watchers backing `recent_commits` subscriptions,
    /// ref-counted per working tree. Armed only while a subscription watches a
    /// tree, off the version signal. See [`crate::gitwatch`].
    #[cfg(unix)]
    git_watch: crate::gitwatch::GitWatch,
}

impl Inner {
    /// The entry path `build` and `assembly_roots` receive: the folder-repo
    /// directory, which is the workspace root.
    fn entry(&self) -> &Path {
        &self.root
    }

    /// Rebuild if the knowledge base's content changed, level-triggered.
    ///
    /// Snapshots a content fingerprint; if it matches the held one (and a knowledge base
    /// is already held), nothing changed and this is a no-op, no version
    /// advance. Otherwise it does a full build outside the lock, then
    /// re-snapshots and commits only when the fingerprint held still across the
    /// whole build, so the stored fingerprint always describes the stored
    /// knowledge base. An edit landing mid-build moves the fingerprint and forces another
    /// pass, so no change is missed; a transient read error (a file mid-rename)
    /// is retried the same way rather than dropping the wake. The first commit
    /// transitions the engine to Up and the ref to Ready.
    ///
    /// Serialized against every other rebuild by the rebuild lock, so a
    /// concurrent scoped or incremental rebuild cannot swap a different knowledge base in
    /// mid-pass.
    fn rebuild(&self) {
        let _rebuild = self.rebuild_lock.lock().unwrap();
        self.rebuild_locked();
    }

    /// The full-rebuild body. The caller already holds the rebuild lock; this
    /// never re-takes it, so [`Inner::rebuild_paths_locked`] can fall through to
    /// it while holding the lock (the std mutex is not reentrant).
    #[tracing::instrument(skip_all)]
    fn rebuild_locked(&self) {
        // Bounds a pathological loop: edits faster than a build completes, or a
        // persistently failing build. Pending watch events re-drive `rebuild`
        // after this returns, so yielding a pass never strands a change.
        const MAX_PASSES: u32 = 8;

        for _ in 0..MAX_PASSES {
            let fp = match fingerprint(self.entry(), &self.config, &RealFileSystem) {
                Ok(fp) => fp,
                // A walk failure is transient (a directory mid-rename); retry.
                Err(_) => continue,
            };

            // WHICH paths changed, not merely whether any did. This pass is the
            // backstop for a silently-dropped watcher event, and it used to cost
            // a whole-knowledge-base rebuild even when one file had moved,
            // because the changed set was computed and discarded.
            //
            // Empty when there is nothing to diff against (the first build), so
            // the whole pass below runs, which is what establishes the state.
            let dirty: BTreeSet<PathBuf> = {
                let held = self.held.lock().unwrap();
                match held.kb.as_ref() {
                    Some(_) if held.fingerprint == fp => return,
                    // Scope-filter the fingerprint diff before the incremental
                    // splice below. `fingerprint` walks with the hard-floor
                    // excludes ONLY (build.rs), so it over-includes auignored
                    // files by design; without this filter `splice_until_settled`
                    // would splice a scoped-out file straight into the graph, the
                    // leak `rebuild_paths_locked`'s `scope_filter_dirty` prevents
                    // on the watcher/write path. An all-scoped-out diff falls
                    // through to the full build below, which excludes the file and
                    // refreshes the fingerprint.
                    Some(kb) => expand_removed_directories(
                        kb,
                        &scope_filter_dirty(
                            kb,
                            &fingerprint_diff(&held.fingerprint, &fp),
                            &RealFileSystem,
                        ),
                        &RealFileSystem,
                    ),
                    None => BTreeSet::new(),
                }
            };

            // Recover at the cost of the WRITE rather than of a rebuild, when
            // the change is one the fast path models. It classifies the set
            // itself and declines anything wider (a type-def, a manifest), so
            // this only ever narrows the work, never changes what is correct.
            if !dirty.is_empty() && self.splice_until_settled(dirty.clone(), &RealFileSystem) {
                return;
            }

            // Snapshot the prior parse layer under the lock, so the build reuses
            // every unchanged file's parse instead of re-parsing it. A
            // pointer-copy map, the parses are Arc-shared, so this is cheap.
            let prior = {
                let held = self.held.lock().unwrap();
                held.kb
                    .as_ref()
                    .map(|v| v.parse_layer())
                    .unwrap_or_default()
            };

            // Build outside the lock; reads keep serving the old knowledge base. Reusing
            // the prior parse layer bounds per-edit parse work to what changed,
            // the graph and validation still recompute whole-knowledge-base. The
            // fingerprint already hashed every file, so pass it as the known
            // hashes: an unchanged file reuses its parse without a second read.
            let kb = match build_reusing(
                self.entry(),
                &RealFileSystem,
                &crate::repo::load_user_registry(&self.config, &RealFileSystem),
                &prior,
                &fp,
                self.package_cache_root.as_deref(),
            ) {
                Ok(v) => v,
                // A read failure mid-build (a file mid-rename) is transient;
                // re-snapshot and retry instead of dropping this wake.
                Err(_) => continue,
            };

            // Commit only if disk held still across the build. Otherwise the
            // knowledge base may mix pre- and post-edit content that `fp` does not
            // describe; rebuild against the newer state instead.
            match fingerprint(self.entry(), &self.config, &RealFileSystem) {
                Ok(after) if after == fp => {}
                _ => continue,
            }

            // The commit is the moment a subscriber can observe the change, so
            // it is the end of write-to-event latency. Spanned to expose a
            // stall waiting on the state lock, which is otherwise invisible.
            let _commit = tracing::info_span!("commit").entered();
            let mut held = self.held.lock().unwrap();
            held.kb = Some(Arc::new(kb));
            held.fingerprint = fp;
            held.version += 1;
            held.engine_state = EngineState::Up;
            held.ref_state = RefState::Ready;
            let version = held.version;
            drop(held);

            // Wake every subscription waiting on a version change. Outside the
            // held lock; `watch::Sender::send` is sync and never blocks on it.
            let _ = self.version_tx.send(version);
            return;
        }
    }

    /// Rebuild over a scope: whole-knowledge-base, or only a dirty set.
    fn rebuild_scoped(&self, scope: RebuildScope) {
        match scope {
            RebuildScope::Full => self.rebuild(),
            RebuildScope::Paths(dirty) => self.rebuild_paths(&dirty),
        }
    }

    /// Splice `dirty`, then re-splice whatever moved AGAIN while the recompute
    /// was running, until the set settles. `false` hands the work to the whole
    /// rebuild, as [`Inner::try_incremental_fast_path`] does.
    ///
    /// The reconcile's confirm pass, at the reconcile's grain. The whole-rebuild
    /// branch below confirms by re-fingerprinting the WORKSPACE and retrying the
    /// outer loop; wiring this the same way, with a `continue`, would make the
    /// reconcile pay two whole-workspace walks. Not paying for that second walk
    /// is the entire reason diffing the fingerprint is affordable, so the confirm
    /// is scoped to the paths that were actually spliced.
    ///
    /// The splice needs no confirm for CONSISTENCY: it reads each dirty path
    /// itself and [`updated_fingerprint`] records what it read, so the committed
    /// fingerprint describes the committed knowledge base by construction. What
    /// the re-check buys is CURRENCY — that the committed state is not already
    /// behind disk when the pass returns.
    ///
    /// **It does not restore the whole-rebuild branch's guarantee, and is not
    /// meant to.** A path OUTSIDE the dirty set that changed after the
    /// fingerprint walk had already visited it is invisible here by
    /// construction: it is not in the diff, so nothing re-reads it. The next
    /// reconcile catches it, and one is armed by any later event; it persists
    /// only if that write's own event never arrived either, which is the
    /// silent-drop class 44 measured rows failed to reproduce.
    ///
    /// **A NARROWING of the whole-rebuild branch's defence-in-depth, not an
    /// equivalent of it.** That branch's second `fingerprint` was an
    /// independent net: it caught a write landing during the walk even when
    /// that write's own event was lost. Scoping the confirm to the spliced set
    /// gives up that coverage for the walk interval. Small, and judged worth a
    /// whole-workspace walk on every reconcile, but a real loss rather than a
    /// no-op.
    ///
    /// `fs` is the SETTLE PROBE's filesystem, and ONLY that. The splice reads
    /// the real disk whatever is passed here, so a test driving this over a
    /// double is exercising the retry decision, never the recompute. Do not
    /// read the parameter as making the whole path injectable.
    ///
    /// `settled` reports whether the set stopped moving before the bound. A
    /// `false` means the commit is behind disk, so a workspace under sustained
    /// write that keeps landing behind is READABLE rather than inferred from
    /// latency, the same reason `watch_event` exists.
    #[tracing::instrument(
        skip_all,
        fields(
            dirty = dirty.len(),
            passes = tracing::field::Empty,
            settled = tracing::field::Empty
        )
    )]
    fn splice_until_settled(&self, dirty: BTreeSet<PathBuf>, fs: &impl FileSystem) -> bool {
        // Bounded, so a file under continuous rewrite cannot spin here. Giving
        // up leaves the splice committed and self-consistent, merely behind
        // disk, which is the state any missed event produces and which the next
        // pass absorbs. It errs toward re-reporting a change, never toward
        // under-reporting one, so it cannot wedge.
        //
        // TIGHTER than `rebuild_locked`'s `MAX_PASSES` of 8, deliberately. That
        // bound guards a loop whose every pass is a whole-workspace fingerprint
        // plus a whole rebuild, so it is worth several attempts before yielding.
        // Each pass here is O(dirty) reads over a set that has already failed to
        // settle twice; a third and fourth attempt is generous, and a workspace
        // rewriting the same paths that fast is not going to settle at eight
        // either.
        const MAX_SETTLE_PASSES: u32 = 4;

        let mut pending = dirty;
        let mut handled = false;
        let mut passes = 0;
        for _ in 0..MAX_SETTLE_PASSES {
            if !self.try_incremental_fast_path(&pending) {
                // Wider than the splice. Anything already committed stays
                // committed and correct; the whole rebuild subsumes it.
                return false;
            }
            handled = true;
            passes += 1;
            let fp = self.held.lock().unwrap().fingerprint.clone();
            pending = still_moving(&pending, &fp, fs);
            if pending.is_empty() {
                break;
            }
        }
        let span = tracing::Span::current();
        span.record("passes", passes);
        span.record("settled", pending.is_empty());
        handled
    }

    /// The incremental fast path: when every dirty path is an instance edit or
    /// add, splice its blast radius into the held knowledge base instead of recomputing
    /// the whole resolved layer. Returns `true` when it handled the rebuild
    /// (committed, or a no-op), `false` to fall through to the scoped whole-knowledge-base
    /// rebuild.
    ///
    /// The recompute runs OFF the state lock. The state lock is taken only twice,
    /// each briefly: once to snapshot the knowledge base (an O(1) `Arc` clone) and
    /// fingerprint, once to swap the freshly-patched knowledge base in. So the dirty-set
    /// disk reads (the no-op gate and `recompute_dirty`) and the graph patch never
    /// block a tokio-side read, even for a bulk burst of instance edits.
    ///
    /// The snapshot is still current at swap time because the caller holds the
    /// rebuild lock ([`Inner::rebuild_lock`]) across the whole pass, so no other
    /// rebuild can swap between this snapshot and this swap. Only ever called
    /// while that lock is held, from [`Inner::rebuild_paths_locked`] and from
    /// [`Inner::splice_until_settled`]. The clone target is an `Arc` share, see
    /// [`crate::incremental::apply_recompute`].
    ///
    /// `ret` records whether the fast path HANDLED the rebuild. A `false` means
    /// the operation fell through to a whole-knowledge-base build, which is the
    /// single most cost-shaping fact about any write, so it belongs in the trace
    /// rather than being inferred from what ran afterwards.

    #[tracing::instrument(skip_all, fields(dirty = dirty.len()), ret)]
    fn try_incremental_fast_path(&self, dirty: &BTreeSet<PathBuf>) -> bool {
        // Snapshot under the lock, then compute off it.
        let (kb, prior_fp) = {
            let held = self.held.lock().unwrap();
            match held.kb.as_ref() {
                Some(kb) => (Arc::clone(kb), held.fingerprint.clone()),
                None => return false, // first build, the full rebuild establishes the knowledge base
            }
        };

        if classify_dirty(&kb, dirty) != DirtyClass::Incremental {
            return false; // a type-def, registry, or manifest is wider
        }
        // Scoped no-op gate: an identical write, a touch, or a content edit to a
        // file the build never reads changes nothing. This runs BEFORE the
        // recompute, which is what keeps an unobservable write from advancing
        // the version even though the recompute would happily produce an
        // identical knowledge base for it.
        if !dirty
            .iter()
            .any(|path| path_changed(path, &prior_fp, &RealFileSystem))
        {
            return true; // nothing to do, and no version advance
        }
        let Some(recompute) = recompute_dirty(&kb, dirty, &RealFileSystem) else {
            return false; // wider than this stage; the full rebuild handles it
        };
        let next = apply_recompute(&kb, recompute);
        let new_fp = updated_fingerprint(prior_fp, dirty, &next);

        // Re-take the lock only to swap in the patched knowledge base.
        let _commit = tracing::info_span!("commit").entered();
        let version = {
            let mut held = self.held.lock().unwrap();
            held.kb = Some(Arc::new(next));
            held.fingerprint = new_fp;
            held.version += 1;
            held.engine_state = EngineState::Up;
            held.ref_state = RefState::Ready;
            held.version
        };
        let _ = self.version_tx.send(version);
        true
    }

    /// Rebuild reusing the prior parse layer, reading only the `dirty` paths.
    ///
    /// The dirty set is authoritative for what changed: every other walked file
    /// is assumed unchanged and reuses its prior parse with no read. Adds and
    /// deletes still fall out of the build's walk. There is no whole-knowledge-base
    /// fingerprint, so this pays O(dirty) reads, not O(knowledge base).
    ///
    /// Falls back to a full [`Inner::rebuild`] when nothing is held yet (the
    /// first build) or a read fails mid-build, both of which a full pass handles
    /// and re-establishes a consistent fingerprint for.
    ///
    /// Serialized against every other rebuild by the rebuild lock, so a
    /// concurrent rebuild cannot snapshot the same base and clobber this swap.
    fn rebuild_paths(&self, incoming: &BTreeSet<PathBuf>) {
        let _rebuild = self.rebuild_lock.lock().unwrap();
        self.rebuild_paths_locked(incoming);
    }

    /// The scoped-rebuild body. The caller holds the rebuild lock; this and its
    /// `try_incremental_fast_path` / `rebuild_locked` fall-throughs never re-take
    /// it (the std mutex is not reentrant).
    #[tracing::instrument(skip_all, fields(incoming = incoming.len()))]
    fn rebuild_paths_locked(&self, incoming: &BTreeSet<PathBuf>) {
        // Scope-filter first: an edit to a scoped-out file (or a floored path)
        // must not re-enter the graph via the incremental fast path.
        // Engine-schema (`.arsumbris/...`) paths survive to force a re-walk.
        let _scope = tracing::info_span!("scope_filter").entered();
        let dirty: BTreeSet<PathBuf> = {
            let held = self.held.lock().unwrap();
            match held.kb.as_ref() {
                // Then expand a removed DIRECTORY into the files that were under
                // it. Here rather than inside the fast path, because BOTH this
                // function's branches have to see the expansion: when the fast
                // path declines (a type-def under the removed folder), the
                // no-op gate below would otherwise ask about the directory,
                // find it unchanged, and return without rebuilding at all.
                Some(kb) => expand_removed_directories(
                    kb,
                    &scope_filter_dirty(kb, incoming, &RealFileSystem),
                    &RealFileSystem,
                ),
                // First build has no scope yet; the full build applies it.
                None => incoming.clone(),
            }
        };
        drop(_scope);
        // Everything was out of scope (a floored or excluded change): nothing to
        // rebuild, no version advance.
        if dirty.is_empty() {
            return;
        }
        // Try the incremental fast path first; anything wider falls through to
        // the scoped whole-knowledge-base rebuild below.
        if self.try_incremental_fast_path(&dirty) {
            return;
        }

        let (prior, prior_fp) = {
            let held = self.held.lock().unwrap();
            match held.kb.as_ref() {
                Some(kb) => (kb.parse_layer(), held.fingerprint.clone()),
                None => {
                    drop(held);
                    return self.rebuild_locked();
                }
            }
        };

        // Scoped no-op gate: if no dirty path actually changed (an identical
        // write, a touch, a content edit to a file the build never reads), there
        // is nothing to rebuild and no version advance. See [`path_changed`].
        if !dirty
            .iter()
            .any(|path| path_changed(path, &prior_fp, &RealFileSystem))
        {
            return;
        }

        // Known hashes for the build: the prior fingerprint minus the dirty
        // paths, so each dirty path is read fresh and every other file reuses
        // its parse without a read.
        let mut known = prior_fp.clone();
        for path in &dirty {
            known.remove(path);
        }

        let kb = match build_reusing(
            self.entry(),
            &RealFileSystem,
            &crate::repo::load_user_registry(&self.config, &RealFileSystem),
            &prior,
            &known,
            self.package_cache_root.as_deref(),
        ) {
            Ok(kb) => kb,
            // A read failure mid-build is transient; a full rebuild re-snapshots
            // and retries, and restores a consistent whole-knowledge-base fingerprint.
            Err(_) => return self.rebuild_locked(),
        };

        let new_fp = updated_fingerprint(prior_fp, &dirty, &kb);

        let _commit = tracing::info_span!("commit").entered();
        let mut held = self.held.lock().unwrap();
        held.kb = Some(Arc::new(kb));
        held.fingerprint = new_fp;
        held.version += 1;
        held.engine_state = EngineState::Up;
        held.ref_state = RefState::Ready;
        let version = held.version;
        drop(held);
        let _ = self.version_tx.send(version);
    }

    fn read<T>(&self, f: impl FnOnce(&KnowledgeBase) -> T) -> Read<T> {
        // Snapshot the versioned knowledge base under the lock, an O(1) `Arc` clone, then
        // run the read OFF the lock. The knowledge base is immutable per version, so the
        // snapshot stays coherent while other reads and the rebuild swap
        // proceed concurrently. The state lock is never held across read work.
        let (version, kb) = {
            let held = self.held.lock().unwrap();
            match (&held.ref_state, &held.kb) {
                (RefState::Ready, Some(kb)) => (held.version, Arc::clone(kb)),
                _ => return Read::NotReady,
            }
        };
        Read::Ready {
            version,
            value: f(&kb),
        }
    }

    /// Simulate a deterministic mutation over an overlay of the held snapshot and
    /// hand the outcome to `f`, without writing disk, committing, or swapping any
    /// state. The read half of the write path: the product's type and diagnostics
    /// come from the same recompute a real mutation's rebuild runs, so a preview
    /// cannot disagree with what the write would land. See
    /// [[spec - mutation preview read - simulate a write over an overlay and
    /// report its product without committing]].
    ///
    /// Lock-free like [`Inner::read`]: it snapshots the held knowledge base under
    /// the state lock, then computes off it. It never takes the rebuild or write
    /// lock, so a preview never blocks a real write and the throwaway knowledge
    /// base it builds is dropped when this returns.
    fn preview<T>(&self, input: PreviewInput, f: impl FnOnce(PreviewOutcome) -> T) -> Read<T> {
        // Snapshot the held knowledge base and its fingerprint, an O(1) Arc clone,
        // then compute off the lock, exactly as `read` does.
        let (version, held, prior_fp) = {
            let guard = self.held.lock().unwrap();
            match (&guard.ref_state, &guard.kb) {
                (RefState::Ready, Some(kb)) => {
                    (guard.version, Arc::clone(kb), guard.fingerprint.clone())
                }
                _ => return Read::NotReady,
            }
        };
        let ready = |value| Read::Ready { version, value };

        // Compute the would-be content, reading the target's current bytes for an
        // edit or delete (the same read the real mutation does). A structural
        // refusal short-circuits to a reject outcome, no product.
        let current = RealFileSystem.read_file(&input.target).ok();
        let would_be = match crate::mutate::preview_content(&input.op, current.as_deref()) {
            Ok(c) => c,
            Err(reject) => return ready(f(PreviewOutcome::Reject(reject))),
        };
        // Fold any stamps into a write or edit product; a delete carries none.
        let would_be = match would_be {
            Some(content) => {
                match crate::mutate::fold_stamps_content(content, &input.target, &input.stamps) {
                    Ok(c) => Some(c),
                    Err(reject) => return ready(f(PreviewOutcome::Reject(reject))),
                }
            }
            None => None,
        };

        // Overlay the snapshot with the one change, then run the engine's own
        // rebuild dispatch over it: the incremental fast path for an instance
        // change, the scoped whole-knowledge-base build for a type-def or
        // engine-schema change. Neither swaps into `held`.
        let over = match &would_be {
            Some(content) => Override::Replace {
                path: input.target.clone(),
                content: content.clone().into_bytes(),
            },
            None => Override::Remove {
                path: input.target.clone(),
            },
        };
        let overlay = OverlayFileSystem::new(&RealFileSystem, over);

        // Scope-filter before the recompute, exactly as `rebuild_paths_locked`
        // does: an out-of-scope (auignored) or floored target must not re-enter
        // the graph via the preview's recompute, or the preview would diagnose a
        // file the committed write leaves absent. The overlay is the FS, so the
        // scope decision reflects the would-be state, not disk.
        let dirty: BTreeSet<PathBuf> = std::iter::once(input.target.clone()).collect();
        let dirty = expand_removed_directories(
            &held,
            &scope_filter_dirty(&held, &dirty, &overlay),
            &overlay,
        );

        // The target scoped out to nothing: a committed write lands the bytes on
        // disk but the file never enters the graph, so it contributes zero
        // diagnostics and no blast radius. `held` already excludes it, so it is
        // the correct product, the parity of `rebuild_paths_locked`'s empty-dirty
        // early return.
        if dirty.is_empty() {
            let value = f(PreviewOutcome::Built {
                preview: &held,
                held: &held,
                target: &input.target,
            });
            return ready(value);
        }

        let preview_kb = if classify_dirty(&held, &dirty) == DirtyClass::Incremental {
            match recompute_dirty(&held, &dirty, &overlay) {
                Some(rc) => apply_recompute(&held, rc),
                // Wider than the fast path (an add the edge-inversion cannot
                // model): fall through to the scoped build, like a real rebuild.
                None => match self.preview_build(&overlay, &held, &prior_fp, &dirty) {
                    Ok(kb) => kb,
                    Err(_) => return Read::NotReady,
                },
            }
        } else {
            match self.preview_build(&overlay, &held, &prior_fp, &dirty) {
                Ok(kb) => kb,
                Err(_) => return Read::NotReady,
            }
        };

        let value = f(PreviewOutcome::Built {
            preview: &preview_kb,
            held: &held,
            target: &input.target,
        });
        ready(value)
        // `preview_kb` drops here: the throwaway state, never swapped in.
    }

    /// The scoped whole-knowledge-base build for a preview, over the overlay,
    /// reusing the held parse layer. Mirrors `rebuild_paths_locked`'s build arm:
    /// every dirty path reads fresh through the overlay, every other file reuses
    /// its parse with no read.
    fn preview_build(
        &self,
        overlay: &impl FileSystem,
        held: &KnowledgeBase,
        prior_fp: &Fingerprint,
        dirty: &BTreeSet<PathBuf>,
    ) -> Result<KnowledgeBase, crate::build::BuildError> {
        let mut known = prior_fp.clone();
        for path in dirty {
            known.remove(path);
        }
        build_reusing(
            self.entry(),
            overlay,
            &crate::repo::load_user_registry(&self.config, &RealFileSystem),
            &held.parse_layer(),
            &known,
            self.package_cache_root.as_deref(),
        )
    }
}

/// The op a preview simulates, resolved to its target path plus any stamps.
pub(crate) struct PreviewInput {
    pub target: PathBuf,
    pub op: PreviewOp,
    pub stamps: Vec<PreviewStamp>,
}

/// The result handed to a preview's read-off closure: a structural reject, or the
/// built throwaway knowledge base plus the held one (for the blast-radius diff)
/// and the resolved target path.
pub(crate) enum PreviewOutcome<'a> {
    /// The op refused before any product existed (an edit or delete of an absent
    /// file, an absent or non-unique `old_string`, a stamp on a non-list field).
    Reject(MutationReject),
    /// The would-be product, ready to read off.
    Built {
        preview: &'a KnowledgeBase,
        held: &'a KnowledgeBase,
        target: &'a Path,
    },
}

/// A cloneable, thread-safe read handle onto an engine's held state.
///
/// Shares the engine's inner state without the watcher, so it is `Send + Sync`
/// and can be handed to a serving thread. Reads observe whatever version is
/// current; rebuilds driven by the owning [`Engine`] are visible through it.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Inner>,
}

impl EngineHandle {
    /// Read against the held knowledge base, coherent at one version. Not-ready while
    /// the ref is Deriving.
    pub fn read<T>(&self, f: impl FnOnce(&KnowledgeBase) -> T) -> Read<T> {
        self.inner.read(f)
    }

    /// The current version, `None` while Deriving.
    pub fn version(&self) -> Option<u64> {
        let held = self.inner.held.lock().unwrap();
        match held.ref_state {
            RefState::Ready => Some(held.version),
            RefState::Deriving => None,
        }
    }

    /// The write-serialization lock, see [`Inner::write_lock`]. The serve layer's
    /// mutation and reconcile handlers hold it across the whole write, so writes
    /// are strictly sequential across connections.
    pub(crate) fn write_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.inner.write_lock
    }

    /// The current lifecycle: engine state and the ref's readiness.
    pub fn lifecycle(&self) -> (EngineState, RefState) {
        let held = self.inner.held.lock().unwrap();
        (held.engine_state, held.ref_state)
    }

    /// A receiver that observes the ref version, waking on every advance. The
    /// initial value is the current version, 0 while Deriving. The serve layer's
    /// per-subscription tasks await changes on it.
    pub fn subscribe_version(&self) -> watch::Receiver<u64> {
        self.inner.version_tx.subscribe()
    }

    /// A receiver of the working-tree roots whose reflogs move, the
    /// `recent_commits` subscription's liveness source (distinct from the
    /// version signal). A tree only produces events while [`arm_git_watch`] holds
    /// interest in it. See [`crate::gitwatch`].
    ///
    /// [`arm_git_watch`]: EngineHandle::arm_git_watch
    #[cfg(unix)]
    pub(crate) fn subscribe_git_events(&self) -> tokio::sync::broadcast::Receiver<PathBuf> {
        self.inner.git_watch.subscribe()
    }

    /// Start watching each tree's reflog for a subscription, ref-counted. Pairs
    /// with [`disarm_git_watch`]. Runs a `git` subprocess and a `notify`
    /// registration per newly-watched tree, so a caller runs it off the reactor.
    ///
    /// Returns the subset actually armed, which the caller disarms (never the
    /// full requested set: a skipped tree must not be decremented).
    ///
    /// [`disarm_git_watch`]: EngineHandle::disarm_git_watch
    #[cfg(unix)]
    pub(crate) fn arm_git_watch(&self, trees: &[PathBuf]) -> Vec<PathBuf> {
        self.inner.git_watch.arm(trees)
    }

    /// Drop a subscription's interest in each tree, stopping a watcher at zero.
    #[cfg(unix)]
    pub(crate) fn disarm_git_watch(&self, trees: &[PathBuf]) {
        self.inner.git_watch.disarm(trees);
    }

    /// The knowledge base root (always a directory).
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The entry path for `assembly_roots`: the folder-repo directory, which is
    /// the workspace root.
    pub fn entry(&self) -> &Path {
        self.inner.entry()
    }

    /// The package-cache root the build and the resolve verb share. `None` when
    /// the home directory is unknown and no override was set. The resolve handler
    /// materializes snapshots here, the same root the build locates them from.
    pub(crate) fn package_cache_root(&self) -> Option<&Path> {
        self.inner.package_cache_root.as_deref()
    }

    /// The registry remote the resolve verb resolves name-only dependencies
    /// through, the engine default or a test/workspace override.
    pub(crate) fn registry_remote(&self) -> &str {
        &self.inner.registry_remote
    }

    /// Where the per-user config (`repos.yaml` / `workspaces.yaml`) is read
    /// from. The `register` mutation writes here and the resolve verb reads
    /// here, the same value the build uses.
    pub(crate) fn config(&self) -> &crate::repo::ConfigSource {
        &self.inner.config
    }

    /// Rebuild after a mutation, reading only the written paths — the mutation
    /// channel's synchronous step after its write, so the response carries fresh
    /// state. The channel knows exactly what it changed, so the dirty set is
    /// exact and the rebuild reads O(dirty), not the whole knowledge base. Crate-private:
    /// consumers mutate through the channel, never drive rebuilds.
    pub(crate) fn rebuild_paths(&self, dirty: BTreeSet<PathBuf>) {
        self.inner.rebuild_paths(&dirty);
    }

    /// Simulate a deterministic mutation and hand its outcome to `f`, without
    /// writing disk or committing. The read-only pre-tool gate surface: a
    /// consumer previews a pending write to know its product (type identity plus
    /// diagnostics) before it lands. See [`Inner::preview`].
    pub(crate) fn preview<T>(
        &self,
        input: PreviewInput,
        f: impl FnOnce(PreviewOutcome) -> T,
    ) -> Read<T> {
        self.inner.preview(input, f)
    }

    /// A full whole-knowledge-base rebuild. Used by the `register` config mutation: a
    /// per-user registry write changes resolution globally (any member may now
    /// mount or move), so a scoped path-rebuild would not suffice. Crate-private,
    /// like `rebuild_paths`.
    pub(crate) fn rebuild(&self) {
        self.inner.rebuild();
    }
}

/// A continuously-held analysis of one knowledge base.
pub struct Engine {
    inner: Arc<Inner>,
    /// The watcher is kept alive for as long as the engine; dropping it stops
    /// watching.
    watcher: Option<notify::RecommendedWatcher>,
    /// The watcher's drop-safety reconcile, read by [`Engine::watch`].
    reconcile: ReconcilePolicy,
}

impl Engine {
    /// Create an engine over a knowledge base root, before any build. The engine is
    /// Starting and the ref is Deriving; reads return not-ready until the first
    /// [`Engine::rebuild`].
    ///
    /// The reconcile policy defaults from the environment ([`ReconcilePolicy::from_env`]);
    /// override it with [`Engine::set_reconcile_policy`] before [`Engine::watch`].
    ///
    /// `config` is STATED, never defaulted: the daemon passes
    /// [`ConfigSource::User`](crate::repo::ConfigSource::User), a test passes
    /// `Empty` (hermetic) or `Dir` (an injected tempdir). It was formerly an
    /// `Option<PathBuf>` defaulting to `None`, which meant the developer's real
    /// `~/.arsumbris/au-engine/config` — so every test that never called the old
    /// `set_config_dir` read the machine's own registry, and a fixture naming a
    /// dep that happened to be registered there would resolve against a real
    /// repo on disk. Making the caller choose removes the silent default.
    pub fn new(root: impl Into<PathBuf>, config: crate::repo::ConfigSource) -> Self {
        let (version_tx, _) = watch::channel(0);
        // The entry is always the folder-repo DIRECTORY: the knowledge base root for
        // identity (socket, watcher, `.arsumbris/` markers) and the walk. The
        // `build` refuses a non-repo entry.
        //
        // Held in ONE spelling, the same normalization the assembly applies to
        // every member root, because this root is compared against catalog paths
        // and the two must agree. `au daemon` already canonicalizes its entry
        // before constructing the engine, so this aligns the library with how the
        // binary has always called it. See [`crate::repo::canonical_root`].
        let root = crate::repo::canonical_root(&root.into());
        Engine {
            inner: Arc::new(Inner {
                root,
                held: Mutex::new(Held {
                    engine_state: EngineState::Starting,
                    ref_state: RefState::Deriving,
                    version: 0,
                    kb: None,
                    fingerprint: Fingerprint::new(),
                }),
                rebuild_lock: Mutex::new(()),
                version_tx,
                write_lock: tokio::sync::Mutex::new(()),
                package_cache_root: crate::build::default_package_cache_root(),
                registry_remote: crate::pkgcache::DEFAULT_REGISTRY_REMOTE.to_string(),
                config,
                #[cfg(unix)]
                git_watch: crate::gitwatch::GitWatch::new(),
            }),
            watcher: None,
            reconcile: ReconcilePolicy::from_env(),
        }
    }

    /// Override the package-cache root, before any build or serve. The build and
    /// the resolve verb both read it, so they agree on where snapshots live. A
    /// test injects a tempdir to keep the resolve verb hermetic, off the real
    /// `~/.arsumbris` cache.
    pub fn set_package_cache_root(&mut self, root: impl Into<PathBuf>) {
        // The engine holds no other reference before `serve`/`watch`, so the
        // `Arc` is uniquely owned here; tests call this immediately after `new`.
        Arc::get_mut(&mut self.inner)
            .expect("set_package_cache_root before the handle is shared")
            .package_cache_root = Some(root.into());
    }

    /// Override the registry remote the resolve verb resolves name-only
    /// dependencies through, before any build or serve. A test injects a local
    /// fixture registry to keep the verb hermetic, off the real org registry.
    pub fn set_registry_remote(&mut self, remote: impl Into<String>) {
        Arc::get_mut(&mut self.inner)
            .expect("set_registry_remote before the handle is shared")
            .registry_remote = remote.into();
    }

    /// Set the watcher's reconcile policy. Takes effect on the next
    /// [`Engine::watch`]; a watcher already running keeps the policy it started
    /// with.
    pub fn set_reconcile_policy(&mut self, policy: ReconcilePolicy) {
        self.reconcile = policy;
    }

    /// Rebuild now if the knowledge base changed. Drives the same logic the watcher
    /// uses; a harness can call it directly for deterministic stepping.
    pub fn rebuild(&self) {
        self.inner.rebuild();
    }

    /// Start watching the knowledge base and rebuilding on change. Edits within a
    /// quiescence window coalesce into one rebuild. Idempotent-ish: calling it
    /// again replaces the watcher.
    ///
    /// LIMITATION: the watched member set is computed ONCE here, at start. A
    /// runtime change to the resolved member set — a `register`, a manifest edit,
    /// or a new `.arsumbris/repo.yaml` that adds a SCATTERED (out-of-tree) member
    /// after startup — does NOT re-arm the watch. Such a member's current content
    /// is ingested on the triggering rebuild, but its subtree is never watched, so
    /// subsequent edits inside it produce no events and no rebuild until the daemon
    /// restarts. Co-present in-tree members are unaffected (they ride the entry
    /// directory's recursive watch). Re-arming needs replacing the drop-based
    /// watcher shutdown so the watcher can be shared into the watch thread.
    /// A consumer that adds a scattered member at runtime should restart the daemon
    /// (or full-rebuild-then-restart) for its edits to stay live.
    pub fn watch(&mut self) -> notify::Result<()> {
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        // Always watch the entry directory: the entry repo's root, holding its
        // `.arsumbris/` (the manifest and lock) and any co-present members.
        watcher.watch(&self.inner.root, RecursiveMode::Recursive)?;
        // Also watch each scattered member tree, the ones that
        // sit outside the entry directory; co-present members are already
        // covered by the recursive watch above. Best-effort, a member path that
        // does not exist is simply not watched.
        let (roots, _manifest) = crate::build::assembly_roots(
            self.inner.entry(),
            &crate::repo::load_user_registry(&self.inner.config, &RealFileSystem),
            &RealFileSystem,
        );
        for root in &roots {
            if !root.starts_with(&self.inner.root) {
                let _ = watcher.watch(root, RecursiveMode::Recursive);
            }
        }

        let inner = Arc::clone(&self.inner);
        let reconcile = self.reconcile.window();
        std::thread::spawn(move || watch_loop(inner, rx, reconcile));

        self.watcher = Some(watcher);
        Ok(())
    }

    /// The current lifecycle: engine state and the ref's readiness.
    pub fn lifecycle(&self) -> (EngineState, RefState) {
        let held = self.inner.held.lock().unwrap();
        (held.engine_state, held.ref_state)
    }

    /// The current version, `None` while the ref is still Deriving.
    pub fn version(&self) -> Option<u64> {
        let held = self.inner.held.lock().unwrap();
        match held.ref_state {
            RefState::Ready => Some(held.version),
            RefState::Deriving => None,
        }
    }

    /// Read against the held knowledge base, coherent at one version. Returns not-ready
    /// while the ref is Deriving.
    pub fn read<T>(&self, f: impl FnOnce(&KnowledgeBase) -> T) -> Read<T> {
        self.inner.read(f)
    }

    /// The whole-knowledge-base diagnostics, stamped with the version.
    pub fn read_diagnostics(&self) -> Read<Vec<Diagnostic>> {
        self.read(|v| v.diagnostics().cloned().collect())
    }

    /// The knowledge base root this engine watches (always a directory).
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// A thread-safe read handle onto this engine's held state, for serving
    /// reads from another thread.
    pub fn handle(&self) -> EngineHandle {
        EngineHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Make a tempdir a folder-repo: the entry MUST carry `.arsumbris/repo.yaml`.
    fn folder_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".arsumbris")).unwrap();
        fs::write(dir.join(".arsumbris/repo.yaml"), "name: v\n").unwrap();
    }

    fn kb() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        folder_repo(dir.path());
        fs::write(dir.path().join("a.md"), "---\ntype: note\n---\n").unwrap();
        dir
    }

    /// A two-instance knowledge base, so a scoped rebuild has an unchanged file to reuse
    /// while one is dirty.
    fn two_file_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        folder_repo(dir.path());
        fs::write(dir.path().join("a.md"), "---\ntype: note\n---\n").unwrap();
        fs::write(dir.path().join("b.md"), "---\ntype: note\n---\n").unwrap();
        dir
    }

    /// A watcher-shaped dirty set: paths under the CANONICAL root, which is what
    /// the watcher delivers now that the assembly holds one spelling per member
    /// root. Building them from the raw tempdir path would simulate an event the
    /// watcher cannot produce, and on macOS `/var` is a symlink to `/private/var`,
    /// so the difference is live on every run here.
    fn dirty(dir: &tempfile::TempDir, name: &str) -> BTreeSet<PathBuf> {
        std::iter::once(crate::repo::canonical_root(dir.path()).join(name)).collect()
    }

    #[test]
    fn scope_filter_drops_excluded_keeps_schema_and_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::write(root.join("keep.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/guide.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join(".arsumbris")).unwrap();
        fs::write(root.join(".arsumbris/.auignore"), "docs/\n").unwrap();
        let kb = crate::build::build(root, &RealFileSystem).unwrap();

        let dirty: BTreeSet<PathBuf> = [
            root.join("keep.md"),
            root.join("docs/guide.md"),        // excluded by .auignore
            root.join(".arsumbris/.auignore"), // engine-schema, forces re-walk
            root.join(".git/HEAD"),            // floored
        ]
        .into_iter()
        .collect();

        let filtered = scope_filter_dirty(&kb, &dirty, &RealFileSystem);
        assert!(filtered.contains(&root.join("keep.md")), "in-scope kept");
        assert!(
            !filtered.contains(&root.join("docs/guide.md")),
            "excluded file dropped"
        );
        assert!(
            filtered.contains(&root.join(".arsumbris/.auignore")),
            "engine-schema kept to force the re-walk"
        );
        assert!(
            !filtered.contains(&root.join(".git/HEAD")),
            "floored path dropped"
        );
    }

    /// The reconcile / full-rebuild path (`rebuild_locked`) must scope-filter the
    /// fingerprint diff before its incremental splice. The fingerprint walks with
    /// the hard-floor excludes ONLY, so it over-includes auignored files; without
    /// the filter the splice lands a scoped-out file (and its diagnostics) in the
    /// graph, the parity of `rebuild_paths_locked`'s scope filter.
    #[test]
    fn full_rebuild_does_not_splice_an_auignored_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::repo::canonical_root(dir.path());
        folder_repo(&root);
        fs::write(root.join("keep.md"), "---\ntype: note\n---\n").unwrap();
        fs::write(root.join(".arsumbris/.auignore"), "docs/\n").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();

        let engine = Engine::new(&root, crate::repo::ConfigSource::Empty);
        engine.rebuild();

        // Create an invalid-YAML instance under the excluded dir, then drive a
        // full rebuild (the reconcile / `register` path). It diffs the floor-only
        // fingerprint, which now includes the new file, and would splice it via
        // the incremental fast path before the scope-filtered full build runs.
        let leak = root.join("docs/leak.md");
        fs::write(&leak, "---\ntldr: text: colon\n---\nbody\n").unwrap();
        engine.rebuild();

        let (in_catalog, diag_count) = engine
            .read(|v| {
                (
                    v.catalog.get(&leak).is_some(),
                    v.diagnostics_for_file(&leak).count(),
                )
            })
            .value()
            .unwrap();
        assert!(
            !in_catalog,
            "an auignored file must stay absent from the catalog after a full rebuild"
        );
        assert_eq!(
            diag_count, 0,
            "an auignored file must carry no diagnostics after a full rebuild"
        );
    }

    /// A comparable summary of the held knowledge base: the sorted catalog and the
    /// diagnostic count, for parity assertions across engines.
    fn summary(engine: &Engine) -> (Vec<String>, usize) {
        engine
            .read(|v| {
                let mut catalog: Vec<String> = v
                    .catalog
                    .iter()
                    .map(|(p, e)| format!("{}|{:?}|{:?}", p.display(), e.kind, e.hash))
                    .collect();
                catalog.sort();
                (catalog, v.diagnostics_len())
            })
            .value()
            .unwrap()
    }

    #[test]
    fn scoped_rebuild_picks_up_an_edit() {
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(dir.path().join("a.md"), "---\ntype: note\nextra: 1\n---\n").unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "a.md"));
        assert_eq!(engine.version(), Some(2));
    }

    #[test]
    fn scoped_rebuild_no_ops_on_unchanged_path() {
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        // The path's content is unchanged: the scoped no-op gate skips the
        // rebuild, no version advance.
        engine.inner.rebuild_paths(&dirty(&dir, "a.md"));
        assert_eq!(engine.version(), Some(1));
    }

    #[test]
    fn preview_reports_the_would_be_product_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::repo::canonical_root(dir.path());
        folder_repo(&root);
        fs::create_dir_all(root.join("type")).unwrap();
        fs::write(
            root.join("type/note.type.yaml"),
            "fields:\n  title: String\n",
        )
        .unwrap();
        let engine = Engine::new(&root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        let handle = engine.handle();

        let product = |input: PreviewInput| {
            handle
                .preview(input, |o| match o {
                    PreviewOutcome::Built {
                        preview,
                        held,
                        target,
                    } => crate::wire::preview_product_view(preview, held, target),
                    PreviewOutcome::Reject(r) => crate::wire::PreviewProductView::Reject {
                        reject: crate::wire::PreviewRejectView {
                            message: r.message,
                            detail: r.detail,
                        },
                    },
                })
                .value()
                .unwrap()
        };

        // A write of a valid note: identity `note`, no diagnostics, and no disk write.
        let good = root.join("good.md");
        match product(PreviewInput {
            target: good.clone(),
            op: PreviewOp::Write {
                content: "---\ntype: note\ntitle: Hi\n---\n".to_string(),
            },
            stamps: Vec::new(),
        }) {
            crate::wire::PreviewProductView::Product {
                target,
                blast_radius,
            } => {
                assert!(
                    target.identities.iter().any(|i| i.name == "note"),
                    "{:?}",
                    target.identities
                );
                assert!(target.diagnostics.is_empty(), "{:?}", target.diagnostics);
                assert!(blast_radius.is_empty());
                assert!(target.hash.is_some());
            }
            other => panic!("expected a product, got {other:?}"),
        }
        assert!(!good.exists(), "preview must not write disk");

        // A write claiming note but missing the required title: the note identity
        // still resolves, and the diagnostics carry the required-field failure.
        match product(PreviewInput {
            target: root.join("bad.md"),
            op: PreviewOp::Write {
                content: "---\ntype: note\n---\n".to_string(),
            },
            stamps: Vec::new(),
        }) {
            crate::wire::PreviewProductView::Product { target, .. } => {
                assert!(target.identities.iter().any(|i| i.name == "note"));
                assert!(
                    target
                        .diagnostics
                        .iter()
                        .any(|d| d.code.0 == "required-field-absent"),
                    "{:?}",
                    target.diagnostics
                );
            }
            other => panic!("expected a product, got {other:?}"),
        }

        // An edit of a file that does not exist: a structural reject, no product.
        match product(PreviewInput {
            target: root.join("absent.md"),
            op: PreviewOp::Edit {
                old_string: "x".to_string(),
                new_string: "y".to_string(),
                replace_all: false,
            },
            stamps: Vec::new(),
        }) {
            crate::wire::PreviewProductView::Reject { reject } => {
                assert!(
                    reject.message.contains("nothing to edit"),
                    "{}",
                    reject.message
                );
            }
            other => panic!("expected a reject, got {other:?}"),
        }
    }

    fn fp_of(entries: &[(&str, Option<u8>)]) -> Fingerprint {
        entries
            .iter()
            .map(|(p, seed)| (PathBuf::from(p), seed.map(|s| ContentHash::of(&[s][..]))))
            .collect()
    }

    #[test]
    fn the_fingerprint_diff_reports_content_adds_and_removes() {
        let prior = fp_of(&[("/v/a.md", Some(1)), ("/v/b.md", Some(2))]);
        let next = fp_of(&[("/v/a.md", Some(9)), ("/v/c.md", Some(3))]);
        let dirty = fingerprint_diff(&prior, &next);
        assert_eq!(
            dirty,
            [
                PathBuf::from("/v/a.md"), // content changed
                PathBuf::from("/v/b.md"), // removed
                PathBuf::from("/v/c.md"), // added
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn the_fingerprint_diff_is_empty_when_nothing_moved() {
        let fp = fp_of(&[("/v/a.md", Some(1)), ("/v/photo.png", None)]);
        assert!(fingerprint_diff(&fp, &fp).is_empty());
    }

    #[test]
    fn the_fingerprint_diff_sees_an_asset_appear_and_vanish() {
        // THE HAZARD. A file the build never reads carries NO hash, so an asset
        // add or delete moves only the KEY SET and leaves every value untouched.
        // A diff comparing values alone reports nothing, and the backstop then
        // silently fails to recover exactly the case it exists for.
        //
        // This is the third appearance of one conflation: `Option<Hash>` serving
        // as both "what is the content" and "is it here". The no-op gate and the
        // scoped fingerprint update each got a different half of it wrong.
        let without = fp_of(&[("/v/a.md", Some(1))]);
        let with = fp_of(&[("/v/a.md", Some(1)), ("/v/photo.png", None)]);

        assert_eq!(
            fingerprint_diff(&without, &with),
            [PathBuf::from("/v/photo.png")]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "an appearing asset must be dirty despite carrying no hash"
        );
        assert_eq!(
            fingerprint_diff(&with, &without),
            [PathBuf::from("/v/photo.png")]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "a vanishing asset must be dirty despite carrying no hash"
        );
    }

    /// A folder of instances, plus a file beside it whose name has the folder's
    /// name as a byte prefix.
    fn repo_with_a_subfolder(root: &Path) {
        folder_repo(root);
        fs::write(root.join("keep.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.md"), "---\ntype: note\n---\n").unwrap();
        fs::write(root.join("sub/b.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join("subdir")).unwrap();
        fs::write(root.join("subdir/c.md"), "---\ntype: note\n---\n").unwrap();
    }

    /// A file that was readable at build and is no longer must rebuild ONCE and
    /// then hold still.
    ///
    /// Two properties in one, both easy to break and neither obvious.
    /// - it stays CATALOGUED. A full build walks it and records an unread entry
    ///   with a read-error diagnostic, so splicing it out as a delete would
    ///   diverge, and the divergence would never converge back.
    /// - it SETTLES. `recorded_content` reports `None` both for a path the
    ///   fingerprint never saw and for one it saw and could not read, and a
    ///   failing read reports `None` too, so the second pass compares equal and
    ///   stops. Making that comparison "more precise" turns this into a rebuild
    ///   loop over a file nobody can read.
    ///
    /// Unix only: the fixture needs a real unreadable-but-present file, and
    /// permission bits are how you get one.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_file_rebuilds_once_then_settles() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
        let target = root.join("x.md");
        fs::write(&target, "---\ntype: note\ntitle: X\n---\n").unwrap();

        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Non-vacuity: running as root, or on a filesystem ignoring the mode,
        // would make every assertion below meaningless.
        assert!(
            fs::read(&target).is_err(),
            "fixture must actually be unreadable; skipped as root?"
        );

        engine.inner.rebuild_paths(&dirty(&dir, "x.md"));
        assert_eq!(
            engine.version(),
            Some(2),
            "becoming unreadable is a change: the file gains a read-error diagnostic"
        );
        let entry = engine
            .read(|kb| kb.catalog.get(&root.join("x.md")).map(|e| e.hash))
            .value()
            .expect("ready");
        assert_eq!(
            entry,
            Some(None),
            "it must stay catalogued, as an UNREAD entry, exactly as a full build leaves it"
        );

        // THE CONVERGENCE. Nothing on disk moved, so a second pass must find
        // nothing to do. A `Recorded`-style refactor that keeps absent and
        // present-unread apart fails right here, forever.
        engine.inner.rebuild_paths(&dirty(&dir, "x.md"));
        assert_eq!(
            engine.version(),
            Some(2),
            "a still-unreadable file must settle, not rebuild on every pass"
        );

        // Restore, so the tempdir teardown has nothing to argue with.
        fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn deleting_a_folder_reported_only_as_the_folder_removes_its_files() {
        // A backend can report a folder removal as ONE event naming the folder,
        // with no event per child. The folder is not a node, so every gate
        // downstream used to read it as "nothing changed" and the whole subtree
        // stayed catalogued, resolving references to files that were gone.
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        repo_with_a_subfolder(root);

        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::remove_dir_all(root.join("sub")).unwrap();
        // ONLY the folder. Naming the children would be testing a different bug.
        engine.inner.rebuild_paths(&dirty(&dir, "sub"));

        assert_eq!(engine.version(), Some(2), "the folder delete must rebuild");
        let held = engine
            .read(|kb| {
                (
                    kb.catalog.contains_key(&root.join("sub/a.md")),
                    kb.catalog.contains_key(&root.join("sub/b.md")),
                    kb.catalog.contains_key(&root.join("subdir/c.md")),
                    kb.catalog.contains_key(&root.join("keep.md")),
                )
            })
            .value()
            .expect("ready");
        assert_eq!(
            held,
            (false, false, true, true),
            "the folder's files leave the catalog; `subdir` and `keep.md` stay. \
             A byte-wise prefix test would have taken `subdir/c.md` too"
        );
    }

    #[test]
    fn deleting_a_folder_holding_a_typedef_still_rebuilds_wholly() {
        // WHY THE EXPANSION IS PLACED WHERE IT IS. A type-def under the removed
        // folder makes the fast path decline, and the scoped rebuild below it
        // runs its own no-op gate. Expanding only inside the fast path would
        // leave that gate asking about the DIRECTORY, finding it unchanged, and
        // returning without rebuilding anything at all.
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::create_dir_all(root.join("vocab/type")).unwrap();
        fs::write(
            root.join("vocab/type/thing.type.yaml"),
            "fields:\n  title: String\n",
        )
        .unwrap();
        fs::write(root.join("a.md"), "---\ntype: thing\ntitle: A\n---\n").unwrap();

        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));
        // Non-vacuity: the claim resolves while the vocabulary is present.
        assert_eq!(
            engine
                .read(|kb| kb
                    .diagnostics()
                    .any(|d| d.code.as_str() == "unknown-type-claim"))
                .value(),
            Some(false),
            "the fixture must start with a resolving claim"
        );

        fs::remove_dir_all(root.join("vocab")).unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "vocab"));

        assert_eq!(engine.version(), Some(2), "losing the vocabulary rebuilds");
        assert_eq!(
            engine
                .read(|kb| kb
                    .diagnostics()
                    .any(|d| d.code.as_str() == "unknown-type-claim"))
                .value(),
            Some(true),
            "with the type-def gone, `a.md`'s claim must stop resolving"
        );
    }

    #[test]
    fn deleting_a_folder_matches_a_full_rebuild() {
        // Parity, the oracle this whole path is held to: the spliced state must
        // equal a build that never saw the folder.
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        repo_with_a_subfolder(root);
        // A referrer, so the delete moves diagnostics and backlinks rather than
        // only catalog keys.
        fs::write(
            root.join("keep.md"),
            "---\ntype: note\n---\nsee [[a]] and [[c]].\n",
        )
        .unwrap();

        let scoped = Engine::new(root, crate::repo::ConfigSource::Empty);
        scoped.rebuild();
        fs::remove_dir_all(root.join("sub")).unwrap();
        scoped.inner.rebuild_paths(&dirty(&dir, "sub"));

        let full = Engine::new(root, crate::repo::ConfigSource::Empty);
        full.rebuild();

        let codes = |e: &Engine| -> Option<Vec<String>> {
            e.read(|kb| {
                kb.diagnostics()
                    .map(|d| format!("{}@{}", d.code.as_str(), d.span.file.display()))
                    .collect()
            })
            .value()
        };
        assert_eq!(
            codes(&scoped),
            codes(&full),
            "a spliced folder delete must match a full rebuild"
        );
        let keys = |e: &Engine| -> Option<Vec<String>> {
            e.read(|kb| kb.catalog.keys().map(|p| p.display().to_string()).collect())
                .value()
        };
        assert_eq!(keys(&scoped), keys(&full), "and so must the catalog");
    }

    #[test]
    fn still_moving_reports_only_what_disk_disagrees_about() {
        // The settle predicate, over a filesystem the test controls. It decides
        // whether a splice is done or has to run again, and until `path_changed`
        // took its filesystem there was no way to ask it anything.
        let mut fs = au_parser::MemoryFileSystem::new();
        fs.insert("/v/same.md", b"---\ntype: note\n---\n".to_vec());
        fs.insert("/v/moved.md", b"---\ntype: note\nnow: 2\n---\n".to_vec());
        fs.insert("/v/photo.png", vec![1, 2, 3]);
        // `gone.md` is in the fingerprint and not on disk.

        let fp: Fingerprint = [
            (
                PathBuf::from("/v/same.md"),
                Some(ContentHash::of(b"---\ntype: note\n---\n")),
            ),
            (
                PathBuf::from("/v/moved.md"),
                Some(ContentHash::of(b"---\ntype: note\nwas: 1\n---\n")),
            ),
            // An asset carries no hash. Present in both, so it has not moved,
            // and a value-only comparison would have to guess.
            (PathBuf::from("/v/photo.png"), None),
            (
                PathBuf::from("/v/gone.md"),
                Some(ContentHash::of(b"---\ntype: note\n---\n")),
            ),
        ]
        .into_iter()
        .collect();

        let candidates: BTreeSet<PathBuf> = [
            "/v/same.md",
            "/v/moved.md",
            "/v/photo.png",
            "/v/gone.md",
            "/v/added.md",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        assert_eq!(
            still_moving(&candidates, &fp, &fs),
            ["/v/moved.md", "/v/gone.md"]
                .iter()
                .map(PathBuf::from)
                .collect::<BTreeSet<_>>(),
            "an unchanged file and a present asset have settled; \
             a rewritten one and a removed one have not"
        );
    }

    /// A filesystem whose FIRST read of one path reports content nothing else
    /// will produce, so the settle probe sees that path as still moving exactly
    /// once.
    ///
    /// The mid-recompute race cannot be driven against a real disk on cue: the
    /// window is between the recompute's read and the settle check, and there is
    /// no seam to write into. So the PROBE is what the test controls, and it
    /// answers the question the wiring actually turns on — when a path is
    /// reported as still moving, does another splice pass run.
    struct MovedOnce {
        path: PathBuf,
        probes: std::sync::atomic::AtomicUsize,
    }

    impl FileSystem for MovedOnce {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            if path == self.path
                && self
                    .probes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            {
                return Ok(b"bytes no build ever committed".to_vec());
            }
            RealFileSystem.read_file(path)
        }
        fn is_file(&self, path: &Path) -> bool {
            RealFileSystem.is_file(path)
        }
        fn walk_files(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<au_parser::Walk> {
            RealFileSystem.walk_files(root, filter)
        }
        fn walk_scope_boundaries(
            &self,
            root: &Path,
            filter: &au_parser::WalkFilter,
        ) -> std::io::Result<(au_parser::ScopeBoundaries, Vec<au_parser::WalkError>)> {
            RealFileSystem.walk_scope_boundaries(root, filter)
        }
    }

    #[test]
    fn a_path_that_moved_again_gets_spliced_again() {
        // The property finding 2 restores. A splice reads each dirty path at
        // some instant; if the path moves after that read, the commit is behind
        // disk and nothing in the old wiring looked again.
        //
        // The retry is NARROW on purpose. The whole-rebuild branch confirms by
        // re-fingerprinting the workspace and retrying the outer loop, and
        // wiring this the same way would make the reconcile walk and hash
        // everything twice — the exact cost diffing the fingerprint exists to
        // avoid.
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(dir.path().join("a.md"), "---\ntype: note\nedit: 1\n---\n").unwrap();

        let probe = MovedOnce {
            // The probe matches against the path the splice reads, which is the
            // canonical catalog key.
            path: crate::repo::canonical_root(dir.path()).join("a.md"),
            probes: std::sync::atomic::AtomicUsize::new(0),
        };
        let spans = spans_during(|| {
            assert!(
                engine
                    .inner
                    .splice_until_settled(dirty(&dir, "a.md"), &probe),
                "the splice handled the write"
            );
        });

        assert_eq!(
            spans
                .iter()
                .filter(|n| *n == "try_incremental_fast_path")
                .count(),
            2,
            "one splice, then one more because the probe said the path moved again: {spans:?}"
        );
        assert!(
            !spans.iter().any(|n| n == "build_reusing"),
            "the retry stays narrow, it must not fall through to a whole rebuild: {spans:?}"
        );
        // The second pass is a no-op against the real disk, so it must not
        // invent a version. Recovering a write costs one advance, not two.
        assert_eq!(engine.version(), Some(2));
    }

    #[test]
    fn a_settled_splice_does_not_run_again() {
        // The other half: with nothing moving, the loop runs exactly once. A
        // settle check that always reported "still moving" would spin to its
        // bound on every ordinary write and be invisible except as latency.
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();

        fs::write(dir.path().join("a.md"), "---\ntype: note\nedit: 1\n---\n").unwrap();
        let spans = spans_during(|| {
            engine
                .inner
                .splice_until_settled(dirty(&dir, "a.md"), &RealFileSystem);
        });
        assert_eq!(
            spans
                .iter()
                .filter(|n| *n == "try_incremental_fast_path")
                .count(),
            1,
            "a write that settled must be spliced once: {spans:?}"
        );
    }

    use crate::spancap::capture as spans_during;

    /// Drive [`watch_loop`] against a source the test owns, so "a burst arrived"
    /// and "a change nobody reported" are separable, which a real watcher cannot
    /// do.
    ///
    /// The sender is returned and must be held: dropping it disconnects the
    /// channel and the loop exits.
    fn watch_loop_driven_by(
        engine: &Engine,
        window: Duration,
    ) -> std::sync::mpsc::Sender<notify::Result<notify::Event>> {
        let (tx, rx) = channel();
        let inner = Arc::clone(&engine.inner);
        std::thread::spawn(move || watch_loop(inner, rx, Some(window)));
        tx
    }

    fn wait_for_version(engine: &Engine, want: u64, budget: Duration) {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if engine.version() == Some(want) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn activity_arms_a_reconcile_that_catches_what_the_burst_missed() {
        // The backstop's contract: after a burst settles, ONE whole-knowledge-base
        // pass runs, so anything the burst's own dirty set missed is still
        // caught. Armed by activity, because activity is when a drop is possible.
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        let tx = watch_loop_driven_by(&engine, Duration::from_millis(50));

        // A change with no event of its own.
        fs::write(
            dir.path().join("a.md"),
            "---\ntype: note\nunreported: 1\n---\n",
        )
        .unwrap();

        // An event about something ELSE: the activity that arms the pass.
        tx.send(Ok(notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Any,
        ))
        .add_path(dir.path().join("b.md"))))
            .unwrap();

        wait_for_version(&engine, 2, Duration::from_secs(20));
        assert_eq!(
            engine.version(),
            Some(2),
            "the pass armed by activity must catch the change the burst did not name"
        );
        drop(tx);
    }

    #[test]
    fn the_reconcile_stays_off_when_the_policy_is_off() {
        // The opt-out must keep meaning what it says: where every write goes
        // through the mutation channel the dirty set is exact and the pass is
        // pure cost.
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        let (tx, rx) = channel::<notify::Result<notify::Event>>();
        let inner = Arc::clone(&engine.inner);
        std::thread::spawn(move || watch_loop(inner, rx, None));

        fs::write(
            dir.path().join("a.md"),
            "---\ntype: note\nunreported: 1\n---\n",
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            engine.version(),
            Some(1),
            "with the policy off, an unreported change stays unnoticed by design"
        );
        drop(tx);
    }

    #[test]
    fn a_reconcile_recovers_a_missed_edit_through_the_fast_path() {
        // The dropped-event case, driven directly: write to disk and DO NOT tell
        // the engine, then reconcile. That is what the idle pass sees when
        // FSEvents coalesces an event away.
        //
        // The recovery must cost the WRITE, not a whole-knowledge-base rebuild.
        // The reconcile already computes both fingerprints, so the changed path
        // is in hand; it used to be discarded.
        let dir = two_file_repo();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(
            dir.path().join("a.md"),
            "---\ntype: note\nextra: 1\n---\nrecovered\n",
        )
        .unwrap();

        let spans = spans_during(|| engine.rebuild());
        assert_eq!(engine.version(), Some(2), "the missed edit is absorbed");
        assert!(
            !spans.iter().any(|n| n == "build_reusing"),
            "recovery must splice, not rebuild the whole knowledge base: {spans:?}"
        );
        assert!(
            spans.iter().any(|n| n == "recompute_dirty"),
            "recovery must go through the incremental recompute: {spans:?}"
        );
    }

    #[test]
    fn a_reconcile_still_rebuilds_wholly_for_a_vocabulary_change() {
        // The other half: the fast path declines a type-def, so the reconcile
        // must still take the whole rebuild. Guards against the diff narrowing
        // work that is not safe to narrow.
        let dir = two_file_repo();
        fs::create_dir_all(dir.path().join("type")).unwrap();
        fs::write(
            dir.path().join("type/note.type.yaml"),
            "fields:\n  a?: String\n",
        )
        .unwrap();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        let before = engine.version();

        fs::write(
            dir.path().join("type/note.type.yaml"),
            "fields:\n  a?: String\n  b?: String\n",
        )
        .unwrap();

        let spans = spans_during(|| engine.rebuild());
        assert!(engine.version() > before, "the vocabulary change lands");
        assert!(
            spans.iter().any(|n| n == "build_reusing"),
            "a type-def change must still rebuild wholly: {spans:?}"
        );
    }

    /// An asset's BYTES are never read by the build, so a content edit to one
    /// cannot change anything the engine holds, and must advance nothing.
    ///
    /// The gate hashed every dirty path unconditionally while `fingerprint`
    /// recorded a non-read path as `None`, so `Some(hash) != None` read every
    /// asset write as a change: a whole-knowledge-base rebuild, plus a full read
    /// of a file that can be arbitrarily large, for a change nothing can observe.
    #[test]
    fn an_asset_content_edit_advances_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();
        fs::write(root.join("photo.png"), b"original bytes").unwrap();

        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(root.join("photo.png"), b"completely different bytes").unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "photo.png"));
        assert_eq!(
            engine.version(),
            Some(1),
            "an asset's content is never read, so editing it changes nothing"
        );
    }

    /// The other half: an asset's PRESENCE is load-bearing, so its add and its
    /// delete must still rebuild.
    ///
    /// An asset is a `RepoIndex` member, so it resolves `file*` and navigational
    /// references. Guards the no-op gate against over-correcting into "an asset
    /// never matters".
    #[test]
    fn an_asset_add_and_delete_still_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::write(root.join("a.md"), "---\ntype: note\n---\n").unwrap();

        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(root.join("photo.png"), b"x").unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "photo.png"));
        assert_eq!(
            engine.version(),
            Some(2),
            "an asset add changes the path set"
        );
        // Non-vacuity: the add really did land, so the version bump is the
        // asset's and not some unrelated churn.
        assert_eq!(
            engine
                .read(|kb| kb.catalog.contains_key(&root.join("photo.png")))
                .value(),
            Some(true),
            "the added asset must be catalogued"
        );

        fs::remove_file(root.join("photo.png")).unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "photo.png"));
        assert_eq!(
            engine.version(),
            Some(3),
            "an asset delete changes the path set"
        );
    }

    /// Concurrent rebuilds with disjoint dirty sets must not lose an update.
    ///
    /// Two rebuild entry points run on separate threads in the daemon: the
    /// watcher thread and a mutation's `spawn_blocking` task. They share no lock
    /// but the rebuild lock. The incremental fast path snapshots the knowledge base,
    /// computes off the state lock, then swaps its `base + one-file` patch in.
    /// Without rebuild serialization, several threads snapshot the same base and
    /// the last swap clobbers the others, dropping their edits. This drives that
    /// race directly: N files each edited on their own thread, released at one
    /// instant, then every edit must be present in the held knowledge base.
    #[test]
    fn concurrent_disjoint_rebuilds_do_not_lose_an_update() {
        use std::sync::{Arc as StdArc, Barrier};
        use std::thread;

        const FILES: usize = 8;
        const ROUNDS: usize = 20;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        folder_repo(&root);
        for i in 0..FILES {
            fs::write(
                root.join(format!("f{i}.md")),
                "---\ntype: note\nn: 0\n---\n",
            )
            .unwrap();
        }
        let engine = Engine::new(&root, crate::repo::ConfigSource::Empty);
        engine.rebuild();

        for round in 1..=ROUNDS {
            // Edit every file to a fresh, distinct content for this round.
            let mut expected: Vec<(PathBuf, ContentHash)> = Vec::new();
            for i in 0..FILES {
                let path = root.join(format!("f{i}.md"));
                let bytes = format!("---\ntype: note\nn: {round}\n---\n").into_bytes();
                fs::write(&path, &bytes).unwrap();
                expected.push((path, ContentHash::of(&bytes)));
            }
            // One rebuild_paths per file, all released together.
            let barrier = StdArc::new(Barrier::new(FILES));
            let mut handles = Vec::new();
            for i in 0..FILES {
                let inner = Arc::clone(&engine.inner);
                let path = root.join(format!("f{i}.md"));
                let barrier = StdArc::clone(&barrier);
                handles.push(thread::spawn(move || {
                    let set: BTreeSet<PathBuf> = std::iter::once(path).collect();
                    barrier.wait();
                    inner.rebuild_paths(&set);
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            // Every edit must have landed; a lost update leaves a stale hash.
            let got = engine
                .read(|v| {
                    expected
                        .iter()
                        .map(|(p, _)| v.catalog.get(p).and_then(|e| e.hash))
                        .collect::<Vec<_>>()
                })
                .value()
                .unwrap();
            for ((path, want), have) in expected.iter().zip(got) {
                assert_eq!(
                    have,
                    Some(*want),
                    "round {round}: {} lost its edit to a concurrent rebuild",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn editing_an_excluded_file_is_a_no_op_but_in_scope_still_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let root = &crate::repo::canonical_root(dir.path());
        folder_repo(root);
        fs::write(root.join("keep.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/guide.md"), "---\ntype: note\n---\n").unwrap();
        fs::create_dir_all(root.join(".arsumbris")).unwrap();
        fs::write(root.join(".arsumbris/.auignore"), "docs/\n").unwrap();
        let engine = Engine::new(root, crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        // The excluded file's content genuinely changes, so the no-op gate would
        // NOT catch it (its prior fingerprint is absent); only the scope filter
        // keeps it out. No rebuild, no version advance.
        fs::write(
            root.join("docs/guide.md"),
            "---\ntype: note\nextra: 1\n---\n",
        )
        .unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "docs/guide.md"));
        assert_eq!(engine.version(), Some(1), "excluded edit is a no-op");

        // A sibling in-scope edit still rebuilds.
        fs::write(root.join("keep.md"), "---\ntype: note\nextra: 1\n---\n").unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "keep.md"));
        assert_eq!(engine.version(), Some(2), "in-scope edit rebuilds");
    }

    #[test]
    fn scoped_rebuild_matches_a_full_rebuild() {
        let dir = two_file_repo();
        let scoped = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        scoped.rebuild();
        fs::write(dir.path().join("a.md"), "---\ntype: note\nextra: 1\n---\n").unwrap();
        scoped.inner.rebuild_paths(&dirty(&dir, "a.md"));

        let full = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        full.rebuild();

        assert_eq!(summary(&scoped), summary(&full));
    }

    #[test]
    fn instance_fast_path_is_byte_identical_to_a_full_rebuild() {
        // An instance content edit routes through the instance-only fast path.
        // The spliced knowledge base must be byte-identical to a from-scratch build, the
        // wired-path check over the whole knowledge base, not just the catalog summary.
        let dir = two_file_repo();
        let scoped = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        scoped.rebuild();
        assert_eq!(scoped.version(), Some(1));

        fs::write(dir.path().join("a.md"), "---\ntype: note\nextra: 1\n---\n").unwrap();
        scoped.inner.rebuild_paths(&dirty(&dir, "a.md"));
        assert_eq!(
            scoped.version(),
            Some(2),
            "the instance edit advanced the version"
        );

        let full = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        full.rebuild();

        let s = scoped.read(|v| v.parity_facts()).value().unwrap();
        let f = full.read(|v| v.parity_facts()).value().unwrap();
        assert_eq!(
            s, f,
            "fast-path knowledge base equals a full rebuild fact for fact"
        );
    }

    #[test]
    fn scoped_rebuild_picks_up_a_new_then_dropped_file() {
        let dir = two_file_repo();
        // Catalog keys are canonical, the one spelling the assembly holds.
        let root = &crate::repo::canonical_root(dir.path());
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();

        // Add: a created file is in the walk, missing the prior layer, so it
        // reads and joins the catalog.
        fs::write(root.join("c.md"), "---\ntype: note\n---\n").unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "c.md"));
        let present = engine
            .read(|v| v.catalog.contains_key(root.join("c.md").as_path()))
            .value()
            .unwrap();
        assert!(present, "created file joined the catalog");

        // Delete: it leaves the walk and drops from the catalog.
        fs::remove_file(root.join("c.md")).unwrap();
        engine.inner.rebuild_paths(&dirty(&dir, "c.md"));
        let present = engine
            .read(|v| v.catalog.contains_key(root.join("c.md").as_path()))
            .value()
            .unwrap();
        assert!(!present, "deleted file dropped from the catalog");
    }

    /// A repo entered through a symlink is held under ONE spelling, the
    /// canonical one.
    ///
    /// Two spellings of one directory would be two path identities for the same
    /// files: two catalog key sets, and in the write path two saga members over
    /// one git working tree, which lands two commits carrying one `Mutation-Id`
    /// where compensation can only revert the newest. Normalizing at the assembly
    /// boundary is what makes every downstream lexical path operation safe.
    #[test]
    fn a_repo_entered_through_a_symlink_is_held_canonically() {
        let dir = tempfile::tempdir().unwrap();
        let real = crate::repo::canonical_root(dir.path()).join("ws");
        fs::create_dir_all(&real).unwrap();
        folder_repo(&real);
        fs::write(real.join("a.md"), "---\ntype: note\n---\n").unwrap();

        let link = crate::repo::canonical_root(dir.path()).join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_ne!(link, real, "the two spellings must actually differ");

        // Entered by the SYMLINK, held under the real path.
        let engine = Engine::new(&link, crate::repo::ConfigSource::Empty);
        engine.rebuild();

        let (canonical, aliased) = engine
            .read(|v| {
                (
                    v.catalog.contains_key(real.join("a.md").as_path()),
                    v.catalog.contains_key(link.join("a.md").as_path()),
                )
            })
            .value()
            .unwrap();
        assert!(canonical, "the catalog keys the file under its real path");
        assert!(
            !aliased,
            "the symlink spelling must not be a second identity for the same file"
        );
    }

    #[test]
    fn rebuild_commits_then_no_ops_when_disk_is_unchanged() {
        let dir = kb();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);

        engine.rebuild();
        assert_eq!(engine.version(), Some(1));
        assert_eq!(engine.lifecycle(), (EngineState::Up, RefState::Ready));

        // Disk is unchanged: the stored fingerprint must describe the stored
        // knowledge base, so this is a no-op. Guards the fingerprint/knowledge base skew that
        // would otherwise let a later edit be wrongly seen as "no change".
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));
    }

    #[test]
    fn rebuild_picks_up_an_edit_then_settles() {
        let dir = kb();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(dir.path().join("a.md"), "---\ntype: note\nextra: 1\n---\n").unwrap();
        engine.rebuild();
        assert_eq!(engine.version(), Some(2));

        // No further change re-bumps the version.
        engine.rebuild();
        assert_eq!(engine.version(), Some(2));
    }

    #[test]
    fn rebuild_picks_up_a_new_file() {
        let dir = kb();
        let engine = Engine::new(dir.path(), crate::repo::ConfigSource::Empty);
        engine.rebuild();
        assert_eq!(engine.version(), Some(1));

        fs::write(dir.path().join("b.md"), "---\ntype: note\n---\n").unwrap();
        engine.rebuild();
        assert_eq!(engine.version(), Some(2));
    }

    #[test]
    fn accumulate_collects_paths_from_a_plain_event() {
        use notify::event::{EventKind, ModifyKind};
        let mut dirty = BTreeSet::new();
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/v/a.md"))
            .add_path(PathBuf::from("/v/b.md"));
        let full = accumulate_event(&mut dirty, Ok(event));
        assert!(!full, "a plain event does not force a full rebuild");
        assert!(dirty.contains(Path::new("/v/a.md")));
        assert!(dirty.contains(Path::new("/v/b.md")));
    }

    #[test]
    fn accumulate_forces_full_on_a_backend_error() {
        let mut dirty = BTreeSet::new();
        let full = accumulate_event(&mut dirty, Err(notify::Error::generic("backend error")));
        assert!(full, "an error cannot be attributed, so it forces full");
        assert!(dirty.is_empty());
    }

    #[test]
    fn accumulate_forces_full_on_a_rescan() {
        use notify::event::{EventKind, Flag};
        let mut dirty = BTreeSet::new();
        let event = notify::Event::new(EventKind::Any).set_flag(Flag::Rescan);
        let full = accumulate_event(&mut dirty, Ok(event));
        assert!(full, "a rescan signal forces full");
    }

    #[test]
    fn every_delivered_message_is_spanned() {
        // The span carries no timing worth reading. It exists so that "the
        // backend delivered nothing" is OBSERVABLE, because an unobservable
        // fact gets inferred instead, and the inference made here was wrong.
        //
        // Guarded for the same reason `fingerprint`'s span is: losing it is
        // silent, and what it would cost is a reader confidently concluding an
        // event was lost when nothing ever looked.
        for (label, message) in [
            (
                "change",
                Ok(notify::Event::new(notify::EventKind::Modify(
                    notify::event::ModifyKind::Any,
                ))),
            ),
            (
                "rescan",
                Ok(notify::Event::new(notify::EventKind::Other)
                    .set_flag(notify::event::Flag::Rescan)),
            ),
            ("error", Err(notify::Error::generic("backend failed"))),
        ] {
            let seen = spans_during(|| {
                let mut dirty = BTreeSet::new();
                accumulate_event(&mut dirty, message);
            });
            assert!(
                seen.iter().any(|n| n == "watch_event"),
                "a delivered {label} message must be spanned, got {seen:?}"
            );
        }
    }

    #[test]
    fn reconcile_policy_parses_the_env_knob() {
        assert_eq!(ReconcilePolicy::parse(None), ReconcilePolicy::default());
        assert_eq!(ReconcilePolicy::parse(Some("off")), ReconcilePolicy::Off);
        assert_eq!(ReconcilePolicy::parse(Some("OFF")), ReconcilePolicy::Off);
        assert_eq!(
            ReconcilePolicy::parse(Some("3")),
            ReconcilePolicy::Idle {
                window: Duration::from_secs(3)
            }
        );
        // Zero and unparseable fall back to the default.
        assert_eq!(
            ReconcilePolicy::parse(Some("0")),
            ReconcilePolicy::default()
        );
        assert_eq!(
            ReconcilePolicy::parse(Some("garbage")),
            ReconcilePolicy::default()
        );
    }
}
