//! A client connects over the IPC socket and issues the starter reads.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use au_engine::{serve, Client, ConfigSource, Engine};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name.
    let root = fs::canonicalize(dir.path()).unwrap().join("v");
    fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(root.join("a.md"), "---\ntype: note\nlink: \"[[b]]\"\n---\n").unwrap();
    fs::write(root.join("b.md"), "---\ntype: note\n---\n").unwrap();
    (dir, root)
}

#[test]
fn client_connects_and_issues_the_starter_reads() {
    let (_dir, root) = fixture();
    // Socket lives outside the knowledge base so the watcher never sees it as content.
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");

    // Before the first build, the ref is Deriving: reads are not ready.
    let mut client = Client::connect(&socket).expect("connect");
    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert_eq!(resp["schema_version"], 29);
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["ready"], false);
    let resp = client.query(&json!({ "read": "diagnostics" })).unwrap();
    assert_eq!(resp["ready"], false);
    assert!(resp.get("version").is_none() || resp["version"].is_null());

    // Build, then the same reads resolve, stamped with the version.
    engine.rebuild();

    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert_eq!(resp["ready"], true);
    assert_eq!(resp["version"], 1);
    assert_eq!(resp["result"]["lifecycle"]["ref"], "ready");

    let resp = client.query(&json!({ "read": "diagnostics" })).unwrap();
    assert_eq!(resp["ready"], true);
    assert_eq!(resp["version"], 1);
    assert!(
        resp["result"]["diagnostics"].is_array(),
        "diagnostics is an array"
    );

    // Resolved view of a.md: claims `note`, and the value layer carries the
    // `link` field with its reference contribution.
    let resp = client
        .query(&json!({ "read": "instance", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    assert_eq!(resp["version"], 1);
    assert_eq!(resp["result"]["instance"]["resolved"], true);
    assert_eq!(resp["result"]["instance"]["claim"][0], "note");
    let values = resp["result"]["instance"]["effective_values"]
        .as_array()
        .unwrap();
    let link = values
        .iter()
        .find(|e| e["field"] == "link")
        .unwrap_or_else(|| panic!("effective_values includes link, got {values:?}"));
    // A whole-value wikilink in a reference-admitting slot is a REFERENCE, on
    // the frontmatter surface exactly as on the body one — the value layer
    // gates on the SLOT, not on which surface the link was written. This
    // assertion previously demanded the opposite (a scalar carrying the raw
    // `[[b]]` text), which was the bug: the two surfaces then produced
    // structurally different values for one target, so a bare slot naming it
    // twice falsely tripped `field-cardinality-exceeded`.
    assert_eq!(link["containers"][0]["value"]["kind"], "reference");
    assert_eq!(link["containers"][0]["value"]["target"], "b");

    // Backlinks of b.md: one inbound edge from a.md through the `link` slot.
    let resp = client
        .query(&json!({ "read": "references_in", "path": "b.md" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    let edges = resp["result"]["references_in"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "one inbound edge, got {edges:?}");
    assert!(edges[0]["source"].as_str().unwrap().ends_with("a.md"));
    assert_eq!(edges[0]["slot"], "link");
    assert_eq!(edges[0]["surface"], "frontmatter");

    // Content reads source text from disk, off the held-state lock, stamped
    // with the version it observed.
    let resp = client
        .query(&json!({ "read": "content", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["ready"], true);
    assert_eq!(resp["version"], 1);
    assert_eq!(
        resp["result"]["content"]["hash"].as_str().unwrap().len(),
        16,
        "content carries the 16-hex content hash for the save guard, got {:?}",
        resp["result"]["content"]
    );
    assert!(
        resp["result"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("type: note"),
        "content returns the file's source text, got {:?}",
        resp["result"]["content"]
    );

    // An unreadable path returns null, content and hash alike.
    let resp = client
        .query(&json!({ "read": "content", "path": "does-not-exist.md" }))
        .unwrap();
    assert!(
        resp["result"]["content"].is_null(),
        "an unreadable file yields null, got {:?}",
        resp["result"]["content"]
    );

    // A malformed request is reported, not fatal.
    let resp = client.query(&json!({ "read": "nonsense" })).unwrap();
    assert!(resp["error"].is_string());
}

#[test]
fn a_second_serve_on_a_live_socket_fails_without_orphaning_the_first() {
    // The bind is the mutual-exclusion primitive: a second serve on a socket a
    // live daemon already owns must fail, and must not clobber the live socket.
    let (_dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    let engine = Engine::new(&root, ConfigSource::Empty);
    let first = serve(engine.handle(), &socket).expect("first serve binds");

    // A second serve on the same path is rejected, not silently rebound.
    let engine2 = Engine::new(&root, ConfigSource::Empty);
    let second = serve(engine2.handle(), &socket);
    assert!(second.is_err(), "second serve must not bind a live socket");

    // The first daemon is untouched: a client still reaches it.
    let mut client = Client::connect(&socket).expect("first daemon still reachable");
    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert_eq!(resp["type"], "response");
    drop(first);
}

#[test]
fn a_stale_socket_is_reclaimed() {
    // A socket file with nothing listening (an ungraceful exit) is stale: a
    // fresh serve reclaims it rather than failing on the leftover path.
    let (_dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    // A leftover regular file at the path stands in for a stale socket: bind
    // fails with AddrInUse, and connect refuses, so it is reclaimed.
    fs::write(&socket, b"stale").unwrap();

    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("stale socket reclaimed");
    let mut client = Client::connect(&socket).expect("connect after reclaim");
    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert_eq!(resp["type"], "response");
}

#[test]
fn the_socket_is_owner_only() {
    // The wire is unauthenticated, so the socket must not be world- or
    // group-accessible: the trust boundary is the owning user.
    use std::os::unix::fs::PermissionsExt;
    let (_dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");

    let mode = fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "socket is owner-only, got {:o}", mode);
}

#[test]
fn an_oversized_frame_length_is_rejected_without_allocating() {
    // A client-controlled length prefix above the cap is a protocol
    // violation. The reader must reject it before allocating, not honor a
    // multi-gigabyte length. The connection closes; the daemon stays up.
    let (_dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket).expect("serve");

    // The maximal u32 length: a ~4 GB allocation if it were honored.
    let mut raw = UnixStream::connect(&socket).expect("connect");
    raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    raw.write_all(&u32::MAX.to_be_bytes()).unwrap();
    raw.flush().unwrap();
    // The server closes the connection rather than reading the body. The
    // read returns EOF (0 bytes) promptly instead of blocking on a 4 GB read.
    let mut buf = [0u8; 1];
    let n = raw.read(&mut buf).expect("read returns, not blocks");
    assert_eq!(
        n, 0,
        "the daemon closes the connection on an oversized frame"
    );

    // The daemon is still serving: a fresh client gets a normal reply.
    let mut client = Client::connect(&socket).expect("reconnect");
    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert_eq!(resp["type"], "response");
}

/// A knowledge base with three diagnostics on distinct axes: a warning in `a.md`
/// (dangling reference, open-world growth), an error in `z.md` (a required
/// field absent), and a warning in `sub/c.md` (anchor not found), so every
/// filter has something to keep and something to drop. `z.md` sorts last, so
/// the stream order is `a.md`, `sub/c.md`, `z.md`.
fn diagnostics_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Build in a `v` subdir so the folder basename matches the seeded repo name.
    let root = fs::canonicalize(dir.path()).unwrap().join("v");
    fs::create_dir_all(&root).unwrap();
    crate::seed_repo(&root);
    fs::create_dir(root.join("type")).unwrap();
    fs::write(
        root.join("type/note.type.yaml"),
        "fields:\n  link?: file*\n",
    )
    .unwrap();
    fs::write(
        root.join("a.md"),
        "---\ntype: note\nlink: \"[[missing]]\"\n---\n",
    )
    .unwrap();
    // A heading-bearing body, so an anchor to a missing heading is known-absent
    // (not silent for lack of a body to check against).
    fs::write(root.join("b.md"), "---\ntype: note\n---\n# Real Heading\n").unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    // A navigational body link to a missing heading — anchor-not-found warning.
    // (A `#head` on a `file*` slot is rejected as a shape error instead, so the
    // anchor must be navigational to exercise the warning.)
    fs::write(
        root.join("sub/c.md"),
        "---\ntype: note\n---\nsee [[b#nope]]\n",
    )
    .unwrap();
    // A real error, so the severity/error axis still has something to keep now
    // that a dangling typed reference is a warning. `gadget` requires `size`;
    // `z.md` omits it → required-field-absent. Named to sort last.
    fs::write(
        root.join("type/gadget.type.yaml"),
        "fields:\n  size: Number\n",
    )
    .unwrap();
    fs::write(root.join("z.md"), "---\ntype: gadget\n---\n").unwrap();
    (dir, root)
}

#[test]
fn diagnostics_read_filters_compose_and_unknown_args_reject() {
    let (_dir, root) = diagnostics_fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket).expect("serve");
    let mut client = Client::connect(&socket).expect("connect");

    // Unfiltered: all three diagnostics, the fixture's baseline.
    let resp = client.query(&json!({ "read": "diagnostics" })).unwrap();
    let all = resp["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(all.len(), 3, "fixture has three diagnostics, got {all:?}");

    // severity keeps the two warnings (the dangling ref and the anchor).
    let resp = client
        .query(&json!({ "read": "diagnostics", "severity": "warning" }))
        .unwrap();
    let diags = resp["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 2, "two warnings, got {diags:?}");
    let warn_codes: Vec<&str> = diags.iter().map(|d| d["code"].as_str().unwrap()).collect();
    assert!(warn_codes.contains(&"reference-target-missing"));
    assert!(warn_codes.contains(&"anchor-not-found"));

    // severity keeps the one error (the required field absent).
    let resp = client
        .query(&json!({ "read": "diagnostics", "severity": "error" }))
        .unwrap();
    let diags = resp["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "one error, got {diags:?}");
    assert_eq!(diags[0]["code"], "required-field-absent");

    // code keeps the dangling reference only.
    let resp = client
        .query(&json!({ "read": "diagnostics", "code": "reference-target-missing" }))
        .unwrap();
    let diags = resp["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "one match by code, got {diags:?}");
    assert!(diags[0]["span"]["file"].as_str().unwrap().ends_with("a.md"));

    // path_prefix keeps the subtree only.
    let resp = client
        .query(&json!({ "read": "diagnostics", "path_prefix": "sub" }))
        .unwrap();
    let diags = resp["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "one diagnostic under sub/, got {diags:?}");
    assert!(diags[0]["span"]["file"]
        .as_str()
        .unwrap()
        .ends_with("sub/c.md"));

    // path keeps one exact file.
    let resp = client
        .query(&json!({ "read": "diagnostics", "path": "a.md" }))
        .unwrap();
    assert_eq!(resp["result"]["diagnostics"].as_array().unwrap().len(), 1);

    // Filters compose: nothing under sub/ is an error.
    let resp = client
        .query(&json!({ "read": "diagnostics", "path_prefix": "sub", "severity": "error" }))
        .unwrap();
    assert_eq!(resp["result"]["diagnostics"].as_array().unwrap().len(), 0);

    // A path_prefix matches whole components, not string prefixes: `su`
    // does not cover `sub/`.
    let resp = client
        .query(&json!({ "read": "diagnostics", "path_prefix": "su" }))
        .unwrap();
    assert_eq!(resp["result"]["diagnostics"].as_array().unwrap().len(), 0);

    // An unknown arg is rejected loudly, not silently ignored.
    let resp = client
        .query(&json!({ "read": "diagnostics", "severty": "error" }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    assert!(
        resp["error"].as_str().unwrap().contains("severty"),
        "the error names the unknown arg, got {:?}",
        resp["error"]
    );

    // So is an unknown severity value.
    let resp = client
        .query(&json!({ "read": "diagnostics", "severity": "fatal" }))
        .unwrap();
    assert_eq!(resp["type"], "error");
}

#[test]
fn replies_echo_the_request_id_and_errors_carry_a_discriminator() {
    let (_dir, root) = fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");

    let engine = Engine::new(&root, ConfigSource::Empty);
    let _server = serve(engine.handle(), &socket).expect("serve");
    engine.rebuild();

    let mut client = Client::connect(&socket).expect("connect");

    // A read response echoes the request id verbatim.
    let resp = client
        .query(&json!({ "read": "lifecycle", "id": 7 }))
        .unwrap();
    assert_eq!(resp["type"], "response");
    assert_eq!(resp["id"], 7);

    // A string id round-trips too.
    let resp = client
        .query(&json!({ "read": "lifecycle", "id": "abc" }))
        .unwrap();
    assert_eq!(resp["id"], "abc");

    // No id in, no id out.
    let resp = client.query(&json!({ "read": "lifecycle" })).unwrap();
    assert!(resp.get("id").is_none(), "id omitted when not supplied");

    // An unknown read is an error frame carrying the id and a `read` discriminator.
    let resp = client.query(&json!({ "read": "bogus", "id": 9 })).unwrap();
    assert_eq!(resp["type"], "error");
    assert_eq!(resp["for"], "read");
    assert_eq!(resp["id"], 9);

    // A malformed subscribe is an error frame too, distinguished by `for`.
    let resp = client
        .query(&json!({ "subscribe": "bogus", "id": 11 }))
        .unwrap();
    assert_eq!(resp["type"], "error");
    assert_eq!(resp["for"], "subscribe");
    assert_eq!(resp["id"], 11);
}

#[test]
fn diagnostics_read_pages_with_limit_and_offset() {
    let (_dir, root) = diagnostics_fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket).expect("serve");
    let mut client = Client::connect(&socket).expect("connect");

    // Stream order is by source path: a.md (warning), sub/c.md (warning), z.md (error).
    let first = client
        .query(&json!({ "read": "diagnostics", "limit": 1 }))
        .unwrap();
    let page = first["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(page.len(), 1, "limit 1 returns one entry, got {page:?}");
    assert_eq!(page[0]["code"], "reference-target-missing");

    // offset skips into the page.
    let second = client
        .query(&json!({ "read": "diagnostics", "offset": 1 }))
        .unwrap();
    let page = second["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(
        page.len(),
        2,
        "offset 1 drops the first, keeps the rest, got {page:?}"
    );
    assert_eq!(page[0]["code"], "anchor-not-found");

    // offset + limit windows one entry.
    let windowed = client
        .query(&json!({ "read": "diagnostics", "offset": 1, "limit": 1 }))
        .unwrap();
    assert_eq!(
        windowed["result"]["diagnostics"].as_array().unwrap()[0]["code"],
        "anchor-not-found"
    );

    // Past the end is an empty page, not an error (three diagnostics, offset 3).
    let past = client
        .query(&json!({ "read": "diagnostics", "offset": 3 }))
        .unwrap();
    assert_eq!(past["result"]["diagnostics"].as_array().unwrap().len(), 0);

    // Paging composes with the filters: both warnings fit in the page.
    let filtered = client
        .query(&json!({ "read": "diagnostics", "severity": "warning", "limit": 5 }))
        .unwrap();
    let page = filtered["result"]["diagnostics"].as_array().unwrap();
    assert_eq!(page.len(), 2);
}

#[test]
fn diagnostic_counts_summarizes_by_severity_and_code() {
    let (_dir, root) = diagnostics_fixture();
    let sock_dir = tempfile::tempdir().unwrap();
    let socket = sock_dir.path().join("s");
    let engine = Engine::new(&root, ConfigSource::Empty);
    engine.rebuild();
    let _server = serve(engine.handle(), &socket).expect("serve");
    let mut client = Client::connect(&socket).expect("connect");

    let resp = client
        .query(&json!({ "read": "diagnostic_counts" }))
        .unwrap();
    let r = &resp["result"]["diagnostic_counts"];
    assert_eq!(r["total"], 3);
    assert_eq!(r["by_severity"]["error"], 1);
    assert_eq!(r["by_severity"]["warning"], 2);
    assert_eq!(r["by_code"]["reference-target-missing"], 1);
    assert_eq!(r["by_code"]["anchor-not-found"], 1);
    assert_eq!(r["by_code"]["required-field-absent"], 1);

    // Counts honor the same filters.
    let resp = client
        .query(&json!({ "read": "diagnostic_counts", "severity": "error" }))
        .unwrap();
    let r = &resp["result"]["diagnostic_counts"];
    assert_eq!(r["total"], 1);
    assert_eq!(r["by_severity"]["error"], 1);
    assert!(
        r["by_severity"].get("warning").is_none(),
        "warning absent under the error filter"
    );
    assert_eq!(r["by_code"]["required-field-absent"], 1);

    // Counts are whole-set: paging args are ignored.
    let resp = client
        .query(&json!({ "read": "diagnostic_counts", "limit": 1 }))
        .unwrap();
    assert_eq!(
        resp["result"]["diagnostic_counts"]["total"], 3,
        "counts ignore limit"
    );
}
