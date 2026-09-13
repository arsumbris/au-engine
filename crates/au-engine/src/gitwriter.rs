//! The git write side per
//! [[spec - git write path - commit-per-mutation as a local saga over the workspace's materialized repos]].
//!
//! One commit per accepted mutation. Every method is scoped to exactly the
//! paths it is given, so a mutation never disturbs unrelated work in the same
//! working tree. The trait is the contract; the adapter is swappable — shell-git
//! today, a plumbing-write adapter later for the materialization spectrum.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repo::RepoName;

/// A mutation's correlation id.
///
/// Stamped into every commit of the mutation as a `Mutation-Id` trailer, so the
/// N commits of a multi-repo saga are one logical mutation, and crash recovery
/// can group them.
///
/// Deliberately NOT `Default`. An empty id emits a bare `Mutation-Id: `, which
/// groups every defaulted commit under one blank id and matches nothing a lookup
/// searches for. The id is an enforced invariant of a mutation commit, so the
/// type does not offer a way to construct one without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MutationId(pub String);

/// A git commit sha, returned by `commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitSha(pub String);

/// A move or rename this commit performed, emitted as a `Moved:` trailer.
///
/// The strongest tier of the pinned-reference forward trace, recorded rather
/// than authenticated (see `TraceTier::Trailer`): git cannot
/// reconstruct an edited-and-moved file at diff time, so the engine records its
/// own moves at the only point it knows them. Paths are relative to the
/// COMMITTING WORKING TREE, the basis every path handed to a `GitWriter` takes.
///
/// The trailer is line-based, `Moved: <from> -> <to>`, so a path must not contain
/// a newline or the ` -> ` delimiter, it could not round-trip. A robust encoding
/// (NUL / quoting) is a follow-on, tracked with the trace's consumer surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MovedRecord {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// One `Moved:` record read back OUT of history, with the commit it rode in.
///
/// The read counterpart of [`MovedRecord`], and deliberately a separate type: a
/// record being WRITTEN belongs to a commit that does not exist yet, so it can
/// carry no sha, while a record being READ always has one. Folding them into a
/// nullable field would let the write side's structural absence and a read-side
/// failure wear the same shape.
///
/// The commit is what makes a chain of moves checkable. A path is not an
/// identity; it is an identity only while it is occupied, so a hop may only
/// extend a chain when its commit is a descendant of the previous hop's. Without
/// the sha, a path vacated out of band and reoccupied by an unrelated file
/// chains onto the newcomer's later move and reports it at the strongest
/// tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TracedMove {
    pub from: PathBuf,
    pub to: PathBuf,
    /// The commit whose message carried this `Moved:` trailer.
    pub commit: String,
}

/// One trailer line parsed off a commit message, `Key: value`.
///
/// A [`CommitMetaRecord`] surfaces every trailer, the engine's own
/// (`Mutation-Id`, `Moved:`, ...) and a caller-supplied attribution line alike,
/// uninterpreted. Interpreting a `session` or a `span` is the consumer's, the
/// engine stays domain-pure, see
/// [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitTrailer {
    pub key: String,
    pub value: String,
}

/// One commit's metadata, the record `commit_meta` returns per requested sha.
///
/// Positional: one record per input commit, in input order, so a consumer maps
/// a record back to its query by position. An abbreviated input resolves to the
/// full `commit` oid. An absent commit is `available: false` with every other
/// field `None`, the shape of `pinned-commit-unavailable`, never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitMetaRecord {
    /// The resolved full oid when present, else the requested input verbatim.
    pub commit: String,
    pub available: bool,
    /// Committer date, unix seconds, when the commit entered history.
    pub timestamp: Option<i64>,
    /// The author, `Name <email>`.
    pub author: Option<String>,
    /// The raw commit message, summary and body.
    pub message: Option<String>,
    /// Every trailer line, uninterpreted.
    pub trailers: Vec<CommitTrailer>,
}

/// One commit in a file's history, the record `file_history` returns per row.
///
/// A raw pass-through of `git log --name-status -M --follow`: `status` and
/// `from` are git's own similarity heuristic, best-effort, NOT authoritative.
/// Adjudicating a rename is a consumer's lineage tool, not this read. See
/// [[spec - engine-mediated git reads - member-aware commit metadata and file history over the object store]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileHistoryRecord {
    /// The commit that touched the path, full oid.
    pub commit: String,
    /// Committer date, unix seconds.
    pub timestamp: i64,
    /// The author, `Name <email>`.
    pub author: String,
    /// The commit SUBJECT line (the first line). The full body is a
    /// `commit_meta` join away, kept off the stream so the log parse stays
    /// single-line per commit.
    pub message: String,
    /// The change kind, `added` / `modified` / `deleted` / `renamed`.
    pub status: &'static str,
    /// The prior path on a `renamed`, git's heuristic match; absent otherwise.
    pub from: Option<String>,
}

/// One changed file in a commit, for the `recent_commits` stream.
///
/// Git's `--name-status -M` output per file: a status letter and the path, a
/// rename or copy carrying the prior path too. Best-effort, git's own
/// similarity heuristic, surfaced raw like [`FileHistoryRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangedFile {
    /// The changed path, the NEW path on a rename, relative to the tree root.
    pub path: String,
    /// The change kind, `added` / `modified` / `deleted` / `renamed`.
    pub status: &'static str,
    /// The prior path on a `renamed`, git's heuristic match; absent otherwise.
    pub from: Option<String>,
}

/// One commit in the `recent_commits` stream, from one working tree.
///
/// A raw pass-through of `git log --name-status -M`, no path filter and no
/// `--follow`, so a commit carries its full changed-file list. Unlike
/// [`FileHistoryRecord`] it also carries the parsed trailers, so a consumer
/// collapses a mutation's cross-tree commits client-side without a `commit_meta`
/// join. The owning tree root and its member names are tagged at the serve
/// layer, not here. See
/// [[spec - recent-commits activity stream - a member-aware bounded commit stream with a reflog-watched append-only subscription]].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentCommitRecord {
    /// The commit, full oid.
    pub commit: String,
    /// Committer date, unix seconds.
    pub timestamp: i64,
    /// The author name and email, split so a consumer can distinguish a human
    /// commit from an engine one (`au-engine <au-engine@arsumbris.ai>`).
    ///
    /// Author identity is paired with the committer date (`timestamp`); under
    /// the append-only invariant (no rebase / amend) the author and committer
    /// coincide, so the pairing is faithful, not a mix of two commits.
    pub author_name: String,
    pub author_email: String,
    /// The commit SUBJECT line (the first line); the full body is a
    /// `commit_meta` join away.
    pub subject: String,
    /// The files the commit changed, git's `--name-status -M` per file. Empty
    /// for a merge commit, whose diff git omits by default.
    pub changed_files: Vec<ChangedFile>,
    /// Every trailer line, uninterpreted, the same parse `commit_meta` returns.
    pub trailers: Vec<CommitTrailer>,
}

/// The message a mutation commit carries.
///
/// `summary` is the human line. `mutation_id`, `members`, any `moves`, and a
/// `reverts` back-reference are emitted as trailers. Crash recovery reads the
/// first two to group a saga's commits and learn every repo it touched; the
/// forward trace reads `Moved:`.
///
/// Not `Default`, for the reason [`MutationId`] is not: a defaulted message
/// carries no id, and every construction site names all its fields explicitly so
/// a field added later cannot be absorbed silently into a contract type.
#[derive(Debug, Clone)]
pub(crate) struct CommitMessage {
    pub summary: String,
    pub mutation_id: MutationId,
    pub members: Vec<RepoName>,
    pub moves: Vec<MovedRecord>,
    /// The commit this one compensates, emitted as `Reverts: <sha>`.
    ///
    /// Only a saga's compensating commit sets it. It says WHY this commit's
    /// `Moved:` records run backwards, so a reader sees a rollback rather than an
    /// ordinary rename, and can discard the reverted mutation's edges if it wants
    /// more than the net answer the counter-records already give.
    pub reverts: Option<String>,
    /// A caller-supplied opaque attribution payload, emitted as trailers beside
    /// `Mutation-Id`. The engine NEVER interprets it, so a `session` or a `span`
    /// stays a caller concept and the engine stays domain-pure. Validated
    /// against the reserved keys before it reaches here, see
    /// [`validate_attribution`]. Empty for an engine-internal commit (a
    /// compensation, a package write).
    pub attribution: Vec<CommitTrailer>,
}

/// The trailer keys the engine reserves for its own records. A caller
/// attribution payload may not use one, so it cannot forge an engine record
/// through the attribution slot. Compared case-insensitively.
pub(crate) const RESERVED_TRAILER_KEYS: &[&str] = &[
    "Mutation-Id",
    "Mutation-Members",
    "Moved",
    "Reverts",
    "Traced",
];

/// Validate a caller attribution payload into commit trailers, or reject.
///
/// A key must be a well-formed trailer token (non-empty, no `:`, no newline)
/// and not collide with a reserved engine key; a value must carry no newline.
/// The engine writes the payload verbatim but never interprets it, so this only
/// guards the FORM and the reserved namespace, never the meaning.
pub(crate) fn validate_attribution(
    input: &[(String, String)],
) -> Result<Vec<CommitTrailer>, String> {
    let mut out = Vec::with_capacity(input.len());
    for (key, value) in input {
        let k = key.trim();
        if k.is_empty() {
            return Err("attribution key is empty".to_string());
        }
        if k.contains(':') || k.contains('\n') || value.contains('\n') {
            return Err(format!(
                "attribution '{k}' is not a well-formed trailer (no ':' or newline in a key, no newline in a value)"
            ));
        }
        if RESERVED_TRAILER_KEYS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(k))
        {
            return Err(format!(
                "attribution key '{k}' is reserved by the engine ({}); a caller cannot forge an engine trailer",
                RESERVED_TRAILER_KEYS.join(" / ")
            ));
        }
        out.push(CommitTrailer {
            key: k.to_string(),
            value: value.trim().to_string(),
        });
    }
    Ok(out)
}

/// One `-z`-framed path from git, decoded without loss where the platform allows.
///
/// A pathspec built from a lossily-decoded path matches nothing, so the
/// compensating commit would silently under-scope, which is the failure mode the
/// scoping exists to prevent. `-z` keeps the bytes intact, so on unix the path
/// round-trips exactly; elsewhere paths are unicode anyway and the lossy decode
/// cannot lose anything.
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// A failed git write. The message is human-facing context.
///
/// A failure leaves the call's intent unrealized; the saga compensates the
/// repos that did change.
#[derive(Debug)]
pub(crate) struct GitWriteError {
    pub message: String,
}

impl GitWriteError {
    pub fn new(message: impl Into<String>) -> Self {
        GitWriteError {
            message: message.into(),
        }
    }
}

/// The engine's git write side, one owning daemon per repo.
///
/// The saga and its compensation contract sit above this port; the mechanism
/// below is swappable. Every method targets one repo's working tree and is
/// scoped to the paths it is handed.
pub(crate) trait GitWriter {
    /// Stage exactly `paths` and commit them, returning the new commit sha.
    ///
    /// Hooks are skipped — a human's pre-commit hooks do not gate mediated
    /// mutations. Staging is path-scoped: a human's unrelated staged or dirty
    /// work is left in place. `message` supplies the summary and the trailers.
    fn commit(
        &self,
        repo: &Path,
        paths: &[PathBuf],
        message: &CommitMessage,
    ) -> Result<CommitSha, GitWriteError>;

    /// Add a compensating commit that reverts the commit carrying `mutation_id`.
    ///
    /// Addressed by the trailer, not by position, so it is idempotent and
    /// race-free against a concurrent human commit. The committed half of saga
    /// compensation.
    fn revert_commit(&self, repo: &Path, mutation_id: &MutationId) -> Result<(), GitWriteError>;

    /// Restore exactly `paths` to their HEAD state, discarding their
    /// working-tree changes.
    ///
    /// A path absent at HEAD is removed. Scoped: any other path's staged or
    /// dirty state is untouched. The written-but-not-committed half of saga
    /// compensation.
    fn restore_paths(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), GitWriteError>;

    /// Whether every path in `paths` is clean, equal to HEAD, with no
    /// uncommitted change.
    ///
    /// The clean-at-HEAD precondition reads this before a mutation applies. A
    /// path absent both at HEAD and in the working tree is clean.
    fn is_clean(&self, repo: &Path, paths: &[PathBuf]) -> Result<bool, GitWriteError>;
}

/// The engine commit identity, used when the repo configures none.
/// Richer per-originator identity is future provenance work.
const COMMIT_AUTHOR_NAME: &str = "au-engine";
const COMMIT_AUTHOR_EMAIL: &str = "au-engine@arsumbris.ai";

/// `index.lock` contention retries before giving up.
const LOCK_RETRIES: u32 = 10;

/// The shell-git `GitWriter`, shelling out to `git -C <repo>`.
///
/// It does exactly what a human's git does, so it handles `index.lock` and the
/// working tree natively. The paths it is handed are relative to the working
/// tree it is pointed at, which for a monorepo member is an ANCESTOR of that
/// member's own root, not the member root.
pub(crate) struct ShellGit;

