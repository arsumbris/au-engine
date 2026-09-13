//! The on-demand git-reflog watcher backing the `recent_commits` subscription.
//!
//! A `recent_commits` subscription's SOLE liveness source: the reflog moves on
//! every commit, engine-mediated or from a terminal, so watching each tree's
//! reflog is the complete signal. Independent of the content watcher (which the
//! `.git` floor keeps out of the graph); this is a separate, deliberate watch on
//! the resolved reflog path, off the knowledge-base version signal. See
//! [[spec - recent-commits activity stream - a member-aware bounded commit stream with a reflog-watched append-only subscription]].
//!
//! Armed on demand and REF-COUNTED per working tree: the first subscription to
//! watch a tree starts its `notify` watcher, the last to drop it stops it, so
//! nothing is watched while no one is looking. A reflog event broadcasts the
//! tree root that moved; a subscriber re-logs that one tree and re-merges. The
//! broadcast delivers the tree per event, so the common wake is scoped to the
//! one store that changed; a lagged receiver re-logs all its trees instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use notify::{RecursiveMode, Watcher};
use tokio::sync::broadcast;

/// How many buffered tree-moved events a slow subscriber may fall behind before
/// it lags. On lag a subscriber re-logs all its trees, so the bound only trades
/// scoped re-logs for a coarse one under a burst, never a missed commit.
const GIT_EVENT_BUFFER: usize = 256;

/// The device's on-demand reflog watchers, ref-counted per working tree.
///
/// One per engine. `arm` / `disarm` bracket a subscription's interest in a set
/// of trees; `subscribe` hands out a receiver of the tree roots that move.
pub(crate) struct GitWatch {
    /// Broadcasts the working-tree root whose reflog just moved.
    tx: broadcast::Sender<PathBuf>,
    /// The live watchers, keyed by tree root, each with its interest count.
    watched: Mutex<HashMap<PathBuf, Watched>>,
}

/// One tree's live watch: its interest count and the `notify` watcher whose drop
/// stops it.
struct Watched {
    refs: usize,
    /// Dropping the watcher stops watching. `_`-prefixed: held for its Drop, not
    /// read.
    _watcher: notify::RecommendedWatcher,
}

impl GitWatch {
    pub(crate) fn new() -> Self {
        let (tx, _rx) = broadcast::channel(GIT_EVENT_BUFFER);
        GitWatch {
            tx,
            watched: Mutex::new(HashMap::new()),
        }
    }

    /// A receiver of the tree roots whose reflogs move. Each subscription holds
    /// one; every armed tree's events reach every receiver.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<PathBuf> {
        self.tx.subscribe()
    }

    /// Register interest in each tree, starting a watcher for one not yet
    /// watched. Idempotent per caller pairing with [`GitWatch::disarm`]: a second
    /// arm of a watched tree only bumps its count.
    ///
    /// Returns the subset of `trees` it ACTUALLY armed (incremented a count for),
    /// so the caller disarms exactly that set. A skipped tree is absent from the
    /// return, so a later `disarm` of it cannot decrement another subscription's
    /// count for the same tree, the asymmetry a "disarm everything requested"
    /// caller would hit across overlapping subscriptions.
    ///
    /// Best-effort per tree: a tree whose reflog cannot be located or watched
    /// (an empty repo with no `logs/` yet, a non-git dir) is skipped, so its
    /// commits simply do not wake the pulse, never an error. Sync (a `git`
    /// subprocess plus a `notify` registration), so a caller runs it off the
    /// reactor.
    pub(crate) fn arm(&self, trees: &[PathBuf]) -> Vec<PathBuf> {
        let mut armed = Vec::new();
        let mut watched = self.watched.lock().unwrap();
        for tree in trees {
            if let Some(w) = watched.get_mut(tree) {
                w.refs += 1;
                armed.push(tree.clone());
                continue;
            }
            let Some(dir) = reflog_watch_dir(tree) else {
                continue; // no reflog to watch yet; a commit here won't wake, best-effort.
            };
            let tx = self.tx.clone();
            let moved = tree.clone();
            let mut watcher = match notify::recommended_watcher(move |res: notify::Result<_>| {
                // Any event under the reflog dir is commit / ref activity for
                // this tree. The value is a claim of "this tree moved"; a
                // subscriber re-logs and diffs, so a spurious wake is harmless.
                if res.is_ok() {
                    let _ = tx.send(moved.clone());
                }
            }) {
                Ok(w) => w,
                Err(_) => continue, // cannot build a watcher; skip this tree.
            };
            if watcher.watch(&dir, RecursiveMode::Recursive).is_err() {
                continue; // the reflog dir vanished between probe and watch; skip.
            }
            watched.insert(
                tree.clone(),
                Watched {
                    refs: 1,
                    _watcher: watcher,
                },
            );
            armed.push(tree.clone());
        }
        armed
    }

    /// Drop interest in each tree, stopping a watcher whose count reaches zero.
    /// Pairs with [`GitWatch::arm`]; a tree that was skipped at arm (no watcher)
    /// is simply absent here.
    pub(crate) fn disarm(&self, trees: &[PathBuf]) {
        let mut watched = self.watched.lock().unwrap();
        for tree in trees {
            if let Some(w) = watched.get_mut(tree) {
                w.refs -= 1;
                if w.refs == 0 {
                    watched.remove(tree); // drops the notify watcher, stops watching.
                }
            }
        }
    }

    /// The number of trees currently watched, for tests and instrumentation.
    #[cfg(test)]
    pub(crate) fn watched_count(&self) -> usize {
        self.watched.lock().unwrap().len()
    }
}

