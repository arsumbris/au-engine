//! Walk scoping: a per-root filter deciding which files the walk keeps.
//!
//! Applied ABOVE the hard floor. The floor (`.git`, `.arsumbris`) is a name
//! check in the walk itself, structurally unsafe to ingest and never
//! reachable by any pattern. This filter decides the rest.
//!
//! One shape, deny. The default excludes (`node_modules`, `target`) plus any
//! `.arsumbris/.auignore` file are one layered gitignore. A matched path is
//! dropped, a matched directory is pruned. Allowlisting is negation inside the
//! one file, the standard gitignore idiom `/*` then `!/keep`, there is no
//! separate allowlist mode.
//!
//! Pattern semantics are delegated to the `ignore` crate's gitignore matcher.
//! See [[spec - knowledge base file scoping - a per-repo auignore file over a hard floor]].

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

/// Directory names never walked, regardless of any user scoping. `.arsumbris/`
/// holds the engine's own index / socket / lock / registry, `.git` is VCS
/// internals. No `.auignore` pattern, negation included, reaches them.
pub const FLOOR_DIR_NAMES: &[&str] = &[".git", ".arsumbris"];

/// Directory names excluded by default, overridable by an `.auignore`.
/// Conventionally build / cache output, large and rarely graph content.
pub const DEFAULT_EXCLUDE_DIR_NAMES: &[&str] = &["node_modules", "target"];

/// A per-root walk filter, deny semantics over a layered gitignore. Built
/// anchored to the walk root, so its paths are the absolute paths the walk
/// produces.
pub struct WalkFilter {
    matcher: Gitignore,
}

impl WalkFilter {
    /// The default filter: default excludes only, no user scoping. The
    /// pre-scoping behaviour, minus the floor which the walk applies itself.
    pub fn default_excludes(root: &Path) -> Self {
        // Static, valid patterns; construction cannot fail.
        Self::build(root, "").expect("default-exclude patterns build cleanly")
    }

    /// The default excludes seeded first, then the `.auignore` contents layered
    /// after. Anchored to `root`, so patterns resolve against the member root.
    /// Errors on a malformed pattern; the caller surfaces `auignore-load-error`
    /// and falls back to [`WalkFilter::default_excludes`].
    pub fn with_auignore(root: &Path, contents: &str) -> Result<Self, ignore::Error> {
        Self::build(root, contents)
    }

    fn build(root: &Path, contents: &str) -> Result<Self, ignore::Error> {
        let mut builder = GitignoreBuilder::new(root);
        for name in DEFAULT_EXCLUDE_DIR_NAMES {
            builder.add_line(None, name)?;
        }
        for line in contents.lines() {
            builder.add_line(None, line)?;
        }
        Ok(WalkFilter {
            matcher: builder.build()?,
        })
    }

    /// Keep this file? `path` is absolute, under the anchoring root.
    pub fn keep_file(&self, path: &Path) -> bool {
        !self
            .matcher
            .matched_path_or_any_parents(path, false)
            .is_ignore()
    }

    /// Enter this directory? A matched directory is pruned.
    pub fn enter_dir(&self, path: &Path) -> bool {
        !self
            .matcher
            .matched_path_or_any_parents(path, true)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const ROOT: &str = "/v";

    fn p(rel: &str) -> PathBuf {
        Path::new(ROOT).join(rel)
    }

    fn f(contents: &str) -> WalkFilter {
        WalkFilter::with_auignore(Path::new(ROOT), contents).unwrap()
    }

    #[test]
    fn a_malformed_pattern_errors() {
        // A lone backslash is not a valid glob; the caller turns this into an
        // `auignore-load-error` and falls back to the default excludes.
        assert!(WalkFilter::with_auignore(Path::new(ROOT), "\\\n").is_err());
    }

    #[test]
    fn default_excludes_prune_node_modules_and_target() {
        let f = WalkFilter::default_excludes(Path::new(ROOT));
        assert!(!f.enter_dir(&p("node_modules")), "node_modules pruned");
        assert!(!f.enter_dir(&p("target")), "target pruned");
        assert!(f.enter_dir(&p("src")), "src entered");
        assert!(f.keep_file(&p("src/a.md")), "normal file kept");
    }

    #[test]
    fn auignore_drops_a_matched_subtree() {
        let f = f("docs/\n");
        assert!(!f.enter_dir(&p("docs")), "docs pruned");
        assert!(!f.keep_file(&p("docs/guide.md")), "file under docs dropped");
        assert!(f.keep_file(&p("src/a.md")), "sibling kept");
    }

    #[test]
    fn default_excludes_still_seeded_alongside_auignore() {
        let f = f("docs/\n");
        assert!(!f.enter_dir(&p("node_modules")), "default still applies");
    }

    #[test]
    fn whole_dir_negation_reincludes_a_default() {
        // A single file under a pruned dir cannot be re-included, but negating
        // the whole default-excluded directory does re-include it.
        let f = f("!target\n");
        assert!(f.enter_dir(&p("target")), "!target re-enters the directory");
        assert!(f.keep_file(&p("target/out.txt")), "file under it kept");
    }

    #[test]
    fn allowlist_by_root_anchored_negation() {
        // The standard gitignore allowlist idiom: ignore everything at root,
        // re-include one directory. Its contents are then walked (`/*` is
        // root-only, so it doesn't re-match them).
        let f = f("/*\n!/docs\n");
        assert!(f.enter_dir(&p("docs")), "docs re-included");
        assert!(f.keep_file(&p("docs/guide.md")), "docs contents kept");
        assert!(!f.enter_dir(&p("src")), "everything else pruned");
        assert!(!f.keep_file(&p("src/a.md")), "unlisted file dropped");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let f = f("# a comment\n\ndocs/\n");
        assert!(!f.enter_dir(&p("docs")), "the real pattern still applies");
        assert!(f.keep_file(&p("src/a.md")), "comments add no patterns");
    }
}
