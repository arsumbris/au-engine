//! `file_history` reads a file's commit stream over the daemon socket,
//! member-aware.
//!
//! A raw pass-through of `git log --name-status -M --follow`: per commit it
//! returns the subject, author, timestamp, and a `status` / `from` that are
//! git's own rename similarity heuristic, surfaced as-is. Member-aware: git
//! resolves the covering working tree from the file's own directory, and a
//! named `repo` resolves the path against that member's root. See
//! [[spec - engine-mediated git reads - member-aware commit metadata and file
//! history over the object store]].

#![cfg(unix)]

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::wire_fixtures::{harness, Harness};

fn git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "tester@example.com"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "seed"]);
}

/// A seeded single folder-repo, git-initialized with a seed commit.
fn git_repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    git_init(&root);
    (dir, root)
}

fn file_history(h: &mut Harness, path: &str, repo: Option<&str>) -> Vec<Value> {
    let mut args = json!({ "path": path });
    if let Some(r) = repo {
        args["repo"] = json!(r);
    }
    h.payload("file_history", args)
        .as_array()
        .expect("file_history is an array")
        .clone()
}

/// A file's stream follows a rename: newest-first, the edit (`modified`), the
/// rename (`renamed` carrying its `from`), and the create (`added`), each with
/// author, subject, and timestamp. git's rename column is surfaced as-is.
#[test]
fn follows_a_file_across_a_rename() {
    let (_d, root) = git_repo();
    // Build the history BEFORE booting, so no watcher rebuild races the reads.
    std::fs::write(root.join("a.md"), "one\n").unwrap();
    git(&root, &["add", "a.md"]);
    git(&root, &["commit", "-q", "-m", "add a"]);
    git(&root, &["mv", "a.md", "b.md"]);
    git(&root, &["commit", "-q", "-m", "rename to b"]);
    std::fs::write(root.join("b.md"), "one\ntwo\n").unwrap();
    git(&root, &["add", "b.md"]);
    git(&root, &["commit", "-q", "-m", "edit b"]);

    let mut h = harness(&root);
    let rows = file_history(&mut h, "b.md", None);
    assert_eq!(rows.len(), 3, "{rows:?}");

    assert_eq!(rows[0]["status"], "modified");
    assert!(rows[0]["message"].as_str().unwrap().contains("edit b"));
    assert!(rows[0]["author"].as_str().unwrap().contains("Tester"));
    assert!(rows[0]["timestamp"].as_i64().unwrap() > 1_600_000_000);
    assert!(
        rows[0].get("from").is_none(),
        "a modify has no from: {}",
        rows[0]
    );

    assert_eq!(rows[1]["status"], "renamed", "{}", rows[1]);
    assert_eq!(rows[1]["from"], "a.md");

    assert_eq!(rows[2]["status"], "added", "{}", rows[2]);
    assert!(rows[2].get("from").is_none());
}

/// A path with no history yields an empty stream, not an error.
#[test]
fn a_path_with_no_history_is_empty() {
    let (_d, root) = git_repo();
    let mut h = harness(&root);
    assert!(
        file_history(&mut h, "never-existed.md", None).is_empty(),
        "an untracked path has no history"
    );
}

/// An unknown `repo` cannot be located, so the stream is empty, not an error.
#[test]
fn an_unknown_repo_is_empty() {
    let (_d, root) = git_repo();
    let mut h = harness(&root);
    assert!(file_history(&mut h, "README.md", Some("nope")).is_empty());
}

/// `repo` routes the path against that member's OWN root. `content/app-hub.md`
/// resolves inside member `app`; the same path relative to the entry (no `repo`)
/// resolves to a `content/` the thin entry does not have, so it is empty. The
/// member-awareness: the resolved location, not the entry, decides the store.
#[test]
fn a_named_member_routes_the_path_to_its_own_root() {
    let (_d, root) = crate::wire_fixtures::multi_member_kb();
    git_init(&root);
    let mut h = harness(&root);

    let in_app = file_history(&mut h, "content/app-hub.md", Some("app"));
    assert!(
        !in_app.is_empty(),
        "app-hub.md has history in app: {in_app:?}"
    );
    assert_eq!(in_app[0]["status"], "added");

    let in_entry = file_history(&mut h, "content/app-hub.md", None);
    assert!(
        in_entry.is_empty(),
        "the thin entry has no content/ dir, so the entry-relative path is empty: {in_entry:?}"
    );
}
