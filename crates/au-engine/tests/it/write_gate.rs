//! The mutation channel's editability gate: a write reaches any declared member
//! the same way a read does, but only an EDITABLE member (the entry or an `edit`
//! member) may be authored through the channel. A consumed member (`discover` /
//! `dep`) is rejected, so a write never lands on an unwatched, regenerable cache
//! snapshot.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::{json, Value};

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    engine: Engine,
    _server: ServeHandle,
    client: Client,
    root: PathBuf,
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

/// A workspace whose entry `v` is editable and whose member `dep` is mounted for
/// DISCOVERY only, a consumed member. `seed_workspace` lists everything in `edit`,
/// so the `discover:` composition is written inline here. `dep/note.md` is the
/// consumed-member file the gate must protect.
fn started() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_repo(&root);
    // Entry `v` composes itself (edit) plus `dep` (discover), overriding the
    // all-edit workspace.yaml that seed_repo/seed_workspace would write.
    fs::write(
        root.join(".arsumbris/workspace.yaml"),
        "type: au.engine.workspace::au-engine\nedit:\n  - v\ndiscover:\n  - dep\n",
    )
    .unwrap();
    // A type and an instance in the editable entry, so the entry-write control
    // case has somewhere valid to land.
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

    // The consumed `dep` member: its own folder-repo with a file to target.
    let dep = root.join("dep");
    fs::create_dir_all(dep.join(".arsumbris")).unwrap();
    fs::write(
        dep.join(".arsumbris/repo.yaml"),
        "name: dep\ntype: au.engine.repo::au-engine\n",
    )
    .unwrap();
    crate::seed_readme(&dep);
    fs::write(dep.join("note.md"), "consumed member content\n").unwrap();
    // A file in the consumed member with a cross-repo value link INTO the entry,
    // so renaming the entry's `a.md` would want to rewrite a referrer the engine
    // does not own (GAP 2).
    fs::write(dep.join("refs.md"), "see [[a::v]]\n").unwrap();
    // A type-def OWNED by the consumed member, so `rename_type` has a consumed
    // def file to refuse (GAP 1).
    fs::create_dir(dep.join("type")).unwrap();
    fs::write(
        dep.join("type/widget.type.yaml"),
        "fields:\n  label: String\n",
    )
    .unwrap();

    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Tester"]);
    git(&root, &["config", "user.email", "tester@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "seed"]);

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
        root,
    }
}

/// Assert a reject frame names the consumed member and the fix.
fn assert_not_editable(resp: &Value) {
    assert_eq!(resp["type"], "error", "expected a reject, got {resp:?}");
    let msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not editable") && msg.contains("discover") && msg.contains("edit:"),
        "reject names the role and the fix: {msg:?}"
    );
}

#[test]
fn writes_to_a_discover_member_reject_without_touching_disk() {
    let mut h = started();

    // write_file over an existing consumed-member file.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file", "path": "dep/note.md", "content": "clobbered\n",
        }))
        .unwrap();
    assert_not_editable(&resp);

    // write_file creating a NEW file inside the consumed member.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file", "path": "dep/new.md", "content": "x\n",
        }))
        .unwrap();
    assert_not_editable(&resp);

    // edit_file over the consumed-member file.
    let resp = h
        .client
        .query(&json!({
            "mutate": "edit_file",
            "path": "dep/note.md",
            "old_string": "consumed member content",
            "new_string": "edited",
        }))
        .unwrap();
    assert_not_editable(&resp);

    // delete_file of the consumed-member file.
    let resp = h
        .client
        .query(&json!({ "mutate": "delete_file", "path": "dep/note.md" }))
        .unwrap();
    assert_not_editable(&resp);

    // Nothing landed: the file is untouched and the new file never appeared.
    assert_eq!(
        fs::read_to_string(h.root.join("dep/note.md")).unwrap(),
        "consumed member content\n",
        "a rejected write leaves the consumed member's file byte-for-byte"
    );
    assert!(
        !h.root.join("dep/new.md").exists(),
        "a rejected create writes no file"
    );
}

#[test]
fn preview_over_a_discover_member_reports_the_same_reject() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "read": "preview_mutation",
            "op": "write_file",
            "path": "dep/note.md",
            "content": "clobbered\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "{resp:?}");
    let msg = resp["result"]["preview_mutation"]["reject"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        msg.contains("not editable") && msg.contains("discover"),
        "preview reports the identical not-editable reject: {msg:?}"
    );
}

