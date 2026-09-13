//! `recent_commits` reads a bounded, newest-first commit stream merged across
//! the workspace's working trees over the daemon socket, member-aware.
//!
//! Each row is keyed by its working-tree `tree` root plus the `members` in it,
//! and carries `author{name,email}`, `timestamp`, `subject`, `changed_files`,
//! and `trailers`. `members` filters to a subset (each mapped to its tree),
//! `limit` / `since` bound. A monorepo's members share one tree, so one commit
//! is one row tagged with them all. See [[spec - recent-commits activity stream
//! - a member-aware bounded commit stream with a reflog-watched append-only
//! subscription]].

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::{json, Value};

use crate::wire_fixtures::{harness, Harness};

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
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

fn git_config(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.name", "Tester"]);
    git(repo, &["config", "user.email", "tester@example.com"]);
}

/// Stage everything and commit at a FIXED committer date, so the merge order is
/// deterministic regardless of wall-clock (`recent_commits` sorts on committer
/// date, tie-broken by oid).
fn commit_all_at(repo: &Path, ts: i64, msg: &str) {
    git(repo, &["add", "-A"]);
    // The `@` prefix is git's raw-epoch date form; a bare number is rejected.
    let date = format!("@{ts} +0000");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-q", "-m", msg])
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn recent(h: &mut Harness, args: Value) -> Vec<Value> {
    h.payload("recent_commits", args)
        .as_array()
        .expect("recent_commits is an array")
        .clone()
}

/// Two separate working trees: the entry `v` (which ignores `app/`) and the
/// `app` edit member as its own git repo. So `v` and `app` are DISTINCT trees,
/// the cross-tree merge case. History is built before booting so no watcher
/// rebuild races the reads.
fn two_tree_kb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["app"]); // entry `v`, edit members [v, app]

    // `app` is its own repo and its own git tree.
    write(&root, "app/.arsumbris/repo.yaml", "name: app\n");
    write(&root, "app/hub.md", "app hub\n");
    let app = root.join("app");
    git_config(&app);
    commit_all_at(&app, 2000, "app: add hub\n\nMutation-Id: m-app");

    // `v`'s tree ignores `app/`, so the two trees stay disjoint.
    write(&root, ".gitignore", "app/\n");
    git_config(&root);
    commit_all_at(&root, 1000, "v: seed");

    (dir, root)
}

/// The stream merges both trees newest-first, and each row carries the full
/// shape: tree root, members, split author, timestamp, subject, changed_files,
/// and trailers.
#[test]
fn merges_two_trees_newest_first_with_row_shape() {
    let (_d, root) = two_tree_kb();
    let mut h = harness(&root);
    let rows = recent(&mut h, json!({}));
    assert_eq!(rows.len(), 2, "one commit per tree, merged: {rows:?}");

    // Newest is app@2000, then v@1000.
    let app = &rows[0];
    assert_eq!(app["subject"], "app: add hub");
    assert!(app["tree"].as_str().unwrap().ends_with("app"), "{app}");
    assert_eq!(app["members"], json!(["app"]));
    assert_eq!(app["author"]["name"], "Tester");
    assert_eq!(app["author"]["email"], "tester@example.com");
    assert_eq!(app["timestamp"].as_i64().unwrap(), 2000);
    // changed_files: hub.md added.
    let files = app["changed_files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .any(|f| f["path"] == "hub.md" && f["status"] == "added"),
        "changed_files: {files:?}"
    );
    // trailers: the Mutation-Id surfaces, uninterpreted.
    let trailers = app["trailers"].as_array().unwrap();
    assert!(
        trailers
            .iter()
            .any(|t| t["key"] == "Mutation-Id" && t["value"] == "m-app"),
        "trailers: {trailers:?}"
    );

    let v = &rows[1];
    assert_eq!(v["subject"], "v: seed");
    assert_eq!(v["members"], json!(["v"]));
    assert!(!v["tree"].as_str().unwrap().ends_with("app"), "{v}");
}

/// `limit` cuts the merged stream to the newest N; `members` scopes to the
/// trees the named members live in.
#[test]
fn limit_bounds_and_members_filter_scopes() {
    let (_d, root) = two_tree_kb();
    let mut h = harness(&root);

    // limit 1 keeps only the newest across all trees (app@2000).
    let one = recent(&mut h, json!({ "limit": 1 }));
    assert_eq!(one.len(), 1, "{one:?}");
    assert_eq!(one[0]["subject"], "app: add hub");

    // members: ["v"] scopes to v's tree only, so app's commit is excluded.
    let only_v = recent(&mut h, json!({ "members": ["v"] }));
    assert_eq!(only_v.len(), 1, "{only_v:?}");
    assert_eq!(only_v[0]["subject"], "v: seed");
    assert_eq!(only_v[0]["members"], json!(["v"]));

    // members: ["app"] scopes to app's tree only.
    let only_app = recent(&mut h, json!({ "members": ["app"] }));
    assert_eq!(only_app.len(), 1, "{only_app:?}");
    assert_eq!(only_app[0]["subject"], "app: add hub");
}

