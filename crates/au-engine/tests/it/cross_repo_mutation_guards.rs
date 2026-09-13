//! Cross-repo parity for the promote / inline mutation guards.
//!
//! The guards sit on `slot_shape_at` (`record_targets.rs`), which resolves the
//! host's top-level shape own-graph-only. When the host file claims a peer type
//! (`type: holder::base`), that claim does not resolve against the consumer's own
//! graph, so the guard sees no slot and skips — letting a mutation that would
//! leave the graph broken proceed unchecked. The single-repo path works; the
//! cross-repo path silently loses the guard.
//!
//! These are the RED tests for the cross-repo sibling surface, mirroring the
//! single-repo `promote_rejects_a_bare_inline_only_host_slot` /
//! `inline into a reference-only slot` guards in `mutate.rs`. They fail against
//! current code (the guard does not fire cross-repo, so the broken write lands)
//! and pass once `slot_shape_at` becomes owner-relative.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use au_engine::{serve, Client, ConfigSource, Engine, ServeHandle};
use serde_json::json;

struct Harness {
    _dir: tempfile::TempDir,
    _sock_dir: tempfile::TempDir,
    _engine: Engine,
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

/// A two-repo workspace: `base` owns the schema, `app` peers `base` and holds the
/// host instances that claim its types. Git-initialized at the entry root, so the
/// one enclosing `.git` covers every member's mutation commit. `writes` seeds the
/// `app`-side content each test needs on top of the shared base.
fn started(writes: &[(&str, &str)]) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    crate::seed_workspace(&root, &["base", "app"]);
    let w = |rel: &str, content: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    w(
        "base/.arsumbris/repo.yaml",
        "name: base\ntype: au.engine.repo::au-engine\n",
    );
    crate::seed_readme(&root.join("base"));
    w(
        "app/.arsumbris/repo.yaml",
        "name: app\ntype: au.engine.repo::au-engine\ndeps:\n  - name: base\n",
    );
    crate::seed_readme(&root.join("app"));
    for (rel, content) in writes {
        w(rel, content);
    }

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
        _engine: engine,
        _server: server,
        client,
        root,
    }
}

/// Promote out of a bare inline-only slot must reject — the slot would be left
/// holding a `[[newFile]]` reference it cannot accept. The slot (`root: node`) is
/// declared on `holder`, owned by `base`; the host claims `holder::base`, so the
/// guard must resolve the slot owner-relative to see it is inline-only.
///
/// The single-repo sibling is `mutate::promote_rejects_a_bare_inline_only_host_slot`.
#[test]
fn promote_rejects_a_bare_inline_only_host_slot_cross_repo() {
    let host = "---\ntype: holder::base\nroot:\n  type: node::base\n  content: x\n---\n";
    let mut h = started(&[
        ("base/type/holder.type.yaml", "fields:\n  root: node\n"),
        ("base/type/node.type.yaml", "fields:\n  content?: String\n"),
        ("app/holder.md", host),
    ]);

    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "app/holder.md",
            "at": at,
            "to": "app/promoted.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a bare inline-only slot rejects, even cross-repo, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("inline-or-reference"),
        "the reject names the `&` requirement, got {resp:?}"
    );
    assert!(
        !h.root.join("app/promoted.md").exists(),
        "nothing should be written on a rejected promote"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("app/holder.md")).unwrap(),
        host,
        "the host file is untouched on a rejected promote"
    );
}