impl ShellGit {
    /// Run `git -C <repo> <args>`, retrying on `index.lock` contention.
    /// Returns trimmed stdout on success.
    fn git(&self, repo: &Path, args: &[OsString]) -> Result<String, GitWriteError> {
        let mut tries = 0;
        loop {
            tries += 1;
            let output = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .map_err(|e| GitWriteError::new(format!("could not run git: {e}")))?;
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if tries < LOCK_RETRIES && lock_contended(&stderr) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            return Err(GitWriteError::new(format!(
                "git {} failed: {}",
                display_args(args),
                stderr.trim()
            )));
        }
    }

    /// A query whose non-zero exit is a legitimate "no", not an error.
    fn git_succeeds(&self, repo: &Path, args: &[OsString]) -> Result<bool, GitWriteError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .map_err(|e| GitWriteError::new(format!("could not run git: {e}")))?;
        Ok(output.status.success())
    }

    /// A query returning stdout bytes verbatim. `Ok(None)` on a non-zero exit,
    /// a legitimate "no" (an absent object), `Ok(Some(bytes))` on success.
    /// Bytes are not trimmed or lossily decoded, so a binary blob round-trips.
    #[allow(dead_code)] // wired to the resolver / mutation channel in subsequent slices
    fn git_bytes(&self, repo: &Path, args: &[OsString]) -> Result<Option<Vec<u8>>, GitWriteError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .map_err(|e| GitWriteError::new(format!("could not run git: {e}")))?;
        Ok(output.status.success().then_some(output.stdout))
    }

    /// Run `git -C <repo> <args>` feeding `stdin` to the process, trimmed stdout
    /// on success. For the batch plumbing (`cat-file --batch-check`) that reads
    /// its object list from stdin, one process over a whole set.
    ///
    /// Stdin is written on a SEPARATE thread while `wait_with_output` drains
    /// stdout on this one. Writing the whole input before reading any output
    /// deadlocks once both pipe buffers fill (~64 KB each way, a few-thousand-
    /// commit batch): git blocks writing stdout, we block writing stdin, forever.
    /// A `BrokenPipe` (git exited early on bad input) is not ours to report; the
    /// status check below carries the real failure.
    fn git_stdin(
        &self,
        repo: &Path,
        args: &[OsString],
        stdin: &str,
    ) -> Result<String, GitWriteError> {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| GitWriteError::new(format!("could not run git: {e}")))?;
        let mut si = child
            .stdin
            .take()
            .ok_or_else(|| GitWriteError::new("could not open git stdin"))?;
        let input = stdin.as_bytes().to_vec();
        let writer = std::thread::spawn(move || {
            let _ = si.write_all(&input);
            // `si` drops here, closing git's stdin (EOF).
        });
        let output = child
            .wait_with_output()
            .map_err(|e| GitWriteError::new(format!("git wait failed: {e}")))?;
        let _ = writer.join();
        if !output.status.success() {
            return Err(GitWriteError::new(format!(
                "git {} failed: {}",
                display_args(args),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string())
    }

    fn exists_at_head(&self, repo: &Path, path: &Path) -> Result<bool, GitWriteError> {
        let spec = format!("HEAD:{}", path.to_string_lossy());
        self.git_succeeds(repo, &[os("cat-file"), os("-e"), os(spec)])
    }

    /// The repo's `HEAD` commit, `rev-parse HEAD`, or `None` when `repo` is not
    /// a git working tree (or HEAD is unborn). The pin anchor a `content` read
    /// hands back: the commit the returned working-tree bytes resolve against,
    /// symmetric with a mutation's `result.commit`. A non-git read carries no
    /// commit, so the field is nullable, never an error.
    pub(crate) fn head_commit(&self, repo: &Path) -> Option<String> {
        let sha = self.git(repo, &[os("rev-parse"), os("HEAD")]).ok()?;
        (!sha.is_empty()).then_some(sha)
    }

    /// Whether `commit` resolves to a commit object in this repo's store.
    ///
    /// Distinguishes the two pinned-resolution failures: a false here is
    /// `pinned-commit-unavailable` (the commit is not local), a true with an
    /// absent path is `pinned-path-absent` (a bogus pin). A pin names a commit
    /// that may be absent from a shallow or fresh clone, so this is checked
    /// before reading a path against it.
    #[allow(dead_code)] // wired to the resolver / mutation channel in subsequent slices
    pub(crate) fn commit_present(&self, repo: &Path, commit: &str) -> Result<bool, GitWriteError> {
        let spec = format!("{commit}^{{commit}}");
        self.git_succeeds(
            repo,
            &[os("rev-parse"), os("--verify"), os("--quiet"), os(spec)],
        )
    }

    /// Undo a revert that could not be completed, touching only what it disturbed.
    ///
    /// **Not `git revert --abort`, and the reason is measured rather than
    /// assumed.** `--abort` restores the whole pre-revert state, which DELETES a
    /// human's unrelated staged work: verified on git 2.50.1, a staged `human.md`
    /// vanished from the index and from disk. Using it here would fix the engine
    /// damaging a human's tree by damaging it worse, and it would undo the very
    /// thing the compensating commit's pathspec exists to protect.
    ///
    /// So `--quit` forgets the operation without touching the tree, and
    /// `restore_paths` puts back exactly the paths the revert disturbed. Anything
    /// the revert never touched is left alone.
    ///
    /// BEST-EFFORT, and deliberately silent: every caller is already returning an
    /// error, and a failure to clean up must not replace the error that caused it.
    fn unwind_revert(&self, repo: &Path, scope: &[PathBuf]) {
        let _ = self.git(repo, &[os("revert"), os("--quit")]);
        let _ = GitWriter::restore_paths(self, repo, scope);
    }

    /// The paths a commit touched, both sides of a rename.
    ///
    /// The pathspec a compensating commit must be scoped to. Rename detection is
    /// deliberately OFF, so a rename yields its old AND its new path: reverting
    /// it restores the one and removes the other, and both have to be in scope.
    ///
    /// `-z`, so a path containing a newline cannot truncate the list. Engine
    /// commits are never merges, and `diff-tree` reports nothing for a merge, so
    /// an empty result means the commit changed nothing rather than that this
    /// missed something.
    pub(crate) fn changed_paths(
        &self,
        repo: &Path,
        sha: &str,
    ) -> Result<Vec<PathBuf>, GitWriteError> {
        let out = self.git_bytes(
            repo,
            &[
                os("diff-tree"),
                os("--no-commit-id"),
                os("--name-only"),
                os("-r"),
                os("-z"),
                os(sha),
            ],
        )?;
        let Some(bytes) = out else {
            return Err(GitWriteError::new(format!(
                "could not list the paths changed by {sha}"
            )));
        };
        Ok(bytes
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(bytes_to_path)
            .collect())
    }

    /// The bytes of `path` at `commit`'s tree, `cat-file -p <commit>:<path>`.
    ///
    /// Generalizes `exists_at_head` from existence-at-HEAD to content-at-any-commit.
    /// `Ok(None)` when the path is absent at that commit; call `commit_present`
    /// first to tell an absent path from an absent commit. The commit's tree is
    /// immutable, so a pinned read is stable and cacheable forever. Bytes are
    /// verbatim, so an image or a PDF round-trips.
    #[allow(dead_code)] // wired to the resolver / mutation channel in subsequent slices
    pub(crate) fn read_at_commit(
        &self,
        repo: &Path,
        commit: &str,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, GitWriteError> {
        let spec = format!("{}:{}", commit, path.to_string_lossy());
        self.git_bytes(repo, &[os("cat-file"), os("-p"), os(spec)])
    }

    /// The blob oid of `path` at `commit`, `rev-parse <commit>:<path>`.
    ///
    /// The exact bytes' identity, recorded in the resolution so the version-exact
    /// content resolves on demand and a content-addressed cache can key on it.
    /// `Ok(None)` when the path is absent at that commit.
    #[allow(dead_code)] // wired to the resolver / mutation channel in subsequent slices
    pub(crate) fn blob_oid_at_commit(
        &self,
        repo: &Path,
        commit: &str,
        path: &Path,
    ) -> Result<Option<String>, GitWriteError> {
        let spec = format!("{}:{}", commit, path.to_string_lossy());
        let out = self.git_bytes(
            repo,
            &[os("rev-parse"), os("--verify"), os("--quiet"), os(spec)],
        )?;
        Ok(out.and_then(|bytes| {
            let oid = String::from_utf8_lossy(&bytes).trim().to_string();
            (!oid.is_empty()).then_some(oid)
        }))
    }

    /// The repo-relative paths in `commit`'s tree, `ls-tree -r --name-only -z`.
    ///
    /// NUL-separated so `-z` disables git's path quoting and an odd filename
    /// survives. The basis for the per-commit name index a bare-name pin
    /// resolves through, built once and cached because the tree is immutable.
    #[allow(dead_code)] // wired to the resolver / mutation channel in subsequent slices
    pub(crate) fn list_tree_at_commit(
        &self,
        repo: &Path,
        commit: &str,
    ) -> Result<Vec<PathBuf>, GitWriteError> {
        let out = self.git_bytes(
            repo,
            &[
                os("ls-tree"),
                os("-r"),
                os("--name-only"),
                os("-z"),
                os(commit),
            ],
        )?;
        let Some(bytes) = out else {
            return Ok(Vec::new());
        };
        Ok(String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    /// Metadata for a set of commits in one store, off the build path.
    ///
    /// Two processes per store, never one per commit: on macOS the fork-and-exec
    /// fan-out dominates git's own work.
    /// Pass 1, `cat-file --batch-check`, resolves each input to its full oid and
    /// its presence, positionally. Pass 2, `git log`, reads the fields for the
    /// present set. The result is positional, one record per input in order, so
    /// an absent commit is `available: false` rather than dropped, keeping a
    /// consumer's index-into-the-request valid.
    ///
    /// `repo` is the OWNING member's store; the caller routes each commit to its
    /// member before calling, the member-awareness the read is built for.
    pub(crate) fn commit_meta(
        &self,
        repo: &Path,
        commits: &[String],
    ) -> Result<Vec<CommitMetaRecord>, GitWriteError> {
        if commits.is_empty() {
            return Ok(Vec::new());
        }

        // Pass 1: presence + full-oid resolution, positional, one process.
        // `--batch-check` emits one line per input line, same order:
        //   present commit  -> "<oid> commit <size>"
        //   present non-commit / missing / ambiguous -> not "<oid> commit ...".
        let probe = format!("{}\n", commits.join("\n"));
        let checked = self.git_stdin(repo, &[os("cat-file"), os("--batch-check")], &probe)?;
        let resolved: Vec<Option<String>> = checked
            .lines()
            .map(|line| {
                let mut it = line.split(' ');
                let oid = it.next().unwrap_or("");
                let kind = it.next().unwrap_or("");
                (kind == "commit").then(|| oid.to_string())
            })
            .collect();

        // Pass 2: metadata for the present oids, one process, keyed by `%H`.
        let present: Vec<&String> = resolved.iter().flatten().collect();
        let fields = if present.is_empty() {
            HashMap::new()
        } else {
            let mut args = vec![os("log"), os("--no-walk"), os(COMMIT_META_FORMAT)];
            args.extend(present.iter().map(|oid| os(oid.as_str())));
            parse_commit_meta_log(&self.git(repo, &args)?)
        };

        // Stitch positionally against the original request.
        Ok(commits
            .iter()
            .zip(resolved)
            .map(|(input, oid)| match oid {
                Some(full) => {
                    let f = fields.get(&full);
                    CommitMetaRecord {
                        commit: full.clone(),
                        available: true,
                        timestamp: f.and_then(|f| f.timestamp),
                        author: f.map(|f| f.author.clone()),
                        message: f.map(|f| f.message.clone()),
                        trailers: f.map(|f| f.trailers.clone()).unwrap_or_default(),
                    }
                }
                None => CommitMetaRecord {
                    commit: input.clone(),
                    available: false,
                    timestamp: None,
                    author: None,
                    message: None,
                    trailers: Vec::new(),
                },
            })
            .collect())
    }

    /// A file's commit stream in one store, off the build path.
    ///
    /// Runs `git -C <dir>`, so git resolves the COVERING working tree itself,
    /// the member-awareness a shell-out from the entry dir gets wrong when the
    /// file lives in a member mounted elsewhere. `dir` is the file's own
    /// directory and `file_name` its basename, so the pathspec is relative to
    /// `-C` and `--follow`'s single-path requirement holds.
    ///
    /// A non-git `dir` (a `.git`-free package snapshot) or a path with no
    /// history yields an EMPTY stream, named rather than an error. The rename
    /// column is git's similarity heuristic, surfaced as-is.
    pub(crate) fn file_history(
        &self,
        dir: &Path,
        file_name: &std::ffi::OsStr,
    ) -> Result<Vec<FileHistoryRecord>, GitWriteError> {
        let out = self.git_bytes(
            dir,
            &[
                os("log"),
                os("--follow"),
                os("-M"),
                os("--name-status"),
                os(FILE_HISTORY_FORMAT),
                os("--"),
                os(file_name),
            ],
        )?;
        // `None` is a non-zero exit: a non-git dir, the "empty stream" case.
        let Some(bytes) = out else {
            return Ok(Vec::new());
        };
        Ok(parse_file_history(&String::from_utf8_lossy(&bytes)))
    }

    /// A working tree's recent commits, newest first, off the build path.
    ///
    /// Runs `git -C <dir> log --name-status -M` with no path and no `--follow`,
    /// so git resolves the covering working tree and each commit carries its
    /// full changed-file list. Bounded by `cap` (a `-n` count) and / or `since`
    /// (a git `--since` window); the caller supplies at least one bound. Rows
    /// carry parsed trailers, so a consumer groups a mutation's cross-tree
    /// commits client-side without a `commit_meta` join.
    ///
    /// A non-git `dir` (a `.git`-free package snapshot) yields an EMPTY stream,
    /// named rather than an error, the same as [`ShellGit::file_history`]. The
    /// rename column is git's similarity heuristic, surfaced as-is.
    pub(crate) fn recent_commits(
        &self,
        dir: &Path,
        cap: Option<usize>,
        since: Option<&str>,
    ) -> Result<Vec<RecentCommitRecord>, GitWriteError> {
        let mut args = vec![
            os("log"),
            os("-M"),
            os("--name-status"),
            os(RECENT_COMMITS_FORMAT),
        ];
        if let Some(n) = cap {
            args.push(os(format!("-n{n}")));
        }
        if let Some(s) = since {
            args.push(os(format!("--since={s}")));
        }
        // `None` is a non-zero exit: a non-git dir, the "empty stream" case,
        // the same contract as `file_history`.
        let Some(bytes) = self.git_bytes(dir, &args)? else {
            return Ok(Vec::new());
        };
        Ok(parse_recent_commits(&String::from_utf8_lossy(&bytes)))
    }

    /// Whether `path` was DELETED by any commit in `from..to`.
    ///
    /// The occupancy witness for a move chain: a path continuously occupied held
    /// one identity throughout, whatever its bytes did, while a path deleted and
    /// later re-taken is a different file wearing the same name.
    ///
    /// Sees only a deletion that is its own commit. A vacate-and-retake inside
    /// ONE commit records as a modification, which no tree comparison can tell
    /// from an edit.
    pub(crate) fn path_deleted_between(
        &self,
        repo: &Path,
        from: &str,
        to: &str,
        path: &Path,
    ) -> Result<bool, GitWriteError> {
        let out = self.git_bytes(
            repo,
            &[
                os("log"),
                os("--diff-filter=D"),
                os("--format=%H"),
                os(format!("{from}..{to}")),
                os("--"),
                os(path),
            ],
        )?;
        Ok(out
            .map(|b| !String::from_utf8_lossy(&b).trim().is_empty())
            .unwrap_or(false))
    }

    /// Whether `ancestor` is an ancestor of `descendant`, reflexively (a commit
    /// is its own ancestor, matching `git merge-base --is-ancestor`).
    ///
    /// The DAG-order primitive the move replay needs. Commits are only PARTIALLY
    /// ordered, so no linearization can decide whether one move may follow
    /// another; `git log` order, with or without `--topo-order`, must place
    /// incomparable branches in some sequence and every choice is arbitrary.
    /// Reachability is the real relation, and it is per-pair.
    ///
    /// A non-zero exit means "not an ancestor" OR a git failure, and this cannot
    /// tell them apart. It fails CLOSED, refusing the hop, so the
    /// failure mode is a missing edge rather than a wrong one.
    #[allow(dead_code)] // consumed by the forward trace, which has no callers yet
    pub(crate) fn is_ancestor(
        &self,
        repo: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, GitWriteError> {
        self.git_succeeds(
            repo,
            &[
                os("merge-base"),
                os("--is-ancestor"),
                os(ancestor),
                os(descendant),
            ],
        )
    }

    /// The `Moved:` records in `commit..HEAD`, each with the commit it rode in.
    ///
    /// The strongest forward-trace tier, recorded rather than authenticated.
    /// Reads commit message bodies, the
    /// trailer's canonical home, so it survives daemon restarts and rides every
    /// clone.
    ///
    /// Each record carries its commit, so a caller chains by REACHABILITY rather
    /// than by list order. The order here is `--topo-order --reverse`, which
    /// keeps a parent before its child, but that is a rendering convenience: two
    /// commits on parallel branches are incomparable, and every linearization
    /// puts one first, so order alone can never license a hop. See
    /// [`Self::is_ancestor`].
    ///
    /// Only commits carrying a `Mutation-Id` are trusted, the engine's own. A
    /// human or quoted commit body with a `Moved: a -> b` line is ignored, so it
    /// cannot inject a spurious move, mirroring `find_mutation_sha`'s anchoring.
    #[allow(dead_code)] // consumed by the forward trace, which has no callers yet
    pub(crate) fn moved_trailers_since(
        &self,
        repo: &Path,
        commit: &str,
    ) -> Result<Vec<TracedMove>, GitWriteError> {
        let range = format!("{commit}..HEAD");
        // RS-separated records, each `<sha>\0<body>`, so a `Moved:` line keeps
        // the commit it rode in, and is gated on that commit being engine-authored.
        let out = self.git_bytes(
            repo,
            &[
                os("log"),
                os("--topo-order"),
                os("--reverse"),
                os("--format=%x1e%H%x00%B"),
                os(range),
            ],
        )?;
        let Some(bytes) = out else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut moves = Vec::new();
        for record in text.split('\x1e').filter(|r| !r.is_empty()) {
            let Some((sha, body)) = record.split_once('\0') else {
                continue;
            };
            let sha = sha.trim();
            // Trust `Moved:` only on an engine-authored commit, one carrying a
            // `Mutation-Id` trailer. A foreign commit cannot inject a move.
            //
            // Matched after TRIMMING, because `git merge --squash` indents the
            // bodies it folds in by four spaces. A column-anchored test fails
            // there and drops every move in the range SILENTLY, with no
            // diagnostic and no partial answer. A record read with a
            // less-precise commit attribution is recoverable; one that vanishes
            // is not, and the per-hop content check refuses a hop it cannot
            // verify anyway.
            if !body.lines().any(|l| is_trailer(l, "Mutation-Id: ")) {
                continue;
            }
            for line in body.lines() {
                if let Some(rest) = trailer_value(line, "Moved: ") {
                    if let Some((from, to)) = rest.split_once(" -> ") {
                        moves.push(TracedMove {
                            from: PathBuf::from(from.trim()),
                            to: PathBuf::from(to.trim()),
                            commit: sha.to_string(),
                        });
                    }
                }
            }
        }
        Ok(moves)
    }

    /// Every path in `commit`'s tree whose blob oid equals `oid`.
    ///
    /// The exact-oid forward-trace tier's evidence. One match is a certain move;
    /// several mean the bytes are duplicated, so the caller must not claim a
    /// confident move. `ls-tree -r` lists `<mode> <type> <oid>\t<path>`.
    #[allow(dead_code)] // consumed by the forward trace, surfaced in a later slice
    pub(crate) fn find_paths_by_oid(
        &self,
        repo: &Path,
        commit: &str,
        oid: &str,
    ) -> Result<Vec<PathBuf>, GitWriteError> {
        let out = self.git_bytes(repo, &[os("ls-tree"), os("-r"), os("-z"), os(commit)])?;
        let Some(bytes) = out else {
            return Ok(Vec::new());
        };
        let mut matches = Vec::new();
        for entry in String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter(|e| !e.is_empty())
        {
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            // `<mode> <type> <oid>`
            if meta.split_whitespace().nth(2) == Some(oid) {
                matches.push(PathBuf::from(path));
            }
        }
        Ok(matches)
    }

    /// The sha of THE ONE commit carrying `mutation_id`'s trailer, if the
    /// repo holds it. Addressed by the trailer, not by position.
    ///
    /// The grep is anchored to the whole trailer line (`^…$`), so a prefix id
    /// never matches a longer one — `m-x-1` must not find `m-x-10`. Ids are
    /// base36 plus hyphens, no regex metacharacters, so no escaping is needed;
    /// escape here if the id shape ever broadens.
    ///
    /// One commit per mutation per working tree is an INVARIANT, and this
    /// enforces it rather than assuming it. The saga maintains it by making the
    /// tree its member unit, so several repos inside one tree coalesce into a
    /// single commit. Git does not: a `cherry-pick` copies a
    /// body verbatim, so one id can sit on two commits.
    ///
    /// Several matches is therefore an ERROR, never a pick. Choosing the newest
    /// would revert the copy and report a successful rollback while the original
    /// stayed applied. `a_shared_tree_failure_restores_every_coalesced_path` in
    /// `serve.rs` guards the member-unit half of the same invariant.
    fn find_mutation_sha(
        &self,
        repo: &Path,
        mutation_id: &MutationId,
    ) -> Result<Option<String>, GitWriteError> {
        // Anchored at column 0, DELIBERATELY, and the opposite call from reading
        // a `Moved:` record. Reading is forgiving because a lost record is
        // unrecoverable; this decides a commit may be REVERTED WHOLESALE. An
        // indented trailer means the commit is a `merge --squash` fold carrying
        // other work too, so reverting it would discard changes the mutation
        // never made. Not matching is the safe answer there.
        let pattern = format!("^Mutation-Id: {}$", mutation_id.0);
        // Every match, not `-1`. One commit per mutation per tree is an
        // INVARIANT the saga maintains, not a property of git: a `cherry-pick`
        // copies the body verbatim, so one id can land on two commits, and `-1`
        // would silently pick the newer and revert the copy while reporting
        // success. A broken invariant is refused rather than resolved.
        let out = self.git(
            repo,
            &[
                os("log"),
                os("--format=%H"),
                os(format!("--grep={pattern}")),
            ],
        )?;
        let shas: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        match shas.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some((*one).to_string())),
            many => Err(GitWriteError::new(format!(
                "Mutation-Id {} is carried by {} commits ({}); one commit per mutation is an \
                 invariant, so this history has been copied or rewritten. Refusing to guess which \
                 to act on",
                mutation_id.0,
                many.len(),
                many.join(", ")
            ))),
        }
    }

    /// Whether the repo carries this mutation's commit. Crash recovery uses it to
    /// classify a member as committed (revert) or written-not-committed
    /// (path-restore).
    pub(crate) fn committed(
        &self,
        repo: &Path,
        mutation_id: &MutationId,
    ) -> Result<bool, GitWriteError> {
        Ok(self.find_mutation_sha(repo, mutation_id)?.is_some())
    }

    /// Fetch `remote` at `ref_` into a git store at `git_dir`, returning the
    /// resolved commit sha (`FETCH_HEAD`).
    ///
    /// The store is created if absent and holds only the fetched objects, enough
    /// to extract the tree. `ref_` is a branch, a tag, or a sha the remote
    /// serves. A network or ref failure surfaces as an error, so a caller leaves
    /// no half-built cache entry behind.
    pub(crate) fn fetch_ref(
        &self,
        git_dir: &Path,
        remote: &str,
        ref_: &str,
    ) -> Result<String, GitWriteError> {
        std::fs::create_dir_all(git_dir).map_err(|e| {
            GitWriteError::new(format!("could not create {}: {e}", git_dir.display()))
        })?;
        // A bare-ish store: `init` is idempotent, `fetch` needs only the objects,
        // never a working tree. `--no-tags` keeps the fetch to exactly the ref.
        self.git(git_dir, &[os("init"), os("-q")])?;
        self.git(
            git_dir,
            &[os("fetch"), os("-q"), os("--no-tags"), os(remote), os(ref_)],
        )?;
        self.git(git_dir, &[os("rev-parse"), os("FETCH_HEAD")])
    }

    /// Extract `commit`'s tree from the store at `git_dir` into `dest`, a plain
    /// file tree with no `.git`.
    ///
    /// `read-tree` loads the tree into the index, `checkout-index` writes every
    /// entry under the `dest/` prefix, creating subdirectories. `dest` is created
    /// first, the prefix's base must exist. The result is the immutable snapshot
    /// the assembly walk reads, see the package cache.
    pub(crate) fn extract_tree(
        &self,
        git_dir: &Path,
        commit: &str,
        dest: &Path,
    ) -> Result<(), GitWriteError> {
        std::fs::create_dir_all(dest)
            .map_err(|e| GitWriteError::new(format!("could not create {}: {e}", dest.display())))?;
        self.git(git_dir, &[os("read-tree"), os(commit)])?;
        let mut prefix = OsString::from("--prefix=");
        prefix.push(dest.as_os_str());
        prefix.push("/");
        self.git(git_dir, &[os("checkout-index"), os("-a"), os("-f"), prefix])?;
        Ok(())
    }
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

fn path_args(paths: &[PathBuf]) -> impl Iterator<Item = OsString> + '_ {
    paths.iter().map(|p| p.as_os_str().to_os_string())
}

/// `-c user.name=... -c user.email=...`, so a commit never fails on a repo that
/// configures no identity.
fn identity_args() -> Vec<OsString> {
    vec![
        os("-c"),
        os(format!("user.name={COMMIT_AUTHOR_NAME}")),
        os("-c"),
        os(format!("user.email={COMMIT_AUTHOR_EMAIL}")),
    ]
}

fn lock_contended(stderr: &str) -> bool {
    stderr.contains("index.lock") || stderr.contains("Another git process")
}

/// Remove any parent directory of `path` (repo-relative) that is now empty, up to
/// but not including `repo`. `remove_dir` removes only an empty directory, so a
/// directory holding other content stops the walk. Best-effort, a removal error
/// just ends the walk. Git does not track empty directories, so a pre-existing
/// intentionally-empty directory being pruned is low-impact.
fn prune_empty_parents(repo: &Path, path: &Path) {
    let mut dir = repo.join(path);
    dir.pop();
    while dir.as_path() != repo && dir.starts_with(repo) {
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        dir.pop();
    }
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `git log` format `commit_meta` parses, `%x1e`-delimited records of
/// `%x1f`-delimited fields: oid, committer unix date, author, trailers, body.
///
/// The two separators are control bytes a commit message never carries, so a
/// multi-line message or trailer block cannot break the split. The body is last
/// so its newlines stay inside its own field.
const COMMIT_META_FORMAT: &str =
    "--format=%x1e%H%x1f%ct%x1f%an <%ae>%x1f%(trailers:only=true,unfold=true)%x1f%B";

/// One commit's parsed fields, keyed by full oid for the positional stitch.
struct ParsedCommitMeta {
    timestamp: Option<i64>,
    author: String,
    message: String,
    trailers: Vec<CommitTrailer>,
}

/// Parse [`COMMIT_META_FORMAT`] output into a by-oid map.
fn parse_commit_meta_log(out: &str) -> HashMap<String, ParsedCommitMeta> {
    let mut map = HashMap::new();
    for rec in out.split('\u{1e}') {
        // `splitn(5)` keeps the body intact even if it somehow held a separator.
        let mut parts = rec.splitn(5, '\u{1f}');
        let oid = parts.next().unwrap_or("").trim();
        if oid.is_empty() {
            continue; // the empty lead-in before the first record separator.
        }
        let ts = parts.next().unwrap_or("").trim();
        let author = parts.next().unwrap_or("").trim().to_string();
        let trailers_block = parts.next().unwrap_or("");
        let message = parts.next().unwrap_or("").trim_end().to_string();
        map.insert(
            oid.to_string(),
            ParsedCommitMeta {
                timestamp: ts.parse::<i64>().ok(),
                author,
                message,
                trailers: trailers_block
                    .lines()
                    .filter_map(parse_trailer_line)
                    .collect(),
            },
        );
    }
    map
}

/// Parse one trailer line `Key: value` into a [`CommitTrailer`].
///
/// Splits on the FIRST colon, so a value carrying a `:` (a `Moved: a -> b`, a
/// url) keeps it. A line with no colon, or an empty key, is not a trailer.
fn parse_trailer_line(line: &str) -> Option<CommitTrailer> {
    let (key, value) = line.trim().split_once(':')?;
    let key = key.trim();
    (!key.is_empty()).then(|| CommitTrailer {
        key: key.to_string(),
        value: value.trim().to_string(),
    })
}

/// The `git log` format `file_history` parses: a `%x1e`-delimited record of
/// `%x1f`-delimited fields, oid / committer date / author / SUBJECT. The
/// subject is single-line (`%s`, not `%B`), so the record's first line holds
/// every field and the `--name-status` lines follow it cleanly.
const FILE_HISTORY_FORMAT: &str = "--format=%x1e%H%x1f%ct%x1f%an <%ae>%x1f%s";

/// Parse [`FILE_HISTORY_FORMAT`] + `--name-status` output into per-commit rows.
///
/// Each `%x1e` record is the metadata line, then the name-status line(s). With
/// `--follow` a commit touches the one path, so the first status line settles
/// its status and rename source.
fn parse_file_history(out: &str) -> Vec<FileHistoryRecord> {
    let mut recs = Vec::new();
    for block in out.split('\u{1e}') {
        let mut lines = block.split('\n');
        let head = lines.next().unwrap_or("");
        let mut f = head.splitn(4, '\u{1f}');
        let commit = f.next().unwrap_or("").trim().to_string();
        if commit.is_empty() {
            continue; // the empty lead-in before the first record separator.
        }
        let timestamp = f.next().unwrap_or("").trim().parse::<i64>().unwrap_or(0);
        let author = f.next().unwrap_or("").trim().to_string();
        let message = f.next().unwrap_or("").trim().to_string();
        // The first non-empty line after the metadata is the name-status row.
        let (status, from) = lines
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(parse_name_status)
            .unwrap_or(("modified", None));
        recs.push(FileHistoryRecord {
            commit,
            timestamp,
            author,
            message,
            status,
            from,
        });
    }
    recs
}

/// Map one `--name-status` line to a `(status, from)`.
///
/// The status column carries git's rename/copy SIMILARITY score (`R096`), so
/// only its first letter is read. A rename or copy carries the prior path in
/// its second tab column, git's heuristic match, surfaced as `from`.
fn parse_name_status(line: &str) -> (&'static str, Option<String>) {
    let mut cols = line.split('\t');
    let code = cols.next().unwrap_or("");
    let mut from = || cols.next().filter(|s| !s.is_empty()).map(str::to_string);
    match code.chars().next() {
        Some('A') => ("added", None),
        Some('D') => ("deleted", None),
        Some('R') => ("renamed", from()),
        Some('C') => ("renamed", from()), // a copy carries a source too.
        _ => ("modified", None),
    }
}

/// The `git log` format `recent_commits` parses: a `%x1e`-delimited record of
/// `%x1f`-delimited fields, oid / committer date / author / trailers / SUBJECT,
/// with the `--name-status` lines following the last field.
///
/// The trailers field is multi-line (unfolded), so the record is split by the
/// `%x1f` control byte, never by line. Only the trailing subject-plus-name-status
/// field is then split by line: its first line is the subject, the rest the
/// changed-file rows git appends after the format.
const RECENT_COMMITS_FORMAT: &str =
    "--format=%x1e%H%x1f%ct%x1f%an%x1f%ae%x1f%(trailers:only=true,unfold=true)%x1f%s";

/// Parse [`RECENT_COMMITS_FORMAT`] + `--name-status` output into per-commit rows.
///
/// Each `%x1e` record splits into its five format fields by the `%x1f`
/// separator; the fifth field carries the subject then the name-status block,
/// separated only by newlines, so it is split by line after the field split.
fn parse_recent_commits(out: &str) -> Vec<RecentCommitRecord> {
    let mut recs = Vec::new();
    for rec in out.split('\u{1e}') {
        // `splitn(6)` splits on the format's field separators only, so the
        // trailers field's own newlines and the trailing name-status block both
        // stay intact inside their fields.
        let mut parts = rec.splitn(6, '\u{1f}');
        let commit = parts.next().unwrap_or("").trim().to_string();
        if commit.is_empty() {
            continue; // the empty lead-in before the first record separator.
        }
        let timestamp = parts
            .next()
            .unwrap_or("")
            .trim()
            .parse::<i64>()
            .unwrap_or(0);
        let author_name = parts.next().unwrap_or("").trim().to_string();
        let author_email = parts.next().unwrap_or("").trim().to_string();
        let trailers = parts
            .next()
            .unwrap_or("")
            .lines()
            .filter_map(parse_trailer_line)
            .collect();
        // The final field is the subject line, then the name-status rows git
        // appends after the format (a blank line may separate them).
        let tail = parts.next().unwrap_or("");
        let mut tail_lines = tail.split('\n');
        let subject = tail_lines.next().unwrap_or("").trim().to_string();
        let changed_files = tail_lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter_map(parse_changed_file)
            .collect();
        recs.push(RecentCommitRecord {
            commit,
            timestamp,
            author_name,
            author_email,
            subject,
            changed_files,
            trailers,
        });
    }
    recs
}

/// Map one `--name-status` line to a [`ChangedFile`].
///
/// The status column carries git's rename/copy similarity score (`R096`), so
/// only its first letter is read. A rename or copy carries the prior path in the
/// second tab column and the new path in the third; every other status carries
/// the path in the second. A line with no path is skipped.
fn parse_changed_file(line: &str) -> Option<ChangedFile> {
    let mut cols = line.split('\t');
    let code = cols.next().unwrap_or("");
    match code.chars().next()? {
        'A' => Some(ChangedFile {
            path: cols.next()?.to_string(),
            status: "added",
            from: None,
        }),
        'D' => Some(ChangedFile {
            path: cols.next()?.to_string(),
            status: "deleted",
            from: None,
        }),
        // A rename or a copy: `R096<tab>old<tab>new`. The prior path is `from`,
        // the new path is `path`.
        'R' | 'C' => {
            let from = cols.next().filter(|s| !s.is_empty()).map(str::to_string);
            Some(ChangedFile {
                path: cols.next()?.to_string(),
                status: "renamed",
                from,
            })
        }
        // Modified, type-change, and any other code with a path read as a plain
        // change, mirroring `file_history`'s catch-all.
        _ => Some(ChangedFile {
            path: cols.next()?.to_string(),
            status: "modified",
            from: None,
        }),
    }
}

/// One `recent_commits` row: a commit tagged with the working tree it lives in
/// and the member names that tree holds.
///
/// The `tree` root is the dedup and lane key;
/// `members` labels a monorepo tree's au-repos. The commit fields ride in
/// `record`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentCommitRow {
    /// The owning working-tree root, the lane key.
    pub tree: String,
    /// The au-repo member names living in that tree, sorted.
    pub members: Vec<String>,
    /// The commit.
    pub record: RecentCommitRecord,
}

/// Group members into their distinct working trees, the dedup the stream keys on.
///
/// `members` is each member's name and its covering working-tree root (the
/// `members` read's `git.root`), `None` for a `.git`-free snapshot or an
/// untracked member. `filter`, when present, keeps only the named members. A
/// member with no tree root contributes nothing. Trees come back sorted, each
/// with its member names sorted, so the result is deterministic.
pub(crate) fn group_members_by_tree(
    members: &[(String, Option<String>)],
    filter: Option<&[String]>,
) -> Vec<(String, Vec<String>)> {
    let mut by_tree: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, tree) in members {
        if let Some(f) = filter {
            if !f.iter().any(|n| n == name) {
                continue;
            }
        }
        // A `.git`-free / untracked member has no tree, so it contributes nothing.
        let Some(tree) = tree else { continue };
        by_tree.entry(tree.clone()).or_default().push(name.clone());
    }
    by_tree
        .into_iter()
        .map(|(tree, mut members)| {
            members.sort();
            members.dedup();
            (tree, members)
        })
        .collect()
}

/// Merge per-tree commit lists into one newest-first stream, bounded.
///
/// Each `(tree, members, records)` tags its rows with the tree and its members;
/// the merged stream sorts newest committer-date first, equal timestamps
/// tie-broken by commit oid for a total, deterministic order (CLAUDE.md's
/// "stable sorted results everywhere"), then cuts to `limit` when set.
pub(crate) fn merge_recent_commits(
    per_tree: Vec<(String, Vec<String>, Vec<RecentCommitRecord>)>,
    limit: Option<usize>,
) -> Vec<RecentCommitRow> {
    let mut rows: Vec<RecentCommitRow> = per_tree
        .into_iter()
        .flat_map(|(tree, members, records)| {
            records.into_iter().map(move |record| RecentCommitRow {
                tree: tree.clone(),
                members: members.clone(),
                record,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.record
            .timestamp
            .cmp(&a.record.timestamp)
            .then_with(|| a.record.commit.cmp(&b.record.commit))
    });
    if let Some(n) = limit {
        rows.truncate(n);
    }
    rows
}

/// The commit message, summary then the `Mutation-Id` and `Mutation-Members`
/// trailers crash recovery reads back.
fn format_message(message: &CommitMessage) -> String {
    let members = message
        .members
        .iter()
        .map(|r| r.0.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut trailers = format!(
        "Mutation-Id: {}\nMutation-Members: {}\n",
        message.mutation_id.0, members
    );
    // `Moved:` trailers follow, one per move, the strongest forward-trace
    // tier. Paths relative to the COMMITTING WORKING TREE, the same basis as the
    // commit itself, which is the form the trace chains by.
    for mv in &message.moves {
        let from = mv.from.to_string_lossy();
        let to = mv.to.to_string_lossy();
        // The line-based trailer cannot round-trip a path with a newline or the
        // ` -> ` delimiter. Such a path is pathological; catch it in dev rather
        // than emit a structurally malformed commit message.
        debug_assert!(
            !from.contains('\n')
                && !to.contains('\n')
                && !from.contains(" -> ")
                && !to.contains(" -> "),
            "Moved: trailer cannot encode a path with a newline or ' -> ': {from:?} -> {to:?}"
        );
        trailers.push_str(&format!("Moved: {from} -> {to}\n"));
    }
    if let Some(sha) = &message.reverts {
        trailers.push_str(&format!("Reverts: {sha}\n"));
    }
    // The caller attribution follows the engine's own trailers, verbatim. The
    // keys are validated against the reserved set at the write boundary, so a
    // caller line can never shadow `Mutation-Id` or a move record.
    for t in &message.attribution {
        trailers.push_str(&format!("{}: {}\n", t.key, t.value));
    }
    format!("{}\n\n{}", message.summary, trailers)
}

/// The value after a trailer key, tolerating leading whitespace.
///
/// `git merge --squash` indents every body it folds in by four spaces, so a
/// column-0 match silently loses the whole squashed range. Reading a trailer is
/// forgiving for that reason. Deciding a commit IS a given mutation's commit is
/// not, see [`ShellGit::find_mutation_sha`].
fn trailer_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.trim_start().strip_prefix(key)
}

/// Whether a line carries this trailer key, tolerating leading whitespace.
fn is_trailer(line: &str, key: &str) -> bool {
    trailer_value(line, key).is_some()
}

/// The `Moved:` records one commit carries, and its `Mutation-Members`.
///
/// Read back off a single commit rather than a range, so a compensating commit
/// can invert what it is undoing. Unlike [`ShellGit::moved_trailers_since`] this
/// applies NO engine-authored gate: the caller already located the commit by its
/// `Mutation-Id`, so it is the engine's by construction.
fn commit_moves_and_members(body: &str) -> (Vec<RepoName>, Vec<MovedRecord>) {
    let mut members = Vec::new();
    let mut moves = Vec::new();
    for line in body.lines() {
        if let Some(rest) = trailer_value(line, "Mutation-Members: ") {
            members.extend(
                rest.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| RepoName(s.to_string())),
            );
        }
        if let Some(rest) = trailer_value(line, "Moved: ") {
            if let Some((from, to)) = rest.split_once(" -> ") {
                moves.push(MovedRecord {
                    from: PathBuf::from(from.trim()),
                    to: PathBuf::from(to.trim()),
                });
            }
        }
    }
    (members, moves)
}

impl GitWriter for ShellGit {
    fn commit(
        &self,
        repo: &Path,
        paths: &[PathBuf],
        message: &CommitMessage,
    ) -> Result<CommitSha, GitWriteError> {
        // Stage exactly our paths: -A captures new, modified, and deleted.
        let mut add = vec![os("add"), os("-A"), os("--")];
        add.extend(path_args(paths));
        self.git(repo, &add)?;

        // Commit only our paths. The pathspec limits the commit, so a human's
        // unrelated staged work stays in the index.
        let mut commit = identity_args();
        commit.extend([
            os("commit"),
            os("--no-verify"),
            os("--no-gpg-sign"),
            os("-m"),
            os(format_message(message)),
            os("--"),
        ]);
        commit.extend(path_args(paths));
        self.git(repo, &commit)?;

        let sha = self.git(repo, &[os("rev-parse"), os("HEAD")])?;
        Ok(CommitSha(sha))
    }

    fn revert_commit(&self, repo: &Path, mutation_id: &MutationId) -> Result<(), GitWriteError> {
        let Some(sha) = self.find_mutation_sha(repo, mutation_id)? else {
            return Err(GitWriteError::new(format!(
                "no commit carries Mutation-Id: {}",
                mutation_id.0
            )));
        };

        // Read what is being undone BEFORE undoing it, so the compensating commit
        // can carry the inverse. Git's own revert message carries no trailers at
        // all, which leaves the original `Moved: a -> b` standing as the only
        // record in history: a reader chains to `b`, finds it absent, and answers
        // "deleted" with full confidence about a file sitting untouched at `a`.
        // The engine's own rollback path produces that state, so it is written by
        // us, not inflicted on us.
        let subject = self.git(repo, &[os("log"), os("-1"), os("--format=%s"), os(&sha)])?;
        let body = self.git(repo, &[os("log"), os("-1"), os("--format=%B"), os(&sha)])?;
        let (members, undone) = commit_moves_and_members(&body);

        // The paths to scope the compensating commit to, read BEFORE the revert
        // stages anything, so this describes the reverted commit rather than the
        // index it is about to disturb.
        //
        // `git revert` only refuses when the files it TOUCHES are dirty, so a
        // human's unrelated staged work passes straight through it. Without a
        // pathspec the compensating commit sweeps that work up under the engine's
        // `Revert "…"` summary and its `Mutation-Id`, making the engine the author
        // of a commit the human never made. `commit` scopes for exactly this
        // reason, and the rollback path needs it more, not less: it runs when
        // something has already gone wrong.
        let scope = self.changed_paths(repo, &sha)?;
        if scope.is_empty() {
            // Unreachable for an engine commit, which always carries a non-empty
            // pathspec and is never empty at HEAD. Refused rather than committed
            // unscoped, because unscoped is the defect above.
            return Err(GitWriteError::new(format!(
                "commit {sha} changed no paths, so there is nothing to compensate"
            )));
        }

        // Stage the inverse without committing, so the message is ours.
        //
        // A revert CONFLICTS whenever a later commit touched the same region, and
        // then it leaves `REVERT_HEAD` plus `<<<<<<<` markers inside tracked
        // knowledge-base files. The daemon is watching those files, so the next
        // rebuild indexes the markers as content. Every failure from here on
        // therefore unwinds before returning.
        let mut revert = identity_args();
        revert.extend([os("revert"), os("--no-commit"), os(&sha)]);
        if let Err(e) = self.git(repo, &revert) {
            self.unwind_revert(repo, &scope);
            return Err(e);
        }

        // The inverse moves, in reverse order: undoing `a -> b -> c` restores
        // `c -> b` then `b -> a`, so a replay composes back to `a`.
        let moves = undone
            .iter()
            .rev()
            .map(|mv| MovedRecord {
                from: mv.to.clone(),
                to: mv.from.clone(),
            })
            .collect();

        // A DISTINCT id, never the original's. `find_mutation_sha` REFUSES a
        // duplicated id, so reusing it would leave two commits carrying one id
        // and make a second rollback attempt fail outright rather than find the
        // original. The `Reverts:` trailer carries the link instead.
        let message = CommitMessage {
            summary: format!("Revert \"{subject}\""),
            mutation_id: MutationId(crate::mutate::generate_mutation_id()),
            members,
            moves,
            reverts: Some(sha),
            attribution: Vec::new(),
        };
        let mut commit = identity_args();
        commit.extend([
            os("commit"),
            os("--no-verify"),
            os("--no-gpg-sign"),
            os("-m"),
            os(format_message(&message)),
            os("--"),
        ]);
        commit.extend(path_args(&scope));
        // The second window, and it is newer than the conflict above: the revert
        // SUCCEEDED and staged its changes, so a failure here leaves staged revert
        // content plus `REVERT_HEAD` behind.
        if let Err(e) = self.git(repo, &commit) {
            self.unwind_revert(repo, &scope);
            return Err(e);
        }
        Ok(())
    }

    fn restore_paths(&self, repo: &Path, paths: &[PathBuf]) -> Result<(), GitWriteError> {
        let mut tracked = Vec::new();
        for path in paths {
            if self.exists_at_head(repo, path)? {
                tracked.push(path.clone());
            } else {
                // Absent at HEAD: created by the mutation, remove it to restore absence.
                let abs = repo.join(path);
                if abs.exists() {
                    std::fs::remove_file(&abs).map_err(|e| {
                        GitWriteError::new(format!("could not remove {}: {e}", abs.display()))
                    })?;
                    // A rename into a new subdirectory leaves the directory behind;
                    // prune any the removal left empty.
                    prune_empty_parents(repo, path);
                }
            }
        }
        if !tracked.is_empty() {
            let mut checkout = vec![os("checkout"), os("HEAD"), os("--")];
            checkout.extend(path_args(&tracked));
            self.git(repo, &checkout)?;
        }
        Ok(())
    }

    fn is_clean(&self, repo: &Path, paths: &[PathBuf]) -> Result<bool, GitWriteError> {
        let mut status = vec![os("status"), os("--porcelain"), os("--")];
        status.extend(path_args(paths));
        let out = self.git(repo, &status)?;
        Ok(out.is_empty())
    }
}

/// The git working tree holding `dir`, itself or the nearest enclosing one.
///
/// The engine's ONE notion of git-ness. A repo nested inside a larger working
/// tree is covered BY that tree: it is where the repo's files physically live,
/// so it is the only tree that can hold their commits. Answering `None` means no
/// tree covers the path at all, the genuine non-git case.
///
/// Shared by the saga (which member commits where) and the `members` read (which
/// tree covers a member), so the two cannot drift into disagreeing predicates.
///
/// Distinct from [`enclosing_git_root`] in `serve.rs`, which SKIPS its input
/// because it is handed a file path. This includes `dir` itself, so a repo that
/// is its own working tree is covered by it.
pub(crate) fn working_tree_of(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.join(".git").exists())
        .map(|a| a.to_path_buf())
}

/// HEAD for the `content` read: the direct ref-file read where it resolves
/// cleanly, else the `rev-parse HEAD` subprocess. The same value
/// [`ShellGit::head_commit`] returns, without the per-read fork in the common
/// case (a normal repo with a loose or packed branch ref).
pub(crate) fn head_commit_fast(start: &Path) -> Option<String> {
    match discover_gitdir(start) {
        // A git tree: the direct ref read, or `rev-parse` for a state the direct
        // read cannot resolve cleanly (a symref chain, a shared-common ref).
        GitAnchor::Gitdir(gitdir) => {
            head_from_gitdir(&gitdir).or_else(|| ShellGit.head_commit(start))
        }
        // A `.git` FILE we could not parse (unreadable, or no `gitdir:` line):
        // git may still resolve it, so fall back to the subprocess rather than
        // dropping the anchor. Parity with the former always-shell path, and a
        // transient read of the `.git` file is retried instead of lost.
        GitAnchor::Unresolved => ShellGit.head_commit(start),
        // No `.git` above `start`: no anchor, and no point forking git to
        // rediscover that (the original shell path paid a failed subprocess).
        GitAnchor::Absent => None,
    }
}

/// The outcome of walking up for a git anchor: a resolved gitdir, a `.git` FILE
/// we could not resolve (the subprocess might), or no `.git` at all (nothing to
/// resolve, so no subprocess). The middle case is what keeps a malformed or
/// transiently-unreadable `.git` file routing to the fallback, not short-
/// circuiting to `None`.
enum GitAnchor {
    Gitdir(PathBuf),
    Unresolved,
    Absent,
}

/// The HEAD commit read directly from a gitdir's ref files, no subprocess.
///
/// Returns `Some(sha)` ONLY on a clean resolve. Any state it cannot resolve
/// with certainty (a symref chain, a worktree's shared-common ref, a parse
/// doubt) returns `None`, so [`head_commit_fast`] falls back to `rev-parse`. It
/// never guesses, the value is a pin anchor.
fn head_from_gitdir(gitdir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(refname) => resolve_ref(gitdir, refname.trim()),
        // A detached HEAD is the sha itself.
        None => is_hex_sha(head).then(|| head.to_string()),
    }
}

/// Walk up from `start` to the enclosing `.git`. A `.git` directory IS the
/// gitdir; a `.git` FILE (a worktree or submodule) holds `gitdir: <path>`,
/// relative to the file's own directory. A `.git` FILE we cannot read or parse
/// is [`GitAnchor::Unresolved`] (the subprocess may still resolve it), a tree
/// with no `.git` is [`GitAnchor::Absent`].
fn discover_gitdir(start: &Path) -> GitAnchor {
    let mut dir = start;
    loop {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return GitAnchor::Gitdir(dotgit);
        }
        if dotgit.is_file() {
            let Ok(contents) = std::fs::read_to_string(&dotgit) else {
                return GitAnchor::Unresolved;
            };
            let Some(rel) = contents.trim().strip_prefix("gitdir:") else {
                return GitAnchor::Unresolved;
            };
            return GitAnchor::Gitdir(dir.join(rel.trim()));
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return GitAnchor::Absent,
        }
    }
}