#[test]
fn rename_type_of_a_consumed_members_def_rejects_and_moves_nothing() {
    let mut h = started();
    // `widget` is owned by the discover member `dep`. Renaming it would move a
    // type-def file the engine does not author (GAP 1).
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename_type",
            "old_name": "widget::dep",
            "new_name": "gadget",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "expected a reject, got {resp:?}");
    let msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not editable") && msg.contains("dep"),
        "reject names the consumed owner and editability: {msg:?}"
    );
    // Nothing moved: the def file is untouched, the renamed one never appeared.
    assert!(
        h.root.join("dep/type/widget.type.yaml").exists(),
        "the consumed def file must be untouched"
    );
    assert!(
        !h.root.join("dep/type/gadget.type.yaml").exists(),
        "no renamed def file is created"
    );
}

#[test]
fn a_rename_whose_referrer_is_in_a_consumed_member_rejects_with_the_blocking_list() {
    let mut h = started();
    // `dep/refs.md` (a consumed member) links to the entry's `a.md`. Renaming
    // `a.md` would rewrite that referrer, which the engine may not author (GAP 2).
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename", "path": "a.md", "to": "moved.md",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "expected a reject, got {resp:?}");
    let msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("consumed member") && msg.contains("dep"),
        "reject names the consumed member: {msg:?}"
    );
    // The blocking list is machine-usable in `detail`.
    let blocking = &resp["detail"]["blocking_consumed_referrers"]["dep"];
    let files: Vec<String> = blocking
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        files.iter().any(|f| f.ends_with("dep/refs.md")),
        "detail names the blocking referrer file: {blocking:?}"
    );
    // Nothing moved: the rename never happened.
    assert!(
        h.root.join("a.md").exists() && !h.root.join("moved.md").exists(),
        "a rejected rename moves nothing"
    );
    // And the promote-to-edit route unblocks: once `dep` is an edit member, the
    // same rename succeeds and rewrites the (now editable) referrer.
    fs::write(
        h.root.join(".arsumbris/workspace.yaml"),
        "type: au.engine.workspace::au-engine\nedit:\n  - v\n  - dep\n",
    )
    .unwrap();
    h.engine.rebuild();
    let resp = h
        .client
        .query(&json!({
            "mutate": "rename", "path": "a.md", "to": "moved.md",
        }))
        .unwrap();
    assert_eq!(
        resp["ready"], true,
        "with dep editable, the rename lands: {resp:?}"
    );
    let refs = fs::read_to_string(h.root.join("dep/refs.md")).unwrap();
    assert!(
        refs.contains("[[moved::v]]"),
        "the now-editable referrer was rewritten: {refs}"
    );
}

#[test]
fn set_ignores_into_a_consumed_member_rejects() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores", "root": "dep", "patterns": ["skip/"],
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "expected a reject, got {resp:?}");
    let msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not editable") && msg.contains("dep"),
        "reject names the consumed member: {msg:?}"
    );
    assert!(
        !h.root.join("dep/.arsumbris/.auignore").exists(),
        "a rejected set_ignores writes no file"
    );
}

#[test]
fn set_config_repo_into_a_consumed_member_rejects() {
    let mut h = started();
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_config",
            "scope": "repo",
            "root": "dep",
            "consumer": "host-app",
            "file": "v.yaml",
            "type": "viewer-default-set",
            "content": "viewer: a\n",
        }))
        .unwrap();
    assert_eq!(resp["type"], "error", "expected a reject, got {resp:?}");
    let msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("not editable") && msg.contains("dep"),
        "reject names the consumed member: {msg:?}"
    );
}

#[test]
fn set_ignores_on_the_editable_entry_still_writes() {
    let mut h = started();
    // Control: the same config channel writes into the editable entry, so the gate
    // rejects on role, not on incidental fixture breakage.
    let resp = h
        .client
        .query(&json!({
            "mutate": "set_ignores", "root": ".", "patterns": ["skip/"],
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "the entry is editable: {resp:?}");
    assert!(
        h.root.join(".arsumbris/.auignore").exists(),
        "the entry set_ignores landed"
    );
}

#[test]
fn the_editable_entry_still_writes() {
    let mut h = started();
    // A control: the same channel writes cleanly into the editable entry, so the
    // gate rejects on role, not on some incidental fixture breakage.
    let resp = h
        .client
        .query(&json!({
            "mutate": "write_file",
            "path": "b.md",
            "content": "---\ntype: note\ndescription: fresh\n---\n",
        }))
        .unwrap();
    assert_eq!(resp["ready"], true, "the entry is editable: {resp:?}");
    assert!(h.root.join("b.md").exists(), "the entry write landed");
}