/// Promote at DEPTH: a claim-less slot-pinned peer record nested inside another
/// claim-less slot-pinned peer record. The host claims `outer::base` (peer), whose
/// `root: mid` slot pins a `mid::base` record, whose `leaf: deepnode` slot (bare,
/// inline-only) pins a `deepnode::base` record. Promoting the DEEP `leaf` record
/// forces `slot_shape_at` to DESCEND owner-relative — resolve `outer::base`, then
/// `mid::base`'s `leaf` slot — to see the slot is inline-only and reject. This is a
/// distinct traversal from the single-level guard (the enumerate walk proves depth
/// elsewhere; the walk-to-a-byte-span does not). Own-graph-only, the top-level
/// `outer::base` does not resolve, the descent never starts, and promote lands a
/// `[[newFile]]` reference in an inline-only slot — a broken write.
#[test]
fn promote_rejects_a_deep_slot_pinned_inline_only_slot_cross_repo() {
    let host = "---\ntype: outer::base\nroot:\n  leaf:\n    content: x\n---\n";
    let mut h = started(&[
        ("base/type/outer.type.yaml", "fields:\n  root: mid\n"),
        ("base/type/mid.type.yaml", "fields:\n  leaf: deepnode\n"),
        (
            "base/type/deepnode.type.yaml",
            "fields:\n  content?: String\n",
        ),
        ("app/holder.md", host),
    ]);

    // Offset inside the DEEP `leaf` record (its `content: x`), so the locator
    // finds the leaf record and the guard must resolve its slot two levels down.
    let at = host.find("content: x").unwrap();
    let resp = h
        .client
        .query(&json!({
            "mutate": "promote",
            "path": "app/holder.md",
            "at": at,
            "to": "app/promoted.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a deep bare inline-only slot rejects, even cross-repo, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("inline-or-reference"),
        "the reject names the `&` requirement, got {resp:?}"
    );
    assert!(
        !h.root.join("app/promoted.md").exists(),
        "nothing should be written on a rejected deep promote"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("app/holder.md")).unwrap(),
        host,
        "the host file is untouched on a rejected deep promote"
    );
}

/// Inline into a reference-only (`*`) slot must reject — the slot holds the
/// `[[target]]` reference but cannot hold the inlined record. The slot
/// (`link: node*`) is declared on `holder`, owned by `base`; the host `into`
/// claims `holder::base`, so the guard must resolve the slot owner-relative.
///
/// The single-repo sibling is the `inline into a reference-only slot` guard in
/// `mutate.rs`.
#[test]
fn inline_rejects_a_reference_only_host_slot_cross_repo() {
    let into = "---\ntype: holder::base\nlink: \"[[target]]\"\n---\n";
    let target = "---\ntype: node::base\n---\n";
    let mut h = started(&[
        ("base/type/holder.type.yaml", "fields:\n  link: node*\n"),
        ("base/type/node.type.yaml", "fields: {}\n"),
        ("app/into.md", into),
        ("app/target.md", target),
    ]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "app/target.md",
            "into": "app/into.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a reference-only slot rejects inline, even cross-repo, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("inline-or-reference"),
        "the reject names the `&` requirement, got {resp:?}"
    );
    assert!(
        h.root.join("app/target.md").exists(),
        "the referenced file is not deleted on a rejected inline"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("app/into.md")).unwrap(),
        into,
        "the host file is untouched on a rejected inline"
    );
}

/// Inline must reject when ANOTHER referrer addresses the inlined file through a
/// `file*` slot — that slot references a whole file and cannot hold the
/// `[[into^id]]` block-id the rewrite would write. The offending referrer here
/// claims a peer type (`type: ref-holder::base`), so the offender check must
/// resolve its slot owner-relative to see the `file*`; own-graph-only, the slot
/// is unseen, the referrer is not flagged, and inline rewrites it to a block-id
/// the `file*` slot cannot hold.
#[test]
fn inline_rejects_a_cross_repo_file_star_referrer() {
    // `into` (app-owned `host`) references `file` through an `&` slot, so the
    // host-slot guard passes and inline reaches the referrer check. `referrer`
    // claims the peer `ref-holder`, whose `asset: file*` slot also points at
    // `file` — the offender the cross-repo offender check must catch.
    let into = "---\ntype: host\nlink: \"[[file]]\"\n---\n";
    let referrer = "---\ntype: ref-holder::base\nasset: \"[[file]]\"\n---\n";
    let mut h = started(&[
        (
            "base/type/ref-holder.type.yaml",
            "fields:\n  asset: file*\n",
        ),
        ("app/type/node.type.yaml", "fields: {}\n"),
        ("app/type/host.type.yaml", "fields:\n  link: node&\n"),
        ("app/file.md", "---\ntype: node\n---\n"),
        ("app/into.md", into),
        ("app/referrer.md", referrer),
    ]);

    let resp = h
        .client
        .query(&json!({
            "mutate": "inline",
            "path": "app/file.md",
            "into": "app/into.md",
        }))
        .unwrap();
    assert_eq!(
        resp["type"], "error",
        "a cross-repo `file*` referrer blocks inline, got {resp:?}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap()
            .contains("reference the whole file"),
        "the reject names the whole-file-referrer cause, got {resp:?}"
    );
    assert!(
        resp["detail"]
            .to_string()
            .contains("a `file*` slot references a whole file"),
        "the offending referrer is named in detail, got {resp:?}"
    );
    assert!(
        h.root.join("app/file.md").exists(),
        "the inlined file is not deleted on a rejected inline"
    );
    assert_eq!(
        fs::read_to_string(h.root.join("app/referrer.md")).unwrap(),
        referrer,
        "the `file*` referrer is untouched on a rejected inline"
    );
}
