//! The mutation channel over the socket: write_file through the one
//! mediated path, synchronous rebuild, fresh-state responses.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  description: String\n",
    )
    .unwrap();
    fs::write(
        root.join("a.md"),
        "---\ntype: note\ndescription: original\n---\n",
    )
    .unwrap();
    (dir, root)
}

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    engine: Engine,
    _server: au_engine::ServeHandle,
    client: Client,
    socket: PathBuf,
    root: PathBuf,
}

fn started() -> Harness {
    let (dir, root) = fixture();
    start(dir, root)
}

/// Git-initialize the fixture with one seed commit, then start the engine. The
/// walker ignores `.git`, so the knowledge base loads as usual.
fn started_in_git() -> Harness {
    let (dir, root) = fixture();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Tester"]);
    git(&root, &["config", "user.email", "tester@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    start(dir, root)
}

fn start(dir: tempfile::TempDir, root: PathBuf) -> Harness {
    let root_kept = root.clone();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();
    let client = Client::connect(&socket).expect("connect");
    Harness {
        _dir: dir,
        _sock_dir: sock_dir,
        engine,
        _server: server,
        client,
        socket,
        root: root_kept,
    }
}

fn git(repo: &std::path::Path, args: &[&str]) {
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

fn git_out(repo: &std::path::Path, args: &[&str]) -> String {
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

#[test]
fn write_file_creates_rebuilds_and_answers_with_fresh_state() {
    let mut h = started();

    // Create a new instance through the channel.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: written through the channel\n---\n",
            "id": "m-1",
        }))
        .unwrap();
    assert_eq!(resp["type"], "response");
    assert_eq!(
        resp["id"], "m-1",
        "the mutation response echoes the request id"
    );
    assert_eq!(resp["ready"], true);
    assert_eq!(
        resp["version"], 2,
        "the synchronous rebuild advanced the version"
    );
    assert!(resp["result"]["path"].as_str().unwrap().ends_with("b.md"));
    assert!(
        resp["result"]["hash"].is_string(),
        "hash ready for the next expected_hash"
    );
    assert_eq!(
        resp["result"]["diagnostics"].as_array().unwrap().len(),
        0,
        "a clean write carries no diagnostics"
    );
    assert_eq!(
        resp["result"]["reflected"], true,
        "the synchronous rebuild caught up, so the response reflects the write"
    );

    // The next read is already current — no rebuild lag.
    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "b.md" }))
        .unwrap();
    assert!(resp["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("through the channel"));
}

#[test]
fn expected_hash_guards_against_stale_writes() {
    let mut h = started();

    // The current hash comes from resolve_target.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_target", "target": "a" }))
        .unwrap();
    let hash = resp["result"]["resolve_target"]["hash"]
        .as_str()
        .unwrap()
        .to_string();

    // A write with the fresh hash lands.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: updated\n---\n",
            "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let new_hash = resp["result"]["hash"].as_str().unwrap().to_string();
    assert_ne!(new_hash, hash);

    // Re-using the stale hash rejects and reports the current one.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: lost update\n---\n",
            "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    assert_eq!(
        resp["detail"]["current_hash"].as_str().unwrap(),
        new_hash,
        "the reject carries the current hash for a retry"
    );
    // Nothing was written.
    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    assert!(resp["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("updated"));
}

#[test]
fn content_read_hash_arms_the_write_guard() {
    let mut h = started();

    // The hash a consumer gets from a plain content read is guard-usable: it
    // is the same value `write_file`'s `expected_hash` compares against.
    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    let hash = resp["result"]["content"]["hash"]
        .as_str()
        .unwrap()
        .to_string();

    // A write carrying that hash passes the read-before-write guard and lands.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: saved via content hash\n---\n",
            "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(
        resp["ready"], true,
        "the content read's hash armed the guard, got {resp:?}"
    );

    // Re-using the now-stale content-read hash rejects, like any stale guard.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: lost\n---\n",
            "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");
}

#[test]
fn a_mutation_with_validation_errors_lands_and_reports_them() {
    let mut h = started();
    // `description` is required; this write violates it. Advisory: it lands.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "c.md",
            "content": "---\ntype: note\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "advisory — the write landed");
    let diags = resp["result"]["diagnostics"].as_array().unwrap();
    assert!(
        diags.iter().any(|d| d["code"] == "required-field-absent"),
        "the response carries the fresh diagnostics, got {diags:?}"
    );
}

#[test]
fn path_guards_reject_without_writing() {
    let mut h = started();
    for (path, reason) in [
        ("../outside.md", "escape"),
        (".arsumbris/cache.db", "cache"),
    ] {
        let resp = h
            .client
            .query(&json!({
                "mutate": "write_file", "path": path, "content": "x",
            }))
            .unwrap();
        assert_eq!(resp["type"], "error", "{reason} rejects, got {resp:?}");
    }
    // Unknown args and unknown primitives reject loudly.
    let resp = h
        .client
        .query(&json!({ "mutate": "write_file", "path": "x.md", "contnet": "x" }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    let resp = h
        .client
        .query(&json!({ "mutate": "delete_everything" }))
        .unwrap();
    assert_eq!(resp["type"], "error");
}

#[test]
fn a_subscriber_sees_one_change_event_at_the_response_version() {
    let mut h = started();
    let _ = &h.engine;

    // A second consumer watches `changes`.
    let mut watcher = Client::connect(&h.socket).expect("connect watcher");
    watcher.send(&json!({ "subscribe": "changes" })).unwrap();
    let ack = watcher.recv().unwrap().unwrap();
    assert_eq!(ack["type"], "ack");

    // The mutation lands; the originator's response names the version.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: b\n---\n",
        }))
        .unwrap();
    let at_version = resp["version"].as_u64().unwrap();

    // The watcher wakes exactly once, at that version, with the delta.
    let ev = watcher.recv().unwrap().unwrap();
    assert_eq!(ev["kind"], "knowledge-base-changed");
    assert_eq!(ev["at_version"].as_u64().unwrap(), at_version);
    let added = ev["scope_hint"]["added"].as_array().unwrap();
    assert!(added[0].as_str().unwrap().ends_with("b.md"));
}

#[test]
fn edit_file_replaces_exactly_and_guards_uniqueness() {
    let mut h = started();
    let _ = &h.engine;

    // A unique exact match lands and the rebuild is synchronous.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "description: original",
            "new_string": "description: edited",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "edit landed, got {resp:?}");
    assert_eq!(resp["version"], 2);
    assert_eq!(resp["result"]["reflected"], true);
    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    assert!(resp["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("description: edited"));

    // The stale old_string now misses — its own read-before-write guard.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "description: original",
            "new_string": "description: lost",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");

    // A non-unique match rejects naming the count; replace_all lifts it.
    h.client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: dup dup\n---\n",
        }))
        .unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "dup",
            "new_string": "one",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    assert_eq!(resp["detail"]["occurrences"], 2);
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "dup",
            "new_string": "one",
            "replace_all": true,
        }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let resp = h
        .client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    assert!(resp["result"]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("one one"));

    // Identical strings and missing files reject.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file", "path": "a.md",
            "old_string": "x", "new_string": "x",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file", "path": "missing.md",
            "old_string": "x", "new_string": "y",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error");
}

#[test]
fn assign_block_id_addresses_a_yaml_record_and_returns_a_resolving_ref() {
    let mut h = started();
    let _ = &h.engine;
    // A session-log shape: an events list of inline records, none addressed.
    fs::write(
        h.root.join("type/session-log.type.yaml"),
        "fields:\n  session: String\n  events?: sessionEvent[]\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/sessionEvent.type.yaml"),
        "fields:\n  at: String\n",
    )
    .unwrap();
    fs::write(
        h.root.join("s-001.yaml"),
        "type: session-log\nsession: \"s-001\"\nevents:\n  - type: sessionEvent\n    at: \"t1\"\n",
    )
    .unwrap();
    h.engine.rebuild();

    // The offset points inside the record (at its `at:` field).
    let content = fs::read_to_string(h.root.join("s-001.yaml")).unwrap();
    let at = content.find("at: \"t1\"").unwrap();
    let resp = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "s-001.yaml", "at": at }))
        .unwrap();
    assert_eq!(resp["ready"], true, "assigned, got {resp:?}");
    assert_eq!(resp["result"]["reflected"], true);
    let id = resp["result"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("b-"));
    assert_eq!(
        resp["result"]["ref"].as_str().unwrap(),
        format!("[[s-001^{id}]]")
    );

    // The id landed as the record's first key, hand-authored shape.
    let updated = fs::read_to_string(h.root.join("s-001.yaml")).unwrap();
    assert!(
        updated.contains(&format!("  - ^: {id}\n    type: sessionEvent")),
        "id rides the first-key line, got:\n{updated}"
    );

    // The returned ref resolves through the read surface.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id", "target": "s-001", "block_id": id,
        }))
        .unwrap();
    assert_eq!(resp["result"]["resolve_block_id"]["kind"], "record");

    // Assigning again at the same offset is idempotent: same id, no write.
    let at = updated.find("at: \"t1\"").unwrap();
    let resp = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "s-001.yaml", "at": at }))
        .unwrap();
    assert_eq!(resp["result"]["id"].as_str().unwrap(), id);
    assert_eq!(
        resp["result"]["reflected"], true,
        "an idempotent no-op wrote nothing, so there is nothing to lag"
    );
}

#[test]
fn assign_block_id_marks_a_markdown_block() {
    let mut h = started();
    let _ = &h.engine;
    fs::write(
        h.root.join("note.md"),
        "---\ntype: note\ndescription: d\n---\n# Head\n\nA finding worth citing.\n\nMore prose.\n",
    )
    .unwrap();
    h.engine.rebuild();

    let content = fs::read_to_string(h.root.join("note.md")).unwrap();
    let at = content.find("finding").unwrap();
    let resp = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "note.md", "at": at }))
        .unwrap();
    assert_eq!(resp["ready"], true, "marked, got {resp:?}");
    let id = resp["result"]["id"].as_str().unwrap().to_string();

    let updated = fs::read_to_string(h.root.join("note.md")).unwrap();
    assert!(
        updated.contains(&format!("A finding worth citing. ^{id}")),
        "trailing marker on the block, got:\n{updated}"
    );

    // The marker resolves as a navigational target.
    let resp = h
        .client
        .query(&json!({
            "read": "resolve_block_id", "target": "note", "block_id": id,
        }))
        .unwrap();
    assert_eq!(resp["result"]["resolve_block_id"]["kind"], "marker");
}

// A markdown block that already carries a `^id` marker must return that id
// unwritten on a re-assign, mirroring the inline-record path's idempotency. A
// retry (agent timeout, or not realizing the block is addressed) otherwise
// accumulates a second marker and "the block's id" becomes ambiguous.
// Regression for the assign_block_id report.
#[test]
fn assign_block_id_on_a_markdown_block_is_idempotent() {
    let mut h = started();
    let _ = &h.engine;
    fs::write(
        h.root.join("note.md"),
        "---\ntype: note\ndescription: d\n---\n# Head\n\nA finding worth citing.\n\nMore prose.\n",
    )
    .unwrap();
    h.engine.rebuild();

    let content = fs::read_to_string(h.root.join("note.md")).unwrap();
    let at = content.find("finding").unwrap();
    let first = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "note.md", "at": at }))
        .unwrap();
    assert_eq!(first["ready"], true, "first assign, got {first:?}");
    let id1 = first["result"]["id"].as_str().unwrap().to_string();
    let after_first = fs::read_to_string(h.root.join("note.md")).unwrap();

    // The block now carries a marker. Re-assign at the same offset: it must
    // return the SAME id and write nothing.
    h.engine.rebuild();
    let at2 = after_first.find("finding").unwrap();
    let second = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "note.md", "at": at2 }))
        .unwrap();
    assert_eq!(second["ready"], true, "second assign, got {second:?}");
    let id2 = second["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(id2, id1, "re-assign returned a different id: {second:?}");

    let after_second = fs::read_to_string(h.root.join("note.md")).unwrap();
    assert_eq!(
        after_first, after_second,
        "re-assign wrote a second marker:\n{after_second}"
    );
    assert_eq!(
        after_second.matches('^').count(),
        1,
        "expected exactly one marker on the block:\n{after_second}"
    );
}

#[test]
fn assign_block_id_rejects_an_offset_outside_any_addressable_entity() {
    let mut h = started();
    let _ = &h.engine;
    // a.md's frontmatter `description` scalar: no record encloses it, and
    // the offset is before the body.
    let resp = h
        .client
        .query(&json!({ "mutate": "assign_block_id", "path": "a.md", "at": 4 }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
}

#[test]
fn delete_file_removes_through_the_channel_and_rebuilds() {
    let mut h = started();
    assert!(h.root.join("a.md").exists(), "the seed file is present");

    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "a.md", "id": "d-1" }))
        .unwrap();
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["id"], "d-1", "the response echoes the request id");
    assert_eq!(resp["ready"], true);
    assert_eq!(
        resp["version"], 2,
        "the synchronous rebuild advanced the version"
    );
    assert!(resp["result"]["path"].as_str().unwrap().ends_with("a.md"));
    assert!(
        resp["result"]["hash"].is_null(),
        "the deleted file has no held hash"
    );
    assert!(!h.root.join("a.md").exists(), "the file is gone from disk");

    // Deleting again rejects: nothing to delete, no silent success.
    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
}