/// Resolve a ref name to its sha: the loose ref file, else the `packed-refs`
/// table. `None` when neither yields a plain sha, which routes to the fallback.
fn resolve_ref(gitdir: &Path, refname: &str) -> Option<String> {
    // A loose ref is authoritative when present. A non-sha loose file (a symref
    // chain) returns None rather than chasing it, the fallback handles it.
    if let Ok(loose) = std::fs::read_to_string(gitdir.join(refname)) {
        let loose = loose.trim();
        return is_hex_sha(loose).then(|| loose.to_string());
    }
    let packed = std::fs::read_to_string(gitdir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let line = line.trim();
        // Skip the header, comments, and `^`-prefixed peeled-tag lines.
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        if let Some((sha, name)) = line.split_once(' ') {
            if name == refname && is_hex_sha(sha) {
                return Some(sha.to_string());
            }
        }
    }
    None
}

/// A git object id: 40 hex chars (SHA-1) or 64 (SHA-256).
fn is_hex_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.name", "Tester"]);
        run(p, &["config", "user.email", "tester@example.com"]);
        fs::write(p.join("seed.md"), "seed\n").unwrap();
        run(p, &["add", "."]);
        run(p, &["commit", "-q", "-m", "seed"]);
        dir
    }

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

    /// The direct read from a knowledge base path: discover the gitdir, then resolve.
    /// The no-fallback half of [`head_commit_fast`], so a test asserts the pure
    /// direct read without a subprocess in the loop.
    fn head_from_refs(start: &Path) -> Option<String> {
        match discover_gitdir(start) {
            GitAnchor::Gitdir(g) => head_from_gitdir(&g),
            _ => None,
        }
    }

    #[test]
    fn head_from_refs_matches_rev_parse_on_a_real_repo() {
        let dir = init_repo();
        let direct = head_from_refs(dir.path());
        let shelled = ShellGit.head_commit(dir.path());
        assert!(shelled.is_some());
        assert_eq!(direct, shelled, "the direct read must equal rev-parse HEAD");
    }

    #[test]
    fn head_from_refs_reads_a_loose_branch_ref() {
        let dir = TempDir::new().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        fs::write(git.join("refs/heads/main"), format!("{sha}\n")).unwrap();
        assert_eq!(head_from_refs(dir.path()).as_deref(), Some(sha));
    }

    #[test]
    fn head_from_refs_falls_to_packed_refs() {
        let dir = TempDir::new().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = "89abcdef0123456789abcdef0123456789abcdef";
        fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(head_from_refs(dir.path()).as_deref(), Some(sha));
    }

    #[test]
    fn head_from_refs_reads_a_detached_head() {
        let dir = TempDir::new().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        let sha = "fedcba9876543210fedcba9876543210fedcba98";
        fs::write(git.join("HEAD"), format!("{sha}\n")).unwrap();
        assert_eq!(head_from_refs(dir.path()).as_deref(), Some(sha));
    }

    #[test]
    fn head_from_refs_follows_a_dotgit_file() {
        let dir = TempDir::new().unwrap();
        let realgit = dir.path().join("realgit");
        fs::create_dir_all(realgit.join("refs/heads")).unwrap();
        fs::write(realgit.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        let sha = "1111111111111111111111111111111111111111";
        fs::write(realgit.join("refs/heads/wt"), sha).unwrap();
        // A worktree/submodule `.git` FILE pointing at the real gitdir.
        fs::write(dir.path().join(".git"), "gitdir: realgit\n").unwrap();
        assert_eq!(head_from_refs(dir.path()).as_deref(), Some(sha));
    }

    #[test]
    fn head_from_refs_is_none_off_a_git_tree() {
        let dir = TempDir::new().unwrap();
        assert_eq!(head_from_refs(dir.path()), None);
    }

    /// A malformed `.git` FILE routes to the fallback, not a short-circuit.
    ///
    /// The direct read cannot resolve a `.git` file with no `gitdir:` line, but
    /// git might, so `discover_gitdir` reports `Unresolved` (which routes
    /// `head_commit_fast` to the subprocess), NOT `Absent` (which would skip it).
    /// No behavioral output delta on garbage contents (git rejects the same
    /// input), the value is transient-read parity and matching the former
    /// always-shell path. This locks the routing decision.
    #[test]
    fn malformed_dotgit_file_routes_to_the_fallback_not_a_short_circuit() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".git"), "not a gitfile\n").unwrap();
        assert!(matches!(discover_gitdir(dir.path()), GitAnchor::Unresolved));

        // No `.git` anywhere is genuinely no anchor: short-circuit, no subprocess.
        let empty = TempDir::new().unwrap();
        assert!(matches!(discover_gitdir(empty.path()), GitAnchor::Absent));
    }

    fn msg(id: &str, members: &[&str]) -> CommitMessage {
        CommitMessage {
            summary: format!("test mutation {id}"),
            mutation_id: MutationId(id.to_string()),
            members: members.iter().map(|m| RepoName(m.to_string())).collect(),
            moves: Vec::new(),
            reverts: None,
            attribution: Vec::new(),
        }
    }

    #[test]
    fn commit_round_trip_and_trailers() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        let sg = ShellGit;
        let sha = sg
            .commit(p, &[PathBuf::from("a.md")], &msg("m1", &["repoA"]))
            .unwrap();
        assert_eq!(sha.0.len(), 40, "expected a full sha, got {:?}", sha.0);
        assert!(sg.is_clean(p, &[PathBuf::from("a.md")]).unwrap());
        let body = run(p, &["log", "-1", "--format=%B"]);
        assert!(body.contains("Mutation-Id: m1"), "body: {body}");
        assert!(body.contains("Mutation-Members: repoA"), "body: {body}");
    }

    #[test]
    fn commit_meta_reads_metadata_and_trailers() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        let sg = ShellGit;
        let sha = sg
            .commit(p, &[PathBuf::from("a.md")], &msg("m1", &["repoA"]))
            .unwrap();

        let recs = sg.commit_meta(p, &[sha.0.clone()]).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert!(r.available);
        assert_eq!(r.commit, sha.0);
        assert!(r.timestamp.unwrap() > 1_600_000_000, "a sane unix date");
        assert_eq!(
            r.author.as_deref(),
            Some("au-engine <au-engine@arsumbris.ai>")
        );
        assert!(r.message.as_deref().unwrap().contains("test mutation m1"));
        // Every trailer surfaces, uninterpreted.
        let ids: Vec<_> = r
            .trailers
            .iter()
            .filter(|t| t.key == "Mutation-Id")
            .collect();
        assert_eq!(ids.len(), 1, "one Mutation-Id trailer");
        assert_eq!(ids[0].value, "m1");
        assert!(
            r.trailers
                .iter()
                .any(|t| t.key == "Mutation-Members" && t.value == "repoA"),
            "trailers: {:?}",
            r.trailers
        );
    }

    #[test]
    fn commit_meta_marks_absent_commit_unavailable() {
        let repo = init_repo();
        let sg = ShellGit;
        let ghost = "0".repeat(40);
        let recs = sg.commit_meta(repo.path(), &[ghost.clone()]).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert!(!r.available);
        assert_eq!(r.commit, ghost, "an absent sha echoes the input verbatim");
        assert!(r.timestamp.is_none());
        assert!(r.author.is_none());
        assert!(r.message.is_none());
        assert!(r.trailers.is_empty());
    }

    #[test]
    fn commit_meta_is_positional_over_present_and_absent() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        let sha = sg
            .commit(p, &[PathBuf::from("a.md")], &msg("m1", &["repoA"]))
            .unwrap();
        let ghost = "1".repeat(40);
        // request order [absent, present, absent] must round-trip positionally.
        let recs = sg
            .commit_meta(p, &[ghost.clone(), sha.0.clone(), ghost.clone()])
            .unwrap();
        assert_eq!(recs.len(), 3);
        assert!(!recs[0].available && recs[0].commit == ghost);
        assert!(recs[1].available && recs[1].commit == sha.0);
        assert!(!recs[2].available && recs[2].commit == ghost);
    }

    #[test]
    fn commit_meta_resolves_an_abbreviated_input_to_the_full_oid() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        let sha = sg
            .commit(p, &[PathBuf::from("a.md")], &msg("m1", &["repoA"]))
            .unwrap();
        let abbrev = sha.0[..12].to_string();
        let recs = sg.commit_meta(p, &[abbrev]).unwrap();
        assert!(recs[0].available);
        assert_eq!(
            recs[0].commit, sha.0,
            "abbreviated input resolves to full oid"
        );
    }

    #[test]
    fn commit_meta_over_an_empty_request_is_empty() {
        let repo = init_repo();
        assert!(ShellGit.commit_meta(repo.path(), &[]).unwrap().is_empty());
    }

    #[test]
    fn commit_meta_over_a_large_batch_does_not_deadlock() {
        // Past ~2000 shas the write-all-then-read git_stdin deadlocked on the two
        // pipe buffers (git blocked writing stdout, us writing stdin). All shas
        // are fabricated (absent), so the assertion is only that the call RETURNS,
        // positional and complete, well past both 64 KB buffers.
        let repo = init_repo();
        let commits: Vec<String> = (0..4000).map(|i| format!("{i:040x}")).collect();
        let recs = ShellGit.commit_meta(repo.path(), &commits).unwrap();
        assert_eq!(recs.len(), 4000);
        assert!(recs.iter().all(|r| !r.available));
    }

    #[test]
    fn file_history_follows_a_rename() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        // a.md added, renamed to b.md, then modified.
        fs::write(p.join("a.md"), "one\n").unwrap();
        run(p, &["add", "a.md"]);
        run(p, &["commit", "-q", "-m", "add a"]);
        run(p, &["mv", "a.md", "b.md"]);
        run(p, &["commit", "-q", "-m", "rename to b"]);
        fs::write(p.join("b.md"), "one\ntwo\n").unwrap();
        run(p, &["add", "b.md"]);
        run(p, &["commit", "-q", "-m", "edit b"]);

        let hist = sg.file_history(p, std::ffi::OsStr::new("b.md")).unwrap();
        // Newest first: edit (modified), rename (renamed from a.md), add (added).
        assert_eq!(hist.len(), 3, "{hist:?}");
        assert_eq!(hist[0].status, "modified");
        assert!(hist[0].message.contains("edit b"), "{:?}", hist[0]);
        assert!(hist[0].timestamp > 1_600_000_000);
        assert!(hist[0].author.contains("Tester"), "{:?}", hist[0]);
        assert!(hist[0].from.is_none());

        assert_eq!(hist[1].status, "renamed", "{:?}", hist[1]);
        assert_eq!(hist[1].from.as_deref(), Some("a.md"));

        assert_eq!(hist[2].status, "added", "{:?}", hist[2]);
        assert!(hist[2].from.is_none());
    }

    #[test]
    fn file_history_of_an_unknown_path_is_empty() {
        let repo = init_repo();
        let hist = ShellGit
            .file_history(repo.path(), std::ffi::OsStr::new("nope.md"))
            .unwrap();
        assert!(hist.is_empty());
    }

    #[test]
    fn file_history_off_a_git_tree_is_empty() {
        let dir = TempDir::new().unwrap(); // no git init
        let hist = ShellGit
            .file_history(dir.path(), std::ffi::OsStr::new("x.md"))
            .unwrap();
        assert!(
            hist.is_empty(),
            "a non-git dir yields an empty stream, not an error"
        );
    }

    #[test]
    fn recent_commits_streams_newest_first_with_files_and_trailers() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        // Commit 1: add a.md and c.md, via the engine so it carries trailers.
        fs::write(p.join("a.md"), "one\n").unwrap();
        fs::write(p.join("c.md"), "cee\n").unwrap();
        sg.commit(
            p,
            &[PathBuf::from("a.md"), PathBuf::from("c.md")],
            &msg("m1", &["repoA"]),
        )
        .unwrap();
        // Commit 2 (newest): rename a.md -> b.md, a plain `git mv` commit.
        run(p, &["mv", "a.md", "b.md"]);
        run(p, &["commit", "-q", "-m", "rename a to b"]);

        let recs = sg.recent_commits(p, Some(10), None).unwrap();
        // seed + m1 + rename = 3, newest first.
        assert_eq!(recs.len(), 3, "{recs:?}");

        // Newest: the rename, one changed file, renamed from a.md to b.md.
        assert_eq!(recs[0].subject, "rename a to b");
        assert!(recs[0].timestamp > 1_600_000_000);
        assert_eq!(recs[0].author_name, "Tester", "{:?}", recs[0]);
        assert_eq!(recs[0].author_email, "tester@example.com", "{:?}", recs[0]);
        assert_eq!(recs[0].changed_files.len(), 1, "{:?}", recs[0]);
        assert_eq!(recs[0].changed_files[0].status, "renamed");
        assert_eq!(recs[0].changed_files[0].path, "b.md");
        assert_eq!(recs[0].changed_files[0].from.as_deref(), Some("a.md"));

        // Next: the engine commit, two added files, and its Mutation-Id trailer.
        assert_eq!(recs[1].subject, "test mutation m1");
        let added: Vec<&str> = recs[1]
            .changed_files
            .iter()
            .map(|f| {
                assert_eq!(f.status, "added", "{f:?}");
                f.path.as_str()
            })
            .collect();
        assert!(
            added.contains(&"a.md") && added.contains(&"c.md"),
            "{added:?}"
        );
        assert!(
            recs[1]
                .trailers
                .iter()
                .any(|t| t.key == "Mutation-Id" && t.value == "m1"),
            "trailers: {:?}",
            recs[1].trailers
        );
    }

    #[test]
    fn recent_commits_bounds_by_count() {
        let repo = init_repo();
        let p = repo.path();
        for i in 0..5 {
            fs::write(p.join(format!("f{i}.md")), "x\n").unwrap();
            run(p, &["add", "."]);
            run(p, &["commit", "-q", "-m", &format!("commit {i}")]);
        }
        let recs = ShellGit.recent_commits(p, Some(2), None).unwrap();
        assert_eq!(recs.len(), 2, "the -n cap bounds the stream");
        assert_eq!(recs[0].subject, "commit 4");
        assert_eq!(recs[1].subject, "commit 3");
    }

    #[test]
    fn recent_commits_off_a_git_tree_is_empty() {
        let dir = TempDir::new().unwrap(); // no git init
        let recs = ShellGit.recent_commits(dir.path(), Some(10), None).unwrap();
        assert!(
            recs.is_empty(),
            "a non-git dir yields an empty stream, not an error"
        );
    }

    fn rec(commit: &str, ts: i64) -> RecentCommitRecord {
        RecentCommitRecord {
            commit: commit.to_string(),
            timestamp: ts,
            author_name: "Tester".to_string(),
            author_email: "t@e".to_string(),
            subject: format!("s-{commit}"),
            changed_files: Vec::new(),
            trailers: Vec::new(),
        }
    }

    #[test]
    fn group_members_by_tree_dedups_and_filters() {
        let members = vec![
            ("a".to_string(), Some("/t1".to_string())),
            ("b".to_string(), Some("/t1".to_string())), // shares t1 with a (a monorepo)
            ("c".to_string(), Some("/t2".to_string())),
            ("d".to_string(), None), // .git-free / untracked: contributes nothing
        ];
        // No filter: two trees, t1 holds a + b, d dropped.
        assert_eq!(
            group_members_by_tree(&members, None),
            vec![
                ("/t1".to_string(), vec!["a".to_string(), "b".to_string()]),
                ("/t2".to_string(), vec!["c".to_string()]),
            ]
        );
        // Filter to a subset by member name.
        assert_eq!(
            group_members_by_tree(&members, Some(&["a".to_string(), "c".to_string()])),
            vec![
                ("/t1".to_string(), vec!["a".to_string()]),
                ("/t2".to_string(), vec!["c".to_string()]),
            ]
        );
    }

    #[test]
    fn merge_recent_commits_orders_bounds_and_tags() {
        let per_tree = vec![
            (
                "/t1".to_string(),
                vec!["m1".to_string()],
                vec![rec("aaa", 100), rec("ccc", 300)],
            ),
            (
                "/t2".to_string(),
                vec!["m2".to_string()],
                vec![rec("bbb", 200), rec("ddd", 300)],
            ),
        ];
        let rows = merge_recent_commits(per_tree.clone(), None);
        // Newest ts first; equal ts (300) tie-broken by oid asc: ccc before ddd.
        let order: Vec<&str> = rows.iter().map(|r| r.record.commit.as_str()).collect();
        assert_eq!(order, vec!["ccc", "ddd", "bbb", "aaa"]);
        // Tags ride along: ccc is from t1/m1, ddd from t2/m2.
        assert_eq!(rows[0].tree, "/t1");
        assert_eq!(rows[0].members, vec!["m1".to_string()]);
        assert_eq!(rows[1].tree, "/t2");

        // The limit cuts the merged stream, newest kept.
        let bounded = merge_recent_commits(per_tree, Some(2));
        let order: Vec<&str> = bounded.iter().map(|r| r.record.commit.as_str()).collect();
        assert_eq!(order, vec!["ccc", "ddd"]);
    }

    #[test]
    fn validate_attribution_guards_form_and_reserved_keys() {
        // A plain payload passes, trimmed.
        let ok =
            validate_attribution(&[("session".into(), "s".into()), ("span".into(), "t".into())])
                .unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(
            ok[0],
            CommitTrailer {
                key: "session".into(),
                value: "s".into()
            }
        );

        // A reserved key is refused, case-insensitively.
        assert!(validate_attribution(&[("Mutation-Id".into(), "x".into())]).is_err());
        assert!(validate_attribution(&[("mutation-id".into(), "x".into())]).is_err());
        assert!(validate_attribution(&[("Moved".into(), "a -> b".into())]).is_err());

        // Malformed keys / values.
        assert!(validate_attribution(&[("".into(), "x".into())]).is_err());
        assert!(validate_attribution(&[("has:colon".into(), "x".into())]).is_err());
        assert!(validate_attribution(&[("k".into(), "line1\nline2".into())]).is_err());
    }

    #[test]
    fn a_commit_carries_a_caller_attribution_trailer() {
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        let sg = ShellGit;
        let attribution = validate_attribution(&[
            ("session".into(), "s-1".into()),
            ("span".into(), "t-9".into()),
        ])
        .unwrap();
        let message = CommitMessage {
            summary: "test attributed".to_string(),
            mutation_id: MutationId("m-attr".to_string()),
            members: vec![RepoName("repoA".to_string())],
            moves: Vec::new(),
            reverts: None,
            attribution,
        };
        let sha = sg.commit(p, &[PathBuf::from("a.md")], &message).unwrap();

        // The caller trailers read back verbatim, beside the engine's own.
        let recs = sg.commit_meta(p, &[sha.0.clone()]).unwrap();
        let t = &recs[0].trailers;
        assert!(
            t.iter().any(|x| x.key == "session" && x.value == "s-1"),
            "{t:?}"
        );
        assert!(
            t.iter().any(|x| x.key == "span" && x.value == "t-9"),
            "{t:?}"
        );
        assert!(
            t.iter()
                .any(|x| x.key == "Mutation-Id" && x.value == "m-attr"),
            "the engine trailer survives beside the attribution: {t:?}"
        );
    }

    #[test]
    fn commit_excludes_unrelated_staged_work() {
        let repo = init_repo();
        let p = repo.path();
        // a human stages an unrelated file
        fs::write(p.join("human.md"), "human edit\n").unwrap();
        run(p, &["add", "human.md"]);
        // the engine commits only its own file
        fs::write(p.join("engine.md"), "engine\n").unwrap();
        ShellGit
            .commit(p, &[PathBuf::from("engine.md")], &msg("m2", &["r"]))
            .unwrap();
        let files = run(p, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(files.contains("engine.md"), "files: {files}");
        assert!(
            !files.contains("human.md"),
            "engine commit swept human work: {files}"
        );
        let staged = run(p, &["diff", "--cached", "--name-only"]);
        assert!(staged.contains("human.md"), "human staging lost: {staged}");
    }

    #[test]
    fn revert_by_mutation_id_keeps_unrelated_commit() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "v1\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("mX", &["r"]))
            .unwrap();
        // an unrelated human commit lands on top
        fs::write(p.join("b.md"), "human\n").unwrap();
        run(p, &["add", "b.md"]);
        run(p, &["commit", "-q", "-m", "human commit"]);
        sg.revert_commit(p, &MutationId("mX".into())).unwrap();
        assert!(!p.join("a.md").exists(), "revert did not undo a.md");
        assert!(p.join("b.md").exists(), "revert clobbered the human commit");
        let log = run(p, &["log", "--format=%s"]);
        assert!(log.contains("human commit"), "human commit missing: {log}");
    }

    #[test]
    fn a_revert_leaves_a_humans_unrelated_staged_work_alone() {
        // `commit` is pathspec-scoped and says why; the compensating commit was
        // not. `git revert` refuses only when the files it TOUCHES are dirty, so
        // unrelated staged work passes straight through it and an unscoped commit
        // swept it up — the engine authoring a human's work under its own
        // `Mutation-Id`, on the path that runs when something already went wrong.
        //
        // All three shapes in one reverted commit, since the pathspec has to
        // cover both sides of a rename and the removal of an add.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("modified.md"), "v1\n").unwrap();
        fs::write(p.join("renamed-from.md"), "content\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "seed"]);

        fs::write(p.join("modified.md"), "v2\n").unwrap();
        fs::write(p.join("added.md"), "new\n").unwrap();
        fs::remove_file(p.join("renamed-from.md")).unwrap();
        fs::write(p.join("renamed-to.md"), "content\n").unwrap();
        sg.commit(
            p,
            &[
                PathBuf::from("modified.md"),
                PathBuf::from("added.md"),
                PathBuf::from("renamed-from.md"),
                PathBuf::from("renamed-to.md"),
            ],
            &msg("mScope", &["r"]),
        )
        .unwrap();

        // The human stages something the mutation never touched.
        fs::write(p.join("human.md"), "mine\n").unwrap();
        run(p, &["add", "human.md"]);

        sg.revert_commit(p, &MutationId("mScope".into())).unwrap();

        // The compensating commit carries the revert and NOTHING else.
        let touched = run(p, &["show", "--name-only", "--format=", "HEAD"]);
        assert!(
            !touched.contains("human.md"),
            "the engine committed the human's staged work: {touched}"
        );
        let staged = run(p, &["diff", "--cached", "--name-only"]);
        assert!(
            staged.contains("human.md"),
            "the human's staging was consumed rather than left alone: {staged:?}"
        );

        // And the revert itself is complete, all three shapes undone.
        assert_eq!(fs::read_to_string(p.join("modified.md")).unwrap(), "v1\n");
        assert!(
            !p.join("added.md").exists(),
            "the added file was not removed"
        );
        assert!(
            p.join("renamed-from.md").exists(),
            "the rename was not undone"
        );
        assert!(
            !p.join("renamed-to.md").exists(),
            "the rename was not undone"
        );
    }

    #[test]
    fn a_conflicting_revert_leaves_no_mid_revert_state_behind() {
        // A revert conflicts whenever a later commit touched the same region, and
        // git then leaves REVERT_HEAD plus `<<<<<<<` markers inside the tracked
        // file. The daemon is watching, so the next rebuild would index the
        // markers as content — the engine corrupting the graph by its own hand,
        // on the path that only runs after something already failed.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "line1\nline2\nline3\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "seed"]);

        fs::write(p.join("a.md"), "line1\nMUTATION\nline3\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("mConflict", &["r"]))
            .unwrap();

        // A later human commit on the same line, so the revert must conflict.
        fs::write(p.join("a.md"), "line1\nHUMAN-LATER\nline3\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "human edits the same line"]);

        // And unrelated staged work, which the cleanup must NOT take. This is why
        // `git revert --abort` is the wrong tool: it restores the whole pre-revert
        // state and deletes this file.
        fs::write(p.join("human.md"), "mine\n").unwrap();
        run(p, &["add", "human.md"]);

        let err = sg.revert_commit(p, &MutationId("mConflict".into()));
        assert!(err.is_err(), "the conflicting revert reported success");

        assert!(
            !p.join(".git/REVERT_HEAD").exists(),
            "the repo was left mid-revert"
        );
        let content = fs::read_to_string(p.join("a.md")).unwrap();
        assert!(
            !content.contains("<<<<<<<") && !content.contains(">>>>>>>"),
            "conflict markers were left in a tracked file the watcher indexes: {content:?}"
        );
        assert_eq!(
            content, "line1\nHUMAN-LATER\nline3\n",
            "the file was not restored to HEAD"
        );
        let staged = run(p, &["diff", "--cached", "--name-only"]);
        assert!(
            staged.contains("human.md"),
            "the cleanup took the human's unrelated staged work: {staged:?}"
        );
        assert!(p.join("human.md").exists(), "the cleanup deleted human.md");
    }

    #[test]
    fn a_revert_whose_commit_fails_leaves_no_mid_revert_state_behind() {
        // The second window, newer than the conflict: the revert SUCCEEDS and
        // stages its changes, then the scoped commit fails. Provoked by making
        // the commit step fail on an unborn identity — any failure after the
        // revert must unwind the same way.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "v1\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "seed"]);

        fs::write(p.join("a.md"), "v2\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("mFail", &["r"]))
            .unwrap();

        // A human undoes the mutation BY HAND. The revert then applies cleanly and
        // stages nothing, so `git revert --no-commit` exits 0 and sets
        // REVERT_HEAD, and the scoped commit fails with "nothing to commit".
        // Verified as the reachable shape of this window: an `index.lock` instead
        // fails the REVERT step, which is window one wearing this test's name.
        fs::write(p.join("a.md"), "v1\n").unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "human undoes it by hand"]);

        let err = sg.revert_commit(p, &MutationId("mFail".into()));
        assert!(err.is_err(), "the failing commit reported success");

        assert!(
            !p.join(".git/REVERT_HEAD").exists(),
            "the repo was left mid-revert after the commit failed"
        );
        // `git status` must be clean: a lingering revert shows up here even when
        // the tree itself is untouched, which is what the daemon would inherit.
        assert_eq!(
            run(p, &["status", "--porcelain"]),
            "",
            "the repo was left in a dirty state after the commit failed"
        );
    }

    #[test]
    fn a_revert_counter_records_the_move_it_undoes() {
        // The engine's own rollback used to leave a lying record: git's revert
        // message carries no trailers, so the original `Moved: a -> b` stayed as
        // the only claim in history. A reader chains to `b`, finds it absent, and
        // confidently reports a file that is sitting untouched at `a`.
        //
        // The compensating commit now carries the inverse, so a replay composes
        // back to the truth.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "content\n").unwrap();
        run(p, &["add", "a.md"]);
        run(p, &["commit", "-q", "-m", "seed a"]);
        let base = run(p, &["rev-parse", "HEAD"]);

        // A mediated rename: a.md -> b.md, recorded.
        fs::remove_file(p.join("a.md")).unwrap();
        fs::write(p.join("b.md"), "content\n").unwrap();
        let mut message = msg("mMove", &["r"]);
        message.moves = vec![MovedRecord {
            from: PathBuf::from("a.md"),
            to: PathBuf::from("b.md"),
        }];
        sg.commit(p, &[PathBuf::from("a.md"), PathBuf::from("b.md")], &message)
            .unwrap();

        sg.revert_commit(p, &MutationId("mMove".into())).unwrap();
        assert!(p.join("a.md").exists(), "revert did not restore a.md");
        assert!(!p.join("b.md").exists(), "revert left b.md behind");

        // Replayed from the base, the records compose back to a.md rather than
        // stopping at the vanished b.md.
        let moves = sg.moved_trailers_since(p, &base).unwrap();
        let mut at = PathBuf::from("a.md");
        for mv in &moves {
            if mv.from == at {
                at = mv.to.clone();
            }
        }
        assert_eq!(
            at,
            PathBuf::from("a.md"),
            "the replay does not return to the truth: {moves:?}"
        );

        // The compensating commit says WHAT it undid and carries its OWN id, so
        // the id lookup still resolves the original rather than refusing on a duplicate.
        let body = run(p, &["log", "-1", "--format=%B"]);
        assert!(
            body.contains("Moved: b.md -> a.md"),
            "no counter-record: {body}"
        );
        assert!(
            body.contains("Reverts: "),
            "no Reverts back-reference: {body}"
        );
        assert!(
            !body.contains("Mutation-Id: mMove"),
            "the revert reused the reverted id, which would shadow it: {body}"
        );
        assert_eq!(
            sg.find_mutation_sha(p, &MutationId("mMove".into()))
                .unwrap()
                .as_deref(),
            Some(run(p, &["rev-parse", "HEAD~1"]).as_str()),
            "the id lookup no longer finds the original commit"
        );
    }

    #[test]
    fn an_indented_trailer_is_still_read() {
        // `git merge --squash` indents every body it folds in by four spaces. A
        // column-anchored gate fails there and drops EVERY move in the range,
        // silently: no diagnostic, no partial answer, just an absent edge. A
        // record read with a coarser commit attribution is recoverable; one that
        // vanishes is not.
        let repo = init_repo();
        let p = repo.path();
        fs::write(p.join("a.md"), "x\n").unwrap();
        run(p, &["add", "a.md"]);
        run(p, &["commit", "-q", "-m", "seed a"]);
        let base = run(p, &["rev-parse", "HEAD"]);

        fs::remove_file(p.join("a.md")).unwrap();
        fs::write(p.join("b.md"), "x\n").unwrap();
        run(p, &["add", "-A"]);
        run(
            p,
            &[
                "commit",
                "-q",
                "-m",
                "squashed work\n\n    Mutation-Id: m-sq\n    Moved: a.md -> b.md",
            ],
        );

        let moves = ShellGit.moved_trailers_since(p, &base).unwrap();
        assert_eq!(
            moves.len(),
            1,
            "an indented trailer was dropped rather than read: {moves:?}"
        );
        assert_eq!(moves[0].from, PathBuf::from("a.md"));
        assert_eq!(moves[0].to, PathBuf::from("b.md"));
    }

    #[test]
    fn a_duplicated_mutation_id_refuses_rather_than_picking() {
        // A cherry-pick copies a commit body verbatim, so one id lands on two
        // commits. Taking the newest would revert the COPY and report a
        // successful rollback while the original stayed applied. One commit per
        // mutation is an invariant the saga maintains, not something git
        // guarantees, so a broken one is refused.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;

        fs::write(p.join("a.md"), "v1\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("mDup", &["r"]))
            .unwrap();
        let original = run(p, &["rev-parse", "HEAD"]);

        // One id, one commit: resolvable.
        assert_eq!(
            sg.find_mutation_sha(p, &MutationId("mDup".into()))
                .unwrap()
                .as_deref(),
            Some(original.as_str())
        );

        // Cherry-pick it onto a branch that rejoins HEAD, so both copies are
        // reachable, exactly what an ordinary pick-and-merge produces.
        run(p, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        run(p, &["cherry-pick", "--allow-empty", &original]);
        run(p, &["checkout", "-q", "main"]);
        run(
            p,
            &[
                "merge",
                "-q",
                "--no-edit",
                "--allow-unrelated-histories",
                "side",
            ],
        );

        let err = sg
            .find_mutation_sha(p, &MutationId("mDup".into()))
            .expect_err("a duplicated id must not resolve");
        assert!(
            err.message.contains("2 commits"),
            "the refusal does not say what it found: {}",
            err.message
        );
        // And the refusal reaches the caller rather than being swallowed.
        assert!(sg.revert_commit(p, &MutationId("mDup".into())).is_err());
    }

    #[test]
    fn restore_paths_is_scoped_and_removes_created() {
        let repo = init_repo();
        let p = repo.path();
        // a tracked file modified, plus a newly-created file
        fs::write(p.join("seed.md"), "modified\n").unwrap();
        fs::write(p.join("new.md"), "created\n").unwrap();
        // an unrelated dirty file we must not touch
        fs::write(p.join("other.md"), "dirty\n").unwrap();
        ShellGit
            .restore_paths(p, &[PathBuf::from("seed.md"), PathBuf::from("new.md")])
            .unwrap();
        assert_eq!(
            fs::read_to_string(p.join("seed.md")).unwrap(),
            "seed\n",
            "tracked file not restored to HEAD"
        );
        assert!(!p.join("new.md").exists(), "created file not removed");
        assert_eq!(
            fs::read_to_string(p.join("other.md")).unwrap(),
            "dirty\n",
            "unrelated path disturbed"
        );
    }

    #[test]
    fn is_clean_reports_state() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        assert!(
            sg.is_clean(p, &[PathBuf::from("seed.md")]).unwrap(),
            "committed file should be clean"
        );
        fs::write(p.join("seed.md"), "dirty\n").unwrap();
        assert!(
            !sg.is_clean(p, &[PathBuf::from("seed.md")]).unwrap(),
            "modified file should be dirty"
        );
        assert!(
            sg.is_clean(p, &[PathBuf::from("ghost.md")]).unwrap(),
            "absent path should be clean"
        );
    }

    #[test]
    fn restore_paths_prunes_a_directory_left_empty() {
        let repo = init_repo();
        let p = repo.path();

        // A created file in a fresh nested directory: the whole new chain prunes.
        fs::create_dir_all(p.join("sub/deep")).unwrap();
        fs::write(p.join("sub/deep/new.md"), "created\n").unwrap();
        ShellGit
            .restore_paths(p, &[PathBuf::from("sub/deep/new.md")])
            .unwrap();
        assert!(
            !p.join("sub/deep/new.md").exists(),
            "created file not removed"
        );
        assert!(!p.join("sub/deep").exists(), "empty directory not pruned");
        assert!(!p.join("sub").exists(), "empty parent not pruned");

        // A directory with other content is not pruned.
        fs::create_dir_all(p.join("keep")).unwrap();
        fs::write(p.join("keep/other.md"), "x\n").unwrap();
        fs::write(p.join("keep/new.md"), "created\n").unwrap();
        ShellGit
            .restore_paths(p, &[PathBuf::from("keep/new.md")])
            .unwrap();
        assert!(!p.join("keep/new.md").exists(), "created file not removed");
        assert!(
            p.join("keep").exists(),
            "a non-empty directory must not be pruned"
        );
        assert!(
            p.join("keep/other.md").exists(),
            "the sibling was disturbed"
        );
    }

    #[test]
    fn a_move_commit_carries_a_moved_trailer() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("new.md"), "moved content\n").unwrap();
        let mut message = msg("mv1", &["r"]);
        message.moves = vec![MovedRecord {
            from: PathBuf::from("old.md"),
            to: PathBuf::from("new.md"),
        }];
        sg.commit(p, &[PathBuf::from("new.md")], &message).unwrap();
        let body = run(p, &["log", "-1", "--format=%B"]);
        assert!(
            body.contains("Moved: old.md -> new.md"),
            "missing Moved: trailer: {body}"
        );
        // The other trailers still ride alongside.
        assert!(body.contains("Mutation-Id: mv1"), "body: {body}");
    }

    #[test]
    fn a_non_move_commit_carries_no_moved_trailer() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("m1", &["r"]))
            .unwrap();
        let body = run(p, &["log", "-1", "--format=%B"]);
        assert!(
            !body.contains("Moved:"),
            "unexpected Moved: trailer: {body}"
        );
    }

    #[test]
    fn read_at_commit_returns_version_exact_bytes_after_the_live_file_changes() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        let path = PathBuf::from("doc.md");

        // v1, captured as the pinned commit.
        fs::write(p.join("doc.md"), "version one\n").unwrap();
        run(p, &["add", "doc.md"]);
        run(p, &["commit", "-q", "-m", "v1"]);
        let c1 = run(p, &["rev-parse", "HEAD"]);
        let oid1 = sg.blob_oid_at_commit(p, &c1, &path).unwrap();

        // v2, the live file moves on.
        fs::write(p.join("doc.md"), "version two\n").unwrap();
        run(p, &["add", "doc.md"]);
        run(p, &["commit", "-q", "-m", "v2"]);

        // v3, the live file is deleted.
        fs::remove_file(p.join("doc.md")).unwrap();
        run(p, &["add", "-A"]);
        run(p, &["commit", "-q", "-m", "delete"]);

        // The pin resolves to the bytes at c1, deletion-stable.
        assert_eq!(
            sg.read_at_commit(p, &c1, &path).unwrap().as_deref(),
            Some(&b"version one\n"[..]),
            "pinned read did not return the version-exact bytes"
        );
        // The oid at c1 is stable across the later history.
        assert_eq!(
            sg.blob_oid_at_commit(p, &c1, &path).unwrap(),
            oid1,
            "the recorded oid drifted"
        );
        assert!(oid1.is_some(), "expected a blob oid at c1");

        // At HEAD the path is gone: present commit, absent path (pinned-path-absent).
        assert!(sg.commit_present(p, "HEAD").unwrap());
        assert_eq!(
            sg.read_at_commit(p, "HEAD", &path).unwrap(),
            None,
            "the deleted path should read as absent at HEAD"
        );
        assert_eq!(sg.blob_oid_at_commit(p, "HEAD", &path).unwrap(), None);
    }

    #[test]
    fn commit_present_distinguishes_missing_commit_from_missing_path() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;

        // A real commit is present; a bogus commit-ish is not (pinned-commit-unavailable).
        let head = run(p, &["rev-parse", "HEAD"]);
        assert!(sg.commit_present(p, &head).unwrap());
        assert!(
            !sg.commit_present(p, "0000000000000000000000000000000000000000")
                .unwrap(),
            "a non-existent commit must report absent"
        );
        assert!(
            !sg.commit_present(p, "not-a-ref").unwrap(),
            "a malformed commit-ish must report absent, not error"
        );

        // Present commit, absent path: read is None, the bogus-pin signal.
        assert!(sg.commit_present(p, &head).unwrap());
        assert_eq!(
            sg.read_at_commit(p, &head, &PathBuf::from("never-existed.md"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn read_at_commit_preserves_binary_bytes_verbatim() {
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        // Bytes that are not valid UTF-8 and contain a NUL, an asset a file* pin holds.
        let blob: &[u8] = &[0x00, 0xff, 0xfe, 0x01, b'\n', 0x80];
        fs::write(p.join("asset.bin"), blob).unwrap();
        run(p, &["add", "asset.bin"]);
        run(p, &["commit", "-q", "-m", "asset"]);
        let head = run(p, &["rev-parse", "HEAD"]);
        assert_eq!(
            sg.read_at_commit(p, &head, &PathBuf::from("asset.bin"))
                .unwrap()
                .as_deref(),
            Some(blob),
            "binary bytes were corrupted by the read"
        );
    }

    #[test]
    fn mutation_id_lookup_does_not_match_a_longer_id() {
        // Two ids where one is a textual prefix of the other.
        let repo = init_repo();
        let p = repo.path();
        let sg = ShellGit;
        fs::write(p.join("a.md"), "alpha\n").unwrap();
        sg.commit(p, &[PathBuf::from("a.md")], &msg("m-x-1", &["r"]))
            .unwrap();
        fs::write(p.join("b.md"), "beta\n").unwrap();
        sg.commit(p, &[PathBuf::from("b.md")], &msg("m-x-10", &["r"]))
            .unwrap();

        // The lookup is anchored, so `m-x-1` resolves to its own commit, not the
        // more recent `m-x-10`.
        assert!(sg.committed(p, &MutationId("m-x-1".into())).unwrap());
        assert!(sg.committed(p, &MutationId("m-x-10".into())).unwrap());
        assert!(!sg.committed(p, &MutationId("m-x-99".into())).unwrap());

        // Reverting `m-x-1` undoes a.md and leaves the `m-x-10` commit (b.md)
        // intact — an unanchored grep would have reverted the wrong commit.
        sg.revert_commit(p, &MutationId("m-x-1".into())).unwrap();
        assert!(!p.join("a.md").exists(), "m-x-1's commit was not reverted");
        assert!(p.join("b.md").exists(), "the m-x-10 commit was clobbered");
    }
}
