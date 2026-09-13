//! On-demand resolution of a commit-pinned reference into a recorded edge, per
//! [[spec - pinned references - a recorded resolved edge with an immutable past and an on-demand forward trace]].
//!
//! A pin `[[target::repo@commit]]` resolves the target against the commit's
//! immutable tree, not the working tree. The result is a [`PinnedResolution`]:
//! the path the target had at that commit, even from a bare name, plus the blob
//! oid, the exact bytes. The commit's tree never changes, so the resolution is
//! computed once and cached forever.
//!
//! The name index reuses [`RepoIndex`] over the commit's `ls-tree` output, so a
//! bare name resolves by exactly the rules the working-tree index applies,
//! basename, stem, relpath, type-name, ambiguity.
//!
// The resolver is wired to the mutation channel (construction) and the on-demand
// read surface in subsequent slices; until then its API is exercised by tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use au_references::{RepoIndex, ResolutionError};

use crate::gitwriter::{GitWriteError, ShellGit};

/// A pinned reference resolved against its commit's immutable tree.
///
/// The recorded resolved edge. Its fields never change, the commit is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinnedResolution {
    /// The pinned commit-ish, verbatim from the reference.
    pub commit: String,
    /// The path the target resolved to at that commit, even when the reference
    /// wrote a bare name.
    ///
    /// Relative to the GIT WORKING TREE root, the same basis a `Moved:` trailer
    /// records and the only basis `<rev>:<path>` accepts. For a repo nested in a
    /// monorepo that is an ANCESTOR of the au-repo root, so the two bases differ
    /// and mixing them is silently wrong rather than an error: the trace compares
    /// this against a trailer's `from`, and hands it to `blob_oid_at_commit`,
    /// which resolves against the tree root regardless of `-C`. So the `repo`
    /// argument that produced it must be a working-tree root.
    pub path: PathBuf,
    /// The blob object id, the exact bytes. The version-exact content resolves
    /// from it on demand, and a content-addressed cache keys on it.
    pub oid: String,
}

/// Why a pin could not be resolved into a record.
#[derive(Debug)]
pub(crate) enum PinnedResolutionError {
    /// The commit is not in the local object store. `pinned-commit-unavailable`,
    /// a warning, the pin may resolve once the commit is fetched.
    CommitUnavailable,
    /// The target is absent at the commit, a bogus pin naming bytes that never
    /// existed there. `pinned-path-absent`, an error.
    PathAbsent,
    /// A bare name matched several paths at the commit, carrying every match.
    /// The `reference-target-ambiguous` family, scoped to the commit's tree.
    Ambiguous(Vec<PathBuf>),
    /// A git invocation failed for a reason other than an absent object.
    Git(GitWriteError),
}

/// Resolves commit-pinned references, caching one name index per commit.
///
/// A commit's tree is immutable, so a cache entry never invalidates; the cache
/// is bounded by the distinct commits ever queried. So a bare-name pin costs one
/// `ls-tree` per pinned commit ever resolved, then O(1) per later query.
pub(crate) struct PinnedResolver {
    /// Keyed by (repo root, commit) so a cross-repo pin caches per peer store.
    name_index: Mutex<HashMap<(PathBuf, String), Arc<RepoIndex>>>,
}

impl PinnedResolver {
    pub(crate) fn new() -> Self {
        PinnedResolver {
            name_index: Mutex::new(HashMap::new()),
        }
    }

    /// The commit's name index, built once from its tree and cached.
    fn index_at(&self, repo: &Path, commit: &str) -> Result<Arc<RepoIndex>, PinnedResolutionError> {
        let key = (repo.to_path_buf(), commit.to_string());
        if let Some(ix) = self.name_index.lock().unwrap().get(&key) {
            return Ok(ix.clone());
        }
        let files = ShellGit
            .list_tree_at_commit(repo, commit)
            .map_err(PinnedResolutionError::Git)?;
        // RepoIndex wants absolute paths under root; resolution is pure path
        // logic, it never touches the filesystem, so these need not exist live.
        let absolute: Vec<PathBuf> = files.iter().map(|rel| repo.join(rel)).collect();
        let (index, _diags) = RepoIndex::build(repo.to_path_buf(), absolute);
        let index = Arc::new(index);
        self.name_index.lock().unwrap().insert(key, index.clone());
        Ok(index)
    }