#[test]
fn delete_file_guards_against_a_stale_view() {
    let mut h = started();

    // The current hash from a resolve.
    let resp = h
        .client
        .query(&json!({ "read": "resolve_target", "target": "a" }))
        .unwrap();
    let hash = resp["result"]["resolve_target"]["hash"]
        .as_str()
        .unwrap()
        .to_string();

    // A stale hash rejects and leaves the file in place.
    let resp = h
        .client
        .query(&json!({
            "mutate": "delete_file", "path": "a.md", "expected_hash": "0000000000000000",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        h.root.join("a.md").exists(),
        "the rejected delete wrote nothing"
    );

    // The matching hash deletes.
    let resp = h
        .client
        .query(&json!({
            "mutate": "delete_file", "path": "a.md", "expected_hash": hash,
        }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    assert!(!h.root.join("a.md").exists());
}

#[test]
fn delete_file_in_git_surfaces_the_last_live_commit() {
    let mut h = started_in_git();
    // HEAD before the delete is the last commit where a.md still existed.
    let last_live_expected = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "a.md", "id": "d-1" }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert!(!h.root.join("a.md").exists(), "the file is gone from disk");

    // The deletion commit is HEAD now, and it is what `commit` / `commits` name.
    let deletion = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_ne!(deletion, last_live_expected, "a deletion commit landed");
    assert_eq!(
        resp["result"]["commit"].as_str().unwrap(),
        deletion,
        "`commit` anchors the deletion commit, the invariant intact"
    );
    let committed_shas: Vec<&str> = resp["result"]["commits"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        committed_shas.iter().all(|s| *s == deletion),
        "`commits` names the deletion commit: {resp:?}"
    );

    // The new field is the LAST-LIVE commit, the parent of the deletion commit.
    let last_live = resp["result"]["last_live_commit"]
        .as_str()
        .expect("a git delete surfaces the last-live commit");
    assert_eq!(
        last_live, last_live_expected,
        "last_live_commit is HEAD immediately before the delete"
    );
    assert_ne!(
        last_live, deletion,
        "the last-live commit is distinct from the deletion commit"
    );
    assert_eq!(
        git_out(&h.root, &["rev-parse", &format!("{deletion}^")]),
        last_live,
        "last_live_commit is the parent of the deletion commit"
    );

    // The file's last content reads back at the last-live commit, so the
    // tombstone pin resolves.
    let content = git_out(&h.root, &["show", &format!("{last_live}:a.md")]);
    assert!(
        content.contains("description: original"),
        "the last content is readable at last_live_commit: {content:?}"
    );
}

#[test]
fn delete_file_off_git_carries_no_last_live_commit() {
    let mut h = started();

    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert!(!h.root.join("a.md").exists());
    assert!(
        resp["result"]["last_live_commit"].is_null(),
        "off-git there is no deletion commit and no last-live to pin: {resp:?}"
    );
}

#[test]
fn delete_file_rejects_an_untracked_file_so_last_live_always_holds() {
    let mut h = started_in_git();
    // A never-committed working-tree-only file.
    fs::write(
        h.root.join("untracked.md"),
        "---\ntype: note\ndescription: never committed\n---\n",
    )
    .unwrap();

    // The clean-at-HEAD precondition rejects it: an untracked file shows as
    // dirty, so no deletion commit is ever made. This is what guarantees that a
    // delete which DOES commit always has a live parent to pin.
    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "untracked.md" }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an untracked file's delete rejects at clean-at-HEAD: {resp:?}"
    );
    assert!(
        h.root.join("untracked.md").exists(),
        "the rejected delete wrote nothing"
    );
}

#[test]
fn mutation_in_a_git_repo_commits_with_a_mutation_id() {
    let mut h = started_in_git();
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: committed through the channel\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The response carries the commit sha.
    let sha = resp["result"]["commit"]
        .as_str()
        .expect("a git-repo mutation reports its commit");
    assert_eq!(sha.len(), 40, "expected a full sha, got {sha:?}");

    // HEAD advanced to that commit, and it carries the Mutation-Id trailer.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(head, sha, "HEAD is the mutation's commit");
    assert_ne!(head, before, "a new commit landed");
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Mutation-Id: m-"), "trailer missing: {body}");

    // The working tree is clean: the write was committed, not left dirty.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after a committed mutation"
    );

    // A second mutation to a now-committed file also commits — clean-at-HEAD is
    // satisfied because the first mutation committed.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "b.md",
            "old_string": "committed through the channel",
            "new_string": "edited through the channel",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert!(
        resp["result"]["commit"].is_string(),
        "the edit committed too"
    );
}

/// A plain `write_file` into a nested member repo that has NO `.git` of its own
/// commits into the ENCLOSING parent tree, rather than landing on disk
/// written-but-uncommitted.
///
/// The regression this guards: saga grouping once keyed git-ness on the member
/// repo's OWN root (`root.join(".git").exists()`), so a nested folder-repo with
/// no `.git` was classed non-git — written and never committed. Grouping now
/// resolves each repo to its enclosing working tree, so the write commits to the
/// parent. The reporter's exact case: "just wrote a file in a subrepo that did
/// not have git."
#[test]
fn write_file_into_a_nested_non_git_subrepo_commits_to_the_parent_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();

    // Parent folder-repo `v`, composing a nested member `sub`. `v` owns the git
    // working tree; `sub` is its own folder-repo with NO `.git` of its own.
    crate::seed_repo(&root);
    fs::write(
        root.join(".arsumbris/workspace.yaml"),
        "edit:\n  - v\n  - sub\ntype: au.engine.workspace::au-engine\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("sub/.arsumbris")).unwrap();
    fs::write(root.join("sub/.arsumbris/repo.yaml"), "name: sub\n").unwrap();
    crate::seed_readme(&root.join("sub"));
    fs::create_dir_all(root.join("sub/type")).unwrap();
    fs::write(
        root.join("sub/type/note.type.yaml"),
        "fields:\n  description: String\n",
    )
    .unwrap();

    // Git-initialize the PARENT only, one seed commit covering `sub/` as tracked
    // subdirectories. `sub` has no `.git`, so it belongs to the parent's tree.
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Tester"]);
    git(&root, &["config", "user.email", "tester@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "seed"]);
    assert!(
        !root.join("sub/.git").exists(),
        "the nested member has no git of its own — the case under test"
    );

    let mut h = start(dir, root);
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "sub/b.md",
            "content": "---\ntype: note\ndescription: written into the nested subrepo\n---\n",
            "id": "m-nested-1",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The write landed on disk.
    assert!(
        h.root.join("sub/b.md").exists(),
        "the write landed in the nested member"
    );

    // It COMMITTED — the bug's symptom was a write with no commit at all.
    let sha = resp["result"]["commit"].as_str().expect(
        "the nested write must commit to the parent tree, not land written-but-uncommitted",
    );
    assert_eq!(sha.len(), 40, "expected a full sha, got {sha:?}");

    // The commit is reported under the nested repo `sub`, sharing the parent sha.
    let commits = resp["result"]["commits"].as_object().unwrap();
    assert_eq!(
        commits.get("sub").and_then(|v| v.as_str()),
        Some(sha),
        "`commits` names the nested repo `sub` with the parent-tree sha: {resp:?}"
    );

    // The PARENT tree's HEAD advanced to that commit, carrying the Mutation-Id.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(head, sha, "the parent tree's HEAD is the mutation commit");
    assert_ne!(head, before, "a new commit landed in the parent tree");
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Mutation-Id: m-"), "trailer missing: {body}");

    // The commit holds the nested path, rebased onto the tree (`sub/b.md`).
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        files.lines().any(|l| l == "sub/b.md"),
        "the parent commit holds the nested path rebased onto the tree: {files}"
    );

    // The working tree is clean: the nested write was committed, not left dirty.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after a committed nested write"
    );
}

#[test]
fn write_file_stamp_lands_the_record_in_one_commit() {
    let mut h = started_in_git();
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n# N\n",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.create", "session": "[[s-1.spans]]" },
                "match_on": { "type": "file-change.create" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // Exactly one commit landed, carrying BOTH the content and the stamp.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "the write and its stamp are one commit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:n.md")]);
    assert!(committed.contains("description: hi"), "{committed}");
    assert!(
        committed.contains("provenance:") && committed.contains("- type: file-change.create"),
        "the stamp rides the write's commit: {committed}"
    );

    // The response hash reflects the STAMPED bytes, not the pre-stamp content.
    assert_eq!(
        resp["result"]["reflected"], true,
        "the response reflects the stamped content: {resp:?}"
    );
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree is clean after the stamped write"
    );
}

#[test]
fn edit_file_stamp_dedups_per_session() {
    let mut h = started_in_git();
    // Seed a file already carrying one edit entry for session s-1.
    h.client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: v1\n---\n",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();

    // A second edit in the SAME session: the stamp dedups, so the content
    // changes but there is still exactly one edit entry.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "n.md",
            "old_string": "v1", "new_string": "v2",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let content = fs::read_to_string(h.root.join("n.md")).unwrap();
    assert!(content.contains("description: v2"), "{content}");
    assert_eq!(
        content.matches("file-change.edit").count(),
        1,
        "a repeat edit in one session keeps a single entry: {content}"
    );

    // A DIFFERENT session appends a second entry.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "n.md",
            "old_string": "v2", "new_string": "v3",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-2.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-2.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let content = fs::read_to_string(h.root.join("n.md")).unwrap();
    assert_eq!(
        content.matches("file-change.edit").count(),
        2,
        "a new session appends a fresh entry: {content}"
    );
}

#[test]
fn write_file_stamp_applies_off_git() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.create", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert!(
        resp["result"]["commit"].is_null(),
        "off-git there is no commit: {resp:?}"
    );
    let content = fs::read_to_string(h.root.join("n.md")).unwrap();
    assert!(
        content.contains("provenance:") && content.contains("file-change.create"),
        "the stamp applied to disk off-git: {content}"
    );
}

#[test]
fn write_file_two_stamps_into_different_fields_land_in_one_commit() {
    let mut h = started_in_git();
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    // Two stampers on one write, into DIFFERENT fields: independent ensures.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "stamps": [
                {
                    "field": "provenance",
                    "record": { "type": "file-change.create", "session": "[[s-1.spans]]" }
                },
                { "field": "reviewed_by", "record": { "who": "agent-x" } }
            ]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // Both stamps ride the write's SINGLE commit.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "both stamps fold into one commit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:n.md")]);
    assert!(
        committed.contains("provenance:") && committed.contains("file-change.create"),
        "the first stamp landed: {committed}"
    );
    assert!(
        committed.contains("reviewed_by:") && committed.contains("who: agent-x"),
        "the second stamp landed in its own field: {committed}"
    );
}

#[test]
fn write_file_two_stamps_into_the_same_field_append_in_order_one_commit() {
    let mut h = started_in_git();
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    // Two stamps into the SAME field: N order-stable appends into one list.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "stamps": [
                {
                    "field": "provenance",
                    "record": { "type": "file-change.create", "session": "[[s-1.spans]]" }
                },
                {
                    "field": "provenance",
                    "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
                }
            ]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "both same-field stamps fold into one commit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:n.md")]);
    // Two entries in ONE provenance list, in list order (create before edit).
    assert_eq!(
        committed.matches("- type: file-change").count(),
        2,
        "two entries in one provenance list: {committed}"
    );
    let create_at = committed
        .find("file-change.create")
        .expect("create present");
    let edit_at = committed.find("file-change.edit").expect("edit present");
    assert!(
        create_at < edit_at,
        "the second stamp appends after the first, in list order: {committed}"
    );
}

#[test]
fn a_stamp_list_applies_only_its_non_deduped_entries() {
    let mut h = started_in_git();
    // Seed a file already carrying one create entry.
    h.client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: v1\n---\n",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.create", "session": "[[s-1.spans]]" },
                "match_on": { "type": "file-change.create" }
            }]
        }))
        .unwrap();

    // An edit whose list RE-ASSERTS create (dedups to a no-op) and ADDS an edit
    // (applies): the first entry contributes nothing, the second appends.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "n.md",
            "old_string": "v1", "new_string": "v2",
            "stamps": [
                {
                    "field": "provenance",
                    "record": { "type": "file-change.create", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.create" }
                },
                {
                    "field": "provenance",
                    "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
                }
            ]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let content = fs::read_to_string(h.root.join("n.md")).unwrap();
    assert!(content.contains("description: v2"), "{content}");
    assert_eq!(
        content.matches("file-change.create").count(),
        1,
        "the create entry deduped, not duplicated: {content}"
    );
    assert_eq!(
        content.matches("file-change.edit").count(),
        1,
        "the fresh edit entry applied: {content}"
    );
}

#[test]
fn an_empty_stamps_list_is_exactly_no_stamp() {
    let mut h = started_in_git();
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "stamps": []
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let content = fs::read_to_string(h.root.join("n.md")).unwrap();
    assert!(
        !content.contains("provenance"),
        "an empty stamps list adds no field: {content}"
    );
    assert!(
        content.contains("description: hi"),
        "the primary write still landed: {content}"
    );
}

#[test]
fn a_fully_deduped_stamp_list_leaves_the_commit_to_the_primary_write() {
    let mut h = started_in_git();
    // Seed with BOTH a create and an edit entry, in one write.
    h.client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: v1\n---\n",
            "stamps": [
                {
                    "field": "provenance",
                    "record": { "type": "file-change.create", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.create" }
                },
                {
                    "field": "provenance",
                    "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
                }
            ]
        }))
        .unwrap();
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    // Edit the content; both stamps re-assert the existing entries, so BOTH
    // dedup. The commit carries only the primary content change.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "n.md",
            "old_string": "v1", "new_string": "v2",
            "stamps": [
                {
                    "field": "provenance",
                    "record": { "type": "file-change.create", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.create" }
                },
                {
                    "field": "provenance",
                    "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                    "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
                }
            ]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "one commit, the primary edit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:n.md")]);
    assert!(
        committed.contains("description: v2"),
        "the content change landed: {committed}"
    );
    assert_eq!(
        committed.matches("- type: file-change").count(),
        2,
        "both stamps deduped, so still exactly two entries: {committed}"
    );
}

#[test]
fn clean_at_head_rejects_an_uncommitted_working_tree_change() {
    let mut h = started_in_git();

    // An unmediated edit dirties the file behind the engine's back.
    fs::write(
        h.root.join("a.md"),
        "---\ntype: note\ndescription: dirtied out of band\n---\n",
    )
    .unwrap();

    // A channel mutation to that path is rejected by the clean-at-HEAD precondition.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "dirtied out of band",
            "new_string": "mediated",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("uncommitted changes"),
        "expected a clean-at-HEAD rejection, got {resp:?}"
    );
}

#[test]
fn concurrent_mutations_stay_consistent() {
    // Several connections fire mutations at once. The write pipeline serializes
    // them, so all commit cleanly with distinct Mutation-Ids and a clean tree.
    // The serialization guarantee is structural (the write lock); the marker's
    // crash-recovery soundness only manifests on a crash mid-interleave, which
    // is not deterministically testable, so this is a consistency regression
    // guard, not a proof of serialization.
    let h = started_in_git();
    let n = 8usize;

    let workers: Vec<_> = (0..n)
        .map(|i| {
            let socket = h.socket.clone();
            std::thread::spawn(move || {
                let mut client = au_engine::Client::connect(&socket).expect("connect");
                client
                    .query(&json!({
                        "mutate": "write_file",
                        "path": format!("f{i}.md"),
                        "content": format!("---\ntype: note\ndescription: file {i}\n---\n"),
                    }))
                    .unwrap()
            })
        })
        .collect();
    let responses: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();

    // Every mutation committed, with a distinct sha.
    let mut shas = std::collections::HashSet::new();
    for resp in &responses {
        assert_eq!(
            resp["ready"], true,
            "a concurrent mutation failed: {resp:?}"
        );
        let sha = resp["result"]["commit"]
            .as_str()
            .unwrap_or_else(|| panic!("a concurrent mutation did not commit: {resp:?}"));
        shas.insert(sha.to_string());
    }
    assert_eq!(
        shas.len(),
        n,
        "expected {n} distinct commits, got {}",
        shas.len()
    );

    // The tree is clean and every file landed.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the tree is dirty after concurrent mutations"
    );
    for i in 0..n {
        assert!(h.root.join(format!("f{i}.md")).exists(), "f{i}.md missing");
    }
    // Exactly n mutation commits on top of the seed.
    let count: usize = git_out(&h.root, &["rev-list", "--count", "HEAD"])
        .parse()
        .unwrap();
    assert_eq!(count, n + 1, "expected {} commits (seed + {n})", n + 1);
}

#[test]
fn a_no_op_write_makes_no_commit() {
    let mut h = started_in_git();
    let head_before = git_out(&h.root, &["rev-parse", "HEAD"]);

    // Re-write a.md with its existing content: a no-op.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "a.md",
            "content": "---\ntype: note\ndescription: original\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "a no-op write succeeds: {resp:?}");
    // The no-op committed nothing: the provenance map is empty.
    assert!(
        resp["result"]["commits"]
            .as_object()
            .expect("commits present")
            .is_empty(),
        "a no-op write's commits map is empty: {resp:?}"
    );
    // But `commit` is the anchor, not the provenance: it is the unchanged HEAD,
    // a valid pin anchor, not null. (Null would mean off-git.)
    assert_eq!(
        resp["result"]["commit"]
            .as_str()
            .expect("commit is the anchor"),
        head_before,
        "a no-op's commit anchors the unchanged HEAD: {resp:?}"
    );

    // HEAD did not advance, and the tree stays clean.
    assert_eq!(
        git_out(&h.root, &["rev-parse", "HEAD"]),
        head_before,
        "a no-op write advanced HEAD"
    );
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "a no-op write left the tree dirty"
    );
}