/// The directory to watch for a tree's reflog activity, resolved via git.
///
/// `git -C <tree> rev-parse --git-path logs/HEAD` yields the reflog path for the
/// COVERING working tree, wherever its git dir lives — a linked worktree's is
/// under the main store's `.git/worktrees/<id>/`, a submodule's under
/// `.git/modules/<name>/`, so a hardcoded `<tree>/.git/logs/HEAD` would miss
/// them. FSEvents watches directories, so the reflog's PARENT dir is the target;
/// `None` when it does not exist yet (an empty repo) or git cannot resolve it.
fn reflog_watch_dir(tree: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "--git-path", "logs/HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // not a git tree.
    }
    let rel = String::from_utf8(out.stdout).ok()?;
    let rel = rel.trim();
    if rel.is_empty() {
        return None;
    }
    // `--git-path` yields a path relative to the tree (or absolute); resolve it
    // against the tree, then take the reflog's parent directory to watch.
    let logs_head = {
        let p = Path::new(rel);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            tree.join(p)
        }
    };
    let dir = logs_head.parent()?.to_path_buf();
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn repo_with_a_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "T"]);
        git(p, &["config", "user.email", "t@e"]);
        std::fs::write(p.join("a.md"), "x\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "seed"]);
        dir
    }

    #[test]
    fn arm_is_refcounted_and_disarm_stops_at_zero() {
        let repo = repo_with_a_commit();
        let tree = std::fs::canonicalize(repo.path()).unwrap();
        let gw = GitWatch::new();

        // arm returns the trees it armed, so the caller disarms exactly those.
        assert_eq!(gw.arm(std::slice::from_ref(&tree)), vec![tree.clone()]);
        assert_eq!(gw.watched_count(), 1);
        // A second arm of the same tree only bumps the count, one watcher.
        assert_eq!(gw.arm(std::slice::from_ref(&tree)), vec![tree.clone()]);
        assert_eq!(gw.watched_count(), 1);
        // One disarm leaves it watched, the second stops it.
        gw.disarm(std::slice::from_ref(&tree));
        assert_eq!(gw.watched_count(), 1);
        gw.disarm(std::slice::from_ref(&tree));
        assert_eq!(gw.watched_count(), 0);
    }

    #[test]
    fn a_non_git_tree_is_skipped_not_watched() {
        let dir = tempfile::tempdir().unwrap(); // no git init
        let tree = std::fs::canonicalize(dir.path()).unwrap();
        let gw = GitWatch::new();
        // A skipped tree is NOT in the armed set, so a later disarm of it cannot
        // decrement another subscription's count for the same tree.
        assert!(
            gw.arm(std::slice::from_ref(&tree)).is_empty(),
            "a non-git tree is not armed"
        );
        assert_eq!(
            gw.watched_count(),
            0,
            "a non-git tree has no reflog to watch"
        );
    }

    #[test]
    fn reflog_dir_resolves_for_a_repo_with_history() {
        let repo = repo_with_a_commit();
        let dir = reflog_watch_dir(repo.path()).expect("a repo with a commit has logs/HEAD");
        assert!(dir.is_dir());
        assert!(
            dir.join("HEAD").exists(),
            "logs/HEAD is under the resolved dir"
        );
    }
}