/// A monorepo tree holding several members yields one row per commit, tagged
/// with every member that lives in the tree (the working-tree dedup).
#[test]
fn a_monorepo_tree_tags_all_its_members() {
    // `multi_member_kb` nests `app` and `base` inside the entry `v`.
    let (_d, root) = crate::wire_fixtures::multi_member_kb();
    // One git tree over the whole thing, so all members share it.
    git_config(&root);
    commit_all_at(&root, 3000, "monorepo: seed");

    let mut h = harness(&root);
    let rows = recent(&mut h, json!({}));
    // One tree, so one commit is one row.
    assert_eq!(
        rows.len(),
        1,
        "a single tree yields one row per commit: {rows:?}"
    );
    let members = rows[0]["members"].as_array().unwrap();
    // Every member sharing the tree is tagged: the entry plus app plus base.
    for name in ["v", "app", "base"] {
        assert!(
            members.iter().any(|m| m == name),
            "member '{name}' missing from {members:?}"
        );
    }
}

/// The subscription seeds with the current page, then streams a `commits-appeared`
/// event when a new commit lands out-of-band — the reflog watcher's liveness,
/// with no rebuild involved.
#[test]
fn subscription_seeds_then_streams_an_out_of_band_commit() {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name.
    let root = std::fs::canonicalize(dir.path()).unwrap().join("v");
    std::fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    git_config(&root);
    commit_all_at(&root, 1000, "v: seed");

    let sock_dir = tempfile::tempdir().unwrap();
    let socket_path = sock_dir.path().join("s");

    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild(); // ready
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "recent_commits", "id": "rc-1" }))
        .unwrap();

    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["channel"], "recent_commits");
    assert_eq!(ack["accepted"], true);

    // The seed page carries the current commit.
    let init = client.recv().unwrap().unwrap();
    assert_eq!(init["type"], "initial_value");
    let seed = init["result"].as_array().unwrap();
    assert_eq!(seed.len(), 1, "{seed:?}");
    assert_eq!(seed[0]["subject"], "v: seed");

    // Let the reflog watcher's fsevent stream finish starting before the commit,
    // so the event is not missed by a stream that has not begun.
    std::thread::sleep(Duration::from_millis(500));

    // A new commit out-of-band (no engine mutation, no rebuild).
    std::fs::write(root.join("b.md"), "x\n").unwrap();
    commit_all_at(&root, 2000, "v: add b");

    // The reflog watcher fires; the subscription re-logs and streams the commit.
    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["type"], "change_event");
    assert_eq!(ev["kind"], "commits-appeared");
    let commits = ev["scope_hint"]["commits"].as_array().unwrap();
    assert!(
        commits.iter().any(|c| c["subject"] == "v: add b"),
        "the new commit streams: {commits:?}"
    );
}

/// Regression for the multi-tree leak (codereview 2609100225 finding 1.1): a
/// commit that fell BELOW the seed's global cut in another tree must NOT be
/// emitted as newly-appeared when a different tree gets a new commit. The dedup
/// set is the full per-tree union, not the cut seed page.
#[test]
fn subscription_does_not_leak_below_cut_commits_across_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("v");
    std::fs::create_dir_all(&root).unwrap();
    crate::seed_workspace(&root, &["app"]); // entry `v`, edit members [v, app]

    // `app` is its own git tree with one OLD commit (below the cut at limit 2).
    write(&root, "app/.arsumbris/repo.yaml", "name: app\n");
    write(&root, "app/old.md", "old\n");
    let app = root.join("app");
    git_config(&app);
    commit_all_at(&app, 1000, "app: old");

    // `v` ignores `app/` and has two commits, both newer than app's.
    write(&root, ".gitignore", "app/\n");
    git_config(&root);
    commit_all_at(&root, 2000, "v: seed");
    write(&root, "v-two.md", "two\n");
    commit_all_at(&root, 3000, "v: two");

    let sock_dir = tempfile::tempdir().unwrap();
    let socket_path = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket_path).expect("serve");

    let mut client = Client::connect(&socket_path).expect("connect");
    client
        .send(&json!({ "subscribe": "recent_commits", "limit": 2, "id": "rc-2" }))
        .unwrap();
    let ack = client.recv().unwrap().unwrap();
    assert_eq!(ack["channel"], "recent_commits");

    // The seed is the newest 2 globally: v's two commits; app's old commit is
    // below the cut and NOT in the page.
    let init = client.recv().unwrap().unwrap();
    let seed = init["result"].as_array().unwrap();
    assert_eq!(seed.len(), 2, "{seed:?}");
    assert!(
        !seed.iter().any(|c| c["subject"] == "app: old"),
        "app: old is below the cut, not in the seed page: {seed:?}"
    );

    std::thread::sleep(Duration::from_millis(500));

    // A new commit on v. The event must carry v's new commit and NOT app's old
    // one, which existed all along below the cut.
    write(&root, "v-three.md", "three\n");
    commit_all_at(&root, 4000, "v: three");

    let ev = client.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "commits-appeared");
    let commits = ev["scope_hint"]["commits"].as_array().unwrap();
    assert!(
        commits.iter().any(|c| c["subject"] == "v: three"),
        "the genuinely new commit streams: {commits:?}"
    );
    assert!(
        !commits.iter().any(|c| c["subject"] == "app: old"),
        "an old below-the-cut commit must NOT leak as newly-appeared: {commits:?}"
    );
}