#[test]
fn single_repo_mutate_carries_a_one_entry_commits_map() {
    let mut h = started_in_git();

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: committed\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The commits map carries the single committing repo; the `commit` scalar is
    // that same sha, and HEAD advanced to it.
    let commit = resp["result"]["commit"]
        .as_str()
        .expect("a git-repo mutation reports its commit");
    let commits = resp["result"]["commits"]
        .as_object()
        .expect("commits map present");
    assert_eq!(commits.len(), 1, "one repo committed: {commits:?}");
    let only = commits.values().next().unwrap().as_str().unwrap();
    assert_eq!(only, commit, "the map's single sha equals the scalar");
    assert_eq!(
        commit,
        git_out(&h.root, &["rev-parse", "HEAD"]),
        "commit is the repo's HEAD"
    );
}

#[test]
fn off_git_mutate_has_null_commit_and_empty_commits() {
    // `started()` is a non-git fixture: the write lands, nothing commits.
    let mut h = started();

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: written off git\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "an off-git write succeeds: {resp:?}");
    assert!(
        h.root.join("b.md").exists(),
        "the off-git write did not land"
    );

    // `commit` is present-but-null off git, symmetric with the content read;
    // `commits` is an empty object, not absent.
    assert!(
        resp["result"]["commit"].is_null(),
        "commit is null off git: {resp:?}"
    );
    assert!(
        resp["result"]["commits"]
            .as_object()
            .expect("commits present off git")
            .is_empty(),
        "commits is empty off git: {resp:?}"
    );
}

#[test]
fn rename_moves_a_referrerless_file_and_commits() {
    let mut h = started_in_git();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The move committed as one mutation, with a Mutation-Id trailer.
    let sha = resp["result"]["commit"]
        .as_str()
        .expect("a rename in a git repo reports its commit");
    assert_eq!(sha.len(), 40, "expected a full sha, got {sha:?}");
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Mutation-Id: m-"), "trailer missing: {body}");
    // And the authoritative move record the forward trace reads.
    assert!(
        body.contains("Moved: a.md -> c.md"),
        "Moved: trailer missing: {body}"
    );

    // The file moved on disk, content intact, old path gone.
    assert!(
        !h.root.join("a.md").exists(),
        "the old path is still present"
    );
    assert!(h.root.join("c.md").exists(), "the new path is missing");
    assert_eq!(
        fs::read_to_string(h.root.join("c.md")).unwrap(),
        "---\ntype: note\ndescription: original\n---\n",
        "the content moved intact"
    );

    // The move was committed, so the working tree is clean.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after a committed rename"
    );

    // The response points at the new path.
    assert!(
        resp["result"]["path"].as_str().unwrap().ends_with("c.md"),
        "got {resp:?}"
    );
}

#[test]
fn rename_stamp_records_the_rename_on_the_moved_file_in_one_commit() {
    let mut h = started_in_git();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
            "stamps": [{
                "field": "provenance",
                "record": {
                    "type": "file-change.rename",
                    "session": "[[s-9.spans]]",
                    "from": "a.md"
                }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The moved file carries the rename entry, with its frozen `from`.
    let moved = fs::read_to_string(h.root.join("c.md")).unwrap();
    assert!(
        moved.contains("provenance:")
            && moved.contains("- type: file-change.rename")
            && moved.contains("from: a.md"),
        "the moved file records the rename: {moved}"
    );

    // One commit carries BOTH the move (its `Moved:` trailer) and the stamp.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Moved: a.md -> c.md"),
        "the move and stamp are one commit: {body}"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:c.md")]);
    assert!(
        committed.contains("file-change.rename"),
        "the stamp is in the rename's commit: {committed}"
    );
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "clean after the stamped rename"
    );
}

#[test]
fn rename_to_an_existing_path_rejects() {
    let mut h = started_in_git();

    // A clean-at-HEAD sibling at the destination.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: other\n---\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add b"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "b.md",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"].as_str().unwrap().contains("already exists"),
        "got {resp:?}"
    );
    assert!(h.root.join("a.md").exists(), "a.md should be untouched");
}

#[test]
fn rename_rewrites_an_inbound_reference_in_the_same_commit() {
    let mut h = started_in_git();

    // A committed referrer to a.md via a body wikilink.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: refers\n---\nsee [[a]]\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The file moved.
    assert!(
        !h.root.join("a.md").exists(),
        "the old path is still present"
    );
    assert!(h.root.join("c.md").exists(), "the new path is missing");

    // The referrer now points at the new name, not the old.
    let referrer = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        referrer.contains("[[c]]"),
        "the referrer was not rewritten: {referrer}"
    );
    assert!(
        !referrer.contains("[[a]]"),
        "the old reference survived: {referrer}"
    );

    // The move and the rewrite are one commit, and the tree is clean.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after the rename"
    );
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        files.contains("c.md"),
        "the moved file is not in the commit: {files}"
    );
    assert!(
        files.contains("b.md"),
        "the rewritten referrer is not in the commit: {files}"
    );
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the move and rewrite should share one Mutation-Id: {body}"
    );
}

#[test]
fn rename_rewrites_a_reference_inside_a_body_fence_record() {
    // Regression: a reference that is a FIELD VALUE inside a marked record fence
    // is a real edge, so rename must rewrite it. It was previously unindexed, so
    // rename silently stranded it, leaving the graph broken.
    let mut h = started_in_git();

    // Types for a fence-record contribution: a host with a record slot, filled
    // by a `[:slot]` fence whose `step` record holds a `file*` reference.
    fs::write(
        h.root.join("type/step.type.yaml"),
        "fields:\n  ref: file*\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/host.type.yaml"),
        "fields:\n  slot: step\n",
    )
    .unwrap();
    fs::write(
        h.root.join("b.md"),
        "---\ntype: host\nslot:\n---\n\n```yaml [:slot]\ntype: step\nref: \"[[a]]\"\n```\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add fence referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({ "mutate": "rename", "path": "a.md", "to": "c.md" }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The fence's reference now points at the new name, not the old.
    let referrer = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        referrer.contains("[[c]]"),
        "the fence reference was not rewritten: {referrer}"
    );
    assert!(
        !referrer.contains("[[a]]"),
        "the old fence reference survived: {referrer}"
    );
}

#[test]
fn rename_preserves_a_referrers_extension_spelling() {
    let mut h = started_in_git();

    // The referrer names a.md by its full basename, the with-extension spelling.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: refers\n---\nsee [[a.md]]\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The extension spelling is preserved: [[a.md]] becomes [[c.md]], not [[c]].
    let referrer = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        referrer.contains("[[c.md]]"),
        "the extension spelling was not preserved: {referrer}"
    );
    assert!(
        !referrer.contains("[[a.md]]"),
        "the old reference survived: {referrer}"
    );
}

#[test]
fn rename_preserves_fragments_and_attribution_on_referrers() {
    let mut h = started_in_git();

    // One referrer with a navigational anchor and a body-contribution
    // attribution, both naming a.md.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: refers\n---\nnav [[a#head]] and contrib [[a:role]]\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // Only the name changes; the #anchor and :field ride along (recovered by
    // re-parsing the source, since the index does not store the anchor).
    let referrer = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        referrer.contains("[[c#head]]"),
        "the anchor was not preserved: {referrer}"
    );
    assert!(
        referrer.contains("[[c:role]]"),
        "the attribution was not preserved: {referrer}"
    );
    assert!(
        !referrer.contains("[[a#"),
        "the old anchored reference survived: {referrer}"
    );
}

#[test]
fn rename_rejects_a_type_def_file() {
    let mut h = started_in_git();

    // The fixture's note type-def. Renaming it would rename the type "note",
    // which `rename` cannot do (the claims are not wikilinks).
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "type/note.type.yaml",
            "to": "type/renamed.type.yaml",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"].as_str().unwrap().contains("rename_type"),
        "expected a type-def refusal pointing at rename_type, got {resp:?}"
    );
    assert!(
        h.root.join("type/note.type.yaml").exists(),
        "the type-def must be untouched"
    );
}

#[test]
fn rename_type_cascades_a_claim_and_moves_the_def_file() {
    let mut h = started_in_git();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The def file moved: the type name derives from the filename.
    assert!(
        !h.root.join("type/note.type.yaml").exists(),
        "the old def file is still present"
    );
    assert!(
        h.root.join("type/memo.type.yaml").exists(),
        "the new def file is missing"
    );

    // The instance's `type:` claim cascaded — a type-name reference, not a
    // wikilink, so the file rename's rewrite could never touch it.
    let inst = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert!(
        inst.contains("type: memo"),
        "the claim was not rewritten: {inst}"
    );
    assert!(
        !inst.contains("type: note"),
        "the old claim survived: {inst}"
    );

    // The move and every rewrite are one commit, the tree clean.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after the rename"
    );
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the move and cascade should share one Mutation-Id: {body}"
    );
    // The forward-trace `Moved:` trailer records the def-file move, so a pinned
    // `type<T>*@` whose def was renamed can be traced.
    assert!(
        body.contains("Moved:"),
        "the def-file move should carry a Moved: trailer: {body}"
    );
}

#[test]
fn rename_type_rewrites_a_slot_reference() {
    let mut h = started_in_git();

    // A second type-def whose slot references `note` by name (a shape token, in
    // a `.type.yaml`, not a wikilink).
    fs::write(
        h.root.join("type/tagged.type.yaml"),
        "fields:\n  ref: note*\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add tagged"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let td = fs::read_to_string(h.root.join("type/tagged.type.yaml")).unwrap();
    assert!(
        td.contains("ref: memo*"),
        "the slot was not rewritten: {td}"
    );
    assert!(!td.contains("note*"), "the old slot survived: {td}");
}

#[test]
fn rename_type_rewrites_a_wikilink_to_the_def_file() {
    let mut h = started_in_git();

    // A referrer that both claims `note` (surface 1) and links the def file by
    // its type-name (surface 2) — both must follow the rename, merged in one
    // rewrite of the file.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: refers\n---\nsee [[note]]\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let b = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(b.contains("type: memo"), "the claim was not rewritten: {b}");
    assert!(
        b.contains("[[memo]]"),
        "the wikilink to the def file was not rewritten: {b}"
    );
    assert!(!b.contains("[[note]]"), "the old wikilink survived: {b}");
}

#[test]
fn rename_type_rejects_an_existing_target_name() {
    let mut h = started_in_git();

    // `memo` already names a type-def — renaming onto it would be a duplicate.
    fs::write(h.root.join("type/memo.type.yaml"), "fields:\n  x: String\n").unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add memo"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"].as_str().unwrap().contains("already exists"),
        "expected a duplicate-name rejection, got {resp:?}"
    );
    assert!(
        h.root.join("type/note.type.yaml").exists(),
        "the type-def must be untouched on a rejected rename"
    );
}

#[test]
fn rename_type_parent_rewrites_a_leaf_claim_and_keeps_the_leaf_filename() {
    let mut h = started_in_git();

    // A sealed parent and one leaf; the leaf claims the parent by name.
    fs::write(
        h.root.join("type/decision.type.yaml"),
        "sealed:\n  - decision.decided\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/decision.decided.type.yaml"),
        "extends: decision\nfields:\n  choice: String\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add sealed family"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "decision",
            "new_name": "verdict",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The parent moved.
    assert!(h.root.join("type/verdict.type.yaml").exists());
    assert!(!h.root.join("type/decision.type.yaml").exists());
    // The leaf FILE keeps its name (a separate rename, per the plan), but its
    // `type:` claim cascaded to the new parent name — else the sealed family
    // would be left with an orphaned leaf.
    assert!(
        h.root.join("type/decision.decided.type.yaml").exists(),
        "the leaf filename must be untouched"
    );
    let leaf = fs::read_to_string(h.root.join("type/decision.decided.type.yaml")).unwrap();
    assert!(
        leaf.contains("extends: verdict"),
        "the leaf claim was not rewritten: {leaf}"
    );
    assert!(
        !leaf.contains("extends: decision"),
        "the old leaf claim survived: {leaf}"
    );
    // The leaf branch name `decision.decided` is not the parent name, so the
    // parent's `sealed:` entry is left alone (the leaf was not renamed).
    let parent = fs::read_to_string(h.root.join("type/verdict.type.yaml")).unwrap();
    assert!(
        parent.contains("decision.decided"),
        "the leaf branch name should be untouched: {parent}"
    );
}

#[test]
fn rename_type_leaf_rewrites_the_parent_sealed_branch() {
    let mut h = started_in_git();

    fs::write(
        h.root.join("type/decision.type.yaml"),
        "sealed:\n  - decision.decided\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/decision.decided.type.yaml"),
        "extends: decision\nfields:\n  choice: String\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add sealed family"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "decision.decided",
            "new_name": "decision.resolved",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The leaf moved, and the parent's `sealed:` branch name followed it.
    assert!(h.root.join("type/decision.resolved.type.yaml").exists());
    assert!(!h.root.join("type/decision.decided.type.yaml").exists());
    let parent = fs::read_to_string(h.root.join("type/decision.type.yaml")).unwrap();
    assert!(
        parent.contains("decision.resolved"),
        "the sealed branch was not rewritten: {parent}"
    );
    assert!(
        !parent.contains("decision.decided"),
        "the old sealed branch survived: {parent}"
    );
}

#[test]
fn rename_type_rewrites_a_self_reference() {
    let mut h = started_in_git();

    // A type-def whose slot references itself — the def file is its own
    // referrer, rewritten at the old path then carried by the move.
    fs::write(
        h.root.join("type/tree.type.yaml"),
        "fields:\n  children: tree*[]\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add tree"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "tree",
            "new_name": "node",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert!(h.root.join("type/node.type.yaml").exists());
    assert!(!h.root.join("type/tree.type.yaml").exists());
    let td = fs::read_to_string(h.root.join("type/node.type.yaml")).unwrap();
    assert!(
        td.contains("children: node*[]"),
        "the self-slot was not rewritten: {td}"
    );
    assert!(!td.contains("tree*"), "the old self-slot survived: {td}");
}

#[test]
fn rename_type_rewrites_a_body_use_and_a_qualified_key() {
    let mut h = started_in_git();

    // A base type with a body, a derived type splicing it via `use:`.
    fs::write(
        h.root.join("type/base.type.yaml"),
        "body:\n  - section: Intro\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/derived.type.yaml"),
        "extends: base\nbody:\n  - use: base\n",
    )
    .unwrap();
    // An instance using the qualifier form `description{note}`.
    fs::write(
        h.root.join("p.md"),
        "---\ntype: note\ndescription{note}: via qualifier\n---\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add base/derived/p"]);
    h.engine.rebuild();

    // The parent claim and the body `use:` both follow.
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "base",
            "new_name": "foundation",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let derived = fs::read_to_string(h.root.join("type/derived.type.yaml")).unwrap();
    assert!(
        derived.contains("extends: foundation"),
        "the parent claim was not rewritten: {derived}"
    );
    assert!(
        derived.contains("use: foundation"),
        "the body use: was not rewritten: {derived}"
    );
    assert!(
        !derived.contains("base"),
        "the old name survived: {derived}"
    );

    // The qualified key follows too.
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let p = fs::read_to_string(h.root.join("p.md")).unwrap();
    assert!(
        p.contains("description{memo}"),
        "the qualified key was not rewritten: {p}"
    );
    assert!(
        !p.contains("description{note}"),
        "the old qualified key survived: {p}"
    );
}