    /// Resolve a pinned reference into its recorded edge.
    ///
    /// `target` is the wikilink name, a path or a bare name. `commit` is checked
    /// for presence first, so an absent commit is `CommitUnavailable`, not
    /// mistaken for an absent path.
    pub(crate) fn resolve(
        &self,
        repo: &Path,
        commit: &str,
        target: &str,
    ) -> Result<PinnedResolution, PinnedResolutionError> {
        if !ShellGit
            .commit_present(repo, commit)
            .map_err(PinnedResolutionError::Git)?
        {
            return Err(PinnedResolutionError::CommitUnavailable);
        }
        let index = self.index_at(repo, commit)?;
        let absolute = match index.resolve(target) {
            Ok(path) => path,
            Err(ResolutionError::Missing) => return Err(PinnedResolutionError::PathAbsent),
            Err(ResolutionError::Ambiguous(matches)) => {
                return Err(PinnedResolutionError::Ambiguous(matches));
            }
        };
        let rel = absolute
            .strip_prefix(repo)
            .unwrap_or(&absolute)
            .to_path_buf();
        let oid = ShellGit
            .blob_oid_at_commit(repo, commit, &rel)
            .map_err(PinnedResolutionError::Git)?
            .ok_or(PinnedResolutionError::PathAbsent)?;
        Ok(PinnedResolution {
            commit: commit.to_string(),
            path: rel,
            oid,
        })
    }

    /// The version-exact bytes for a resolved pin, on demand.
    ///
    /// Reads the recorded path at the recorded commit. Deletion-stable, the bytes
    /// resolve even after the live file is gone.
    pub(crate) fn read_bytes(
        &self,
        repo: &Path,
        resolution: &PinnedResolution,
    ) -> Result<Vec<u8>, PinnedResolutionError> {
        ShellGit
            .read_at_commit(repo, &resolution.commit, &resolution.path)
            .map_err(PinnedResolutionError::Git)?
            .ok_or(PinnedResolutionError::PathAbsent)
    }
}

/// How a moved pin's live counterpart was located, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceTier {
    /// A `Moved:` commit trailer, RECORDED rather than authenticated.
    ///
    /// The strongest tier, because the engine wrote the record at the moment it
    /// performed the move instead of inferring it afterwards. It is not proof.
    /// A trailer is unsigned free text in a commit body, the gate is a prefix
    /// test, and the engine commits with `--no-gpg-sign`, so anyone able to
    /// write a commit body can assert an edge, and a fetched peer's bodies enter
    /// the local store like any other. A consumer may prefer it over the oid
    /// tier; it must not treat it as authenticated.
    Trailer,
    /// The pinned blob's exact oid found in HEAD, an out-of-band same-byte move.
    ExactOid,
}

/// Where the pinned content is now in the working tree, HEAD-relative.
///
/// The live whereabouts, separate from the immutable recorded resolution. The
/// adjudicated and git-fuzzy tiers (a moved-and-edited out-of-band file) are not
/// here, they need the adjudication flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForwardTrace {
    /// The exact pinned bytes still sit at the recorded path, the pin is dormant.
    Present,
    /// The content moved, located by `tier` at `to`.
    Moved { to: PathBuf, tier: TraceTier },
    /// The pinned bytes exist at several HEAD paths, so no single move is
    /// certain. A candidate set for adjudication, never presented as a fact.
    Ambiguous(Vec<PathBuf>),
    /// Not at the recorded path, no trailer chain, oid absent from HEAD.
    Gone,
}

