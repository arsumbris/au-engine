//! `commit_meta` reads per-commit metadata over the daemon socket, member-aware.
//!
//! The git-enrichment join partner for `pins`: per commit it returns timestamp,
//! author, message, and every trailer (uninterpreted), plus an `available` flag
//! for a commit the member's store does not hold. The result is POSITIONAL, one
//! record per input in order, so an absent commit is `available: false` rather
//! than dropped. See [[spec - engine-mediated git reads - member-aware commit
//! metadata and file history over the object store]].

#![cfg(unix)]

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::wire_fixtures::{harness, Harness};

fn git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A seeded folder-repo, git-initialized with one seed commit, and a booted
/// daemon over it. The entry's declared name is `v` (see `seed_repo`).
fn started_in_git() -> (tempfile::TempDir, PathBuf, Harness) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Tester"]);
    git(&root, &["config", "user.email", "tester@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    let h = harness(&root);
    (dir, root, h)
}

fn commit_meta(h: &mut Harness, commits: Value) -> Vec<Value> {
    h.payload("commit_meta", json!({ "commits": commits }))
        .as_array()
        .expect("commit_meta is an array")
        .clone()
}

/// An engine-written commit round-trips its metadata and its trailers. A
/// mediated `write_file` commits with author `au-engine` and a `Mutation-Id`
/// trailer, and `commit_meta` reads them back uninterpreted — the read half of
/// driver 1 (attribution lookup).
#[test]
fn reads_an_engine_written_commit_with_its_trailers() {
    let (_d, _root, mut h) = started_in_git();
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "note.md",
            "content": "# hi\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    let sha = resp["result"]["commit"]
        .as_str()
        .expect("a committing mutation anchors a commit")
        .to_string();

    let recs = commit_meta(&mut h, json!([{ "commit": sha }]));
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r["available"], true, "{r}");
    assert_eq!(r["commit"], sha);
    assert!(
        r["timestamp"].as_i64().unwrap() > 1_600_000_000,
        "a sane unix date: {r}"
    );
    assert_eq!(r["author"], "au-engine <au-engine@arsumbris.ai>");

    let trailers = r["trailers"].as_array().expect("trailers is an array");
    let mid = trailers
        .iter()
        .find(|t| t["key"] == "Mutation-Id")
        .unwrap_or_else(|| panic!("a Mutation-Id trailer: {trailers:?}"));
    assert!(
        mid["value"].as_str().is_some_and(|v| !v.is_empty()),
        "the Mutation-Id carries a value: {mid}"
    );
    assert!(
        trailers.iter().any(|t| t["key"] == "Mutation-Members"),
        "the member trailer surfaces too: {trailers:?}"
    );
}

/// A caller attribution payload on a `write_file` folds into the commit as
/// trailers, and `commit_meta` reads them back beside the engine's own — the
/// full driver-1 loop, attribution supplied on the write, looked up on the read.
#[test]
fn write_attribution_round_trips_through_commit_meta() {
    let (_d, _root, mut h) = started_in_git();
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "note.md",
            "content": "# hi\n",
            "attribution": [
                { "key": "session", "value": "sess-1" },
                { "key": "span", "value": "span-9" },
            ],
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    let sha = resp["result"]["commit"].as_str().unwrap().to_string();

    let recs = commit_meta(&mut h, json!([{ "commit": sha }]));
    let trailers = recs[0]["trailers"].as_array().unwrap();
    let has = |k: &str, v: &str| trailers.iter().any(|t| t["key"] == k && t["value"] == v);
    assert!(has("session", "sess-1"), "{trailers:?}");
    assert!(has("span", "span-9"), "{trailers:?}");
    assert!(
        trailers.iter().any(|t| t["key"] == "Mutation-Id"),
        "the engine trailer survives beside the attribution: {trailers:?}"
    );
}

/// A caller attribution key colliding with a reserved engine key is refused
/// before any write, so a caller cannot forge an engine record and nothing
/// lands half-attributed.
#[test]
fn a_reserved_attribution_key_is_refused() {
    let (_d, root, mut h) = started_in_git();
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "note.md",
            "content": "# hi\n",
            "attribution": [ { "key": "Mutation-Id", "value": "forged" } ],
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "a reserved key rejects: {resp:?}");
    assert!(
        resp["error"].as_str().unwrap().contains("reserved"),
        "the reject names the reserved key: {resp:?}"
    );
    assert!(!root.join("note.md").exists(), "no file lands on a reject");
}

/// A commit the store does not hold is `available: false` with the metadata
/// fields omitted, never an error — the `pinned-commit-unavailable` shape.
#[test]
fn marks_an_absent_sha_unavailable() {
    let (_d, _root, mut h) = started_in_git();
    let ghost = "0".repeat(40);
    let recs = commit_meta(&mut h, json!([{ "commit": ghost }]));
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r["available"], false);
    assert_eq!(r["commit"], ghost, "an absent sha echoes the input");
    assert!(r.get("timestamp").is_none(), "no metadata when absent: {r}");
    assert!(r.get("author").is_none());
    assert!(r.get("message").is_none());
}

/// The result lines up with the request by position: a request mixing present
/// and absent commits keeps each record at its input index.
#[test]
fn is_positional_over_present_and_absent() {
    let (_d, root, mut h) = started_in_git();
    let head = git(&root, &["rev-parse", "HEAD"]);
    let ghost = "1".repeat(40);
    let recs = commit_meta(
        &mut h,
        json!([
            { "commit": ghost },
            { "commit": head },
            { "commit": ghost },
        ]),
    );
    assert_eq!(recs.len(), 3);
    assert_eq!(recs[0]["available"], false);
    assert_eq!(recs[1]["available"], true);
    assert_eq!(recs[1]["commit"], head);
    assert_eq!(recs[2]["available"], false);
}

/// `repo` routes a commit to that member's store. Naming the entry repo (`v`)
/// resolves; an unknown repo cannot be located, so its commit is `available:
/// false` rather than an error — the member-aware routing branch.
#[test]
fn routes_a_named_repo_and_flags_an_unknown_one() {
    let (_d, root, mut h) = started_in_git();
    let head = git(&root, &["rev-parse", "HEAD"]);

    let named = commit_meta(&mut h, json!([{ "commit": head, "repo": "v" }]));
    assert_eq!(
        named[0]["available"], true,
        "the named entry repo routes to its store: {named:?}"
    );

    let unknown = commit_meta(&mut h, json!([{ "commit": head, "repo": "nope" }]));
    assert_eq!(
        unknown[0]["available"], false,
        "an unknown repo cannot be located, so it is unavailable, not an error: {unknown:?}"
    );
}