#[test]
fn rename_type_rewrites_a_nested_inline_record_claim() {
    let mut h = started_in_git();

    // A record slot, and an instance filling it with an inline record that
    // claims the slot type by name — the recursive value walk must reach it.
    fs::write(
        h.root.join("type/inner.type.yaml"),
        "fields:\n  v: String\n",
    )
    .unwrap();
    fs::write(
        h.root.join("type/outer.type.yaml"),
        "fields:\n  slot: inner\n",
    )
    .unwrap();
    fs::write(
        h.root.join("o.md"),
        "---\ntype: outer\nslot:\n  type: inner\n  v: x\n---\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add inline"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "inner",
            "new_name": "core",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let outer = fs::read_to_string(h.root.join("type/outer.type.yaml")).unwrap();
    assert!(
        outer.contains("slot: core"),
        "the slot shape was not rewritten: {outer}"
    );
    let o = fs::read_to_string(h.root.join("o.md")).unwrap();
    assert!(
        o.contains("type: core"),
        "the nested inline claim was not rewritten: {o}"
    );
    assert!(
        !o.contains("type: inner"),
        "the old nested claim survived: {o}"
    );
}

#[test]
fn rename_type_rejects_a_referrer_that_drifted_from_the_held_index() {
    let mut h = started_in_git();

    // A surface-1 referrer (claims `note`); we drift it out-of-band so its held
    // hash goes stale, the same read-before-write guard `rename` has.
    wr(
        &h.root,
        "r.md",
        "---\ntype: note\ndescription: original\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    wr(
        &h.root,
        "r.md",
        "---\ntype: note\ndescription: changed\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "out-of-band edit"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a drifted referrer should reject, got {resp:?}"
    );
    assert!(
        resp["error"].as_str().unwrap().contains("changed since"),
        "the reject should name the referrer drift, got {resp:?}"
    );
    // Nothing moved: the saga compensated the whole touched set.
    assert!(
        h.root.join("type/note.type.yaml").exists() && !h.root.join("type/memo.type.yaml").exists(),
        "the rename should not have happened"
    );
}

#[test]
fn rename_type_rejects_an_invalid_new_name() {
    let mut h = started_in_git();
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "1bad",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("not a valid type name"),
        "got {resp:?}"
    );
    assert!(h.root.join("type/note.type.yaml").exists());
}

#[test]
fn rename_type_rejects_an_absent_type() {
    let mut h = started_in_git();
    // A name no type-def owns hits the `find_owned_type_def` -> None path.
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "ghost",
            "new_name": "specter",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "got {resp:?}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("no type-def named"),
        "got {resp:?}"
    );
}

#[test]
fn rename_type_rewrites_a_cross_repo_wikilink_to_the_def_file() {
    // ra owns the type-def, rb links it by type-name across the boundary. The
    // mounted-set surface-2 rewrite must follow it, the ::ra qualifier kept.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(
        &root,
        "ra/type/note.type.yaml",
        "fields:\n  description: String\n",
    );
    wr(
        &root,
        "rb/.arsumbris/repo.yaml",
        "name: rb\ndeps:\n  - name: ra\n",
    );
    wr(&root, "rb/doc.md", "see the type [[note::ra]]\n");
    crate::seed_workspace(&root, &["ra", "rb"]);
    for r in ["ra", "rb"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert!(h.root.join("ra/type/memo.type.yaml").exists());
    assert!(!h.root.join("ra/type/note.type.yaml").exists());
    let doc = fs::read_to_string(h.root.join("rb/doc.md")).unwrap();
    assert!(
        doc.contains("[[memo::ra]]"),
        "the cross-repo wikilink was not rewritten: {doc}"
    );
    assert!(
        !doc.contains("[[note::ra]]"),
        "the old cross-repo wikilink survived: {doc}"
    );
}

#[test]
fn rename_type_rewrites_a_cross_repo_type_name_claim() {
    // ra owns the type-def, rb CLAIMS it by type-name (`type: note::ra`, a
    // surface-1 reference, not a wikilink). The repo-aware referrer scan must find
    // it across the boundary and rewrite it QUALIFIER-PRESERVING to `memo::ra`;
    // the old repo-local scan stranded it, returning success with a dangling ref
    // (finding 2.2).
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(&root, "ra/type/note.type.yaml", "fields: {}\n");
    wr(
        &root,
        "rb/.arsumbris/repo.yaml",
        "name: rb\ndeps:\n  - name: ra\n",
    );
    wr(&root, "rb/item.md", "---\ntype: note::ra\n---\n");
    crate::seed_workspace(&root, &["ra", "rb"]);
    for r in ["ra", "rb"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert!(h.root.join("ra/type/memo.type.yaml").exists());
    let item = fs::read_to_string(h.root.join("rb/item.md")).unwrap();
    assert!(
        item.contains("type: memo::ra"),
        "the cross-repo type-name claim was not rewritten: {item}"
    );
    assert!(
        !item.contains("note::ra"),
        "the old cross-repo claim survived: {item}"
    );
}

#[test]
fn rename_type_leaves_a_same_named_peer_types_references_untouched() {
    // ra and rb each OWN a same-named `widget`, and EACH has an instance
    // claiming its own widget. Renaming `widget::rb` must rewrite ONLY rb's
    // instance claim; ra's identically-named claim is a reference to ra's OWN
    // widget and must survive byte-for-byte. This guards the repo-scoping of the
    // reference REWRITE (`site_targets`), which the def-file-move test does not:
    // it carries no references, so a repo-unaware rewrite passes it. A miss here
    // corrupts a peer repo's file — the broken-write class.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(&root, "ra/type/widget.type.yaml", "fields:\n  a: String\n");
    wr(&root, "ra/card.md", "---\ntype: widget\na: hi\n---\n");
    wr(&root, "rb/.arsumbris/repo.yaml", "name: rb\n");
    wr(&root, "rb/type/widget.type.yaml", "fields:\n  b: Number\n");
    wr(&root, "rb/card.md", "---\ntype: widget\nb: 5\n---\n");
    crate::seed_workspace(&root, &["ra", "rb"]);
    for r in ["ra", "rb"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "widget::rb",
            "new_name": "gizmo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // rb's own instance claim is rewritten to the new name.
    let rb_card = fs::read_to_string(h.root.join("rb/card.md")).unwrap();
    assert!(
        rb_card.contains("type: gizmo"),
        "rb's claim rewrites to gizmo: {rb_card:?}"
    );
    // ra's same-named claim is a reference to ra's OWN widget and must NOT move.
    let ra_card = fs::read_to_string(h.root.join("ra/card.md")).unwrap();
    assert_eq!(
        ra_card, "---\ntype: widget\na: hi\n---\n",
        "ra's same-named widget claim must survive byte-for-byte: {ra_card:?}"
    );
}

#[test]
fn rename_type_qualified_old_name_selects_the_owner_identity() {
    // ra and rb each OWN a same-named `widget` (legal, name uniqueness is
    // per-repo). A bare `rename_type("widget")` would rename whichever owner
    // comes first; `widget::rb` must select rb's identity and leave ra's alone.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(&root, "ra/type/widget.type.yaml", "fields:\n  a: String\n");
    wr(&root, "rb/.arsumbris/repo.yaml", "name: rb\n");
    wr(&root, "rb/type/widget.type.yaml", "fields:\n  b: Number\n");
    crate::seed_workspace(&root, &["ra", "rb"]);
    for r in ["ra", "rb"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "widget::rb",
            "new_name": "gizmo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // rb's widget is renamed; ra's identically-named widget is untouched.
    assert!(
        h.root.join("rb/type/gizmo.type.yaml").exists(),
        "rb's widget was renamed to gizmo"
    );
    assert!(
        !h.root.join("rb/type/widget.type.yaml").exists(),
        "rb's old widget file is gone"
    );
    assert!(
        h.root.join("ra/type/widget.type.yaml").exists(),
        "ra's same-named widget must NOT be touched by the qualified rename"
    );
}

#[test]
fn rename_type_rejects_a_non_git_cross_repo_referrer() {
    // ra (git) owns the def, rb claims it `type: note::ra`. No working tree
    // covers rb: neither its own root nor the tempdir above it is a git tree, so
    // it is genuinely uncovered rather than merely lacking its own `.git`. The
    // rewrite would touch rb, which cannot be compensated on a mid-saga failure,
    // so the whole refactor rejects up front rather than half-apply. Nothing is
    // written.
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(&root, "ra/type/note.type.yaml", "fields: {}\n");
    wr(
        &root,
        "rb/.arsumbris/repo.yaml",
        "name: rb\ndeps:\n  - name: ra\n",
    );
    wr(&root, "rb/item.md", "---\ntype: note::ra\n---\n");
    // ra is git-backed; rb is deliberately NOT, so it cannot be compensated.
    let ra = root.join("ra");
    git(&ra, &["init", "-q", "-b", "main"]);
    git(&ra, &["config", "user.name", "Tester"]);
    git(&ra, &["config", "user.email", "tester@example.com"]);
    git(&ra, &["add", "."]);
    git(&ra, &["commit", "-q", "-m", "seed"]);
    crate::seed_workspace(&root, &["ra", "rb"]);
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a non-git referrer should reject, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("no git working tree covers"),
        "the reject should name the uncovered member, got {resp:?}"
    );
    // Nothing written: the def keeps its name, rb keeps its claim.
    assert!(
        h.root.join("ra/type/note.type.yaml").exists(),
        "the def was renamed despite the reject"
    );
    let item = fs::read_to_string(h.root.join("rb/item.md")).unwrap();
    assert!(
        item.contains("note::ra"),
        "rb's claim was rewritten despite the reject: {item}"
    );
}

#[test]
fn rename_rejects_a_referrer_that_drifted_from_the_held_index() {
    // A referrer committed a change the engine has not rebuilt yet: the held
    // span and hash are stale. clean-at-HEAD passes (the tree is clean), so the
    // per-referrer read-before-write hash guard is what must catch it. Without
    // it, the stale span is rewritten against drifted content — here it would
    // silently clobber a different link sitting where the original one was.
    let mut h = started_in_git();
    wr(&h.root, "r.md", "see [[a]] x\n");
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    // Drift the referrer and commit it WITHOUT a rebuild — the held index keeps
    // the old hash and the old `[[a]]` span, but disk now holds `[[c]]` there
    // (same length, same offset).
    wr(&h.root, "r.md", "see [[c]] x\n");
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "out-of-band edit to the referrer"],
    );

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "b.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a drifted referrer should reject, got {resp:?}"
    );
    assert!(
        resp["error"].as_str().unwrap().contains("changed since"),
        "the reject should name the referrer drift, got {resp:?}"
    );
    // Nothing written: the referrer keeps its content, the file was not renamed.
    assert_eq!(
        fs::read_to_string(h.root.join("r.md")).unwrap(),
        "see [[c]] x\n"
    );
    assert!(
        h.root.join("a.md").exists() && !h.root.join("b.md").exists(),
        "the rename should not have happened"
    );
}

#[test]
fn rename_rewrites_a_self_reference_in_the_moved_file() {
    let mut h = started_in_git();

    // A file that references itself in its own body.
    fs::write(
        h.root.join("s.md"),
        "---\ntype: note\ndescription: x\n---\nsee [[s]] here\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add self-ref"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "s.md",
            "to": "t.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert!(!h.root.join("s.md").exists(), "s.md still present");
    let moved = fs::read_to_string(h.root.join("t.md")).unwrap();
    assert!(
        moved.contains("[[t]]"),
        "the self-reference was not rewritten in the moved file: {moved}"
    );
    assert!(
        !moved.contains("[[s]]"),
        "the old self-reference survived: {moved}"
    );
}

#[test]
fn rename_does_not_edit_the_moved_files_bytes() {
    // THE INVARIANT: a mediated rename never writes the moved file's content, so
    // its blob oid survives the move and git reports R100. That is what lets blob
    // identity carry a rename exactly, instead of a similarity heuristic — an
    // out-of-band `git mv` becomes recoverable, and rename detection stops being
    // probabilistic. A later change that starts writing the moved file's content
    // would silently degrade all of that, so it fails here instead.
    //
    // The referrer is load-bearing: the rewrite pass runs over OTHER files, and
    // the moved file must be filtered out of it. Without a referrer the test
    // could pass while that filter was broken.
    let mut h = started_in_git();
    wr(
        &h.root,
        "moved.md",
        "---\ntype: note\ndescription: distinctive content that must survive verbatim\n---\nbody\n",
    );
    wr(
        &h.root,
        "referrer.md",
        "---\ntype: note\ndescription: refers\n---\nsee [[moved]]\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add file and referrer"]);
    h.engine.rebuild();

    let oid_before = git_out(&h.root, &["rev-parse", "HEAD:moved.md"]);

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "moved.md", "to": "renamed.md"}))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The bytes are identical, which is the invariant itself. Asserted on the oid
    // rather than on git's rename score, so no similarity threshold is involved.
    let oid_after = git_out(&h.root, &["rev-parse", "HEAD:renamed.md"]);
    assert_eq!(
        oid_before, oid_after,
        "the rename edited the moved file's bytes — blob identity no longer carries the rename"
    );

    // And git's own rendering of it, the form the trace reads: a pure R100.
    let status = git_out(&h.root, &["diff", "--name-status", "-M", "HEAD~1", "HEAD"]);
    assert!(
        status.contains("R100\tmoved.md\trenamed.md"),
        "the move is not a pure rename: {status}"
    );

    // The referrer really was rewritten in the same commit, so the filter above
    // is what kept the moved file clean, not an absence of rewriting.
    let referrer = fs::read_to_string(h.root.join("referrer.md")).unwrap();
    assert!(
        referrer.contains("[[renamed]]"),
        "the referrer was not rewritten: {referrer}"
    );
}

#[test]
fn rename_edits_the_moved_file_only_when_it_is_its_own_referrer() {
    // The one documented exception: a file containing a link to itself must have
    // that link rewritten, so the move is not R100. Asserted rather than left
    // implicit, because the two-witness model splits exactly here — blob identity
    // covers every other move, and the `Moved:` trailer covers this one.
    //
    // Splitting the move and the referrer rewrite into two commits would remove
    // the exception, and is rejected: it would break commit-per-mutation for a
    // case the trailer already covers.
    let mut h = started_in_git();
    wr(
        &h.root,
        "s.md",
        "---\ntype: note\ndescription: x\n---\nsee [[s]] here\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add self-ref"]);
    h.engine.rebuild();

    let oid_before = git_out(&h.root, &["rev-parse", "HEAD:s.md"]);

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "s.md", "to": "t.md"}))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let oid_after = git_out(&h.root, &["rev-parse", "HEAD:t.md"]);
    assert_ne!(
        oid_before, oid_after,
        "the self-reference was not rewritten, so the exception no longer exists — \
         if that is intended, the two-witness model should be revisited, not this test"
    );

    // The trailer is what covers the case blob identity cannot see.
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert!(
        body.contains("Moved: s.md -> t.md"),
        "the exception is not carried by a Moved: trailer: {body}"
    );
}