/// Trace a resolved pin forward to its live counterpart at HEAD.
///
/// The precision ladder, best first.
/// 1. the exact pinned bytes still at the recorded path, `Present`.
/// 2. a `Moved:` trailer chain from the pinned commit to HEAD, recorded rather
/// than inferred (but unauthenticated, see [`TraceTier::Trailer`]).
/// 3. the pinned blob's exact oid present once in HEAD, certain (out-of-band move).
/// 4. the oid present several times, `Ambiguous`, a candidate set, never a fact.
/// 5. else `Gone`.
pub(crate) fn forward_trace(
    repo: &Path,
    resolution: &PinnedResolution,
) -> Result<ForwardTrace, PinnedResolutionError> {
    // The EXACT pinned bytes still sit at the recorded path: dormant, not drifted.
    // Comparing the oid, not mere existence, so a path reoccupied by a different
    // file falls through to the trace instead of falsely reading as present.
    if ShellGit
        .blob_oid_at_commit(repo, "HEAD", &resolution.path)
        .map_err(PinnedResolutionError::Git)?
        .as_deref()
        == Some(resolution.oid.as_str())
    {
        return Ok(ForwardTrace::Present);
    }

    // Tier 1: replay the engine's own `Moved:` records, chaining by path AND by
    // reachability. Authoritative, the engine recorded the rename instead of
    // inferring it.
    //
    // A path is not an identity. It is an identity only while it is occupied, so
    // a hop may extend the chain only when its commit descends from the hop
    // before it. Chaining on the path alone lets a path vacated out of band and
    // reoccupied by an unrelated file pick up the newcomer's later mediated move,
    // and report that unrelated file at the strongest tier.
    //
    // The whole replay is gated on the pinned commit being an ancestor of HEAD.
    // `commit..HEAD` on an unmerged or rewritten history is not this pin's past
    // at all; it yields everything reachable from HEAD, so unrelated moves would
    // replay as if they were this chain's. Not an ancestor means tier 1 has
    // nothing to say, and the oid tiers below still answer.
    // Each hop is checked by TWO witnesses, order and content, because neither
    // alone is enough.
    // - ORDER catches the concurrent case: two branches independently move a
    //   different file onto one path. They are incomparable, so no list order can
    //   separate them, and reachability must.
    // - CONTENT catches the sequential case: a path vacated out of band, then
    //   reoccupied, then moved by us. That history is perfectly linear, so
    //   ancestry accepts it; only the bytes reveal the occupant changed.
    // A path is not an identity. It is an identity only while it is occupied.
    let mut chained = false;
    let mut current = resolution.path.clone();
    if ShellGit
        .is_ancestor(repo, &resolution.commit, "HEAD")
        .map_err(PinnedResolutionError::Git)?
    {
        let moves = ShellGit
            .moved_trailers_since(repo, &resolution.commit)
            .map_err(PinnedResolutionError::Git)?;
        // Where the chain stands: the commit it reached, and the bytes it carried
        // there. Both start at the pin and advance only on an accepted hop.
        let mut at = resolution.commit.clone();
        let mut oid = resolution.oid.clone();
        for mv in &moves {
            if mv.from != current {
                continue;
            }
            if !ShellGit
                .is_ancestor(repo, &at, &mv.commit)
                .map_err(PinnedResolutionError::Git)?
            {
                continue;
            }
            // Is the thing being moved still OURS? Two ways to be satisfied, and
            // byte-equality alone is not one of them: a pinned file is normally
            // EDITED between the pin and a later rename, which is the commonest
            // history there is and exactly what a provenance pin generates.
            // Refusing on changed bytes would answer `Gone` about a file sitting
            // at a known path, the same confident-wrong shape the `Moved:` trailer
            // exists to prevent.
            //
            // `mv.commit^` reads the parent. A `Moved:` record is trusted only on
            // an engine-authored commit, and the saga writes those with one
            // parent, but the trailer gate now accepts an indented body so a
            // `merge --squash` fold reaches here too. A squash commit also has one
            // parent, so `^` is still unambiguous; the durability of a trailer
            // through history rewriting is its own question, see the
            // trailer-durability todo.
            let before = ShellGit
                .blob_oid_at_commit(repo, &format!("{}^", mv.commit), &mv.from)
                .map_err(PinnedResolutionError::Git)?;
            let unchanged = before.as_deref() == Some(oid.as_str());
            // OCCUPANCY, the general rule: the path was never VACATED between
            // where the chain stands and this move. A path that was continuously
            // occupied held one identity throughout, whatever its bytes did, so
            // the move is ours. A path that was deleted and later re-taken is a
            // different file wearing the same name.
            //
            // Sound but incomplete, deliberately, and there are TWO residues.
            //
            // First, a vacate-and-retake inside ONE commit records as a plain
            // modification, indistinguishable from an edit, and that case is
            // already known to be undecidable from trees alone.
            //
            // Second, `path_deleted_between` inherits git's default history
            // simplification, so a deletion on a side branch whose merge result
            // matches the other parent is not reported. `--full-history` would
            // report it, and is NOT used: it also surfaces deletions on discarded
            // branches, trading a wrong `Moved` for a wrong `Gone`, and the
            // paragraph above is the reason a wrong `Gone` is the worse of the
            // two. So the tradeoff is chosen here rather than inherited from a
            // git default.
            //
            // Both residues cost a wrong `Moved`; refusing on changed bytes cost
            // a wrong `Gone` on the commonest history there is.
            let occupied = unchanged
                || !ShellGit
                    .path_deleted_between(repo, &at, &format!("{}^", mv.commit), &mv.from)
                    .map_err(PinnedResolutionError::Git)?;
            if !occupied {
                continue;
            }
            // The bytes after the move, so a later hop compares against what the
            // chain actually carries. Our own renames preserve content, so this is
            // normally unchanged; reading it keeps the chain honest if that ever
            // stops holding.
            let after = ShellGit
                .blob_oid_at_commit(repo, &mv.commit, &mv.to)
                .map_err(PinnedResolutionError::Git)?;
            let Some(after) = after else {
                // The record claims a destination the commit does not hold. The
                // record is wrong, so the chain stops rather than guessing.
                break;
            };
            current = mv.to.clone();
            at = mv.commit.clone();
            oid = after;
            chained = true;
        }
    }
    if chained
        && ShellGit
            .read_at_commit(repo, "HEAD", &current)
            .map_err(PinnedResolutionError::Git)?
            .is_some()
    {
        return Ok(ForwardTrace::Moved {
            to: current,
            tier: TraceTier::Trailer,
        });
    }

    // Tier 2: the exact bytes, found by oid in HEAD's tree. One match is a certain
    // out-of-band same-byte move; several mean the bytes are duplicated, so the
    // move is not certain, surface the candidates rather than guess.
    let mut paths = ShellGit
        .find_paths_by_oid(repo, "HEAD", &resolution.oid)
        .map_err(PinnedResolutionError::Git)?;
    match paths.len() {
        0 => {}
        1 => {
            return Ok(ForwardTrace::Moved {
                to: paths.pop().expect("one match"),
                tier: TraceTier::ExactOid,
            });
        }
        _ => return Ok(ForwardTrace::Ambiguous(paths)),
    }

    Ok(ForwardTrace::Gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        dir
    }

    fn commit_all(repo: &Path, msg: &str) -> String {
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", msg]);
        run(repo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn resolves_a_relative_path_at_the_commit() {
        let repo = init_repo();
        let p = repo.path();
        fs::create_dir_all(p.join("notes")).unwrap();
        fs::write(p.join("notes/draft.md"), "one\n").unwrap();
        let c1 = commit_all(p, "v1");

        let r = PinnedResolver::new();
        let res = r.resolve(p, &c1, "notes/draft.md").unwrap();
        assert_eq!(res.path, PathBuf::from("notes/draft.md"));
        assert!(!res.oid.is_empty());
        assert_eq!(r.read_bytes(p, &res).unwrap(), b"one\n");
    }

    #[test]
    fn resolves_a_bare_name_at_the_commit() {
        let repo = init_repo();
        let p = repo.path();
        fs::create_dir_all(p.join("notes")).unwrap();
        fs::write(p.join("notes/draft.md"), "one\n").unwrap();
        let c1 = commit_all(p, "v1");

        // A bare name, no path, resolves through the per-commit name index.
        let r = PinnedResolver::new();
        let res = r.resolve(p, &c1, "draft").unwrap();
        assert_eq!(res.path, PathBuf::from("notes/draft.md"));
    }

    #[test]
    fn the_pin_is_deletion_stable() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("touched.md"), "the touched bytes\n").unwrap();
        let c1 = commit_all(p, "v1");
        // The live file is deleted in a later commit.
        fs::remove_file(p.join("touched.md")).unwrap();
        commit_all(p, "delete");

        // The pin still resolves to c1's bytes, the whole point.
        let r = PinnedResolver::new();
        let res = r.resolve(p, &c1, "touched").unwrap();
        assert_eq!(r.read_bytes(p, &res).unwrap(), b"the touched bytes\n");
    }

    #[test]
    fn an_absent_target_is_path_absent() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("real.md"), "x\n").unwrap();
        let c1 = commit_all(p, "v1");

        let r = PinnedResolver::new();
        match r.resolve(p, &c1, "ghost") {
            Err(PinnedResolutionError::PathAbsent) => {}
            other => panic!("expected PathAbsent, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_commit_is_commit_unavailable() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("real.md"), "x\n").unwrap();
        commit_all(p, "v1");

        let r = PinnedResolver::new();
        match r.resolve(p, "0000000000000000000000000000000000000000", "real") {
            Err(PinnedResolutionError::CommitUnavailable) => {}
            other => panic!("expected CommitUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_name_matching_several_paths_is_ambiguous() {
        let repo = init_repo();
        let p = repo.path();
        fs::create_dir_all(p.join("a")).unwrap();
        fs::create_dir_all(p.join("b")).unwrap();
        fs::write(p.join("a/dup.md"), "a\n").unwrap();
        fs::write(p.join("b/dup.md"), "b\n").unwrap();
        let c1 = commit_all(p, "v1");

        let r = PinnedResolver::new();
        match r.resolve(p, &c1, "dup") {
            Err(PinnedResolutionError::Ambiguous(matches)) => assert_eq!(matches.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn forward_trace_reports_present_when_the_path_survives() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("note.md"), "x\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "note").unwrap();
        assert_eq!(forward_trace(p, &res).unwrap(), ForwardTrace::Present);
    }

    #[test]
    fn forward_trace_follows_a_moved_trailer() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("old.md"), "original\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "old.md").unwrap();
        // Rename with EDITED content, so only the trailer, not the exact oid, can locate it.
        // The commit carries a Mutation-Id, so the trailer is engine-authored and trusted.
        fs::remove_file(p.join("old.md")).unwrap();
        fs::write(p.join("new.md"), "edited after the move\n").unwrap();
        commit_msg(p, "rename\n\nMutation-Id: m-1\nMoved: old.md -> new.md");
        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("new.md"),
                tier: TraceTier::Trailer,
            }
        );
    }

    #[test]
    fn forward_trace_follows_a_move_of_a_file_edited_since_the_pin() {
        // The COMMON history, and the one a provenance pin generates constantly:
        // pin a file, edit it in its own commit, rename it later. The bytes at the
        // source path no longer match the pin, so a byte-equality witness refuses
        // the hop and answers `Gone` about a file sitting at a known path — a
        // confident wrong claim, and the exact shape the `Moved:` trailer exists
        // to prevent. Occupancy accepts it: the path was never vacated.
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("old.md"), "original\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "old.md").unwrap();

        fs::write(p.join("old.md"), "edited in its own commit\n").unwrap();
        commit_all(p, "edit");

        fs::rename(p.join("old.md"), p.join("new.md")).unwrap();
        commit_msg(p, "rename\n\nMutation-Id: m-1\nMoved: old.md -> new.md");

        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("new.md"),
                tier: TraceTier::Trailer,
            }
        );
    }

    #[test]
    fn forward_trace_does_not_chain_onto_a_reoccupied_paths_later_move() {
        // The OCCUPANCY witness. History here is perfectly linear, so reachability
        // accepts every hop; only the path's occupancy reveals that the file we
        // would follow is not ours.
        //
        // The vacate and the retake are SEPARATE commits, which is what makes this
        // decidable: git records a same-commit vacate-and-retake as a plain
        // modification, indistinguishable from an edit. See the sibling test.
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "the pinned content\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "a.md").unwrap();

        // Out of band, with no trailer: our file moves away, and is edited, so the
        // oid tiers cannot find it either.
        fs::remove_file(p.join("a.md")).unwrap();
        fs::write(p.join("elsewhere.md"), "the pinned content, edited\n").unwrap();
        commit_all(p, "out-of-band move away");

        // Then an unrelated file takes the vacated path.
        fs::write(p.join("a.md"), "a completely unrelated newcomer\n").unwrap();
        commit_all(p, "an unrelated file takes the name");

        // Later, WE move the newcomer, and record it faithfully.
        fs::rename(p.join("a.md"), p.join("z.md")).unwrap();
        commit_msg(p, "rename\n\nMutation-Id: m-1\nMoved: a.md -> z.md");

        // Chaining by path alone answers `Moved { to: z.md, tier: Trailer }`, full
        // confidence in a file with no relationship to the pin. `a.md` was deleted
        // between the pin and the move, so the hop is refused.
        assert_eq!(forward_trace(p, &res).unwrap(), ForwardTrace::Gone);
    }

    #[test]
    fn a_same_commit_retake_is_undecidable_and_chains() {
        // The KNOWN residue, asserted so it is a recorded limit rather than a
        // surprise. A vacate-and-retake inside ONE commit records as a plain
        // modification, which no tree comparison distinguishes from an edit. So
        // occupancy accepts the hop and the trace follows the newcomer.
        //
        // The alternative is refusing every changed-bytes hop, which breaks the
        // common edit-then-rename case above. A wrong `Moved` here is the price of
        // not answering a wrong `Gone` there, and this shape needs an out-of-band
        // move to arise at all.
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "the pinned content\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "a.md").unwrap();

        // Both the vacate and the retake in ONE commit.
        fs::write(p.join("elsewhere.md"), "the pinned content, edited\n").unwrap();
        fs::write(p.join("a.md"), "a completely unrelated newcomer\n").unwrap();
        commit_all(p, "out-of-band shuffle, one commit");

        fs::rename(p.join("a.md"), p.join("z.md")).unwrap();
        commit_msg(p, "rename\n\nMutation-Id: m-1\nMoved: a.md -> z.md");

        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("z.md"),
                tier: TraceTier::Trailer,
            },
            "the undecidable case changed shape; if it is now decidable, say so here"
        );
    }

    #[test]
    fn forward_trace_does_not_chain_across_incomparable_branches() {
        // The ORDER witness. Two branches each move a different file onto `b.md`,
        // and both moves are genuine and engine-authored. The commits are
        // incomparable, so every linearization puts one before the other and no
        // list order can separate them; reachability can.
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "the pinned content\n").unwrap();
        fs::write(p.join("x.md"), "unrelated\n").unwrap();
        let base = commit_all(p, "base");
        let res = PinnedResolver::new().resolve(p, &base, "a.md").unwrap();

        // Branch one: our file moves to b.md.
        run(p, &["checkout", "-q", "-b", "one"]);
        fs::remove_file(p.join("a.md")).unwrap();
        fs::write(p.join("b.md"), "the pinned content\n").unwrap();
        commit_msg(p, "move a\n\nMutation-Id: m-1\nMoved: a.md -> b.md");

        // Branch two, from the same base: the unrelated file takes b.md, then
        // moves on to c.md. Both recorded, both true on this branch.
        run(p, &["checkout", "-q", "-b", "two", &base]);
        fs::remove_file(p.join("x.md")).unwrap();
        fs::write(p.join("b.md"), "unrelated\n").unwrap();
        commit_msg(p, "move x\n\nMutation-Id: m-2\nMoved: x.md -> b.md");
        fs::remove_file(p.join("b.md")).unwrap();
        fs::write(p.join("c.md"), "unrelated, edited\n").unwrap();
        commit_msg(p, "move b\n\nMutation-Id: m-3\nMoved: b.md -> c.md");

        run(p, &["checkout", "-q", "one"]);
        run(p, &["merge", "-q", "--no-edit", "two"]);

        // Replaying in log order chains a.md -> b.md -> c.md and answers c.md at
        // the strongest tier. The b.md -> c.md hop is incomparable with the
        // hop before it, so it is refused, and the answer is the truth: b.md.
        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("b.md"),
                tier: TraceTier::Trailer,
            }
        );
    }

    #[test]
    fn forward_trace_ignores_a_moved_trailer_on_a_non_engine_commit() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("old.md"), "original\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "old.md").unwrap();
        // A human commit FALSELY claims old.md moved to real.md (which exists,
        // with unrelated content). No Mutation-Id, so the claim is not trusted.
        fs::remove_file(p.join("old.md")).unwrap();
        fs::write(p.join("real.md"), "unrelated content\n").unwrap();
        commit_msg(p, "human edit\n\nMoved: old.md -> real.md");
        // The spurious move is ignored; old.md's bytes are nowhere, so Gone, not
        // a confident (wrong) Moved to real.md.
        assert_eq!(forward_trace(p, &res).unwrap(), ForwardTrace::Gone);
    }

    #[test]
    fn forward_trace_is_ambiguous_when_the_oid_is_duplicated() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "dup\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "a.md").unwrap();
        // The pinned file is gone, but its exact bytes now live at two paths.
        fs::remove_file(p.join("a.md")).unwrap();
        fs::write(p.join("b.md"), "dup\n").unwrap();
        fs::write(p.join("c.md"), "dup\n").unwrap();
        commit_all(p, "two identical copies");
        match forward_trace(p, &res).unwrap() {
            ForwardTrace::Ambiguous(mut paths) => {
                paths.sort();
                assert_eq!(paths, vec![PathBuf::from("b.md"), PathBuf::from("c.md")]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn forward_trace_does_not_report_present_when_the_path_is_reoccupied() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "original\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "a.md").unwrap();
        // a.md moves to b.md (engine trailer), and a NEW a.md reoccupies the path.
        fs::write(p.join("b.md"), "original\n").unwrap();
        fs::write(p.join("a.md"), "a brand new file at the old path\n").unwrap();
        commit_msg(
            p,
            "rename and reuse\n\nMutation-Id: m-1\nMoved: a.md -> b.md",
        );
        // The path resolves at HEAD, but its oid differs, so it is not Present;
        // the trailer leads to the real new home.
        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("b.md"),
                tier: TraceTier::Trailer,
            }
        );
    }

    #[test]
    fn forward_trace_finds_an_out_of_band_same_byte_move_by_oid() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("old.md"), "same bytes\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "old.md").unwrap();
        // Out-of-band move: identical bytes, NO Moved: trailer.
        fs::remove_file(p.join("old.md")).unwrap();
        fs::write(p.join("moved.md"), "same bytes\n").unwrap();
        commit_all(p, "out of band move");
        assert_eq!(
            forward_trace(p, &res).unwrap(),
            ForwardTrace::Moved {
                to: PathBuf::from("moved.md"),
                tier: TraceTier::ExactOid,
            }
        );
    }

    #[test]
    fn forward_trace_reports_gone_when_the_content_vanishes() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("old.md"), "unique gone bytes\n").unwrap();
        let c1 = commit_all(p, "v1");
        let res = PinnedResolver::new().resolve(p, &c1, "old.md").unwrap();
        fs::remove_file(p.join("old.md")).unwrap();
        commit_all(p, "delete");
        assert_eq!(forward_trace(p, &res).unwrap(), ForwardTrace::Gone);
    }

    fn commit_msg(repo: &Path, msg: &str) -> String {
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", msg]);
        run(repo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn the_name_index_is_cached_per_commit() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("real.md"), "x\n").unwrap();
        let c1 = commit_all(p, "v1");

        let r = PinnedResolver::new();
        let _ = r.resolve(p, &c1, "real").unwrap();
        // A second resolve hits the cache; correctness is identical.
        let res = r.resolve(p, &c1, "real").unwrap();
        assert_eq!(res.path, PathBuf::from("real.md"));
        assert_eq!(
            r.name_index.lock().unwrap().len(),
            1,
            "one commit should yield one cached index"
        );
    }
}