#[test]
fn promote_extracts_a_referrerless_record_into_its_own_file() {
    let mut h = started_in_git();

    // A type with a record slot, and a host holding one inline record with no
    // `^:` id — so it can carry no referrers, the referrerless case.
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    let host = "---\ntype: canvas\nnodes:\n  - type: node\n    content: x\n---\nbody\n";
    wr(&h.root, "canvas.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas"]);
    h.engine.rebuild();

    // An offset inside the record.
    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "at": at,
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The new file holds the record as a standalone instance, fenced.
    let promoted = fs::read_to_string(h.root.join("promoted.md")).unwrap();
    assert_eq!(
        promoted, "---\ntype: node\ncontent: x\n---\n",
        "unexpected promoted-file content: {promoted:?}"
    );

    // The host keeps a quoted reference where the record sat, and the record
    // content is gone from it.
    let after = fs::read_to_string(h.root.join("canvas.md")).unwrap();
    assert!(
        after.contains("\"[[promoted]]\""),
        "host lacks the new reference: {after:?}"
    );
    assert!(
        !after.contains("content: x"),
        "the record content stayed in the host: {after:?}"
    );

    // The extraction and the new file are one commit, and the tree is clean.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after promote"
    );
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        files.contains("promoted.md"),
        "the new file is not in the commit: {files}"
    );
    assert!(
        files.contains("canvas.md"),
        "the edited host is not in the commit: {files}"
    );
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the host edit and new file should share one Mutation-Id: {body}"
    );
}

#[test]
fn promote_stamp_records_on_the_new_file() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    let host = "---\ntype: canvas\nnodes:\n  - type: node\n    content: x\n---\nbody\n";
    wr(&h.root, "canvas.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas"]);
    h.engine.rebuild();

    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md", "at": at, "to": "promoted.md",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.create", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The newly-extracted file carries the stamp, in the promote's own commit.
    let committed = git_out(&h.root, &["show", "HEAD:promoted.md"]);
    assert!(
        committed.contains("provenance:") && committed.contains("file-change.create"),
        "the new file records the stamp in the promote commit: {committed}"
    );
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the promote and stamp share one commit: {body}"
    );
}

#[test]
fn append_record_with_a_stamp_lands_both_in_one_commit() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/log.type.yaml",
        "fields:\n  entries?: String[]\n",
    );
    wr(
        &h.root,
        "l.md",
        "---\ntype: log\nentries:\n  - first\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add log"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "append_record",
            "path": "l.md", "field_path": ["entries"], "value": "second",
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let committed = git_out(&h.root, &["show", "HEAD:l.md"]);
    assert!(
        committed.contains("- second") && committed.contains("file-change.edit"),
        "the append and the stamp both land in one commit: {committed}"
    );
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "one commit: {body}"
    );
}

#[test]
fn assign_block_id_folds_a_stamp_including_the_idempotent_case() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/session-log.type.yaml",
        "fields:\n  events?: sessionEvent[]\n",
    );
    wr(
        &h.root,
        "type/sessionEvent.type.yaml",
        "fields:\n  at: String\n",
    );
    wr(
        &h.root,
        "s.md",
        "---\ntype: session-log\nevents:\n  - type: sessionEvent\n    at: t1\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add"]);
    h.engine.rebuild();

    // First assign: writes the block-id AND folds the stamp (session s-1).
    let content = fs::read_to_string(h.root.join("s.md")).unwrap();
    let at = content.find("at: t1").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "assign_block_id", "path": "s.md", "at": at,
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-1.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-1.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let after = fs::read_to_string(h.root.join("s.md")).unwrap();
    assert!(after.contains("^: b-"), "block-id assigned: {after}");
    assert_eq!(
        after.matches("file-change.edit").count(),
        1,
        "the stamp folded once: {after}"
    );

    // Second assign at the SAME record (id now exists → idempotent, no block-id
    // write) with a NEW session: the stamp still lands its own entry.
    let content2 = fs::read_to_string(h.root.join("s.md")).unwrap();
    let at2 = content2.find("at: t1").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "assign_block_id", "path": "s.md", "at": at2,
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-2.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-2.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "idempotent-with-stamp: {resp:?}");
    let after2 = fs::read_to_string(h.root.join("s.md")).unwrap();
    assert_eq!(
        after2.matches("file-change.edit").count(),
        2,
        "the idempotent block-id case still stamped a fresh entry: {after2}"
    );

    // Third assign at the SAME record, SAME session: the id exists (no block-id
    // write) AND the stamp dedups (no-op). Nothing is written, so no commit lands
    // and the file stays byte-identical.
    let head_before = git_out(&h.root, &["rev-parse", "HEAD"]);
    let at3 = after2.find("at: t1").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "assign_block_id", "path": "s.md", "at": at3,
            "stamps": [{
                "field": "provenance",
                "record": { "type": "file-change.edit", "session": "[[s-2.spans]]" },
                "match_on": { "type": "file-change.edit", "session": "[[s-2.spans]]" }
            }]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "double no-op: {resp:?}");
    assert_eq!(
        git_out(&h.root, &["rev-parse", "HEAD"]),
        head_before,
        "a double no-op (id exists + stamp dedups) makes no commit"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("s.md")).unwrap(),
        after2,
        "a double no-op leaves the file byte-identical"
    );
}

#[test]
fn rename_type_rejects_a_stamp() {
    let mut h = started_in_git();
    wr(&h.root, "type/old.type.yaml", "fields:\n  x: String\n");
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add"]);
    h.engine.rebuild();

    // rename_type has no single target file, so a stamp rejects in v1.
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "old", "new_name": "new",
            "stamps": [{ "field": "provenance", "record": { "type": "file-change.edit" } }]
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "rename_type rejects a stamp (no single target): {resp:?}"
    );
}

// A record that is a DIRECT field value (not a sequence element) carries the
// field's terminating newline inside its span. The reference replacement must
// not consume that newline, or the following line (the closing `---` fence, or
// the next key) glues onto the reference and the host stops parsing. Regression
// for the promote-corruption report. The check is that the host
// RE-PARSES as its instance, not mere substring containment — the gap the older
// sequence-element tests left open.
#[test]
fn promote_a_direct_field_value_record_keeps_the_host_parseable_last_key() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/thing.type.yaml",
        "fields:\n  label: String\n",
    );
    wr(
        &h.root,
        "type/box.type.yaml",
        "fields:\n  contents: thing&\n",
    );
    // The record is the LAST frontmatter key (repro A).
    let host = "---\ntype: box\ncontents:\n  ^: r1\n  type: thing\n  label: hello\n---\nbody\n";
    wr(&h.root, "box.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add box"]);
    h.engine.rebuild();

    let at = host.find("label: hello").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "box.md",
            "at": at,
            "to": "extracted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let host_after = fs::read_to_string(h.root.join("box.md")).unwrap();
    let resolved = h
        .client
        .query(&json!({ "read": "instance", "path": "box.md" }))
        .unwrap();
    assert_eq!(
        resolved["result"]["instance"]["claim"],
        json!(["box"]),
        "host no longer parses as a box instance after promote: {host_after:?}"
    );
}

#[test]
fn promote_a_direct_field_value_record_keeps_the_host_parseable_non_last_key() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/thing.type.yaml",
        "fields:\n  label: String\n",
    );
    wr(
        &h.root,
        "type/box.type.yaml",
        "fields:\n  contents: thing&\n",
    );
    // The record is FOLLOWED by another key (repro B) — rules out "last key" as
    // the cause; a non-last record glues the next key onto the reference.
    let host =
        "---\ntype: box\ncontents:\n  ^: r1\n  type: thing\n  label: hello\nnote: tail\n---\nbody\n";
    wr(&h.root, "box.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add box"]);
    h.engine.rebuild();

    let at = host.find("label: hello").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "box.md",
            "at": at,
            "to": "extracted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let host_after = fs::read_to_string(h.root.join("box.md")).unwrap();
    let resolved = h
        .client
        .query(&json!({ "read": "instance", "path": "box.md" }))
        .unwrap();
    assert_eq!(
        resolved["result"]["instance"]["claim"],
        json!(["box"]),
        "host no longer parses as a box instance after promote: {host_after:?}"
    );
}

#[test]
fn promote_extracts_a_referenced_record_and_rewrites_referrers() {
    let mut h = started_in_git();

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    // The host holds an addressable record, and another file references it.
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\nbody\n",
    );
    wr(
        &h.root,
        "other.md",
        "---\ntype: note\ndescription: refers\n---\nsee [[canvas^rec]] here\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas and referrer"]);
    h.engine.rebuild();

    // Located by its block-id, not an offset.
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The new file holds the record.
    let promoted = fs::read_to_string(h.root.join("promoted.md")).unwrap();
    assert_eq!(
        promoted, "---\ntype: node\ncontent: x\n---\n",
        "unexpected promoted-file content: {promoted:?}"
    );

    // The host keeps a reference; the record content is gone.
    let host = fs::read_to_string(h.root.join("canvas.md")).unwrap();
    assert!(
        host.contains("\"[[promoted]]\"") && !host.contains("content: x"),
        "host not rewritten: {host:?}"
    );

    // The referrer's block-ref became a file-ref to the new file.
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[promoted]]") && !other.contains("[[canvas^rec]]"),
        "referrer not rewritten: {other:?}"
    );

    // The host edit, the referrer rewrite, and the new file are one commit.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after promote"
    );
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    for f in ["promoted.md", "canvas.md", "other.md"] {
        assert!(files.contains(f), "{f} missing from the commit: {files}");
    }
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the whole refactor should share one Mutation-Id: {body}"
    );
}

#[test]
fn promote_redirects_a_records_self_reference_to_the_new_file() {
    let mut h = started_in_git();

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    // The record references its own block-id from inside itself.
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n    related: \"[[canvas^rec]]\"\n---\nbody\n",
    );
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add self-referential record"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The self-reference now points at the new file — the record IS that file —
    // not at a block-id that no longer exists. No dangling reference is left by
    // our own mutation.
    let promoted = fs::read_to_string(h.root.join("promoted.md")).unwrap();
    assert_eq!(
        promoted, "---\ntype: node\ncontent: x\nrelated: \"[[promoted]]\"\n---\n",
        "the self-reference was not redirected to the new file: {promoted:?}"
    );
    assert!(
        !promoted.contains("canvas^rec"),
        "a dangling self-reference survived in the new file: {promoted:?}"
    );

    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after promote"
    );
}

#[test]
fn promote_redirects_a_local_form_self_reference() {
    // The local form `[[^rec]]` (no file name) must be handled like the explicit
    // `[[canvas^rec]]`. It only works because local-form references now enter the
    // backlink index, resolving to their own file.
    let mut h = started_in_git();

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    related: \"[[^rec]]\"\n---\nbody\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add local self-ref"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let promoted = fs::read_to_string(h.root.join("promoted.md")).unwrap();
    assert_eq!(
        promoted, "---\ntype: node\nrelated: \"[[promoted]]\"\n---\n",
        "the local-form self-reference was not redirected: {promoted:?}"
    );
}

#[test]
fn promote_requires_exactly_one_locator() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas"]);
    h.engine.rebuild();

    // Neither locator rejects.
    let neither = h
        .client
        .query(&json!({ "mutate": "promote", "path": "canvas.md", "to": "p.md" }))
        .unwrap();
    assert_eq!(neither["type"], "error", "got {neither:?}");

    // Both locators rejects.
    let both = h
        .client
        .query(&json!({
            "mutate": "promote", "path": "canvas.md", "at": 30, "block_id": "rec", "to": "p.md",
        }))
        .unwrap();
    assert_eq!(both["type"], "error", "got {both:?}");

    assert!(!h.root.join("p.md").exists(), "nothing should be written");
}

#[test]
fn promote_rejects_an_unsupported_target_extension() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    let host = "---\ntype: canvas\nnodes:\n  - type: node\n    content: x\n---\n";
    wr(&h.root, "canvas.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas"]);
    h.engine.rebuild();

    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "at": at,
            "to": "promoted.txt",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an unsupported extension rejects, got {resp:?}"
    );
    assert!(
        !h.root.join("promoted.txt").exists(),
        "nothing should be written on a rejected promote"
    );

    // The host is untouched.
    assert_eq!(fs::read_to_string(h.root.join("canvas.md")).unwrap(), host);
}

#[test]
fn promote_rejects_a_bare_inline_only_host_slot() {
    // A record in a bare `node` slot (inline-only) cannot be promoted: the slot
    // would be left holding a `[[newFile]]` reference it cannot accept.
    let mut h = started_in_git();
    wr(&h.root, "type/holder.type.yaml", "fields:\n  root: node\n");
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    let host = "---\ntype: holder\nroot:\n  type: node\n  content: x\n---\n";
    wr(&h.root, "holder.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add holder"]);
    h.engine.rebuild();

    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "holder.md",
            "at": at,
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a bare inline-only slot rejects, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("inline-or-reference"),
        "the reject names the `&` requirement, got {resp:?}"
    );
    assert!(
        !h.root.join("promoted.md").exists(),
        "nothing should be written"
    );
    assert_eq!(fs::read_to_string(h.root.join("holder.md")).unwrap(), host);
}

#[test]
fn promote_rejects_a_bare_caret_referrer_in_a_reference_slot() {
    // `other.md` cites the canvas via a bare `^rec` in a reference slot
    // (`about: canvas*`): the value is the canvas FILE, `^rec` a jump anchor.
    // Rewriting it to follow the promoted record would silently change the value
    // from the canvas to the node — promote rejects and asks for `^^rec` first.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(
        &h.root,
        "type/note.type.yaml",
        "fields:\n  about: canvas*\n",
    );
    let canvas = "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\n";
    wr(&h.root, "canvas.md", canvas);
    let other = "---\ntype: note\nabout: \"[[canvas^rec]]\"\n---\n";
    wr(&h.root, "other.md", other);
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add canvas and a bare-^ referrer"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a bare-^ reference-slot referrer rejects, got {resp:?}"
    );
    let msg = resp["error"].as_str().unwrap();
    assert!(
        msg.contains("^^rec") && msg.contains("about"),
        "the reject names the fix and the slot, got {msg:?}"
    );
    // No partial write: both files and the destination are untouched.
    assert!(
        !h.root.join("promoted.md").exists(),
        "nothing should be written"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("canvas.md")).unwrap(),
        canvas
    );
    assert_eq!(fs::read_to_string(h.root.join("other.md")).unwrap(), other);
}

#[test]
fn promote_follows_a_block_referent_referrer() {
    // The `^^` contrast: `about: node*` cites the record's VALUE via `[[canvas^^rec]]`.
    // The value legitimately follows the record to its new file, so promote succeeds.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(&h.root, "type/note.type.yaml", "fields:\n  about: node*\n");
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\n",
    );
    wr(
        &h.root,
        "other.md",
        "---\ntype: note\nabout: \"[[canvas^^rec]]\"\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add canvas and a ^^ referrer"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(
        resp["ready"], true,
        "a ^^ referrer promotes cleanly, got {resp:?}"
    );
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[promoted]]") && !other.contains("[[canvas^^rec]]"),
        "the ^^ referrer should follow the record to the new file: {other:?}"
    );
}

#[test]
fn inline_folds_a_file_into_a_host_and_repoints_the_others() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    // A standalone node file, referenced by three canvases.
    wr(&h.root, "child.md", "---\ntype: node\ncontent: x\n---\n");
    for f in ["a.md", "b.md", "c.md"] {
        wr(
            &h.root,
            f,
            "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n",
        );
    }
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvases and child"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The file is gone.
    assert!(
        !h.root.join("child.md").exists(),
        "child.md should be deleted"
    );

    // The host holds the record inline, correctly indented, the quoted
    // reference fully replaced (no leftover quotes, no dangling `[[child]]`).
    let a = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert!(
        a.contains("\n  - ^: "),
        "record's id not inline after the dash: {a:?}"
    );
    assert!(
        a.contains("\n    type: node\n"),
        "record body not indented to 4: {a:?}"
    );
    assert!(
        a.contains("\n    content: x\n"),
        "record field not indented to 4: {a:?}"
    );
    assert!(
        !a.contains('"'),
        "a leftover quote survived the fold: {a:?}"
    );
    assert!(
        !a.contains("[[child]]"),
        "host still references child: {a:?}"
    );

    // The other referrers point at the host's record as a BLOCK-REFERENT
    // (`^^id`, doubled caret), not a bare `^id`. The `nodes: node&[]` slot
    // demanded the node's value; a navigational `^id` would resolve to the
    // host file `a` and lose it. This guards the whole-file-to-record repoint.
    for f in ["b.md", "c.md"] {
        let body = fs::read_to_string(h.root.join(f)).unwrap();
        assert!(
            body.contains("[[a^^") && !body.contains("[[child]]"),
            "{f} not repointed to the host record as a block-referent: {body:?}"
        );
    }

    // One atomic commit across every touched file, clean tree.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after inline"
    );
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    for f in ["a.md", "b.md", "c.md", "child.md"] {
        assert!(files.contains(f), "{f} missing from the commit: {files}");
    }
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the whole inline should share one Mutation-Id: {body}"
    );
}

#[test]
fn inline_rejects_a_reference_only_host_slot() {
    // The chosen host slot is `node*` (reference-only): it holds the `[[child]]`
    // reference but cannot hold the inlined record. Only an `&` slot can. Reject,
    // write nothing.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node*[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(&h.root, "child.md", "---\ntype: node\ncontent: x\n---\n");
    let host = "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n";
    wr(&h.root, "a.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "seed"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a reference-only host slot rejects, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("inline-or-reference"),
        "the reject names the `&` requirement, got {resp:?}"
    );
    assert!(
        h.root.join("child.md").exists(),
        "nothing should be deleted"
    );
    assert_eq!(fs::read_to_string(h.root.join("a.md")).unwrap(), host);
}

#[test]
fn inline_rejects_referrers_that_cannot_hold_a_block_id() {
    // The chosen host (`a.md`, an `&` slot) is fine, but `child` has two referrers
    // that cannot be re-pointed to a block-id: a `file*` whole-file slot, and a
    // `[[child#head]]` anchor. Both are named in the `detail`, nothing is written.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(
        &h.root,
        "type/gallery.type.yaml",
        "fields:\n  asset: file*\n",
    );
    wr(&h.root, "child.md", "---\ntype: node\ncontent: x\n---\n");
    wr(
        &h.root,
        "a.md",
        "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n",
    );
    wr(
        &h.root,
        "g.md",
        "---\ntype: gallery\nasset: \"[[child]]\"\n---\n",
    );
    wr(
        &h.root,
        "n.md",
        "a note linking [[child#head]] for context\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "seed"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "non-block-addressable referrers reject, got {resp:?}"
    );
    let offenders = resp["detail"]["offenders"]
        .as_array()
        .expect("detail names the offenders");
    let joined = offenders
        .iter()
        .filter_map(|o| o.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("g.md") && joined.contains("file*"),
        "the file* offender is named: {joined}"
    );
    assert!(
        joined.contains("n.md") && joined.contains("#head"),
        "the #head offender is named: {joined}"
    );

    // Nothing written: the file survives and no commit landed.
    assert!(
        h.root.join("child.md").exists(),
        "the rejected inline deleted nothing"
    );
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree stays clean on a rejected inline"
    );
}

#[test]
fn rename_block_id_rewrites_the_declaration_and_referrers() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    // The host declares the record and references it locally; another file
    // references it cross-file.
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\nalso see [[^rec]] here\n",
    );
    wr(&h.root, "other.md", "see [[canvas^rec]] for context\n");
    // A second cross-file referrer pulls the record as a VALUE (`^^`, a typed
    // slot), so the rename must preserve its block-referent mode.
    wr(
        &h.root,
        "valref.md",
        "---\ntype: canvas\nnodes:\n  - \"[[canvas^^rec]]\"\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas and referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_block_id",
            "path": "canvas.md",
            "block_id": "rec",
            "to_block_id": "ref2",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The declaration and the host-local self-reference both follow the new id.
    let canvas = fs::read_to_string(h.root.join("canvas.md")).unwrap();
    assert!(
        canvas.contains("^: ref2"),
        "declaration not renamed: {canvas:?}"
    );
    assert!(
        canvas.contains("[[^ref2]]"),
        "local self-ref not renamed: {canvas:?}"
    );
    assert!(
        !canvas.contains("rec"),
        "a stale `rec` survived: {canvas:?}"
    );

    // The navigational cross-file referrer follows too, staying bare `^`.
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[canvas^ref2]]") && !other.contains("[[canvas^rec]]"),
        "cross-file referrer not renamed: {other:?}"
    );

    // The value-referrer follows AND keeps its `^^` block-referent mode.
    let valref = fs::read_to_string(h.root.join("valref.md")).unwrap();
    assert!(
        valref.contains("[[canvas^^ref2]]") && !valref.contains("rec"),
        "value-referrer lost its ^^ mode or a stale id survived: {valref:?}"
    );

    // One atomic commit across both touched files, clean tree.
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after the block-id rename"
    );
    let files = git_out(&h.root, &["show", "--name-only", "--format=", "HEAD"]);
    for f in ["canvas.md", "other.md", "valref.md"] {
        assert!(files.contains(f), "{f} missing from the commit: {files}");
    }
    let body = git_out(&h.root, &["log", "-1", "--format=%B"]);
    assert_eq!(
        body.matches("Mutation-Id:").count(),
        1,
        "the whole rename should share one Mutation-Id: {body}"
    );
}

#[test]
fn rename_block_id_rejects_a_taken_id_and_a_missing_record() {
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    let host =
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    content: x\n  - ^: keep\n    content: y\n---\n";
    wr(&h.root, "canvas.md", host);
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas"]);
    h.engine.rebuild();

    // Renaming onto an id already present in the file rejects.
    let taken = h
        .client
        .query(&json!({
            "mutate": "rename_block_id",
            "path": "canvas.md",
            "block_id": "rec",
            "to_block_id": "keep",
        }))
        .unwrap();
    assert_eq!(
        taken["type"], "error",
        "a taken target id rejects, got {taken:?}"
    );
    assert!(
        taken["error"].as_str().unwrap().contains("already exists"),
        "the reject names the collision, got {taken:?}"
    );

    // A block-id no inline record carries rejects.
    let missing = h
        .client
        .query(&json!({
            "mutate": "rename_block_id",
            "path": "canvas.md",
            "block_id": "nope",
            "to_block_id": "fresh",
        }))
        .unwrap();
    assert_eq!(
        missing["type"], "error",
        "a missing record rejects, got {missing:?}"
    );
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("no inline record"),
        "the reject explains the miss, got {missing:?}"
    );

    // Nothing was written on either reject.
    assert_eq!(fs::read_to_string(h.root.join("canvas.md")).unwrap(), host);
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "rejected renames write nothing"
    );
}

#[test]
fn promote_redirects_a_cross_file_referrer_to_a_nested_block() {
    // The promoted record carries a nested `^:kid` record; another file
    // references that nested block. After promote the nested block lives in the
    // new file, so the referrer must follow to `[[promoted^kid]]`, not dangle at
    // `canvas`.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n    related:\n      ^: kid\n      type: node\n      content: y\n---\nbody\n",
    );
    wr(&h.root, "other.md", "see [[canvas^kid]] here\n");
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add nested record and referrer"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The nested block travelled to the new file; the referrer must follow.
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[promoted^kid]]") && !other.contains("[[canvas^kid]]"),
        "the referrer to the nested block was not redirected to the new file: {other:?}"
    );
    assert!(
        git_out(&h.root, &["status", "--porcelain"]).is_empty(),
        "the working tree should be clean after promote"
    );
}

#[test]
fn promote_redirects_an_intra_record_reference_to_a_nested_sibling() {
    // The promoted record references its own nested `^:kid` from inside itself.
    // Both travel to the new file, so the reference must resolve there, not still
    // read `[[canvas^kid]]` (which would dangle after the move).
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: \"see [[canvas^kid]]\"\n    related:\n      ^: kid\n      type: node\n      content: y\n---\nbody\n",
    );
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &[
            "commit",
            "-q",
            "-m",
            "add record with intra-record nested ref",
        ],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The intra-record reference to the nested sibling must resolve in the new
    // file, not dangle at the host it left.
    let promoted = fs::read_to_string(h.root.join("promoted.md")).unwrap();
    assert!(
        !promoted.contains("canvas^kid"),
        "a dangling reference to the nested sibling survived: {promoted:?}"
    );
    assert!(
        promoted.contains("[[promoted^kid]]") || promoted.contains("[[^kid]]"),
        "the intra-record reference was not rebased onto the new file: {promoted:?}"
    );
}

#[test]
fn inline_preserves_a_referrer_to_a_nested_block() {
    // The inlined file carries a nested `^:kid`; another file references that
    // nested block via `[[child^kid]]`. After inline the nested block lives under
    // the new record in the host, so the referrer must become `[[a^kid]]`, not be
    // clobbered to the outer record's id.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "child.md",
        "---\ntype: node\ncontent: x\nrelated:\n  ^: kid\n  type: node\n  content: y\n---\n",
    );
    wr(
        &h.root,
        "a.md",
        "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n",
    );
    wr(&h.root, "other.md", "see [[child^kid]] here\n");
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add nested file and referrer"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The nested block folded under the host's new record keeps its id, so the
    // referrer points at it specifically, not at the outer record.
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[a^kid]]"),
        "the referrer to the nested block was clobbered to the outer record id: {other:?}"
    );
}

#[test]
fn inline_resolves_a_nested_block_id_colliding_with_the_host() {
    // The inlined file's subtree carries `^:dup`, and the host already declares a
    // `^:dup`. Block-ids are file-local-unique, so the travelling `dup` is renamed
    // to a fresh id during the fold, and every referrer to it follows. The host's
    // own `dup` is untouched, and no duplicate is created.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "child.md",
        "---\ntype: node\ncontent: x\nrelated:\n  ^: dup\n  type: node\n  content: y\n---\n",
    );
    wr(
        &h.root,
        "a.md",
        "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n  - ^: dup\n    type: node\n    content: existing\n---\n",
    );
    wr(&h.root, "other.md", "see [[child^dup]] here\n");
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add colliding block-id"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // No duplicate block-id was created by the fold.
    let diags = resp["result"]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !diags.iter().any(|d| d["code"] == "block-id-duplicate"),
        "the fold created a duplicate block-id: {resp:?}"
    );
    assert!(
        !h.root.join("child.md").exists(),
        "child.md should be deleted"
    );

    // The host's own `dup` survives exactly once; the travelling one was renamed.
    let a = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert_eq!(
        a.matches("^: dup").count(),
        1,
        "the host's dup was duplicated or lost: {a:?}"
    );
    assert!(
        a.contains("existing") && a.contains("content: y"),
        "both records present: {a:?}"
    );

    // The referrer follows to the renamed id, which exists in the host.
    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        !other.contains("child^dup") && other.contains("[[a^"),
        "the referrer was not repointed: {other:?}"
    );
    let start = other.find("[[a^").unwrap() + 4;
    let id = &other[start..start + other[start..].find("]]").unwrap()];
    assert_ne!(id, "dup", "the colliding id was not renamed: {other:?}");
    assert!(
        a.contains(&format!("^: {id}")),
        "the referrer points at a renamed id present in the host: id={id} a={a:?}"
    );
}

#[test]
fn inline_rebases_a_file_internal_reference_to_its_nested_block() {
    // The inlined file references its own nested block by explicit name
    // `[[child^kid]]`. After the fold the block lives in the host, so the
    // reference must resolve there, not dangle at the deleted file.
    let mut h = started_in_git();
    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    wr(
        &h.root,
        "child.md",
        "---\ntype: node\ncontent: \"see [[child^kid]]\"\nrelated:\n  ^: kid\n  type: node\n  content: y\n---\n",
    );
    wr(
        &h.root,
        "a.md",
        "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n",
    );
    git(&h.root, &["add", "."]);
    git(
        &h.root,
        &["commit", "-q", "-m", "add file-internal nested ref"],
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "child.md",
            "into": "a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let a = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert!(
        !a.contains("child^kid"),
        "a dangling file-internal reference survived the fold: {a:?}"
    );
    assert!(
        a.contains("[[^kid]]") || a.contains("[[a^kid]]"),
        "the file-internal reference was not rebased onto the host: {a:?}"
    );
}

// ----- the pinned-reference freeze, one test per rewriting verb -----
//
// A `[[target::@commit]]` pin asserts what the target was called at that commit,
// so no refactor may re-point it: the guard lives in `rename::ref_edits`, and
// each verb below proves its own path reaches it. Every test pairs a pinned
// referrer with an unpinned one in the same mutation, so a verb that froze
// everything (or nothing) fails just as loudly as one that rewrote the pin.
//
// The consequence is deliberate and disclosed: a commit-pinned edge forms no
// backlink at all, it is an inert snapshot that never enters the index, so a
// refactor has nothing to re-point and an inbound query never sees it. See
// [[spec - pinned references - a recorded resolved edge with an immutable past
// and an on-demand forward trace]].

/// The seed commit's sha, the commit a fixture's pins name.
fn head_sha(repo: &std::path::Path) -> String {
    git_out(repo, &["rev-parse", "HEAD"])
}

#[test]
fn rename_freezes_a_pinned_referrer_and_rewrites_the_unpinned_one() {
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    // One referrer carrying both forms: an ordinary link and a pin at the seed
    // commit, where `a.md` really was called `a`.
    wr(
        &h.root,
        "b.md",
        &format!("---\ntype: note\ndescription: refers\n---\nsee [[a]] and [[a::@{sha}]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "a.md", "to": "c.md"}))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let b = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(b.contains("[[c]]"), "the live link was not rewritten: {b}");
    assert!(
        b.contains(&format!("[[a::@{sha}]]")),
        "the pin was rewritten — a historical record was edited: {b}"
    );
}

#[test]
fn an_all_frozen_referrer_is_not_written_at_all() {
    // The freeze makes an all-pinned referrer yield NO edits, so its rewrite is
    // byte-identical. Writing it anyway is a pointless disk write, and worse, it
    // wakes the watcher for a file that did not change.
    //
    // mtime is the assertion because content equality cannot tell "not written"
    // from "written with the same bytes", and it is the mtime the watcher reacts
    // to.
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    // EVERY reference pinned, so the rename has nothing to rewrite here.
    wr(
        &h.root,
        "b.md",
        &format!(
            "---\ntype: note\ndescription: refers\n---\nsee [[a::@{sha}]] and [[a::@{sha}]]\n"
        ),
    );
    // An ordinary referrer beside it, so the test cannot pass by the rename
    // failing to touch anything at all.
    wr(
        &h.root,
        "d.md",
        "---\ntype: note\ndescription: refers\n---\nsee [[a]]\n",
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrers"]);
    h.engine.rebuild();

    let frozen = h.root.join("b.md");
    let before = fs::metadata(&frozen).unwrap().modified().unwrap();

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "a.md", "to": "c.md"}))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert_eq!(
        fs::metadata(&frozen).unwrap().modified().unwrap(),
        before,
        "the all-frozen referrer was rewritten with identical bytes, waking the watcher"
    );
    // The control: the unpinned referrer WAS rewritten, so the rename really ran.
    let d = fs::read_to_string(h.root.join("d.md")).unwrap();
    assert!(d.contains("[[c]]"), "the rename did not run at all: {d}");
}

#[test]
fn a_dirty_all_frozen_referrer_does_not_block_the_refactor() {
    // clean-at-HEAD exists because the engine is the writer: it must never
    // clobber uncommitted work. A referrer whose every reference is frozen is
    // NOT written, so it cannot be clobbered, and gating on it let a file the
    // refactor would not touch reject the whole refactor.
    //
    // The shape a provenance log has: an append-only file of pins, edited while
    // a rename runs elsewhere.
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    wr(
        &h.root,
        "log.md",
        &format!("---\ntype: note\ndescription: refers\n---\nsee [[a::@{sha}]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add pinned log"]);
    h.engine.rebuild();

    // Dirty it, WITHOUT committing: exactly what the guard reacts to.
    wr(
        &h.root,
        "log.md",
        &format!("---\ntype: note\ndescription: refers\n---\nsee [[a::@{sha}]]\n\nediting\n"),
    );
    // The daemon watches, so it settles on the edit before any mutation arrives.
    // Without this the CONTENT-HASH guard rejects first ("wait for the rebuild
    // and retry"), which is a race rather than the standing block under test.
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "a.md", "to": "c.md"}))
        .unwrap();
    assert_eq!(
        resp["ready"], true,
        "a file the rename does not write blocked it: {resp:?}"
    );

    // The human's uncommitted edit is still there, untouched.
    let log = fs::read_to_string(h.root.join("log.md")).unwrap();
    assert!(
        log.contains("editing"),
        "the mutation clobbered the uncommitted edit: {log}"
    );
    assert!(
        log.contains(&format!("[[a::@{sha}]]")),
        "the pin was rewritten: {log}"
    );
}

#[test]
fn a_dirty_referrer_the_refactor_does_write_still_blocks_it() {
    // The other half, and the one that must not regress: the exemption is for
    // files the refactor leaves alone, never a general relaxation. A dirty
    // referrer carrying an UNPINNED reference is still written, so the guard
    // must still reject.
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    // One frozen reference and one live one, so the file IS rewritten.
    wr(
        &h.root,
        "log.md",
        &format!("---\ntype: note\ndescription: refers\n---\nsee [[a::@{sha}]] and [[a]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add mixed log"]);
    h.engine.rebuild();

    wr(
        &h.root,
        "log.md",
        &format!(
            "---\ntype: note\ndescription: refers\n---\nsee [[a::@{sha}]] and [[a]]\n\nediting\n"
        ),
    );
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({"mutate": "rename", "path": "a.md", "to": "c.md"}))
        .unwrap();
    assert_ne!(
        resp["ready"], true,
        "a dirty referrer the rename rewrites was not gated: {resp:?}"
    );
}

#[test]
fn promote_freezes_a_pinned_referrer() {
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\nbody\n",
    );
    // Promote changes the reference FORM (block-ref to file-ref), so this also
    // proves the freeze is not merely a target-name guard.
    wr(
        &h.root,
        "other.md",
        &format!(
            "---\ntype: note\ndescription: refers\n---\nsee [[canvas^rec]] and [[canvas::@{sha}^rec]]\n"
        ),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas and referrers"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "canvas.md",
            "block_id": "rec",
            "to": "promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[promoted]]"),
        "the live referrer was not repointed: {other:?}"
    );
    assert!(
        other.contains(&format!("[[canvas::@{sha}^rec]]")),
        "the pin was rewritten — the record really was in canvas at that commit: {other:?}"
    );
}

#[test]
fn inline_freezes_a_pinned_referrer() {
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(&h.root, "child.md", "---\ntype: node\ncontent: x\n---\n");
    wr(
        &h.root,
        "host.md",
        "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n",
    );
    // A slot reference that inline repoints, plus a pinned prose link that it
    // must not. `child.md` is DELETED by the fold, so the frozen pin is left
    // dangling live — the disclosed trade, a true record over a live edge.
    wr(
        &h.root,
        "other.md",
        &format!("---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\nsee [[child::@{sha}]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add host and referrers"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({"mutate": "inline", "path": "child.md", "into": "host.md"}))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[host^^"),
        "the slot reference was not repointed at the host record: {other:?}"
    );
    assert!(
        other.contains(&format!("[[child::@{sha}]]")),
        "the pin was rewritten — child.md existed under that name at that commit: {other:?}"
    );
}

#[test]
fn rename_block_id_freezes_a_pinned_referrer() {
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    wr(
        &h.root,
        "type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &h.root,
        "type/node.type.yaml",
        "fields:\n  content?: String\n",
    );
    wr(
        &h.root,
        "canvas.md",
        "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\n",
    );
    // The freeze covers the whole reference, fragments included: the block
    // carried the id `rec` at that commit, so the pin keeps it.
    wr(
        &h.root,
        "other.md",
        &format!("see [[canvas^rec]] and [[canvas::@{sha}^rec]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add canvas and referrers"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_block_id",
            "path": "canvas.md",
            "block_id": "rec",
            "to_block_id": "ref2",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let other = fs::read_to_string(h.root.join("other.md")).unwrap();
    assert!(
        other.contains("[[canvas^ref2]]"),
        "the live referrer did not follow the new id: {other:?}"
    );
    assert!(
        other.contains(&format!("[[canvas::@{sha}^rec]]")),
        "the pin's block-id was rewritten — it names the id as of that commit: {other:?}"
    );
}

#[test]
fn rename_type_freezes_a_pinned_wikilink_but_still_rewrites_the_claim() {
    let mut h = started_in_git();
    let sha = head_sha(&h.root);

    // Three surfaces in one file: the type CLAIM, a live wikilink to the def
    // file, and a pinned one. The claim goes through `type_ref_edits`, which
    // rewrites type names rather than wikilinks and so carries no pin — it must
    // keep cascading. Only the wikilink surface freezes.
    wr(
        &h.root,
        "b.md",
        &format!("---\ntype: note\ndescription: refers\n---\nsee [[note]] and [[note::@{sha}]]\n"),
    );
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "note",
            "new_name": "memo",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let b = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        b.contains("type: memo"),
        "the claim cascade was frozen too — a pin must not stop it: {b}"
    );
    assert!(
        b.contains("[[memo]]"),
        "the live wikilink was not rewritten: {b}"
    );
    assert!(
        b.contains(&format!("[[note::@{sha}]]")),
        "the pinned wikilink was rewritten: {b}"
    );
}

/// Write `content` to `root/rel`, creating parent directories.
fn wr(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The `Mutation-Id` trailer of a repo's HEAD commit.
fn mutation_id_at_head(repo: &std::path::Path) -> String {
    git_out(repo, &["log", "-1", "--format=%B"])
        .lines()
        .find_map(|l| l.strip_prefix("Mutation-Id: "))
        .expect("HEAD carries a Mutation-Id")
        .to_string()
}

/// Three co-present member repos under one walked root, each its own git repo.
/// `ra` holds the typed `a.md`; `rb` and `rc` each reference it cross-repo via
/// `[[a::ra]]`. The pattern mirrors the cross-repo reference tests, plus git.
fn three_repo_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["ra", "rb", "rc"]);

    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(
        &root,
        "ra/type/note.type.yaml",
        "fields:\n  description: String\n",
    );
    wr(
        &root,
        "ra/a.md",
        "---\ntype: note\ndescription: original\n---\n",
    );

    for r in ["rb", "rc"] {
        wr(
            &root,
            &format!("{r}/.arsumbris/repo.yaml"),
            &format!("name: {r}\ndeps:\n  - name: ra\n"),
        );
        wr(&root, &format!("{r}/doc.md"), "see [[a::ra]] for context\n");
    }

    for r in ["ra", "rb", "rc"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    (dir, root)
}

#[test]
fn cross_repo_rename_commits_every_repo_with_one_mutation_id() {
    let (dir, root) = three_repo_workspace();
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "ra/a.md",
            "to": "ra/c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The move landed in ra.
    assert!(!h.root.join("ra/a.md").exists(), "a.md still present in ra");
    assert!(h.root.join("ra/c.md").exists(), "c.md missing in ra");

    // Both referrers were rewritten to the new name, the ::ra qualifier kept.
    for r in ["rb", "rc"] {
        let doc = fs::read_to_string(h.root.join(r).join("doc.md")).unwrap();
        assert!(doc.contains("[[c::ra]]"), "{r} not rewritten: {doc}");
        assert!(
            !doc.contains("[[a::ra]]"),
            "{r} old reference survived: {doc}"
        );
    }

    // Every repo committed, all three sharing one Mutation-Id.
    let ids: Vec<String> = ["ra", "rb", "rc"]
        .iter()
        .map(|r| mutation_id_at_head(&h.root.join(r)))
        .collect();
    assert_eq!(ids[0], ids[1], "ra and rb share the Mutation-Id: {ids:?}");
    assert_eq!(ids[1], ids[2], "rb and rc share the Mutation-Id: {ids:?}");

    // The result carries every committing repo, nothing dropped, each its HEAD.
    let commits = resp["result"]["commits"]
        .as_object()
        .expect("commits map present");
    assert_eq!(commits.len(), 3, "every repo committed: {commits:?}");
    for r in ["ra", "rb", "rc"] {
        let sha = commits[r]
            .as_str()
            .unwrap_or_else(|| panic!("{r} sha missing: {commits:?}"));
        assert_eq!(
            sha,
            git_out(&h.root.join(r), &["rev-parse", "HEAD"]),
            "{r}'s map sha is its HEAD"
        );
    }
    // result.path is ra/c.md, so the `commit` scalar is ra's HEAD: the response
    // file's own repo, not a different repo's sha.
    let commit = resp["result"]["commit"].as_str().expect("commit scalar");
    assert_eq!(
        commit,
        commits["ra"].as_str().unwrap(),
        "commit anchors result.path's own repo (ra)"
    );

    // Every working tree is clean.
    for r in ["ra", "rb", "rc"] {
        assert!(
            git_out(&h.root.join(r), &["status", "--porcelain"]).is_empty(),
            "{r} working tree not clean"
        );
    }
}

#[test]
fn cross_repo_rename_failure_compensates_every_repo() {
    let (dir, root) = three_repo_workspace();
    let mut h = start(dir, root);

    // Make rc's referrer unwritable, so the rewrite fails mid-saga.
    let rc_doc = h.root.join("rc/doc.md");
    let mut perms = fs::metadata(&rc_doc).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&rc_doc, perms).unwrap();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "ra/a.md",
            "to": "ra/c.md",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "the saga should reject: {resp:?}");

    // Nothing moved, nothing rewritten, every tree clean — all repos compensated.
    assert!(h.root.join("ra/a.md").exists(), "a.md should be restored");
    assert!(!h.root.join("ra/c.md").exists(), "c.md should not exist");
    for r in ["rb", "rc"] {
        let doc = fs::read_to_string(h.root.join(r).join("doc.md")).unwrap();
        assert!(
            doc.contains("[[a::ra]]"),
            "{r} should still reference the old name: {doc}"
        );
    }
    for r in ["ra", "rb", "rc"] {
        assert!(
            git_out(&h.root.join(r), &["status", "--porcelain"]).is_empty(),
            "{r} not clean after compensation"
        );
    }

    // Restore write permission so the temp dir can be cleaned up.
    let mut perms = fs::metadata(&rc_doc).unwrap().permissions();
    perms.set_readonly(false);
    let _ = fs::set_permissions(&rc_doc, perms);
}

/// A two-repo workspace where the file's OWNING repo (`zz`) sorts AFTER the
/// referrer's repo (`aa`). `zz` holds the typed `a.md`; `aa` references it via
/// `[[a::zz]]`. A rename of `zz/a.md` commits both, name-sorted: `aa` first,
/// `zz` second — so the name-sorted-first commit is NOT result.path's repo.
fn referrer_sorts_before_owner_workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["aa", "zz"]);

    wr(&root, "zz/.arsumbris/repo.yaml", "name: zz\n");
    wr(
        &root,
        "zz/type/note.type.yaml",
        "fields:\n  description: String\n",
    );
    wr(
        &root,
        "zz/a.md",
        "---\ntype: note\ndescription: original\n---\n",
    );
    wr(
        &root,
        "aa/.arsumbris/repo.yaml",
        "name: aa\ndeps:\n  - name: zz\n",
    );
    wr(&root, "aa/doc.md", "see [[a::zz]] for context\n");

    for r in ["aa", "zz"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    (dir, root)
}

#[test]
fn mutate_commit_follows_result_path_repo_not_commit_order() {
    let (dir, root) = referrer_sorts_before_owner_workspace();
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "zz/a.md",
            "to": "zz/b.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let commits = resp["result"]["commits"]
        .as_object()
        .expect("commits map present");
    assert_eq!(commits.len(), 2, "both repos committed: {commits:?}");

    // result.path is zz/b.md. The `commit` scalar must be zz's sha — result.path's
    // own repo — NOT aa's, even though aa is the name-sorted-first commit. This is
    // the regression guard against the old first-of-N behaviour.
    let commit = resp["result"]["commit"].as_str().expect("commit scalar");
    assert_eq!(
        commit,
        commits["zz"].as_str().unwrap(),
        "commit anchors zz, result.path's own repo"
    );
    assert_ne!(
        commit,
        commits["aa"].as_str().unwrap(),
        "commit must not be the name-sorted-first (aa) sha"
    );
}

/// A two-repo workspace for cross-repo refactor tests. `ra` owns the canvas /
/// node types; `rb` declares `ra` as a peer. The caller supplies each repo's
/// files (paths relative to that repo's root); both repos are git-seeded.
fn cross_repo_canvas_workspace(
    ra_files: &[(&str, &str)],
    rb_files: &[(&str, &str)],
) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["ra", "rb"]);
    wr(&root, "ra/.arsumbris/repo.yaml", "name: ra\n");
    wr(
        &root,
        "ra/type/canvas.type.yaml",
        "fields:\n  nodes: node&[]\n",
    );
    wr(
        &root,
        "ra/type/node.type.yaml",
        "fields:\n  content?: String\n  related?: node&\n",
    );
    for (rel, content) in ra_files {
        wr(&root, &format!("ra/{rel}"), content);
    }
    wr(
        &root,
        "rb/.arsumbris/repo.yaml",
        "name: rb\ndeps:\n  - name: ra\n",
    );
    for (rel, content) in rb_files {
        wr(&root, &format!("rb/{rel}"), content);
    }
    for r in ["ra", "rb"] {
        let repo = root.join(r);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["config", "user.email", "tester@example.com"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
    }
    (dir, root)
}

#[test]
fn cross_repo_promote_redirects_a_referrer_including_a_nested_block() {
    // A record in `ra`, referenced from `rb` both by its own id and by a nested
    // id. Promote it into a new `ra` file: the cross-repo referrer follows, the
    // `::ra` qualifier kept, the outer id collapsing and the nested id preserved.
    let (dir, root) = cross_repo_canvas_workspace(
        &[(
            "canvas.md",
            "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n    related:\n      ^: kid\n      type: node\n      content: y\n---\n",
        )],
        &[("doc.md", "outer [[canvas::ra^rec]] and nested [[canvas::ra^kid]]\n")],
    );
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "ra/canvas.md",
            "block_id": "rec",
            "to": "ra/promoted.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let doc = fs::read_to_string(h.root.join("rb/doc.md")).unwrap();
    assert!(
        doc.contains("[[promoted::ra]]"),
        "outer cross-repo ref not collapsed: {doc}"
    );
    assert!(
        doc.contains("[[promoted::ra^kid]]"),
        "nested cross-repo ref not preserved: {doc}"
    );
    assert!(
        !doc.contains("canvas::ra"),
        "a stale cross-repo ref survived: {doc}"
    );

    assert_eq!(
        mutation_id_at_head(&h.root.join("ra")),
        mutation_id_at_head(&h.root.join("rb")),
        "ra and rb should share one Mutation-Id"
    );
    for r in ["ra", "rb"] {
        assert!(
            git_out(&h.root.join(r), &["status", "--porcelain"]).is_empty(),
            "{r} working tree not clean"
        );
    }
}

#[test]
fn cross_repo_inline_repoints_a_referrer_including_a_nested_block() {
    // A file in `ra`, referenced from `rb` both whole and by a nested id. Inline
    // it into an `ra` host: the cross-repo whole-file ref takes the new record id,
    // the nested ref keeps its id, both under `[[a::ra^…]]`.
    let (dir, root) = cross_repo_canvas_workspace(
        &[
            (
                "child.md",
                "---\ntype: node\ncontent: x\nrelated:\n  ^: kid\n  type: node\n  content: y\n---\n",
            ),
            ("a.md", "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n"),
        ],
        &[("doc.md", "whole [[child::ra]] and nested [[child::ra^kid]]\n")],
    );
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "ra/child.md",
            "into": "ra/a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    assert!(
        !h.root.join("ra/child.md").exists(),
        "child.md should be deleted"
    );
    let doc = fs::read_to_string(h.root.join("rb/doc.md")).unwrap();
    // The nested `^kid` stays a bare navigational anchor across the repo boundary.
    assert!(
        doc.contains("[[a::ra^kid]]") && !doc.contains("[[a::ra^^kid]]"),
        "nested cross-repo ref not preserved as a bare `^`: {doc}"
    );
    // The whole-file ref repoints at the host record as a BLOCK-REFERENT (`^^`),
    // so it pulls the record's value, not the host file.
    assert!(
        doc.contains("[[a::ra^^"),
        "whole-file cross-repo ref not repointed as a `^^` block-referent: {doc}"
    );
    assert!(
        !doc.contains("child::ra"),
        "a stale cross-repo ref survived: {doc}"
    );

    for r in ["ra", "rb"] {
        assert!(
            git_out(&h.root.join(r), &["status", "--porcelain"]).is_empty(),
            "{r} working tree not clean"
        );
    }
}

#[test]
fn cross_repo_rename_block_id_follows_a_referrer() {
    let (dir, root) = cross_repo_canvas_workspace(
        &[(
            "canvas.md",
            "---\ntype: canvas\nnodes:\n  - ^: rec\n    type: node\n    content: x\n---\n",
        )],
        &[(
            "doc.md",
            "nav [[canvas::ra^rec]] and pull [[canvas::ra^^rec]] here\n",
        )],
    );
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_block_id",
            "path": "ra/canvas.md",
            "block_id": "rec",
            "to_block_id": "ref2",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let doc = fs::read_to_string(h.root.join("rb/doc.md")).unwrap();
    // Both modes follow the rename across the repo boundary, each keeping its own
    // caret count: the bare `^rec` stays navigational, the `^^rec` stays a
    // block-referent.
    assert!(
        doc.contains("[[canvas::ra^ref2]]") && doc.contains("[[canvas::ra^^ref2]]"),
        "cross-repo block-id refs not renamed in both modes: {doc}"
    );
    assert!(!doc.contains("rec]]"), "a stale `rec` id survived: {doc}");
    for r in ["ra", "rb"] {
        assert!(
            git_out(&h.root.join(r), &["status", "--porcelain"]).is_empty(),
            "{r} working tree not clean"
        );
    }
}

#[test]
fn inline_drops_the_repo_qualifier_when_rebasing_to_a_local_reference() {
    // A file self-references one of its blocks with its own repo qualifier
    // (`[[child::ra^kid]]` — `::ra` resolves because ra is a co-present repo).
    // Inlining rebases that to a local reference; the repo qualifier must drop,
    // not produce a malformed empty-target `[[::ra^kid]]`.
    let (dir, root) = cross_repo_canvas_workspace(
        &[
            (
                "child.md",
                "---\ntype: node\ncontent: \"self [[child::ra^kid]]\"\nrelated:\n  ^: kid\n  type: node\n  content: y\n---\n",
            ),
            ("a.md", "---\ntype: canvas\nnodes:\n  - \"[[child]]\"\n---\n"),
        ],
        &[],
    );
    let mut h = start(dir, root);

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "ra/child.md",
            "into": "ra/a.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let a = fs::read_to_string(h.root.join("ra/a.md")).unwrap();
    assert!(
        !a.contains("[[::ra"),
        "a malformed empty-target repo link was produced: {a:?}"
    );
    assert!(
        !a.contains("child::ra"),
        "a dangling self-reference survived: {a:?}"
    );
    assert!(
        a.contains("[[^kid]]"),
        "the self-reference was not rebased to a clean local form: {a:?}"
    );
}

#[test]
fn cross_repo_referrer_drift_rejects() {
    // The per-referrer hash guard applies across the repo boundary: a cross-repo
    // referrer that drifted from the held index (committed, tree clean, no
    // rebuild) rejects the whole refactor.
    let (dir, root) = three_repo_workspace();
    let mut h = start(dir, root);

    // Drift rb's cross-repo referrer to a same-length link at the same offset,
    // commit it, no rebuild.
    wr(&h.root, "rb/doc.md", "see [[c::ra]] for context\n");
    git(&h.root.join("rb"), &["add", "."]);
    git(
        &h.root.join("rb"),
        &["commit", "-q", "-m", "out-of-band edit"],
    );

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "ra/a.md",
            "to": "ra/b.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a drifted cross-repo referrer should reject, got {resp:?}"
    );
    assert!(
        resp["error"].as_str().unwrap().contains("changed since"),
        "the reject should name the referrer drift, got {resp:?}"
    );
    assert!(
        h.root.join("ra/a.md").exists() && !h.root.join("ra/b.md").exists(),
        "the rename should not have happened"
    );
}

#[test]
fn rename_rewrites_a_link_embedded_in_a_longer_string_value() {
    let mut h = started_in_git();

    // The reference sits inside a longer frontmatter string value, not as a
    // whole-value slot reference.
    fs::write(
        h.root.join("b.md"),
        "---\ntype: note\ndescription: see [[a]] for context\n---\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "add referrer"]);
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "c.md",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // The embedded link is rewritten in place, the surrounding text intact.
    let referrer = fs::read_to_string(h.root.join("b.md")).unwrap();
    assert!(
        referrer.contains("description: see [[c]] for context"),
        "the embedded link was not rewritten in place: {referrer}"
    );
}

// --- ensure_mixins rider ---

/// Seed `marker` (a no-field tag) and `tagged` (a required `tag`) type-defs into
/// the git fixture and commit them, so the working tree stays clean.
fn seed_mixin_types(h: &Harness) {
    fs::write(h.root.join("type/marker.type.yaml"), "fields: {}\n").unwrap();
    fs::write(
        h.root.join("type/tagged.type.yaml"),
        "fields:\n  tag: String\n",
    )
    .unwrap();
    git(&h.root, &["add", "."]);
    git(&h.root, &["commit", "-q", "-m", "seed mixin types"]);
    h.engine.rebuild();
}

#[test]
fn ensure_mixin_applies_and_lands_in_one_commit() {
    let mut h = started_in_git();
    seed_mixin_types(&h);
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "n.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    // Exactly one commit, carrying the content AND the mixin claim.
    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "the write and its mixin are one commit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:n.md")]);
    assert!(
        committed.contains("type: [note, marker]"),
        "the mixin rides the write's commit: {committed}"
    );
    assert_eq!(
        resp["result"]["ensure_mixins"][0]["outcome"], "applied",
        "the response reports the applied outcome: {resp:?}"
    );
    assert_eq!(resp["result"]["ensure_mixins"][0]["mixin"], "marker");
}

#[test]
fn ensure_mixin_strict_rejects_and_the_edit_is_compensated() {
    let mut h = started_in_git();
    seed_mixin_types(&h);
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    // `tagged` requires `tag`, which `a.md` does not supply, so the mixin is
    // un-appliable and strict (the default) rejects the whole edit.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "original", "new_string": "changed",
            "ensure_mixins": ["tagged"]
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "an un-appliable mixin rejects under strict: {resp:?}"
    );

    // Nothing committed, and the edit was compensated back to HEAD.
    assert_eq!(
        git_out(&h.root, &["rev-parse", "HEAD"]),
        before,
        "the strict reject committed nothing"
    );
    let on_disk = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert!(
        on_disk.contains("description: original") && !on_disk.contains("tagged"),
        "the rejected edit left a.md unchanged: {on_disk}"
    );
}

#[test]
fn ensure_mixin_lenient_skips_and_reports() {
    let mut h = started_in_git();
    seed_mixin_types(&h);

    // Same un-appliable mixin, but lenient: the edit lands, the mixin is skipped
    // and reported, and the claim is untouched.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "a.md",
            "old_string": "original", "new_string": "changed",
            "ensure_mixins": ["tagged"],
            "ensure_mixins_strict": false
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "the lenient edit lands: {resp:?}");
    let on_disk = fs::read_to_string(h.root.join("a.md")).unwrap();
    assert!(
        on_disk.contains("description: changed"),
        "the primary edit applied: {on_disk}"
    );
    assert!(
        on_disk.contains("type: note") && !on_disk.contains("tagged"),
        "the un-appliable mixin was not added: {on_disk}"
    );
    assert_eq!(
        resp["result"]["ensure_mixins"][0]["outcome"], "skipped",
        "the skip is reported: {resp:?}"
    );
    assert!(
        resp["result"]["ensure_mixins"][0]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("tag")),
        "the skip carries its reason: {resp:?}"
    );
}

#[test]
fn ensure_mixin_already_present_is_a_no_op() {
    let mut h = started_in_git();
    seed_mixin_types(&h);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "m.md",
            "content": "---\ntype: [note, marker]\ndescription: hi\n---\n",
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert_eq!(
        resp["result"]["ensure_mixins"][0]["outcome"], "no_op",
        "an already-present mixin is a no-op: {resp:?}"
    );
    let on_disk = fs::read_to_string(h.root.join("m.md")).unwrap();
    assert_eq!(
        on_disk.matches("marker").count(),
        1,
        "the claim was not duplicated: {on_disk}"
    );
}

#[test]
fn a_stamp_and_a_mixin_ride_one_commit() {
    let mut h = started_in_git();
    seed_mixin_types(&h);
    let before = git_out(&h.root, &["rev-parse", "HEAD"]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "s.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "stamps": [{ "field": "provenance", "record": { "type": "file-change.create" } }],
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");

    let head = git_out(&h.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_out(
            &h.root,
            &["rev-list", "--count", &format!("{before}..{head}")]
        ),
        "1",
        "the write, its stamp, and its mixin are one commit"
    );
    let committed = git_out(&h.root, &["show", &format!("{head}:s.md")]);
    assert!(
        committed.contains("type: [note, marker]")
            && committed.contains("- type: file-change.create"),
        "the file is both typed and stamped in one commit: {committed}"
    );
}

#[test]
fn ensure_mixin_creates_the_claim_on_a_claimless_note() {
    let mut h = started_in_git();
    seed_mixin_types(&h);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "p.md",
            "content": "---\ndescription: hi\n---\nbody\n",
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let on_disk = fs::read_to_string(h.root.join("p.md")).unwrap();
    assert!(
        on_disk.contains("type: marker") && on_disk.contains("description: hi"),
        "the claimless note is promoted, its field kept: {on_disk}"
    );
}

#[test]
fn rename_carries_a_mixin_onto_the_destination() {
    let mut h = started_in_git();
    seed_mixin_types(&h);

    let resp = h
        .client
        .query(&json!({
            "mutate": "rename",
            "path": "a.md",
            "to": "r.md",
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    let on_disk = fs::read_to_string(h.root.join("r.md")).unwrap();
    assert!(
        on_disk.contains("type: [note, marker]"),
        "the renamed file gained the mixin: {on_disk}"
    );
}

#[test]
fn an_unresolvable_mixin_repo_rejects_under_strict() {
    let mut h = started_in_git();
    seed_mixin_types(&h);

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "u.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "ensure_mixins": ["ghost::nope"]
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a mixin naming an unknown repo rejects: {resp:?}"
    );
}

#[test]
fn ensure_mixin_applies_off_git_without_committing() {
    let mut h = started();
    // Off-git, so seed the mixin type without a commit.
    fs::write(h.root.join("type/marker.type.yaml"), "fields: {}\n").unwrap();
    h.engine.rebuild();

    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "o.md",
            "content": "---\ntype: note\ndescription: hi\n---\n",
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert!(
        resp["result"]["commit"].is_null(),
        "off-git there is no commit: {resp:?}"
    );
    let on_disk = fs::read_to_string(h.root.join("o.md")).unwrap();
    assert!(
        on_disk.contains("type: [note, marker]"),
        "the mixin applied to disk off-git: {on_disk}"
    );
}

/// `base` owns `note` and `pmark` (a no-field mixin). `app` peers `base` and holds
/// a card claiming `note::base` — so `app` imports `base`, but has never claimed
/// `pmark::base`.
fn workspace_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w("base/.arsumbris/repo.yaml", "name: base\n");
    w(
        "base/type/note.type.yaml",
        "fields:\n  description: String\n",
    );
    w("base/type/pmark.type.yaml", "fields: {}\n");
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ndeps:\n  - name: base\n",
    );
    w(
        "app/card.md",
        "---\ntype: note::base\ndescription: hi\n---\n",
    );
    (dir, root)
}

#[test]
fn a_rider_on_a_type_def_target_rejects_without_corrupting_it() {
    let mut h = started_in_git();
    let def = "fields:\n  x: String\n";
    // A standalone type-def: its `type:` is a parent claim, not an instance
    // identity. A stamp or a mixin must reject.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "type/widget.type.yaml",
            "content": def,
            "ensure_mixins": ["marker"]
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a mixin on a type-def rejects: {resp:?}"
    );

    // A stamp on the same target rejects too (the shared guard).
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "type/widget.type.yaml",
            "content": def,
            "stamps": [{ "field": "provenance", "record": { "type": "file-change.edit" } }]
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a stamp on a type-def rejects: {resp:?}"
    );

    // A PLAIN write to the same type-def (no rider) still works, and lands clean.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "type/widget.type.yaml",
            "content": def
        }))
        .unwrap();
    assert_eq!(
        resp["ready"], true,
        "a plain type-def write works: {resp:?}"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("type/widget.type.yaml")).unwrap(),
        def,
        "the type-def is byte-identical, never `---`-wrapped"
    );
}

#[test]
fn a_cross_repo_mixin_applies_via_the_on_demand_fold() {
    // `pmark::base` is a declared dependency's type that no `app` file has claimed
    // yet, so it is absent from the pre-built resolution graph. The gate must
    // resolve it via the on-demand fold, not falsely reject it.
    let (dir, root) = workspace_fixture();
    let mut h = start(dir, root);
    let card = h.root.join("app/card.md").display().to_string();

    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": card,
            "old_string": "hi", "new_string": "hello",
            "ensure_mixins": ["pmark::base"]
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "got {resp:?}");
    assert_eq!(
        resp["result"]["ensure_mixins"][0]["outcome"], "applied",
        "the cross-repo mixin resolved and applied: {resp:?}"
    );
    let on_disk = fs::read_to_string(h.root.join("app/card.md")).unwrap();
    assert!(
        on_disk.contains("type: [note::base, pmark::base]"),
        "the peer mixin was added to the qualified claim: {on_disk}"
    );
}
